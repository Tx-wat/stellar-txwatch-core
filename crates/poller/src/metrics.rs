//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes four counters:
//! - `txwatch_transactions_total`       — total transactions processed
//! - `txwatch_alerts_total`             — total alert payloads sent (rules matched)
//! - `txwatch_webhook_failures_total`   — total permanent webhook delivery failures
//! - `txwatch_enrichment_errors_total`  — transactions skipped because enrichment failed
//!
//! And one histogram:
//! - `txwatch_horizon_latency_seconds`  — Horizon API request duration, labelled by `endpoint`
//!
//! An optional HTTP `/metrics` endpoint can be started by calling [`serve_metrics`].
//! It serves the Prometheus text exposition format on the configured bind address.

use anyhow::{Context, Result};
use prometheus::{register_histogram_vec, register_int_counter, HistogramVec, IntCounter};
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

// ── Histograms ────────────────────────────────────────────────────────────────

fn horizon_latency_seconds() -> &'static HistogramVec {
    static H: OnceLock<HistogramVec> = OnceLock::new();
    H.get_or_init(|| {
        register_histogram_vec!(
            "txwatch_horizon_latency_seconds",
            "Horizon API request duration in seconds, labelled by endpoint",
            &["endpoint"],
            vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0]
        )
        .expect("register txwatch_horizon_latency_seconds")
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

/// Increment the `txwatch_webhook_failures_total` counter by 1.
pub fn inc_webhook_failures() {
    webhook_failures_total().inc();
}

/// Increment the `txwatch_enrichment_errors_total` counter by 1.
pub fn inc_enrichment_errors() {
    enrichment_errors_total().inc();
}

/// Record a Horizon request duration (in seconds) for the given `endpoint`
/// label (e.g. `"transactions"` or `"operations"`).
pub fn observe_horizon_latency(endpoint: &str, seconds: f64) {
    horizon_latency_seconds()
        .with_label_values(&[endpoint])
        .observe(seconds);
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
    use prometheus::Encoder;
    let encoder = prometheus::TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&prometheus::gather(), &mut buf)
        .unwrap_or_default();
    Ok(Response::builder()
        .status(200)
        .header("Content-Type", encoder.format_type())
        .body(Full::new(Bytes::from(buf)))
        .unwrap())
}
