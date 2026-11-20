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
use txwatch_notifier::{send_webhook, AlertPayload, PayloadContext};
use txwatch_rules::{evaluate, extract_soroban_details, EnrichedTransaction};

use crate::circuit_breaker::CircuitBreakerMap;
use crate::counters::Counters;
use crate::cursor::CursorMap;
use crate::enrichment::fetch_soroban_details;
use crate::horizon::HorizonPage;

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
    let user_agent     = cfg
        .http_user_agent
        .as_deref()
        .unwrap_or(concat!("txwatch/", env!("CARGO_PKG_VERSION")));

    let webhook_timeout_secs = cfg.webhook_timeout_seconds.unwrap_or(15);

    let client = Client::builder()
        .user_agent(user_agent)
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(max_idle)
        .tcp_keepalive(Some(Duration::from_secs(keepalive_secs)))
        .build()
        .context("failed to build HTTP client")?;

    // #461/#462: shared HTTP settings threaded into the notifier's webhook
    // client builders so TLS / pinned-cert clients behave like the main client.
    let webhook_http = txwatch_notifier::WebhookHttpConfig {
        user_agent: user_agent.to_string(),
        pool_max_idle_per_host: max_idle,
        tcp_keepalive_secs: keepalive_secs,
        timeout_secs: webhook_timeout_secs,
    };

    let mut cursors = if let Some(path) = &cfg.cursor_file {
        CursorMap::load_from_file(std::path::Path::new(path), &cfg.contracts)
    } else {
        CursorMap::new_for_contracts(&cfg.contracts)
    };

    // #374: per-contract circuit breaker state, keyed by contract label.
    let mut breakers = CircuitBreakerMap::new();

    let interval      = Duration::from_secs(cfg.poll_interval_seconds);
    let summary_every = Duration::from_secs(60);
    let counters      = Arc::new(Counters::default());
    let n_contracts   = cfg.contracts.len();
    let contract_labels: Vec<String> =
        cfg.contracts.iter().map(|c| c.label.clone()).collect();

    // #259: set the gauge once at startup so dashboards always have a
    // baseline signal for how many contracts are being watched.
    #[cfg(feature = "metrics")]
    crate::metrics::set_contracts_active(n_contracts);

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

    // ── #380/#451: optional startup connectivity check (warn-only, never
    // aborts). Delegates to the single implementation in `horizon.rs`.
    let horizon_urls: Vec<String> = cfg
        .contracts
        .iter()
        .map(|c| c.network.horizon_base_url().to_string())
        .collect();
    crate::horizon::startup_connectivity_check(&client, &horizon_urls, cfg.horizon_startup_check)
        .await;

    // Spawn a background summary logger that never blocks the poll loop.
    let counters_for_summary = Arc::clone(&counters);
    let labels_for_summary = contract_labels.clone();
    let _summary_guard = tokio::spawn(async move {
        loop {
            let c = Arc::clone(&counters_for_summary);
            let labels = labels_for_summary.clone();
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
            let _ = handle.await;
            let _ = labels;
        }
    });

    // #583: record the last fired alert (timestamp + rule) into the cursor
    // entry so operators can replay/debug from the persisted cursor file.
    // The fields are optional and default to `None`, keeping older cursor
    // files (which lack them) fully backward compatible.
    let record_alert = |cursors: &mut CursorMap, label: &str, rule: &str| {
        if let Some(entry) = cursors.get_mut(label) {
            entry.last_alert_at = Some(chrono::Utc::now().to_rfc3339());
            entry.last_alert_rule = Some(rule.to_string());
        }
    };
    let _ = &record_alert;

    loop {
        if *shutdown_rx.borrow() {
            info!("shutdown signal received, stopping poll loop");
            break;
        }

        for contract in &cfg.contracts {
            if *shutdown_rx.borrow() {
                break;
            }
            if let Err(e) = poll_contract(
                &client,
                contract,
                &mut cursors,
                &mut breakers,
                &counters,
                dry_run,
                &webhook_http,
            )
            .await
            {
                error!(contract = %contract.label, error = %e, "poll cycle failed");
            }
        }

        // Persist cursors after each cycle so a restart resumes from the
        // last processed paging token (and, per #583, the last alert).
        if let Some(path) = &cfg.cursor_file {
            if let Err(e) = cursors.save_to_file(std::path::Path::new(path)) {
                warn!(error = %e, "failed to persist cursor file");
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    info!("shutdown signal received, stopping poll loop");
                    break;
                }
            }
        }
    }

    Ok(())
}

// ── Per-contract polling ──────────────────────────────────────────────────────

/// Poll a single contract once: fetch a page of transactions, evaluate rules,
/// and dispatch alerts. Kept `pub(crate)` for integration tests.
pub(crate) async fn poll_contract(
    client: &Client,
    contract: &WatchedContract,
    cursors: &mut CursorMap,
    breakers: &mut CircuitBreakerMap,
    counters: &Arc<Counters>,
    dry_run: bool,
    webhook_http: &txwatch_notifier::WebhookHttpConfig,
) -> Result<()> {
    let _ = MAX_CONTRACTS;
    let _ = (oneshot::channel::<()>(), AtomicU64::new(0));

    let cursor = cursors.get(&contract.label).cloned().unwrap_or_default();
    let page = fetch_page(client, contract, cursor.paging_token.as_deref()).await?;

    for tx in &page.records {
        counters.transactions.fetch_add(1, Ordering::Relaxed);
        let enriched = EnrichedTransaction::from(tx.clone());
        let _ = extract_soroban_details(&enriched);

        let alerts = evaluate(&enriched, &contract.rules);
        for alert in alerts {
            counters.alerts.fetch_add(1, Ordering::Relaxed);
            if dry_run {
                info!(contract = %contract.label, rule = %alert.rule, "dry-run: alert suppressed");
                continue;
            }
            let payload = AlertPayload::new(alert.clone(), PayloadContext::default());
            if let Err(e) = send_webhook(client, &contract.webhook_url, &payload, webhook_http).await {
                warn!(contract = %contract.label, error = %e, "webhook delivery failed");
            }
            // #583: persist last alert metadata for replay/debugging.
            if let Some(entry) = cursors.get_mut(&contract.label) {
                entry.last_alert_at = Some(chrono::Utc::now().to_rfc3339());
                entry.last_alert_rule = Some(alert.rule.clone());
            }
        }
    }

    if let Some(entry) = cursors.get_mut(&contract.label) {
        entry.paging_token = page.next_cursor.clone();
    }

    Ok(())
}

async fn fetch_page(
    client: &Client,
    contract: &WatchedContract,
    cursor: Option<&str>,
) -> Result<HorizonPage> {
    let base = contract.network.horizon_base_url();
    let mut url = format!(
        "{}/accounts/{}/transactions?limit=200",
        base.trim_end_matches('/'),
        contract.contract_id
    );
    if let Some(c) = cursor {
        url.push_str(&format!("&cursor={}", c));
    }
    let resp = client.get(&url).send().await.context("horizon request failed")?;
    let page: HorizonPage = resp.json().await.context("failed to decode horizon page")?;
    Ok(page)
}
