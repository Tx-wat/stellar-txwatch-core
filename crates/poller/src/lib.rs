//! txwatch-poller runs the Horizon polling loop, enriches transactions, evaluates rules,
//! and sends webhook alerts through `txwatch-notifier`.

use std::{
    collections::HashMap,
    sync::atomic::Ordering,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json;
use std::fs;
use tokio::sync::{oneshot, watch};
use tracing::{debug, error, info, warn};
use txwatch_config::{AppConfig, WatchedContract};
use txwatch_notifier::{build_tls_client, send_webhook};
use txwatch_rules::{evaluate, EnrichedTransaction, HorizonTransaction};

// ── Optional Prometheus metrics ───────────────────────────────────────────────

#[cfg(feature = "metrics")]
pub mod metrics;

#[cfg(feature = "metrics")]
pub use metrics::serve_metrics;

// ── Horizon response shapes ───────────────────────────────────────────────────

/// Horizon operation record — we only need the fields relevant to Soroban.
#[derive(Deserialize)]
struct HorizonOperation {
    #[serde(rename = "type")]
    op_type: String,
    /// Present on `invoke_host_function` operations.
    function: Option<String>,
    /// Present on `payment` operations (string, e.g. "1000.0000000").
    amount: Option<String>,
}

/// A Horizon transaction record that may include inline operations via `join=operations`.
/// The transaction fields come from `txwatch_rules::HorizonTransaction`; only `operations` is added here.
#[derive(Deserialize)]
struct HorizonTransactionWithOps {
    #[serde(flatten)]
    tx: HorizonTransaction,
    /// Inline operations embedded when `join=operations` is used.
    #[serde(default)]
    operations: Vec<HorizonOperation>,
}

/// Generic Horizon page wrapper used by `paginate()`.
#[derive(Deserialize)]
struct HorizonPage<T> {
    _embedded: Embedded<T>,
}

#[derive(Deserialize)]
struct Embedded<T> {
    records: Vec<T>,
}

#[derive(Deserialize)]
struct OperationsPage {
    _embedded: OpsEmbedded,
}

#[derive(Deserialize)]
struct OpsEmbedded {
    records: Vec<HorizonOperation>,
}

// ── Summary counters ──────────────────────────────────────────────────────────

mod counters;
pub(crate) use counters::Counters;

// ── Pagination helper ─────────────────────────────────────────────────────────

/// Fetches all pages from a paginated Horizon endpoint and collects every record.
///
/// - `first_url`: the initial URL (already includes cursor, order, limit, etc.)
/// - `paging_token`: closure that extracts the paging token from a single record;
///   used to build the cursor for the next page request.
///
/// Stops when a page returns fewer than `page_limit` records (last page), or when
/// the page is empty. The caller is responsible for rate-limit handling before
/// invoking this function.
async fn paginate<T>(
    client: &Client,
    first_url: &str,
    page_limit: usize,
    paging_token: impl Fn(&T) -> &str,
) -> Result<Vec<T>>
where
    T: DeserializeOwned,
{
    let mut all_records: Vec<T> = Vec::new();
    let mut url = first_url.to_string();

    loop {
        let page: HorizonPage<T> = client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?
            .json()
            .await
            .with_context(|| format!("failed to parse Horizon response from {}", url))?;

        let records = page._embedded.records;
        if records.is_empty() {
            break;
        }

        let last_token = records.last().map(|r| paging_token(r).to_string());
        let count = records.len();
        all_records.extend(records);

        if count < page_limit {
            break;
        }

        match last_token {
            Some(token) => {
                // Replace the cursor value in the URL for the next page.
                // The URL contains `cursor=<value>` — update it to the new token.
                url = replace_cursor_param(&url, &token);
            }
            None => break,
        }
    }

    Ok(all_records)
}

/// Replace the `cursor=<old>` query parameter in a URL with a new cursor value.
fn replace_cursor_param(url: &str, new_cursor: &str) -> String {
    // Split on `cursor=` and replace through to the next `&` or end-of-string.
    if let Some(start) = url.find("cursor=") {
        let before = &url[..start];
        let after_cursor = &url[start + "cursor=".len()..];
        let end = after_cursor.find('&').map(|i| i + 1).unwrap_or(after_cursor.len());
        let rest = &after_cursor[end..];
        // Preserve the trailing portion (parameters after cursor).
        if rest.is_empty() {
            format!("{}cursor={}", before, new_cursor)
        } else {
            format!("{}cursor={}&{}", before, new_cursor, rest)
        }
    } else {
        // No cursor param found — append it.
        if url.contains('?') {
            format!("{}&cursor={}", url, new_cursor)
        } else {
            format!("{}?cursor={}", url, new_cursor)
        }
    }
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Backwards-compatible wrapper: default (non-dry) run.
pub async fn run(cfg: AppConfig) -> Result<()> {
    run_with(cfg, false).await
}

/// Run the polling loop forever. Each contract is polled concurrently via a
/// tokio JoinSet, capped at `MAX_CONCURRENT_POLLS` in flight by a semaphore;
/// one slow or failing contract never blocks the others.
/// Logs a summary every 60 seconds: contracts watched, transactions processed,
/// alerts fired.
pub async fn run_with(cfg: AppConfig, dry_run: bool) -> Result<()> {
    run_with_shutdown(cfg, dry_run, watch::channel(false).1).await
}

/// Run the polling loop with graceful shutdown support.
///
/// The `from_beginning` flag is controlled via the `dry_run` parameter and the
/// `shutdown_rx` channel. When `from_beginning` is `true`, all contracts start
/// with cursor `"0"` (the Horizon keyword for the earliest available transaction)
/// instead of `"now"`. Any persisted cursor_file values are ignored for this run
/// (the file is not modified either).
pub async fn run_with_shutdown(
    cfg: AppConfig,
    dry_run: bool,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    run_internal(cfg, dry_run, false, shutdown_rx).await
}

/// Full internal entry point with all options.
pub async fn run_with_options(
    cfg: AppConfig,
    dry_run: bool,
    from_beginning: bool,
    shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    run_internal(cfg, dry_run, from_beginning, shutdown_rx).await
}

async fn run_internal(
    cfg: AppConfig,
    dry_run: bool,
    from_beginning: bool,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<()> {
    let max_idle       = cfg.http_pool_max_idle_per_host.unwrap_or(10);
    let keepalive_secs = cfg.http_tcp_keepalive_secs.unwrap_or(30);

/// Run the polling loop until `shutdown` is set to `true`. A shutdown request
/// is observed between poll cycles, so the in-flight cycle always completes.
pub async fn run_with_shutdown(
    cfg:        AppConfig,
    dry_run:    bool,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let client = build_http_client(&cfg)?;

    let mut cursors: HashMap<String, String> = if from_beginning {
        // Issue #262: --from-beginning overrides any persisted cursor values.
        // All contracts start from cursor "0" (earliest available transaction).
        warn!("--from-beginning: replaying all historical transactions — this may generate many alerts");
        cfg.contracts
            .iter()
            .map(|c| (c.contract_id.clone(), "0".to_string()))
            .collect()
    } else if let Some(path) = &cfg.cursor_file {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, String>>(&raw) {
                Ok(mut map) => {
                    // Ensure every configured contract has a cursor entry.
                    for c in &cfg.contracts {
                        map.entry(c.contract_id.clone())
                            .or_insert_with(|| "now".to_string());
                    }
                    map
                }
                Err(e) => {
                    warn!(error = ?e, "failed to parse cursor_file; starting from 'now' for all contracts");
                    cfg.contracts
                        .iter()
                        .map(|c| (c.contract_id.clone(), "now".to_string()))
                        .collect()
                }
            },
            Err(e) => {
                debug!(error = ?e, "could not read cursor_file; starting from 'now'");
                cfg.contracts
                    .iter()
                    .map(|c| (c.contract_id.clone(), "now".to_string()))
                    .collect()
            }
        }
    } else {
        cfg.contracts
            .iter()
            .map(|c| (c.contract_id.clone(), "now".to_string()))
            .collect()
    };

    let interval      = Duration::from_secs(cfg.poll_interval_seconds);
    let summary_every = Duration::from_secs(60);
    let counters      = Arc::new(Counters::default());
    let n_contracts   = cfg.contracts.len();

    let contracts_list = cfg.contracts.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ");
    let mut networks: Vec<&str> = cfg.contracts.iter().map(|c| c.network.as_str()).collect();
    networks.sort(); networks.dedup();
    let networks_str = networks.join(", ");

    let mut horizon_urls: Vec<(&str, &str)> = cfg
        .contracts.iter().map(|c| (c.network.as_str(), c.network.horizon_base_url())).collect();
    horizon_urls.sort(); horizon_urls.dedup();
    let horizon_urls_str = horizon_urls.iter()
        .map(|(net, url)| format!("{}={}", net, url)).collect::<Vec<_>>().join(", ");

    info!(
        version        = env!("CARGO_PKG_VERSION"),
        contracts      = n_contracts,
        contracts_list = %contracts_list,
        networks       = %networks_str,
        horizon_urls   = %horizon_urls_str,
        interval_secs  = cfg.poll_interval_seconds,
        "TxWatch polling engine started"
    );

    if let Some(requests_per_minute) = rate_limit_warning(cfg.poll_interval_seconds, cfg.contracts.len()) {
        warn!(
            poll_interval_seconds = cfg.poll_interval_seconds,
            contracts = cfg.contracts.len(),
            estimated_requests_per_minute = requests_per_minute,
            "polling interval is below {}s: estimated {} requests/minute ({} contract(s) x 60 / {}s). \
             Public Horizon enforces a per-IP request quota and answers with 429 Too Many Requests \
             when it is exceeded; consider poll_interval_seconds >= {}",
            MIN_RECOMMENDED_POLL_INTERVAL_SECS,
            requests_per_minute,
            cfg.contracts.len(),
            cfg.poll_interval_seconds,
            MIN_RECOMMENDED_POLL_INTERVAL_SECS,
        );
    }

    let counters_for_summary = Arc::clone(&counters);
    let labels_for_summary: Vec<String> = cfg.contracts.iter().map(|c| c.label.clone()).collect();
    let _summary_guard = tokio::spawn(async move {
        loop {
            let c = Arc::clone(&counters_for_summary);
            let labels = labels_for_summary.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(summary_every).await;
                    let interval_txs    = c.interval_transactions.swap(0, Ordering::Relaxed);
                    let interval_alerts = c.interval_alerts.swap(0, Ordering::Relaxed);
                    let interval_suppressed = c.interval_suppressed.swap(0, Ordering::Relaxed);
                    info!(
                        contracts             = n_contracts,
                        transactions_total    = c.transactions.load(Ordering::Relaxed),
                        alerts_total          = c.alerts.load(Ordering::Relaxed),
                        transactions_interval = interval_txs,
                        alerts_interval       = interval_alerts,
                        alerts_suppressed_interval = interval_suppressed,
                        "60-second summary"
                    );
                    // One structured line per contract, so operators can tell
                    // which contract is generating traffic.
                    for (contract, txs, alerts) in c.take_interval_per_contract(&labels) {
                        info!(
                            contract              = %contract,
                            transactions_interval = txs,
                            alerts_interval       = alerts,
                            "60-second summary (per contract)"
                        );
                    }
                }
            });
            if let Err(e) = handle.await {
                error!(error = ?e, "summary logger panicked — restarting");
            }
        }
    });

    // Contracts with `[contracts.tls]` deliver webhooks through a dedicated client,
    // so their client certificate is never presented to Horizon.
    let mut webhook_clients: HashMap<String, Client> = HashMap::new();
    for contract in &cfg.contracts {
        if let Some(tls) = &contract.tls {
            webhook_clients.insert(contract.label.clone(), build_tls_client(tls)?);
        }
    }

    loop {
        poll_cycle(&client, &cfg.contracts, &mut cursors, dry_run, &counters, MAX_CONCURRENT_POLLS).await;
        tokio::time::sleep(interval).await;
    }

    info!("TxWatch polling engine stopped cleanly");
    Ok(())
}

