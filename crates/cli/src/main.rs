use std::{fmt, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use tokio::sync::{watch, RwLock};
use tracing::{info, warn};
use txwatch_config::AppConfig;
use serde::Deserialize;
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
                            #[cfg(feature = "metrics")]
                            txwatch_poller::metrics::inc_config_reload_errors();
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
    disable_version_flag = true,
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
            writeln!(f)?;
        }
        Ok(())
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Validate { check_webhooks } => {
            let cfg = AppConfig::from_file(&cli.config)
                .with_context(|| format!("failed to load config from {}", cli.config.display()))?;
            print!("{}", ValidateSummary { config: &cfg });

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
                        Ok(true) => {
                            info!(webhook_url = %c.webhook_url, contract = %c.label, "webhook reachable");
                        }
                    }
                }
            }
            Ok(())
        }

        Command::TestWebhook { url, label, secret, timeout } => {
            let cfg = AppConfig::from_file(&cli.config)
                .with_context(|| format!("failed to load config from {}", cli.config.display()))?;
            let first_contract = cfg.contracts.first().ok_or_else(|| {
                anyhow::anyhow!("config has no contracts; cannot derive network for test-webhook")
            })?;
            let payload = test_payload_with_network(
                &label,
                &url,
                &first_contract.network,
                first_contract.network.horizon_base_url(),
            );

            // Prefer the explicit --secret flag, otherwise fall back to the
            // webhook_secret configured for the first contract.
            let secret = secret.or_else(|| {
                first_contract
                    .webhook_secret
                    .as_ref()
                    .map(|s| s.expose_secret().to_owned())
            });

            let client = Client::builder()
                .timeout(Duration::from_secs(timeout))
                .build()
                .context("failed to build HTTP client")?;

            info!(url = %url, timeout_secs = timeout, "sending test webhook");
            send_webhook_once(&client, &url, &payload, secret.as_deref())
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;

            match secret.as_deref() {
                Some(s) => {
                    let body = serde_json::to_vec(&payload)?;
                    let signature = txwatch_notifier::sign_payload(s, &body);
                    println!("X-TxWatch-Signature: {}", signature);
                    println!(
                        "Test webhook delivered successfully to {} (signed with HMAC-SHA256)",
                        url
                    );
                }
                None => {
                    println!("Test webhook delivered successfully to {} (unsigned)", url);
                }
            }
            Ok(())
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
            Ok(())
        }

        Command::Watch {
            dry_run,
            from_beginning,
            poll_once,
            stream_addr,
            stream_token,
            contracts,
        } => {
            let mut cfg = AppConfig::from_file(&cli.config)
                .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

            if let Some(labels) = &contracts {
                cfg.contracts.retain(|c| labels.iter().any(|l| l == &c.label));
            }

            if poll_once {
                let failed = txwatch_poller::run_once(cfg, dry_run).await?;
                info!(failed, "poll-once: single cycle complete");
                return Ok(());
            }

            run_watch(cli.config, cfg, dry_run, from_beginning, stream_addr, stream_token).await
        }
    }
}

/// Sends a HEAD request to `url` and reports whether the endpoint responded.
/// Any HTTP response (including 405 Method Not Allowed) counts as reachable;
/// only 5xx responses and transport errors are treated as unreachable.
async fn check_webhook_reachable(client: &Client, url: &str) -> Result<bool> {
    let resp = client.head(url).send().await?;
    Ok(!resp.status().is_server_error())
}

/// Runs the continuous watch loop (default behavior).
async fn run_watch(
    config_path: PathBuf,
    cfg: AppConfig,
    dry_run: bool,
    from_beginning: bool,
    stream_addr: Option<std::net::SocketAddr>,
    stream_token: Option<String>,
) -> Result<()> {
    // The hot-reload watcher keeps its own copy (AppConfig is not Clone).
    let shared = Arc::new(RwLock::new(AppConfig::from_file(&config_path)?));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    spawn_config_watcher(config_path, Arc::clone(&shared), shutdown_rx.clone());

    // Keep the sender alive for the lifetime of the watch loop.
    let (_alert_tx, alert_rx) = tokio::sync::broadcast::channel::<String>(256);
    if let Some(addr) = stream_addr {
        stream::run_sse_server(addr, stream_token, alert_rx).await?;
    }

    let poller = tokio::spawn(txwatch_poller::run_with_options(
        cfg,
        dry_run,
        from_beginning,
        shutdown_rx,
    ));

    tokio::signal::ctrl_c().await.ok();
    info!("shutdown signal received");
    let _ = shutdown_tx.send(true);
    poller.await.context("poller task panicked")?
}
