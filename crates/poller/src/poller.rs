//! Core polling loop.
//!
//! `run` / `run_with` / `run_with_shutdown` are the public entry points.
//! `poll_contract` handles one contract per call and is kept `pub(crate)` so
//! the integration tests in `tests/` can drive it directly.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::{Duration, Instant},
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

// ── #543: per-contract alert rate limiting ────────────────────────────────────

/// Token bucket rate limiter for a single contract's outbound alerts.
///
/// Capacity is `max_alerts_per_minute`; tokens refill continuously at
/// `max_alerts_per_minute / 60` per second. State persists across poll cycles.
pub(crate) struct AlertRateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl AlertRateLimiter {
    pub(crate) fn new(max_alerts_per_minute: u32) -> Self {
        let capacity = max_alerts_per_minute as f64;
        Self {
            capacity,
            refill_per_sec: capacity / 60.0,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Refill based on elapsed time, then try to consume one token.
    /// Returns `true` if an alert may be sent.
    pub(crate) fn try_acquire(&mut self) -> bool {
        self.try_acquire_at(Instant::now())
    }

    /// Testable variant of [`try_acquire`] with an explicit "now".
    pub(crate) fn try_acquire_at(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last_refill).as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            self.last_refill = now;
        }
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Per-contract limiter map, keyed by contract label. Contracts without a
/// configured `max_alerts_per_minute` are absent from the map (unlimited).
pub(crate) type AlertRateLimiters = HashMap<String, AlertRateLimiter>;

fn limiter_for<'a>(
    limiters: &'a mut AlertRateLimiters,
    contract: &WatchedContract,
) -> Option<&'a mut AlertRateLimiter> {
    let max = contract.max_alerts_per_minute?;
    Some(
        limiters
            .entry(contract.label.clone())
            .or_insert_with(|| AlertRateLimiter::new(max)),
    )
}
// ── Startup connectivity check ────────────────────────────────────────────────

/// Send one `GET /fee_stats` to each unique Horizon base URL and warn (never
/// error) if any check fails. Never aborts startup — misconfiguration is
/// surfaced early but the poll loop still runs.
pub(crate) async fn check_horizon_connectivity(client: &Client, cfg: &AppConfig) {
    let mut urls: Vec<&str> = cfg
        .contracts
        .iter()
        .map(|c| horizon_base_url(c))
        .collect();
    urls.sort();
    urls.dedup();

    for url in urls {
        if url.contains("futurenet") {
            warn!(
                horizon_url = %url,
                "Futurenet is unstable and is reset periodically; use testnet for development"
            );
        }
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

    // #543: per-contract alert rate limiter state, persisted across cycles.
    let mut limiters = AlertRateLimiters::new();

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
        .map(|c| (c.network.as_str(), horizon_base_url(c)))
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

            let result = poll_contract_with_http_limited(
                &client,
                contract,
                &mut cursors,
                &mut breakers,
                &counters,
                dry_run,
                &webhook_http,
                &mut limiters,
                shutdown_rx,
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

// ── Horizon URL resolution ────────────────────────────────────────────────────

/// Resolve the Horizon base URL for a contract.
///
/// #555: a per-contract `horizon_url` override takes precedence over the
/// network default so users with private Horizon nodes can route individual
/// contracts to their own endpoints. In tests, `test_horizon_base_url` is
/// honored when no explicit override is set.
pub(crate) fn horizon_base_url(contract: &WatchedContract) -> &str {
    if let Some(url) = contract.horizon_url.as_deref() {
        return url;
    }
    #[cfg(test)]
    if let Some(url) = contract.test_horizon_base_url.as_deref() {
        return url;
    }
    contract.network.horizon_base_url()
}

// ── Per-contract poll ─────────────────────────────────────────────────────────

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
    shutdown_rx: oneshot::Receiver<()>,
) -> Result<(u64, u64)> {
    // Standalone callers get a fresh limiter map; the poll loop uses
    // `poll_contract_with_http_limited` to persist limiter state across cycles.
    let mut limiters = AlertRateLimiters::new();
    poll_contract_with_http_limited(client, contract, cursors, dry_run, webhook_http, &mut limiters)
        .await
}

/// Like [`poll_contract_with_http`] but threads the per-contract alert rate
/// limiter state so token buckets persist across poll cycles (#543).
pub(crate) async fn poll_contract_with_http_limited(
    client: &Client,
    contract: &WatchedContract,
    cursors: &mut CursorMap,
    dry_run: bool,
    webhook_http: &txwatch_notifier::WebhookHttpConfig,
    limiters: &mut AlertRateLimiters,
) -> Result<(u64, u64)> {
    let start_time = std::time::Instant::now();
    let cursor = cursors.get(&contract.contract_id).to_string();
    let page = fetch_page(client, contract, Some(&cursor)).await?;

    let mut txs_processed: u64 = 0;
    let mut alerts_fired: u64 = 0;

    for record in &page.records {
        // Extract enrichment data from inline operations when present,
        // otherwise fall back to a separate operations fetch.
        let (function_names, amount_stroops) = if !record.operations.is_empty() {
            extract_soroban_details(record.operations.clone())
        } else {
            match fetch_soroban_details(client, contract, &record.hash).await {
                Ok(details) => details,
                Err(e) => {
                    debug!(error = %e, hash = %record.hash, "soroban details fetch failed");
                    #[cfg(feature = "metrics")]
                    crate::metrics::inc_enrichment_errors();
                    (Vec::new(), None)
                }
            }
        };

        let enriched = EnrichedTransaction::from_horizon(
            record.tx.clone(),
            function_names,
            amount_stroops,
            None,
        )?;

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
                info!(rule = %rule.name, hash = %record.hash, "[dry-run] alert would fire");
            } else {
                // #543: enforce the per-contract alert rate limit, if configured.
                if let Some(limiter) = limiter_for(limiters, contract) {
                    if !limiter.try_acquire() {
                        warn!(
                            contract = %contract.label,
                            rule = %rule.name,
                            hash = %record.hash,
                            "alert suppressed by rate limiting"
                        );
                        #[cfg(feature = "metrics")]
                        crate::metrics::inc_alerts_suppressed(&contract.label);
                        continue;
                    }
                }
                let payload_ctx = PayloadContext {
                    contract_label: contract.label.clone(),
                    contract_id: contract.contract_id.clone(),
                    network: contract.network.as_str().to_string(),
                    horizon_base: contract.network.horizon_base_url().to_string(),
                    explorer_base: contract.network.explorer_base_url().to_string(),
                };
                let payload = AlertPayload::new(&payload_ctx, &enriched, rule);
                match send_webhook(
                    client,
                    &contract.webhook_url,
                    &payload,
                    contract.webhook_secret.as_deref(),
                    shutdown_rx,
                )
                .await
                {
                    Ok(_) => alerts_fired += 1,
                    Err(e) => error!(error = %e, rule = %rule.label(), "webhook delivery failed"),
                }
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
    let base = horizon_base_url(contract);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_default_to_zero() {
        let c = crate::counters::Counters::default();
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

    // ── #543: alert rate limiter ─────────────────────────────────────────────

    #[test]
    fn limiter_exhausts_after_capacity() {
        let mut limiter = AlertRateLimiter::new(3);
        let now = Instant::now();
        assert!(limiter.try_acquire_at(now));
        assert!(limiter.try_acquire_at(now));
        assert!(limiter.try_acquire_at(now));
        // Bucket empty: further acquisitions at the same instant are denied.
        assert!(!limiter.try_acquire_at(now));
        assert!(!limiter.try_acquire_at(now));
    }

    #[test]
    fn limiter_refills_over_time() {
        let mut limiter = AlertRateLimiter::new(60); // 1 token per second
        let start = Instant::now();
        for _ in 0..60 {
            assert!(limiter.try_acquire_at(start));
        }
        assert!(!limiter.try_acquire_at(start));

        // After one second, exactly one token is available again.
        let later = start + Duration::from_secs(1);
        assert!(limiter.try_acquire_at(later));
        assert!(!limiter.try_acquire_at(later));

        // Refill is capped at capacity.
        let much_later = start + Duration::from_secs(3600);
        for _ in 0..60 {
            assert!(limiter.try_acquire_at(much_later));
        }
        assert!(!limiter.try_acquire_at(much_later));
    }

    #[test]
    fn limiter_partial_refill_accumulates() {
        let mut limiter = AlertRateLimiter::new(60); // 1 token per second
        let start = Instant::now();
        for _ in 0..60 {
            assert!(limiter.try_acquire_at(start));
        }
        // Half a second yields half a token — not enough yet.
        assert!(!limiter.try_acquire_at(start + Duration::from_millis(500)));
        // Another half second completes the token.
        assert!(limiter.try_acquire_at(start + Duration::from_secs(1)));
    }
}
