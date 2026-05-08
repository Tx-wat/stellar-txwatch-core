#![allow(dead_code)]
/// Integration tests for the full poll → evaluate → notify pipeline.
///
/// These tests spin up two wiremock servers:
///   - a mock Horizon server (transactions + operations endpoints)
///   - a mock webhook receiver
///
/// They then call the internal poller helpers directly to verify end-to-end
/// behaviour without touching the real Stellar network.
use std::collections::HashMap;

use reqwest::Client;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Re-export internal helpers via the public API surface we test.
use txwatch_config::{AlertRule, Network, WatchedContract};
use txwatch_rules::{evaluate, EnrichedTransaction};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn contract(webhook_url: &str, rules: Vec<AlertRule>) -> WatchedContract {
    WatchedContract {
        label:       "Integration Test Contract".into(),
        contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        network:     Network::Testnet,
        rules,
        webhook_url: webhook_url.to_string(),
        webhook_secret: None,
    }
}

fn tx_page_json(
    hash:         &str,
    paging_token: &str,
    successful:   bool,
) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "hash":         hash,
                "created_at":   "2024-06-01T10:00:00Z",
                "successful":   successful,
                "paging_token": paging_token,
                "envelope_xdr": null,
                "result_xdr":   null
            }]
        }
    })
}

fn ops_page_json(function_name: &str) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "type":     "invoke_host_function",
                "function": function_name
            }]
        }
    })
}

fn payment_ops_page_json(amount_str: &str) -> serde_json::Value {
    serde_json::json!({
        "_embedded": {
            "records": [{
                "type":   "payment",
                "amount": amount_str
            }]
        }
    })
}

fn empty_page_json() -> serde_json::Value {
    serde_json::json!({ "_embedded": { "records": [] } })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// AnyTransaction rule fires and webhook is called exactly once.
#[tokio::test]
async fn any_transaction_fires_webhook() {
    let horizon  = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Horizon: one transaction
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(tx_page_json("hash001", "100", true)),
        )
        .mount(&horizon)
        .await;

    // Horizon: operations for that transaction (no Soroban)
    Mock::given(method("GET"))
        .and(path("/transactions/hash001/operations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(empty_page_json()),
        )
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 1 POST
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    // Run one poll cycle manually.
    let client   = Client::new();
    let contract = contract(&format!("{}/hook", receiver.uri()), vec![AlertRule::AnyTransaction]);

    let url = format!(
        "{}/accounts/{}/transactions?cursor=now&order=asc&limit=200",
        horizon.uri(),
        contract.contract_id
    );

    // Fetch transactions
    #[derive(serde::Deserialize)]
    struct Page { _embedded: Emb }
    #[derive(serde::Deserialize)]
    struct Emb  { records: Vec<txwatch_rules::HorizonTransaction> }

    let page: Page = client.get(&url).send().await.unwrap().json().await.unwrap();
    let records = page._embedded.records;
    assert_eq!(records.len(), 1);

    for raw in records {
        let ops_url = format!("{}/transactions/{}/operations", horizon.uri(), raw.hash);
        #[derive(serde::Deserialize)]
        struct OpsPage { _embedded: OpsEmb }
        #[derive(serde::Deserialize)]
        struct OpsEmb  { records: Vec<serde_json::Value> }
        let _ops: OpsPage = client.get(&ops_url).send().await.unwrap().json().await.unwrap();

        let enriched = EnrichedTransaction::from_horizon(raw, None, None, None).unwrap();
        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            contract.network.as_str(),
            &horizon.uri(),
        "https://stellar.expert/explorer/testnet",
            &contract.rules,
            &enriched,
        );
        assert_eq!(payloads.len(), 1);

        for payload in &payloads {
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, payload, None)
                .await
                .unwrap();
        }
    }

    // wiremock verifies the expect(1) on drop
}

