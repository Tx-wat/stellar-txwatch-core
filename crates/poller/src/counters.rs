use std::{
    collections::HashMap,
    sync::atomic::AtomicU64,
    sync::Mutex,
};

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) transactions: AtomicU64,
    pub(crate) alerts: AtomicU64,
    pub(crate) interval_transactions: AtomicU64,
    pub(crate) interval_alerts: AtomicU64,
    /// Per-contract `(transactions, alerts)` since the last summary, keyed by label.
    interval_per_contract: Mutex<HashMap<String, (u64, u64)>>,
}

impl Counters {
    /// Add one poll's results to the contract's interval counters.
    pub(crate) fn record_contract(&self, label: &str, txs: u64, alerts: u64) {
        let mut map = self
            .interval_per_contract
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(label.to_string()).or_insert((0, 0));
        entry.0 += txs;
        entry.1 += alerts;
    }

    /// Return `(label, transactions, alerts)` for every label, in the given
    /// order (zeros for contracts with no activity), and reset the counters.
    pub(crate) fn take_interval_per_contract(&self, labels: &[String]) -> Vec<(String, u64, u64)> {
        let mut map = self
            .interval_per_contract
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let out = labels
            .iter()
            .map(|label| {
                let (txs, alerts) = map.get(label).copied().unwrap_or((0, 0));
                (label.clone(), txs, alerts)
            })
            .collect();
        map.clear();
        out
    }
}
