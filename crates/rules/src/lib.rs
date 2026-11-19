#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

//! txwatch-rules evaluates `AlertRule` conditions against enriched Stellar transactions
//! and constructs structured `AlertPayload` webhook bodies.
//!
//! Also exposes `HorizonOperation` and `extract_soroban_details` so that the pure
//! data-extraction logic lives in this crate (no I/O, fully unit-testable) and the
//! poller only handles HTTP concerns.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use txwatch_config::{AlertRule, ContractType};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Maximum XLM supply in stroops: 50 billion XLM × 10^7 stroops/XLM.
/// The total Stellar XLM supply is capped at ~50 billion XLM. This constant serves
/// as a reference for validating that u64 is sufficient for any realistic transaction
/// amount, since 500 trillion is well below u64::MAX (18.4 quintillion).
pub const MAX_XLM_SUPPLY_STROOPS: u64 = 500_000_000_000_000_000;

// ── Horizon operation shape ───────────────────────────────────────────────────

/// A single Horizon operation record.
///
/// Only the fields relevant to Soroban enrichment are captured; unknown fields
/// are ignored by serde so this struct stays forward-compatible with Horizon API changes.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonOperation {
    /// The operation type string as returned by Horizon (e.g. `"invoke_host_function"`,
    /// `"payment"`).
    #[serde(rename = "type")]
    pub op_type: String,
    /// Present on `invoke_host_function` operations — the Soroban function name.
    pub function: Option<String>,
    /// Present on `payment` operations — amount as a decimal string (e.g. `"1000.0000000"`).
    pub amount: Option<String>,
}

/// Extract Soroban function names and total payment amount from a list of operations.
///
/// This is a pure data transformation with no I/O — it lives in `txwatch-rules` so it
/// can be unit-tested without spinning up mock HTTP servers.
///
/// # Returns
/// `(function_names, amount_stroops)` where:
/// - `function_names` contains one entry per `invoke_host_function` op that has a
///   `function` field.
/// - `amount_stroops` is `Some(total)` when at least one `payment` op is present,
///   otherwise `None`. Amounts are summed using saturating arithmetic.
pub fn extract_soroban_details(ops: Vec<HorizonOperation>) -> (Vec<String>, Option<u64>) {
    let mut function_names: Vec<String> = Vec::new();
    let mut total_stroops: u64 = 0;
    let mut has_payment = false;

    for op in ops {
        if op.op_type == "invoke_host_function" {
            if let Some(f) = op.function {
                function_names.push(f);
            }
        }
        if op.op_type == "payment" {
            if let Some(amt_str) = op.amount {
                if let Some(stroops) = parse_xlm_to_stroops(&amt_str) {
                    total_stroops = total_stroops.saturating_add(stroops);
                    has_payment = true;
                }
            }
        }
    }

    (function_names, if has_payment { Some(total_stroops) } else { None })
}

/// Parse a decimal XLM amount string into an exact stroop count.
///
/// Uses integer/fixed-point arithmetic (never `f64`) so that amounts with up to
/// 7 fractional digits convert to stroops without any rounding drift. The
/// fractional part is padded with zeros or truncated to exactly 7 digits.
///
/// Returns `None` for malformed input (empty, non-numeric, more than one `.`,
/// or values that overflow `u64` when scaled to stroops).
fn parse_xlm_to_stroops(amount: &str) -> Option<u64> {
    const STROOPS_PER_XLM: u64 = 10_000_000;
    const FRACTION_DIGITS: usize = 7;

    let amount = amount.trim();
    if amount.is_empty() {
        return None;
    }

    let (whole_str, frac_str) = match amount.split_once('.') {
        Some((w, f)) => (w, f),
        None => (amount, ""),
    };

    // Reject a second decimal point or any non-digit characters.
    if frac_str.contains('.') {
        return None;
    }
    if !whole_str.chars().all(|c| c.is_ascii_digit())
        || !frac_str.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    if whole_str.is_empty() && frac_str.is_empty() {
        return None;
    }

    let whole: u64 = if whole_str.is_empty() {
        0
    } else {
        whole_str.parse().ok()?
    };

    // Pad or truncate the fractional part to exactly 7 digits.
    let mut frac_digits = [0u8; FRACTION_DIGITS];
    for (i, byte) in frac_str.bytes().take(FRACTION_DIGITS).enumerate() {
        frac_digits[i] = byte - b'0';
    }
    let mut frac: u64 = 0;
    for digit in frac_digits {
        frac = frac * 10 + u64::from(digit);
    }

    whole
        .checked_mul(STROOPS_PER_XLM)?
        .checked_add(frac)
}

