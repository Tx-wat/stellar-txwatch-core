use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use txwatch_config::AppConfig;
use txwatch_notifier::{build_client, send_webhook_once, test_payload_with_network};

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
    /// Print version information and exit
    #[arg(short = 'V', long = "version", action = clap::ArgAction::Version)]
    version: Option<bool>,

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
    },

    /// Parse and validate the config file, then print a summary
    ///
    /// Exit codes: 0 = valid config, 1 = invalid or missing config
    Validate {
        /// Send a HEAD/OPTIONS request to each webhook URL and warn on unreachable endpoints.
        #[arg(long)]
        check_webhooks: bool,
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

    /// Import a contract watchlist from a JSON file and write a TOML config
    ///
    /// The JSON file must contain a top-level `contracts` array. Each entry
    /// requires `contract_id`, `label`, `network`, and `webhook_url`.
    ///
    /// Exit codes: 0 = import succeeded, 1 = invalid JSON or validation error
    Import {
        /// Path to the JSON watchlist file to import
        #[arg(long = "from", value_name = "JSON")]
        from: PathBuf,

        /// Path to write the generated TOML config to
        #[arg(long, value_name = "TOML")]
        output: PathBuf,
    },
}

// ── JSON import schema ────────────────────────────────────────────────────────

/// Top-level JSON import document.
#[derive(Debug, Deserialize)]
struct ImportDocument {
    contracts: Vec<ImportContract>,
}

/// A single contract entry in the JSON import document.
#[derive(Debug, Deserialize)]
struct ImportContract {
    contract_id: String,
    label: String,
    network: String,
    webhook_url: String,
    #[serde(default)]
    webhook_secret: Option<String>,
}

/// Validates a Stellar contract ID: a 56-character StrKey starting with `C`
/// and consisting only of base32 characters.
fn is_valid_contract_id(id: &str) -> bool {
    id.len() == 56
        && id.starts_with('C')
        && id
            .chars()
            .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c))
}

/// Validates the imported contracts and returns a list of human-readable errors.
fn validate_import(doc: &ImportDocument) -> Vec<String> {
    let mut errors = Vec::new();
    if doc.contracts.is_empty() {
        errors.push("import contains no contracts".to_string());
    }
    for (i, c) in doc.contracts.iter().enumerate() {
        if c.label.trim().is_empty() {
            errors.push(format!("contracts[{i}]: label must not be empty"));
        }
        if !is_valid_contract_id(&c.contract_id) {
            errors.push(format!(
                "contracts[{i}]: invalid contract_id '{}' (expected 56-char StrKey starting with 'C')",
                c.contract_id
            ));
        }
        if c.network.trim().is_empty() {
            errors.push(format!("contracts[{i}]: network must not be empty"));
        }
        if !(c.webhook_url.starts_with("http://") || c.webhook_url.starts_with("https://")) {
            errors.push(format!(
                "contracts[{i}]: webhook_url '{}' must start with http:// or https://",
                c.webhook_url
            ));
        }
    }
    errors
}

