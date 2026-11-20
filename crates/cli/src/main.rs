use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use reqwest::{Client, StatusCode};
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use txwatch_config::{AppConfig, WatchedContract};
use txwatch_notifier::{
    build_client_with_settings, send_webhook_once_compressed, test_payload_with_network,
    HttpClientSettings,
};

mod stream;

/// Debounce window for config file change events.
const CONFIG_WATCH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Spawns a background task that watches `path` for modifications and reloads
/// the config into `shared` on each debounced change. Parse/validation failures
/// are logged and the previous config is retained. The task exits when
/// `shutdown_rx` observes `true`.
fn spawn_config_watcher(
    path: PathBuf,
    shared: Arc<RwLock<AppConfig>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        use notify::{Event, EventKind, RecursiveMode, Watcher};

        let (event_tx, mut event_rx) = tokio::sync::mpsc::channel::<()>(16);
        let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| {
            if let Ok(event) = res {
                if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                    // Non-blocking send; drop if the channel is full (already pending reload).
                    let _ = event_tx.try_send(());
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "failed to create config file watcher; hot-reload disabled");
                return;
            }
        };

        // Watch the parent directory so atomic-rename saves (common editors) are detected.
        let watch_target = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        if let Err(e) = watcher.watch(&watch_target, RecursiveMode::NonRecursive) {
            warn!(error = %e, path = %watch_target.display(), "failed to watch config directory; hot-reload disabled");
            return;
        }

        info!(path = %path.display(), "config file watcher started");

        loop {
            tokio::select! {
                _ = shutdown_rx.changed() => {
                    if *shutdown_rx.borrow() {
                        info!("config file watcher shutting down");
                        break;
                    }
                }
                evt = event_rx.recv() => {
                    if evt.is_none() {
                        break;
                    }
                    // Debounce: drain any additional events that arrive within the window.
                    tokio::time::sleep(CONFIG_WATCH_DEBOUNCE).await;
                    while event_rx.try_recv().is_ok() {}

                    match AppConfig::from_file(&path) {
                        Ok(new_cfg) => {
                            let mut guard = shared.write().await;
                            *guard = new_cfg;
                            info!(path = %path.display(), "config reloaded");
                        }
                        Err(e) => {
                            warn!(error = %e, path = %path.display(), "config reload failed; keeping previous config");
                        }
                    }
                }
            }
        }
    });
}

// ── CLI definition ────────────────────────────────────────────────────────────

const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("TXWATCH_GIT_SHA"),
    " built ",
    env!("TXWATCH_BUILD_TIMESTAMP"),
    ")"
);

#[derive(Parser)]
#[command(
    name    = "txwatch",
    version = VERSION,
    about   = "Stellar Soroban contract monitor & webhook alert engine"
)]
struct Cli {
    /// Path to the TOML config file [env: TXWATCH_CONFIG]
    #[arg(short, long, env = "TXWATCH_CONFIG", default_value = "config/example.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine (watches all contracts in the config)
    Watch {
        /// Do not actually send webhooks; only log matched rules
        #[arg(long)]
        dry_run: bool,

        /// Start polling from the earliest available transaction (cursor = 0).
        /// Overrides any persisted cursor_file values for this run (does not modify the file).
        /// Warning: this may generate many alerts for historical transactions.
        #[arg(long)]
        from_beginning: bool,

        /// Run exactly one poll cycle across all contracts and exit.
        /// Useful for CI, one-shot cron jobs, or debugging without a long-running process.
        #[arg(long)]
        poll_once: bool,

        /// Optional address to bind the SSE /stream alert server (e.g. 127.0.0.1:8088)
        #[arg(long)]
        stream_addr: Option<std::net::SocketAddr>,

        /// Optional bearer token for SSE /stream endpoint authentication
        #[arg(long, env = "TXWATCH_STREAM_TOKEN")]
        stream_token: Option<String>,

        /// Only watch contracts whose label matches one of these (comma-separated)
        #[arg(long, value_delimiter = ',')]
        contracts: Option<Vec<String>>,

        /// Before polling, look up each contract_id on Horizon and warn (without
        /// failing) when it is not found on the configured network.
        #[arg(long)]
        check_contracts: bool,
    },