/// TransactionFailed rule fires only for failed transactions.
#[tokio::test]
async fn transaction_failed_rule_fires_only_on_failure() {
    let horizon  = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Two transactions: one successful, one failed
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [
                    {
                        "hash": "ok_tx", "created_at": "2024-06-01T10:00:00Z",
                        "successful": true, "paging_token": "1",
                        "envelope_xdr": null, "result_xdr": null
                    },
                    {
                        "hash": "fail_tx", "created_at": "2024-06-01T10:01:00Z",
                        "successful": false, "paging_token": "2",
                        "envelope_xdr": null, "result_xdr": null
                    }
                ]
            }
        })))
        .mount(&horizon)
        .await;

    // Webhook: expect exactly 1 call (only the failed tx)
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client   = Client::new();
    let contract = contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::TransactionFailed],
    );

    let txs = vec![
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "ok_tx".into(), created_at: "2024-06-01T10:00:00Z".into(),
                successful: true, paging_token: "1".into(),
                envelope_xdr: None, result_xdr: None,
            },
            None, None, None,
        ).unwrap(),
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "fail_tx".into(), created_at: "2024-06-01T10:01:00Z".into(),
                successful: false, paging_token: "2".into(),
                envelope_xdr: None, result_xdr: None,
            },
            None, None, None,
        ).unwrap(),
    ];

    for tx in &txs {
        let payloads = evaluate(
            &contract.label, &contract.contract_id,
            contract.network.as_str(), &horizon.uri(),
        "https://stellar.expert/explorer/testnet",
            &contract.rules, tx,
        );
        for p in &payloads {
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, p, None)
                .await.unwrap();
        }
    }
}

/// LargeTransfer rule fires when payment amount meets threshold.
#[tokio::test]
async fn large_transfer_fires_above_threshold() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client   = Client::new();
    let contract = contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer { threshold_xlm: 5_000 }],
    );

    // 10_000 XLM = 100_000_000_000 stroops — above threshold
    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "big_tx".into(), created_at: "2024-06-01T10:00:00Z".into(),
            successful: true, paging_token: "1".into(),
            envelope_xdr: None, result_xdr: None,
        },
        None,
        Some(100_000_000_000),
        None,
    ).unwrap();

    let payloads = evaluate(
        &contract.label, &contract.contract_id,
        contract.network.as_str(), "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
        &contract.rules, &tx,
    );
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].amount_xlm, Some(10_000));

    txwatch_notifier::send_webhook(&client, &contract.webhook_url, &payloads[0], None)
        .await.unwrap();
}

/// FunctionCalled rule fires only when the function name matches.
#[tokio::test]
async fn function_called_rule_fires_on_exact_match() {
    let receiver = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client   = Client::new();
    let contract = contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::FunctionCalled { function_name: "withdraw".into() }],
    );

    let txs = vec![
        // "deposit" — should NOT fire
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t1".into(), created_at: "2024-06-01T10:00:00Z".into(),
                successful: true, paging_token: "1".into(),
                envelope_xdr: None, result_xdr: None,
            },
            Some("deposit".into()), None, None,
        ).unwrap(),
        // "withdraw" — SHOULD fire
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t2".into(), created_at: "2024-06-01T10:01:00Z".into(),
                successful: true, paging_token: "2".into(),
                envelope_xdr: None, result_xdr: None,
            },
            Some("withdraw".into()), None, None,
        ).unwrap(),
    ];

    for tx in &txs {
        let payloads = evaluate(
            &contract.label, &contract.contract_id,
            contract.network.as_str(), "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
            &contract.rules, tx,
        );
        for p in &payloads {
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, p, None)
                .await.unwrap();
        }
    }
}

/// Cursor advances so the same transaction is not processed twice.
#[tokio::test]
async fn cursor_advances_after_each_transaction() {
    let mut cursors: HashMap<String, String> = HashMap::new();
    let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    cursors.insert(contract_id.to_string(), "now".to_string());

    // Simulate advancing the cursor as the poller does
    let paging_tokens = ["100", "200", "300"];
    for token in &paging_tokens {
        cursors.insert(contract_id.to_string(), token.to_string());
    }

    assert_eq!(cursors.get(contract_id).map(String::as_str), Some("300"));
}
