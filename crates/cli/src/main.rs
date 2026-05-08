use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use txwatch_config::AppConfig;

#[derive(Parser)]
#[command(name = "txwatch", about = "Stellar Soroban contract monitor & webhook alert engine")]
struct Cli {
    /// Path to the TOML config file
    #[arg(short, long, default_value = "config/example.toml")]
    config: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the polling engine
    Start,
    /// Validate the config file and exit
    Validate,
    /// List all contracts defined in the config
    ListContracts,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = AppConfig::from_file(&cli.config)?;

    match cli.command {
        Command::Validate => {
            println!("Config is valid. {} contract(s) loaded.", cfg.contracts.len());
        }

        Command::ListContracts => {
            println!("{:<30} {:<60} {}", "Label", "Contract ID", "Network");
            println!("{}", "-".repeat(100));
            for c in &cfg.contracts {
                println!("{:<30} {:<60} {}", c.label, c.contract_id, c.network.as_str());
            }
        }

        Command::Start => {
            println!(
                "Starting TxWatch — {} contract(s), polling every {}s",
                cfg.contracts.len(),
                cfg.poll_interval_seconds
            );
            txwatch_poller::run(cfg).await?;
        }
    }

    Ok(())
}