    /// Parse and validate the config file, then print a summary
    ///
    /// Exit codes: 0 = valid config, 1 = invalid or missing config
    Validate {
        /// Send a HEAD/OPTIONS request to each webhook URL and warn on unreachable endpoints.
        #[arg(long)]
        check_webhooks: bool,

        /// Look up each contract_id on its network's Horizon (GET /accounts/{contract_id})
        /// and warn (without failing) when it is not found.
        #[arg(long)]
        check_contracts: bool,

        /// Output format for the validation summary.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        output: OutputFormat,
    },

    /// Send a test webhook payload to a URL and exit
    ///
    /// Exit codes: 0 = webhook delivered, 1 = delivery failed (unreachable or HTTP error)
    TestWebhook {
        /// The webhook URL to POST to
        #[arg(long)]
        url: String,

        /// Label to include in the test payload
        #[arg(long, default_value = "TxWatch Test")]
        label: String,

        /// Sign the payload with HMAC-SHA256 using this secret (sent as X-TxWatch-Signature)
        #[arg(long)]
        secret: Option<String>,

        /// HTTP request timeout in seconds
        #[arg(long, default_value_t = 15)]
        timeout: u64,
    },
}

/// Output format for the `validate` subcommand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum OutputFormat {
    /// Human-readable summary (default).
    Text,
    /// Machine-readable JSON, for CI pipelines.
    Json,
}

// ── Validate summary formatter ────────────────────────────────────────────────

/// Renders the human-readable summary printed by the `validate` subcommand.
struct ValidateSummary<'a> {
    config: &'a AppConfig,
}

impl fmt::Display for ValidateSummary<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cfg = self.config;
        writeln!(f, "Config is valid.")?;
        writeln!(f, "  poll_interval_seconds : {}", cfg.poll_interval_seconds)?;
        writeln!(f, "  contracts             : {}", cfg.contracts.len())?;
        writeln!(f)?;
        for c in &cfg.contracts {
            writeln!(
                f,
                "  [{network}] {label}",
                network = c.network.display_name(),
                label = c.label
            )?;
            writeln!(f, "    contract_id  : {}", c.contract_id)?;
            writeln!(f, "    webhook_url  : {}", c.webhook_url)?;
            writeln!(
                f,
                "    secret       : {}",
                if c.webhook_secret.is_some() {
                    "set"
                } else {
                    "none"
                }
            )?;
            writeln!(f, "    rules        : {}", c.rules.len())?;
            for rule in &c.rules {
                writeln!(f, "      - {}", rule.label())?;
            }
            writeln!(f, "    horizon      : {}", c.network.horizon_base_url())?;
            writeln!(
                f,
                "    explorer     : {}/contract/{}",
                c.network.explorer_base_url(),
                c.contract_id
            )?;
        }
        Ok(())
    }
}

// ── Startup banner ────────────────────────────────────────────────────────────

/// Renders the human-readable startup banner printed when the watch engine starts.
struct StartupBanner<'a> {
    config: &'a AppConfig,
}

impl fmt::Display for StartupBanner<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cfg = self.config;
        let mut networks: Vec<String> = cfg
            .contracts
            .iter()
            .map(|c| c.network.display_name().to_string())
            .collect();
        networks.sort();
        networks.dedup();
        let networks = if networks.is_empty() {
            "none".to_string()
        } else {
            networks.join(", ")
        };

        writeln!(f, "╔══════════════════════════════════════════════════════════╗")?;
        writeln!(f, "║  txwatch — Stellar Soroban contract monitor              ║")?;
        writeln!(f, "╚══════════════════════════════════════════════════════════╝")?;
        writeln!(f, "  version        : {}", VERSION)?;
        writeln!(f, "  contracts      : {}", cfg.contracts.len())?;
        writeln!(f, "  networks       : {}", networks)?;
        writeln!(f, "  poll_interval  : {}s", cfg.poll_interval_seconds)?;
        Ok(())
    }
}

// ── Validate JSON output ──────────────────────────────────────────────────────

/// Results of the optional `validate` network checks for one contract.
/// `None` means the check was not requested.
#[derive(Default, Clone, Copy)]
struct ContractChecks {
    webhook_reachable: Option<bool>,
    contract_found: Option<bool>,
}

