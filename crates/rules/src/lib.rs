use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use txwatch_config::AlertRule;

// ── Horizon transaction shape ─────────────────────────────────────────────────

/// Raw Horizon transaction record as returned by the REST API.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonTransaction {
    pub hash:         String,
    pub created_at:   String,   // RFC 3339
    pub successful:   bool,
    pub paging_token: String,

    /// Base64-encoded XDR transaction envelope.
    pub envelope_xdr: Option<String>,

    /// Base64-encoded XDR transaction result.
    pub result_xdr:   Option<String>,
}

// ── Enriched transaction ──────────────────────────────────────────────────────

/// A transaction enriched with Soroban-specific fields extracted from the
/// Horizon `operations` sub-resource JSON (returned inline via `join=operations`
/// or fetched separately). We keep this as a plain struct so rule evaluation
/// stays pure and testable without network calls.
#[derive(Debug, Clone)]
pub struct EnrichedTransaction {
    pub hash:          String,
    pub timestamp:     DateTime<Utc>,
    pub successful:    bool,
    pub paging_token:  String,
    /// Soroban contract function that was invoked, if any.
    pub function_name: Option<String>,
    /// Transfer amount in stroops (1 XLM = 10_000_000 stroops), if detected.
    pub amount_stroops: Option<u64>,
    /// Fee charged for this transaction in stroops.
    pub fee_charged_stroops: Option<u64>,
}

impl EnrichedTransaction {
    /// Build from a raw Horizon record plus optional Soroban operation details.
    pub fn from_horizon(
        tx: HorizonTransaction,
        function_name: Option<String>,
        amount_stroops: Option<u64>,
        fee_charged_stroops: Option<u64>,
    ) -> Result<Self> {
        let timestamp = tx
            .created_at
            .parse::<DateTime<Utc>>()
            .with_context(|| {
                format!("cannot parse timestamp '{}' for tx {}", tx.created_at, tx.hash)
            })?;

        Ok(Self {
            hash: tx.hash,
            timestamp,
            successful: tx.successful,
            paging_token: tx.paging_token,
            function_name,
            amount_stroops,
            fee_charged_stroops,
        })
    }
}

// ── AlertPayload ──────────────────────────────────────────────────────────────

/// The JSON body POSTed to the webhook URL when a rule fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertPayload {
    pub label:            String,
    pub contract_id:      String,
    pub network:          String,
    pub rule_triggered:   String,
    pub transaction_hash: String,
    pub function_name:    Option<String>,
    /// Amount in whole XLM (stroops / 10_000_000), present for LargeTransfer.
    pub amount_xlm:       Option<u64>,
    /// Unix timestamp (seconds).
    pub timestamp:        i64,
    pub horizon_link:     String,
    /// Stellar Expert explorer link for the transaction.
    pub explorer_link:    String,
}

// ── Rule evaluation ───────────────────────────────────────────────────────────

/// Evaluate all rules for one contract against one transaction.
/// Returns one `AlertPayload` per matching rule.
/// Never panics — errors in individual rule evaluation are logged and skipped.
pub fn evaluate(
    label:         &str,
    contract_id:   &str,
    network:       &str,
    horizon_base:  &str,
    explorer_base: &str,
    rules:         &[AlertRule],
    tx:            &EnrichedTransaction,
) -> Vec<AlertPayload> {
    let horizon_link  = format!("{}/transactions/{}", horizon_base, tx.hash);
    let explorer_link = format!("{}/tx/{}", explorer_base, tx.hash);
    let timestamp     = tx.timestamp.timestamp();

    rules
        .iter()
        .filter_map(|rule| {
            match eval_rule(rule, tx) {
                Ok(true) => Some(AlertPayload {
                    label:            label.to_string(),
                    contract_id:      contract_id.to_string(),
                    network:          network.to_string(),
                    rule_triggered:   rule_label(rule),
                    transaction_hash: tx.hash.clone(),
                    function_name:    tx.function_name.clone(),
                    amount_xlm:       tx.amount_stroops.map(|s| s / 10_000_000),
                    timestamp,
                    horizon_link:     horizon_link.clone(),
                    explorer_link:    explorer_link.clone(),
                }),
                Ok(false) => None,
                Err(e) => {
                    tracing::warn!(
                        tx = %tx.hash,
                        rule = %rule_label(rule),
                        error = %e,
                        "rule evaluation error — skipping"
                    );
                    None
                }
            }
        })
        .collect()
}

fn eval_rule(rule: &AlertRule, tx: &EnrichedTransaction) -> Result<bool> {
    Ok(match rule {
        AlertRule::AnyTransaction => true,

        AlertRule::TransactionFailed => !tx.successful,

        AlertRule::LargeTransfer { threshold_xlm } => {
            let threshold_stroops = threshold_xlm
                .checked_mul(10_000_000)
                .context("threshold_xlm overflow when converting to stroops")?;
            tx.amount_stroops
                .map(|s| s >= threshold_stroops)
                .unwrap_or(false)
        }

        AlertRule::FunctionCalled { function_name } => tx
            .function_name
            .as_deref()
            .map(|f| f == function_name.as_str())
            .unwrap_or(false),

        AlertRule::AdminFunctionCalled { function_names } => tx
            .function_name
            .as_deref()
            .map(|f| function_names.iter().any(|n| n == f))
            .unwrap_or(false),

        AlertRule::HighFee { threshold_stroops } => tx
            .fee_charged_stroops
            .map(|f| f >= *threshold_stroops)
            .unwrap_or(false),
    })
}

