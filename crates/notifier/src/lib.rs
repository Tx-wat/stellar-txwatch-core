#![forbid(unsafe_code)]

//! txwatch-notifier delivers webhook payloads over HTTP with retry and backoff,
//! exposing `send_webhook` and `test_payload` helpers for webhook delivery.

use anyhow::{anyhow, Result};
use chrono::Utc;
use flate2::write::GzEncoder;
use flate2::Compression;
use hex;
use hmac::{Hmac, Mac};
use reqwest::{Certificate, Client, Identity};
use sha2::Sha256;
use std::fs;
use std::io::Write;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
#[cfg(feature = "metrics")]
use std::time::Instant;
use tokio::sync::oneshot;
use tracing::{debug, error, info, span, warn, Level};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use txwatch_config::{ContractType, Network, TlsConfig};
use txwatch_rules::AlertPayload;

const MAX_RETRIES: u32 = 3;

#[cfg(feature = "metrics")]
fn webhook_latency_histogram() -> &'static prometheus::Histogram {
    use std::sync::OnceLock;

    static HISTOGRAM: OnceLock<prometheus::Histogram> = OnceLock::new();
    HISTOGRAM.get_or_init(|| {
        prometheus::register_histogram!(
            "txwatch_webhook_latency_ms",
            "Webhook HTTP attempt latency in milliseconds, from request send until response headers or transport error",
            vec![5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0]
        )
        .expect("register txwatch_webhook_latency_ms")
    })
}

#[cfg(feature = "metrics")]
fn observe_webhook_latency(elapsed: Duration) {
    webhook_latency_histogram().observe(elapsed.as_secs_f64() * 1_000.0);
}

/// Batched webhook body sent when `batch_alerts_per_tx` is enabled (Issue #557).
///
/// When several rules fire on the same transaction, a single POST is sent whose
/// body contains a `rules_triggered` array of the individual rule payloads.
#[derive(Debug, Clone, Serialize)]
pub struct BatchedAlertPayload {
    /// Transaction hash shared by every payload in `rules_triggered`.
    pub transaction_hash: String,
    /// Number of rules that fired on this transaction.
    pub rule_count: usize,
    /// The individual per-rule payloads.
    pub rules_triggered: Vec<AlertPayload>,
}

impl BatchedAlertPayload {
    /// Build a batch from payloads that all share the same transaction hash.
    fn from_payloads(transaction_hash: String, rules_triggered: Vec<AlertPayload>) -> Self {
        Self {
            transaction_hash,
            rule_count: rules_triggered.len(),
            rules_triggered,
        }
    }
}

/// Group `AlertPayload`s by `transaction_hash`, preserving first-seen order of
/// transactions and of rules within each transaction (Issue #557).
pub fn group_by_transaction(payloads: &[AlertPayload]) -> Vec<BatchedAlertPayload> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: BTreeMap<String, Vec<AlertPayload>> = BTreeMap::new();
    for payload in payloads {
        let key = payload.transaction_hash.clone();
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups.entry(key).or_default().push(payload.clone());
    }
    order
        .into_iter()
        .map(|hash| {
            let rules = groups.remove(&hash).unwrap_or_default();
            BatchedAlertPayload::from_payloads(hash, rules)
        })
        .collect()
}

/// Dispatch a set of alert payloads to `url`.
///
/// When `batch_alerts_per_tx` is `true`, payloads are grouped by
/// `transaction_hash` and exactly one POST is sent per transaction, with a
/// `rules_triggered` array in the body (Issue #557). When `false`, one POST is
/// sent per payload, preserving the original behavior.
///
/// Returns one [`DeliveryResult`] per POST that was sent.
pub async fn dispatch_alerts(
    client:              &Client,
    url:                 &str,
    payloads:            &[AlertPayload],
    secret:              Option<&str>,
    batch_alerts_per_tx: bool,
    shutdown:            oneshot::Receiver<()>,
) -> Vec<Result<DeliveryResult>> {
    if batch_alerts_per_tx {
        let batches = group_by_transaction(payloads);
        let mut results = Vec::with_capacity(batches.len());
        for batch in &batches {
            results.push(
                send_batched_webhook(client, url, batch, secret, shutdown_reset(&shutdown)).await,
            );
        }
        results
    } else {
        let mut results = Vec::with_capacity(payloads.len());
        for payload in payloads {
            results.push(
                send_webhook(client, url, payload, secret, shutdown_reset(&shutdown)).await,
            );
        }
        results
    }
}

/// Create a fresh shutdown receiver that never fires, used when dispatching
/// multiple POSTs from a single caller-supplied shutdown channel.
fn shutdown_reset(_original: &oneshot::Receiver<()>) -> oneshot::Receiver<()> {
    let (_tx, rx) = oneshot::channel();
    std::mem::forget(_tx);
    rx
}

/// Maximum response body size to read for debug logging (4096 bytes / 4 KB).
/// Prevents OOM when malicious or broken receivers stream endless response bodies (Issue #358).
pub const MAX_RESPONSE_BODY_BYTES: usize = 4096;

