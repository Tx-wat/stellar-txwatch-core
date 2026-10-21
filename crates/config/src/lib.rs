#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_path_to_error::Deserializer as PathDeserializer;
use std::{env, fmt, fs, path::Path};
use url::Url;

const MAX_LARGE_TRANSFER_THRESHOLD_XLM: u64 = 1_000_000_000;

/// Stroops per XLM (1 XLM = 10^7 stroops).
pub const STROOPS_PER_XLM: u64 = 10_000_000;

/// Formats a stroop amount as whole or decimal XLM, e.g. `5_000_000` → `"0.5"`.
pub fn format_xlm(stroops: u64) -> String {
    let whole = stroops / STROOPS_PER_XLM;
    let frac = stroops % STROOPS_PER_XLM;
    if frac == 0 {
        whole.to_string()
    } else {
        format!("{}.{}", whole, format!("{:07}", frac).trim_end_matches('0'))
    }
}

/// Human-readable `LargeTransfer` label, e.g. `"LargeTransfer(>=10000XLM)"`.
pub fn large_transfer_label(threshold_xlm: Option<u64>, threshold_stroops: Option<u64>) -> String {
    match (threshold_xlm, threshold_stroops) {
        (Some(xlm), None) => format!("LargeTransfer(>={}XLM)", xlm),
        (None, Some(stroops)) => format!("LargeTransfer(>={}XLM)", format_xlm(stroops)),
        _ => "LargeTransfer".into(),
    }
}

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

impl FromStr for Network {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "mainnet" => Ok(Network::Mainnet),
            "testnet" => Ok(Network::Testnet),
            "futurenet" => Ok(Network::Futurenet),
            other => bail!(
                "unknown network '{}'. Valid values: mainnet, testnet, futurenet.",
                other
            ),
        }
    }
}

// ── AlertRule ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum AlertRule {
    AnyTransaction,
    TransactionFailed,
    /// Fires when a transaction fails because the source account cannot cover the fee
    /// (Horizon result code `txINSUFFICIENT_BALANCE`). Other failures do not match.
    InsufficientFunds,
    /// Fires when the transaction invokes any Soroban host function, regardless of name.
    SorobanInvocation,
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
    /// Fires when the transaction's source account equals `source_account`.
    SourceAccountMatch { source_account: String },
}