fn rule_label(rule: &AlertRule) -> String {
    match rule {
        AlertRule::AnyTransaction                          => "AnyTransaction".into(),
        AlertRule::TransactionFailed                       => "TransactionFailed".into(),
        AlertRule::LargeTransfer { threshold_xlm }        => format!("LargeTransfer(>={}XLM)", threshold_xlm),
        AlertRule::FunctionCalled { function_name }       => format!("FunctionCalled({})", function_name),
        AlertRule::AdminFunctionCalled { function_names } => {
            format!("AdminFunctionCalled([{}])", function_names.join(", "))
        }
        AlertRule::HighFee { threshold_stroops } => format!("HighFee(>={} stroops)", threshold_stroops),
    }
}

impl AlertPayload {
    /// Builder helper to override the label (used by test-webhook).
    pub fn with_label(mut self, label: String) -> Self {
        self.label = label;
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use txwatch_config::AlertRule;

    fn make_tx(
        successful: bool,
        function_name: Option<&str>,
        amount_stroops: Option<u64>,
    ) -> EnrichedTransaction {
        EnrichedTransaction {
            hash:                "abc123".into(),
            timestamp:           "2024-01-15T12:00:00Z".parse().unwrap(),
            successful,
            paging_token:        "100".into(),
            function_name:       function_name.map(str::to_string),
            amount_stroops,
            fee_charged_stroops: None,
        }
    }

    fn run(rules: &[AlertRule], tx: &EnrichedTransaction) -> Vec<AlertPayload> {
        evaluate("Label", "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                 "testnet", "https://horizon-testnet.stellar.org",
                 "https://stellar.expert/explorer/testnet", rules, tx)
    }

    #[test]
    fn any_transaction_always_fires() {
        let tx = make_tx(true, None, None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction");
    }

    #[test]
    fn transaction_failed_fires_on_failure() {
        let tx = make_tx(false, None, None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn transaction_failed_does_not_fire_on_success() {
        let tx = make_tx(true, None, None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_fires_at_threshold() {
        // exactly 10_000 XLM = 100_000_000_000 stroops
        let tx = make_tx(true, None, Some(100_000_000_000));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: 10_000 }], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(10_000));
    }

    #[test]
    fn large_transfer_does_not_fire_below_threshold() {
        let tx = make_tx(true, None, Some(9_999 * 10_000_000));
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: 10_000 }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_no_amount_does_not_fire() {
        let tx = make_tx(true, None, None);
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: 1 }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn function_called_fires_on_match() {
        let tx = make_tx(true, Some("withdraw"), None);
        let payloads = run(
            &[AlertRule::FunctionCalled { function_name: "withdraw".into() }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].function_name.as_deref(), Some("withdraw"));
    }

    #[test]
    fn function_called_does_not_fire_on_mismatch() {
        let tx = make_tx(true, Some("deposit"), None);
        let payloads = run(
            &[AlertRule::FunctionCalled { function_name: "withdraw".into() }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_fires_on_any_match() {
        let tx = make_tx(true, Some("upgrade"), None);
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
    fn multiple_rules_can_fire_on_same_tx() {
        let tx = make_tx(false, Some("set_admin"), Some(200_000_000_000));
        let rules = vec![
            AlertRule::AnyTransaction,
            AlertRule::TransactionFailed,
            AlertRule::LargeTransfer { threshold_xlm: 10_000 },
            AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into()],
            },
        ];
        let payloads = run(&rules, &tx);
        assert_eq!(payloads.len(), 4);
    }

    #[test]
    fn horizon_link_is_correct() {
        let tx = make_tx(true, None, None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(
            payloads[0].horizon_link,
            "https://horizon-testnet.stellar.org/transactions/abc123"
        );
    }

    #[test]
    fn high_fee_fires_at_threshold() {
        let mut tx = make_tx(true, None, None);
        tx.fee_charged_stroops = Some(10_000);
        let payloads = run(&[AlertRule::HighFee { threshold_stroops: 10_000 }], &tx);
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].rule_triggered.contains("HighFee"));
    }

    #[test]
    fn high_fee_does_not_fire_below_threshold() {
        let mut tx = make_tx(true, None, None);
        tx.fee_charged_stroops = Some(9_999);
        let payloads = run(&[AlertRule::HighFee { threshold_stroops: 10_000 }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn high_fee_no_fee_does_not_fire() {
        let tx = make_tx(true, None, None);
        let payloads = run(&[AlertRule::HighFee { threshold_stroops: 1 }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn enriched_transaction_parses_timestamp() {
        let raw = HorizonTransaction {
            hash:         "h1".into(),
            created_at:   "2024-06-01T00:00:00Z".into(),
            successful:   true,
            paging_token: "1".into(),
            envelope_xdr: None,
            result_xdr:   None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, None, None, None).unwrap();
        assert_eq!(enriched.timestamp.year(), 2024);
    }
}

// bring chrono::Datelike into scope for the test above
#[cfg(test)]
use chrono::Datelike;