// ── Horizon transaction shape ─────────────────────────────────────────────────

/// Raw Horizon transaction record as returned by the REST API.
///
/// This is the single definition of the Horizon transaction shape. The poller
/// reuses it (via `#[serde(flatten)]`) rather than redeclaring its fields.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonTransaction {
    pub hash: String,
    pub created_at: String, // RFC 3339
    pub successful: bool,
    pub paging_token: String,
    /// Fee charged in stroops (Horizon returns this as a string).
    pub fee_charged: Option<String>,
    /// Total number of operations in this transaction as reported by Horizon.
    pub operation_count: Option<u32>,
    /// Base64-encoded XDR transaction envelope.
    pub envelope_xdr: Option<String>,
    /// Base64-encoded XDR transaction result.
    pub result_xdr: Option<String>,
    /// Memo type: `"text"`, `"hash"`, `"id"`, `"return"` or `"none"`.
    pub memo_type: Option<String>,
    /// Memo value; absent when `memo_type` is `"none"`.
    pub memo: Option<String>,
}

// ── Enriched transaction ──────────────────────────────────────────────────────

/// A transaction enriched with Soroban-specific fields extracted from the
/// Horizon `operations` sub-resource JSON (returned inline via `join=operations`
/// or fetched separately). We keep this as a plain struct so rule evaluation
/// stays pure and testable without network calls.
#[derive(Debug, Clone)]
pub struct EnrichedTransaction {
    pub hash: String,
    pub timestamp: DateTime<Utc>,
    pub successful: bool,
    pub paging_token: String,
    /// All Soroban contract functions invoked in this transaction (may be multiple).
    pub function_names: Vec<String>,
    /// Transfer amount in stroops (1 XLM = 10_000_000 stroops), if detected.
    /// Uses u64 because the total XLM supply is ~50 billion XLM = ~500 trillion stroops,
    /// which is well within u64::MAX (18.4 quintillion). This type is sufficient for any
    /// realistic transaction amount on the Stellar network.
    pub amount_stroops: Option<u64>,
    /// Fee charged for this transaction in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// Total number of operations in this transaction as reported by Horizon.
    pub operation_count: Option<u32>,
    /// Memo type from Horizon (`"text"`, `"hash"`, `"id"`, `"return"`, `"none"`).
    pub memo_type: Option<String>,
    /// Memo value from Horizon, if the transaction carries one.
    pub memo: Option<String>,
}

impl EnrichedTransaction {
    /// Build from a raw Horizon record plus optional Soroban operation details.
    ///
    /// # Fee precedence
    /// An explicitly passed `fee_charged_stroops` always takes precedence over
    /// the record's own `fee_charged`. `tx.fee_charged` is parsed only as a
    /// fallback when `fee_charged_stroops` is `None`. The poller passes `None`
    /// (the only call site outside tests), so the Horizon record's value is used.
    pub fn from_horizon(
        tx: HorizonTransaction,
        function_names: Vec<String>,
        amount_stroops: Option<u64>,
        fee_charged_stroops: Option<u64>,
    ) -> Result<Self> {
        let timestamp = tx.created_at.parse::<DateTime<Utc>>().with_context(|| {
            format!(
                "cannot parse timestamp '{}' for tx {}",
                tx.created_at, tx.hash
            )
        })?;

        let result_code = tx.result_xdr.as_deref().and_then(transaction_result_code);

        Ok(Self {
            hash:          tx.hash,
            timestamp,
            successful:    tx.successful,
            paging_token:  tx.paging_token,
            function_names,
            amount_stroops,
            fee_charged_stroops: fee_charged_stroops.or_else(|| {
                tx.fee_charged
                    .as_deref()
                    .and_then(|s| s.parse::<u64>().ok())
            }),
            operation_count: tx.operation_count,
            memo_type:     tx.memo_type,
            memo:          tx.memo,
        })
    }
}

/// `TransactionResultCode::txINSUFFICIENT_BALANCE` from Stellar-transaction.x.
pub const TX_INSUFFICIENT_BALANCE: i32 = -7;

