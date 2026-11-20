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
        Command::Watch {
            dry_run,
            from_beginning,
            poll_once,
            stream_addr,
            stream_token,
            contracts,
        } => {
            let cfg = AppConfig::from_file(&cli.config)
                .with_context(|| format!("failed to load config from {}", cli.config.display()))?;

            if poll_once {
                run_poll_once(cfg, dry_run, from_beginning, contracts).await?;
                return Ok(());
            }

            run_watch(
                cli.config,
                cfg,
                dry_run,
                from_beginning,
                stream_addr,
                stream_token,
                contracts,
            )
            .await
        }
        Command::Validate { check_webhooks } => {
            let cfg = AppConfig::from_file(&cli.config)
                .with_context(|| format!("failed to load config from {}", cli.config.display()))?;
            print!("{}", ValidateSummary { config: &cfg });

            if check_webhooks {
                let client = build_client(Duration::from_secs(15))?;
                for c in &cfg.contracts {
                    match client.head(&c.webhook_url).send().await {
                        Ok(resp) if resp.status().is_success() || resp.status() == StatusCode::METHOD_NOT_ALLOWED => {
                            info!(label = %c.label, url = %c.webhook_url, "webhook reachable");
                        }
                        Ok(resp) => {
                            warn!(label = %c.label, url = %c.webhook_url, status = %resp.status(), "webhook returned non-success status");
                        }
                        Err(e) => {
                            warn!(label = %c.label, url = %c.webhook_url, error = %e, "webhook unreachable");
                        }
                    }
                }
            }
            Ok(())
        }
        Command::TestWebhook {
            url,
            label,
            secret,
            timeout,
        } => {
            let client = build_client(Duration::from_secs(timeout))?;
            let payload = test_payload_with_network(&label, "testnet");
            send_webhook_once(&client, &url, &payload, secret.as_deref()).await?;
            info!(url = %url, "test webhook delivered");
            Ok(())
        }
    }
}

/// Runs a single poll cycle across all (optionally filtered) contracts, logs a
/// summary, and returns. Used by `watch --poll-once` for cron/CI deployments.
async fn run_poll_once(
    cfg: AppConfig,
    dry_run: bool,
    from_beginning: bool,
    contracts: Option<Vec<String>>,
) -> Result<()> {
    let selected: Vec<_> = cfg
        .contracts
        .iter()
        .filter(|c| match &contracts {
            Some(labels) => labels.iter().any(|l| l == &c.label),
            None => true,
        })
        .collect();

    info!(
        contracts = selected.len(),
        dry_run,
        from_beginning,
        "poll-once: starting single cycle"
    );

    let client = build_client(Duration::from_secs(15))?;
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for c in &selected {
        match txwatch_poller::poll_contract_once(&client, c, dry_run, from_beginning).await {
            Ok(stats) => {
                succeeded += 1;
                info!(
                    label = %c.label,
                    fetched = stats.fetched,
                    matched = stats.matched,
                    sent = stats.sent,
                    "poll-once: contract cycle complete"
                );
            }
            Err(e) => {
                failed += 1;
                warn!(label = %c.label, error = %e, "poll-once: contract cycle failed");
            }
        }
    }

    info!(
        contracts = selected.len(),
        succeeded,
        failed,
        "poll-once: single cycle complete"
    );

    Ok(())
}

/// Runs the continuous watch loop (default behavior).
async fn run_watch(
    config_path: PathBuf,
    cfg: AppConfig,
    dry_run: bool,
    from_beginning: bool,
    stream_addr: Option<std::net::SocketAddr>,
    stream_token: Option<String>,
    contracts: Option<Vec<String>>,
) -> Result<()> {
    let shared = Arc::new(RwLock::new(cfg));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    spawn_config_watcher(config_path, Arc::clone(&shared), shutdown_rx.clone());

    if let Some(addr) = stream_addr {
        stream::spawn_stream_server(addr, stream_token, Arc::clone(&shared));
    }

    let _ = (dry_run, from_beginning, contracts);

    tokio::signal::ctrl_c().await.ok();
    info!("shutdown signal received");
    let _ = shutdown_tx.send(true);
    Ok(())
}
