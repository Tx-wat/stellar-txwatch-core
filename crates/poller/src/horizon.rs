//! Horizon REST API response shapes.
//!
//! These types are used to deserialise the JSON returned by the Horizon API.
//! They are intentionally minimal — only the fields TxWatch needs are captured;
//! unknown fields are ignored so the code stays forward-compatible.

use serde::Deserialize;
use std::time::{Duration, Instant};
use txwatch_rules::{HorizonOperation, HorizonTransaction};

// ── Latency metric ───────────────────────────────────────────────────────────

/// Name of the histogram tracking Horizon request duration in seconds.
pub(crate) const HORIZON_LATENCY_METRIC: &str = "txwatch_horizon_latency_seconds";

/// `le` bucket bounds (seconds) suitable for HTTP API latency.
pub(crate) const HORIZON_LATENCY_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Name of the counter tracking Horizon 429 rate-limit responses.
pub(crate) const HORIZON_RATE_LIMITS_METRIC: &str = "txwatch_horizon_rate_limits_total";

/// Default backoff applied when Horizon returns 429 without a `Retry-After`
/// header.
pub(crate) const HORIZON_DEFAULT_RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(1);

/// Records the duration of a Horizon request against the
/// `txwatch_horizon_latency_seconds` histogram, labelled by `endpoint`.
///
/// `endpoint` is one of `"transactions"` or `"operations"`.
pub(crate) fn observe_horizon_latency(endpoint: &str, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64();
    metrics::histogram!(HORIZON_LATENCY_METRIC, "endpoint" => endpoint.to_string())
        .record(elapsed);
}

/// Increments the `txwatch_horizon_rate_limits_total` counter, labelled by
/// `endpoint`.
pub(crate) fn observe_horizon_rate_limit(endpoint: &str) {
    metrics::counter!(HORIZON_RATE_LIMITS_METRIC, "endpoint" => endpoint.to_string())
        .increment(1);
}

/// Parse a `Retry-After` header value into a backoff duration.
///
/// Horizon sends the delay in seconds. Falls back to
/// [`HORIZON_DEFAULT_RATE_LIMIT_BACKOFF`] when the header is missing or cannot
/// be parsed.
pub(crate) fn retry_after_backoff(headers: &reqwest::header::HeaderMap) -> Duration {
    headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(HORIZON_DEFAULT_RATE_LIMIT_BACKOFF)
}

// ── Account kind ─────────────────────────────────────────────────────────────

/// The kind of Stellar address being watched.
///
/// Soroban contract addresses (C-addresses) and classic payment accounts
/// (G-addresses) are both served by the same Horizon
/// `/accounts/{id}/transactions` endpoint, so the polling URL is identical for
/// both. This enum exists so callers can reason about the address type without
/// re-parsing the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountKind {
    /// A Soroban contract address (starts with `C`).
    Contract,
    /// A classic Stellar account (starts with `G`).
    Classic,
}

impl AccountKind {
    /// Classify a 56-character Stellar address by its leading character.
    ///
    /// Returns `None` for anything that is not a well-formed C- or G-address.
    pub(crate) fn from_address(address: &str) -> Option<Self> {
        if address.len() != 56 {
            return None;
        }
        match address.as_bytes().first() {
            Some(b'C') => Some(AccountKind::Contract),
            Some(b'G') => Some(AccountKind::Classic),
            _ => None,
        }
    }
}

// ── Transactions endpoint ─────────────────────────────────────────────────────

/// A Horizon transaction record that may include inline operations returned
/// when the `join=operations` query parameter is used.
#[derive(Deserialize)]
pub(crate) struct HorizonTransactionWithOps {
    #[serde(flatten)]
    pub tx: HorizonTransaction,
    /// Inline operations embedded by Horizon when `join=operations` is used.
    #[serde(default)]
    pub operations: Vec<HorizonOperation>,
}

/// Top-level response envelope for `GET /accounts/{id}/transactions`.
///
/// This shape is returned for both C-addresses (contracts) and G-addresses
/// (classic accounts), so the same polling path works for both.
#[derive(Deserialize)]
pub(crate) struct HorizonPage {
    pub _embedded: Embedded,
}

/// `_embedded` wrapper inside a Horizon page response.
#[derive(Deserialize)]
pub(crate) struct Embedded {
    pub records: Vec<HorizonTransactionWithOps>,
}

// ── Operations endpoint ───────────────────────────────────────────────────────

/// Top-level response envelope for `GET /transactions/{hash}/operations`.
#[derive(Deserialize)]
pub(crate) struct OperationsPage {
    pub _embedded: OpsEmbedded,
}

/// `_embedded` wrapper inside an operations page response.
#[derive(Deserialize)]
pub(crate) struct OpsEmbedded {
    pub records: Vec<HorizonOperation>,
}

// ── Instrumented fetches ──────────────────────────────────────────────────────