/// Builds the JSON document printed by `validate --output json` (Issue #530).
///
/// The webhook secret is never included — only whether one is set.
fn validate_json(cfg: &AppConfig, checks: &[ContractChecks]) -> serde_json::Value {
    let contracts: Vec<serde_json::Value> = cfg
        .contracts
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut entry = serde_json::json!({
                "label": c.label,
                "contract_id": c.contract_id,
                "network": c.network.as_str(),
                "webhook_url": c.webhook_url,
                "secret": if c.webhook_secret.is_some() { "set" } else { "none" },
                "rules": c.rules.iter().map(|r| r.label()).collect::<Vec<_>>(),
                "horizon": c.horizon_base_url(),
            });
            let check = checks.get(i).copied().unwrap_or_default();
            if let Some(reachable) = check.webhook_reachable {
                entry["webhook_reachable"] = serde_json::Value::Bool(reachable);
            }
            if let Some(found) = check.contract_found {
                entry["contract_found"] = serde_json::Value::Bool(found);
            }
            entry
        })
        .collect();

    serde_json::json!({
        "valid": true,
        "poll_interval_seconds": cfg.poll_interval_seconds,
        "contracts": contracts,
    })
}

// ── Webhook reachability helper ───────────────────────────────────────────────

/// Returns `Ok(true)` if the endpoint responded with a non-error status,
/// `Ok(false)` if it responded with an error status, and `Err` on transport failure.
async fn check_webhook_reachable(client: &Client, url: &str) -> Result<bool> {
    let resp = client
        .head(url)
        .send()
        .await
        .with_context(|| format!("HEAD request to '{}' failed", url))?;
    Ok(!resp.status().is_client_error() && !resp.status().is_server_error()
        && resp.status() != StatusCode::NOT_FOUND)
}

/// Runs the webhook reachability check for every contract, logging a warning
/// for each unreachable endpoint. Returns one result per contract.
async fn check_all_webhooks(client: &Client, contracts: &[WatchedContract]) -> Vec<bool> {
    let mut results = Vec::with_capacity(contracts.len());
    for c in contracts {
        let ok = match check_webhook_reachable(client, &c.webhook_url).await {
            Err(e) => {
                warn!(webhook_url = %c.webhook_url, contract = %c.label, error = %e, "webhook reachability check failed");
                false
            }
            Ok(false) => {
                warn!(webhook_url = %c.webhook_url, contract = %c.label, "webhook endpoint is unreachable");
                false
            }
            Ok(true) => true,
        };
        results.push(ok);
    }
    results
}

// ── Contract existence check (Issue #525) ─────────────────────────────────────

/// Looks up `contract_id` via `GET {horizon_base}/accounts/{contract_id}`.
///
/// Returns `Ok(true)` when Horizon knows the address, `Ok(false)` on 404, and
/// `Err` on transport failures or any other unexpected status.
async fn check_contract_exists(client: &Client, horizon_base: &str, contract_id: &str) -> Result<bool> {
    let url = format!("{}/accounts/{}", horizon_base.trim_end_matches('/'), contract_id);
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {} failed", url))?;
    match resp.status() {
        s if s.is_success() => Ok(true),
        StatusCode::NOT_FOUND => Ok(false),
        s => anyhow::bail!("Horizon returned HTTP {} for {}", s, url),
    }
}

/// Checks every contract against its network's Horizon. Never fails: a missing
/// contract or an unreachable Horizon only produces a warning, since the
/// contract may simply not have been deployed yet.
///
/// Returns one entry per contract: `Some(found)` when Horizon answered, or
/// `None` when the lookup itself failed.
async fn check_all_contracts(client: &Client, contracts: &[WatchedContract]) -> Vec<Option<bool>> {
    let mut results = Vec::with_capacity(contracts.len());
    for c in contracts {
        let horizon = c.horizon_base_url();
        let result = match check_contract_exists(client, horizon, &c.contract_id).await {
            Ok(true) => Some(true),
            Ok(false) => {
                warn!(
                    contract = %c.label,
                    contract_id = %c.contract_id,
                    network = %c.network.as_str(),
                    horizon = %horizon,
                    "contract_id not found on the configured network — check the network setting or deployment"
                );
                Some(false)
            }
            Err(e) => {
                warn!(contract = %c.label, contract_id = %c.contract_id, error = %e, "contract existence check failed");
                None
            }
        };
        results.push(result);
    }
    results
}

fn check_http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build HTTP client")
}

// ── Tracing ───────────────────────────────────────────────────────────────────

