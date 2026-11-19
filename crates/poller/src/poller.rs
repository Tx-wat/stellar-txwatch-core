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

use crate::circuit_breaker::CircuitBreakerMap;
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
======

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

    // #460: track consecutive cursor-save failures so persistent persistence
    // breakage is escalated to error level rather than a single warn line.
    let mut cursor_save_failures: u32 = 0;
    const CURSOR_SAVE_FAILURE_THRESHOLD: u32 = 3;

    loop {
        // Check for shutdown signal before starting the next poll cycle.
        if *shutdown_rx.borrow() {
            break;
        }

        for contract in &cfg.contracts {
            // #374: skip polling while the circuit breaker is open for this
            // contract (backing off exponentially after consecutive failures).
            if breakers.should_skip(&contract.label) {
                debug!(
                    contract = %contract.label,
                    "circuit breaker open — skipping poll for this contract"
                );
                continue;
            }

            let result =
                poll_contract_with_http(&client, contract, &mut cursors, dry_run, &webhook_http)
                    .await;
            // #352: count every poll attempt (success or failure) per contract.
            #[cfg(feature = "metrics")]
            crate::metrics::inc_poll_cycles(&contract.label);
            match result {
                Ok((txs, alerts)) => {
                    // #374: reset the breaker and log recovery on success.
                    if breakers.record_success(&contract.label) {
                        info!(
                            contract = %contract.label,
                            "circuit breaker closed — contract polling recovered"
                        );
                    }
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
                Err(e) => {
                    // #374: count the failure and log when the breaker opens.
                    if breakers.record_failure(&contract.label) {
                        warn!(
                            contract = %contract.label,
                            error = %e,
                            "circuit breaker open — skipping polling for this contract"
                        );
                        #[cfg(feature = "metrics")]
                        crate::metrics::inc_circuit_breaker_open();
                    }
                    error!(error = %e, "contract polling task failed");
                }
            }
        }

        // Persist cursors after every full cycle (if a cursor_file is configured).
        if let Some(path) = &cfg.cursor_file {
            match cursors.save_to_file(std::path::Path::new(path)) {
                Ok(()) => {
                    if cursor_save_failures > 0 {
                        info!(
                            consecutive_failures = cursor_save_failures,
                            "cursor file persistence recovered"
                        );
                    }
                    cursor_save_failures = 0;
                }
                Err(e) => {
                    cursor_save_failures = cursor_save_failures.saturating_add(1);
                    #[cfg(feature = "metrics")]
                    crate::metrics::inc_cursor_save_failures();
                    if cursor_save_failures >= CURSOR_SAVE_FAILURE_THRESHOLD {
                        error!(
                            error = %e,
                            consecutive_failures = cursor_save_failures,
                            cursor_file = %path,
                            "failed to save cursor file repeatedly — cursor persistence is broken"
                        );
                    } else {
                        warn!(
                            error = %e,
                            consecutive_failures = cursor_save_failures,
                            "failed to save cursor file"
                        );
                    }
                }
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
    tx_count    = tracing::field::Empty,
    alert_count = tracing::field::Empty,
    duration_ms = tracing::field::Empty,
    outcome     = tracing::field::Empty,
))]
pub(crate) async fn poll_contract(
    client: &Client,
    contract: &WatchedContract,
    cursors: &mut CursorMap,
    dry_run: bool,
) -> Result<(u64, u64)> {
    poll_contract_with_http(client, contract, cursors, dry_run, &txwatch_notifier::WebhookHttpConfig::default()).await
}

/// Like [`poll_contract`] but threads the shared webhook HTTP settings into
/// webhook delivery so TLS / pinned-cert clients honor the configured timeout,
/// user agent, pool and keepalive settings (#461/#462).
pub(crate) async fn poll_contract_with_http(
    client: &Client,
    contract: &WatchedContract,
    cursors: &mut CursorMap,
    dry_run: bool,
    webhook_http: &txwatch_notifier::WebhookHttpConfig,
) -> Result<(u64, u64)> {
    let start_time = std::time::Instant::now();
    let cursor = cursors.get(&contract.label).map(|s| s.to_string());
    let page = fetch_page(client, contract, cursor.as_deref()).await?;

    let mut txs_processed: u64 = 0;
    let mut alerts_fired: u64 = 0;

    for record in &page.records {
        let mut enriched = EnrichedTransaction::from_horizon(record);

        // Fall back to a separate operations fetch when inline ops are missing.
        if enriched.operations.is_empty() {
            match fetch_soroban_details(client, contract, &record.hash).await {
                Ok(details) => enriched.soroban = details,
                Err(e) => {
                    debug!(error = %e, hash = %record.hash, "soroban details fetch failed");
                    #[cfg(feature = "metrics")]
                    crate::metrics::inc_enrichment_errors();
                }
            }
        } else {
            enriched.soroban = extract_soroban_details(&enriched.operations);
        }

        txs_processed += 1;

        let matched = evaluate(&contract.rules, &enriched);
        for rule in matched {
            if dry_run {
                info!(rule = %rule.name, hash = %record.hash, "[dry-run] alert would fire");
            } else {
                match send_webhook(&contract.webhook_url, &rule, &enriched, &webhook_http).await {
                    Ok(()) => alerts_fired += 1,
                    Err(e) => error!(error = %e, rule = %rule.name, "webhook delivery failed"),
                }
            }
        }
    }

    if let Some(last) = page.records.last() {
        cursors.set(&contract.label, last.paging_token.clone());
    }

    let duration_ms = start_time.elapsed().as_millis() as u64;
    let span = tracing::Span::current();
    span.record("tx_count", txs_processed);
    span.record("alert_count", alerts_fired);
    span.record("duration_ms", duration_ms);
    span.record("outcome", "ok");

    Ok((txs_processed, alerts_fired))
}

async fn fetch_page(
    client: &Client,
    contract: &WatchedContract,
    cursor: Option<&str>,
) -> Result<HorizonPage> {
    let base = contract.network.horizon_base_url();
    let mut url = format!(
        "{}/accounts/{}/transactions?order=asc&limit=200&join=operations",
        base, contract.contract_id
    );
    if let Some(c) = cursor {
        url.push_str(&format!("&cursor={}", c));
    }

    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("request to {} failed", url))?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("Horizon returned {} for {}", status, url);
    }

    let page: HorizonPage = resp
        .json()
        .await
        .with_context(|| format!("failed to parse Horizon response from {}", url))?;

    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_default_to_zero() {
        let c = Counters::default();
        assert_eq!(c.transactions.load(Ordering::Relaxed), 0);
        assert_eq!(c.alerts.load(Ordering::Relaxed), 0);
    }

    #[cfg(feature = "metrics")]
    #[test]
    fn poll_cycles_counter_increments_per_contract() {
        let before_a = crate::metrics::poll_cycles_total("test-contract-a");
        let before_b = crate::metrics::poll_cycles_total("test-contract-b");

        crate::metrics::inc_poll_cycles("test-contract-a");
        crate::metrics::inc_poll_cycles("test-contract-a");
        crate::metrics::inc_poll_cycles("test-contract-b");

        assert_eq!(crate::metrics::poll_cycles_total("test-contract-a"), before_a + 2);
        assert_eq!(crate::metrics::poll_cycles_total("test-contract-b"), before_b + 1);
    }

    // ── #374: circuit breaker integration ────────────────────────────────────

    #[test]
    fn breaker_skips_after_three_consecutive_failures() {
        let mut breakers = CircuitBreakerMap::new();
        let label = "breaker-test-contract";

        // Two failures: still polled.
        assert!(!breakers.record_failure(label));
        assert!(!breakers.record_failure(label));
        assert!(!breakers.should_skip(label));

        // Third failure opens the breaker.
        assert!(breakers.record_failure(label));
        assert!(breakers.should_skip(label));
    }

    #[test]
    fn breaker_recovers_on_success() {
        let mut breakers = CircuitBreakerMap::new();
        let label = "breaker-recovery-contract";

        for _ in 0..3 {
            breakers.record_failure(label);
        }
        assert!(breakers.should_skip(label));

        // A success closes the breaker and reports the transition.
        assert!(breakers.record_success(label));
        assert!(!breakers.should_skip(label));
        // Subsequent successes do not report a transition.
        assert!(!breakers.record_success(label));
    }

    #[test]
    fn breaker_backoff_grows_exponentially() {
        let mut breakers = CircuitBreakerMap::new();
        let label = "breaker-backoff-contract";

        for _ in 0..3 {
            breakers.record_failure(label);
        }
        // 2^3 = 8 intervals skipped after the third failure.
        assert_eq!(breakers.skip_intervals(label), 8);
    }
}
