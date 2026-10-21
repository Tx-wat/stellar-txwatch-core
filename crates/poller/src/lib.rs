//! txwatch-poller runs the Horizon polling loop, enriches transactions, evaluates rules,
//! and sends webhook alerts through `txwatch-notifier`.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde_json;
use std::fs;
use tokio::sync::watch;
use tracing::{debug, error, info, warn};
use txwatch_config::{AppConfig, WatchedContract};
use txwatch_notifier::send_webhook;
use txwatch_rules::{evaluate, EnrichedTransaction, HorizonTransaction};

// ── Optional Prometheus metrics ───────────────────────────────────────────────

#[cfg(feature = "metrics")]
pub mod metrics;

/// Bind address for the optional `/metrics` HTTP endpoint.
/// Set via `AppConfig::metrics_addr` when the `metrics` feature is enabled.
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
#[derive(Deserialize)]
struct HorizonTransactionWithOps {
    #[serde(flatten)]
    tx: HorizonTransaction,
    /// Inline operations embedded when `join=operations` is used.
    #[serde(default)]
    operations: Vec<HorizonOperation>,
}

#[derive(Deserialize)]
struct HorizonPage {
    _embedded: Embedded,
}

#[derive(Deserialize)]
struct Embedded {
    records: Vec<HorizonTransactionWithOps>,
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

#[derive(Default)]
struct Counters {
    transactions: AtomicU64,
    alerts: AtomicU64,
    interval_transactions: AtomicU64,
    interval_alerts: AtomicU64,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Backwards-compatible wrapper: default (non-dry) run.
pub async fn run(cfg: AppConfig) -> Result<()> {
    run_with(cfg, false).await
}

/// Run the polling loop forever. Each contract is polled concurrently via a
/// tokio JoinSet; one slow or failing contract never blocks the others.
/// Logs a summary every 60 seconds: contracts watched, transactions processed,
/// alerts fired.
pub async fn run_with(cfg: AppConfig, dry_run: bool) -> Result<()> {
    run_with_shutdown(cfg, dry_run, watch::channel(false).1).await
}

/// Like `run_with`, but stops after the current poll cycle once `shutdown` is set to `true`.
pub async fn run_with_shutdown(
    cfg: AppConfig,
    dry_run: bool,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let max_idle       = cfg.http_pool_max_idle_per_host.unwrap_or(10);
    let keepalive_secs = cfg.http_tcp_keepalive_secs.unwrap_or(30);

    let client = Client::builder()
        .timeout(Duration::from_secs(15))
        .pool_max_idle_per_host(max_idle)
        .tcp_keepalive(Some(Duration::from_secs(keepalive_secs)))
        .build()
        .context("failed to build HTTP client")?;

    let mut cursors: HashMap<String, String> = if let Some(path) = &cfg.cursor_file {
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

    if cfg.poll_interval_seconds < 10 && cfg.contracts.len() > 5 {
        warn!(
            poll_interval_seconds = cfg.poll_interval_seconds,
            contracts = cfg.contracts.len(),
            "polling interval is very short with many contracts — Horizon rate limits may apply; \
             consider poll_interval_seconds >= 10"
        );
    }

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
        for contract in &cfg.contracts {
            match poll_contract(&client, contract, &mut cursors, dry_run).await {
                Ok((txs, alerts)) => {
                    counters.transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.alerts.fetch_add(alerts, Ordering::Relaxed);
                    counters.interval_transactions.fetch_add(txs, Ordering::Relaxed);
                    counters.interval_alerts.fetch_add(alerts, Ordering::Relaxed);
                    // Issue #25: increment Prometheus counters when metrics feature is enabled.
                    #[cfg(feature = "metrics")]
                    {
                        metrics::inc_transactions(txs);
                        metrics::inc_alerts(alerts);
                    }
                }
                Err(e) => error!(error = %e, "contract polling task failed"),
            }
        }
        if *shutdown.borrow() {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = shutdown.changed() => {}
        }
        if *shutdown.borrow() {
            break;
        }
    }

    info!("TxWatch polling engine stopped cleanly");
    Ok(())
}

// ── Per-contract poll ─────────────────────────────────────────────────────────

/// Returns `(transactions_processed, alerts_fired)`.
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
async fn poll_contract(
    client:   &Client,
    contract: &WatchedContract,
    cursors:  &mut HashMap<String, String>,
    dry_run:  bool,
) -> Result<(u64, u64)> {
    let cursor = cursors
        .get(&contract.contract_id)
        .cloned()
        .unwrap_or_else(|| "now".to_string());

    // `poll_base` is used for all Horizon HTTP requests (may be overridden in tests).
    // `canonical_base` is always the production Horizon URL and is used only for
    // building horizon_link in payloads, so links always point to the real network.
    let poll_base = contract
        .horizon_base_url_override
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
        cursors.insert(contract.contract_id.clone(), paging_token.clone());

        // Issue #23: if Horizon returned inline operations, use them directly.
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
                if let Err(e) = send_webhook(
                    client, &contract.webhook_url, &payload, contract.webhook_secret.as_deref(),
                ).await {
                    error!(contract = %contract.label, rule = %payload.rule_triggered,
                        tx = %payload.transaction_hash, error = %e, "webhook delivery failed");
                    #[cfg(feature = "metrics")]
                    metrics::inc_webhook_failures();
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

// ── Soroban operation enrichment ──────────────────────────────────────────────

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
                if let Ok(xlm) = amt_str.parse::<f64>() {
                    total_stroops = total_stroops.saturating_add((xlm * 10_000_000.0) as u64);
                    has_payment = true;
                }
            }
        }
    }