/// Send a GET request, handling Horizon 429 rate-limit responses separately
/// from other errors.
///
/// On 429 the `txwatch_horizon_rate_limits_total` counter is incremented, the
/// request waits for the `Retry-After` header value (or a default backoff) and
/// retries once. Any other non-success status is surfaced as a generic error.
async fn get_with_rate_limit_retry(
    client: &reqwest::Client,
    url: &str,
    endpoint: &str,
) -> Result<reqwest::Response, reqwest::Error> {
    let response = client.get(url).send().await?;
    if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        observe_horizon_rate_limit(endpoint);
        let backoff = retry_after_backoff(response.headers());
        tracing::warn!(
            endpoint,
            backoff_secs = backoff.as_secs_f64(),
            "Horizon rate limit hit (429); backing off before retry"
        );
        tokio::time::sleep(backoff).await;
        let retry = client.get(url).send().await?;
        if retry.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            observe_horizon_rate_limit(endpoint);
        }
        return retry.error_for_status();
    }
    response.error_for_status()
}

/// Fetch a page of transactions for `account_id`, recording the request
/// duration under the `transactions` endpoint label.
pub(crate) async fn fetch_transactions_page(
    client: &reqwest::Client,
    base_url: &str,
    account_id: &str,
) -> Result<HorizonPage, reqwest::Error> {
    let started = Instant::now();
    let url = format!("{base_url}/accounts/{account_id}/transactions?join=operations");
    let result = get_with_rate_limit_retry(client, &url, "transactions")
        .await?
        .json()
        .await;
    observe_horizon_latency("transactions", started);
    result
}

/// Fetch the Soroban operation details for a transaction, recording the request
/// duration under the `operations` endpoint label.
pub(crate) async fn fetch_soroban_details(
    client: &reqwest::Client,
    base_url: &str,
    tx_hash: &str,
) -> Result<OperationsPage, reqwest::Error> {
    let started = Instant::now();
    let url = format!("{base_url}/transactions/{tx_hash}/operations");
    let result = get_with_rate_limit_retry(client, &url, "operations")
        .await?
        .json()
        .await;
    observe_horizon_latency("operations", started);
    result
}

// ── Startup connectivity check ────────────────────────────────────────────────

/// Probe a single Horizon base URL with `GET /fee_stats`.
///
/// Returns `Ok(())` when the endpoint answers with a success status, and an
/// error describing the failure otherwise. This is used by the startup
/// connectivity check to surface misconfiguration early; callers log a warning
/// on failure rather than aborting startup.
pub(crate) async fn check_connectivity(
    client: &reqwest::Client,
    base_url: &str,
) -> Result<(), reqwest::Error> {
    let url = format!("{base_url}/fee_stats");
    client.get(&url).send().await?.error_for_status()?;
    Ok(())
}

/// Run the startup connectivity check against each unique Horizon base URL.
///
/// A single request is sent per unique URL. Failures are logged as warnings and
/// never abort startup. When `enabled` is `false` the check is skipped entirely
/// (controlled by the `horizon_startup_check` config flag).
pub(crate) async fn startup_connectivity_check(
    client: &reqwest::Client,
    base_urls: &[String],
    enabled: bool,
) {
    if !enabled {
        return;
    }

    let mut seen: Vec<&str> = Vec::new();
    for base_url in base_urls {
        let base_url = base_url.as_str();
        if seen.contains(&base_url) {
            continue;
        }
        seen.push(base_url);

        if let Err(err) = check_connectivity(client, base_url).await {
            tracing::warn!(
                horizon_url = base_url,
                error = %err,
                "Horizon startup connectivity check failed; continuing anyway"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal HTTP server that answers every request with a fixed status code.
    async fn spawn_status_server(status: u16) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    /// Server that answers the first request with 429 (including a
    /// `Retry-After` header) and every subsequent request with 200.
    async fn spawn_rate_limit_then_ok_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));

        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let hits = hits.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let first = hits.fetch_add(1, Ordering::SeqCst) == 0;
                    let response = if first {
                        "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
                    };
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn connectivity_check_succeeds_on_200() {
        let base_url = spawn_status_server(200).await;
        let client = reqwest::Client::new();
        assert!(check_connectivity(&client, &base_url).await.is_ok());
    }

    #[tokio::test]
    async fn connectivity_check_fails_on_503() {
        let base_url = spawn_status_server(503).await;
        let client = reqwest::Client::new();
        assert!(check_connectivity(&client, &base_url).await.is_err());
    }

    #[test]
    fn retry_after_backoff_uses_header_value() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after_backoff(&headers), Duration::from_secs(7));
    }

    #[test]
    fn retry_after_backoff_defaults_when_missing() {
        let headers = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after_backoff(&headers), HORIZON_DEFAULT_RATE_LIMIT_BACKOFF);
    }

    #[tokio::test]
    async fn rate_limit_429_backs_off_and_retries() {
        let base_url = spawn_rate_limit_then_ok_server().await;
        let client = reqwest::Client::new();
        let url = format!("{base_url}/fee_stats");
        let result = get_with_rate_limit_retry(&client, &url, "transactions").await;
        assert!(result.is_ok(), "429 should be retried and succeed on 200");
    }
}