/// Maximum number of contracts polled at the same time in one cycle. Caps open
/// connections and file descriptors when many contracts are configured.
const MAX_CONCURRENT_POLLS: usize = 20;

/// Poll every contract once, concurrently via a `JoinSet`, with at most
/// `max_concurrent` polls in flight (enforced by a `Semaphore`). One slow or
/// failing contract never blocks the others. Cursors are updated after all
/// polls in the cycle have finished.
async fn poll_cycle(
    client:         &Client,
    contracts:      &[WatchedContract],
    cursors:        &mut HashMap<String, String>,
    dry_run:        bool,
    counters:       &Arc<Counters>,
    max_concurrent: usize,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrent.max(1)));
    let mut set = tokio::task::JoinSet::new();

    for contract in contracts {
        let client    = client.clone();
        let contract  = contract.clone();
        let counters  = Arc::clone(counters);
        let semaphore = Arc::clone(&semaphore);
        let cursor    = cursors.get(&contract.contract_id).cloned();
        set.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok();
            let mut local: HashMap<String, String> = HashMap::new();
            if let Some(c) = cursor {
                local.insert(contract.contract_id.clone(), c);
            }
            match poll_contract(&client, &contract, &mut local, dry_run).await {
                Ok((txs, alerts)) => {
                    counters.transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.alerts.fetch_add(alerts, Ordering::Relaxed);
                    counters.interval_transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.interval_alerts.fetch_add(alerts, Ordering::Relaxed);
                    counters.record_contract(&contract.label, txs, alerts);
                    // Issue #25: increment Prometheus counters when metrics feature is enabled.
                    #[cfg(feature = "metrics")]
                    {
                        metrics::inc_transactions(txs);
                        metrics::inc_alerts(alerts);
                    }
                }
                Err(e) => error!(error = %e, "contract polling task failed"),
            }
            local
        });
    }

    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(local) => cursors.extend(local),
            Err(e) => error!(error = ?e, "contract polling task panicked"),
        }
    }
}