/// Installs the log subscriber. Logs go to stderr so that stdout carries only
/// command output (e.g. `validate --output json` stays machine-parseable).
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use txwatch_config::Network;

    fn contract(label: &str, contract_id: &str, network: Network, webhook_url: &str) -> WatchedContract {
        WatchedContract {
            label: label.to_string(),
            contract_id: contract_id.to_string(),
            network,
            rules: vec![],
            webhook_url: webhook_url.to_string(),
            webhook_secret: None,
            test_horizon_base_url: None,
            horizon_url: None,
            max_alerts_per_cycle: None,
            webhook_timeout_seconds: None,
            allow_internal_webhook_urls: None,
            tags: None,
            tls: None,
            webhook_tls_cert_pem: None,
            webhook_compress: false,
        }
    }

    fn fixed_config() -> AppConfig {
        // Deserialise (without validation) so every optional field takes its default.
        let mut cfg: AppConfig =
            serde_json::from_value(serde_json::json!({ "poll_interval_seconds": 30, "contracts": [] }))
                .expect("minimal AppConfig should deserialise");
        cfg.contracts = vec![
            contract(
                "alpha",
                "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM",
                Network::Testnet,
                "https://example.com/hook-a",
            ),
            contract(
                "beta",
                "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBSC4",
                Network::Mainnet,
                "https://example.com/hook-b",
            ),
            contract(
                "gamma",
                "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
                Network::Testnet,
                "https://example.com/hook-c",
            ),
        ];
        cfg
    }

    #[test]
    fn startup_banner_snapshot() {
        let cfg = fixed_config();
        let rendered = StartupBanner { config: &cfg }.to_string();
        let expected = format!(
            "╔══════════════════════════════════════════════════════════╗\n\
             ║  txwatch — Stellar Soroban contract monitor              ║\n\
             ╚══════════════════════════════════════════════════════════╝\n\
             \x20 version        : {}\n\
             \x20 contracts      : 3\n\
             \x20 networks       : Stellar Mainnet, Stellar Testnet\n\
             \x20 poll_interval  : 30s\n",
            VERSION
        );
        assert_eq!(rendered, expected);
    }

    #[test]
    fn validate_json_lists_contracts_with_label_rules_and_webhook_url() {
        let mut cfg = fixed_config();
        cfg.contracts[0].rules = vec![txwatch_config::AlertRule::AnyTransaction];
        cfg.contracts[1].webhook_secret = Some("super-secret".into());

        let json = validate_json(&cfg, &[]);
        let contracts = json["contracts"].as_array().expect("contracts array");
        assert_eq!(json["valid"], true);
        assert_eq!(json["poll_interval_seconds"], 30);
        assert_eq!(contracts.len(), 3);
        assert_eq!(contracts[0]["label"], "alpha");
        assert_eq!(contracts[0]["webhook_url"], "https://example.com/hook-a");
        assert_eq!(contracts[0]["rules"], serde_json::json!(["AnyTransaction"]));
        assert_eq!(contracts[1]["secret"], "set");
        assert!(contracts[0].get("contract_found").is_none());
        assert!(
            !json.to_string().contains("super-secret"),
            "secret must never appear in JSON output"
        );
    }

    #[test]
    fn validate_json_includes_check_results_when_run() {
        let cfg = fixed_config();
        let checks = vec![
            ContractChecks { webhook_reachable: Some(true), contract_found: Some(false) },
            ContractChecks::default(),
            ContractChecks::default(),
        ];
        let json = validate_json(&cfg, &checks);
        assert_eq!(json["contracts"][0]["webhook_reachable"], true);
        assert_eq!(json["contracts"][0]["contract_found"], false);
        assert!(json["contracts"][1].get("webhook_reachable").is_none());
    }

    #[test]
    fn debug_output_redacts_webhook_secret() {
        let mut cfg = fixed_config();
        cfg.contracts[0].webhook_secret = Some("super-secret".into());
        let dumped = format!("{:?}", cfg);
        assert!(!dumped.contains("super-secret"), "Debug leaked secret: {}", dumped);
        assert!(dumped.contains("<redacted>"));
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Validate { check_webhooks, check_contracts, output } => {
            let cfg = AppConfig::from_file(&cli.config)?;

            let mut checks = vec![ContractChecks::default(); cfg.contracts.len()];
            if check_webhooks || check_contracts {
                let client = check_http_client()?;
                if check_webhooks {
                    for (check, ok) in checks.iter_mut().zip(check_all_webhooks(&client, &cfg.contracts).await) {
                        check.webhook_reachable = Some(ok);
                    }
                }
                if check_contracts {
                    for (check, found) in checks.iter_mut().zip(check_all_contracts(&client, &cfg.contracts).await) {
                        check.contract_found = found;
                    }
                }
            }

            match output {
                OutputFormat::Text => print!("{}", ValidateSummary { config: &cfg }),
                OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&validate_json(&cfg, &checks))
                        .context("failed to serialise validate output")?
                ),
            }
        }

        Command::TestWebhook { url, label, secret, timeout } => {
            let cfg = AppConfig::from_file(&cli.config)?;
            let first_contract = cfg.contracts.first().ok_or_else(|| {
                anyhow::anyhow!("config has no contracts; cannot derive network for test-webhook")
            })?;
            let payload = test_payload_with_network(
                &label,
                &url,
                &first_contract.network,
                first_contract.network.horizon_base_url(),
            );
            let client = build_client_with_settings(&HttpClientSettings {
                timeout: Duration::from_secs(timeout),
                ..HttpClientSettings::default()
            })
            .context("failed to build HTTP client")?;

            info!(url = %url, timeout_secs = timeout, "sending test webhook");
            send_webhook_once_compressed(
                &client,
                &url,
                &payload,
                secret.as_deref(),
                first_contract.webhook_compress,
            )
            .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            let signing = if secret.is_some() { "signed with HMAC-SHA256" } else { "unsigned" };
            println!("Test webhook delivered successfully to {} ({})", url, signing);
        }

        Command::Watch {
            dry_run,
            from_beginning,
            poll_once,
            stream_addr,
            stream_token,
            contracts,
            check_contracts,
        } => {
            let mut cfg = AppConfig::from_file(&cli.config)?;

            if let Some(labels) = contracts {
                cfg.contracts.retain(|c| labels.iter().any(|l| l == &c.label));
                if cfg.contracts.is_empty() {
                    anyhow::bail!(
                        "--contracts filter matched no configured contract labels: {}",
                        labels.join(", ")
                    );
                }
            }

            if check_contracts {
                let client = check_http_client()?;
                check_all_contracts(&client, &cfg.contracts).await;
            }

            let mut networks: Vec<String> = cfg
                .contracts
                .iter()
                .map(|c| c.network.display_name().to_string())
                .collect();
            networks.sort();
            networks.dedup();

            info!(
                version             = VERSION,
                contracts           = cfg.contracts.len(),
                poll_interval_seconds = cfg.poll_interval_seconds,
                networks            = ?networks,
                "starting txwatch"
            );
            info!("\n{}", StartupBanner { config: &cfg });

            let alerts_tx = match stream_addr {
                Some(addr) => {
                    let (tx, rx) = tokio::sync::broadcast::channel(128);
                    tokio::spawn(async move {
                        if let Err(e) = crate::stream::run_sse_server(addr, stream_token, rx).await {
                            warn!(error = ?e, "SSE server terminated");
                        }
                    });
                    Some(tx)
                }
                None => None,
            };

            if poll_once {
                let failed = txwatch_poller::run_once_with_broadcast(cfg, dry_run, alerts_tx).await?;
                if failed > 0 {
                    return Err(anyhow::anyhow!("{} contract poll(s) failed", failed));
                }
                info!("poll-once cycle completed for all contracts");
                return Ok(());
            }

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            // Handles both Ctrl+C (SIGINT) and SIGTERM (e.g. from Docker/systemd).
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            // Hot-reload: watch the config file and swap the shared config on change.
            let shared_cfg = Arc::new(RwLock::new(cfg));
            spawn_config_watcher(
                cli.config.clone(),
                Arc::clone(&shared_cfg),
                shutdown_rx.clone(),
            );

            tokio::spawn(async move {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{signal, SignalKind};
                    let mut sigterm = signal(SignalKind::terminate())
                        .expect("failed to install SIGTERM handler");
                    tokio::select! {
                        result = tokio::signal::ctrl_c() => {
                            if let Err(e) = result {
                                warn!(error = ?e, "failed to install Ctrl+C handler");
                            }
                        }
                        _ = sigterm.recv() => {
                            info!("received SIGTERM");
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    if let Err(e) = tokio::signal::ctrl_c().await {
                        warn!(error = ?e, "failed to install Ctrl+C handler");
                    }
                }
                info!("shutdown signal received — finishing current poll cycle");
                let _ = shutdown_tx.send(true);
            });

            let cfg = shared_cfg.read().await.clone();
            txwatch_poller::run_with_broadcast(cfg, dry_run, from_beginning, shutdown_rx, alerts_tx)
                .await?;
        }
    }

    Ok(())
}