/// Extracts the transaction result code from a base64-encoded `TransactionResult`.
///
/// XDR layout: `int64 feeCharged` (8 bytes, big-endian), then the union discriminant
/// `TransactionResultCode` as a big-endian `int32` (bytes 8..12). Returns `None` for
/// invalid base64 or input shorter than 12 bytes.
pub fn transaction_result_code(result_xdr: &str) -> Option<i32> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(result_xdr.trim())
        .ok()?;
    let code: [u8; 4] = bytes.get(8..12)?.try_into().ok()?;
    Some(i32::from_be_bytes(code))
}

// ── AlertPayload ──────────────────────────────────────────────────────────────

/// The JSON body POSTed to the webhook URL when a rule fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertPayload {
    pub label: String,
    pub contract_id: String,
    /// Whether the monitored address is a Soroban contract (`"soroban"`) or a
    /// classic account (`"classic"`), derived from the `contract_id` prefix.
    pub contract_type: ContractType,
    /// Serialises as the lowercase name (`"mainnet"`, `"testnet"`, `"futurenet"`),
    /// so the webhook wire format is unchanged from the old `String` field.
    pub network: Network,
    /// Stable machine-readable rule variant (e.g. `"LargeTransfer"`).
    pub rule_type: String,
    pub rule_triggered: String,
    pub transaction_hash: String,
    /// First invoked function name (backward-compat singular field).
    pub function_name: Option<String>,
    /// All invoked function names in this transaction.
    pub function_names: Vec<String>,
    /// Amount in whole XLM (stroops / 10_000_000), present for LargeTransfer.
    #[serde(rename = "amount_xlm")]
    pub amount_xlm: Option<u64>,
    /// Amount in stroops (1 XLM = 10_000_000), present for LargeTransfer.
    /// Full precision alongside the truncated `amount_xlm`.
    pub amount_stroops: Option<u64>,
    /// Fee charged in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// Total number of operations in the transaction.
    pub operation_count: Option<u32>,
    /// Memo type of the transaction (`"text"`, `"hash"`, `"id"`, `"return"`, `"none"`).
    pub memo_type: Option<String>,
    /// Memo value of the transaction, if any.
    pub memo: Option<String>,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    /// ISO 8601 timestamp string.
    pub timestamp_iso: String,
    pub horizon_link: String,
    /// Stellar Expert explorer link for the transaction.
    pub explorer_link: String,
    /// Ledger sequence number this transaction was included in.
    pub ledger_sequence: Option<u32>,
    /// Stellar account that signed and submitted this transaction.
    pub source_account: Option<String>,
}

// ── Rule evaluation ───────────────────────────────────────────────────────────

/// Evaluate all rules for one contract against one transaction.
/// Returns one `AlertPayload` per matching rule.
/// Never panics — errors in individual rule evaluation are logged and skipped.
pub fn evaluate(
    label: &str,
    contract_id: &str,
    network: &Network,
    horizon_base: &str,
    explorer_base: &str,
    rules: &[AlertRule],
    tags: Option<&HashMap<String, String>>,
    tx: &EnrichedTransaction,
) -> Vec<AlertPayload> {
    let horizon_base  = horizon_base.trim_end_matches('/');
    let explorer_base = explorer_base.trim_end_matches('/');
    let horizon_link  = format!("{}/transactions/{}", horizon_base, tx.hash);
    let explorer_link = format!("{}/tx/{}", explorer_base, tx.hash);
    let timestamp = tx.timestamp.timestamp();
    let timestamp_iso = tx.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    rules
        .iter()
        .filter_map(|rule| match eval_rule(rule, tx) {
            Ok(true) => Some(AlertPayload {
                label: label.to_string(),
                contract_id: contract_id.to_string(),
                contract_type: ContractType::from_contract_id(contract_id),
                network: network.to_string(),
                rule_type: rule.rule_type(),
                rule_triggered: rule.label(),
                transaction_hash: tx.hash.clone(),
                function_name: tx.function_names.first().cloned(),
                function_names: tx.function_names.clone(),
                amount_xlm: tx.amount_stroops.map(|s| s / 10_000_000),
                amount_stroops: tx.amount_stroops,
                fee_charged_stroops: tx.fee_charged_stroops,
                operation_count: tx.operation_count,
                memo_type: tx.memo_type.clone(),
                memo: tx.memo.clone(),
                timestamp,
                timestamp_iso: timestamp_iso.clone(),
                horizon_link: horizon_link.clone(),
                explorer_link: explorer_link.clone(),
                ledger_sequence: tx.ledger_sequence,
                source_account: tx.source_account.clone(),
            }),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    tx = %tx.hash,
                    rule = %rule.label(),
                    error = %e,
                    "rule evaluation error — skipping"
                );
                None
            }
        })
        .collect()
}