impl AlertRule {
    pub fn validate(&mut self, contract_label: &str) -> Result<()> {
        match self {
            AlertRule::LargeTransfer { threshold_xlm, threshold_stroops } => {
                match (*threshold_xlm, *threshold_stroops) {
                    (Some(_), Some(_)) => bail!(
                        "contract '{}': LargeTransfer: specify either threshold_stroops or \
                         threshold_xlm, not both",
                        contract_label
                    ),
                    (None, None) => bail!(
                        "contract '{}': LargeTransfer requires threshold_xlm or threshold_stroops",
                        contract_label
                    ),
                    (Some(xlm), None) => {
                        if xlm == 0 {
                            bail!(
                                "contract '{}': LargeTransfer threshold_xlm must be > 0",
                                contract_label
                            );
                        }
                        if xlm > MAX_LARGE_TRANSFER_THRESHOLD_XLM {
                            bail!(
                                "contract '{}': LargeTransfer threshold_xlm must be <= {}",
                                contract_label,
                                MAX_LARGE_TRANSFER_THRESHOLD_XLM
                            );
                        }
                    }
                    (None, Some(stroops)) => {
                        if stroops == 0 {
                            bail!(
                                "contract '{}': LargeTransfer threshold_stroops must be > 0",
                                contract_label
                            );
                        }
                        if stroops > MAX_LARGE_TRANSFER_THRESHOLD_XLM * STROOPS_PER_XLM {
                            bail!(
                                "contract '{}': LargeTransfer threshold_stroops must be <= {}",
                                contract_label,
                                MAX_LARGE_TRANSFER_THRESHOLD_XLM * STROOPS_PER_XLM
                            );
                        }
                    }
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
            AlertRule::AnyTransaction
            | AlertRule::TransactionFailed
            | AlertRule::InsufficientFunds
            | AlertRule::SorobanInvocation => {}
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
            AlertRule::SourceAccountMatch { source_account } => {
                // Stellar account (G-) addresses are 56 base32 characters.
                if source_account.len() != 56 || !source_account.starts_with('G') {
                    bail!(
                        "contract '{}': SourceAccountMatch source_account '{}' is not a valid \
                         Stellar account address (must start with 'G' and be 56 characters)",
                        contract_label,
                        source_account
                    );
                }
            }
        }
        Ok(())
    }
    /// Human-readable rule description with parameters.
    /// Used as `rule_triggered` in alert payloads (e.g. `"LargeTransfer(>=10000XLM)"`).
    pub fn label(&self) -> String {
        match self {
            AlertRule::AnyTransaction => "AnyTransaction".into(),
            AlertRule::TransactionFailed => "TransactionFailed".into(),
            AlertRule::InsufficientFunds => "InsufficientFunds".into(),
            AlertRule::SorobanInvocation => "SorobanInvocation".into(),
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
            AlertRule::SourceAccountMatch { source_account } => {
                format!("SourceAccountMatch({})", source_account)
            }
        }
    }

    /// Stable machine-readable rule variant name.
    /// Used as `rule_type` in alert payloads (e.g. `"LargeTransfer"`, `"HighFee"`).
    /// Use this for programmatic routing — it never includes parameters.
    pub fn rule_type(&self) -> String {
        match self {
            AlertRule::AnyTransaction => "AnyTransaction".into(),
            AlertRule::TransactionFailed => "TransactionFailed".into(),
            AlertRule::LargeTransfer { .. } => "LargeTransfer".into(),
            AlertRule::FunctionCalled { .. } => "FunctionCalled".into(),
            AlertRule::AdminFunctionCalled { .. } => "AdminFunctionCalled".into(),
            AlertRule::HighFee { .. } => "HighFee".into(),
        }
    }
}

// ── TlsConfig ─────────────────────────────────────────────────────────────────

/// Optional mutual-TLS settings for webhook delivery to one contract.
/// All paths point to PEM files and are checked for existence at validation time.
/// The private key contents are never logged.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM client certificate presented to the webhook receiver.
    pub client_cert: String,
    /// PEM private key (PKCS#8) matching `client_cert`.
    pub client_key: String,
    /// Optional PEM CA bundle used to validate the webhook server's certificate.
    #[serde(default)]
    pub ca_cert: Option<String>,
}

impl TlsConfig {
    pub fn validate(&self, contract_label: &str) -> Result<()> {
        let mut paths = vec![("client_cert", &self.client_cert), ("client_key", &self.client_key)];
        if let Some(ca) = &self.ca_cert {
            paths.push(("ca_cert", ca));
        }
        for (field, path) in paths {
            if path.trim().is_empty() {
                bail!("contract '{}': tls.{} must not be empty", contract_label, field);
            }
            if !Path::new(path).is_file() {
                bail!(
                    "contract '{}': tls.{} file '{}' does not exist or is not a file",
                    contract_label,
                    field,
                    path
                );
            }
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
    pub webhook_url: String,
    /// Optional secret sent as X-TxWatch-Secret header on every webhook POST.
    /// Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`).
    pub webhook_secret: Option<String>,
    /// Test-only: override the Horizon base URL. Never read from TOML (`serde(skip)`) —
    /// set programmatically in tests. Production code must leave this `None`.
    #[serde(skip, default)]
    pub test_horizon_base_url: Option<String>,
}

/// Maximum length, in characters, of a tag key or value.
const MAX_TAG_LEN: usize = 64;

fn validate_tag_part(contract_label: &str, kind: &str, part: &str) -> Result<()> {
    if part.trim().is_empty() {
        bail!("contract '{}': tag {} must not be empty", contract_label, kind);
    }
    if part.chars().count() > MAX_TAG_LEN {
        bail!(
            "contract '{}': tag {} '{}' must be at most {} characters",
            contract_label,
            kind,
            part,
            MAX_TAG_LEN
        );
    }
    Ok(())
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

        if self.max_alerts_per_cycle == Some(0) {
            bail!(
                "contract '{}': max_alerts_per_cycle must be >= 1 when set",
                self.label
            );
        }

        if let Some(horizon_url) = &self.horizon_url {
            let parsed_horizon = Url::parse(horizon_url).map_err(|e| {
                anyhow!(
                    "contract '{}': horizon_url '{}' is not a valid URL: {}",
                    self.label,
                    horizon_url,
                    e
                )
            })?;
            if parsed_horizon.scheme() != "http" && parsed_horizon.scheme() != "https" {
                bail!(
                    "contract '{}': horizon_url '{}' must use http or https scheme",
                    self.label,
                    horizon_url
                );
            }
            if parsed_horizon.host().is_none() {
                bail!(
                    "contract '{}': horizon_url '{}' has no host",
                    self.label,
                    horizon_url
                );
            }
        }

        if self.rules.is_empty() {
            bail!("contract '{}': at least one rule is required", self.label);
        }

        let label = self.label.clone();
        for rule in &mut self.rules {
            rule.validate(&label)?;
        }

        if let Some(tls) = &self.tls {
            tls.validate(&self.label)?;
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
    /// Maximum webhooks sent per contract in a single poll cycle. Alerts above the cap
    /// are logged and suppressed, not silently dropped. Unset (default) means unlimited.
    /// A contract's own `max_alerts_per_cycle` takes precedence over this value.
    #[serde(default)]
    pub max_alerts_per_cycle: Option<u64>,
}

fn default_http_pool_max_idle_per_host() -> Option<usize> {
    None
}

fn default_http_tcp_keepalive_secs() -> Option<u64> {
    None
}

fn deserialize_toml_with_field_context<'de, T>(raw: &'de str) -> Result<T>
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

/// Checks `cursor_file` before polling starts (#279).
///
/// - Existing file: must be readable and contain valid JSON.
/// - Missing file: its parent directory must exist and be writable.
fn validate_cursor_file(path: &str) -> Result<()> {
    let cursor_path = Path::new(path);

    if cursor_path.is_file() {
        let raw = fs::read_to_string(cursor_path)
            .with_context(|| format!("cursor_file '{}' cannot be read", path))?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .with_context(|| format!("cursor_file '{}' is not valid JSON", path))?;
        return Ok(());
    }

    if cursor_path.exists() {
        bail!("cursor_file '{}' exists but is not a regular file", path);
    }

    let dir = match cursor_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if !dir.is_dir() {
        bail!("cursor_file directory '{}' does not exist", dir.display());
    }
    check_dir_writable(dir)
}

/// Probes writability by creating and removing a uniquely named file.
fn check_dir_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".txwatch-write-probe-{}", std::process::id()));
    match fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => bail!("cursor_file directory '{}' is not writable", dir.display()),
    }
}