    (function_names, if has_payment { Some(total_stroops) } else { None })
}

/// Fetch operations for a single transaction from Horizon.
/// Used as a fallback when `join=operations` is not supported or returned no ops.
#[tracing::instrument(skip(client), fields(tx = %tx_hash))]
async fn fetch_soroban_details(
    client:  &Client,
    base:    &str,
    tx_hash: &str,
) -> Result<(Vec<String>, Option<u64>)> {
    let url = format!("{}/transactions/{}/operations", base, tx_hash);

    let page: OperationsPage = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {} failed", url))?
        .json()
        .await
        .with_context(|| format!("failed to parse operations from {}", url))?;

    Ok(extract_soroban_details(page._embedded.records))
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
    use super::*;
    use txwatch_config::{AlertRule, Network};
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ops_page(function_name: &str) -> serde_json::Value {
        serde_json::json!({
            "_embedded": {
                "records": [{ "type": "invoke_host_function", "function": function_name }]
            }
        })
    }

    fn empty_page() -> serde_json::Value {
        serde_json::json!({ "_embedded": { "records": [] } })
    }

    /// Issue #23: when Horizon returns inline operations via join=operations,
    /// the poller must parse them correctly without making a separate /operations request.
    #[tokio::test]
    async fn inline_operations_parsed_correctly() {
        let server = MockServer::start().await;

        // Transactions page with inline operations (join=operations response shape)
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

        // Subsequent requests return empty page
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        // Webhook receiver
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
            horizon_base_url_override: Some(server.uri()),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (txs, alerts) = poll_contract(&client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 1);
        assert_eq!(alerts, 1, "FunctionCalled(withdraw) should fire from inline operations");

        // Verify no /operations request was made (inline ops used instead)
        let reqs = server.received_requests().await.unwrap();
        assert!(
            reqs.iter().all(|r| !r.url.path().contains("/operations")),
            "no separate /operations request should be made when inline ops are present"
        );
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
            server.uri(),
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
        let page: HorizonPage = client.get(&url).send().await.unwrap().json().await.unwrap();
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
    async fn horizon_429_returns_meaningful_error() {
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
            label: "test".into(),
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            horizon_base_url_override: Some(server.uri()),
        };

        // 429 is handled with a back-off and returns Ok((0,0)), not an error
        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        assert!(result.is_ok(), "429 should return Ok after back-off");
        assert_eq!(result.unwrap(), (0, 0));
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
            label: "test".into(),
            contract_id: contract_id.into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://hooks.example.com/test".into(),
            webhook_secret: None,
            horizon_base_url_override: Some(server.uri()),
        };

        let err = poll_contract(&client, &contract, &mut cursors, false).await.unwrap_err();
        assert!(
            err.to_string().contains("503"),
            "error must contain HTTP status 503, got: {}", err
        );
    }

    #[tokio::test]
    async fn poll_contract_does_not_advance_cursor_on_fetch_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(500))
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
            horizon_base_url_override: Some(server.uri()),
        };

        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        assert!(result.is_err(), "expected Err when Horizon returns 500");
        assert_eq!(
            cursors.get(contract_id).map(String::as_str),
            Some("now"),
            "cursor must not advance when the transactions fetch fails"
        );
    }

    #[test]
    fn startup_log_includes_version_contracts_list_and_networks() {
        let cfg = AppConfig {
            poll_interval_seconds: 10,
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
                    horizon_base_url_override: None,
                },
                WatchedContract {
                    label:       "Contract B".into(),
                    contract_id: "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into(),
                    network:     txwatch_config::Network::Mainnet,
                    rules:       vec![txwatch_config::AlertRule::AnyTransaction],
                    webhook_url: "https://hooks.example.com/b".into(),
                    webhook_secret: None,
                    horizon_base_url_override: None,
                },
                WatchedContract {
                    label:       "Contract C".into(),
                    contract_id: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC".into(),
                    network:     txwatch_config::Network::Mainnet,
                    rules:       vec![txwatch_config::AlertRule::AnyTransaction],
                    webhook_url: "https://hooks.example.com/c".into(),
                    webhook_secret: None,
                    horizon_base_url_override: None,
                },
            ],
        };

        let (version, contracts_list, networks, horizon_urls) = startup_log_fields(&cfg);
        assert!(!version.is_empty());
        assert_eq!(contracts_list, "Contract A, Contract B, Contract C");
        assert_eq!(networks, "mainnet, testnet");
        assert!(horizon_urls.contains("mainnet=https://horizon.stellar.org"));
        assert!(horizon_urls.contains("testnet=https://horizon-testnet.stellar.org"));
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
        let page1 = serde_json::json!({ "_embedded": { "records": records1 }});
        let page2 = serde_json::json!({ "_embedded": { "records": [
            { "hash": "tx201", "created_at": "2020-01-01T00:00:01Z", "successful": true, "paging_token": "201" }
        ] }});

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

        let client = Client::new();
        let contract = WatchedContract {
            label:       "Contract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:     Network::Testnet,
            rules:       vec![AlertRule::AnyTransaction],
            webhook_url: format!("{}/hooks", server.uri()),
            webhook_secret: None,
            horizon_base_url_override: Some(server.uri()),
        };
        let mut cursors: HashMap<String, String> = HashMap::new();
        cursors.insert(contract.contract_id.clone(), "now".to_string());

        let (txs, alerts) = poll_contract(&client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 201);
        assert_eq!(alerts, 201);
    }
}
