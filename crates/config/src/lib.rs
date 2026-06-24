#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_path_to_error::Deserializer as PathDeserializer;
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

// ── RetryPolicy ───────────────────────────────────────────────────────────────

/// Webhook retry configuration. Used globally via `[webhook_retry]` and
/// optionally overridden per contract via `webhook_max_retries` / `webhook_backoff_base`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RetryPolicy {
    /// Maximum number of delivery attempts (1–10). Default: 3.
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Exponential backoff base in seconds (1–60). Default: 2.
    #[serde(default = "default_backoff_base")]
    pub backoff_base: u64,
}

fn default_max_retries() -> u32 { 3 }
fn default_backoff_base() -> u64 { 2 }

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_retries: default_max_retries(), backoff_base: default_backoff_base() }
    }
}

impl RetryPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.max_retries < 1 || self.max_retries > 10 {
            bail!("webhook_retry.max_retries must be between 1 and 10");
        }
        if self.backoff_base < 1 || self.backoff_base > 60 {
            bail!("webhook_retry.backoff_base must be between 1 and 60");
        }
        Ok(())
    }
}

// ── WatchedContract ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchedContract {
    pub label: String,
    pub contract_id: String,
    pub network: Network,
    pub rules: Vec<AlertRule>,
    /// Single webhook URL (legacy). Use `webhook_urls` for multiple endpoints.
    /// Supports `${ENV_VAR}` interpolation.
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Multiple webhook URLs for fan-out delivery. Takes precedence over `webhook_url`.
    /// Each URL supports `${ENV_VAR}` interpolation.
    #[serde(default)]
    pub webhook_urls: Vec<String>,
    /// Optional secret sent as X-TxWatch-Signature header on every webhook POST.
    /// Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`).
    pub webhook_secret: Option<String>,
    /// Whether this contract is actively polled. Default: true.
    /// When false the contract is validated but skipped by the poller.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Per-contract override for maximum webhook retries (1–10).
    #[serde(default)]
    pub webhook_max_retries: Option<u32>,
    /// Override the Horizon base URL; never read from TOML — set programmatically in tests.
    #[serde(skip, default)]
    pub horizon_base_url_override: Option<String>,
}

fn default_enabled() -> bool { true }

impl WatchedContract {
    /// Returns the resolved list of webhook URLs (from `webhook_urls` or `webhook_url`).
    pub fn resolved_webhook_urls(&self) -> &[String] {
        if !self.webhook_urls.is_empty() {
            &self.webhook_urls
        } else if let Some(u) = &self.webhook_url {
            std::slice::from_ref(u)
        } else {
            &[]
        }
    }

    /// Returns the effective retry policy, merging per-contract override with global policy.
    pub fn effective_retry_policy(&self, global: &RetryPolicy) -> RetryPolicy {
        RetryPolicy {
            max_retries: self.webhook_max_retries.unwrap_or(global.max_retries),
            backoff_base: global.backoff_base,
        }
    }
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

        // Validate all webhook URLs
        let urls: Vec<String> = self.resolved_webhook_urls().to_vec();
        if urls.is_empty() {
            bail!("contract '{}': at least one webhook URL is required (webhook_url or webhook_urls)", self.label);
        }
        for url in &urls {
            validate_webhook_url(&self.label, url)?;
        }