// NOTE: When adding a new AlertRule variant, update both `eval_rule()` and
// `AlertRule::label()` together. Rust's exhaustive matching catches missing arms,
// but this convention should be preserved for new rule variants.
fn eval_rule(rule: &AlertRule, tx: &EnrichedTransaction) -> Result<bool> {
    Ok(match rule {
        AlertRule::AnyTransaction => true,

        AlertRule::TransactionFailed => !tx.successful,

        AlertRule::InsufficientFunds => {
            !tx.successful && tx.result_code == Some(TX_INSUFFICIENT_BALANCE)
        }

        AlertRule::SorobanInvocation => !tx.function_names.is_empty(),

        AlertRule::LargeTransfer { threshold_xlm } => {
            let threshold_stroops = threshold_xlm
                .checked_mul(10_000_000)
                .context("threshold_xlm overflow when converting to stroops")?;
            tx.amount_stroops
                .map(|s| s >= threshold)
                .unwrap_or(false)
        }

        // Exact, case-sensitive match: Soroban function names are case-sensitive
        // symbols, so `Withdraw` and `withdraw` are different functions.
        // (`AdminFunctionCalled` below is deliberately case-insensitive.)
        AlertRule::FunctionCalled { function_name } => tx
            .function_names
            .iter()
            .any(|f| f == function_name.as_str()),

        AlertRule::AdminFunctionCalled { function_names } => tx
            .function_names
            .iter()
            .any(|f| {
                let f_lower = f.to_lowercase();
                function_names.iter().any(|n| n.to_lowercase() == f_lower)
            }),

        AlertRule::HighFee { threshold_stroops, .. } => tx
            .fee_charged_stroops
            .map(|f| f >= *threshold_stroops)
            .unwrap_or(false),

        AlertRule::SourceAccountMatch { source_account } => {
            tx.source_account.as_deref() == Some(source_account.as_str())
        }

        AlertRule::TimeWindow { hours_utc, weekdays, wrap_rule } => {
            eval_time_window(hours_utc, weekdays, wrap_rule.as_deref(), tx, chrono::Utc::now())?
        }
    })
}

pub fn eval_time_window(
    hours_utc: &str,
    weekdays: &[String],
    wrap_rule: Option<&str>,
    tx: &EnrichedTransaction,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    use chrono::{Datelike, Timelike};
    if !weekdays.is_empty() {
        let day_str = match now.weekday() {
            chrono::Weekday::Mon => "mon",
            chrono::Weekday::Tue => "tue",
            chrono::Weekday::Wed => "wed",
            chrono::Weekday::Thu => "thu",
            chrono::Weekday::Fri => "fri",
            chrono::Weekday::Sat => "sat",
            chrono::Weekday::Sun => "sun",
        };
        if !weekdays.iter().any(|d| d.to_lowercase() == day_str) {
            return Ok(false);
        }
    }

    let parts: Vec<&str> = hours_utc.split('-').collect();
    if parts.len() == 2 {
        let parse_hm = |s: &str| -> Result<(u32, u32)> {
            let hm: Vec<&str> = s.split(':').collect();
            if hm.len() != 2 {
                bail!("invalid time component '{}'", s);
            }
            Ok((hm[0].parse()?, hm[1].parse()?))
        };
        let (sh, sm) = parse_hm(parts[0])?;
        let (eh, em) = parse_hm(parts[1])?;
        let now_minutes = now.hour() * 60 + now.minute();
        let start_minutes = sh * 60 + sm;
        let end_minutes = eh * 60 + em;

        let in_window = if start_minutes <= end_minutes {
            now_minutes >= start_minutes && now_minutes <= end_minutes
        } else {
            now_minutes >= start_minutes || now_minutes <= end_minutes
        };
        if !in_window {
            return Ok(false);
        }
    }

    if let Some(wrapped) = wrap_rule {
        match wrapped {
            "TransactionFailed" => Ok(!tx.successful),
            "InsufficientFunds" => Ok(!tx.successful && tx.result_code == Some(TX_INSUFFICIENT_BALANCE)),
            "SorobanInvocation" => Ok(!tx.function_names.is_empty()),
            "LargeTransfer" => Ok(tx.amount_stroops.map(|s| s > 0).unwrap_or(false)),
            "AnyTransaction" => Ok(true),
            _ => Ok(true),
        }
    } else {
        Ok(true)
    }
}