/// Renders validated import contracts into the TOML config format.
fn render_import_toml(doc: &ImportDocument) -> String {
    let mut out = String::new();
    for c in &doc.contracts {
        out.push_str("[[contracts]]\n");
        out.push_str(&format!("contract_id = {:?}\n", c.contract_id));
        out.push_str(&format!("label = {:?}\n", c.label));
        out.push_str(&format!("network = {:?}\n", c.network));
        out.push_str(&format!("webhook_url = {:?}\n", c.webhook_url));
        if let Some(secret) = &c.webhook_secret {
            out.push_str(&format!("webhook_secret = {:?}\n", secret));
        }
        out.push('\n');
    }
    out
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
            writeln!(
                f,
                "    description  : {}",
                c.description.as_deref().unwrap_or("none")
            )?;
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use txwatch_config::{ContractConfig, Network};

    fn fixed_config() -> AppConfig {
        AppConfig {
            poll_interval_seconds: 30,
            contracts: vec![
                ContractConfig {
                    label: "alpha".to_string(),
                    contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
                        .to_string(),
                    network: Network::Testnet,
                    webhook_url: "https://example.com/hook-a".to_string(),
                    webhook_secret: None,
                    rules: vec![],
                    cursor_file: None,
                },
                ContractConfig {
                    label: "beta".to_string(),
                    contract_id: "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBSC4"
                        .to_string(),
                    network: Network::Mainnet,
                    webhook_url: "https://example.com/hook-b".to_string(),
                    webhook_secret: None,
                    rules: vec![],
                    cursor_file: None,
                },
                ContractConfig {
                    label: "gamma".to_string(),
                    contract_id: "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC"
                        .to_string(),
                    network: Network::Testnet,
                    webhook_url: "https://example.com/hook-c".to_string(),
                    webhook_secret: None,
                    rules: vec![],
                    cursor_file: None,
                },
            ],
        }
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
             \x20 networks       : Mainnet, Testnet\n\
             \x20 poll_interval  : 30s\n",
            VERSION
        );
        assert_eq!(rendered, expected);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // With the `opentelemetry` feature, the returned provider flushes buffered spans on drop.
    #[cfg(feature = "opentelemetry")]
    let _telemetry = init_tracing();
    #[cfg(not(feature = "opentelemetry"))]
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Validate { check_webhooks } => {
            let cfg = AppConfig::from_file(&cli.config)?;
            println!("{}", ValidateSummary { config: &cfg });

            if check_webhooks {
                let client = Client::builder()
                    .timeout(Duration::from_secs(15))
                    .build()
                    .context("failed to build HTTP client")?;

                for c in &cfg.contracts {
                    match check_webhook_reachable(&client, &c.webhook_url).await {
                        Err(e) => {
                            warn!(webhook_url = %c.webhook_url, contract = %c.label, error = %e, "webhook reachability check failed");
                        }
                        Ok(false) => {
                            warn!(webhook_url = %c.webhook_url, contract = %c.label, "webhook endpoint is unreachable");
                        }
                        Ok(true) => {}
                    }
                }
            }
        }

        Command::TestWebhook { url, label, secret, timeout } => {
            let cfg = AppConfig::from_file(&cli.config)?;
            if cfg.contracts.is_empty() {
                return Err(anyhow::anyhow!(
                    "config has no contracts; cannot derive network for test-webhook"
                ));
            }
            let first_contract = &cfg.contracts[0];
            let payload = test_payload_with_network(&label, &first_contract.network);

            // Prefer the explicit --secret flag, otherwise fall back to the
            // webhook_secret configured for the first contract.
            let secret = secret.or_else(|| first_contract.webhook_secret.clone());

            let client = build_client(Duration::from_secs(timeout))
                .context("failed to build HTTP client")?;

            info!(url = %url, timeout_secs = timeout, "sending test webhook");
            send_webhook_once(&client, &url, &payload, secret.as_deref())
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;

            match secret.as_deref() {
                Some(s) => {
                    let signature = txwatch_notifier::sign_payload(&payload, s);
                    println!("X-TxWatch-Signature: {}", signature);
                    println!(
                        "Test webhook delivered successfully to {} (signed with HMAC-SHA256)",
                        url
                    );
                }
                None => {
                    println!(
                        "Test webhook delivered successfully to {} (unsigned)",
                        url
                    );
                }
            }
        }

        Command::Import { from, output } => {
            let raw = std::fs::read_to_string(&from)
                .with_context(|| format!("failed to read import file '{}'", from.display()))?;
            let doc: ImportDocument = serde_json::from_str(&raw)
                .with_context(|| format!("failed to parse JSON import file '{}'", from.display()))?;

            let errors = validate_import(&doc);
            if !errors.is_empty() {
                for e in &errors {
                    eprintln!("error: {}", e);
                }
                return Err(anyhow::anyhow!(
                    "import validation failed with {} error(s)",
                    errors.len()
                ));
            }

            let toml = render_import_toml(&doc);
            std::fs::write(&output, toml)
                .with_context(|| format!("failed to write output file '{}'", output.display()))?;
            println!(
                "Imported {} contract(s) from {} to {}",
                doc.contracts.len(),
                from.display(),
                output.display()
            );
        }

        Command::Watch {
            dry_run,
            from_beginning,
            poll_once,
            stream_addr,
            stream_token,
        } => {
            let cfg = AppConfig::from_file(&cli.config)?;

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

            let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(128);
            let broadcast_tx = if stream_addr.is_some() {
                Some(broadcast_tx)
            } else {
                None
            };

            if let Some(addr) = stream_addr {
                let rx = broadcast_rx;
                let tx = broadcast_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = crate::stream::run_sse_server(addr, stream_token, rx).await {
                        warn!(error = ?e, "SSE server terminated");
                    }
                });
                let _ = tx;
            }

            if poll_once {
                let failed = txwatch_poller::run_once_with_broadcast(
                    cfg,
                    dry_run,
                    broadcast_tx.clone(),
                )
                .await?;
                if failed > 0 {
                    return Err(anyhow::anyhow!("{} contract poll(s) failed", failed));
                }
                info!("poll-once cycle completed for all contracts");
                return Ok(());
            }

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            // Handles both Ctrl+C (SIGINT) and SIGTERM (e.g. from Docker/systemd).
            let (shutdown_tx, shutdown_rx) = watch::channel(false);

            // Hot-reload: watch the config file and swap the active config on change.
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
            info!(
                version        = VERSION,
                contracts      = cfg.contracts.len(),
                interval_secs  = cfg.poll_interva

/* … truncated 1554 chars — edit only what you need near the top … */
