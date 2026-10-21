//! CursorMap — a newtype wrapping `HashMap<String, String>` that tracks the
//! last-seen Horizon paging token for each watched contract.
//!
//! Keys are contract IDs (56-character Stellar addresses starting with `C`).
//! Values are Horizon paging tokens (or the special sentinel `"now"` which
//! tells Horizon to start from the latest transaction).
//!
//! The newtype adds semantic methods so call sites are self-documenting, and
//! provides JSON load/save helpers for persistence across restarts.

use std::{
    collections::HashMap,
    path::Path,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use txwatch_config::WatchedContract;

// ── CursorMap ─────────────────────────────────────────────────────────────────

/// Tracks the last-seen Horizon paging token for each watched contract.
///
/// All mutating operations go through typed methods rather than raw `HashMap`
/// APIs so the intent is always clear at the call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CursorMap(HashMap<String, String>);

impl CursorMap {
    /// Build a fresh `CursorMap` from a slice of contracts.
    ///
    /// Every contract starts at `"now"` (Horizon sentinel meaning "start from
    /// the most recent transaction").
    pub fn new_for_contracts(contracts: &[WatchedContract]) -> Self {
        Self(
            contracts
                .iter()
                .map(|c| (c.contract_id.clone(), "now".to_string()))
                .collect(),
        )
    }

    /// Return the current paging token for `contract_id`, or `"now"` if not set.
    pub fn get(&self, contract_id: &str) -> &str {
        self.0.get(contract_id).map(String::as_str).unwrap_or("now")
    }

    /// Update the paging token for `contract_id`.
    pub fn set(&mut self, contract_id: &str, token: &str) {
        self.0.insert(contract_id.to_string(), token.to_string());
    }

    /// Load a `CursorMap` from a JSON file.
    ///
    /// If the file cannot be read or parsed, logs a warning/debug message and
    /// returns a fresh map initialised to `"now"` for all supplied contracts.
    /// This matches the existing graceful-degradation behaviour in the poller.
    pub fn load_from_file(path: &Path, contracts: &[WatchedContract]) -> Self {
        match std::fs::read_to_string(path) {
            Ok(raw) => match serde_json::from_str::<CursorMap>(&raw) {
                Ok(mut map) => {
                    // Ensure every configured contract has a cursor entry.
                    for c in contracts {
                        map.0
                            .entry(c.contract_id.clone())
                            .or_insert_with(|| "now".to_string());
                    }
                    map
                }
                Err(e) => {
                    warn!(
                        error = ?e,
                        path  = %path.display(),
                        "failed to parse cursor_file; starting from 'now' for all contracts"
                    );
                    Self::new_for_contracts(contracts)
                }
            },
            Err(e) => {
                debug!(
                    error = ?e,
                    path  = %path.display(),
                    "could not read cursor_file; starting from 'now'"
                );
                Self::new_for_contracts(contracts)
            }
        }
    }

    /// Persist the current cursor map to a JSON file.
    ///
    /// Errors are returned to the caller; the poller can choose to log and
    /// continue rather than aborting the poll cycle.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .context("failed to serialise cursor map to JSON")?;
        std::fs::write(path, json)
            .with_context(|| format!("failed to write cursor file '{}'", path.display()))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use txwatch_config::{AlertRule, Network, WatchedContract};

    fn contract(id: &str) -> WatchedContract {
        WatchedContract {
            label: id.to_string(),
            contract_id: id.to_string(),
            network: Network::Testnet,
            rules: vec![AlertRule::AnyTransaction],
            webhook_url: "https://example.com/hook".into(),
            webhook_secret: None,
            horizon_base_url_override: None,
        }
    }

    const ID_A: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const ID_B: &str = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

    #[test]
    fn new_for_contracts_initialises_all_to_now() {
        let contracts = vec![contract(ID_A), contract(ID_B)];
        let map = CursorMap::new_for_contracts(&contracts);
        assert_eq!(map.get(ID_A), "now");
        assert_eq!(map.get(ID_B), "now");
    }

    #[test]
    fn get_returns_now_for_unknown_contract() {
        let map = CursorMap::default();
        assert_eq!(map.get(ID_A), "now");
    }

    #[test]
    fn set_then_get_roundtrips() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        map.set(ID_A, "12345678");
        assert_eq!(map.get(ID_A), "12345678");
    }

    #[test]
    fn json_roundtrip_via_save_and_load() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_test.json");
        let contracts = vec![contract(ID_A), contract(ID_B)];

        let mut original = CursorMap::new_for_contracts(&contracts);
        original.set(ID_A, "999");
        original.set(ID_B, "888");

        original.save_to_file(&tmp).unwrap();

        let loaded = CursorMap::load_from_file(&tmp, &contracts);
        assert_eq!(loaded.get(ID_A), "999");
        assert_eq!(loaded.get(ID_B), "888");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_from_missing_file_falls_back_to_now() {
        let path = std::path::Path::new("/tmp/txwatch_cursor_nonexistent_xyz.json");
        let contracts = vec![contract(ID_A)];
        let map = CursorMap::load_from_file(path, &contracts);
        assert_eq!(map.get(ID_A), "now");
    }

    #[test]
    fn load_from_invalid_json_falls_back_to_now() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_bad.json");
        std::fs::write(&tmp, "not valid json!!!").unwrap();
        let contracts = vec![contract(ID_A)];
        let map = CursorMap::load_from_file(&tmp, &contracts);
        assert_eq!(map.get(ID_A), "now");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn load_merges_persisted_and_new_contracts() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_merge.json");
        // File only has ID_A
        let partial = serde_json::json!({ ID_A: "500" });
        std::fs::write(&tmp, partial.to_string()).unwrap();

        // But config also has ID_B (new contract added after last save)
        let contracts = vec![contract(ID_A), contract(ID_B)];
        let map = CursorMap::load_from_file(&tmp, &contracts);

        assert_eq!(map.get(ID_A), "500", "existing cursor should be preserved");
        assert_eq!(map.get(ID_B), "now", "new contract should start at 'now'");

        let _ = std::fs::remove_file(&tmp);
    }
}