// ── Env-var interpolation ─────────────────────────────────────────────────────

/// Resolves a `${VAR_NAME}` reference to the corresponding environment variable.
///
/// Supported syntax: the whole value must be exactly `${VAR_NAME}`, where `VAR_NAME`
/// contains only ASCII letters, digits and underscores. Values containing no `$`
/// reference are returned unchanged. Any other `$` pattern (`$VAR` without braces,
/// `${VAR:-default}`, or a reference embedded in a longer string) is rejected with an
/// error rather than silently passed through as a literal.
fn resolve_env_interpolation(value: &str) -> Result<String> {
    if let Some(var_name) = value.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        let valid = !var_name.is_empty()
            && var_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !valid {
            bail!(
                "unsupported env var interpolation '{}': only '${{VAR_NAME}}' is supported",
                value
            );
        }
        return std::env::var(var_name)
            .with_context(|| format!("env var '{}' referenced in config is not set", var_name));
    }
    let looks_like_reference = value
        .match_indices('$')
        .any(|(i, _)| matches!(
            value[i + 1..].chars().next(),
            Some(c) if c == '{' || c == '_' || c.is_ascii_alphabetic()
        ));
    if looks_like_reference {
        bail!(
            "unsupported env var interpolation '{}': only a value of exactly '${{VAR_NAME}}' is supported",
            value
        );
    }
    Ok(value.to_owned())
}

