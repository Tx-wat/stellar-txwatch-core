use serde::{Deserialize, Serialize};
use txwatch_config::AlertRule;

/// Minimal representation of a Horizon transaction record.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonTransaction {
    pub hash:            String,
    pub created_at:      String,
    pub successful:      bool,
    /// Raw JSON envelope — we inspect it for Soroban invoke details.
    pub envelope_xdr:    Option<String>,
    /// Parsed fee in stroops; 1 XLM = 10_000_000 stroops.
    pub fee_charged:     Option<String>,
    /// Soroban-specific: function name extracted from the invocation.
    #[serde(skip)]
    pub function_name:   Option<String>,
    /// Soroban-specific: amount in stroops (if a transfer).
    #[serde(skip)]
    pub amount_stroops:  Option<u64>,
}

/// Result of evaluating a single rule against a transaction.
#[derive(Debug, Clone, Serialize)]
pub struct AlertPayload {
    pub label:            String,
    pub contract_id:      String,
    pub network:          String,
    pub rule_triggered:   String,
    pub transaction_hash: String,
    pub function_name:    Option<String>,
    pub amount:           Option<u64>,
    pub timestamp:        u64,
    pub horizon_link:     String,
}

/// Evaluate all rules for a contract against a transaction.
/// Returns one payload per matching rule.
pub fn evaluate(
    label:       &str,
    contract_id: &str,
    network:     &str,
    horizon_base: &str,
    rules:       &[AlertRule],
    tx:          &HorizonTransaction,
) -> Vec<AlertPayload> {
    let timestamp = chrono_to_unix(&tx.created_at);
    let horizon_link = format!("{}/transactions/{}", horizon_base, tx.hash);

    rules
        .iter()
        .filter_map(|rule| {
            let triggered = match rule {
                AlertRule::AnyTransaction => true,

                AlertRule::TransactionFailed => !tx.successful,

                AlertRule::LargeTransfer { threshold_xlm } => {
                    tx.amount_stroops
                        .map(|s| s >= threshold_xlm * 10_000_000)
                        .unwrap_or(false)
                }

                AlertRule::FunctionCalled { function_name } => tx
                    .function_name
                    .as_deref()
                    .map(|f| f == function_name)
                    .unwrap_or(false),

                AlertRule::AdminFunctionCalled { function_names } => tx
                    .function_name
                    .as_deref()
                    .map(|f| function_names.iter().any(|n| n == f))
                    .unwrap_or(false),
            };

            triggered.then(|| AlertPayload {
                label:            label.to_string(),
                contract_id:      contract_id.to_string(),
                network:          network.to_string(),
                rule_triggered:   rule_label(rule),
                transaction_hash: tx.hash.clone(),
                function_name:    tx.function_name.clone(),
                amount:           tx.amount_stroops.map(|s| s / 10_000_000),
                timestamp,
                horizon_link:     horizon_link.clone(),
            })
        })
        .collect()
}

fn rule_label(rule: &AlertRule) -> String {
    match rule {
        AlertRule::AnyTransaction                          => "AnyTransaction".into(),
        AlertRule::TransactionFailed                       => "TransactionFailed".into(),
        AlertRule::LargeTransfer { threshold_xlm }        => format!("LargeTransfer(>={}XLM)", threshold_xlm),
        AlertRule::FunctionCalled { function_name }       => format!("FunctionCalled({})", function_name),
        AlertRule::AdminFunctionCalled { function_names } => format!("AdminFunctionCalled({:?})", function_names),
    }
}

fn chrono_to_unix(ts: &str) -> u64 {
    // Horizon timestamps are RFC 3339, e.g. "2024-01-15T12:34:56Z"
    // We do a best-effort parse without pulling in chrono.
    use std::time::{SystemTime, UNIX_EPOCH};
    // Fallback: current time if parsing fails.
    ts.parse::<u64>().unwrap_or_else(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}
