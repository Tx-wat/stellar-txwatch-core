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
use txwatch_config::AlertRule;

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
                if let Ok(xlm) = amt_str.parse::<f64>() {
                    total_stroops = total_stroops.saturating_add((xlm * 10_000_000.0) as u64);
                    has_payment = true;
                }
            }
        }
    }

    (function_names, if has_payment { Some(total_stroops) } else { None })
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
    })
}

impl AlertPayload {
    /// Build the webhook payload for `rule` matching `tx` on the contract described by `ctx`.
    pub fn new(ctx: &PayloadContext<'_>, tx: &EnrichedTransaction, rule: &AlertRule) -> Self {
        let horizon_base = ctx.horizon_base.trim_end_matches('/');
        let explorer_base = ctx.explorer_base.trim_end_matches('/');
        Self {
            label: ctx.label.to_string(),
            contract_id: ctx.contract_id.to_string(),
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Datelike;
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

    #[test]
    fn any_transaction_always_fires() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction");
    }

    #[test]
    fn any_transaction_fires_on_failed_transaction() {
        let tx = make_tx(false, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction");
    }

    #[test]
    fn alert_payload_new_populates_fields_from_context_and_transaction() {
        let ctx = PayloadContext {
            label: "Label",
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            network: "testnet",
            horizon_base: "https://horizon-testnet.stellar.org",
            explorer_base: "https://stellar.expert/explorer/testnet",
            tags: None,
        };
        let mut tx = make_tx(true, &["transfer", "withdraw"], Some(15_000 * 10_000_000));
        tx.fee_charged_stroops = Some(500);

        let payload = AlertPayload::new(&ctx, &tx, &AlertRule::AnyTransaction);

        assert_eq!(payload.label, "Label");
        assert_eq!(payload.network, "testnet");
        assert_eq!(payload.rule_type, "AnyTransaction");
        assert_eq!(payload.rule_triggered, "AnyTransaction");
        assert_eq!(payload.transaction_hash, "abc123");
        assert_eq!(payload.function_name.as_deref(), Some("transfer"));
        assert_eq!(payload.function_names, vec!["transfer", "withdraw"]);
        assert_eq!(payload.amount_xlm, Some(15_000));
        assert_eq!(payload.fee_charged_stroops, Some(500));
        assert_eq!(payload.timestamp_iso, "2024-01-15T12:00:00Z");
        assert_eq!(
            payload.horizon_link,
            "https://horizon-testnet.stellar.org/transactions/abc123"
        );
        assert_eq!(
            payload.explorer_link,
            "https://stellar.expert/explorer/testnet/tx/abc123"
        );
    }

    #[test]
    fn evaluate_forwards_contract_tags_to_every_payload() {
        let tags = HashMap::from([
            ("team".to_string(), "defi".to_string()),
            ("severity".to_string(), "high".to_string()),
        ]);
        let tx = make_tx(false, &[], None);
        let payloads = evaluate(
            "Label",
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "testnet",
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            &[AlertRule::AnyTransaction, AlertRule::TransactionFailed],
            Some(&tags),
            &tx,
        );
        assert_eq!(payloads.len(), 2);
        for payload in &payloads {
            assert_eq!(payload.tags.as_ref(), Some(&tags));
        }
    }

    #[test]
    fn evaluate_without_tags_yields_none() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads[0].tags, None);
    }

    #[test]
    fn rule_label_formats_are_stable() {
        assert_eq!(AlertRule::AnyTransaction.label(), "AnyTransaction");
        assert_eq!(AlertRule::TransactionFailed.label(), "TransactionFailed");
        assert_eq!(AlertRule::LargeTransfer { threshold_xlm: 10_000 }.label(), "LargeTransfer(>=10000XLM)");
        assert_eq!(AlertRule::FunctionCalled { function_name: "withdraw".into() }.label(), "FunctionCalled(withdraw)");
        assert_eq!(AlertRule::AdminFunctionCalled { function_names: vec!["set_admin".into(), "upgrade".into()] }.label(), "AdminFunctionCalled([set_admin, upgrade])");
        assert_eq!(AlertRule::HighFee { threshold_stroops: 10_000, threshold_xlm: None }.label(), "HighFee(>=10000 stroops)");
    }

