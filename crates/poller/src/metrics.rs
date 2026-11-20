//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes eight counters:
//! - `txwatch_transactions_total`       — total transactions processed
//! - `txwatch_transactions_per_contract` — transactions processed, labelled by `contract`
//! - `txwatch_alerts_total`             — total alert payloads sent (rules matched)
//! - `txwatch_alerts_suppressed_total`  — total alerts suppressed by the per-cycle cap (labelled by `contract`)
//! - `txwatch_webhook_failures_total`   — total permanent webhook delivery failures
//! - `txwatch_enrichment_errors_total`  — transactions skipped because enrichment failed
//! - `txwatch_poll_cycles_total`        — total poll cycles per contract (labelled by `contract`)
//! - `txwatch_cursor_save_failures_total` — total failed cursor-file persistence attempts
//! - `txwatch_circuit_breaker_open_total` — total times a contract entered circuit-breaker state
//! - `txwatch_horizon_errors_total`     — total Horizon HTTP errors (labelled by `status_code`)
//!
//! And one histogram:
//! - `txwatch_horizon_latency_seconds`  — Horizon API request duration, labelled by `endpoint`
//!
//! An optional HTTP `/metrics` endpoint can be started by calling [`serve_metrics`].
//! It serves the Prometheus text exposition format on the configured bind address.

use anyhow::{Context, Result};
use prometheus::{register_int_counter, register_int_counter_vec, IntCounter, IntCounterVec};
use std::{net::SocketAddr, sync::OnceLock};

use http_body_util::Full;
use hyper::{body::Bytes, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

// ── Counters ──────────────────────────────────────────────────────────────────

fn transactions_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_transactions_total",
            "Total Stellar transactions processed across all watched contracts"
        )
        .expect("register txwatch_transactions_total")
    })
}

fn alerts_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_alerts_total",
            "Total alert payloads sent (rules matched)"
        )
        .expect("register txwatch_alerts_total")
    })
}

fn alerts_suppressed_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_alerts_suppressed_total",
            "Total alerts suppressed because max_alerts_per_cycle was reached (see issue #570)",
            &["contract"]
        )
        .expect("register txwatch_alerts_suppressed_total")
    })
}

fn enrichment_errors_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_enrichment_errors_total",
            "Transactions skipped because enrichment failed (the cursor still advances past them)"
        )
        .expect("register txwatch_enrichment_errors_total")
    })
}

fn webhook_failures_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_webhook_failures_total",
            "Total permanent webhook delivery failures (after all retries)"
        )
        .expect("register txwatch_webhook_failures_total")
    })
}

fn config_reload_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        let counter = register_int_counter!(
            "txwatch_config_reload_total",
            "Total configuration loads and reloads (starts at 1 for initial startup; see issue #227)"
        )
        .expect("register txwatch_config_reload_total");
        counter.inc(); // Starts at 1 for initial load
        counter
    })
}

fn cursor_save_failures_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_cursor_save_failures_total",
            "Total failed attempts to persist the cursor file (see issue #460)"
        )
        .expect("register txwatch_cursor_save_failures_total")
    })
}

fn circuit_breaker_open_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_circuit_breaker_open_total",
            "Total times a contract entered circuit-breaker state after consecutive Horizon failures (see issue #374)"
        )
        .expect("register txwatch_circuit_breaker_open_total")
    })
}

fn horizon_rate_limits_total() -> &'static IntCounter {
    static C: OnceLock<IntCounter> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter!(
            "txwatch_horizon_rate_limits_total",
            "Total Horizon 429 Too Many Requests responses (see issue #574)"
        )
        .expect("register txwatch_horizon_rate_limits_total")
    })
}

fn poll_cycles_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_poll_cycles_total",
            "Total poll cycles per contract (incremented after each poll attempt, success or failure)",
            &["contract"]
        )
        .expect("register txwatch_poll_cycles_total")
    })
}

fn horizon_errors_total() -> &'static IntCounterVec {
    static C: OnceLock<IntCounterVec> = OnceLock::new();
    C.get_or_init(|| {
        register_int_counter_vec!(
            "txwatch_horizon_errors_total",
            "Total Horizon HTTP errors by status code (see issue #581)",
            &["status_code"]
        )
        .expect("register txwatch_horizon_errors_total")
    })
}

/// Increment the `txwatch_transactions_total` counter by `n`.
pub fn inc_transactions(n: u64) {
    transactions_total().inc_by(n);
}

