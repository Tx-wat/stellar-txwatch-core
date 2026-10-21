//! Rule evaluation throughput benchmark (issue #273).
//!
//! Establishes a baseline for `evaluate()` and catches regressions as rules are added.
//! Run with: `cargo bench -p txwatch-rules`

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use txwatch_config::AlertRule;
use txwatch_rules::{evaluate, EnrichedTransaction};

const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

fn make_transactions(count: usize) -> Vec<EnrichedTransaction> {
    (0..count)
        .map(|i| EnrichedTransaction {
            hash: format!("bench{:08}", i),
            timestamp: "2024-06-01T10:00:00Z".parse().unwrap(),
            successful: i % 10 != 0,
            paging_token: i.to_string(),
            function_names: vec!["transfer".into()],
            amount_stroops: Some((i as u64 % 2_000) * 10_000_000),
            fee_charged_stroops: Some(100 + (i as u64 % 500)),
        })
        .collect()
}

fn one_of_each_rule() -> Vec<AlertRule> {
    vec![
        AlertRule::AnyTransaction,
        AlertRule::TransactionFailed,
        AlertRule::LargeTransfer { threshold_xlm: 1_000 },
        AlertRule::FunctionCalled { function_name: "withdraw".into() },
        AlertRule::AdminFunctionCalled { function_names: vec!["set_admin".into(), "upgrade".into()] },
        AlertRule::HighFee { threshold_stroops: 10_000, threshold_xlm: None },
    ]
}

fn evaluate_all(rules: &[AlertRule], txs: &[EnrichedTransaction]) -> usize {
    txs.iter()
        .map(|tx| {
            black_box(evaluate(
                "Bench",
                CONTRACT_ID,
                "testnet",
                "https://horizon-testnet.stellar.org",
                "https://stellar.expert/explorer/testnet",
                rules,
                tx,
            ))
            .len()
        })
        .sum()
}

fn bench_rule_evaluation(c: &mut Criterion) {
    let mut group = c.benchmark_group("rule_evaluation");

    // Scenario 1: single AnyTransaction rule, 10,000 transactions.
    let txs_10k = make_transactions(10_000);
    let single_rule = vec![AlertRule::AnyTransaction];
    group.throughput(Throughput::Elements(txs_10k.len() as u64));
    group.bench_function("single_any_transaction_rule_10k_txs", |b| {
        b.iter(|| evaluate_all(&single_rule, &txs_10k))
    });

    let txs_1k = make_transactions(1_000);

    // Scenario 2: one rule of each type, 1,000 transactions.
    let six_rules = one_of_each_rule();
    group.throughput(Throughput::Elements(txs_1k.len() as u64));
    group.bench_function("six_rules_one_of_each_type_1k_txs", |b| {
        b.iter(|| evaluate_all(&six_rules, &txs_1k))
    });

    // Scenario 3: 50 FunctionCalled rules (worst-case rule count), 1,000 transactions.
    let fifty_rules: Vec<AlertRule> = (0..50)
        .map(|i| AlertRule::FunctionCalled { function_name: format!("fn_{:02}", i) })
        .collect();
    group.throughput(Throughput::Elements(txs_1k.len() as u64));
    group.bench_function("fifty_function_called_rules_1k_txs", |b| {
        b.iter(|| evaluate_all(&fifty_rules, &txs_1k))
    });

    group.finish();
}

criterion_group!(benches, bench_rule_evaluation);
criterion_main!(benches);
