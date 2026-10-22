//! Prometheus metrics for TxWatch (enabled with the `metrics` feature flag).
//!
//! Exposes five counters:
//! - `txwatch_transactions_total`       — total transactions processed
//! - `txwatch_alerts_total`             — total alert payloads sent (rules matched)
//! - `txwatch_webhook_failures_total`   — total permanent webhook delivery failures
//! - `txwatch_enrichment_errors_total`  — transactions skipped because enrichment failed
//! - `txwatch_poll_cycles_total`        — total poll cycles per contract (labelled by `contract`)
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

/// Increment the `txwatch_poll_cycles_total` counter for `contract` by 1.
///
/// Call this after each poll attempt, whether it succeeds or fails, so that a
/// stalled contract can be detected by the counter no longer incrementing.
pub fn inc_poll_cycle(contract: &str) {
    poll_cycles_total().with_label_values(&[contract]).inc();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poll_cycles_increment_per_contract() {
        let before_a = poll_cycles_total()
            .with_label_values(&["contract-a"])
            .get();
        let before_b = poll_cycles_total()
            .with_label_values(&["contract-b"])
            .get();

        inc_poll_cycle("contract-a");
        inc_poll_cycle("contract-a");
        inc_poll_cycle("contract-b");

        assert_eq!(
            poll_cycles_total()
                .with_label_values(&["contract-a"])
                .get(),
            before_a + 2
        );
        assert_eq!(
            poll_cycles_total()
                .with_label_values(&["contract-b"])
                .get(),
            before_b + 1
        );
    }
}
