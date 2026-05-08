use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{fs, path::Path};

// ── Network ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Futurenet,
}

impl Network {
    pub fn horizon_base_url(&self) -> &'static str {
        match self {
            Network::Mainnet   => "https://horizon.stellar.org",
            Network::Testnet   => "https://horizon-testnet.stellar.org",
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

    /// Human-readable display name shown in logs and CLI output.
    pub fn display_name(&self) -> &'static str {
        match self {
            Network::Mainnet   => "Stellar Mainnet",
            Network::Testnet   => "Stellar Testnet",
            Network::Futurenet => "Stellar Futurenet",
        }
    }

    /// Stellar Expert explorer base URL for this network.
    pub fn explorer_base_url(&self) -> &'static str {
        match self {
            Network::Mainnet   => "https://stellar.expert/explorer/public",
            Network::Testnet   => "https://stellar.expert/explorer/testnet",
            Network::Futurenet => "https://stellar.expert/explorer/futurenet",
        }
    }
}

// ── AlertRule ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AlertRule {
    AnyTransaction,
    TransactionFailed,
    LargeTransfer       { threshold_xlm: u64 },
    FunctionCalled      { function_name: String },
    AdminFunctionCalled { function_names: Vec<String> },
    /// Fires when the transaction's fee (in stroops) exceeds the threshold.
    HighFee             { threshold_stroops: u64 },
}

impl AlertRule {
    pub fn validate(&self, contract_label: &str) -> Result<()> {
        match self {
            AlertRule::LargeTransfer { threshold_xlm } => {
                if *threshold_xlm == 0 {
                    bail!(
                        "contract '{}': LargeTransfer threshold_xlm must be > 0",
                        contract_label
                    );
                }
            }
            AlertRule::FunctionCalled { function_name } => {
                if function_name.trim().is_empty() {
                    bail!(
                        "contract '{}': FunctionCalled function_name must not be empty",
                        contract_label
                    );
                }
            }
            AlertRule::AdminFunctionCalled { function_names } => {
                if function_names.is_empty() {
                    bail!(
                        "contract '{}': AdminFunctionCalled function_names must not be empty",
                        contract_label
                    );
                }
                for name in function_names {
                    if name.trim().is_empty() {
                        bail!(
                            "contract '{}': AdminFunctionCalled contains a blank function name",
                            contract_label
                        );
                    }
                }
            }
            AlertRule::AnyTransaction | AlertRule::TransactionFailed => {}
            AlertRule::HighFee { threshold_stroops } => {
                if *threshold_stroops == 0 {
                    bail!(
                        "contract '{}': HighFee threshold_stroops must be > 0",
                        contract_label
                    );
                }
            }
        }
        Ok(())
    }
}

// ── WatchedContract ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct WatchedContract {
    pub label:       String,
    pub contract_id: String,
    pub network:     Network,
    pub rules:       Vec<AlertRule>,
    pub webhook_url: String,
    /// Optional secret sent as X-TxWatch-Secret header on every webhook POST.
    pub webhook_secret: Option<String>,
}

impl WatchedContract {
    pub fn validate(&self) -> Result<()> {
        if self.label.trim().is_empty() {
            bail!("a contract has an empty label");
        }

        // Stellar contract addresses start with 'C' and are 56 chars (base32)
        if self.contract_id.len() != 56 || !self.contract_id.starts_with('C') {
            bail!(
                "contract '{}': contract_id '{}' is not a valid Stellar contract address \
                 (must start with 'C' and be 56 characters)",
                self.label,
                self.contract_id
            );
        }

        if !self.webhook_url.starts_with("http://") && !self.webhook_url.starts_with("https://") {
            bail!(
                "contract '{}': webhook_url '{}' must start with http:// or https://",
                self.label,
                self.webhook_url
            );
        }

        if self.rules.is_empty() {
            bail!("contract '{}': at least one rule is required", self.label);
        }

        for rule in &self.rules {
            rule.validate(&self.label)?;
        }

        Ok(())
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub contracts: Vec<WatchedContract>,
}

impl AppConfig {
    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{}'", path.display()))?;
        let cfg: AppConfig = toml::from_str(&raw)
            .with_context(|| format!("failed to parse config file '{}'", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if self.poll_interval_seconds == 0 {
            bail!("poll_interval_seconds must be > 0");
        }
        if self.poll_interval_seconds > 3600 {
            bail!("poll_interval_seconds must be <= 3600 (1 hour)");
        }
        if self.contracts.is_empty() {
            bail!("at least one [[contracts]] entry is required");
        }
        for contract in &self.contracts {
            contract.validate()?;
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_contract() -> WatchedContract {
        WatchedContract {
            label:          "Test".into(),
            contract_id:    "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:        Network::Testnet,
            rules:          vec![AlertRule::AnyTransaction],
            webhook_url:    "https://example.com/hook".into(),
            webhook_secret: None,
        }
    }

    #[test]
    fn valid_config_passes() {
        let c = valid_contract();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_short_contract_id() {
        let mut c = valid_contract();
        c.contract_id = "CSHORT".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_non_c_contract_id() {
        let mut c = valid_contract();
        c.contract_id = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_bad_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = "ftp://bad".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_rules() {
        let mut c = valid_contract();
        c.rules = vec![];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_zero_threshold() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::LargeTransfer { threshold_xlm: 0 }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled { function_name: "  ".into() }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_admin_function_names() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled { function_names: vec![] }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn network_urls() {
        assert!(Network::Mainnet.horizon_base_url().contains("horizon.stellar.org"));
        assert!(Network::Testnet.horizon_base_url().contains("testnet"));
        assert!(Network::Futurenet.horizon_base_url().contains("futurenet"));
    }

    #[test]
    fn network_display_names() {
        assert_eq!(Network::Mainnet.display_name(), "Stellar Mainnet");
        assert_eq!(Network::Testnet.display_name(), "Stellar Testnet");
        assert_eq!(Network::Futurenet.display_name(), "Stellar Futurenet");
    }

    #[test]
    fn network_explorer_urls() {
        assert!(Network::Mainnet.explorer_base_url().contains("public"));
        assert!(Network::Testnet.explorer_base_url().contains("testnet"));
    }

    #[test]
    fn rejects_poll_interval_over_max() {
        let raw = r#"
            poll_interval_seconds = 9999
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert!(cfg.validate().is_err());
    }
}