    #[test]
    fn transaction_failed_fires_on_failure() {
        let tx = make_tx(false, &[], None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert_eq!(payloads.len(), 1);
    }

    // ── Issue #280: InsufficientFunds ────────────────────────────────────────

    /// Base64 `TransactionResult` for txINSUFFICIENT_BALANCE: fee 100 stroops,
    /// result code -7, ext 0. Hand-encoded from the XDR spec, not captured from testnet.
    const INSUFFICIENT_BALANCE_XDR: &str = "AAAAAAAAAGT////5AAAAAA==";
    /// txFAILED (-1) with an empty operation-result array.
    const TX_FAILED_XDR: &str = "AAAAAAAAAGT/////AAAAAAAAAAA=";
    /// txBAD_SEQ (-5).
    const BAD_SEQ_XDR: &str = "AAAAAAAAAGT////7AAAAAA==";

    fn make_tx_with_result(successful: bool, result_xdr: &str) -> EnrichedTransaction {
        let raw = horizon_tx_with_result(successful, result_xdr);
        EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap()
    }

    fn horizon_tx_with_result(successful: bool, result_xdr: &str) -> HorizonTransaction {
        HorizonTransaction {
            hash: "insufficient".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful,
            paging_token: "1".into(),
            fee_charged: Some("100".into()),
            envelope_xdr: None,
            result_xdr: Some(result_xdr.into()),
        }
    }

    #[test]
    fn insufficient_funds_fires_on_insufficient_balance_result() {
        let tx = make_tx_with_result(false, INSUFFICIENT_BALANCE_XDR);
        assert_eq!(tx.result_code, Some(TX_INSUFFICIENT_BALANCE));
        let payloads = run(&[AlertRule::InsufficientFunds], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "InsufficientFunds");
        assert_eq!(payloads[0].rule_type, "InsufficientFunds");
    }

    #[test]
    fn insufficient_funds_does_not_fire_on_other_failure_codes() {
        for xdr in [TX_FAILED_XDR, BAD_SEQ_XDR] {
            let tx = make_tx_with_result(false, xdr);
            let payloads = run(&[AlertRule::InsufficientFunds], &tx);
            assert!(payloads.is_empty(), "should not fire for {}", xdr);
        }
    }

    #[test]
    fn insufficient_funds_does_not_fire_on_success() {
        let tx = make_tx_with_result(true, INSUFFICIENT_BALANCE_XDR);
        let payloads = run(&[AlertRule::InsufficientFunds], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn insufficient_funds_does_not_fire_without_result_xdr() {
        let tx = make_tx(false, &[], None);
        let payloads = run(&[AlertRule::InsufficientFunds], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn transaction_result_code_decodes_known_samples() {
        assert_eq!(transaction_result_code(INSUFFICIENT_BALANCE_XDR), Some(-7));
        assert_eq!(transaction_result_code(TX_FAILED_XDR), Some(-1));
        assert_eq!(transaction_result_code(BAD_SEQ_XDR), Some(-5));
    }

    #[test]
    fn transaction_result_code_returns_none_for_bad_input() {
        assert_eq!(transaction_result_code("%%% not base64 %%%"), None);
        assert_eq!(transaction_result_code("AAAAAAAAAGQ="), None); // 8 bytes only
        assert_eq!(transaction_result_code(""), None);
    }

    // ── Issue #281: SorobanInvocation ────────────────────────────────────────

    #[test]
    fn soroban_invocation_fires_with_one_function_name() {
        let tx = make_tx(true, &["deposit"], None);
        let payloads = run(&[AlertRule::SorobanInvocation], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "SorobanInvocation");
        assert_eq!(payloads[0].rule_type, "SorobanInvocation");
    }

    #[test]
    fn soroban_invocation_fires_with_multiple_function_names() {
        let tx = make_tx(true, &["deposit", "withdraw"], None);
        let payloads = run(&[AlertRule::SorobanInvocation], &tx);
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn soroban_invocation_does_not_fire_for_native_only_transaction() {
        let tx = make_tx(true, &[], Some(100_000_000));
        let payloads = run(&[AlertRule::SorobanInvocation], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn transaction_failed_does_not_fire_on_success() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_fires_at_threshold() {
        // exactly 10_000 XLM = 100_000_000_000 stroops
        let tx = make_tx(true, &[], Some(100_000_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer { threshold_xlm: Some(10_000), threshold_stroops: None }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(10_000));
    }

    #[test]
    fn large_transfer_does_not_fire_below_threshold() {
        let tx = make_tx(true, &[], Some(9_999 * 10_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer { threshold_xlm: Some(10_000), threshold_stroops: None }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_no_amount_does_not_fire() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: Some(1), threshold_stroops: None }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_overflow_is_handled_gracefully() {
        let tx = make_tx(true, &[], Some(1_000_000_000_000_000));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: Some(u64::MAX), threshold_stroops: None }], &tx);
        assert!(payloads.is_empty(), "overflowing LargeTransfer thresholds should not panic");
    }

    #[test]
    fn large_transfer_payload_keeps_sub_xlm_precision() {
        let tx = make_tx(true, &[], Some(5_000_001));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: None, threshold_stroops: Some(5_000_000) }], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(0));
        assert_eq!(payloads[0].amount_stroops, Some(5_000_001));
    }

    #[test]
    fn large_transfer_fires_at_exact_threshold() {
        let tx = make_tx(true, &[], Some(10_000 * 10_000_000));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: Some(10_000), threshold_stroops: None }], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(10_000));
    }

    #[test]
    fn large_transfer_does_not_fire_one_stroop_below_threshold() {
        let tx = make_tx(true, &[], Some(10_000 * 10_000_000 - 1));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: Some(10_000), threshold_stroops: None }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn function_called_fires_on_match() {
        let tx = make_tx(true, &["withdraw"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].function_name.as_deref(), Some("withdraw"));
    }

    #[test]
    fn function_called_does_not_fire_on_mismatch() {
        let tx = make_tx(true, &["deposit"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn function_called_is_case_sensitive() {
        let tx = make_tx(true, &["withdraw"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "Withdraw".into(),
            }],
            &tx,
        );
        assert!(
            payloads.is_empty(),
            "FunctionCalled must not match a different casing"
        );
    }

    #[test]
    fn explicit_fee_takes_precedence_over_record_fee() {
        let raw = |fee: Option<&str>| HorizonTransaction {
            hash: "h".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: fee.map(Into::into),
            envelope_xdr: None,
            result_xdr: None,
        };

        let explicit = EnrichedTransaction::from_horizon(raw(Some("100")), vec![], None, Some(999)).unwrap();
        assert_eq!(explicit.fee_charged_stroops, Some(999));

        let fallback = EnrichedTransaction::from_horizon(raw(Some("100")), vec![], None, None).unwrap();
        assert_eq!(fallback.fee_charged_stroops, Some(100));
    }

    #[test]
    fn admin_function_called_fires_on_any_match() {
        let tx = make_tx(true, &["upgrade"], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].rule_triggered.contains("upgrade"));
    }

    #[test]
    fn function_called_does_not_fire_when_function_name_is_none() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_does_not_fire_when_function_name_is_none() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn multiple_rules_can_fire_on_same_tx() {
        let tx = make_tx(false, &["set_admin"], Some(200_000_000_000));
        let rules = vec![
            AlertRule::AnyTransaction,
            AlertRule::TransactionFailed,
            AlertRule::LargeTransfer { threshold_xlm: Some(10_000), threshold_stroops: None },
            AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into()],
            },
        ];
        let payloads = run(&rules, &tx);
        assert_eq!(payloads.len(), 4);
    }

    #[test]
    fn horizon_link_is_correct() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(
            payloads[0].horizon_link,
            "https://horizon-testnet.stellar.org/transactions/abc123"
        );
    }