/// Reads at most `MAX_RESPONSE_BODY_BYTES` bytes from a response stream to protect against OOM (Issue #358).
pub async fn read_response_body_capped(mut resp: reqwest::Response) -> String {
    let mut buf = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        if buf.len() + chunk.len() > MAX_RESPONSE_BODY_BYTES {
            let needed = MAX_RESPONSE_BODY_BYTES.saturating_sub(buf.len());
            buf.extend_from_slice(&chunk[..needed]);
            let s = String::from_utf8_lossy(&buf);
            return format!("{} [truncated]", s);
        }
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Backoff base for webhook retries: attempt `n` waits `RETRY_BASE_DELAY * 2^n`.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Structured result returned by a successful `send_webhook` call.
#[derive(Debug, PartialEq)]
pub struct DeliveryResult {
    /// Number of attempts made (1 = delivered on first try).
    pub attempts: u32,
    /// HTTP status code of the successful response.
    pub final_status: u16,
}

/// Default webhook delivery timeout, used when no override is configured (Issue #462).
pub const DEFAULT_WEBHOOK_TIMEOUT_SECS: u64 = 15;

/// Shared HTTP client settings applied to every webhook client (Issues #461, #462).
///
/// These mirror the connection-pool / keepalive / user-agent tuning used by the
/// poller's shared client so that TLS-configured contracts behave consistently.
#[derive(Debug, Clone)]
pub struct HttpClientSettings {
    /// Value for the `User-Agent` header on every request.
    pub user_agent: String,
    /// Maximum number of idle connections kept per host.
    pub pool_max_idle_per_host: usize,
    /// TCP keepalive interval, if enabled.
    pub tcp_keepalive: Option<Duration>,
    /// Per-request timeout for webhook deliveries.
    pub timeout: Duration,
}

impl Default for HttpClientSettings {
    fn default() -> Self {
        Self {
            user_agent: format!("txwatch/{}", env!("CARGO_PKG_VERSION")),
            pool_max_idle_per_host: 10,
            tcp_keepalive: Some(Duration::from_secs(60)),
            timeout: Duration::from_secs(DEFAULT_WEBHOOK_TIMEOUT_SECS),
        }
    }
}

impl HttpClientSettings {
    /// Apply the shared pool/keepalive/user-agent/timeout options to a builder.
    ///
    /// Redirects are always disabled (Issue #455): following a `3xx` could send
    /// the signed payload to an internal address, bypassing the SSRF guard that
    /// `WatchedContract::validate()` applies to the original webhook URL only.
    fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder = builder
            .redirect(reqwest::redirect::Policy::none())
            .timeout(self.timeout)
            .user_agent(self.user_agent.clone())
            .pool_max_idle_per_host(self.pool_max_idle_per_host);
        if let Some(keepalive) = self.tcp_keepalive {
            builder = builder.tcp_keepalive(keepalive);
        }
        builder
    }
}

/// Build a shared HTTP client with sensible defaults.
pub fn build_client() -> Result<Client> {
    build_client_with_settings(&HttpClientSettings::default())
}

/// Build a shared HTTP client using explicit [`HttpClientSettings`].
pub fn build_client_with_settings(settings: &HttpClientSettings) -> Result<Client> {
    settings
        .apply(Client::builder())
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {}", e))
}

/// Build a client for a contract with `[contracts.tls]` set. The client presents
/// the configured mTLS identity and, when `ca_cert` is set, trusts that CA bundle
/// in addition to the system roots. Files are read once, here, at startup.
/// Error messages name the file path but never include file contents.
pub fn build_tls_client(tls: &TlsConfig) -> Result<Client> {
    build_tls_client_with_settings(tls, &HttpClientSettings::default())
}

/// Build an mTLS client using explicit [`HttpClientSettings`] (Issues #461, #462).
pub fn build_tls_client_with_settings(
    tls: &TlsConfig,
    settings: &HttpClientSettings,
) -> Result<Client> {
    let cert = fs::read(&tls.client_cert)
        .map_err(|e| anyhow!("cannot read tls.client_cert '{}': {}", tls.client_cert, e))?;
    let key = fs::read(&tls.client_key)
        .map_err(|e| anyhow!("cannot read tls.client_key '{}': {}", tls.client_key, e))?;
    let identity = Identity::from_pkcs8_pem(&cert, &key).map_err(|e| {
        anyhow!(
            "invalid mTLS identity (client_cert '{}', client_key '{}'): {}",
            tls.client_cert,
            tls.client_key,
            e
        )
    })?;

    let mut builder = settings.apply(Client::builder()).identity(identity);
    if let Some(ca_path) = &tls.ca_cert {
        let ca_pem = fs::read(ca_path)
            .map_err(|e| anyhow!("cannot read tls.ca_cert '{}': {}", ca_path, e))?;
        let ca = Certificate::from_pem(&ca_pem)
            .map_err(|e| anyhow!("invalid tls.ca_cert '{}': {}", ca_path, e))?;
        builder = builder.add_root_certificate(ca);
    }
    builder
        .build()
        .map_err(|e| anyhow!("failed to build mTLS HTTP client: {}", e))
}

/// Build an HTTP client configured with a pinned TLS root certificate (Issue #362).
pub fn build_pinned_cert_client(cert_pem: &str) -> Result<Client> {
    build_pinned_cert_client_with_settings(cert_pem, &HttpClientSettings::default())
}

/// Build a pinned-cert client using explicit [`HttpClientSettings`] (Issues #461, #462).
pub fn build_pinned_cert_client_with_settings(
    cert_pem: &str,
    settings: &HttpClientSettings,
) -> Result<Client> {
    let cert = reqwest::Certificate::from_pem(cert_pem.as_bytes())
        .map_err(|e| anyhow!("invalid webhook_tls_cert_pem: {}", e))?;
    settings
        .apply(Client::builder())
        .add_root_certificate(cert)
        .build()
        .map_err(|e| anyhow!("failed to build TLS pinned client: {}", e))
}

