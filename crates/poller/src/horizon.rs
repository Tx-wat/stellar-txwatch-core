//! Horizon REST API response shapes.
//!
//! These types are used to deserialise the JSON returned by the Horizon API.
//! They are intentionally minimal — only the fields TxWatch needs are captured;
//! unknown fields are ignored so the code stays forward-compatible.

use serde::Deserialize;
use txwatch_rules::{HorizonOperation, HorizonTransaction};

// ── Transactions endpoint ─────────────────────────────────────────────────────

/// A Horizon transaction record that may include inline operations returned
/// when the `join=operations` query parameter is used.
#[derive(Deserialize)]
pub(crate) struct HorizonTransactionWithOps {
    #[serde(flatten)]
    pub tx: HorizonTransaction,
    /// Inline operations embedded by Horizon when `join=operations` is used.
    #[serde(default)]
    pub operations: Vec<HorizonOperation>,
}

/// Top-level response envelope for `GET /accounts/{id}/transactions`.
#[derive(Deserialize)]
pub(crate) struct HorizonPage {
    pub _embedded: Embedded,
}

/// `_embedded` wrapper inside a Horizon page response.
#[derive(Deserialize)]
pub(crate) struct Embedded {
    pub records: Vec<HorizonTransactionWithOps>,
}

// ── Operations endpoint ───────────────────────────────────────────────────────

/// Top-level response envelope for `GET /transactions/{hash}/operations`.
#[derive(Deserialize)]
pub(crate) struct OperationsPage {
    pub _embedded: OpsEmbedded,
}

/// `_embedded` wrapper inside an operations page response.
#[derive(Deserialize)]
pub(crate) struct OpsEmbedded {
    pub records: Vec<HorizonOperation>,
}