    #[test]
    fn url_fields_have_no_trailing_slash_and_exact_format() {
        // Verify both link fields are normalised even when base URLs have trailing slashes.
        fn run_with_bases(horizon_base: &str, explorer_base: &str) -> AlertPayload {
            let tx = EnrichedTransaction {
                hash: "deadbeef".into(),
                timestamp: "2024-01-15T12:00:00Z".parse().unwrap(),
                successful: true,
                paging_token: "1".into(),
                function_names: vec![],
                amount_stroops: None,
                fee_charged_stroops: None,
                operation_count: None,
                memo_type: None,
                memo: None,
            };
            let mut payloads = evaluate(
                "L", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "testnet", horizon_base, explorer_base,
                &[AlertRule::AnyTransaction], None, &tx,
            );
            payloads.remove(0)
        }

        // Without trailing slash — baseline
        let p = run_with_bases(
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
        );
        assert_eq!(p.horizon_link,  "https://horizon-testnet.stellar.org/transactions/deadbeef");
        assert_eq!(p.explorer_link, "https://stellar.expert/explorer/testnet/tx/deadbeef");

        // With trailing slash — must produce identical output
        let p2 = run_with_bases(
            "https://horizon-testnet.stellar.org/",
            "https://stellar.expert/explorer/testnet/",
        );
        assert_eq!(p.horizon_link,  p2.horizon_link);
        assert_eq!(p.explorer_link, p2.explorer_link);
    }