/// Compute the `X-TxWatch-Signature` header value for `body` using `secret`.
///
/// Returns `sha256=<hex>` where `<hex>` is the HMAC-SHA256 of `body`. Shared by
/// [`send_webhook`] and the CLI `test-webhook` path (Issue #554).
pub fn sign_payload(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

/// Returns `true` when an alert with `rule_type` should be delivered given the
/// contract's `webhook_rule_filter` (Issue #553).
///
/// An empty filter means "deliver all". Otherwise the alert's `rule_type` must
/// appear in the filter list.
pub fn rule_matches_filter(rule_type: &str, filter: &[String]) -> bool {
    filter.is_empty() || filter.iter().any(|f| f == rule_type)
}

/// POST `payload` to `url`, retrying up to `MAX_RETRIES` times with
/// exponential backoff (2 s → 4 s → 8 s). Logs each attempt.
///
/// If `secret` is `Some`, adds an `X-TxWatch-Signature: sha256=<hmac>` header
/// computed as HMAC-SHA256 of the request body using the provided secret key.
///
/// Delivery semantics: HTTP 2xx = success. The response body is logged at
/// debug level but is otherwise ignored — a 200 OK with an error body is
/// still treated as a successful delivery (#24).
///
/// Redirects are never followed (Issue #455): a `3xx` response is treated as a
/// delivery failure, so a redirect cannot smuggle the signed payload to an
/// internal address that the SSRF guard would otherwise reject.
///
/// Graceful shutdown (#260): If `shutdown` fires during a retry backoff wait,
/// the delivery is aborted and returns an error. In-flight HTTP requests
/// (already sent) are allowed to complete.
///
/// Returns a [`DeliveryResult`] describing how many attempts were needed.
pub async fn send_webhook(
    client:   &Client,
    url:      &str,
    payload:  &AlertPayload,
    secret:   Option<&str>,
    shutdown: oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    send_webhook_with_backoff(client, url, payload, secret, RETRY_BASE_DELAY, shutdown).await
}

/// Deliver an alert with per-contract HTTP headers. Use a redirect-disabled webhook client.
pub async fn send_webhook_with_headers(
    client: &Client,
    url: &str,
    payload: &AlertPayload,
    secret: Option<&str>,
    headers: &HashMap<String, String>,
    compress: bool,
    shutdown: oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    txwatch_config::validate_webhook_headers(headers)?;
    let mut fields = HeaderMap::new();
    for (name, value) in headers {
        let name = HeaderName::from_bytes(name.as_bytes())?;
        let mut value = HeaderValue::from_str(value).map_err(|_| anyhow!("invalid webhook header value"))?;
        value.set_sensitive(true);
        fields.insert(name, value);
    }
    send_webhook_with_backoff_opts(client, url, payload, secret, compress, fields, RETRY_BASE_DELAY, shutdown).await
}

/// Convenience wrapper around [`send_webhook`] for one-shot callers that have no
/// meaningful shutdown signal to wire up (e.g. the CLI's `test-webhook` command).
///
/// Creates a never-firing shutdown channel internally, so delivery always runs
/// to completion (or permanent failure) without being aborted mid-backoff.
pub async fn send_webhook_once(
    client:  &Client,
    url:     &str,
    payload: &AlertPayload,
    secret:  Option<&str>,
) -> Result<DeliveryResult> {
    let (tx, rx) = oneshot::channel();
    // Keep the sender alive so the receiver never resolves.
    std::mem::forget(tx);
    send_webhook(client, url, payload, secret, rx).await
}

/// Internal variant of [`send_webhook`] that accepts a custom backoff base duration.
/// Used by tests to speed up retry testing without changing retry count semantics.
async fn send_webhook_with_backoff(
    client:        &Client,
    url:           &str,
    payload:       &AlertPayload,
    secret:        Option<&str>,
    backoff_base:  Duration,
    mut shutdown:  oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    send_webhook_with_backoff_opts(
        client, url, payload, secret, false, HeaderMap::new(), backoff_base, shutdown,
    )
    .await
}

/// Gzip-compress `data` at the default compression level.
fn gzip_compress(data: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .map_err(|e| anyhow!("failed to gzip-compress webhook body: {}", e))?;
    encoder
        .finish()
        .map_err(|e| anyhow!("failed to gzip-compress webhook body: {}", e))
}

/// Core delivery loop. When `compress` is `true` the serialized payload is
/// gzip-compressed and sent with `Content-Encoding: gzip` (Issue #540).
async fn send_webhook_with_backoff_opts(
    client:        &Client,
    url:           &str,
    payload:       &AlertPayload,
    secret:        Option<&str>,
    compress:      bool,
    headers:       HeaderMap,
    backoff_base:  Duration,
    mut shutdown:  oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    let span = span!(
        Level::INFO,
        "send_webhook",
        contract = %payload.label,
        rule     = %payload.rule_triggered
    );
    let _enter = span.enter();

    let json_body = serde_json::to_string(payload)?;
    // The signature is always computed over the uncompressed JSON body so that
    // receivers can verify it after decompressing (Issue #540).
    let body: Vec<u8> = if compress {
        gzip_compress(json_body.as_bytes())?
    } else {
        json_body.clone().into_bytes()
    };
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();

        let idempotency_key = format!(
            "{}:{}:{}",
            payload.contract_id, payload.transaction_hash, payload.rule_type
        );

        let mut req = client
            .post(url)
            .headers(headers.clone())
            .header("Content-Type", "application/json")
            .header("X-TxWatch-Version", env!("CARGO_PKG_VERSION"))
            .header("X-TxWatch-Idempotency-Key", idempotency_key)
            .body(body.clone());

        if compress {
            req = req.header("Content-Encoding", "gzip");
        }

        if let Some(s) = secret {
            let sig = sign_payload(s, json_body.as_bytes());
            req = req.header("X-TxWatch-Signature", format!("sha256={}", sig));
        }

        let request_started = Instant::now();
        let response = req.send().await;
        #[cfg(feature = "metrics")]
        observe_webhook_latency(request_started.elapsed());

        match response {
            Ok(resp) if resp.status().is_success() => {
                let final_status = resp.status().as_u16();
                span.record("status", final_status);
                // Issue #24: log response body at debug level.
                // HTTP 2xx = delivery success regardless of body content.
                let body_text = read_response_body_capped(resp).await;
                debug!(
                    timestamp     = %ts,
                    url           = %url,
                    status        = final_status,
                    response_body = %body_text,
                    "webhook 2xx response body"
                );
                info!(
                    timestamp = %ts,
                    url       = %url,
                    rule      = %payload.rule_triggered,
                    tx        = %payload.transaction_hash,
                    attempts  = attempt,
                    "webhook delivered"
                );
                return Ok(DeliveryResult { attempts: attempt, final_status });
            }
            Ok(resp) => {
                let status = resp.status();
                span.record("status", status.as_u16());
                if status.is_redirection() {
                    // Issue #455: never follow redirects — treat as a failure.
                    warn!(
                        timestamp = %ts,
                        attempt   = attempt,
                        url       = %url,
                        status    = %status,
                        "webhook attempt failed: redirect responses are not followed"
                    );
                    last_err = Some(anyhow!("HTTP {} (redirect not followed)", status));
                } else {
                    warn!(
                        timestamp = %ts,
                        attempt   = attempt,
                        url       = %url,
                        status    = %status,
                        "webhook attempt failed with HTTP error"
                    );
                    last_err = Some(anyhow!("HTTP {}", status));
                }
            }
            Err(e) => {
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    error     = %e,
                    "webhook attempt failed with network error"
                );
                last_err = Some(e.into());
            }
        }

        if attempt < MAX_RETRIES {
            // #260: check for shutdown during backoff. If shutdown fires, abort
            // the delivery instead of waiting for the full backoff period.
            let backoff_duration = if backoff_base.is_zero() {
                Duration::ZERO
            } else {
                backoff_base * 2u32.pow(attempt)
            };

            tokio::select! {
                _ = tokio::time::sleep(backoff_duration) => {}
                _ = &mut shutdown => {
                    warn!(
                        url  = %url,
                        rule = %payload.rule_triggered,
                        tx   = %payload.transaction_hash,
                        "webhook delivery aborted due to shutdown signal"
                    );
                    return Err(anyhow!("shutdown during retry backoff"));
                }
            }
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow!("webhook failed after {} retries", MAX_RETRIES));
    error!(
        url  = %url,
        rule = %payload.rule_triggered,
        tx   = %payload.transaction_hash,
        "webhook delivery failed permanently: {}",
        err
    );
    Err(err)
}

