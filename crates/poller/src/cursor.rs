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

/// Number of consecutive Horizon failures before the circuit breaker opens.
pub const CIRCUIT_BREAKER_THRESHOLD: u32 = 3;

// ── ContractState ─────────────────────────────────────────────────────────────

/// Per-contract polling state: the last-seen paging token plus circuit-breaker
/// bookkeeping.
///
/// All circuit-breaker fields use `serde(default)` so that cursor files written
/// by older versions (which stored only the token string) still deserialize.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractState {
    /// Last-seen Horizon paging token (or the sentinel `"now"`).
    #[serde(default = "default_token")]
    pub token: String,
    /// Number of consecutive Horizon failures for this contract.
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Number of poll intervals to skip once the breaker is open.
    #[serde(default)]
    pub skip_intervals: u32,
    /// Remaining poll intervals to skip before polling resumes.
    #[serde(default)]
    pub skip_remaining: u32,
}

fn default_token() -> String {
    "now".to_string()
}

impl Default for ContractState {
    fn default() -> Self {
        Self {
            token: default_token(),
            consecutive_failures: 0,
            skip_intervals: 0,
            skip_remaining: 0,
        }
    }
}

impl ContractState {
    /// Build a fresh state starting at `"now"`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the circuit breaker is currently open (polling should be skipped).
    pub fn is_skipped(&self) -> bool {
        self.skip_remaining > 0
    }
}

// ── CursorMap ─────────────────────────────────────────────────────────────────

/// Tracks the last-seen Horizon paging token for each watched contract.
///
/// All mutating operations go through typed methods rather than raw `HashMap`
/// APIs so the intent is always clear at the call site.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CursorMap(HashMap<String, ContractState>);

impl CursorMap {
    /// Build a fresh `CursorMap` from a slice of contracts.
    ///
    /// Every contract starts at `"now"` (Horizon sentinel meaning "start from
    /// the most recent transaction").
    pub fn new_for_contracts(contracts: &[WatchedContract]) -> Self {
        Self(
            contracts
                .iter()
                .map(|c| (c.contract_id.clone(), ContractState::new()))
                .collect(),
        )
    }

    /// Return the current paging token for `contract_id`, or `"now"` if not set.
    pub fn get(&self, contract_id: &str) -> &str {
        self.0
            .get(contract_id)
            .map(|s| s.token.as_str())
            .unwrap_or("now")
    }

    /// Update the paging token for `contract_id`.
    pub fn set(&mut self, contract_id: &str, token: &str) {
        self.0
            .entry(contract_id.to_string())
            .or_default()
            .token = token.to_string();
    }

    /// Return the full per-contract state, if present.
    pub fn state(&self, contract_id: &str) -> Option<&ContractState> {
        self.0.get(contract_id)
    }

    /// Whether polling for `contract_id` should be skipped this cycle.
    pub fn is_skipped(&self, contract_id: &str) -> bool {
        self.0
            .get(contract_id)
            .map(ContractState::is_skipped)
            .unwrap_or(false)
    }

    /// Record a Horizon failure for `contract_id`.
    ///
    /// Increments the consecutive-failure counter. Once the counter reaches
    /// [`CIRCUIT_BREAKER_THRESHOLD`], the circuit breaker opens and the contract
    /// is skipped for `2^failures` poll intervals.
    ///
    /// Returns `true` if the breaker just opened on this call (i.e. the
    /// contract transitioned into the open state).
    pub fn record_failure(&mut self, contract_id: &str) -> bool {
        let state = self.0.entry(contract_id.to_string()).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);

        if state.consecutive_failures < CIRCUIT_BREAKER_THRESHOLD {
            return false;
        }

        let was_open = state.is_skipped();
        // 2^failures, capped to avoid overflow / absurd skip windows.
        let intervals = 1u32
            .checked_shl(state.consecutive_failures)
            .unwrap_or(u32::MAX);
        state.skip_intervals = intervals;
        state.skip_remaining = intervals;

