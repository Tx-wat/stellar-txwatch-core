#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_path_to_error::Deserializer as PathDeserializer;
use std::collections::HashMap;
use std::str::FromStr;
use std::{fmt, fs, path::Path};
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

// ── Account ID helpers ────────────────────────────────────────────────────────

/// Returns `true` when `s` is a structurally valid Stellar account (G-address):
/// exactly 56 characters, starts with `'G'`, and uses only the uppercase
/// base-32 alphabet (`A-Z` and `2-7`).
///
/// This mirrors the contract-address check used for `C-addresses`, and can be
/// reused by any rule that references a Stellar account ID.
pub fn is_valid_stellar_account_id(s: &str) -> bool {
    s.len() == 56
        && s.starts_with('G')
        && s.bytes().all(|b| matches!(b, b'A'..=b'Z' | b'2'..=b'7'))
}

// ── ContractType ──────────────────────────────────────────────────────────────

/// Distinguishes Soroban contract alerts from classic Stellar account alerts.
///
/// Serializes as `"soroban"` or `"classic"` so webhook receivers can route
/// alerts without parsing the `contract_id` format.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ContractType {
    Soroban,
    Classic,
}

impl ContractType {
    /// Derives the contract type from a `contract_id` prefix:
    /// `'C'` → `Soroban`, `'G'` → `Classic`.
    ///
    /// Returns `None` for any other prefix.
    pub fn from_contract_id(contract_id: &str) -> Option<Self> {
        match contract_id.chars().next() {
            Some('C') => Some(ContractType::Soroban),
            Some('G') => Some(ContractType::Classic),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ContractType::Soroban => "soroban",
            ContractType::Classic => "classic",
        }
    }
}