impl AlertPayload {
    /// Build the webhook payload for `rule` matching `tx` on the contract described by `ctx`.
    pub fn new(ctx: &PayloadContext<'_>, tx: &EnrichedTransaction, rule: &AlertRule) -> Self {
        let horizon_base = ctx.horizon_base.trim_end_matches('/');
        let explorer_base = ctx.explorer_base.trim_end_matches('/');
        Self {
            label: ctx.label.to_string(),
            contract_id: ctx.contract_id.to_string(),
            contract_type: ContractType::from_contract_id(ctx.contract_id),
            network: ctx.network.to_string(),
            rule_type: rule_type(rule),
            rule_triggered: rule_label(rule),
            transaction_hash: tx.hash.clone(),
            function_name: tx.function_names.first().cloned(),
            function_names: tx.function_names.clone(),
            amount_xlm: tx.amount_stroops.map(|s| s / 10_000_000),
            fee_charged_stroops: tx.fee_charged_stroops,
            timestamp: tx.timestamp.timestamp(),
            timestamp_iso: tx.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            horizon_link: format!("{}/transactions/{}", horizon_base, tx.hash),
            explorer_link: format!("{}/tx/{}", explorer_base, tx.hash),
            tags: ctx.tags.cloned(),
        }
    }

    /// Builder helper to override the label (used by test-webhook).
    pub fn with_label(mut self, label: String) -> Self {
        self.label = label;
        self
    }
}

#[cfg(test)]
impl AlertPayload {
    pub fn builder(
        label: impl Into<String>,
        contract_id: impl Into<String>,
        network: Network,
        rule_type: impl Into<String>,
        rule_triggered: impl Into<String>,
        transaction_hash: impl Into<String>,
    ) -> AlertPayloadBuilder {
        AlertPayloadBuilder {
            label: label.into(),
            contract_id: contract_id.into(),
            network,
            rule_type: rule_type.into(),
            rule_triggered: rule_triggered.into(),
            transaction_hash: transaction_hash.into(),
            function_name: None,
            function_names: Vec::new(),
            amount_xlm: None,
            amount_stroops: None,
            fee_charged_stroops: None,
            operation_count: None,
            memo_type: None,
            memo: None,
            timestamp: 0,
            timestamp_iso: "1970-01-01T00:00:00Z".into(),
            horizon_link: String::new(),
            explorer_link: String::new(),
            ledger_sequence: None,
            source_account: None,
        }
    }
}

#[cfg(test)]
#[must_use]
pub struct AlertPayloadBuilder {
    label: String,
    contract_id: String,
    network: Network,
    rule_type: String,
    rule_triggered: String,
    transaction_hash: String,
    function_name: Option<String>,
    function_names: Vec<String>,
    amount_xlm: Option<u64>,
    amount_stroops: Option<u64>,
    fee_charged_stroops: Option<u64>,
    operation_count: Option<u32>,
    memo_type: Option<String>,
    memo: Option<String>,
    timestamp: i64,
    timestamp_iso: String,
    horizon_link: String,
    explorer_link: String,
    ledger_sequence: Option<u32>,
    source_account: Option<String>,
}

#[cfg(test)]
impl AlertPayloadBuilder {
    pub fn function_name(mut self, name: impl Into<String>) -> Self {
        let n = name.into();
        self.function_name = Some(n.clone());
        if self.function_names.is_empty() {
            self.function_names.push(n);
        }
        self
    }

    pub fn function_names(mut self, names: Vec<String>) -> Self {
        self.function_names = names;
        self
    }

    pub fn amount_xlm(mut self, xlm: u64) -> Self {
        self.amount_xlm = Some(xlm);
        self
    }

    pub fn amount_stroops(mut self, stroops: u64) -> Self {
        self.amount_stroops = Some(stroops);
        self
    }

