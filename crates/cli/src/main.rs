use std::{fmt, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use tokio::sync::watch;
use tracing::{info, warn};
use txwatch_config::AppConfig;
use txwatch_notifier::{build_client, send_webhook, test_payload_with_network};

mod stream;

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
        }
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
            let client = build_client(Duration::from_secs(timeout))
                .context("failed to build HTTP client")?;

            info!(url = %url, timeout_secs = timeout, "sending test webhook");
            send_webhook(&client, &url, &payload, secret.as_deref())
                .await
                .with_context(|| format!("test webhook to '{}' failed", url))?;
            let signing = if secret.is_some() { "signed with HMAC-SHA256" } else { "unsigned" };
            println!("Test webhook delivered successfully to {} ({})", url, signing);
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

            let (broadcast_tx, broadcast_rx) = tokio::sync::broadcast::channel(128);


/* … truncated 2684 chars — edit only what you need near the top … */
