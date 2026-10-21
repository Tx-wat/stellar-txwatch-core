#![no_main]

use libfuzzer_sys::fuzz_target;
use txwatch_rules::{EnrichedTransaction, HorizonTransaction};

// `from_horizon` must return `Err` for an invalid `created_at`, never panic.
fuzz_target!(|created_at: &str| {
    let tx = HorizonTransaction {
        hash: "fuzz".into(),
        created_at: created_at.to_string(),
        successful: true,
        paging_token: "1".into(),
        fee_charged: None,
        envelope_xdr: None,
        result_xdr: None,
    };
    let _ = EnrichedTransaction::from_horizon(tx, vec![], None, None);
});