/// Spawn the periodic summary logger. It holds only a `Weak<Counters>`, so once
/// the polling loop drops its last `Arc<Counters>` the task exits cleanly
/// instead of keeping the counters alive. A panic inside the logger is logged
/// and the logger is restarted.
fn spawn_summary_logger(
    counters:    &Arc<Counters>,
    n_contracts: usize,
    every:       Duration,
) -> tokio::task::JoinHandle<()> {
    let weak = Arc::downgrade(counters);
    tokio::spawn(async move {
        loop {
            let weak = weak.clone();
            let handle = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(every).await;
                    let Some(c) = weak.upgrade() else { break };
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
            match handle.await {
                Ok(()) => break,
                Err(e) => error!(error = ?e, "summary logger panicked — restarting"),
            }
        }
    })
}

/// Resolves once the shutdown flag is `true`. If the sender is dropped without
/// signalling, this never resolves so the poll loop keeps running.
async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

// ── Horizon access ───────────────────────────────────────────────────────────

/// One response from the Horizon transactions endpoint.
enum TransactionsPage {
    Records(Vec<HorizonTransactionWithOps>),
    /// Horizon answered 429. The caller backs off for `retry_after` seconds.
    RateLimited { retry_after: u64 },
}

/// The Horizon calls the poller makes. `reqwest::Client` implements this for
/// production; tests can implement it with canned responses and no HTTP server.
trait HorizonClient {
    /// Fetch one page of transactions for `contract_id` after `cursor`.
    async fn fetch_transactions(
        &self,
        base:        &str,
        contract_id: &str,
        cursor:      &str,
    ) -> Result<TransactionsPage>;

    /// Fetch the operations for a single transaction.
    async fn fetch_operations(&self, base: &str, tx_hash: &str) -> Result<Vec<HorizonOperation>>;
}

impl HorizonClient for Client {
    async fn fetch_transactions(
        &self,
        base:        &str,
        contract_id: &str,
        cursor:      &str,
    ) -> Result<TransactionsPage> {
        // Issue #23: use join=operations to fetch operations inline, eliminating
        // one HTTP request per transaction.
        let url = format!(
            "{}/accounts/{}/transactions?cursor={}&order=asc&limit=200&join=operations",
            base, contract_id, cursor
        );

        let response = self
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
            return Ok(TransactionsPage::RateLimited { retry_after });
        }

        if !response.status().is_success() {
            anyhow::bail!("Horizon returned HTTP {} for {}", response.status(), url);
        }

        let page: HorizonPage = response
            .json()
            .await
            .with_context(|| format!("failed to parse Horizon response from {}", url))?;

        Ok(TransactionsPage::Records(page._embedded.records))
    }

    async fn fetch_operations(&self, base: &str, tx_hash: &str) -> Result<Vec<HorizonOperation>> {
        let url = format!("{}/transactions/{}/operations", base, tx_hash);

        let page: OperationsPage = self
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {} failed", url))?
            .json()
            .await
            .with_context(|| format!("failed to parse operations from {}", url))?;

        Ok(page._embedded.records)
    }
}

/// Run exactly one poll cycle over every contract, then return. Uses the same
/// starting cursors as `run_with_shutdown`. Returns the number of contracts whose
/// poll failed; a contract that fails is logged and does not stop the others.
pub async fn run_once(cfg: AppConfig, dry_run: bool) -> Result<usize> {
    let client = build_http_client(&cfg)?;
    let mut cursors = initial_cursors(&cfg);
    let mut failed = 0;

    for contract in &cfg.contracts {
        if let Err(e) = poll_contract(&client, contract, &mut cursors, dry_run).await {
            error!(contract = %contract.label, error = %e, "contract polling task failed");
            failed += 1;
        }
    }

    Ok(failed)
}

fn build_http_client(cfg: &AppConfig) -> Result<Client> {
    let max_idle       = cfg.http_pool_max_idle_per_host.unwrap_or(10);
    let keepalive_secs = cfg.http_tcp_keepalive_secs.unwrap_or(30);
    let connect_secs   = cfg.http_connect_timeout_secs.unwrap_or(5);
    let request_secs   = cfg.http_request_timeout_secs.unwrap_or(15);

    Client::builder()
        .connect_timeout(Duration::from_secs(connect_secs))
        .timeout(Duration::from_secs(request_secs))
        .pool_max_idle_per_host(max_idle)
        .tcp_keepalive(Some(Duration::from_secs(keepalive_secs)))
        .build()
        .context("failed to build HTTP client")
}

/// Starting cursor for each contract: the value from `cursor_file` when present,
/// otherwise `"now"`.
fn initial_cursors(cfg: &AppConfig) -> HashMap<String, String> {
    if let Some(path) = &cfg.cursor_file {
        match fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, String>>(&raw) {
                Ok(mut map) => {
                    // Ensure every configured contract has a cursor entry.
                    for c in &cfg.contracts {
                        map.entry(c.contract_id.clone())
                            .or_insert_with(|| "now".to_string());
                    }
                    map
                }
                Err(e) => {
                    warn!(error = ?e, "failed to parse cursor_file; starting from 'now' for all contracts");
                    cfg.contracts
                        .iter()
                        .map(|c| (c.contract_id.clone(), "now".to_string()))
                        .collect()
                }
            },
            Err(e) => {
                debug!(error = ?e, "could not read cursor_file; starting from 'now'");
                cfg.contracts
                    .iter()
                    .map(|c| (c.contract_id.clone(), "now".to_string()))
                    .collect()
            }
        }
    } else {
        cfg.contracts
            .iter()
            .map(|c| (c.contract_id.clone(), "now".to_string()))
            .collect()
    }
}

// ── Per-contract poll ─────────────────────────────────────────────────────────

