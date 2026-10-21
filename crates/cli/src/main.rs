use std::{path::PathBuf, time::Duration};

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
    },
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
            println!("Config is valid.");
            println!("  poll_interval_seconds : {}", cfg.poll_interval_seconds);
            println!("  contracts             : {}", cfg.contracts.len());
            println!();
            for c in &cfg.contracts {
                println!(
                    "  [{network}] {label}",
                    network = c.network.display_name(),
                    label = c.label
                );
                println!("    contract_id  : {}", c.contract_id);
                println!("    webhook_url  : {}", c.webhook_url);
                println!(
                    "    secret       : {}",
                    if c.webhook_secret.is_some() {
                        "set"
                    } else {
                        "none"
                    }
                );
                println!("    rules        : {}", c.rules.len());
                for rule in &c.rules {
                    println!("      - {}", rule.label());
                }
                println!("    horizon      : {}", c.network.horizon_base_url());
                println!(
                    "    explorer     : {}/contract/{}",
                    c.network.explorer_base_url(),
                    c.contract_id
                );
            }

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

        Command::TestWebhook { url, label } => {
            let cfg = AppConfig::from_file(&cli.config)?;
            if cfg.contracts.is_empty() {
                return Err(anyhow::anyhow!(
                    "config has no contracts; cannot derive network for test-webhook"
                ));
            }
            let first_contract = &cfg.contracts[0];
            let network_name = first_contract.network.as_str();
            let horizon_base_url = first_contract.network.horizon_base_url();
            let payload = test_payload_with_network(&label, &url, network_name, horizon_base_url);
            let client  = build_client().context("failed to build HTTP client")?;

            info!(url = %url, "sending test webhook");
            send_webhook(&client, &url, &payload, None)
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            println!("Test webhook delivered successfully to {}", url);
        }

        Command::Watch { dry_run } => {
            let cfg = AppConfig::from_file(&cli.config)?;

            // Graceful shutdown: allow the current poll cycle to finish before exiting.
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            tokio::spawn(async move {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    warn!(error = ?e, "failed to install Ctrl+C handler");
                    return;
                }
                let _ = shutdown_tx.send(true);
            });

            info!(
                version        = VERSION,
                contracts      = cfg.contracts.len(),
                interval_secs  = cfg.poll_interval_seconds,
                dry_run        = dry_run,
                "starting TxWatch"
            );
            txwatch_poller::run_with_shutdown(cfg, dry_run, shutdown_rx).await?;
        }
    }

    Ok(())
}
async fn check_webhook_reachable(client: &Client, url: &str) -> Result<bool> {
    let response = client.head(url).send().await;
    match response {
        Ok(resp) if resp.status().is_success() => return Ok(true),
        Ok(resp) if resp.status() == StatusCode::METHOD_NOT_ALLOWED || resp.status() == StatusCode::NOT_IMPLEMENTED => {
            let resp = client.request(reqwest::Method::OPTIONS, url).send().await?;
            return Ok(resp.status().is_success());
        }
        Ok(_) => return Ok(false),
        Err(err) => {
            if err.is_builder() {
                return Err(err.into());
            }
            return Ok(false);
        }
    }
}
// ── Tracing initialisation ────────────────────────────────────────────────────

#[cfg(not(feature = "opentelemetry"))]
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
}

/// Installs the log subscriber, plus an OTLP span exporter when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set. The returned provider must stay alive
/// for the whole run so that buffered spans are flushed on shutdown.
#[cfg(feature = "opentelemetry")]
fn init_tracing() -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    use opentelemetry::trace::TracerProvider as _;
    use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let endpoint = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
    let provider = otel_provider(endpoint.as_deref());
    let otel_layer = provider
        .as_ref()
        .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("txwatch")));

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .with(otel_layer)
        .init();
    provider
}

/// Builds a batch OTLP span exporter. Export is opt-in: with no endpoint,
/// no exporter is created and no spans leave the process.
#[cfg(feature = "opentelemetry")]
fn otel_provider(endpoint: Option<&str>) -> Option<opentelemetry_sdk::trace::SdkTracerProvider> {
    // The exporter reads OTEL_EXPORTER_OTLP_ENDPOINT itself; `endpoint` only gates it.
    endpoint?;
    match opentelemetry_otlp::SpanExporter::builder().with_tonic().build() {
        Ok(exporter) => Some(
            opentelemetry_sdk::trace::SdkTracerProvider::builder()
                .with_batch_exporter(exporter)
                .build(),
        ),
        Err(e) => {
            eprintln!("failed to build OTLP span exporter, trace export disabled: {e}");
            None
        }
    }
}

#[cfg(all(test, feature = "opentelemetry"))]
mod otel_tests {
    use super::otel_provider;

    #[test]
    fn no_exporter_is_built_without_an_endpoint() {
        assert!(otel_provider(None).is_none());
    }

    #[tokio::test]
    async fn exporter_is_built_when_an_endpoint_is_configured() {
        assert!(otel_provider(Some("http://127.0.0.1:4317")).is_some());
    }
}
