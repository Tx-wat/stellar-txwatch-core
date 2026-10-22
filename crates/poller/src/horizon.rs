//! Horizon REST API response shapes.
//!
//! These types are used to deserialise the JSON returned by the Horizon API.
//! They are intentionally minimal — only the fields TxWatch needs are captured;
//! unknown fields are ignored so the code stays forward-compatible.

use serde::Deserialize;
use std::time::Instant;
use txwatch_rules::{HorizonOperation, HorizonTransaction};

// ── Latency metric ───────────────────────────────────────────────────────────

/// Name of the histogram tracking Horizon request duration in seconds.
pub(crate) const HORIZON_LATENCY_METRIC: &str = "txwatch_horizon_latency_seconds";

/// `le` bucket bounds (seconds) suitable for HTTP API latency.
pub(crate) const HORIZON_LATENCY_BUCKETS: &[f64] = &[0.1, 0.25, 0.5, 1.0, 2.5, 5.0];

/// Records the duration of a Horizon request against the
/// `txwatch_horizon_latency_seconds` histogram, labelled by `endpoint`.
///
/// `endpoint` is one of `"transactions"` or `"operations"`.
pub(crate) fn observe_horizon_latency(endpoint: &str, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64();
    metrics::histogram!(HORIZON_LATENCY_METRIC, "endpoint" => endpoint.to_string())
        .record(elapsed);
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

/// Fetch a page of transactions for `account_id`, recording the request
/// duration under the `transactions` endpoint label.
pub(crate) async fn fetch_transactions_page(
    client: &reqwest::Client,
    base_url: &str,
    account_id: &str,
) -> Result<HorizonPage, reqwest::Error> {
    let started = Instant::now();
    let url = format!("{base_url}/accounts/{account_id}/transactions?join=operations");
    let result = client.get(&url).send().await?.error_for_status()?.json().await;
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
    let result = client.get(&url).send().await?.error_for_status()?.json().await;
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

    #[tokio::test]
    async fn startup_check_does_not_abort_on_failure() {
        let base_url = spawn_status_server(503).await;
        let client = reqwest::Client::new();
        // Must return normally (warn only) even when the endpoint is unhealthy.
        startup_connectivity_check(&client, &[base_url], true).await;
    }

    #[tokio::test]
    async fn startup_check_skipped_when_disabled() {
        let client = reqwest::Client::new();
        // Unreachable URL is fine because the check is disabled.
        startup_connectivity_check(&client, &["http://127.0.0.1:1".to_string()], false).await;
    }
}