/// Returns `(transactions_processed, alerts_fired, alerts_suppressed_by_cap)`.
///
/// Uses `join=operations` on the transactions endpoint so that Horizon returns
/// operations inline, eliminating one HTTP request per transaction (#23).
/// Falls back to a separate `/transactions/{hash}/operations` fetch only when
/// the inline `operations` array is absent (older Horizon versions).
///
/// `horizon` serves the Horizon reads and `client` sends webhooks. They are
/// separate so the Horizon side can be replaced in tests (#302).
#[tracing::instrument(skip(horizon, client, contract, cursors), fields(
    contract    = %contract.label,
    contract_id = %contract.contract_id,
    network     = %contract.network.as_str()
))]
async fn poll_contract(
    client:         &Client,
    webhook_client: &Client,
    contract:       &WatchedContract,
    cursors:        &mut HashMap<String, String>,
    dry_run:        bool,
) -> Result<(u64, u64)> {
    let cursor = cursors
        .get(&contract.contract_id)
        .cloned()
        .unwrap_or_else(|| "now".to_string());

    // `poll_base` is used for all Horizon HTTP requests (may be overridden in tests).
    // `canonical_base` is always the production Horizon URL and is used only for
    // building horizon_link in payloads, so links always point to the real network.
    let poll_base = contract
        .test_horizon_base_url
        .as_deref()
        .unwrap_or_else(|| contract.network.horizon_base_url());
    let canonical_base = contract.network.horizon_base_url();

    // Collect all pages of transactions.
    let mut all_records: Vec<HorizonTransactionWithOps> = Vec::new();
    let mut page_cursor = cursor.clone();

    loop {
        // Issue #23: use join=operations to fetch operations inline, eliminating
        // one HTTP request per transaction.
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
            return Ok((0, 0, 0));
        }

        // Issue #378: Horizon returns 410 Gone once a paging cursor has expired.
        // Reset to "now" so the next poll resumes from the current ledger.
        if response.status() == reqwest::StatusCode::GONE {
            warn!(contract = %contract.label, cursor = %page_cursor, "Horizon returned 410 — cursor expired, resetting to \"now\"");
            cursors.insert(contract.contract_id.clone(), "now".to_string());
            return Ok((0, 0));
        }

        if !response.status().is_success() {
            anyhow::bail!("Horizon returned HTTP {} from {}", response.status(), url);
        }

        if !response.status().is_success() {
            anyhow::bail!("Horizon returned HTTP {} for {}", response.status(), url);
        }

        let page: HorizonPage = response
            .json()
            .await
            .with_context(|| format!("failed to parse Horizon response from {}", url))?;

        let records = page._embedded.records;
        if records.is_empty() {
            break;
        }

    if !first_response.status().is_success() {
        let status = first_response.status();
        return Err(anyhow::anyhow!(
            "Horizon returned {} for {}",
            status,
            first_url
        ));
    }

    let first_page: HorizonPage<HorizonTransactionWithOps> = first_response
        .json()
        .await
        .with_context(|| format!("failed to parse Horizon response from {}", first_url))?;

    let first_records = first_page._embedded.records;
    let first_count = first_records.len();
    let first_last_token = first_records.last().map(|r| r.tx.paging_token.clone());

    // Collect all pages: start with the first page already fetched, then use
    // paginate() to fetch any additional pages beyond the first.
    let mut all_records: Vec<HorizonTransactionWithOps> = first_records;

    if first_count >= 200 {
        if let Some(next_cursor) = first_last_token {
            let next_url = replace_cursor_param(&first_url, &next_cursor);
            let more = paginate::<HorizonTransactionWithOps>(
                client,
                &next_url,
                200,
                |r| r.tx.paging_token.as_str(),
            )
            .await?;
            all_records.extend(more);
        }
    }

static IDLE_CYCLE_COUNTERS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, u64>>> = std::sync::OnceLock::new();

fn should_log_idle(contract_id: &str) -> bool {
    let mut map = IDLE_CYCLE_COUNTERS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let count = map.entry(contract_id.to_string()).or_insert(0);
    *count += 1;
    *count == 1 || (*count % 60 == 0)
}

fn reset_idle_counter(contract_id: &str) {
    let mut map = IDLE_CYCLE_COUNTERS
        .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    map.insert(contract_id.to_string(), 0);
}

    if !all_records.is_empty() {
        reset_idle_counter(&contract.contract_id);
        info!(contract = %contract.label, count = all_records.len(), "fetched new transactions");
    } else if should_log_idle(&contract.contract_id) {
        debug!(contract = %contract.label, cursor = %cursor, "no new transactions");
    }

    let mut tx_count    = 0u64;
    let mut alert_count = 0u64;
    let mut suppressed  = 0u64;

    for record in all_records {
        let paging_token = record.tx.paging_token.clone();
        let tx_hash      = record.tx.hash.clone();

        // Advance cursor before enrichment so the tx is not re-processed even if
        // op enrichment fails.
        cursors.insert(contract.contract_id.clone(), paging_token.clone());

        // Issue #23: if Horizon returned inline operations, use them directly.
        // Otherwise fall back to a separate /operations fetch.
        let (function_names, amount_stroops) = if !record.operations.is_empty() {
            debug!(contract = %contract.label, tx = %tx_hash, "using inline operations (join=operations)");
            extract_soroban_details(record.operations)
        } else {
            match fetch_soroban_details(horizon, poll_base, &tx_hash).await {
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
                // Skip-and-advance (#305): the cursor was already moved past this
                // transaction above. Retrying would pin the cursor on a record that
                // can never be parsed and stall every transaction after it, so the
                // skip is counted instead and operators can alert on the metric.
                warn!(contract = %contract.label, tx = %tx_hash, error = %e,
                    "skipping transaction due to enrichment error");
                #[cfg(feature = "metrics")]
                metrics::inc_enrichment_errors();
                continue;
            }
        };

        tx_count += 1;

        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            &contract.network,
            canonical_base,
            contract.network.explorer_base_url(),
            &contract.rules,
            contract.tags.as_ref(),
            &enriched,
        );

        if payloads.is_empty() {
            debug!(contract = %contract.label, tx = %tx_hash, rules = ?contract.rules,
                "transaction evaluated but no rules matched");
        }

        for payload in payloads {
            if contract.max_alerts_per_cycle.is_some_and(|cap| alert_count >= cap) {
                suppressed += 1;
                continue;
            }
            alert_count += 1;
            info!(contract = %contract.label, rule = %payload.rule_triggered,
                tx = %payload.transaction_hash, "rule matched");

            if dry_run {
                info!(contract = %contract.label, rule = %payload.rule_triggered,
                    tx = %payload.transaction_hash, "dry-run enabled: not sending webhook");
            } else {
                info!(contract = %contract.label, rule = %payload.rule_triggered,
                    tx = %payload.transaction_hash, "rule fired — sending webhook");
                // #260: create a per-delivery oneshot. The sender is kept alive so the
                // backoff is never interrupted. To abort in-flight retries on shutdown,
                // drop the sender externally.
                let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
                std::mem::forget(_shutdown_tx);
                if let Err(e) = send_webhook(
                    webhook_client, &contract.webhook_url, &payload, contract.webhook_secret.as_deref(),
                    shutdown_rx,
                ).await {
                    error!(contract = %contract.label, rule = %payload.rule_triggered,
                        tx = %payload.transaction_hash, error = %e, "webhook delivery failed");
                    #[cfg(feature = "metrics")]
                    metrics::inc_webhook_failures();
                }
            }
        }
    }

    if suppressed > 0 {
        warn!(contract = %contract.label, cap = ?contract.max_alerts_per_cycle, suppressed,
            "alert cap reached: {} additional alerts suppressed this cycle", suppressed);
    }

    if tx_count > 0 {
        info!(contract = %contract.label, transactions = tx_count, alerts = alert_count,
            "poll cycle complete");
    }

    Ok((tx_count, alert_count, suppressed))
}

// ── Soroban operation enrichment ──────────────────────────────────────────────