/// Increment the `txwatch_alerts_total` counter by `n`.
pub fn inc_alerts(n: u64) {
    alerts_total().inc_by(n);
}

/// Increment the `txwatch_alerts_suppressed_total` counter for `contract` by `n`.
///
/// Call this whenever one or more alerts are dropped because the per-cycle
/// `max_alerts_per_cycle` cap was reached (issue #570).
pub fn inc_alerts_suppressed(contract: &str, n: u64) {
    alerts_suppressed_total()
        .with_label_values(&[contract])
        .inc_by(n);
}

/// Increment the `txwatch_webhook_failures_total` counter by 1.
pub fn inc_webhook_failures() {
    webhook_failures_total().inc();
}

/// Increment the `txwatch_enrichment_errors_total` counter by 1.
pub fn inc_enrichment_errors() {
    enrichment_errors_total().inc();
}

/// Increment the `txwatch_config_reload_total` counter by 1.
/// Reference: Hot-reload on SIGHUP support (issue #227).
pub fn inc_config_reload() {
    config_reload_total().inc();
}

/// Increment the `txwatch_cursor_save_failures_total` counter by 1.
///
/// Call this whenever persisting the cursor file fails, so that persistent
/// cursor-persistence breakage is visible to operators (issue #460).
pub fn inc_cursor_save_failures() {
    cursor_save_failures_total().inc();
}

/// Increment the `txwatch_circuit_breaker_open_total` counter by 1.
///
/// Call this whenever a contract transitions into the circuit-breaker open
/// state after consecutive Horizon failures (issue #374).
pub fn inc_circuit_breaker_open() {
    circuit_breaker_open_total().inc();
}

/// Increment the `txwatch_horizon_rate_limits_total` counter by 1.
///
/// Call this whenever Horizon responds with HTTP 429 Too Many Requests so that
/// rate limiting is observable separately from other errors (issue #574).
pub fn inc_horizon_rate_limits() {
    horizon_rate_limits_total().inc();
}

/// Increment the `txwatch_poll_cycles_total` counter for `contract` by 1.
///
/// Call this after each poll attempt, whether it succeeds or fails, so that a
/// stalled contract can be detected by the counter no longer incrementing.
pub fn inc_poll_cycle(contract: &str) {
    poll_cycles_total().with_label_values(&[contract]).inc();
}

/// Increment the `txwatch_horizon_errors_total` counter for `status_code` by 1.
///
/// Call this whenever a Horizon request returns a non-success HTTP status, so
/// that operators can distinguish e.g. 404 from 500 errors in dashboards
/// (issue #581).
pub fn inc_horizon_error(status_code: u16) {
    let label = status_code.to_string();
    horizon_errors_total().with_label_values(&[label.as_str()]).inc();
}

// ── /metrics HTTP endpoint ────────────────────────────────────────────────────

/// Serve the Prometheus `/metrics` endpoint on `addr`.
/// Spawns a background task and returns immediately.
pub async fn serve_metrics(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind metrics endpoint on {}", addr))?;

    tracing::info!(addr = %addr, "Prometheus /metrics endpoint listening");

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let io = TokioIo::new(stream);
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(io, hyper::service::service_fn(handle_metrics))
                    .await;
            });
        }
    });

    Ok(())
}

async fn handle_metrics(
    _req: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    init_build_info();
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&prometheus::gather(), &mut buf)
        .unwrap_or_default();
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", "text/plain; version=0.0.4; charset=utf-8")
        .body(Full::new(Bytes::from(buf)))
        .unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metrics_endpoint_content_type_header() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        serve_metrics(addr).await.unwrap();

        let client = reqwest::Client::new();
        let resp = client
            .get(format!("http://{}/metrics", addr))
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.starts_with("text/plain"), "unexpected content-type: {ct}");
    }

    #[test]
    fn horizon_errors_counter_registers_with_status_code_label() {
        inc_horizon_error(404);
        inc_horizon_error(500);
        let families = prometheus::gather();
        let family = families
            .iter()
            .find(|f| f.get_name() == "txwatch_horizon_errors_total")
            .expect("txwatch_horizon_errors_total should be registered");
        let labels: Vec<String> = family
            .get_metric()
            .iter()
            .flat_map(|m| m.get_label().iter().map(|l| l.get_name().to_string()))
            .collect();
        assert!(labels.iter().any(|l| l == "status_code"));
    }
}
