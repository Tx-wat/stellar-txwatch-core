//! Horizon client and contract polling.
//!
//! Provides a thin wrapper around the Horizon REST API used by the poller to
//! fetch contract events and advance a cursor.

use anyhow::Result;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};

/// A single transaction record returned by Horizon.
#[derive(Debug, Clone, Deserialize)]
pub struct TransactionRecord {
    pub id: String,
    pub paging_token: String,
}

/// A page of transactions returned by Horizon.
#[derive(Debug, Clone, Deserialize)]
pub struct TransactionPage {
    #[serde(rename = "_embedded")]
    pub embedded: EmbeddedRecords,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddedRecords {
    pub records: Vec<TransactionRecord>,
}

/// Result of a single `poll_contract` invocation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PollResult {
    pub transactions: usize,
    pub alerts: usize,
    pub cursor: Option<String>,
}

// ── Transactions endpoint ─────────────────────────────────────────────────────

/// A Horizon transaction record that may include inline operations returned
/// when the `join=operations` query parameter is used.
#[derive(Deserialize)]
pub(crate) struct HorizonTransactionWithOps {
    /// The transaction record. `source_account` is captured on
    /// `HorizonTransaction` itself (via `#[serde(rename = "source_account")]`),
    /// and `#[serde(flatten)]` passes it through unchanged here.
    #[serde(flatten)]
    pub tx: HorizonTransaction,
    /// Inline operations embedded by Horizon when `join=operations` is used.
    #[serde(default)]
    pub operations: Vec<HorizonOperation>,
}

/// Per-status-code counters backing the `txwatch_horizon_errors_total` metric.
///
/// Indexed by HTTP status code (0..=599); codes outside that range are ignored.
static HORIZON_ERRORS_TOTAL: [AtomicU64; 600] = [const { AtomicU64::new(0) }; 600];

/// Record a Horizon error for the given HTTP status code, incrementing the
/// `txwatch_horizon_errors_total{status_code="<code>"}` counter.
pub fn record_horizon_error(status_code: u16) {
    if let Some(counter) = HORIZON_ERRORS_TOTAL.get(status_code as usize) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// Return the current value of `txwatch_horizon_errors_total` for a status code.
pub fn horizon_errors_total(status_code: u16) -> u64 {
    HORIZON_ERRORS_TOTAL
        .get(status_code as usize)
        .map(|c| c.load(Ordering::Relaxed))
        .unwrap_or(0)
}

/// Fetch a single page of transactions from Horizon, counting non-success
/// HTTP status responses in `txwatch_horizon_errors_total`.
pub fn fetch_page<C: HorizonClient>(
    client: &C,
    cursor: Option<&str>,
) -> Result<TransactionPage> {
    match client.fetch_transactions(cursor) {
        Ok(page) => Ok(page),
        Err(err) => {
            if let Some(status) = err.downcast_ref::<HorizonStatusError>() {
                record_horizon_error(status.status_code);
            }
            Err(err)
        }
    }
}

/// Error carrying the HTTP status code returned by Horizon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HorizonStatusError {
    pub status_code: u16,
}

impl std::fmt::Display for HorizonStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "horizon returned non-success status {}", self.status_code)
    }
}

impl std::error::Error for HorizonStatusError {}

/// Poll a contract for new transactions, advancing the cursor only when new
/// records are present.
pub fn poll_contract<C: HorizonClient>(
    client: &C,
    cursor: Option<&str>,
) -> Result<PollResult> {
    let page = fetch_page(client, cursor)?;
    let records = page.embedded.records;

    if records.is_empty() {
        return Ok(PollResult {
            transactions: 0,
            alerts: 0,
            cursor: cursor.map(|c| c.to_string()),
        });
    }

// ── Horizon URL resolution ────────────────────────────────────────────────────

/// Resolve the effective Horizon base URL for a contract.
///
/// A per-contract `horizon_url` override takes precedence over the network
/// default so that users with private Horizon nodes can point individual
/// contracts at their own endpoint. When no override is set (or it is blank),
/// `network_default` is used.
pub(crate) fn resolve_horizon_url<'a>(
    contract_override: Option<&'a str>,
    network_default: &'a str,
) -> &'a str {
    match contract_override {
        Some(url) if !url.trim().is_empty() => url,
        _ => network_default,
    }
}

// ── Startup connectivity check ────────────────────────────────────────────────

    Ok(PollResult {
        transactions: records.len(),
        alerts: records.len(),
        cursor: new_cursor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockHorizon {
        page: TransactionPage,
    }

    impl HorizonClient for MockHorizon {
        fn fetch_transactions(&self, _cursor: Option<&str>) -> Result<TransactionPage> {
            Ok(self.page.clone())
        }
    }

    struct FailingHorizon {
        status_code: u16,
    }

    impl HorizonClient for FailingHorizon {
        fn fetch_transactions(&self, _cursor: Option<&str>) -> Result<TransactionPage> {
            Err(HorizonStatusError {
                status_code: self.status_code,
            }
            .into())
        }
    }

    fn empty_page() -> TransactionPage {
        TransactionPage {
            embedded: EmbeddedRecords { records: vec![] },
        }
    }

    #[test]
    fn poll_contract_empty_page_does_not_advance_cursor() {
        let client = MockHorizon { page: empty_page() };
        let initial_cursor = Some("cursor-123");

        let result = poll_contract(&client, initial_cursor).expect("poll should succeed");

        assert_eq!(result.transactions, 0, "expected 0 transactions");
        assert_eq!(result.alerts, 0, "expected 0 alerts");
        assert_eq!(
            result.cursor.as_deref(),
            initial_cursor,
            "cursor must not advance on an empty page"
        );
    }

    #[test]
    fn poll_contract_empty_page_with_no_cursor_stays_none() {
        let client = MockHorizon { page: empty_page() };

        let result = poll_contract(&client, None).expect("poll should succeed");

        assert_eq!(result.transactions, 0);
        assert_eq!(result.alerts, 0);
        assert_eq!(result.cursor, None, "cursor must remain unset");
    }

    #[test]
    fn fetch_page_increments_error_counter_by_status_code() {
        let before = horizon_errors_total(500);
        let client = FailingHorizon { status_code: 500 };

        assert!(fetch_page(&client, None).is_err());
        assert_eq!(horizon_errors_total(500), before + 1);
    }

    #[test]
    fn resolve_prefers_contract_override() {
        let resolved = resolve_horizon_url(
            Some("https://private-horizon.example.com"),
            "https://horizon-testnet.stellar.org",
        );
        assert_eq!(resolved, "https://private-horizon.example.com");
    }

    #[test]
    fn resolve_falls_back_to_network_default() {
        let resolved = resolve_horizon_url(None, "https://horizon-testnet.stellar.org");
        assert_eq!(resolved, "https://horizon-testnet.stellar.org");
    }

    #[test]
    fn resolve_ignores_blank_override() {
        let resolved = resolve_horizon_url(Some("   "), "https://horizon-testnet.stellar.org");
        assert_eq!(resolved, "https://horizon-testnet.stellar.org");
    }

    #[test]
    fn transaction_with_ops_deserialises_source_account() {
        // `source_account` lives on `HorizonTransaction` and must survive the
        // `#[serde(flatten)]` in `HorizonTransactionWithOps`.
        let json = r#"{
            "id": "abc",
            "hash": "deadbeef",
            "source_account": "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF",
            "successful": true,
            "operations": []
        }"#;
        let parsed: HorizonTransactionWithOps = serde_json::from_str(json).unwrap();
        assert_eq!(
            parsed.tx.source_account.as_deref(),
            Some("GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF")
        );
    }
}