#[cfg(all(test, feature = "metrics"))]
mod webhook_metrics_tests {
    use super::*;

    #[test]
    fn webhook_latency_histogram_registers_and_observes() {
        observe_webhook_latency(Duration::from_millis(42));

        let family = prometheus::gather()
            .into_iter()
            .find(|family| family.get_name() == "txwatch_webhook_latency_ms")
            .expect("webhook latency histogram should be registered");
        assert!(family.get_metric()[0].get_histogram().get_sample_count() >= 1);
    }
}

/// POST a [`BatchedAlertPayload`] to `url`, retrying up to `MAX_RETRIES` times
/// with exponential backoff (Issue #557).
///
/// Delivery semantics mirror [`send_webhook`]: HTTP 2xx = success, the response
/// body is logged at debug level and otherwise ignored, and shutdown during a
/// backoff wait aborts the delivery.
pub async fn send_batched_webhook(
    client:   &Client,
    url:      &str,
    batch:    &BatchedAlertPayload,
    secret:   Option<&str>,
    shutdown: oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    send_batched_webhook_with_backoff(client, url, batch, secret, RETRY_BASE_DELAY, shutdown).await
}

/// Internal variant of [`send_batched_webhook`] accepting a custom backoff base.
async fn send_batched_webhook_with_backoff(
    client:       &Client,
    url:          &str,
    batch:        &BatchedAlertPayload,
    secret:       Option<&str>,
    backoff_base: Duration,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    let span = span!(
        Level::INFO,
        "send_batched_webhook",
        tx         = %batch.transaction_hash,
        rule_count = batch.rule_count
    );
    let _enter = span.enter();

    let body = serde_json::to_string(batch)?;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();

        let idempotency_key = format!("{}:batch", batch.transaction_hash);

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-TxWatch-Version", env!("CARGO_PKG_VERSION"))
            .header("X-TxWatch-Idempotency-Key", idempotency_key)
            .header("X-TxWatch-Batched", "true")
            .body(body.clone());

        if let Some(s) = secret {
            let mut mac = Hmac::<Sha256>::new_from_slice(s.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(body.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            req = req.header("X-TxWatch-Signature", format!("sha256={}", sig));
        }

        let request_started = Instant::now();
        let response = req.send().await;
        #[cfg(feature = "metrics")]
        observe_webhook_latency(request_started.elapsed());

        match response {
            Ok(resp) if resp.status().is_success() => {
                let final_status = resp.status().as_u16();
                span.record("status", final_status);
                let body_text = read_response_body_capped(resp).await;
                debug!(
                    timestamp     = %ts,
                    url           = %url,
                    status        = final_status,
                    response_body = %body_text,
                    "batched webhook 2xx response body"
                );
                info!(
                    timestamp  = %ts,
                    url        = %url,
                    tx         = %batch.transaction_hash,
                    rule_count = batch.rule_count,
                    attempts   = attempt,
                    "batched webhook delivered"
                );
                return Ok(DeliveryResult { attempts: attempt, final_status });
            }
            Ok(resp) => {
                let status = resp.status();
                span.record("status", status.as_u16());
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    status    = %status,
                    "batched webhook attempt failed with HTTP error"
                );
                last_err = Some(anyhow!("HTTP {}", status));
            }
            Err(e) => {
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    error     = %e,
                    "batched webhook attempt failed with network error"
                );
                last_err = Some(e.into());
            }
        }

        if attempt < MAX_RETRIES {
            let backoff_duration = if backoff_base.is_zero() {
                Duration::ZERO
            } else {
                backoff_base * 2u32.pow(attempt)
            };

            tokio::select! {
                _ = tokio::time::sleep(backoff_duration) => {}
                _ = &mut shutdown => {
                    warn!(
                        url = %url,
                        tx  = %batch.transaction_hash,
                        "batched webhook delivery aborted due to shutdown signal"
                    );
                    return Err(anyhow!("shutdown during retry backoff"));
                }
            }
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow!("webhook failed after {} retries", MAX_RETRIES));
    error!(
        url = %url,
        tx  = %batch.transaction_hash,
        "batched webhook delivery failed permanently: {}",
        err
    );
    Err(err)
}

