#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::{env, fmt, fs, path::Path};
use url::Url;

const MAX_LARGE_TRANSFER_THRESHOLD_XLM: u64 = 1_000_000_000;

// ── Network ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Mainnet,
    Testnet,
    Futurenet,
}

impl Network {
    pub fn horizon_base_url(&self) -> &'static str {
        match self {
            Network::Mainnet => "https://horizon.stellar.org",
            Network::Testnet => "https://horizon-testnet.stellar.org",
            Network::Futurenet => "https://horizon-futurenet.stellar.org",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Network::Mainnet => "mainnet",
            Network::Testnet => "testnet",
            Network::Futurenet => "futurenet",
        }
    }

    /// Human-readable display name shown in logs and CLI output.
    pub fn display_name(&self) -> &'static str {
        match self {
            Network::Mainnet => "Stellar Mainnet",
            Network::Testnet => "Stellar Testnet",
            Network::Futurenet => "Stellar Futurenet",
        }
    }

    /// Stellar Expert explorer base URL for this network.
    pub fn explorer_base_url(&self) -> &'static str {
        match self {
            Network::Mainnet => "https://stellar.expert/explorer/public",
            Network::Testnet => "https://stellar.expert/explorer/testnet",
            Network::Futurenet => "https://stellar.expert/explorer/futurenet",
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── AlertRule ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum AlertRule {
    AnyTransaction,
    TransactionFailed,
    LargeTransfer       { threshold_xlm: u64 },
    FunctionCalled      { function_name: String },
    AdminFunctionCalled { function_names: Vec<String> },
    /// Fires when the transaction's fee exceeds the threshold.
    /// Specify either `threshold_stroops` (raw stroops) or `threshold_xlm` (whole XLM,
    /// converted to stroops during validation); the two are mutually exclusive.
    HighFee {
        #[serde(default)]
        threshold_stroops: u64,
        #[serde(default)]
        threshold_xlm: Option<u64>,
    },
}

impl AlertRule {
    pub fn validate(&mut self, contract_label: &str) -> Result<()> {
        match self {
            AlertRule::LargeTransfer { threshold_xlm } => {
                if *threshold_xlm == 0 {
                    bail!(
                        "contract '{}': LargeTransfer threshold_xlm must be > 0",
                        contract_label
                    );
                }
                if *threshold_xlm > MAX_LARGE_TRANSFER_THRESHOLD_XLM {
                    bail!(
                        "contract '{}': LargeTransfer threshold_xlm must be <= {}",
                        contract_label,
                        MAX_LARGE_TRANSFER_THRESHOLD_XLM
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
                for name in function_names.iter_mut() {
                    if name.trim().is_empty() {
                        bail!(
                            "contract '{}': AdminFunctionCalled contains a blank function name",
                            contract_label
                        );
                    }
                    *name = name.to_lowercase();
                }
            }
            AlertRule::AnyTransaction | AlertRule::TransactionFailed => {}
            AlertRule::HighFee { threshold_stroops, threshold_xlm } => {
                match (*threshold_xlm, *threshold_stroops) {
                    (Some(_), s) if s > 0 => bail!(
                        "contract '{}': HighFee: specify either threshold_stroops or \
                         threshold_xlm, not both",
                        contract_label
                    ),
                    (None, 0) => bail!(
                        "contract '{}': HighFee threshold_stroops must be > 0",
                        contract_label
                    ),
                    (Some(0), _) => bail!(
                        "contract '{}': HighFee threshold_xlm must be > 0",
                        contract_label
                    ),
                    (Some(xlm), 0) => {
                        *threshold_stroops = xlm.checked_mul(10_000_000).with_context(|| {
                            format!(
                                "contract '{}': HighFee threshold_xlm overflow",
                                contract_label
                            )
                        })?;
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
    pub fn label(&self) -> String {
        match self {
            AlertRule::AnyTransaction => "AnyTransaction".into(),
            AlertRule::TransactionFailed => "TransactionFailed".into(),
            AlertRule::LargeTransfer { threshold_xlm } => {
                format!("LargeTransfer(>={}XLM)", threshold_xlm)
            }
            AlertRule::FunctionCalled { function_name } => {
                format!("FunctionCalled({})", function_name)
            }
            AlertRule::AdminFunctionCalled { function_names } => {
                format!("AdminFunctionCalled([{}])", function_names.join(", "))
            }
            AlertRule::HighFee { threshold_stroops, threshold_xlm } => {
                if let Some(xlm) = threshold_xlm {
                    format!("HighFee(>={} XLM)", xlm)
                } else {
                    format!("HighFee(>={} stroops)", threshold_stroops)
                }
            }
        }
    }}

// ── WatchedContract ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchedContract {
    pub label: String,
    pub contract_id: String,
    pub network: Network,
    pub rules: Vec<AlertRule>,
    pub webhook_url: String,
    /// Optional secret sent as X-TxWatch-Secret header on every webhook POST.
    /// Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`).
    pub webhook_secret: Option<String>,
    /// Override the Horizon base URL; never read from TOML — set programmatically in tests.
    #[serde(skip, default)]
    pub horizon_base_url_override: Option<String>,
}

impl WatchedContract {
    pub fn validate(&mut self) -> Result<()> {
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

        let parsed_url = Url::parse(&self.webhook_url).map_err(|e| {
            anyhow::anyhow!(
                "contract '{}': webhook_url '{}' is not a valid URL: {}",
                self.label,
                self.webhook_url,
                e
            )
        })?;
        if parsed_url.scheme() != "http" && parsed_url.scheme() != "https" {
            bail!(
                "contract '{}': webhook_url '{}' must use http or https scheme",
                self.label,
                self.webhook_url
            );
        }
        if parsed_url.host().is_none() {
            bail!(
                "contract '{}': webhook_url '{}' has no host",
                self.label,
                self.webhook_url
            );
        }

        if self.rules.is_empty() {
            bail!("contract '{}': at least one rule is required", self.label);
        }

        let label = self.label.clone();
        for rule in &mut self.rules {
            rule.validate(&label)?;
        }

        Ok(())
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

/// Maximum number of watched contracts allowed in a single configuration.
/// Exceeding this limit would create too many concurrent Horizon polling tasks,
/// potentially exhausting memory or file descriptors.
pub const MAX_CONTRACTS: usize = 100;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub poll_interval_seconds: u64,
    pub contracts: Vec<WatchedContract>,
    /// Optional path to a JSON file used to persist the cursor map across restarts.
    /// When set, the poller will load cursors from this file on startup and write
    /// the updated cursor map after each poll cycle. If absent, cursors default
    /// to the Horizon keyword `now` and are not persisted.
    #[serde(default)]
    pub cursor_file: Option<String>,
    /// Maximum number of idle connections per host in the HTTP connection pool.
    /// Lower values reduce memory usage; higher values improve throughput for many contracts.
    /// Default: 10.
    #[serde(default = "default_http_pool_max_idle_per_host")]
    pub http_pool_max_idle_per_host: Option<usize>,
    /// TCP keepalive interval in seconds for idle HTTP connections.
    /// Helps detect stalled connections quickly; 0 disables keepalive.
    /// Default: 30 seconds.
    #[serde(default = "default_http_tcp_keepalive_secs")]
    pub http_tcp_keepalive_secs: Option<u64>,
    /// Enable verbose output for HTTP connection pool debug information.
    /// Only useful for troubleshooting connection issues.
    /// Default: false.
    #[serde(default)]
    pub http_connection_verbose: Option<bool>,
}

fn default_http_pool_max_idle_per_host() -> Option<usize> {
    None
}

fn default_http_tcp_keepalive_secs() -> Option<u64> {
    None
}

fn deserialize_toml_with_field_context<'de, T>(raw: &'de str, path: &Path) -> Result<T>
where
    T: DeserializeOwned,
{
    let deserializer = toml::Deserializer::new(raw);
    serde_path_to_error::deserialize(deserializer).map_err(|error| {
        let path = error.path().to_string();
        let inner = error.into_inner();
        let message = if path.is_empty() {
            inner.to_string()
        } else {
            format!("{} (field: {})", inner, path)
        };
        anyhow!(message)
    })
}

// ── Env-var interpolation ─────────────────────────────────────────────────────

/// Resolves a `${VAR_NAME}` reference to the corresponding environment variable.
/// Values that don't match the `${...}` pattern are returned unchanged.
fn resolve_env_interpolation(value: &str) -> Result<String> {
    match value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        Some(var_name) => env::var(var_name)
            .with_context(|| format!("env var '{}' referenced in config is not set", var_name)),
        None => Ok(value.to_owned()),
    }
}

impl AppConfig {
    fn resolve_env_vars(&mut self) -> Result<()> {
        for contract in &mut self.contracts {
            if let Some(secret) = &contract.webhook_secret {
                contract.webhook_secret = Some(resolve_env_interpolation(secret)?);
            }
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{}'", path.display()))?;
        let mut cfg: AppConfig = deserialize_toml_with_field_context(&raw, path)
            .with_context(|| format!("failed to parse config file '{}'", path.display()))?;
        cfg.resolve_env_vars()?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&mut self) -> Result<()> {
        if self.poll_interval_seconds < 5 {
            bail!("poll_interval_seconds must be >= 5");
        }
        if self.poll_interval_seconds > 3600 {
            bail!("poll_interval_seconds must be <= 3600 (1 hour)");
        }
        if self.contracts.is_empty() {
            bail!("at least one [[contracts]] entry is required");
        }
        for contract in &mut self.contracts {
            contract.validate()?;
        }
        let mut seen = std::collections::HashSet::new();
        for contract in &self.contracts {
            if !seen.insert(&contract.label) {
                bail!("duplicate contract label '{}'", contract.label);
            }
        }
        Ok(())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn valid_contract() -> WatchedContract {
        WatchedContract {
            label: "Test".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://example.com/hook".into(),
            webhook_secret: None,
            horizon_base_url_override: None,
        }
    }

    #[test]
    fn valid_config_passes() {
        let mut c = valid_contract();
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
    fn rejects_webhook_url_with_no_host() {
        let mut c = valid_contract();
        c.webhook_url = "https://".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_spaces() {
        let mut c = valid_contract();
        c.webhook_url = "https://example .com/hook".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_that_is_not_a_url() {
        let mut c = valid_contract();
        c.webhook_url = "not-a-url-at-all".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_ftp_scheme() {
        let mut c = valid_contract();
        c.webhook_url = "ftp://files.example.com/hook".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_valid_http_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = "http://hooks.example.com/my-webhook".into();
        assert!(c.validate().is_ok());
    }

    #[test]
    fn accepts_valid_https_webhook_url_with_path_and_query() {
        let mut c = valid_contract();
        c.webhook_url = "https://hooks.example.com/alerts?token=abc123".into();
        assert!(c.validate().is_ok());
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
    fn rejects_too_large_large_transfer_threshold() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::LargeTransfer {
            threshold_xlm: MAX_LARGE_TRANSFER_THRESHOLD_XLM + 1,
        }];
        let err = c.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("LargeTransfer threshold_xlm must be <="));
    }

    #[test]
    fn rejects_empty_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::FunctionCalled {
            function_name: "  ".into(),
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_admin_function_names() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec![],
        }];
        assert!(c.validate().is_err());
    }

    /// Issue #18: blank entry in function_names should fail validation.
    #[test]
    fn rejects_blank_entry_in_admin_function_names() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into(), " ".into()],
        }];
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("blank"), "expected 'blank' in error, got: {}", err);
    }

    /// Issue #18: single valid entry in function_names should pass validation.
    #[test]
    fn accepts_single_valid_admin_function_name() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["set_admin".into()],
        }];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn admin_function_names_normalised_to_lowercase() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::AdminFunctionCalled {
            function_names: vec!["Set_Admin".into(), "UPGRADE".into()],
        }];
        c.validate().unwrap();
        if let AlertRule::AdminFunctionCalled { function_names } = &c.rules[0] {
            assert_eq!(function_names, &["set_admin", "upgrade"]);
        } else {
            panic!("expected AdminFunctionCalled");
        }
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
    fn rejects_duplicate_labels() {
        let c = valid_contract();
        let mut cfg = AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![c.clone(), c],
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
            cursor_file: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate contract label"));
    }

    #[test]
    fn appconfig_validate_rejects_empty_contracts() {
        let mut cfg = AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![],
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
            cursor_file: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(
            err.to_string().contains("at least one"),
            "error should mention 'at least one', got: {}",
            err
        );
    }

    #[test]
    fn high_fee_threshold_xlm_normalises_to_stroops() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee { threshold_stroops: 0, threshold_xlm: Some(1) }];
        c.validate().unwrap();
        if let AlertRule::HighFee { threshold_stroops, .. } = &c.rules[0] {
            assert_eq!(*threshold_stroops, 10_000_000, "1 XLM should become 10_000_000 stroops");
        } else {
            panic!("expected HighFee");
        }
    }

    #[test]
    fn high_fee_threshold_xlm_zero_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee { threshold_stroops: 0, threshold_xlm: Some(0) }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn high_fee_both_thresholds_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee { threshold_stroops: 100, threshold_xlm: Some(1) }];
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[test]
    fn high_fee_neither_threshold_is_rejected() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::HighFee { threshold_stroops: 0, threshold_xlm: None }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_poll_interval_too_low() {
        for val in 0u64..5 {
            let mut cfg = AppConfig {
                poll_interval_seconds: val,
                contracts: vec![valid_contract()],
                http_pool_max_idle_per_host: None,
                http_tcp_keepalive_secs: None,
                http_connection_verbose: None,
                cursor_file: None,
            };
            let err = cfg.validate().unwrap_err();
            assert!(
                err.to_string().contains("poll_interval_seconds must be >= 5"),
                "val={} should be rejected: {}", val, err
            );
        }
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
        let mut cfg: AppConfig = toml::from_str(raw).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn from_file_returns_err_for_missing_file() {
        let nonexistent_path = std::path::Path::new("/tmp/txwatch_nonexistent_test_config.toml");
        let result = AppConfig::from_file(nonexistent_path);
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("txwatch_nonexistent_test_config.toml"));
    }

    #[test]
    fn from_file_returns_err_for_wrong_type_field() {
        let path = std::env::temp_dir().join("txwatch_wrong_type_field_test_config.toml");
        let raw = r#"
            poll_interval_seconds = "ten"
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;

        std::fs::write(&path, raw).unwrap();
        let result = AppConfig::from_file(&path);
        let _ = std::fs::remove_file(&path);

        assert!(result.is_err());
        // `{:#}` renders the full context chain, including the field-path cause.
        let error_msg = format!("{:#}", result.unwrap_err());
        assert!(error_msg.contains("failed to parse config file"));
        assert!(error_msg.contains("field: poll_interval_seconds"));
    }
}