/// Parse a Horizon amount string (e.g. `"1000.1234567"`) into stroops using integer
/// arithmetic only. Returns `None` for anything other than a plain decimal with at
/// most 7 fractional digits, rather than silently rounding.
fn parse_xlm_to_stroops(amount: &str) -> Option<u64> {
    const STROOPS_PER_XLM: u64 = 10_000_000;
    const FRACTION_DIGITS: usize = 7;

    let (whole, fraction) = amount.split_once('.').unwrap_or((amount, ""));
    if fraction.len() > FRACTION_DIGITS || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let whole_stroops = whole.parse::<u64>().ok()?.checked_mul(STROOPS_PER_XLM)?;
    // Right-pad the fraction to 7 digits: ".12" -> 1_200_000 stroops.
    let fraction_stroops = if fraction.is_empty() {
        0
    } else {
        format!("{:0<width$}", fraction, width = FRACTION_DIGITS).parse::<u64>().ok()?
    };

    whole_stroops.checked_add(fraction_stroops)
}

/// Extract Soroban details from a slice of already-fetched operations.
/// Used for both inline (join=operations) and separately-fetched operations.
fn extract_soroban_details(ops: Vec<HorizonOperation>) -> (Vec<String>, Option<u64>) {
    let mut function_names: Vec<String> = Vec::new();
    let mut total_stroops: u64 = 0;
    let mut has_payment = false;

    for op in ops {
        if op.op_type == "invoke_host_function" {
            if let Some(f) = op.function {
                function_names.push(f);
            }
        }
        if op.op_type == "payment" {
            if let Some(amt_str) = op.amount {
                if let Some(stroops) = parse_xlm_to_stroops(&amt_str) {
                    total_stroops = total_stroops.saturating_add(stroops);
                    has_payment = true;
                }
            }
        }
    }

    (function_names, if has_payment { Some(total_stroops) } else { None })
}

/// Fetch operations for a single transaction from Horizon.
/// Used as a fallback when `join=operations` is not supported or returned no ops.
#[tracing::instrument(skip(horizon), fields(tx = %tx_hash))]
async fn fetch_soroban_details<H: HorizonClient>(
    horizon: &H,
    base:    &str,
    tx_hash: &str,
) -> Result<(Vec<String>, Option<u64>)> {
    let ops = horizon.fetch_operations(base, tx_hash).await?;
    Ok(extract_soroban_details(ops))
}

/// Polling intervals below this many seconds trigger a startup rate-limit warning.
const MIN_RECOMMENDED_POLL_INTERVAL_SECS: u64 = 10;

/// Returns the estimated Horizon requests per minute when the configured interval is
/// below [`MIN_RECOMMENDED_POLL_INTERVAL_SECS`] (for any number of contracts), else `None`.
fn rate_limit_warning(poll_interval_seconds: u64, n_contracts: usize) -> Option<u64> {
    if poll_interval_seconds >= MIN_RECOMMENDED_POLL_INTERVAL_SECS || poll_interval_seconds == 0 {
        return None;
    }
    Some(n_contracts as u64 * 60 / poll_interval_seconds)
}

// ── Startup log field helpers (for testing) ──────────────────────────────────

