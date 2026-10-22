//! Core polling loop.
//!
//! `run` / `run_with` / `run_with_shutdown` are the public entry points.
//! `poll_contract` handles one contract per call and is kept `pub(crate)` so
//! the integration tests in `tests/` can drive it directly.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::Client;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use txwatch_config::{AppConfig, WatchedContract, MAX_CONTRACTS};
use txwatch_notifier::send_webhook;
use txwatch_rules::{evaluate, extract_soroban_details, EnrichedTransaction};

use crate::cursor::CursorMap;
use crate::enrichment::fetch_soroban_details;
use crate::horizon::HorizonPage;

// ── Summary counters ──────────────────────────────────────────────────────────

#[derive(Default)]
pub(crate) struct Counters {
    pub transactions: AtomicU64,
    pub alerts: AtomicU64,
    pub interval_transactions: AtomicU64,
    pub interval_alerts: AtomicU64,
}

// ── Startup connectivity check ────────────────────────────────────────────────

/// Send one `GET /fee_stats` to each unique Horizon base URL and warn (never
/// error) if any check fails. Never aborts startup — misconfiguration is
/// surfaced early but the poll loop still runs.
pub(crate) async fn check_horizon_connectivity(client: &Client, cfg: &AppConfig) {
    let mut urls: Vec<&str> = cfg
        .contracts
        .iter()
        .map(|c| c.network.horizon_base_url())
        .collect();
    urls.sort();
    urls.dedup();

    for url in urls {
        let probe = format!("{}/fee_stats", url.trim_end_matches('/'));
        match client.get(&probe).send().await {
            Ok(resp) if resp.status().is_success() => {
                debug!(horizon_url = %url, status = %resp.status(), "Horizon startup check OK");
            }
            Ok(resp) => {
                warn!(
                    horizon_url = %url,
                    status = %resp.status(),
                    "Horizon startup check failed: unexpected status"
                );
            }
            Err(e) => {
                warn!(
                    horizon_url = %url,
                    error = %e,
                    "Horizon startup check failed: request error"
                );
            }
        }
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Backwards-compatible wrapper: default (non-dry) run with no shutdown signal.
pub async fn run(cfg: AppConfig) -> Result<()> {
    run_with(cfg, false).await
}

/// Run the polling loop until cancelled.
pub async fn run_with(cfg: AppConfig, dry_run: bool) -> Result<()> {
    // Create a watch channel that never fires — the loop runs forever.
    let (_tx, rx) = watch::channel(false);
    run_with_shutdown(cfg, dry_run, rx).await
}

/// Run the polling loop, stopping cleanly when `shutdown_rx` receives `true`.
///
/// Each contract is polled sequentially per cycle. A slow or failing contract
/// logs an error but does not block the remaining contracts.
/// Logs a 60-second summary: contracts watched, transactions processed, alerts fired.
pub async fn run_with_shutdown(
    cfg: AppConfig,
    dry_run: bool,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let max_idle       = cfg.http_pool_max_idle_per_host.unwrap_or(10);
    let keepalive_secs = cfg.http_tcp_keepalive_secs.unwrap_or(30);

    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(max_idle)
        .tcp_keepalive(Some(Duration::from_secs(keepalive_secs)))
        .build()
        .context("failed to build HTTP client")?;

    let mut cursors = if let Some(path) = &cfg.cursor_file {
        CursorMap::load_from_file(std::path::Path::new(path), &cfg.contracts)
    } else {
        CursorMap::new_for_contracts(&cfg.contracts)
    };

    let interval      = Duration::from_secs(cfg.poll_interval_seconds);
    let summary_every = Duration::from_secs(60);
    let counters      = Arc::new(Counters::default());
    let n_contracts   = cfg.contracts.len();

    // ── #256: warn when approaching the contract limit ────────────────────────
    let warn_threshold = (MAX_CONTRACTS * 80) / 100; // 80 % of 100 = 80
    if n_contracts > warn_threshold {
        warn!(
            contracts    = n_contracts,
            limit        = MAX_CONTRACTS,
            "approaching contract limit: {}/{} contracts configured",
            n_contracts,
            MAX_CONTRACTS
        );
    }

    let contracts_list = cfg.contracts.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ");
    let mut networks: Vec<&str> = cfg.contracts.iter().map(|c| c.network.as_str()).collect();
    networks.sort();
    networks.dedup();
    let networks_str = networks.join(", ");

    let mut horizon_urls: Vec<(&str, &str)> = cfg
        .contracts
        .iter()
        .map(|c| (c.network.as_str(), c.network.horizon_base_url()))
        .collect();
    horizon_urls.sort();
    horizon_urls.dedup();
    let horizon_urls_str = horizon_urls
        .iter()
        .map(|(net, url)| format!("{}={}", net, url))
        .collect::<Vec<_>>()
        .join(", ");

    info!(
        version        = env!("CARGO_PKG_VERSION"),
        contracts      = n_contracts,
        contracts_list = %contracts_list,
        networks       = %networks_str,
        horizon_urls   = %horizon_urls_str,
        interval_secs  = cfg.poll_interval_seconds,
        "TxWatch polling engine started"
    );

    if cfg.poll_interval_seconds < 10 && cfg.contracts.len() > 5 {
        warn!(
            poll_interval_seconds = cfg.poll_interval_seconds,
            contracts = cfg.contracts.len(),
            "polling interval is very short with many contracts — Horizon rate limits may apply; \
             consider poll_interval_seconds >= 10"
        );
    }

    // ── #380: optional startup connectivity check (warn-only, never aborts) ───
    if cfg.horizon_startup_check {
        check_horizon_connectivity(&client, &cfg).await;
    }

    // Spawn a background summary logger that never blocks the poll loop.
    let counters_for_summary = Arc::clone(&counters);
    let _summary_guard = tokio::spawn(async move {
        loop {
            let c = Arc::clone(&counters_for_summary);
            let handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(summary_every).await;
                    let interval_txs    = c.interval_transactions.swap(0, Ordering::Relaxed);
                    let interval_alerts = c.interval_alerts.swap(0, Ordering::Relaxed);
                    info!(
                        contracts             = n_contracts,
                        transactions_total    = c.transactions.load(Ordering::Relaxed),
                        alerts_total          = c.alerts.load(Ordering::Relaxed),
                        transactions_interval = interval_txs,
                        alerts_interval       = interval_alerts,
                        "60-second summary"
                    );
                }
            });
            if let Err(e) = handle.await {
                error!(error = ?e, "summary logger panicked — restarting");
            }
        }
    });

    loop {
        // Check for shutdown signal before starting the next poll cycle.
        if *shutdown_rx.borrow() {
            break;
        }

        for contract in &cfg.contracts {
            match poll_contract(&client, contract, &mut cursors, dry_run).await {
                Ok((txs, alerts)) => {
                    counters.transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.alerts.fetch_add(alerts, Ordering::Relaxed);
                    counters.interval_transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.interval_alerts.fetch_add(alerts, Ordering::Relaxed);
                    #[cfg(feature = "metrics")]
                    {
                        crate::metrics::inc_transactions(txs);
                        crate::metrics::inc_alerts(alerts);
                    }
                }
                Err(e) => error!(error = %e, "contract polling task failed"),
            }
        }

        // Persist cursors after every full cycle (if a cursor_file is configured).
        if let Some(path) = &cfg.cursor_file {
            if let Err(e) = cursors.save_to_file(std::path::Path::new(path)) {
                warn!(error = %e, "failed to save cursor file");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    break;
                }
            }
        }
    }

    info!("TxWatch polling engine stopped cleanly");
    Ok(())
}

// ── Per-contract poll ─────────────────────────────────────────────────────────

/// Poll one contract and return `(transactions_processed, alerts_fired)`.
///
/// Uses `join=operations` on the transactions endpoint so that Horizon returns
/// operations inline, eliminating one HTTP request per transaction (#23).
/// Falls back to a separate `/transactions/{hash}/operations` fetch only when
/// the inline `operations` array is absent (older Horizon versions).
#[tracing::instrument(skip(client, contract, cursors), fields(
    contract    = %contract.label,
    contract_id = %contract.contract_id,
    network     = %contrac

/* … truncated 4442 chars — edit only what you need near the top … */