impl AppConfig {
    fn resolve_env_vars(&mut self) -> Result<()> {
        for contract in &mut self.contracts {
            if let Some(secret) = &contract.webhook_secret {
                contract.webhook_secret = Some(resolve_env_interpolation(secret)?);
            }
            contract.webhook_url = resolve_env_interpolation(&contract.webhook_url)?;
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("cannot read config file '{}'", path.display()))?;
        let mut cfg: AppConfig = deserialize_toml_with_field_context(&raw)
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
        // #256: enforce the hard contract limit with a clear error message.
        if self.contracts.len() > MAX_CONTRACTS {
            bail!(
                "too many contracts: {} configured, maximum is {} \
                 (reduce the number of [[contracts]] entries)",
                self.contracts.len(),
                MAX_CONTRACTS
            );
        }
        for contract in &mut self.contracts {
            contract.validate()?;
        }
        if let Some(path) = &self.cursor_file {
            validate_cursor_file(path)?;
        }
        let mut seen = std::collections::HashSet::new();
        for contract in &self.contracts {
            if !seen_labels.insert(&contract.label) {
                bail!("duplicate contract label '{}'", contract.label);
            }
        }
        // The same contract_id is valid on different networks (mainnet vs testnet),
        // but not twice on the same network.
        let mut seen_ids = std::collections::HashSet::new();
        for contract in &self.contracts {
            let key = (contract.contract_id.as_str(), contract.network.as_str());
            if !seen_ids.insert(key) {
                bail!(
                    "duplicate contract_id '{}' on network '{}'",
                    contract.contract_id,
                    contract.network.as_str()
                );
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
    use std::path::PathBuf;

    #[test]
    fn env_interpolation_passes_plain_values_through() {
        assert_eq!(resolve_env_interpolation("plain-secret").unwrap(), "plain-secret");
        assert_eq!(resolve_env_interpolation("price$5").unwrap(), "price$5");
    }

    #[test]
    fn env_interpolation_resolves_braced_var() {
        std::env::set_var("TXWATCH_TEST_INTERP_SECRET", "s3cret");
        assert_eq!(
            resolve_env_interpolation("${TXWATCH_TEST_INTERP_SECRET}").unwrap(),
            "s3cret"
        );
    }

    #[test]
    fn env_interpolation_rejects_unsupported_patterns() {
        for bad in ["$VAR", "${VAR:-default}", "prefix-${VAR}", "${}", "${VAR}-suffix"] {
            let err = resolve_env_interpolation(bad).unwrap_err().to_string();
            assert!(err.contains("unsupported env var interpolation"), "{bad}: {err}");
        }
    }

    fn valid_contract() -> WatchedContract {
        WatchedContract {
            label: "Test".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://example.com/hook".into(),
            webhook_secret: None,
            test_horizon_base_url: None,
        }
    }

    #[test]
    fn valid_config_passes() {
        let mut c = valid_contract();
        assert!(c.validate().is_ok());
    }

    /// #268: horizon_url is optional and absent by default.
    /// #269: max_alerts_per_cycle is optional and defaults to unlimited.
    #[test]
    fn max_alerts_per_cycle_defaults_to_none() {
        let mut c = valid_contract();
        assert!(c.max_alerts_per_cycle.is_none());
        assert!(c.validate().is_ok());
    }

    /// #269: the global cap and per-contract override both parse from TOML.
    #[test]
    fn max_alerts_per_cycle_is_parsed_globally_and_per_contract() {
        let raw = r#"
            poll_interval_seconds = 10
            max_alerts_per_cycle = 50
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            max_alerts_per_cycle = 5
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(cfg.max_alerts_per_cycle, Some(50));
        assert_eq!(cfg.contracts[0].max_alerts_per_cycle, Some(5));
    }

    #[test]
    fn rejects_zero_max_alerts_per_cycle_on_contract() {
        let mut c = valid_contract();
        c.max_alerts_per_cycle = Some(0);
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("max_alerts_per_cycle must be >= 1"), "got: {}", err);
    }

    #[test]
    fn rejects_zero_global_max_alerts_per_cycle() {
        let mut cfg = AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![valid_contract()],
            cursor_file: None,
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
            max_alerts_per_cycle: Some(0),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("max_alerts_per_cycle must be >= 1"), "got: {}", err);
    }

    #[test]
    fn horizon_url_defaults_to_none() {
        let mut c = valid_contract();
        assert!(c.horizon_url.is_none());
        assert!(c.validate().is_ok());
    }

    /// #268: horizon_url is parsed from TOML.
    #[test]
    fn horizon_url_is_parsed_from_toml() {
        let raw = r#"
            poll_interval_seconds = 10
            [[contracts]]
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            horizon_url = "https://my-private-horizon.example.com"
            [[contracts.rules]]
            type = "AnyTransaction"
        "#;
        let cfg: AppConfig = toml::from_str(raw).unwrap();
        assert_eq!(
            cfg.contracts[0].horizon_url.as_deref(),
            Some("https://my-private-horizon.example.com")
        );
    }

    #[test]
    fn accepts_valid_https_horizon_url() {
        let mut c = valid_contract();
        c.horizon_url = Some("https://my-private-horizon.example.com".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn accepts_valid_http_horizon_url_for_local_replica() {
        let mut c = valid_contract();
        c.horizon_url = Some("http://127.0.0.1:8000".into());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn rejects_horizon_url_that_is_not_a_url() {
        let mut c = valid_contract();
        c.horizon_url = Some("not a url".into());
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("horizon_url"), "got: {}", err);
    }

    #[test]
    fn rejects_horizon_url_with_ftp_scheme() {
        let mut c = valid_contract();
        c.horizon_url = Some("ftp://horizon.example.com".into());
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("must use http or https"), "got: {}", err);
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

    // ── Issue #278: mTLS config ──────────────────────────────────────────────

    fn fixture_path(name: &str) -> String {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../notifier/tests/fixtures")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn tls_section_parses_from_toml_with_optional_ca() {
        let raw = format!(
            r#"
            label = "x"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            [[rules]]
            type = "AnyTransaction"
            [tls]
            client_cert = "{}"
            client_key = "{}"
            "#,
            fixture_path("client-cert.pem"),
            fixture_path("client-key.pem"),
        );
        let mut c: WatchedContract = toml::from_str(&raw).unwrap();
        assert!(c.tls.as_ref().unwrap().ca_cert.is_none());
        c.validate().unwrap();
    }

    #[test]
    fn tls_rejects_missing_client_cert_file() {
        let mut c = valid_contract();
        c.tls = Some(TlsConfig {
            client_cert: "/nonexistent/client.pem".into(),
            client_key: fixture_path("client-key.pem"),
            ca_cert: None,
        });
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("tls.client_cert"), "got: {}", err);
    }

    #[test]
    fn tls_rejects_missing_ca_cert_file() {
        let mut c = valid_contract();
        c.tls = Some(TlsConfig {
            client_cert: fixture_path("client-cert.pem"),
            client_key: fixture_path("client-key.pem"),
            ca_cert: Some("/nonexistent/ca.pem".into()),
        });
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("tls.ca_cert"), "got: {}", err);
    }

    #[test]
    fn tls_rejects_empty_key_path() {
        let mut c = valid_contract();
        c.tls = Some(TlsConfig {
            client_cert: fixture_path("client-cert.pem"),
            client_key: "  ".into(),
            ca_cert: None,
        });
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("tls.client_key must not be empty"), "got: {}", err);
    }