#[cfg(test)]
fn startup_log_fields(cfg: &AppConfig) -> (String, String, String, String) {
    let contracts_list = cfg.contracts.iter().map(|c| c.label.as_str()).collect::<Vec<_>>().join(", ");
    let mut networks: Vec<&str> = cfg.contracts.iter().map(|c| c.network.as_str()).collect();
    networks.sort(); networks.dedup();
    let networks_str = networks.join(", ");
    let mut horizon_urls: Vec<(&str, &str)> = cfg
        .contracts.iter().map(|c| (c.network.as_str(), c.network.horizon_base_url())).collect();
    horizon_urls.sort(); horizon_urls.dedup();
    let horizon_urls_str = horizon_urls.iter()
        .map(|(net, url)| format!("{}={}", net, url)).collect::<Vec<_>>().join(", ");
    (env!("CARGO_PKG_VERSION").to_string(), contracts_list, networks_str, horizon_urls_str)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use reqwest::Client;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use txwatch_config::{AlertRule, AppConfig, Network, WatchedContract};

    use crate::cursor::CursorMap;
    use crate::enrichment::fetch_soroban_details;
    use crate::horizon::HorizonPage;
    use crate::poller::{poll_contract, startup_log_fields};

    fn empty_page() -> serde_json::Value {
        serde_json::json!({ "_embedded": { "records": [] } })
    }

    fn ops_page(function_name: &str) -> serde_json::Value {
        serde_json::json!({
            "_embedded": {
                "records": [{ "type": "invoke_host_function", "function": function_name }]
            }
        })
    }

    fn make_contract(
        label: &str,
        contract_id: &str,
        network: Network,
        webhook_url: &str,
        rules: Vec<AlertRule>,
        horizon_override: Option<String>,
    ) -> WatchedContract {
        WatchedContract {
            label: label.to_string(),
            contract_id: contract_id.to_string(),
            network,
            rules,
            webhook_url: webhook_url.to_string(),
            webhook_secret: None,
            horizon_base_url_override: horizon_override,
        }
    }

    const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    /// Issue #23: when Horizon returns inline operations via join=operations,
    /// the poller must parse them correctly without making a separate /operations request.
    #[tokio::test]
    async fn inline_operations_parsed_correctly() {
        let server = MockServer::start().await;

        let tx_with_ops = serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "inlinetx1",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "100",
                    "envelope_xdr": null,
                    "result_xdr":   null,
                    "operations": [
                        { "type": "invoke_host_function", "function": "withdraw" }
                    ]
                }]
            }
        });

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tx_with_ops))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let receiver = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&receiver)
            .await;

        let client = Client::new();
        let contract = WatchedContract {
            label:       "test".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::FunctionCalled { function_name: "withdraw".into() }],
            webhook_url: format!("{}/hook", receiver.uri()),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (txs, alerts) = poll_contract(&client, &client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 1);
        assert_eq!(alerts, 1, "FunctionCalled(withdraw) should fire from inline operations");

        let reqs = server.received_requests().await.unwrap();
        assert!(
            reqs.iter().all(|r| !r.url.path().contains("/operations")),
            "no separate /operations request should be made when inline ops are present"
        );
    }

    /// #269: with a per-cycle cap of 2, three matching transactions send two webhooks
    /// and report one suppressed alert.
    #[tokio::test]
    async fn alert_cap_suppresses_alerts_above_limit_in_one_cycle() {
        let server = MockServer::start().await;
        let records: Vec<serde_json::Value> = (1..=3)
            .map(|i| {
                serde_json::json!({
                    "hash":         format!("captx{}", i),
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": format!("{}", i),
                    "operations":   [{ "type": "invoke_host_function", "function": "withdraw" }]
                })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "_embedded": { "records": records } }),
            ))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let receiver = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&receiver)
            .await;

        let client = Client::new();
        let contract = WatchedContract {
            label:       "capped".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: format!("{}/hook", receiver.uri()),
            webhook_secret: None,
            horizon_url: Some(server.uri()),
            max_alerts_per_cycle: Some(2),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (txs, alerts, suppressed) =
            poll_contract(&client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 3);
        assert_eq!(alerts, 2, "only the first two alerts should be sent");
        assert_eq!(suppressed, 1, "the third alert should be suppressed, not dropped silently");
    }

    #[tokio::test]
    async fn poll_returns_ok_on_empty_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!(
            "{}/accounts/{}/transactions?cursor=now&order=asc&limit=200&join=operations",
            server.uri(), CONTRACT_ID
        );
        let page: HorizonPage<serde_json::Value> = client.get(&url).send().await.unwrap().json().await.unwrap();
        assert!(page._embedded.records.is_empty());
    }

    #[tokio::test]
    async fn fetch_soroban_details_extracts_function_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops_page("withdraw")))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert_eq!(fn_names, vec!["withdraw"]);
        assert!(amount.is_none());
    }

    #[test]
    fn parse_xlm_to_stroops_is_exact_where_f64_is_not() {
        // 1000000000.1234567 XLM is ~1e16 stroops, beyond f64's 2^53 exact range.
        assert_eq!(parse_xlm_to_stroops("1000000000.1234567"), Some(10_000_000_001_234_567));
    }

    #[test]
    fn parse_xlm_to_stroops_pads_short_fractions() {
        assert_eq!(parse_xlm_to_stroops("10"), Some(100_000_000));
        assert_eq!(parse_xlm_to_stroops("1.5"), Some(15_000_000));
        assert_eq!(parse_xlm_to_stroops("0.0000001"), Some(1));
    }

    #[test]
    fn parse_xlm_to_stroops_rejects_malformed_amounts() {
        assert_eq!(parse_xlm_to_stroops(""), None);
        assert_eq!(parse_xlm_to_stroops("abc"), None);
        assert_eq!(parse_xlm_to_stroops("-1"), None);
        assert_eq!(parse_xlm_to_stroops(".5"), None);
        assert_eq!(parse_xlm_to_stroops("1.12345678"), None);
    }

    #[test]
    fn extract_soroban_details_sums_payments_without_float_drift() {
        let ops = vec![HorizonOperation {
            op_type: "payment".into(),
            function: None,
            amount: Some("1000000000.1234567".into()),
        }];
        let (fn_names, amount) = extract_soroban_details(ops);
        assert!(fn_names.is_empty());
        assert_eq!(amount, Some(10_000_000_001_234_567));
    }

    #[tokio::test]
    async fn fetch_soroban_details_extracts_payment_amount() {
        let server = MockServer::start().await;
        let ops = serde_json::json!({
            "_embedded": { "records": [{ "type": "payment", "amount": "1000.0000000" }] }
        });
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert!(fn_names.is_empty());
        assert_eq!(amount, Some(10_000_000_000));
    }

    #[tokio::test]
    async fn fetch_soroban_details_returns_none_on_empty_ops() {
        let server = MockServer::start().await;
        let ops = serde_json::json!({ "_embedded": { "records": [] } });
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert!(fn_names.is_empty());
        assert!(amount.is_none());
    }

    #[tokio::test]
    async fn horizon_429_returns_ok_after_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract_id.to_string(), "now".to_string());

        let contract = WatchedContract {
            rules: vec![],
            label: "test".into(),
            rules: vec![txwatch_config::AlertRule::AnyTransaction],
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };

        // 429 is handled with a back-off and returns Ok((0,0)), not an error
        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        assert!(result.is_ok(), "429 should return Ok after back-off");
        assert_eq!(result.unwrap(), (0, 0, 0));
    }

    /// Issue #378: a 410 Gone (expired cursor) resets the cursor to "now".
    #[tokio::test]
    async fn horizon_410_resets_cursor_to_now() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(410))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract_id.to_string(), "12345".to_string());

        let contract = WatchedContract {
            label: "test".into(),
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };

        let result = poll_contract(&client, &client, &contract, &mut cursors, false).await;
        assert_eq!(result.unwrap(), (0, 0));
        assert_eq!(cursors.get(contract_id).map(String::as_str), Some("now"));
    }

    /// Issue #271: a 429 makes poll_contract sleep for the Retry-After duration
    /// (default 5s when absent) and return Ok((0, 0)). Paused time keeps it fast.
    async fn assert_429_backoff(retry_after: Option<&str>, expected_secs: u64) {
        let server = MockServer::start().await;
        let mut response = ResponseTemplate::new(429);
        if let Some(value) = retry_after {
            response = response.insert_header("Retry-After", value);
        }
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(response)
            .mount(&server)
            .await;

        let client = Client::new();
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract_id.to_string(), "now".to_string());

        let contract = WatchedContract {
            label: "test".into(),
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            horizon_base_url_override: Some(server.uri()),
        };

        let started = tokio::time::Instant::now();
        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        let elapsed = started.elapsed();

        assert_eq!(result.unwrap(), (0, 0));
        assert_eq!(elapsed, Duration::from_secs(expected_secs));
    }

    #[tokio::test(start_paused = true)]
    async fn horizon_429_retry_after_zero_backs_off_zero_seconds() {
        assert_429_backoff(Some("0"), 0).await;
    }

    #[tokio::test(start_paused = true)]
    async fn horizon_429_retry_after_five_backs_off_five_seconds() {
        assert_429_backoff(Some("5"), 5).await;
    }

    #[tokio::test(start_paused = true)]
    async fn horizon_429_missing_retry_after_defaults_to_five_seconds() {
        assert_429_backoff(None, 5).await;
    }

    #[tokio::test]
    async fn horizon_503_error_contains_status_code() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract_id.to_string(), "now".to_string());

        let contract = WatchedContract {
            rules: vec![],
            label: "test".into(),
            rules: vec![txwatch_config::AlertRule::AnyTransaction],
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };

        let err = poll_contract(&client, &contract, &mut cursors, false).await.unwrap_err();
        assert!(
            err.to_string().contains("503"),
            "error must contain HTTP status 503, got: {}", err
        );
    }

    // ── Trait-based Horizon doubles (#302) ──────────────────────────────────

    /// Horizon double whose transaction fetch always fails.
    struct FailingHorizon;

    impl HorizonClient for FailingHorizon {
        async fn fetch_transactions(
            &self, _base: &str, _contract_id: &str, _cursor: &str,
        ) -> Result<TransactionsPage> {
            anyhow::bail!("simulated Horizon outage")
        }

        async fn fetch_operations(&self, _base: &str, _tx_hash: &str) -> Result<Vec<HorizonOperation>> {
            Ok(Vec::new())
        }
    }

    /// Horizon double that serves `pages` in order, then empty pages.
    /// Transactions carry no inline operations, so enrichment makes a
    /// `fetch_operations` call for each one and gets an empty list back.
    struct ScriptedHorizon {
        pages: std::sync::Mutex<std::collections::VecDeque<Vec<HorizonTransactionWithOps>>>,
    }

    impl HorizonClient for ScriptedHorizon {
        async fn fetch_transactions(
            &self, _base: &str, _contract_id: &str, _cursor: &str,
        ) -> Result<TransactionsPage> {
            let next = self.pages.lock().unwrap().pop_front().unwrap_or_default();
            Ok(TransactionsPage::Records(next))
        }

        async fn fetch_operations(&self, _base: &str, _tx_hash: &str) -> Result<Vec<HorizonOperation>> {
            Ok(Vec::new())
        }
    }

    fn scripted_tx(hash: &str, paging_token: &str) -> HorizonTransactionWithOps {
        serde_json::from_value(serde_json::json!({
            "hash":         hash,
            "created_at":   "2024-06-01T10:00:00Z",
            "successful":   true,
            "paging_token": paging_token,
        }))
        .expect("valid transaction record")
    }

    fn test_contract(contract_id: &str) -> WatchedContract {
        WatchedContract {
            label:       "test".into(),
            contract_id: contract_id.into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };

        let result = poll_contract(&client, &client, &contract, &mut cursors, false).await;
        assert!(result.is_err(), "expected Err when Horizon returns 500");
        assert_eq!(
            cursors.get(CONTRACT_ID),
            "now",
            "cursor must not advance when the transactions fetch fails"
        );
    }

    #[tokio::test]
    async fn poll_contract_skips_tx_with_enrichment_error_and_advances_cursor() {
        let server = MockServer::start().await;
        let body = serde_json::json!({
            "_embedded": { "records": [
                {
                    "hash": "bad_tx",
                    "created_at": "not-a-timestamp",
                    "successful": true,
                    "paging_token": "1",
                    "operations": [{ "type": "invoke_host_function", "function": "transfer" }]
                },
                {
                    "hash": "good_tx",
                    "created_at": "2024-06-01T10:00:00Z",
                    "successful": true,
                    "paging_token": "2",
                    "operations": [{ "type": "invoke_host_function", "function": "transfer" }]
                }
            ] }
        });
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;

        let client      = Client::new();
        let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract_id.to_string(), "now".to_string());

        let contract = WatchedContract {
            label:       "test".into(),
            contract_id: contract_id.into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            horizon_url: Some(server.uri()),
            max_alerts_per_cycle: None,
        };

        let (tx_count, _alerts) = poll_contract(&client, &contract, &mut cursors, true)
            .await
            .expect("poll_contract should succeed");

        assert_eq!(
            cursors.get(contract_id).map(String::as_str),
            Some("2"),
            "cursor must advance to the second transaction's paging_token"
        );
        assert_eq!(tx_count, 1, "only the valid transaction counts");
    }

    #[test]
    fn rate_limit_warning_fires_below_threshold_for_single_contract() {
        assert_eq!(rate_limit_warning(5, 1), Some(12));
        assert_eq!(rate_limit_warning(5, 6), Some(72));
    }

    #[test]
    fn rate_limit_warning_silent_at_or_above_threshold() {
        assert_eq!(rate_limit_warning(10, 100), None);
        assert_eq!(rate_limit_warning(30, 1), None);
    }

    #[test]
    fn per_contract_summary_has_an_entry_for_every_configured_contract() {
        let counters = Counters::default();
        let labels = vec!["Contract A".to_string(), "Contract B".to_string()];

        counters.record_contract("Contract A", 3, 1);
        counters.record_contract("Contract A", 2, 0);

        let summary = counters.take_interval_per_contract(&labels);
        assert_eq!(
            summary,
            vec![
                ("Contract A".to_string(), 5, 1),
                ("Contract B".to_string(), 0, 0),
            ]
        );

        // Interval counters reset after each summary.
        let summary = counters.take_interval_per_contract(&labels);
        assert_eq!(
            summary,
            vec![
                ("Contract A".to_string(), 0, 0),
                ("Contract B".to_string(), 0, 0),
            ]
        );
    }

    #[test]
    fn startup_log_includes_version_contracts_list_and_networks() {
        let cfg = AppConfig {
            poll_interval_seconds: 10,
            cursor_file: None,
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
            cursor_file: None,
            contracts: vec![
                WatchedContract {
                    label:       "Contract A".into(),
                    contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                    network:     txwatch_config::Network::Testnet,
                    rules:       vec![txwatch_config::AlertRule::AnyTransaction],
                    webhook_url: "https://hooks.example.com/a".into(),
                    webhook_secret: None,
                    test_horizon_base_url: None,
                },
                WatchedContract {
                    label:       "Contract B".into(),
                    contract_id: "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into(),
                    network:     txwatch_config::Network::Mainnet,
                    rules:       vec![txwatch_config::AlertRule::AnyTransaction],
                    webhook_url: "https://hooks.example.com/b".into(),
                    webhook_secret: None,
                    test_horizon_base_url: None,
                },
                WatchedContract {
                    label:       "Contract C".into(),
                    contract_id: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC".into(),
                    network:     txwatch_config::Network::Mainnet,
                    rules:       vec![txwatch_config::AlertRule::AnyTransaction],
                    webhook_url: "https://hooks.example.com/c".into(),
                    webhook_secret: None,
                    test_horizon_base_url: None,
                },
            ],
            cursor_file: None,
        };

        let (version, contracts_list, networks, horizon_urls) = startup_log_fields(&cfg);
        assert!(!version.is_empty());
        assert_eq!(contracts_list, "Contract A, Contract B, Contract C");
        assert_eq!(networks, "mainnet, testnet");
        assert!(horizon_urls.contains("mainnet=https://horizon.stellar.org"));
        assert!(horizon_urls.contains("testnet=https://horizon-testnet.stellar.org"));
    }

    /// Reads a counter's current value from the default Prometheus registry.
    #[cfg(feature = "metrics")]
    fn counter_value(name: &str) -> f64 {
        prometheus::gather()
            .iter()
            .find(|family| family.get_name() == name)
            .and_then(|family| family.get_metric().first().map(|m| m.get_counter().get_value()))
            .unwrap_or(0.0)
    }

    /// #305: a transaction whose enrichment fails is skipped and counted, the cursor
    /// still moves past it, and the next transaction in the page is processed.
    #[tokio::test]
    async fn enrichment_failure_skips_tx_and_advances_cursor() {
        let server   = MockServer::start().await;
        let receiver = MockServer::start().await;

        // The first record has an unparseable created_at, so from_horizon fails.
        let page = serde_json::json!({
            "_embedded": {
                "records": [
                    {
                        "hash": "bad_ts_tx", "created_at": "not-a-timestamp",
                        "successful": true, "paging_token": "1",
                        "fee_charged": "100", "envelope_xdr": null, "result_xdr": null
                    },
                    {
                        "hash": "good_tx", "created_at": "2024-06-01T10:00:00Z",
                        "successful": true, "paging_token": "2",
                        "fee_charged": "100", "envelope_xdr": null, "result_xdr": null
                    }
                ]
            }
        });

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        // Only the parseable transaction matches AnyTransaction and fires a webhook.
        Mock::given(method("POST"))
            .and(path("/hooks"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&receiver)
            .await;

        let client   = Client::new();
        let contract = WatchedContract {
            label:       "Contract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: format!("{}/hooks", receiver.uri()),
            webhook_secret: None,
            horizon_base_url_override: Some(server.uri()),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        #[cfg(feature = "metrics")]
        let errors_before = counter_value("txwatch_enrichment_errors_total");

        let (txs, alerts) = poll_contract(&client, &client, &contract, &mut cursors, false).await.unwrap();

        assert_eq!(txs, 1, "only the parseable transaction is counted as processed");
        assert_eq!(alerts, 1);
        assert_eq!(
            cursors.get(&contract.contract_id).map(String::as_str),
            Some("2"),
            "cursor must advance past the skipped transaction so it is not retried"
        );

        #[cfg(feature = "metrics")]
        assert_eq!(
            counter_value("txwatch_enrichment_errors_total") - errors_before,
            1.0,
            "the skipped transaction must increment txwatch_enrichment_errors_total"
        );
    }

    // ── extract_soroban_details (#304) ───────────────────────────────────────

    fn payment_op(amount: &str) -> HorizonOperation {
        HorizonOperation {
            op_type:  "payment".into(),
            function: None,
            amount:   Some(amount.into()),
        }
    }

    fn invoke_op(function: &str) -> HorizonOperation {
        HorizonOperation {
            op_type:  "invoke_host_function".into(),
            function: Some(function.into()),
            amount:   None,
        }
    }

    #[test]
    fn extract_sums_two_payment_ops() {
        let ops = vec![payment_op("1.0000000"), payment_op("2.5000000")];
        let (function_names, total) = extract_soroban_details(ops);

        assert!(function_names.is_empty());
        assert_eq!(total, Some(35_000_000), "1.0 + 2.5 XLM is 35_000_000 stroops");
    }

    #[test]
    fn extract_saturates_when_payment_sum_overflows_u64() {
        // Each op is 1.2e19 stroops, which fits in u64 on its own (max ≈ 1.8e19),
        // but the sum does not. saturating_add must clamp it to u64::MAX.
        let ops = vec![
            payment_op("1200000000000.0000000"),
            payment_op("1200000000000.0000000"),
        ];
        let (_, total) = extract_soroban_details(ops);

        assert_eq!(total, Some(u64::MAX));
    }

    #[test]
    fn extract_populates_function_names_and_total_for_mixed_ops() {
        let ops = vec![
            invoke_op("withdraw"),
            payment_op("10.0000000"),
            payment_op("0.5000000"),
        ];
        let (function_names, total) = extract_soroban_details(ops);

        assert_eq!(function_names, vec!["withdraw"]);
        assert_eq!(total, Some(105_000_000), "10.5 XLM is 105_000_000 stroops");
    }

    #[tokio::test]
    async fn poll_handles_pagination_two_pages() {
        let server = MockServer::start().await;

        let mut records1 = Vec::new();
        for i in 1..=200u64 {
            records1.push(serde_json::json!({
                "hash": format!("tx{}", i),
                "created_at": "2020-01-01T00:00:00Z",
                "successful": true,
                "paging_token": format!("{}", i),
            }));
        }
        let page1 = serde_json::json!({ "_embedded": { "records": records1 } });
        let page2 = serde_json::json!({ "_embedded": { "records": [
            { "hash": "tx201", "created_at": "2020-01-01T00:00:01Z", "successful": true, "paging_token": "201" }
        ] } });

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page2))
            .mount(&server)
            .await;

        // Fallback /operations endpoint for transactions without inline ops
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        // One webhook POST per transaction (AnyTransaction). Unmocked receivers return 404
        // and trigger retry backoff, which would stall this test for minutes.
        Mock::given(method("POST"))
            .and(wiremock::matchers::path("/hooks"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract = WatchedContract {
            label:       "Contract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: format!("{}/hooks", receiver.uri()),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (txs, alerts) = poll_contract(&client, &client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 201);
        assert_eq!(alerts, 201);
    }

    #[test]
    fn replace_cursor_param_replaces_correctly() {
        let url = "https://horizon.stellar.org/accounts/C123/transactions?cursor=now&order=asc&limit=200";
        let updated = replace_cursor_param(url, "12345678");
        assert!(updated.contains("cursor=12345678"), "got: {}", updated);
        assert!(updated.contains("order=asc"), "should preserve other params, got: {}", updated);
        assert!(updated.contains("limit=200"), "should preserve other params, got: {}", updated);
    }

    #[test]
    fn replace_cursor_param_works_when_cursor_is_last() {
        let url = "https://horizon.stellar.org/accounts/C123/transactions?order=asc&cursor=now";
        let updated = replace_cursor_param(url, "99");
        assert!(updated.contains("cursor=99"), "got: {}", updated);
        assert!(updated.contains("order=asc"), "got: {}", updated);
    }

    #[tokio::test]
    async fn all_rules_fire_independently_on_single_transaction() {
        let server = MockServer::start().await;
        let receiver = MockServer::start().await;

        let tx_hash = "deadbeef12345678deadbeef12345678deadbeef12345678deadbeef12345678";
        let page_json = serde_json::json!({
            "_embedded": {
                "records": [
                    {
                        "id": tx_hash,
                        "hash": tx_hash,
                        "paging_token": "1001",
                        "successful": false,
                        "created_at": "2024-01-15T12:00:00Z",
                        "fee_charged": "1000",
                        "_embedded": {
                            "operations": [
                                {
                                    "type": "invoke_host_function",
                                    "function": "set_admin"
                                },
                                {
                                    "type": "payment",
                                    "amount": "1000.0000000"
                                }
                            ]
                        }
                    }
                ]
            }
        });

        Mock::given(method("GET"))
            .and(path_regex("^/accounts/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page_json))
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/hooks"))
            .respond_with(ResponseTemplate::new(200))
            .expect(4)
            .mount(&receiver)
            .await;

        let contract = WatchedContract {
            label: "TestContract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: Network::Testnet,
            rules: vec![
                AlertRule::AnyTransaction,
                AlertRule::TransactionFailed,
                AlertRule::LargeTransfer { threshold_xlm: 100 },
                AlertRule::AdminFunctionCalled { function_names: vec!["set_admin".into()] },
            ],
            webhook_url: format!("{}/hooks", receiver.uri()),
            webhook_secret: None,
            test_horizon_base_url: Some(server.uri()),
        };

        let client = Client::new();
        let mut cursors = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (_txs, alerts) = poll_contract(&client, &client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(alerts, 4, "all 4 rules should fire independently");

        let requests = receiver.received_requests().await.unwrap();
        assert_eq!(requests.len(), 4);
        let mut rule_types: Vec<String> = requests.iter().map(|req| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
            body["rule_type"].as_str().unwrap().to_string()
        }).collect();
        rule_types.sort();
        assert_eq!(rule_types, vec!["AdminFunctionCalled", "AnyTransaction", "LargeTransfer", "TransactionFailed"]);
    }
}
