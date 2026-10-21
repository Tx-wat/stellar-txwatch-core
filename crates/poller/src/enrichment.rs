//! Soroban operation enrichment.
//!
//! `fetch_soroban_details` is the HTTP layer — it retrieves the operations for
//! a single transaction from Horizon and delegates the pure data extraction to
//! `txwatch_rules::extract_soroban_details`.
//!
//! The pure `extract_soroban_details` function lives in `txwatch-rules` (issue
//! #253) so it can be unit-tested without network I/O.

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::debug;

use crate::horizon::OperationsPage;

/// Fetch operations for a single transaction from Horizon and extract Soroban
/// function names and payment amounts.
///
/// This is used as a fallback when `join=operations` is not supported or
/// returned an empty `operations` array on the transactions endpoint.
#[tracing::instrument(skip(client), fields(tx = %tx_hash))]
pub(crate) async fn fetch_soroban_details(
    client:  &Client,
    base:    &str,
    tx_hash: &str,
) -> Result<(Vec<String>, Option<u64>)> {
    let url = format!("{}/transactions/{}/operations", base, tx_hash);

    debug!(url = %url, "fetching soroban operation details (fallback)");

    let page: OperationsPage = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {} failed", url))?
        .json()
        .await
        .with_context(|| format!("failed to parse operations from {}", url))?;

    Ok(txwatch_rules::extract_soroban_details(page._embedded.records))
}
