#![forbid(unsafe_code)]

//! txwatch-notifier delivers webhook payloads over HTTP with retry and backoff,
//! exposing `send_webhook` and `test_payload` helpers for webhook delivery.

use anyhow::{anyhow, Result};
use chrono::Utc;
use hex;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tracing::{debug, error, info, span, warn, Level};
use txwatch_config::TlsConfig;
use txwatch_rules::AlertPayload;

const MAX_RETRIES: u32 = 3;

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

/// Build a shared HTTP client with sensible defaults.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {}", e))
}

/// Build a client for a contract with `[contracts.tls]` set. The client presents
/// the configured mTLS identity and, when `ca_cert` is set, trusts that CA bundle
/// in addition to the system roots. Files are read once, here, at startup.
/// Error messages name the file path but never include file contents.
pub fn build_tls_client(tls: &TlsConfig) -> Result<Client> {
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

    let mut builder = Client::builder()
        .timeout(Duration::from_secs(15))
        .identity(identity);
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
/// Returns a [`DeliveryResult`] describing how many attempts were needed.
pub async fn send_webhook(
    client:  &Client,
    url:     &str,
    payload: &AlertPayload,
    secret:  Option<&str>,
) -> Result<DeliveryResult> {
    let span = span!(
        Level::INFO,
        "send_webhook",
        contract = %payload.label,
        rule     = %payload.rule_triggered
    );
    let _enter = span.enter();

    let body = serde_json::to_string(payload)?;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-TxWatch-Version", env!("CARGO_PKG_VERSION"))
            .body(body.clone());

        if let Some(s) = secret {
            let mut mac = Hmac::<Sha256>::new_from_slice(s.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(body.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            req = req.header("X-TxWatch-Signature", format!("sha256={}", sig));
        }

        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let final_status = resp.status().as_u16();
                span.record("status", final_status);
                // Issue #24: log response body at debug level.
                // HTTP 2xx = delivery success regardless of body content.
                let body_text = resp.text().await.unwrap_or_default();
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
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    status    = %status,
                    "webhook attempt failed with HTTP error"
                );
                last_err = Some(anyhow!("HTTP {}", status));
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
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
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

/// Build a synthetic `AlertPayload` suitable for `test-webhook`.
pub fn test_payload(label: &str, webhook_url: &str) -> AlertPayload {
    test_payload_with_network(label, webhook_url, &Network::Testnet, "https://horizon-testnet.stellar.org")
}

/// Build a synthetic `AlertPayload` with an explicit network name and Horizon base URL.
pub fn test_payload_with_network(
    label:            &str,
    webhook_url:      &str,
    network:          &str,
    horizon_base_url: &str,
) -> AlertPayload {
    let now     = Utc::now();
    let tx_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    // Stellar Expert names the mainnet explorer path "public".
    let explorer_network = if network == "mainnet" { "public" } else { network };
    AlertPayload {
        label:               label.to_string(),
        contract_id:         "CTEST000000000000000000000000000000000000000000000000000".into(),
        network:             network.clone(),
        rule_type:           "TestWebhook".into(),
        rule_triggered:      "TestWebhook".into(),
        transaction_hash:    tx_hash.into(),
        function_name:       Some("test".into()),
        function_names:      vec!["test".into()],
        amount_xlm:          None,
        amount_stroops:      None,
        fee_charged_stroops: None,
        memo_type:           None,
        memo:                None,
        timestamp:           now.timestamp(),
        timestamp_iso:       now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        horizon_link:        format!("{}/transactions/{}", horizon_base_url, tx_hash),
        explorer_link:       format!("https://stellar.expert/explorer/{}/tx/{}", explorer_network, tx_hash),
    }
    .with_label(format!("{} (test-webhook to {})", label, webhook_url))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_payload() -> AlertPayload {
        AlertPayload {
            label:               "Test Contract".into(),
            contract_id:         "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:             Network::Testnet,
            rule_type:           "AnyTransaction".into(),
            rule_triggered:      "AnyTransaction".into(),
            transaction_hash:    "abc123".into(),
            function_name:       None,
            function_names:      vec![],
            amount_xlm:          None,
            amount_stroops:      None,
            fee_charged_stroops: None,
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
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
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
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
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
        let delivery = send_webhook(&client, &url, &sample_payload(), None)
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
        let delivery = send_webhook(&client, &url, &sample_payload(), None)
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
        send_webhook(&client, &url, &payload, Some(secret)).await.unwrap();

        // The raw secret is never sent; only the HMAC signature header is.
        let requests = server.received_requests().await.unwrap();
        let sig = requests[0].headers.get("x-txwatch-signature").unwrap();
        assert_eq!(sig, &expected);
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
        send_webhook(&client, &url, &sample_payload(), None).await.unwrap();

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
        let result = send_webhook_with_backoff(
            &client, &url, &sample_payload(), None, Duration::ZERO,
        ).await;
        assert!(result.is_err());
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
    }

    /// Issue #13: test_payload_with_network derives links from the supplied network config.
    #[test]
    fn test_payload_with_network_derives_links_from_config() {
        let p = test_payload_with_network(
            "Label", "https://example.com/hook",
            "mainnet", "https://horizon.stellar.org",
        );
        assert!(p.horizon_link.starts_with("https://horizon.stellar.org/transactions/"));
        assert!(p.explorer_link.contains("/explorer/public/"));
        assert!(!p.explorer_link.contains("/explorer/mainnet/"));
        assert!(p.explorer_link.contains(&p.transaction_hash));

        let t = test_payload_with_network(
            "Label",
            "https://example.com/hook",
            "testnet",
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
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
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
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }
}