    #[test]
    fn alert_payload_new_normalises_trailing_slashes() {
        let tx = make_tx(true, &[], None);
        let ctx = PayloadContext {
            label: "TestContract",
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            network: "testnet",
            horizon_base: "https://horizon-testnet.stellar.org/",
            explorer_base: "https://stellar.expert/explorer/testnet/",
            tags: None,
        };
        let p = AlertPayload::new(&ctx, &tx, &AlertRule::AnyTransaction);
        assert_eq!(p.horizon_link, format!("https://horizon-testnet.stellar.org/transactions/{}", tx.hash));
        assert_eq!(p.explorer_link, format!("https://stellar.expert/explorer/testnet/tx/{}", tx.hash));
    }

    #[test]
    fn large_transfer_stroops_fires_at_exact_threshold() {
        let tx = make_tx(true, &[], Some(5_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer { threshold_xlm: None, threshold_stroops: Some(5_000_000) }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "LargeTransfer(>=0.5XLM)");
    }

    #[test]
    fn large_transfer_stroops_does_not_fire_one_stroop_below_threshold() {
        let tx = make_tx(true, &[], Some(4_999_999));
        let payloads = run(
            &[AlertRule::LargeTransfer { threshold_xlm: None, threshold_stroops: Some(5_000_000) }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn high_fee_fires_at_threshold() {
        let mut tx = make_tx(true, &[], None);
        tx.fee_charged_stroops = Some(10_000);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 10_000,
                threshold_xlm:     None,
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].rule_triggered.contains("HighFee"));
    }

    #[test]
    fn high_fee_does_not_fire_below_threshold() {
        let mut tx = make_tx(true, &[], None);
        tx.fee_charged_stroops = Some(9_999);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 10_000,
                threshold_xlm:     None,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn high_fee_no_fee_does_not_fire() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 1,
                threshold_xlm:     None,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    // ── SourceAccountMatch (issue #303) ───────────────────────────────────────

    const SOURCE: &str = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const OTHER: &str = "GBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    fn source_rule(source_account: &str) -> AlertRule {
        AlertRule::SourceAccountMatch {
            source_account: source_account.into(),
        }
    }

    #[test]
    fn source_account_match_fires_on_exact_match() {
        let mut tx = make_tx(true, &[], None);
        tx.source_account = Some(SOURCE.into());
        let payloads = run(&[source_rule(SOURCE)], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_type, "SourceAccountMatch");
        assert_eq!(
            payloads[0].rule_triggered,
            format!("SourceAccountMatch({})", SOURCE)
        );
    }

    #[test]
    fn source_account_match_does_not_fire_on_different_account() {
        let mut tx = make_tx(true, &[], None);
        tx.source_account = Some(OTHER.into());
        let payloads = run(&[source_rule(SOURCE)], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn source_account_match_does_not_fire_when_source_account_is_missing() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[source_rule(SOURCE)], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn from_horizon_carries_source_account() {
        let raw = HorizonTransaction {
            hash: "h2".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
            source_account: Some(SOURCE.into()),
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.source_account.as_deref(), Some(SOURCE));
    }

    #[test]
    fn enriched_transaction_parses_timestamp() {
        let raw = HorizonTransaction {
            hash: "h1".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: Some("100".into()),
            operation_count: None,
            envelope_xdr: None,
            result_xdr: None,
            memo_type: None,
            memo: None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.timestamp.year(), 2024);
    }

    #[test]
    fn from_horizon_maps_memo_fields() {
        let raw = HorizonTransaction {
            hash: "h2".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "2".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
            memo_type: Some("text".into()),
            memo: Some("order-1234".into()),
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.memo_type.as_deref(), Some("text"));
        assert_eq!(enriched.memo.as_deref(), Some("order-1234"));

        let payloads = evaluate(
            "L", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "testnet", "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            &[AlertRule::AnyTransaction], &enriched,
        );
        assert_eq!(payloads[0].memo_type.as_deref(), Some("text"));
        assert_eq!(payloads[0].memo.as_deref(), Some("order-1234"));
    }

    #[test]
    fn horizon_transaction_memo_fields_default_to_none_when_absent() {
        let json = r#"{"hash":"h","created_at":"2024-06-01T00:00:00Z","successful":true,"paging_token":"1"}"#;
        let tx: HorizonTransaction = serde_json::from_str(json).unwrap();
        assert!(tx.memo_type.is_none());
        assert!(tx.memo.is_none());
    }

    /// Issue #65: from_horizon must return Err when created_at is not a valid RFC 3339 timestamp.
    #[test]
    fn from_horizon_rejects_invalid_timestamp() {
        let raw = HorizonTransaction {
            hash:         "badhash".into(),
            created_at:   "not-a-timestamp".into(),
            successful:   true,
            paging_token: "1".into(),
            fee_charged:  None,
            operation_count: None,
            envelope_xdr: None,
            result_xdr:   None,
            ledger: None,
            source_account: None,
        };
        let result = EnrichedTransaction::from_horizon(raw, vec![], None, None);
        assert!(result.is_err(), "expected Err for invalid timestamp");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("cannot parse timestamp"),
            "error message should mention 'cannot parse timestamp', got: {}",
            msg
        );
    }

    // ── Issue #77: multiple invoke_host_function ops ──────────────────────────

    #[test]
    fn function_called_fires_when_matching_name_is_second_in_list() {
        // Transaction has two Soroban invocations; rule should match the second
        let tx = make_tx(true, &["deposit", "withdraw"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].function_names, vec!["deposit", "withdraw"]);
    }

    #[test]
    fn function_called_does_not_fire_when_no_names_match() {
        let tx = make_tx(true, &["deposit", "transfer"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_fires_on_any_of_multiple_invocations() {
        // Two invocations; only the second is an admin function
        let tx = make_tx(true, &["transfer", "set_admin"], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn payload_function_names_contains_all_invocations() {
        let tx = make_tx(true, &["foo", "bar", "baz"], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads[0].function_names, vec!["foo", "bar", "baz"]);
        // function_name (singular) is the first for backward compat
        assert_eq!(payloads[0].function_name.as_deref(), Some("foo"));
    }

    /// Issue #265: operation_count is populated from the Horizon transaction record
    /// and propagated through EnrichedTransaction into AlertPayload.
    #[test]
    fn operation_count_is_populated_from_horizon_response() {
        let raw = HorizonTransaction {
            hash: "op_count_tx".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: Some("100".into()),
            operation_count: Some(5),
            envelope_xdr: None,
            result_xdr: None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.operation_count, Some(5));

        let payloads = run(&[AlertRule::AnyTransaction], &enriched);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].operation_count, Some(5));
    }

    /// Issue #265: operation_count is None when Horizon omits the field.
    #[test]
    fn operation_count_is_none_when_absent_from_horizon() {
        let raw = HorizonTransaction {
            hash: "no_op_count_tx".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            operation_count: None,
            envelope_xdr: None,
            result_xdr: None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.operation_count, None);

        let payloads = run(&[AlertRule::AnyTransaction], &enriched);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].operation_count, None);
    }

    #[test]
    fn alert_payload_network_keeps_lowercase_wire_format() {
        for (network, wire) in [
            (Network::Mainnet, "mainnet"),
            (Network::Testnet, "testnet"),
            (Network::Futurenet, "futurenet"),
        ] {
            let payload = AlertPayload {
                label: "x".into(),
                contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                network,
                rule_type: "AnyTransaction".into(),
                rule_triggered: "AnyTransaction".into(),
                transaction_hash: "abc".into(),
                function_name: None,
                function_names: vec![],
                amount_xlm: None,
                fee_charged_stroops: None,
                timestamp: 0,
                timestamp_iso: "1970-01-01T00:00:00Z".into(),
                horizon_link: String::new(),
                explorer_link: String::new(),
            };
            let json: serde_json::Value = serde_json::to_value(&payload).unwrap();
            assert_eq!(json["network"], wire);
            let round_tripped: AlertPayload = serde_json::from_value(json).unwrap();
            assert_eq!(round_tripped, payload);
        }
    }

    #[test]
    fn alert_payload_serialises_to_valid_json_with_all_fields_present() {
        let payload = AlertPayload {
            label: "My Contract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: Network::Testnet,
            rule_type: "LargeTransfer".into(),
            rule_triggered: "LargeTransfer(>=10000XLM)".into(),
            transaction_hash: "abc123".into(),
            function_name: Some("transfer".into()),
            function_names: vec!["transfer".into()],
            amount_xlm: Some(15000),
            amount_stroops: Some(150_000_000_000),
            fee_charged_stroops: Some(50000),
            operation_count: Some(3),
            memo_type: Some("text".into()),
            memo: Some("order-1234".into()),
            timestamp: 1705316096,
            timestamp_iso: "2024-01-15T12:00:00Z".into(),
            horizon_link: "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link: "https://stellar.expert/explorer/testnet/tx/abc123".into(),
            ledger_sequence: Some(12345678),
            source_account: Some("GABC123".into()),
        };

        let json = serde_json::to_value(payload).expect("serialize AlertPayload to JSON");
        let obj = json.as_object().expect("AlertPayload should serialize to a JSON object");

        assert_eq!(obj["label"].as_str(), Some("My Contract"));
        assert_eq!(obj["contract_id"].as_str(), Some("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert_eq!(obj["network"].as_str(), Some("testnet"));
        assert_eq!(obj["rule_type"].as_str(), Some("LargeTransfer"));
        assert_eq!(obj["rule_triggered"].as_str(), Some("LargeTransfer(>=10000XLM)"));
        assert_eq!(obj["transaction_hash"].as_str(), Some("abc123"));
        assert_eq!(obj["function_name"].as_str(), Some("transfer"));
        assert_eq!(obj["function_names"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(obj["amount_xlm"].as_u64(), Some(15000));
        assert_eq!(obj["amount_stroops"].as_u64(), Some(150_000_000_000));
        assert_eq!(obj["fee_charged_stroops"].as_u64(), Some(50000));
        assert_eq!(obj["operation_count"].as_u64(), Some(3));
        assert_eq!(obj["timestamp"].as_i64(), Some(1705316096));
        assert_eq!(obj["timestamp_iso"].as_str(), Some("2024-01-15T12:00:00Z"));
        assert_eq!(obj["horizon_link"].as_str(), Some("https://horizon-testnet.stellar.org/transactions/abc123"));
        assert_eq!(obj["explorer_link"].as_str(), Some("https://stellar.expert/explorer/testnet/tx/abc123"));
        assert_eq!(obj["ledger_sequence"].as_u64(), Some(12345678));
        assert_eq!(obj["source_account"].as_str(), Some("GABC123"));
    }

    // ── Issue #263: source_account propagation ────────────────────────────────

    #[test]
    fn source_account_is_propagated_to_alert_payload() {
        let raw = HorizonTransaction {
            hash: "src_test".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
            source_account: Some("GABC1234567890".into()),
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.source_account.as_deref(), Some("GABC1234567890"));

        let payloads = run(&[AlertRule::AnyTransaction], &EnrichedTransaction {
            source_account: Some("GABC1234567890".into()),
            ..make_tx(true, &[], None)
        });
        assert_eq!(payloads[0].source_account.as_deref(), Some("GABC1234567890"));
    }

    #[test]
    fn source_account_is_null_when_not_provided() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert!(payloads[0].source_account.is_none());
    }

    // ── Issue #264: ledger_sequence propagation ───────────────────────────────

    #[test]
    fn ledger_sequence_is_propagated_to_alert_payload() {
        let raw = HorizonTransaction {
            hash: "ledger_test".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: Some(99887766),
            source_account: None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.ledger_sequence, Some(99887766));

        let payloads = run(&[AlertRule::AnyTransaction], &EnrichedTransaction {
            ledger_sequence: Some(99887766),
            ..make_tx(true, &[], None)
        });
        assert_eq!(payloads[0].ledger_sequence, Some(99887766));
    }

    #[test]
    fn ledger_sequence_is_null_when_not_provided() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert!(payloads[0].ledger_sequence.is_none());
    }

    // ── #253: extract_soroban_details unit tests ─────────────────────────────

    fn make_op(op_type: &str, function: Option<&str>, amount: Option<&str>) -> HorizonOperation {
        HorizonOperation {
            op_type: op_type.to_string(),
            function: function.map(|s| s.to_string()),
            amount: amount.map(|s| s.to_string()),
        }
    }

    #[test]
    fn extract_returns_function_name_from_invoke_host_function_op() {
        let ops = vec![make_op("invoke_host_function", Some("withdraw"), None)];
        let (names, amount) = extract_soroban_details(ops);
        assert_eq!(names, vec!["withdraw"]);
        assert!(amount.is_none());
    }

    #[test]
    fn extract_returns_multiple_function_names_from_multiple_ops() {
        let ops = vec![
            make_op("invoke_host_function", Some("deposit"), None),
            make_op("invoke_host_function", Some("withdraw"), None),
        ];
        let (names, amount) = extract_soroban_details(ops);
        assert_eq!(names, vec!["deposit", "withdraw"]);
        assert!(amount.is_none());
    }

    #[test]
    fn extract_ignores_invoke_host_function_op_with_no_function_field() {
        let ops = vec![make_op("invoke_host_function", None, None)];
        let (names, amount) = extract_soroban_details(ops);
        assert!(names.is_empty());
        assert!(amount.is_none());
    }

    #[test]
    fn extract_returns_payment_amount_in_stroops() {
        // 1000.0000000 XLM = 10_000_000_000 stroops
        let ops = vec![make_op("payment", None, Some("1000.0000000"))];
        let (names, amount) = extract_soroban_details(ops);
        assert!(names.is_empty());
        assert_eq!(amount, Some(10_000_000_000));
    }

    #[test]
    fn extract_sums_multiple_payment_ops() {
        let ops = vec![
            make_op("payment", None, Some("500.0000000")),
            make_op("payment", None, Some("500.0000000")),
        ];
        let (_, amount) = extract_soroban_details(ops);
        assert_eq!(amount, Some(10_000_000_000));
    }

    #[test]
    fn extract_returns_none_amount_when_no_payment_ops() {
        let ops = vec![make_op("invoke_host_function", Some("transfer"), None)];
        let (_, amount) = extract_soroban_details(ops);
        assert!(amount.is_none());
    }

    #[test]
    fn extract_handles_empty_ops_list() {
        let (names, amount) = extract_soroban_details(vec![]);
        assert!(names.is_empty());
        assert!(amount.is_none());
    }

    #[test]
    fn extract_ignores_unknown_op_types() {
        let ops = vec![make_op("create_account", None, None)];
        let (names, amount) = extract_soroban_details(ops);
        assert!(names.is_empty());
        assert!(amount.is_none());
    }

    #[test]
    fn extract_combines_function_names_and_payment_in_same_tx() {
        let ops = vec![
            make_op("invoke_host_function", Some("transfer"), None),
            make_op("payment", None, Some("200.0000000")),
        ];
        let (names, amount) = extract_soroban_details(ops);
        assert_eq!(names, vec!["transfer"]);
        assert_eq!(amount, Some(2_000_000_000));
    }
}