        if let Some(r) = self.webhook_max_retries {
            if r < 1 || r > 10 {
                bail!("contract '{}': webhook_max_retries must be between 1 and 10", self.label);
            }
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

fn validate_webhook_url(label: &str, url: &str) -> Result<()> {
    let parsed = Url::parse(url).map_err(|e| {
        anyhow::anyhow!("contract '{}': webhook_url '{}' is not a valid URL: {}", label, url, e)
    })?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        bail!("contract '{}': webhook_url '{}' must use http or https scheme", label, url);
    }
    if parsed.host().is_none() {
        bail!("contract '{}': webhook_url '{}' has no host", label, url);
    }
    Ok(())
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
    #[serde(default)]
    pub cursor_file: Option<String>,
    /// Maximum number of idle connections per host in the HTTP connection pool.
    #[serde(default = "default_http_pool_max_idle_per_host")]
    pub http_pool_max_idle_per_host: Option<usize>,
    /// TCP keepalive interval in seconds for idle HTTP connections.
    #[serde(default = "default_http_tcp_keepalive_secs")]
    pub http_tcp_keepalive_secs: Option<u64>,
    /// Enable verbose output for HTTP connection pool debug information.
    #[serde(default)]
    pub http_connection_verbose: Option<bool>,
    /// Global webhook retry policy. Defaults to 3 retries with 2s backoff base.
    #[serde(default)]
    pub webhook_retry: RetryPolicy,
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
    let mut deserializer = toml::Deserializer::new(raw);
    let mut path_deserializer = PathDeserializer::new(&mut deserializer);
    T::deserialize(&mut path_deserializer).map_err(|error| {
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
            if let Some(url) = &contract.webhook_url {
                let resolved = resolve_env_interpolation(url)?;
                contract.webhook_url = Some(resolved);
            }
            let mut resolved_urls = Vec::with_capacity(contract.webhook_urls.len());
            for url in &contract.webhook_urls {
                resolved_urls.push(resolve_env_interpolation(url)?);
            }
            contract.webhook_urls = resolved_urls;
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
        self.webhook_retry.validate()?;
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
            webhook_url: Some("https://example.com/hook".into()),
            webhook_urls: vec![],
            webhook_secret: None,
            enabled: true,
            webhook_max_retries: None,
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
        c.webhook_url = Some("ftp://bad".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_no_host() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_spaces() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://example .com/hook".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_that_is_not_a_url() {
        let mut c = valid_contract();
        c.webhook_url = Some("not-a-url-at-all".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_webhook_url_with_ftp_scheme() {
        let mut c = valid_contract();
        c.webhook_url = Some("ftp://files.example.com/hook".into());
        assert!(c.validate().is_err());
    }

    #[test]
    fn accepts_valid_http_webhook_url() {
        let mut c = valid_contract();
        c.webhook_url = Some("http://hooks.example.com/my-webhook".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn accepts_valid_https_webhook_url_with_path_and_query() {
        let mut c = valid_contract();
        c.webhook_url = Some("https://hooks.example.com/alerts?token=abc123".into());
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
            webhook_retry: RetryPolicy::default(),
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
            webhook_retry: RetryPolicy::default(),
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
                webhook_retry: RetryPolicy::default(),
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
        let error_msg = result.unwrap_err().to_string();
        assert!(error_msg.contains("failed to parse config file"));
        assert!(error_msg.contains("field: poll_interval_seconds"));
    }

    // ── Issue #207: webhook_url env var interpolation ──────────────────────────

    #[test]
    fn webhook_url_env_var_is_resolved_and_validated() {
        std::env::set_var("TEST_HOOK_URL_207", "https://hooks.example.com/resolved");
        let mut c = valid_contract();
        c.webhook_url = Some("${TEST_HOOK_URL_207}".into());
        // resolve_env_interpolation is called in AppConfig::resolve_env_vars;
        // simulate it directly here.
        let resolved = resolve_env_interpolation("${TEST_HOOK_URL_207}").unwrap();
        assert_eq!(resolved, "https://hooks.example.com/resolved");
        c.webhook_url = Some(resolved);
        assert!(c.validate().is_ok());
        std::env::remove_var("TEST_HOOK_URL_207");
    }

    #[test]
    fn webhook_url_env_var_not_set_returns_error() {
        std::env::remove_var("TEST_MISSING_HOOK_URL_207");
        let result = resolve_env_interpolation("${TEST_MISSING_HOOK_URL_207}");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("TEST_MISSING_HOOK_URL_207"));
    }

    #[test]
    fn webhook_url_invalid_after_resolution_returns_error() {
        std::env::set_var("TEST_BAD_URL_207", "ftp://bad");
        let mut c = valid_contract();
        let resolved = resolve_env_interpolation("${TEST_BAD_URL_207}").unwrap();
        c.webhook_url = Some(resolved);
        assert!(c.validate().is_err());
        std::env::remove_var("TEST_BAD_URL_207");
    }

    // ── Issue #206: multiple webhook URLs ─────────────────────────────────────

    #[test]
    fn webhook_urls_plural_validates_all_entries() {
        let mut c = valid_contract();
        c.webhook_url = None;
        c.webhook_urls = vec![
            "https://slack.example.com/hook".into(),
            "https://pagerduty.example.com/hook".into(),
        ];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn webhook_urls_one_invalid_entry_rejects() {
        let mut c = valid_contract();
        c.webhook_url = None;
        c.webhook_urls = vec![
            "https://valid.example.com/hook".into(),
            "ftp://invalid".into(),
        ];
        assert!(c.validate().is_err());
    }

    #[test]
    fn webhook_urls_empty_and_no_single_url_rejects() {
        let mut c = valid_contract();
        c.webhook_url = None;
        c.webhook_urls = vec![];
        assert!(c.validate().is_err());
    }

    #[test]
    fn resolved_webhook_urls_prefers_plural_field() {
        let mut c = valid_contract();
        c.webhook_urls = vec!["https://a.example.com".into(), "https://b.example.com".into()];
        let urls = c.resolved_webhook_urls();
        assert_eq!(urls.len(), 2);
    }

    #[test]
    fn resolved_webhook_urls_falls_back_to_singular() {
        let c = valid_contract();
        let urls = c.resolved_webhook_urls();
        assert_eq!(urls, &["https://example.com/hook"]);
    }

    // ── Issue #208: enabled flag ──────────────────────────────────────────────

    #[test]
    fn disabled_contract_passes_validation() {
        let mut c = valid_contract();
        c.enabled = false;
        assert!(c.validate().is_ok());
    }

    #[test]
    fn enabled_defaults_to_true() {
        let raw = r#"
            poll_interval_seconds = 10
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert!(cfg.contracts[0].enabled);
    }

    // ── Issue #209: retry policy ───────────────────────────────────────────────

    #[test]
    fn retry_policy_defaults_are_correct() {
        let p = RetryPolicy::default();
        assert_eq!(p.max_retries, 3);
        assert_eq!(p.backoff_base, 2);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn retry_policy_rejects_out_of_range_max_retries() {
        let mut p = RetryPolicy::default();
        p.max_retries = 0;
        assert!(p.validate().is_err());
        p.max_retries = 11;
        assert!(p.validate().is_err());
    }

    #[test]
    fn retry_policy_rejects_out_of_range_backoff_base() {
        let mut p = RetryPolicy::default();
        p.backoff_base = 0;
        assert!(p.validate().is_err());
        p.backoff_base = 61;
        assert!(p.validate().is_err());
    }

    #[test]
    fn per_contract_retry_override_is_applied() {
        let mut c = valid_contract();
        c.webhook_max_retries = Some(5);
        let global = RetryPolicy::default();
        let effective = c.effective_retry_policy(&global);
        assert_eq!(effective.max_retries, 5);
        assert_eq!(effective.backoff_base, global.backoff_base);
    }

    #[test]
    fn per_contract_retry_override_rejects_out_of_range() {
        let mut c = valid_contract();
        c.webhook_max_retries = Some(11);
        assert!(c.validate().is_err());
    }
}
