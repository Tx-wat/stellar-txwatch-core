use anyhow::{bail, Result};
use serde::Deserialize;
use std::{fs, path::Path};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Futurenet,
}

impl Network {
    pub fn horizon_base_url(&self) -> &'static str {
        match self {
            Network::Mainnet  => "https://horizon.stellar.org",
            Network::Testnet  => "https://horizon-testnet.stellar.org",
            Network::Futurenet => "https://horizon-futurenet.stellar.org",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Network::Mainnet   => "mainnet",
            Network::Testnet   => "testnet",
            Network::Futurenet => "futurenet",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AlertRule {
    LargeTransfer        { threshold_xlm: u64 },
    AdminFunctionCalled  { function_names: Vec<String> },
    AnyTransaction,
    FunctionCalled       { function_name: String },
    TransactionFailed,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchedContract {
    pub label:       String,
    pub contract_id: String,
    pub network:     Network,
    pub rules:       Vec<AlertRule>,
    pub webhook_url: String,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub contracts: Vec<WatchedContract>,
}

impl AppConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)?;
        let cfg: AppConfig = toml::from_str(&raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.poll_interval_seconds == 0 {
            bail!("poll_interval_seconds must be > 0");
        }
        for c in &self.contracts {
            if c.contract_id.is_empty() {
                bail!("contract '{}' has an empty contract_id", c.label);
            }
            if c.webhook_url.is_empty() {
                bail!("contract '{}' has an empty webhook_url", c.label);
            }
            if c.rules.is_empty() {
                bail!("contract '{}' has no rules defined", c.label);
            }
        }
        Ok(())
    }
}
