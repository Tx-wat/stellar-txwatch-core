//! Horizon REST API response shapes.
//!
//! These types are used to deserialise the JSON returned by the Horizon API.
//! They are intentionally minimal — only the fields TxWatch needs are captured;
//! unknown fields are ignored so the code stays forward-compatible.

use serde::Deserialize;
use txwatch_rules::{HorizonOperation, HorizonTransaction};

// ── Account kind ─────────────────────────────────────────────────────────────

/// The kind of Stellar address being watched.
///
/// Soroban contract addresses (C-addresses) and classic payment accounts
/// (G-addresses) are both served by the same Horizon
/// `/accounts/{id}/transactions` endpoint, so the polling URL is identical for
/// both. This enum exists so callers can reason about the address type without
/// re-parsing the string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountKind {
    /// A Soroban contract address (starts with `C`).
    Contract,
    /// A classic Stellar account (starts with `G`).
    Classic,
}

impl AccountKind {
    /// Classify a 56-character Stellar address by its leading character.
    ///
    /// Returns `None` for anything that is not a well-formed C- or G-address.
    pub(crate) fn from_address(address: &str) -> Option<Self> {
        if address.len() != 56 {
            return None;
        }
        match address.as_bytes().first() {
            Some(b'C') => Some(AccountKind::Contract),
            Some(b'G') => Some(AccountKind::Classic),
            _ => None,
        }
    }
}

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
///
/// This shape is returned for both C-addresses (contracts) and G-addresses
/// (classic accounts), so the same polling path works for both.
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
