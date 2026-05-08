use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::Client;
use tracing::info;
use txwatch_config::AppConfig;
use txwatch_notifier::{send_webhook, test_payload};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name    = "txwatch",
    version = "0.1.0",
    about   = "Stellar Soroban contract monitor & webhook alert engine"
)]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config/example.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine (watches all contracts in the config)
    Watch,

    /// Parse and validate the config file, then print a summary
    Validate,

    /// Send a test webhook payload to a URL and exit
    TestWebhook {
        /// The webhook URL to POST to
        #[arg(long)]
        url: String,

        /// Label to include in the test payload
        #[arg(long, default_value = "TxWatch Test")]
        label: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let cli = Cli::parse();

    match cli.command {
        Command::Validate => {
            let cfg = AppConfig::from_file(&cli.config)?;
            println!("Config is valid.");
            println!("  poll_interval_seconds : {}", cfg.poll_interval_seconds);
            println!("  contracts             : {}", cfg.contracts.len());
            println!();
            for c in &cfg.contracts {
                println!(
                    "  [{network}] {label}",
                    network = c.network.as_str(),
                    label   = c.label
                );
                println!("    contract_id : {}", c.contract_id);
                println!("    webhook_url : {}", c.webhook_url);
                println!("    rules       : {}", c.rules.len());
            }
        }

        Command::TestWebhook { url, label } => {
            let payload = test_payload(&label, &url);
            let client  = Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .context("failed to build HTTP client")?;

            info!(url = %url, "sending test webhook");
            send_webhook(&client, &url, &payload)
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            println!("Test webhook delivered successfully to {}", url);
        }

        Command::Watch => {
            let cfg = AppConfig::from_file(&cli.config)?;
            info!(
                contracts      = cfg.contracts.len(),
                interval_secs  = cfg.poll_interval_seconds,
                "starting TxWatch"
            );
            txwatch_poller::run(cfg).await?;
        }
    }

    Ok(())
}

// ── Tracing initialisation ────────────────────────────────────────────────────

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}