    #[test]
    fn tls_accepts_existing_files() {
        let mut c = valid_contract();
        c.tls = Some(TlsConfig {
            client_cert: fixture_path("client-cert.pem"),
            client_key: fixture_path("client-key.pem"),
            ca_cert: Some(fixture_path("client-cert.pem")),
        });
        assert!(c.validate().is_ok());
    }

    // ── Issue #279: cursor_file validation ───────────────────────────────────

    fn cursor_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "txwatch-cursor-{}-{}",
            name,
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn app_config_with_cursor(cursor_file: String) -> AppConfig {
        AppConfig {
            poll_interval_seconds: 10,
            contracts: vec![valid_contract()],
            cursor_file: Some(cursor_file),
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
        }
    }

    #[test]
    fn cursor_file_existing_valid_json_is_accepted() {
        let dir = cursor_test_dir("valid-json");
        let file = dir.join("cursors.json");
        fs::write(&file, r#"{"CABC": "123-1"}"#).unwrap();
        let mut cfg = app_config_with_cursor(file.to_string_lossy().into_owned());
        assert!(cfg.validate().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_file_existing_invalid_json_is_rejected() {
        let dir = cursor_test_dir("invalid-json");
        let file = dir.join("cursors.json");
        fs::write(&file, "{ not json").unwrap();
        let mut cfg = app_config_with_cursor(file.to_string_lossy().into_owned());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("is not valid JSON"), "got: {}", err);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_file_missing_file_in_existing_dir_is_accepted() {
        let dir = cursor_test_dir("missing-file");
        let file = dir.join("not-yet-created.json");
        let mut cfg = app_config_with_cursor(file.to_string_lossy().into_owned());
        assert!(cfg.validate().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_file_missing_directory_is_rejected() {
        let dir = cursor_test_dir("missing-dir").join("no-such-subdir");
        let file = dir.join("cursors.json");
        let mut cfg = app_config_with_cursor(file.to_string_lossy().into_owned());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("does not exist"), "got: {}", err);
    }

    #[cfg(unix)]
    #[test]
    fn cursor_file_unwritable_directory_fails_with_clear_message() {
        use std::os::unix::fs::PermissionsExt;

        let dir = cursor_test_dir("readonly-dir");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let file = dir.join("cursors.json");
        let mut cfg = app_config_with_cursor(file.to_string_lossy().into_owned());
        let err = cfg.validate().unwrap_err().to_string();
        // Restore permissions so cleanup succeeds.
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(err.contains("is not writable"), "got: {}", err);
    }

    #[test]
    fn insufficient_funds_rule_parses_from_toml() {
        let rule: AlertRule = toml::from_str("type = \"InsufficientFunds\"").unwrap();
        assert!(matches!(rule, AlertRule::InsufficientFunds));
        assert_eq!(rule.label(), "InsufficientFunds");
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
        c.rules = vec![AlertRule::LargeTransfer { threshold_xlm: Some(0), threshold_stroops: None }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_too_large_large_transfer_threshold() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::LargeTransfer { threshold_xlm: Some(MAX_LARGE_TRANSFER_THRESHOLD_XLM + 1), threshold_stroops: None }];
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
    fn futurenet_explorer_url_points_to_futurenet_path() {
        assert_eq!(
            Network::Futurenet.explorer_base_url(),
            "https://stellar.expert/explorer/futurenet"
        );
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
            max_alerts_per_cycle: None,
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("duplicate contract label"));
    }

    fn config_with(contracts: Vec<WatchedContract>) -> AppConfig {
        AppConfig {
            poll_interval_seconds: 10,
            contracts,
            cursor_file: None,
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
        }
    }

    #[test]
    fn rejects_same_label_with_different_contract_ids() {
        let first = valid_contract();
        let mut second = valid_contract();
        second.contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".into();
        let err = config_with(vec![first, second]).validate().unwrap_err();
        assert!(
            err.to_string().contains("duplicate contract label 'Test'"),
            "error should name the label, got: {}",
            err
        );
    }

    #[test]
    fn accepts_same_contract_id_on_different_networks() {
        let testnet = valid_contract();
        let mut mainnet = valid_contract();
        mainnet.label = "Mainnet copy".into();
        mainnet.network = Network::Mainnet;
        assert!(config_with(vec![testnet, mainnet]).validate().is_ok());
    }

    #[test]
    fn rejects_same_contract_id_on_same_network() {
        let first = valid_contract();
        let mut second = valid_contract();
        second.label = "Other label".into();
        let err = config_with(vec![first, second]).validate().unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("duplicate contract_id"), "got: {}", msg);
        assert!(msg.contains("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"), "got: {}", msg);
        assert!(msg.contains("testnet"), "got: {}", msg);
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
            max_alerts_per_cycle: None,
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
    fn source_account_match_accepts_valid_g_address() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::SourceAccountMatch {
            source_account: "G".to_string() + &"A".repeat(55),
        }];
        assert!(c.validate().is_ok());
    }

    #[test]
    fn source_account_match_rejects_wrong_length() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::SourceAccountMatch {
            source_account: "G".to_string() + &"A".repeat(54),
        }];
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("SourceAccountMatch"));
    }

    #[test]
    fn source_account_match_rejects_wrong_prefix() {
        let mut c = valid_contract();
        c.rules = vec![AlertRule::SourceAccountMatch {
            source_account: "C".to_string() + &"A".repeat(55),
        }];
        assert!(c.validate().is_err());
    }

    #[test]
    fn tags_accept_valid_key_values() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([
            ("team".to_string(), "defi".to_string()),
            ("severity".to_string(), "high".to_string()),
        ]));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn tags_accept_exactly_64_chars() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([("k".repeat(64), "v".repeat(64))]));
        assert!(c.validate().is_ok());
    }

    #[test]
    fn tags_reject_empty_key() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([("".to_string(), "defi".to_string())]));
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("tag key must not be empty"));
    }

    #[test]
    fn tags_reject_blank_value() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([("team".to_string(), "   ".to_string())]));
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("tag value must not be empty"));
    }

    #[test]
    fn tags_reject_key_over_64_chars() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([("k".repeat(65), "v".to_string())]));
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("tag key"));
        assert!(err.to_string().contains("at most 64 characters"));
    }

    #[test]
    fn tags_reject_value_over_64_chars() {
        let mut c = valid_contract();
        c.tags = Some(HashMap::from([("team".to_string(), "v".repeat(65))]));
        let err = c.validate().unwrap_err();
        assert!(err.to_string().contains("tag value"));
    }

    #[test]
    fn tags_deserialise_from_toml() {
        let raw = r#"
            label = "My Contract"
            contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            network = "testnet"
            webhook_url = "https://example.com/hook"
            tags = { team = "defi", env = "prod", severity = "high" }
            [[rules]]
            type = "AnyTransaction"
        "#;
        let c: WatchedContract = toml::from_str(raw).unwrap();
        let tags = c.tags.unwrap();
        assert_eq!(tags.get("team").map(String::as_str), Some("defi"));
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn source_account_match_deserialises_from_toml() {
        let raw = r#"
            type = "SourceAccountMatch"
            source_account = "GABC"
        "#;
        let rule: AlertRule = toml::from_str(raw).unwrap();
        assert!(matches!(
            rule,
            AlertRule::SourceAccountMatch { ref source_account } if source_account == "GABC"
        ));
    }

    #[test]
    fn rejects_poll_interval_too_low() {
        for val in 0u64..5 {
            let mut cfg = AppConfig {
                poll_interval_seconds: val,
                contracts: vec![],
                cursor_file: None,
                http_pool_max_idle_per_host: None,
                http_tcp_keepalive_secs: None,
                http_connection_verbose: None,
                cursor_file: None,
                contracts: vec![],
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
        // `{:#}` renders the full context chain, including the field path.
        let error_msg = format!("{:#}", result.unwrap_err());
        assert!(error_msg.contains("failed to parse config file"));
        assert!(error_msg.contains("field: poll_interval_seconds"));
    }

    // ── #256: MaxContractCheck tests ──────────────────────────────────────────

    /// Build an AppConfig with `n` distinct contracts for limit testing.
    fn cfg_with_n_contracts(n: usize) -> AppConfig {
        let contracts = (0..n)
            .map(|i| {
                // Generate a unique 56-char contract ID starting with 'C'.
                let suffix = format!("{:0>55}", i); // 55 zero-padded digits
                let contract_id = format!("C{}", &suffix[..55]);
                WatchedContract {
                    label: format!("Contract {}", i),
                    contract_id,
                    network: Network::Testnet,
                    rules: vec![AlertRule::AnyTransaction],
                    webhook_url: "https://example.com/hook".into(),
                    webhook_secret: None,
                    horizon_base_url_override: None,
                }
            })
            .collect();

        AppConfig {
            poll_interval_seconds: 10,
            contracts,
            cursor_file: None,
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
        }
    }

    /// #256: Exactly MAX_CONTRACTS contracts should be accepted.
    #[test]
    fn accepts_exactly_max_contracts() {
        let mut cfg = cfg_with_n_contracts(MAX_CONTRACTS);
        assert!(
            cfg.validate().is_ok(),
            "exactly {} contracts should be valid",
            MAX_CONTRACTS
        );
    }

    /// #256: Exceeding MAX_CONTRACTS must return a clear error.
    #[test]
    fn rejects_more_than_max_contracts() {
        let mut cfg = cfg_with_n_contracts(MAX_CONTRACTS + 1);
        let err = cfg.validate().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("too many contracts"),
            "error should mention 'too many contracts', got: {}",
            msg
        );
        assert!(
            msg.contains(&MAX_CONTRACTS.to_string()),
            "error should mention the limit ({}), got: {}",
            MAX_CONTRACTS,
            msg
        );
    }

    /// #256: The warn threshold is 80% of MAX_CONTRACTS = 80.
    /// This test documents the threshold value so it doesn't drift silently.
    #[test]
    fn warn_threshold_is_80_percent_of_max_contracts() {
        let warn_threshold = (MAX_CONTRACTS * 80) / 100;
        assert_eq!(
            warn_threshold, 80,
            "80%% of MAX_CONTRACTS ({}) should be 80",
            MAX_CONTRACTS
        );
    }

    /// #256: A config with exactly MAX_CONTRACTS * 80 / 100 + 1 contracts (81)
    /// is valid (it should pass validation and only log a warning at startup).
    #[test]
    fn accepts_81_contracts_warn_threshold_config_is_valid() {
        let warn_threshold = (MAX_CONTRACTS * 80) / 100; // 80
        let mut cfg = cfg_with_n_contracts(warn_threshold + 1); // 81
        assert!(
            cfg.validate().is_ok(),
            "{} contracts should be valid (only a warning is emitted at startup)",
            warn_threshold + 1
        );
    }
}
