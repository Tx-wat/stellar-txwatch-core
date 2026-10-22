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
use tokio::sync::{oneshot, watch};
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

    // #259: set the gauge once at startup so dashboards always have a
    // baseline signal for how many contracts are being watched.
    #[cfg(feature = "metrics")]
    crate::metrics::set_contracts_active(n_contracts);

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
    network     = %contract.network.as_str()
))]
pub(crate) async fn poll_contract(
    client:   &Client,
    contract: &WatchedContract,
    cursors:  &mut CursorMap,
    dry_run:  bool,
) -> Result<(u64, u64)> {
    let cursor = cursors.get(&contract.contract_id).to_string();

    // `poll_base` is used for all Horizon HTTP requests (may be overridden in tests).
    // `canonical_base` is always the production Horizon URL and is used only for
    // building horizon_link in payloads, so links always point to the real network.
    let poll_base = contract
        .horizon_base_url_override
        .as_deref()
        .unwrap_or_else(|| contract.network.horizon_base_url());
    let canonical_base = contract.network.horizon_base_url();

    // Collect all pages of transactions.
    let mut all_records = Vec::new();
    let mut page_cursor = cursor.clone();

    loop {
        let url = format!(
            "{}/accounts/{}/transactions?cursor={}&order=asc&limit=200&join=operations",
            poll_base, contract.contract_id, page_cursor
        );

        let response = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?;

        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("Retry-After")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(5);
            warn!(contract = %contract.label, retry_after, "Horizon returned 429 — backing off");
            tokio::time::sleep(Duration::from_secs(retry_after)).await;
            return Ok((0, 0));
        }

        let page: HorizonPage = response
            .json()
            .await
            .with_context(|| format!("failed to parse Horizon response from {}", url))?;

        let records = page._embedded.records;
        if records.is_empty() {
            break;
        }

        let last_token = records.last().map(|r| r.tx.paging_token.clone());
        let count = records.len();
        for r in records {
            all_records.push(r);
        }

        if count < 200 {
            break;
        }
        if let Some(token) = last_token {
            page_cursor = token;
        } else {
            break;
        }
    }

    if !all_records.is_empty() {
        info!(contract = %contract.label, count = all_records.len(), "fetched new transactions");
    } else {
        debug!(contract = %contract.label, cursor = %cursor, "no new transactions");
    }

    let mut tx_count    = 0u64;
    let mut alert_count = 0u64;

    for record in all_records {
        let paging_token = record.tx.paging_token.clone();
        let tx_hash      = record.tx.hash.clone();

        // Advance cursor before enrichment so the tx is not re-processed even if
        // op enrichment fails.
        cursors.set(&contract.contract_id, &paging_token);

        // If Horizon returned inline operations, use them directly.
        // Otherwise fall back to a separate /operations fetch.
        let (function_names, amount_stroops) = if !record.operations.is_empty() {
            debug!(contract = %contract.label, tx = %tx_hash, "using inline operations (join=operations)");
            extract_soroban_details(record.operations)
        } else {
            match fetch_soroban_details(client, poll_base, &tx_hash).await {
                Ok(details) => details,
                Err(e) => {
                    warn!(
                        contract = %contract.label, tx = %tx_hash, error = %e,
                        "could not fetch operation details — evaluating rules without them"
                    );
                    (Vec::new(), None)
                }
            }
        };

        let enriched = match EnrichedTransaction::from_horizon(record.tx, function_names, amount_stroops, None) {
            Ok(t) => t,
            Err(e) => {
                warn!(contract = %contract.label, tx = %tx_hash, error = %e,
                    "skipping transaction due to enrichment error");
                continue;
            }
        };

        tx_count += 1;

        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            contract.network.as_str(),
            canonical_base,
            contract.network.explorer_base_url(),
            &contract.rules,
            &enriched,
        );

        if payloads.is_empty() {
            debug!(contract = %contract.label, tx = %tx_hash, rules = ?contract.rules,
                "transaction evaluated but no rules matched");
        }

        for payload in payloads {
            alert_count += 1;
            info!(contract = %contract.label, rule = %payload.rule_triggered,
                tx = %payload.transaction_hash, "rule matched");

            if dry_run {
                info!(contract = %contract.label, rule = %payload.rule_triggered,
                    tx = %payload.transaction_hash, "dry-run enabled: not sending webhook");
            } else {
                info!(contract = %contract.label, rule = %payload.rule_triggered,
                    tx = %payload.transaction_hash, "rule fired — sending webhook");
                // #260: create a per-delivery oneshot channel. The sender is dropped
                // immediately, which means the receiver observes a permanently-open
                // channel — the delivery runs to completion. If the caller needs to
                // abort in-flight retries, they can pass a live sender and drop it.
                let (_shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
                std::mem::forget(_shutdown_tx);
                if let Err(e) = send_webhook(
                    client,
                    &contract.webhook_url,
                    &payload,
                    contract.webhook_secret.as_deref(),
                    shutdown_rx,
                ).await {
                    error!(contract = %contract.label, rule = %payload.rule_triggered,
                        tx = %payload.transaction_hash, error = %e, "webhook delivery failed");
                    #[cfg(feature = "metrics")]
                    crate::metrics::inc_webhook_failures();
                }
            }
        }
    }

    if tx_count > 0 {
        info!(contract = %contract.label, transactions = tx_count, alerts = alert_count,
            "poll cycle complete");
    }

    Ok((tx_count, alert_count))
}

// ── Startup log field helpers (for testing) ──────────────────────────────────

#[cfg(test)]
pub(crate) fn startup_log_fields(cfg: &AppConfig) -> (String, String, String, String) {
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
    (env!("CARGO_PKG_VERSION").to_string(), contracts_list, networks_str, horizon_urls_str)
}
