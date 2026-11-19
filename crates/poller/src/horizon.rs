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

/// Abstraction over the Horizon endpoint so it can be mocked in tests.
pub trait HorizonClient {
    fn fetch_transactions(&self, cursor: Option<&str>) -> Result<TransactionPage>;
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

    let new_cursor = records
        .last()
        .map(|r| r.paging_token.clone())
        .or_else(|| cursor.map(|c| c.to_string()));

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
}