impl fmt::Display for ContractType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
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
    /// Fires when a transfer of at least the threshold occurs.
    /// Specify either `threshold_stroops` (raw stroops) or `threshold_xlm` (whole XLM,
    /// converted to stroops during validation); the two are mutually exclusive.
    LargeTransfer {
        #[serde(default)]
        threshold_stroops: Option<u64>,
        #[serde(default)]
        threshold_xlm: Option<u64>,
    },
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
    /// Restricts alert firing to a specific time window and days of week (Issue #343).
    TimeWindow {
        hours_utc: String,
        #[serde(default)]
        weekdays: Vec<String>,
        #[serde(default)]
        wrap_rule: Option<String>,
    },
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
                if !is_valid_stellar_account_id(source_account) {
                    bail!(
                        "contract '{}': SourceAccountMatch source_account '{}' is not a valid \
                         Stellar account address (must start with 'G' and be 56 characters)",
                        contract_label,
                        source_account
                    );
                }
            }
            AlertRule::TimeWindow { hours_utc, weekdays, .. } => {
                let parts: Vec<&str> = hours_utc.split('-').collect();
                if parts.len() != 2 {
                    bail!(
                        "contract '{}': TimeWindow hours_utc must be formatted as 'HH:MM-HH:MM'",
                        contract_label
                    );
                }
                for part in parts {
                    let hm: Vec<&str> = part.split(':').collect();
                    if hm.len() != 2 {
                        bail!("contract '{}': TimeWindow invalid time format '{}'", contract_label, part);
                    }
                    let h: u32 = hm[0].parse().map_err(|_| anyhow!("contract '{}': TimeWindow invalid hour in '{}'", contract_label, part))?;
                    let m: u32 = hm[1].parse().map_err(|_| anyhow!("contract '{}': TimeWindow invalid minute in '{}'", contract_label, part))?;
                    if h > 23 || m > 59 {
                        bail!("contract '{}': TimeWindow time '{}:{:02}' out of 24-hour range", contract_label, h, m);
                    }
                }
                let valid_days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
                for day in weekdays {
                    if !valid_days.contains(&day.to_lowercase().as_str()) {
                        bail!(
                            "contract '{}': TimeWindow invalid weekday '{}' (expected Mon, Tue, Wed, Thu, Fri, Sat, Sun)",
                            contract_label,
                            day
                        );
                    }
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
            AlertRule::LargeTransfer { threshold_xlm, threshold_stroops } => {
                large_transfer_label(*threshold_xlm, *threshold_stroops)
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
            AlertRule::TimeWindow { hours_utc, weekdays, wrap_rule } => {
                if let Some(wrapped) = wrap_rule {
                    format!("TimeWindow({}, [{}], wrap={})", hours_utc, weekdays.join(", "), wrapped)
                } else {
                    format!("TimeWindow({}, [{}])", hours_utc, weekdays.join(", "))
                }
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
            AlertRule::InsufficientFunds => "InsufficientFunds".into(),
            AlertRule::SorobanInvocation => "SorobanInvocation".into(),
            AlertRule::LargeTransfer { .. } => "LargeTransfer".into(),
            AlertRule::FunctionCalled { .. } => "FunctionCalled".into(),
            AlertRule::AdminFunctionCalled { .. } => "AdminFunctionCalled".into(),
            AlertRule::HighFee { .. } => "HighFee".into(),
            AlertRule::SourceAccountMatch { .. } => "SourceAccountMatch".into(),
            AlertRule::TimeWindow { .. } => "TimeWindow".into(),
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
    /// Optional per-contract Horizon base URL override (Issue #268).
    #[serde(default)]
    pub horizon_url: Option<String>,
    /// Maximum webhooks sent for this contract in a single poll cycle (Issue #269).
    /// Unset means unlimited (or the global cap, if configured).
    #[serde(default)]
    pub max_alerts_per_cycle: Option<u64>,
    /// Per-contract webhook delivery timeout in seconds (Issue #462).
    /// Unset means the global `webhook_timeout_seconds` applies.
    #[serde(default)]
    pub webhook_timeout_seconds: Option<u64>,
    /// Allow webhook URLs resolving to localhost or private IP ranges (Issue #346).
    #[serde(default)]
    pub allow_internal_webhook_urls: Option<bool>,
    /// Optional key/value tags attached to this contract, e.g. `tags = { team = "defi" }`.
    /// Keys and values are validated by `validate_tag_part`.
    #[serde(default)]
    pub tags: Option<HashMap<String, String>>,
    /// Optional mutual-TLS settings for webhook delivery (Issue #278).
    #[serde(default)]
    pub tls: Option<TlsConfig>,
    /// Optional inline PEM-encoded client certificate for webhook mTLS.
    #[serde(default)]
    pub webhook_tls_cert_pem: Option<String>,
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
    /// Contract type derived from the `contract_id` prefix (`'C'` → Soroban,
    /// `'G'` → Classic). Defaults to `Soroban` for unrecognised prefixes.
    pub fn contract_type(&self) -> ContractType {
        ContractType::from_contract_id(&self.contract_id).unwrap_or(ContractType::Soroban)
    }

    pub fn validate(&mut self) -> Result<()> {
        if self.label.trim().is_empty() {
            bail!("a contract has an empty label");
        }

        // Stellar contract addresses start with 'C', are 56 chars, and use the
        // RFC 4648 base32 alphabet in uppercase (A-Z and 2-7 only).
        let valid_charset = self
            .contract_id
            .bytes()
            .all(|b| matches!(b, b'A'..=b'Z' | b'2'..=b'7'));
        if self.contract_id.len() != 56 || !self.contract_id.starts_with('C') || !valid_charset {
            bail!(
                "contract '{}': contract_id '{}' is not a valid Stellar contract address \
                 (must start with 'C', be 56 characters, and use only uppercase base32 \
                 characters A-Z and 2-7)",
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

        let allow_internal = self.allow_internal_webhook_urls.unwrap_or(false);
        if !allow_internal {
            if let Some(host_str) = parsed_url.host_str() {
                let is_internal = if host_str == "localhost"
                    || host_str.ends_with(".localhost")
                    || host_str.ends_with(".local")
                {
                    true
                } else if let Ok(ip) = host_str.parse::<std::net::IpAddr>() {
                    match ip {
                        std::net::IpAddr::V4(ipv4) => {
                            ipv4.is_loopback()
                                || ipv4.is_private()
                                || ipv4.is_link_local()
                                || ipv4.is_broadcast()
                                || ipv4.is_unspecified()
                        }
                        std::net::IpAddr::V6(ipv6) => {
                            ipv6.is_loopback()
                                || ipv6.is_unspecified()
                                || (ipv6.segments()[0] & 0xfe00) == 0xfc00
                                || (ipv6.segments()[0] & 0xffc0) == 0xfe80
                        }
                    }
                } else {
                    false
                };

                if is_internal {
                    bail!(
                        "contract '{}': webhook_url '{}' resolves to localhost or private IP range (SSRF prevention). Set allow_internal_webhook_urls = true to allow.",
                        self.label,
                        self.webhook_url
                    );
                }
            }
        }

        if self.max_alerts_per_cycle == Some(0) {
            bail!(
                "contract '{}': max_alerts_per_cycle must be >= 1 when set",
                self.label
            );
        }

        if self.webhook_timeout_seconds == Some(0) {
            bail!(
                "contract '{}': webhook_timeout_seconds must be >= 1 when set",
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

        if let Some(tags) = &self.tags {
            for (key, value) in tags {
                validate_tag_part(&self.label, "key", key)?;
                validate_tag_part(&self.label, "value", value)?;
            }
        }

        if let Some(tls) = &self.tls {
            tls.validate(&self.label)?;
        }

        if let Some(pem) = &self.webhook_tls_cert_pem {
            if pem.trim().is_empty() {
                bail!("contract '{}': webhook_tls_cert_pem must not be empty", self.label);
            }
            // #458: actually parse the PEM/DER rather than only checking for the
            // BEGIN/END markers, so `validate` catches invalid certificates before
            // `build_pinned_cert_client` fails at webhook-send time.
            reqwest::Certificate::from_pem(pem.as_bytes()).map_err(|e| {
                anyhow!(
                    "contract '{}': webhook_tls_cert_pem is not a valid PEM-encoded certificate: {}",
                    self.label,
                    e
                )
            })?;
        }

        Ok(())
    }
}

// ── AppConfig ─────────────────────────────────────────────────────────────────

/// Maximum number of watched contracts allowed in a single configuration.
/// Exceeding this limit would create too many concurrent Horizon polling tasks,
/// potentially exhausting memory or file descriptors.
pub const MAX_CONTRACTS: usize = 100;

// Typestate pattern evaluation for AppConfig (Issue #339):
// A typestate design (`AppConfig<Unvalidated>` -> `AppConfig<Validated>`) was evaluated
// and rejected (wontfix) for the following reasons:
// 1. Ergonomics: AppConfig is a plain configuration struct passed across threads, async
//    tasks, and CLI command boundaries. Adding a type-level state parameter complicates function
//    signatures (e.g. run, run_with, run_with_shutdown) without practical safety gains, as
//    `AppConfig::from_file` already guarantees that validation occurs before execution.
// 2. Test maintenance: Many integration tests construct mock `AppConfig` instances directly
//    with minimal fields. Mandating a typestate transition or validated wrapper would add
//    needless ceremony and break test ergonomics across the workspace.
// 3. Deserialization: Serde deserialization directly into the struct remains idiomatic and clean;
//    runtime validation via `validate()` provides richer error reporting with field contexts.
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
    /// Configurable HTTP User-Agent header for Horizon requests (Issue #340).
    /// Defaults to `txwatch/<version>` when unset.
    #[serde(default)]
    pub http_user_agent: Option<String>,
    /// Webhook delivery timeout in seconds (Issue #462). Applies to every contract
    /// unless overridden by a contract's own `webhook_timeout_seconds`.
    /// Defaults to 15 seconds for backward compatibility.
    #[serde(default = "default_webhook_timeout_seconds")]
    pub webhook_timeout_seconds: Option<u64>,
    /// Allow webhook URLs resolving to localhost or private IP ranges for every
    /// contract (Issue #346). Per-contract settings take precedence.
    #[serde(default)]
    pub allow_internal_webhook_urls: Option<bool>,
}

/// Default webhook delivery timeout in seconds (Issue #462).
pub const DEFAULT_WEBHOOK_TIMEOUT_SECONDS: u64 = 15;

fn default_webhook_timeout_seconds() -> Option<u64> {
    Some(DEFAULT_WEBHOOK_TIMEOUT_SECONDS)
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
        // Tolerate a leading UTF-8 BOM, which some editors prepend.
        let raw = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
        let mut cfg