        let just_opened = !was_open;
        if just_opened {
            warn!(
                contract = %contract_id,
                consecutive_failures = state.consecutive_failures,
                skip_intervals = intervals,
                "circuit breaker open: skipping polling for contract"
            );
        }
        just_opened
    }

    /// Record a successful Horizon poll for `contract_id`, resetting the
    /// circuit-breaker state.
    ///
    /// Returns `true` if the contract was previously in the open state (i.e.
    /// the breaker just recovered).
    pub fn record_success(&mut self, contract_id: &str) -> bool {
        let state = self.0.entry(contract_id.to_string()).or_default();
        let was_open = state.is_skipped() || state.consecutive_failures >= CIRCUIT_BREAKER_THRESHOLD;

        state.consecutive_failures = 0;
        state.skip_intervals = 0;
        state.skip_remaining = 0;

        if was_open {
            warn!(
                contract = %contract_id,
                "circuit breaker recovered: resuming polling for contract"
            );
        }
        was_open
    }

    /// Consume one skipped poll interval for `contract_id`, if any remain.
    ///
    /// Returns `true` if the contract is still skipped after this call.
    pub fn tick_skip(&mut self, contract_id: &str) -> bool {
        let state = self.0.entry(contract_id.to_string()).or_default();
        if state.skip_remaining > 0 {
            state.skip_remaining -= 1;
        }
        state.is_skipped()
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
                            .or_insert_with(ContractState::new);
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

    /// Persist the current cursor map to a JSON file atomically.
    ///
    /// The JSON is written to a uniquely-named temporary file in the same
    /// directory as `path`, flushed and fsynced, then atomically renamed over
    /// `path`. A crash mid-write therefore leaves either the old file or the
    /// new file intact — never a truncated/corrupt one. On any error the
    /// temporary file is removed before returning.
    ///
    /// Errors are returned to the caller; the poller can choose to log and
    /// continue rather than aborting the poll cycle.
    pub fn save_to_file(&self, path: &Path) -> Result<()> {
        use std::io::Write;

        let json = serde_json::to_string_pretty(self)
            .context("failed to serialise cursor map to JSON")?;

        // Build a temp path in the same directory so the rename stays on the
        // same filesystem (required for atomicity).
        let tmp_path = {
            let mut name = path
                .file_name()
                .map(|n| n.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from("cursor"));
            name.push(format!(".tmp.{}", std::process::id()));
            match path.parent() {
                Some(dir) if !dir.as_os_str().is_empty() => dir.join(name),
                _ => std::path::PathBuf::from(name),
            }
        };

        let write_result = (|| -> Result<()> {
            let mut file = std::fs::File::create(&tmp_path).with_context(|| {
                format!("failed to create temp cursor file '{}'", tmp_path.display())
            })?;
            file.write_all(json.as_bytes()).with_context(|| {
                format!("failed to write temp cursor file '{}'", tmp_path.display())
            })?;
            file.sync_all().with_context(|| {
                format!("failed to fsync temp cursor file '{}'", tmp_path.display())
            })?;
            drop(file);
            std::fs::rename(&tmp_path, path).with_context(|| {
                format!(
                    "failed to rename temp cursor file '{}' to '{}'",
                    tmp_path.display(),
                    path.display()
                )
            })?;
            Ok(())
        })();

        if write_result.is_err() {
            // Best-effort cleanup so a failed write doesn't leave stale temps.
            let _ = std::fs::remove_file(&tmp_path);
        }

        write_result
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
    fn save_to_file_produces_valid_json() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_valid_json.json");
        let contracts = vec![contract(ID_A)];

        let mut map = CursorMap::new_for_contracts(&contracts);
        map.set(ID_A, "424242");
        map.save_to_file(&tmp).unwrap();

        let raw = std::fs::read_to_string(&tmp).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(parsed.is_object(), "cursor file should be a JSON object");

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

    #[test]
    fn load_legacy_string_cursor_file_is_backward_compatible() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_legacy.json");
        // Legacy format: plain string values, no circuit-breaker fields.
        let legacy = serde_json::json!({ ID_A: "777" });
        std::fs::write(&tmp, legacy.to_string()).unwrap();

        let contracts = vec![contract(ID_A)];
        let map = CursorMap::load_from_file(&tmp, &contracts);

        assert_eq!(map.get(ID_A), "777");
        let state = map.state(ID_A).unwrap();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.skip_intervals, 0);
        assert_eq!(state.skip_remaining, 0);
        assert!(!map.is_skipped(ID_A));

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn failures_below_threshold_do_not_open_breaker() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        assert!(!map.record_failure(ID_A));
        assert!(!map.record_failure(ID_A));
        assert!(!map.is_skipped(ID_A));
        assert_eq!(map.state(ID_A).unwrap().consecutive_failures, 2);
    }

    #[test]
    fn third_failure_opens_breaker_with_exponential_backoff() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        map.record_failure(ID_A);
        map.record_failure(ID_A);
        assert!(map.record_failure(ID_A), "third failure should open breaker");

        let state = map.state(ID_A).unwrap();
        assert_eq!(state.consecutive_failures, 3);
        // 2^3 == 8 intervals
        assert_eq!(state.skip_intervals, 8);
        assert_eq!(state.skip_remaining, 8);
        assert!(map.is_skipped(ID_A));
    }

    #[test]
    fn backoff_grows_with_consecutive_failures() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        for _ in 0..4 {
            map.record_failure(ID_A);
        }
        // 2^4 == 16
        assert_eq!(map.state(ID_A).unwrap().skip_intervals, 16);

        map.record_failure(ID_A);
        // 2^5 == 32
        assert_eq!(map.state(ID_A).unwrap().skip_intervals, 32);
    }

    #[test]
    fn record_failure_returns_true_only_on_transition() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        assert!(!map.record_failure(ID_A));
        assert!(!map.record_failure(ID_A));
        assert!(map.record_failure(ID_A), "transition into open state");
        assert!(!map.record_failure(ID_A), "already open, no new transition");
    }

    #[test]
    fn tick_skip_decrements_until_closed() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        for _ in 0..3 {
            map.record_failure(ID_A);
        }
        assert_eq!(map.state(ID_A).unwrap().skip_remaining, 8);

        for _ in 0..8 {
            assert!(map.tick_skip(ID_A));
        }
        assert!(!map.is_skipped(ID_A));
        assert_eq!(map.state(ID_A).unwrap().skip_remaining, 0);
    }

    #[test]
    fn success_resets_breaker_state() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        for _ in 0..3 {
            map.record_failure(ID_A);
        }
        assert!(map.is_skipped(ID_A));

        assert!(map.record_success(ID_A), "recovery should be reported");
        let state = map.state(ID_A).unwrap();
        assert_eq!(state.consecutive_failures, 0);
        assert_eq!(state.skip_intervals, 0);
        assert_eq!(state.skip_remaining, 0);
        assert!(!map.is_skipped(ID_A));
    }

    #[test]
    fn success_on_healthy_contract_is_noop() {
        let mut map = CursorMap::new_for_contracts(&[contract(ID_A)]);
        assert!(!map.record_success(ID_A));
        assert_eq!(map.state(ID_A).unwrap().consecutive_failures, 0);
    }

    #[test]
    fn interrupted_write_leaves_original_file_intact() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_atomic.json");
        let contracts = vec![contract(ID_A)];

        // Establish a good cursor file first.
        let mut original = CursorMap::new_for_contracts(&contracts);
        original.set(ID_A, "111");
        original.save_to_file(&tmp).unwrap();

        // Simulate a crash mid-write: a stale temp file is left behind.
        let stale_tmp = tmp.with_file_name(format!(
            "{}.tmp.{}",
            tmp.file_name().unwrap().to_string_lossy(),
            std::process::id()
        ));
        std::fs::write(&stale_tmp, "{\"partial\":").unwrap();

        // The original file must be untouched and still load correctly.
        let loaded = CursorMap::load_from_file(&tmp, &contracts);
        assert_eq!(loaded.get(ID_A), "111");

        // A subsequent successful save must overwrite the stale temp and
        // produce a valid file.
        let mut updated = CursorMap::new_for_contracts(&contracts);
        updated.set(ID_A, "222");
        updated.save_to_file(&tmp).unwrap();

        let reloaded = CursorMap::load_from_file(&tmp, &contracts);
        assert_eq!(reloaded.get(ID_A), "222");
        assert!(!stale_tmp.exists(), "temp file should be cleaned up");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn corrupt_cursor_file_falls_back_to_now_without_panicking() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_truncated.json");
        // Truncated JSON, as if a non-atomic write was interrupted.
        std::fs::write(&tmp, "{\"CAAAA").unwrap();

        let contracts = vec![contract(ID_A), contract(ID_B)];
        let map = CursorMap::load_from_file(&tmp, &contracts);

        assert_eq!(map.get(ID_A), "now");
        assert_eq!(map.get(ID_B), "now");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn circuit_breaker_state_persists_via_json() {
        let tmp = std::env::temp_dir().join("txwatch_cursor_breaker.json");
        let contracts = vec![contract(ID_A)];

        let mut original = CursorMap::new_for_contracts(&contracts);
        for _ in 0..3 {
            original.record_failure(ID_A);
        }
        original.save_to_file(&tmp).unwrap();

        let loaded = CursorMap::load_from_file(&tmp, &contracts);
        let state = loaded.state(ID_A).unwrap();
        assert_eq!(state.consecutive_failures, 3);
        assert_eq!(state.skip_intervals, 8);
        assert!(loaded.is_skipped(ID_A));

        let _ = std::fs::remove_file(&tmp);
    }
}
