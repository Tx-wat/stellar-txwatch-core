//! Horizon client and contract polling.
//!
//! Provides a thin wrapper around the Horizon REST API used by the poller to
//! fetch contract events and advance a cursor.

use anyhow::Result;
use serde::Deserialize;

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

/// Poll a contract for new transactions, advancing the cursor only when new
/// records are present.
pub fn poll_contract<C: HorizonClient>(
    client: &C,
    cursor: Option<&str>,
) -> Result<PollResult> {
    let page = client.fetch_transactions(cursor)?;
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
}