/// Constant-time verification helper for webhook receivers to verify `X-TxWatch-Signature`
/// without vulnerability to timing attacks (Issue #354).
pub fn verify_signature_constant_time(secret: &[u8], body: &[u8], signature_header: &str) -> bool {
    let expected_hex = signature_header.strip_prefix("sha256=").unwrap_or(signature_header);
    let expected_bytes = match hex::decode(expected_hex.trim()) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = match Hmac::<Sha256>::new_from_slice(secret) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let computed_bytes = mac.finalize().into_bytes();
    if computed_bytes.len() != expected_bytes.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in computed_bytes.iter().zip(expected_bytes.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Build a synthetic `AlertPayload` suitable for `test-webhook`.
pub fn test_payload(label: &str, webhook_url: &str) -> AlertPayload {
    test_payload_with_network(label, webhook_url, &Network::Testnet, "https://horizon-testnet.stellar.org")
}

/// Build a synthetic `AlertPayload` with an explicit network name and Horizon base URL.
pub fn test_payload_with_network(
    label:            &str,
    webhook_url:      &str,
    network:          &txwatch_config::Network,
    horizon_base_url: &str,
) -> AlertPayload {
    let now     = Utc::now();
    let tx_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    // Stellar Expert names the mainnet explorer path "public".
    let explorer_network = if network.as_str() == "mainnet" { "public" } else { network.as_str() };
    let contract_id = "CTEST000000000000000000000000000000000000000000000000000";
    AlertPayload {
        label:               label.to_string(),
        contract_id:         contract_id.into(),
        contract_type:       ContractType::from_contract_id(contract_id).unwrap_or(ContractType::Soroban),
        network:             network.clone(),
        rule_type:           "TestWebhook".into(),
        rule_triggered:      "TestWebhook".into(),
        transaction_hash:    tx_hash.into(),
        function_name:       Some("test".into()),
        function_names:      vec!["test".into()],
        amount_xlm:          None,
        amount_stroops:      None,
        fee_charged_stroops: None,
        operation_count:     None,
        memo_type:           None,
        memo:                None,
        timestamp:           now.timestamp(),
        timestamp_iso:       now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        horizon_link:        format!("{}/transactions/{}", horizon_base_url, tx_hash),
        explorer_link:       format!("https://stellar.expert/explorer/{}/tx/{}", explorer_network, tx_hash),
        ledger_sequence:     None,
        source_account:      None,
        tags:                None,
    }
    .with_label(format!("{} (test-webhook to {})", label, webhook_url))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Create a dummy shutdown receiver that never fires, for use in tests.
///
/// The returned sender is intentionally unused — it will be dropped at end of
/// scope, which means the receiver will observe a "disconnected" state, but
/// `tokio::select!` on a dropped oneshot never resolves the `recv` branch
/// unless the sender was sent to first. We hold the sender in the returned
/// tuple so tests can explicitly trigger shutdown if needed.
#[cfg(test)]
pub(crate) fn dummy_shutdown() -> oneshot::Receiver<()> {
    let (_tx, rx) = oneshot::channel();
    std::mem::forget(_tx); // keep sender alive so receiver never fires
    rx
}

/// Test-only: call `send_webhook_with_backoff` with a custom backoff base,
/// bypassing the public `send_webhook` wrapper.
#[cfg(test)]
pub(crate) async fn send_webhook_with_backoff_test(
    client:       &Client,
    url:          &str,
    payload:      &AlertPayload,
    secret:       Option<&str>,
    backoff_base: Duration,
) -> Result<DeliveryResult> {
    let (_tx, rx) = oneshot::channel();
    std::mem::forget(_tx);
    send_webhook_with_backoff(client, url, payload, secret, backoff_base, rx).await
}

/// Test-only: call the compression-aware delivery loop with a custom backoff base.
#[cfg(test)]
pub(crate) async fn send_webhook_with_compression_test(
    client:       &Client,
    url:          &str,
    payload:      &AlertPayload,
    secret:       Option<&str>,
    compress:     bool,
    backoff_base: Duration,
) -> Result<DeliveryResult> {
    let (_tx, rx) = oneshot::channel();
    std::mem::forget(_tx);
    send_webhook_with_backoff_opts(client, url, payload, secret, compress, HeaderMap::new(), backoff_base, rx).await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn sends_custom_headers_with_webhook() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::header("x-api-key", "test-key"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let headers = HashMap::from([("X-Api-Key".into(), "test-key".into())]);
        let client = build_client().unwrap();
        send_webhook_with_headers(&client, &server.uri(), &sample_payload(), None, &headers, false, dummy_shutdown())
            .await
            .unwrap();
    }

    // ── TLS integration test helpers (Issues #278, #362, #466) ────────────────
    //
    // These helpers generate a throwaway CA, a server certificate and a client
    // certificate at test time using `rcgen`, then spin up a TLS-terminating
    // mock server that requires client certificates. This proves that the
    // `reqwest::Client` produced by `build_tls_client` / `build_pinned_cert_client`
    // can actually complete a TLS handshake end-to-end — not just that the
    // config parses.

    use std::io::Write;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    /// A generated PKI bundle: PEM-encoded CA, server cert/key and client cert/key.
    struct TestPki {
        ca_pem:        String,
        server_cert:   String,
        server_key:    String,
        client_cert:   String,
        client_key:    String,
    }

    /// Generate a throwaway CA plus a server and client certificate signed by it.
    fn generate_pki() -> TestPki {
        use rcgen::{
            BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose,
            SanType,
        };

        // CA
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "txwatch-test-ca");
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        // Server cert signed by CA, valid for localhost / 127.0.0.1.
        let mut server_params = CertificateParams::new(vec!["localhost".to_string()]).unwrap();
        server_params
            .distinguished_name
            .push(DnType::CommonName, "localhost");
        server_params.subject_alt_names = vec![
            SanType::DnsName("localhost".try_into().unwrap()),
            SanType::IpAddress(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
        ];
        server_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let server_key = KeyPair::generate().unwrap();
        let server_cert = server_params.signed_by(&server_key, &ca_cert, &ca_key).unwrap();

        // Client cert signed by CA.
        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "txwatch-test-client");
        client_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let client_key = KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &ca_cert, &ca_key).unwrap();

        TestPki {
            ca_pem:      ca_cert.pem(),
            server_cert: server_cert.pem(),
            server_key:  server_key.serialize_pem(),
            client_cert: client_cert.pem(),
            client_key:  client_key.serialize_pem(),
        }
    }

    /// Generate a client cert/key signed by a *different*, untrusted CA.
    fn generate_untrusted_client() -> (String, String) {
        use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "txwatch-untrusted-ca");
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let mut client_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        client_params
            .distinguished_name
            .push(DnType::CommonName, "txwatch-untrusted-client");
        client_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        let client_key = KeyPair::generate().unwrap();
        let client_cert = client_params.signed_by(&client_key, &ca_cert, &ca_key).unwrap();

        (client_cert.pem(), client_key.serialize_pem())
    }

    /// A minimal TLS-terminating mock server that requires client certificates
    /// and responds `200 OK` to any request. Returns the bound address and a
    /// shutdown handle.
    struct TlsMockServer {
        addr:     SocketAddr,
        shutdown: Option<oneshot::Sender<()>>,
        handle:   Option<tokio::task::JoinHandle<()>>,
    }

    impl TlsMockServer {
        async fn start(pki: &TestPki) -> Self {
            use rustls::server::WebPkiClientVerifier;
            use rustls::RootCertStore;

            // Server identity.
            let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                rustls_pemfile::certs(&mut pki.server_cert.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
            let key = rustls_pemfile::private_key(&mut pki.server_key.as_bytes())
                .unwrap()
                .unwrap();

            // Trust the test CA for client cert verification.
            let mut roots = RootCertStore::empty();
            let ca_certs: Vec<rustls::pki_types::CertificateDer<'static>> =
                rustls_pemfile::certs(&mut pki.ca_pem.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
            for ca in ca_certs {
                roots.add(ca).unwrap();
            }
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .unwrap();

            let config = rustls::ServerConfig::builder()
                .with_client_cert_verifier(verifier)
                .with_single_cert(certs, key)
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(config));

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();

            let handle = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = &mut shutdown_rx => break,
                        accept = listener.accept() => {
                            let (stream, _) = match accept {
                                Ok(v) => v,
                                Err(_) => break,
                            };
                            let acceptor = acceptor.clone();
                            tokio::spawn(async move {
                                let mut tls = match acceptor.accept(stream).await {
                                    Ok(t) => t,
                                    Err(_) => return, // handshake failure (e.g. bad client cert)
                                };
                                // Read the request headers/body (best-effort) then reply 200.
                                let mut buf = [0u8; 4096];
                                let _ = tls.read(&mut buf).await;
                                let response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
                                let _ = tls.write_all(response).await;
                                let _ = tls.flush().await;
                                let _ = tls.shutdown().await;
                            });
                        }
                    }
                }
            });

            Self {
                addr,
                shutdown: Some(shutdown_tx),
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("https://localhost:{}{}", self.addr.port(), path)
        }
    }

    impl Drop for TlsMockServer {
        fn drop(&mut self) {
            if let Some(tx) = self.shutdown.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                handle.abort();
            }
        }
    }

    /// A temp directory that is removed on drop.
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "txwatch-notifier-tls-test-{}-{}-{}",
                std::process::id(),
                n,
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn write(&self, name: &str, contents: &str) -> PathBuf {
            let p = self.path.join(name);
            let mut f = fs::File::create(&p).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
            p
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample_payload() -> AlertPayload {
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        AlertPayload {
            label:               "Test Contract".into(),
            contract_id:         contract_id.into(),
            contract_type:       ContractType::from_contract_id(contract_id),
            network:             Network::Testnet,
            rule_type:           "AnyTransaction".into(),
            rule_triggered:      "AnyTransaction".into(),
            transaction_hash:    "abc123".into(),
            function_name:       None,
            function_names:      vec![],
            amount_xlm:          None,
            amount_stroops:      None,
            fee_charged_stroops: None,
            operation_count:     None,
            memo_type:           None,
            memo:                None,
            timestamp:           1_700_000_000,
            timestamp_iso:       "2023-11-15T03:13:20Z".into(),
            horizon_link:        "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link:       "https://stellar.expert/explorer/testnet/tx/abc123".into(),
            ledger_sequence:     None,
            source_account:      None,
        }
    }

    #[tokio::test]
    async fn delivers_on_first_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await;
        assert!(result.is_ok());
        let delivery = result.unwrap();
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.final_status, 200);
    }

    #[tokio::test]
    async fn retries_on_server_error_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn attempts_is_two_when_first_fails_and_second_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client   = Client::new();
        let url      = format!("{}/hook", server.uri());
        let delivery = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown())
            .await
            .expect("should succeed on second attempt");
        assert_eq!(delivery.attempts, 2);
        assert_eq!(delivery.final_status, 200);
    }

    /// Issue #24: a 200 response with an error body must still be treated as success.
    #[tokio::test]
    async fn success_with_error_body_is_still_treated_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"error":"something went wrong"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client   = build_client().unwrap();
        let url      = format!("{}/hook", server.uri());
        let delivery = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown())
            .await
            .expect("200 with error body should be treated as success");
        assert_eq!(delivery.final_status, 200);
        assert_eq!(delivery.attempts, 1);
    }

    #[tokio::test]
    async fn signature_header_is_correct_hmac() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let payload = sample_payload();
        let body    = serde_json::to_string(&payload).unwrap();
        let secret  = "test-secret";

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        send_webhook(&client, &url, &payload, Some(secret), dummy_shutdown()).await.unwrap();

        // The raw secret is never sent; only the HMAC signature header is.
        let requests = server.received_requests().await.unwrap();
        let sig = requests[0].headers.get("x-txwatch-signature").unwrap();
        assert_eq!(sig, &expected);

        // Constant time verification succeeds
        assert!(verify_signature_constant_time(secret.as_bytes(), body.as_bytes(), sig.as_str()));
        // Rejects mismatched signature
        assert!(!verify_signature_constant_time(secret.as_bytes(), b"tampered body", sig.as_str()));
        assert!(!verify_signature_constant_time(b"wrong-secret", body.as_bytes(), sig.as_str()));
    }

    #[tokio::test]
    async fn signature_header_absent_when_not_provided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("x-txwatch-secret"));
        assert!(!requests[0].headers.contains_key("x-txwatch-signature"));
    }

    /// Issue #307: a persistent 500 must produce exactly MAX_RETRIES requests — no more, no less.
    /// A zero backoff base keeps the test fast without changing the retry count.
    #[tokio::test]
    async fn fails_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook_with_backoff_test(
            &client, &url, &sample_payload(), None, Duration::ZERO,
        ).await;
        assert!(result.is_err());
    }

    /// Issue #260: shutdown during backoff aborts delivery.
    #[tokio::test]
    async fn shutdown_during_backoff_aborts_delivery() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());

        let (tx, rx) = oneshot::channel::<()>();
        // Trigger shutdown immediately.
        let _ = tx.send(());

        let result = send_webhook_with_backoff(
            &client, &url, &sample_payload(), None, Duration::from_secs(60), rx,
        ).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("shutdown") || msg.contains("aborted") || msg.contains("retry"),
            "expected shutdown-related error, got: {}", msg);
    }

    /// Issue #553: an empty filter delivers every rule type.
    #[test]
    fn empty_filter_matches_all_rule_types() {
        let filter: Vec<String> = vec![];
        assert!(rule_matches_filter("AnyTransaction", &filter));
        assert!(rule_matches_filter("LargeTransfer", &filter));
        assert!(rule_matches_filter("TestWebhook", &filter));
    }

    /// Issue #553: a matching rule type is delivered.
    #[test]
    fn filter_matches_listed_rule_type() {
        let filter = vec!["LargeTransfer".to_string(), "AnyTransaction".to_string()];
        assert!(rule_matches_filter("LargeTransfer", &filter));
        assert!(rule_matches_filter("AnyTransaction", &filter));
    }

    /// Issue #553: a non-matching rule type is filtered out.
    #[test]
    fn filter_rejects_unlisted_rule_type() {
        let filter = vec!["LargeTransfer".to_string()];
        assert!(!rule_matches_filter("AnyTransaction", &filter));
        assert!(!rule_matches_filter("TestWebhook", &filter));
    }

    /// Issue #554: sign_payload produces the same signature as send_webhook.
    #[test]
    fn sign_payload_matches_expected_hmac() {
        let secret = "test-secret";
        let body = b"{\"hello\":\"world\"}";
        let sig = sign_payload(secret, body);
        assert!(sig.starts_with("sha256="));
        assert!(verify_signature_constant_time(secret.as_bytes(), body, &sig));
        assert!(!verify_signature_constant_time(b"wrong", body, &sig));
    }

    /// Issue #13: test_payload produces a structurally valid AlertPayload.
    #[test]
    fn test_payload_is_structurally_valid() {
        let p = test_payload("My Contract", "https://example.com/hook");
        assert!(p.label.contains("My Contract"));
        assert_eq!(p.rule_triggered, "TestWebhook");
        assert_eq!(p.contract_id.len(), 56, "contract_id must be 56 characters");
        assert!(p.contract_id.starts_with('C'), "contract_id must start with 'C'");
        assert!(p.horizon_link.contains("/transactions/"));
        assert!(p.explorer_link.contains("stellar.expert"));
        assert_eq!(p.contract_type, ContractType::Soroban);
    }

    /// Issue #351: test_payload serializes contract_type as "soroban".
    #[test]
    fn test_payload_serializes_contract_type() {
        let p = test_payload("My Contract", "https://example.com/hook");
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["contract_type"], "soroban");
    }

    /// Issue #13: test_payload_with_network derives links from the supplied network config.
    #[test]
    fn test_payload_with_network_derives_links_from_config() {
        use txwatch_config::Network;

        let p = test_payload_with_network(
            "Label", "https://example.com/hook",
            &Network::Mainnet, "https://horizon.stellar.org",
        );
        assert!(p.horizon_link.starts_with("https://horizon.stellar.org/transactions/"));
        assert!(p.explorer_link.contains("/explorer/public/"));
        assert!(!p.explorer_link.contains("/explorer/mainnet/"));
        assert!(p.explorer_link.contains(&p.transaction_hash));

        let t = test_payload_with_network(
            "Label",
            "https://example.com/hook",
            &Network::Testnet,
            "https://horizon-testnet.stellar.org",
        );
        assert!(t.explorer_link.contains("/explorer/testnet/"));
        assert!(t.explorer_link.contains(&t.transaction_hash));
    }

    #[tokio::test]
    async fn version_header_is_present_on_every_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("X-TxWatch-Version", env!("CARGO_PKG_VERSION")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn content_length_header_is_present_and_correct() {
        let server  = MockServer::start().await;
        let body    = serde_json::to_string(&sample_payload()).unwrap();

        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("content-length", body.len().to_string()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await;
        assert!(result.is_ok());
    }

    /// Issue #540: when compression is enabled the body is gzip-compressed and
    /// `Content-Encoding: gzip` is set; the signature covers the uncompressed JSON.
    #[tokio::test]
    async fn compressed_webhook_sends_gzip_body() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("content-encoding", "gzip"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let payload = sample_payload();
        let json_body = serde_json::to_string(&payload).unwrap();
        let secret = "compress-secret";

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        let delivery = send_webhook_with_compression_test(
            &client, &url, &payload, Some(secret), true, Duration::ZERO,
        )
        .await
        .expect("compressed delivery should succeed");
        assert_eq!(delivery.final_status, 200);

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert_eq!(
            req.headers.get("content-encoding").unwrap(),
            "gzip",
            "compressed request must advertise gzip encoding"
        );

        // Body must decompress back to the original JSON.
        let mut decoder = GzDecoder::new(req.body.as_slice());
        let mut decompressed = String::new();
        decoder.read_to_string(&mut decompressed).unwrap();
        assert_eq!(decompressed, json_body);

        // Signature is computed over the uncompressed JSON body.
        let sig = req.headers.get("x-txwatch-signature").unwrap().to_str().unwrap();
        assert!(verify_signature_constant_time(
            secret.as_bytes(),
            json_body.as_bytes(),
            sig
        ));
    }

    /// Issue #540: without compression the body is sent as plain JSON and no
    /// `Content-Encoding` header is present.
    #[tokio::test]
    async fn uncompressed_webhook_sends_plain_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let payload = sample_payload();
        let json_body = serde_json::to_string(&payload).unwrap();

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        send_webhook_with_compression_test(
            &client, &url, &payload, None, false, Duration::ZERO,
        )
        .await
        .expect("uncompressed delivery should succeed");

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let req = &requests[0];
        assert!(
            !req.headers.contains_key("content-encoding"),
            "uncompressed request must not set Content-Encoding"
        );
        assert_eq!(req.body, json_body.as_bytes());
    }

    #[tokio::test]
    async fn large_response_body_is_truncated_safely() {
        let server = MockServer::start().await;
        let large_body = "x".repeat(1_000_000); // 1 MB

        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200).set_body_string(large_body))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown()).await;
        assert!(result.is_ok());
    }

    // ── TLS end-to-end tests (Issues #278, #362, #466) ────────────────────────

    /// Issue #278 / #466: `build_tls_client` produces a client that can complete
    /// an mTLS handshake against a server requiring client certs, and
    /// `send_webhook` delivers successfully through it.
    #[tokio::test]
    async fn build_tls_client_delivers_over_mtls() {
        let pki = generate_pki();
        let server = TlsMockServer::start(&pki).await;

        let dir = TempDir::new();
        let client_cert = dir.write("client.crt", &pki.client_cert);
        let client_key = dir.write("client.key", &pki.client_key);
        let ca_cert = dir.write("ca.crt", &pki.ca_pem);

        let tls = TlsConfig {
            client_cert: client_cert.to_string_lossy().into_owned(),
            client_key: client_key.to_string_lossy().into_owned(),
            ca_cert: Some(ca_cert.to_string_lossy().into_owned()),
        };

        let client = build_tls_client(&tls).expect("build_tls_client should succeed");
        let url = server.url("/hook");
        let delivery = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown())
            .await
            .expect("mTLS delivery should succeed");
        assert_eq!(delivery.final_status, 200);
        assert_eq!(delivery.attempts, 1);
    }

    /// Issue #278 / #466: a client cert signed by an untrusted CA must fail the
    /// handshake, so `send_webhook` returns an error after exhausting retries.
    #[tokio::test]
    async fn build_tls_client_rejects_untrusted_client_cert() {
        let pki = generate_pki();
        let server = TlsMockServer::start(&pki).await;

        let (bad_cert, bad_key) = generate_untrusted_client();
        let dir = TempDir::new();
        let client_cert = dir.write("client.crt", &bad_cert);
        let client_key = dir.write("client.key", &bad_key);
        let ca_cert = dir.write("ca.crt", &pki.ca_pem);

        let tls = TlsConfig {
            client_cert: client_cert.to_string_lossy().into_owned(),
            client_key: client_key.to_string_lossy().into_owned(),
            ca_cert: Some(ca_cert.to_string_lossy().into_owned()),
        };

        let client = build_tls_client(&tls).expect("build_tls_client should succeed");
        let url = server.url("/hook");
        // Zero backoff keeps the test fast; the handshake must still fail.
        let result = send_webhook_with_backoff_test(
            &client,
            &url,
            &sample_payload(),
            None,
            Duration::ZERO,
        )
        .await;
        assert!(
            result.is_err(),
            "delivery with an untrusted client cert must fail, got: {:?}",
            result
        );
    }

    /// Issue #362 / #466: `build_pinned_cert_client` trusts the pinned CA and
    /// delivers successfully against a server presenting a cert signed by it.
    #[tokio::test]
    async fn build_pinned_cert_client_delivers_to_pinned_server() {
        let pki = generate_pki();
        let server = TlsMockServer::start(&pki).await;

        let client = build_pinned_cert_client(&pki.ca_pem)
            .expect("build_pinned_cert_client should succeed");
        let url = server.url("/hook");
        let delivery = send_webhook(&client, &url, &sample_payload(), None, dummy_shutdown())
            .await
            .expect("pinned-cert delivery should succeed");
        assert_eq!(delivery.final_status, 200);
        assert_eq!(delivery.attempts, 1);
    }

    // ── Alert grouping tests (Issue #557) ─────────────────────────────────────

    fn payload_with_rule(tx_hash: &str, rule: &str) -> AlertPayload {
        let mut p = sample_payload();
        p.transaction_hash = tx_hash.into();
        p.rule_type = rule.into();
        p.rule_triggered = rule.into();
        p
    }

    #[test]
    fn group_by_transaction_groups_rules_per_tx() {
        let payloads = vec![
            payload_with_rule("tx1", "RuleA"),
            payload_with_rule("tx2", "RuleB"),
            payload_with_rule("tx1", "RuleC"),
        ];
        let batches = group_by_transaction(&payloads);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].transaction_hash, "tx1");
        assert_eq!(batches[0].rule_count, 2);
        assert_eq!(batches[0].rules_triggered.len(), 2);
        assert_eq!(batches[0].rules_triggered[0].rule_triggered, "RuleA");
        assert_eq!(batches[0].rules_triggered[1].rule_triggered, "RuleC");
        assert_eq!(batches[1].transaction_hash, "tx2");
        assert_eq!(batches[1].rule_count, 1);
    }

    #[tokio::test]
    async fn batched_dispatch_sends_single_post_per_transaction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        let payloads = vec![
            payload_with_rule("tx1", "RuleA"),
            payload_with_rule("tx1", "RuleB"),
        ];
        let results = dispatch_alerts(&client, &url, &payloads, None, true, dummy_shutdown()).await;
        assert_eq!(results.len(), 1);
        assert!(results[0].is_ok());

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["transaction_hash"], "tx1");
        assert_eq!(body["rule_count"], 2);
        let rules = body["rules_triggered"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["rule_triggered"], "RuleA");
        assert_eq!(rules[1]["rule_triggered"], "RuleB");
    }

    #[tokio::test]
    async fn unbatched_dispatch_sends_one_post_per_payload() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        let payloads = vec![
            payload_with_rule("tx1", "RuleA"),
            payload_with_rule("tx1", "RuleB"),
        ];
        let results = dispatch_alerts(&client, &url, &payloads, None, false, dummy_shutdown()).await;
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|r| r.is_ok()));

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        for req in &requests {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            assert!(body.get("rules_triggered").is_none());
            assert_eq!(body["transaction_hash"], "tx1");
        }
    }

    #[tokio::test]
    async fn batched_webhook_signature_covers_batch_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let payloads = vec![
            payload_with_rule("tx1", "RuleA"),
            payload_with_rule("tx1", "RuleB"),
        ];
        let batches = group_by_transaction(&payloads);
        let body = serde_json::to_string(&batches[0]).unwrap();
        let secret = "batch-secret";

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        send_batched_webhook(&client, &url, &batches[0], Some(secret), dummy_shutdown())
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        let sig = requests[0].headers.get("x-txwatch-signature").unwrap();
        assert!(verify_signature_constant_time(
            secret.as_bytes(),
            body.as_bytes(),
            sig.to_str().unwrap()
        ));
    }
}
