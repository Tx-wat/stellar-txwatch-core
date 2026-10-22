use std::{fmt, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use tokio::sync::watch;
use tracing::{info, warn};
use txwatch_config::AppConfig;
use txwatch_notifier::{build_client, send_webhook, test_payload_with_network};

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
        }
        Ok(())
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
                    let reachable = check_webhook_reachable(&client, &c.webhook_url).await;
                    if let Err(e) = reachable {
                        warn!(webhook_url = %c.webhook_url, contract = %c.label, error = %e, "webhook reachability check failed");
                    } else if !reachable.unwrap() {
                        warn!(webhook_url = %c.webhook_url, contract = %c.label, "webhook endpoint is unreachable");
                    }
                }
            }
        }

        Command::TestWebhook { url, label, secret } => {
            let cfg = AppConfig::from_file(&cli.config)?;
            if cfg.contracts.is_empty() {
                return Err(anyhow::anyhow!(
                    "config has no contracts; cannot derive network for test-webhook"
                ));
            }
            let first_contract = &cfg.contracts[0];
            let horizon_base_url = first_contract.network.horizon_base_url();
            let payload = test_payload_with_network(&label, &url, &first_contract.network, horizon_base_url);
            let client  = build_client().context("failed to build HTTP client")?;

            info!(url = %url, "sending test webhook");
            send_webhook(&client, &url, &payload, secret.as_deref())
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            let signing = if secret.is_some() { "signed with HMAC-SHA256" } else { "unsigned" };
            println!("Test webhook delivered successfully to {} ({})", url, signing);
        }

        Command::Watch { dry_run, from_beginning } => {
            let cfg = AppConfig::from_file(&cli.config)?;

            if poll_once {
                let failed = txwatch_poller::run_once(cfg, dry_run).await?;
                if failed > 0 {
                    return Err(anyhow::anyhow!("{} contract poll(s) failed", failed));
                }
                info!("poll-once cycle completed for all contracts");
                return Ok(());
            }

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            // Handles both Ctrl+C (SIGINT) and SIGTERM (e.g. from Docker/systemd).
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
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

            info!(
                version        = VERSION,
                contracts      = cfg.contracts.len(),
                interval_secs  = cfg.poll_interva

/* … truncated 3436 chars — edit only what you need near the top … */