    pub fn fee_charged_stroops(mut self, fee: u64) -> Self {
        self.fee_charged_stroops = Some(fee);
        self
    }

    pub fn operation_count(mut self, count: u32) -> Self {
        self.operation_count = Some(count);
        self
    }

    pub fn memo(mut self, memo_type: impl Into<String>, memo: impl Into<String>) -> Self {
        self.memo_type = Some(memo_type.into());
        self.memo = Some(memo.into());
        self
    }

    pub fn timestamp(mut self, ts: i64, iso: impl Into<String>) -> Self {
        self.timestamp = ts;
        self.timestamp_iso = iso.into();
        self
    }

    pub fn horizon_link(mut self, link: impl Into<String>) -> Self {
        self.horizon_link = link.into();
        self
    }

    pub fn explorer_link(mut self, link: impl Into<String>) -> Self {
        self.explorer_link = link.into();
        self
    }

    pub fn ledger_sequence(mut self, seq: u32) -> Self {
        self.ledger_sequence = Some(seq);
        self
    }

    pub fn source_account(mut self, account: impl Into<String>) -> Self {
        self.source_account = Some(account.into());
        self
    }

    pub fn build(self) -> AlertPayload {
        AlertPayload {
            contract_type: ContractType::from_contract_id(&self.contract_id),
            label: self.label,
            contract_id: self.contract_id,
            network: self.network,
            rule_type: self.rule_type,
            rule_triggered: self.rule_triggered,
            transaction_hash: self.transaction_hash,
            function_name: self.function_name,
            function_names: self.function_names,
            amount_xlm: self.amount_xlm,
            amount_stroops: self.amount_stroops,
            fee_charged_stroops: self.fee_charged_stroops,
            operation_count: self.operation_count,
            memo_type: self.memo_type,
            memo: self.memo,
            timestamp: self.timestamp,
            timestamp_iso: self.timestamp_iso,
            horizon_link: self.horizon_link,
            explorer_link: self.explorer_link,
            ledger_sequence: self.ledger_sequence,
            source_account: self.source_account,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn time_window_evaluates_correctly_with_mocked_time() {
        use chrono::TimeZone;
        let tx = make_tx(true, &[], None);

        // Monday 10:30 UTC - should be inside "09:00-17:00" and ["Mon", "Tue"]
        let monday_morning = chrono::Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 0).unwrap();
        let res = eval_time_window(
            "09:00-17:00",
            &["Mon".into(), "Tue".into()],
            None,
            &tx,
            monday_morning,
        ).unwrap();
        assert!(res, "Monday 10:30 UTC should match business hours");

        // Monday 18:30 UTC - outside hours
        let monday_evening = chrono::Utc.with_ymd_and_hms(2024, 1, 15, 18, 30, 0).unwrap();
        let res_evening = eval_time_window(
            "09:00-17:00",
            &["Mon".into(), "Tue".into()],
            None,
            &tx,
            monday_evening,
        ).unwrap();
        assert!(!res_evening, "Monday 18:30 UTC should be outside business hours");

        // Sunday 10:30 UTC - wrong weekday
        let sunday = chrono::Utc.with_ymd_and_hms(2024, 1, 14, 10, 30, 0).unwrap();
        let res_sunday = eval_time_window(
            "09:00-17:00",
            &["Mon".into(), "Tue".into()],
            None,
            &tx,
            sunday,
        ).unwrap();
        assert!(!res_sunday, "Sunday should be suppressed");
    }

    // ── Issue #454: wrap_rule must not always fire ───────────────────────────

    /// Issue #454: for every `AlertRule` variant name used as a `wrap_rule`, a
    /// transaction crafted NOT to match that rule's condition must yield
    /// `Ok(false)` from `eval_time_window` — not the always-true fallthrough.
    #[test]
    fn time_window_wrap_rule_does_not_fire_when_inner_rule_does_not_match() {
        use chrono::TimeZone;

        // A transaction that matches none of the wrapped rule conditions:
        // successful, no functions, no amount, no fee, no source account,
        // no result code.
        let tx = make_tx(true, &[], None);

        // A time that is inside the window so the window itself does not
        // short-circuit to false — we want to exercise the wrap_rule matcher.
        let inside = chrono::Utc.with_ymd_and_hms(2024, 1, 15, 10, 30, 0).unwrap();

        let variants = [
            "AnyTransaction",
            "TransactionFailed",
            "InsufficientFunds",
            "SorobanInvocation",
            "LargeTransfer",
            "FunctionCalled",
            "AdminFunctionCalled",
            "HighFee",
            "SourceAccountMatch",
        ];

        for variant in variants {
            // AnyTransaction always matches by definition, so skip it here.
            if variant == "AnyTransaction" {
                continue;
            }
            let result = eval_time_window(
                "09:00-17:00",
                &[],
                Some(variant),
                &tx,
                inside,
            )
            .unwrap_or_else(|e| panic!("eval_time_window errored for {}: {}", variant, e));
            assert!(
                !result,
                "wrap_rule {:?} fired for a non-matching transaction (should be false)",
                variant
            );
        }
    }

    use txwatch_config::{AlertRule, Network};

    fn make_tx(
        successful: bool,
        function_names: &[&str],
        amount_stroops: Option<u64>,
    ) -> EnrichedTransaction {
        let function_names: Vec<String> = function_names.iter().map(|s| s.to_string()).collect();

        EnrichedTransaction {
            hash:           "abc123".into(),
            timestamp:      "2024-01-15T12:00:00Z".parse().unwrap(),
            successful,
            paging_token: "100".into(),
            function_names: function_names.iter().map(|s| s.to_string()).collect(),
            amount_stroops,
            fee_charged_stroops: None,
            operation_count: None,
            memo_type: None,
            memo: None,
        }
    }

    fn run(rules: &[AlertRule], tx: &EnrichedTransaction) -> Vec<AlertPayload> {
        evaluate(
            "Label",
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            &Network::Testnet,
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            rules,
            None,
            tx,
        )
    }

    /// Issue #272: a payload built by `evaluate` serializes to exactly the
    /// fields documented in the README webhook payload section.
    #[test]
    fn alert_payload_json_includes_all_documented_fields() {
        let tx = make_tx(true, &["transfer"], Some(150_000_000_000));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: 10_000 }], &tx);
        assert_eq!(payloads.len(), 1);

        let json = serde_json::to_value(&payloads[0]).unwrap();
        let object = json.as_object().unwrap();

        let documented = [
            "label",
            "contract_id",
            "contract_type",
            "network",
            "rule_type",
            "rule_triggered",
            "transaction_hash",
            "function_name",
            "function_names",
            "amount_xlm",
            "fee_charged_stroops",
            "timestamp",
            "timestamp_iso",
            "horizon_link",
            "explorer_link",
        ];
        for field in documented {
            assert!(object.contains_key(field), "missing documented field: {}", field);
        }
        assert_eq!(object.len(), documented.len(), "undocumented fields present: {:?}", object.keys());
    }

    // ── Issue #351: contract_type routing field ──────────────────────────────

    #[test]
    fn contract_type_serializes_as_soroban_for_c_address() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(
            payloads[0].contract_id,
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
        assert_eq!(payloads[0].contract_type, ContractType::Soroban);
        let json = serde_json::to_value(&payloads[0]).unwrap();
        assert_eq!(json["contract_type"], "soroban");
    }

    #[test]
    fn contract_type_serializes_as_classic_for_g_address() {
        let tx = make_tx(true, &[], None);
        let payloads = evaluate(
            "Label",
            "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            &Network::Testnet,
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            &[AlertRule::AnyTransaction],
            None,
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].contract_type, ContractType::Classic);
        let json = serde_json::to_value(&payloads[0]).unwrap();
        assert_eq!(json["contract_type"], "classic");
    }

    #[test]
    fn alert_payload_new_derives_contract_type_from_contract_id() {
        let tx = make_tx(true, &[], None);
        for (contract_id, expected) in [
            ("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", ContractType::Soroban),
            ("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", ContractType::Classic),
        ] {
            let ctx = PayloadContext {
                label: "Label",
                contract_id,
                network: "testnet",
                horizon_base: "https://horizon-testnet.stellar.org",
                explorer_base: "https://stellar.expert/explorer/testnet",
                tags: None,
            };
            let payload = AlertPayload::new(&ctx, &tx, &AlertRule::AnyTransaction);
            assert_eq!(payload.contract_type, expected);
        }
    }

    #[test]
    fn any_transaction_always_fires() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction