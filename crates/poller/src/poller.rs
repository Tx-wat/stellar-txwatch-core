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
    network     = %contract.network.as_str(),
))]
pub(crate) async fn poll_contract(
    client: &Client,
    contract: &WatchedContract,
    cursors: &mut CursorMap,
    dry_run: bool,
) -> Result<(u64, u64)> {
    let cursor = cursors.get(&contract.contract_id).map(|s| s.to_string());
    let page = fetch_transactions_page(client, contract, cursor.as_deref()).await?;

    let mut txs_processed = 0u64;
    let mut alerts_fired = 0u64;

    for tx in &page.transactions {
        txs_processed += 1;

        let mut enriched = EnrichedTransaction::from(tx.clone());

        // If the inline operations array is missing, fetch operations separately.
        if enriched.operations.is_empty() {
            match fetch_operations(client, contract, &tx.hash).await {
                Ok(ops) => enriched.operations = ops,
                Err(e) => {
                    warn!(error = %e, hash = %tx.hash, "failed to fetch operations");
                }
            }
        }

        // Enrich with Soroban details when the transaction is a Soroban call.
        if let Some(details) = fetch_soroban_details(client, contract, &tx.hash).await {
            enriched.soroban = Some(details);
        }

        let matched = evaluate(&enriched, &contract.rules);
        if matched.is_empty() {
            debug!(hash = %tx.hash, "no rules matched");
            continue;
        }

        if dry_run {
            info!(hash = %tx.hash, rules = ?matched, "[dry-run] would fire alert");
            alerts_fired += 1;
            continue;
        }

        match send_webhook(&contract.webhook_url, &enriched, &matched).await {
            Ok(()) => {
                alerts_fired += 1;
                info!(hash = %tx.hash, rules = ?matched, "alert fired");
            }
            Err(e) => error!(error = %e, hash = %tx.hash, "failed to send webhook"),
        }
    }

    if let Some(last) = page.transactions.last() {
        cursors.set(&contract.contract_id, last.paging_token.clone());
    }

    Ok((txs_processed, alerts_fired))
}

// ── Horizon fetches ───────────────────────────────────────────────────────────

/// Fetch one page of transactions for `contract`, optionally starting after `cursor`.
async fn fetch_transactions_page(
    client: &Client,
    contract: &WatchedContract,
    cursor: Option<&str>,
) -> Result<HorizonPage> {
    let base = contract.network.horizon_base_url();
    let mut url = format!(
        "{}/accounts/{}/transactions?order=desc&limit=50&join=operations",
        base, contract.contract_id
    );
    if let Some(c) = cursor {
        url.push_str(&format!("&cursor={}", c));
    }

    let started = std::time::Instant::now();
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    let status = resp.status();
    let body = resp.text().await.context("reading transactions page body")?;
    #[cfg(feature = "metrics")]
    crate::metrics::observe_horizon_latency("transactions", started.elapsed().as_secs_f64());

    if !status.is_success() {
        anyhow::bail!("Horizon transactions request failed: {} — {}", status, body);
    }

    let page: HorizonPage = serde_json::from_str(&body)
        .with_context(|| format!("parsing transactions page from {}", url))?;
    Ok(page)
}

/// Fetch the operations for a single transaction hash.
async fn fetch_operations(
    client: &Client,
    contract: &WatchedContract,
    hash: &str,
) -> Result<Vec<serde_json::Value>> {
    let base = contract.network.horizon_base_url();
    let url = format!("{}/transactions/{}/operations", base, hash);

    let started = std::time::Instant::now();
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {}", url))?;
    let status = resp.status();
    let body = resp.text().await.context("reading operations body")?;
    #[cfg(feature = "metrics")]
    crate::metrics::observe_horizon_latency("operations", started.elapsed().as_secs_f64());

    if !status.is_success() {
        anyhow::bail!("Horizon operations request failed: {} — {}", status, body);
    }

    let parsed: serde_json::Value = serde_json::from_str(&body)
        .with_context(|| format!("parsing operations from {}", url))?;
    let ops = parsed
        .get("_embedded")
        .and_then(|e| e.get("records"))
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(ops)
}
