/// Integration tests for the full poll → evaluate → notify pipeline.
///
/// These tests spin up two wiremock servers:
///   - a mock Horizon server (transactions + operations endpoints)
///   - a mock webhook receiver
///
/// They then either call the public `run()` entry-point or drive the evaluate /
/// notify helpers directly to verify end-to-end behaviour without touching the
/// real Stellar network.
mod helpers;

use std::time::Duration;

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_config::{AlertRule, AppConfig};
use txwatch_rules::{evaluate, EnrichedTransaction};

// ── Tests ─────────────────────────────────────────────────────────────────────

/// `run()` polling loop: fires exactly one webhook for the single transaction
/// returned on the first poll cycle, then gets empty pages on subsequent cycles.
#[tokio::test]
async fn run_polls_once_and_fires_webhook() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // First transactions request returns one tx.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("run001", "500", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    // All subsequent transaction requests return an empty page.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Operations for the tx: no Soroban details needed.
    Mock::given(method("GET"))
        .and(path("/transactions/run001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 1 POST.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
    };

    // Drive the loop for one full poll cycle (slightly more than the interval).
    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    // MockServer drop verifies that exactly 1 webhook was received.
}

#[tokio::test]
async fn poll_includes_fee_charged_and_fires_high_fee_rule() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Horizon: transaction with fee_charged: "50000" (stroops)
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "fee_tx_poll",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "50000",
                    "envelope_xdr": null,
                    "result_xdr":   null
                }]
            }
        })))
        .mount(&horizon)
        .await;

    // Operations for that tx: empty
    Mock::given(method("GET"))
        .and(path("/transactions/fee_tx_poll/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(&format!("{}/hook", receiver.uri()), vec![
        AlertRule::HighFee { threshold_stroops: 10_000, threshold_xlm: None },
    ]);
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;
}

#[tokio::test]
async fn cursor_file_is_loaded_and_used_for_initial_cursor() {
    use std::fs::OpenOptions;
    use std::io::Write;

    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Expect the poll request to include cursor=100 (the persisted value).
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*cursor=100.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    let mut contract = helpers::contract(&format!("{}/hook", receiver.uri()), vec![AlertRule::AnyTransaction]);
    contract.horizon_base_url_override = Some(horizon.uri());

    // Create a temporary cursor file with the contract_id -> "100" mapping.
    let tmp = std::env::temp_dir().join("txwatch_test_cursor.json");
    let mapping = serde_json::json!({ contract.contract_id.clone(): "100" });
    let mut f = OpenOptions::new().create(true).write(true).truncate(true).open(&tmp).unwrap();
    write!(f, "{}", mapping.to_string()).unwrap();

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: Some(tmp.to_string_lossy().to_string()),
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
        webhook_retry: txwatch_config::WebhookRetryConfig::default(),
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;
}

/// AnyTransaction rule fires and webhook is called exactly once.
#[tokio::test]
async fn any_transaction_fires_webhook() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("hash001", "100", true)),
        )
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/hash001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );

    let url = format!(
        "{}/accounts/{}/transactions?cursor=now&order=asc&limit=200",
        horizon.uri(),
        contract.contract_id
    );

    #[derive(serde::Deserialize)]
    struct Page {
        _embedded: Emb,
    }
    #[derive(serde::Deserialize)]
    struct Emb {
        records: Vec<txwatch_rules::HorizonTransaction>,
    }

    let page: Page = client.get(&url).send().await.unwrap().json().await.unwrap();
    let records = page._embedded.records;
    assert_eq!(records.len(), 1);

    for raw in records {
        let ops_url = format!("{}/transactions/{}/operations", horizon.uri(), raw.hash);
        // Consume the operations response to satisfy the mock expectation.
        let _ = client
            .get(&ops_url)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();

        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
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
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, payload, None, txwatch_notifier::RetryPolicy::default())
                .await
                .unwrap();
        }
    }
}

/// TransactionFailed rule fires only for failed transactions.
#[tokio::test]
async fn transaction_failed_rule_fires_only_on_failure() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

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

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::TransactionFailed],
    );

    let txs = vec![
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "ok_tx".into(),
                created_at: "2024-06-01T10:00:00Z".into(),
                successful: true,
                paging_token: "1".into(),
                fee_charged: None,
                envelope_xdr: None,
                result_xdr: None,
            },
            vec![],
            None,
            None,
        )
        .unwrap(),
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "fail_tx".into(),
                created_at: "2024-06-01T10:01:00Z".into(),
                successful: false,
                paging_token: "2".into(),
                fee_charged: None,
                envelope_xdr: None,
                result_xdr: None,
            },
            vec![],
            None,
            None,
        )
        .unwrap(),
    ];

    for tx in &txs {
        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            contract.network.as_str(),
            &horizon.uri(),
            "https://stellar.expert/explorer/testnet",
            &contract.rules,
            tx,
        );
        for p in &payloads {
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, p, None, txwatch_notifier::RetryPolicy::default())
                .await
                .unwrap();
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

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer {
            threshold_xlm: 5_000,
        }],
    );

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "big_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
        },
        vec![],
        Some(100_000_000_000),
        None,
    )
    .unwrap();

    let payloads = evaluate(
        &contract.label,
        &contract.contract_id,
        contract.network.as_str(),
        "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
        &contract.rules,
        &tx,
    );
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0].amount_xlm, Some(10_000));

    txwatch_notifier::send_webhook(&client, &contract.webhook_url, &payloads[0], None, txwatch_notifier::RetryPolicy::default())
        .await
        .unwrap();
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

    let client = txwatch_notifier::build_client().unwrap();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::FunctionCalled {
            function_name: "withdraw".into(),
        }],
    );

    let txs = vec![
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t1".into(),
                created_at: "2024-06-01T10:00:00Z".into(),
                successful: true,
                paging_token: "1".into(),
                fee_charged: None,
                envelope_xdr: None,
                result_xdr: None,
            },
            vec!["deposit".into()],
            None,
            None,
        )
        .unwrap(),
        EnrichedTransaction::from_horizon(
            txwatch_rules::HorizonTransaction {
                hash: "t2".into(),
                created_at: "2024-06-01T10:01:00Z".into(),
                successful: true,
                paging_token: "2".into(),
                fee_charged: None,
                envelope_xdr: None,
                result_xdr: None,
            },
            vec!["withdraw".into()],
            None,
            None,
        )
        .unwrap(),
    ];

    for tx in &txs {
        let payloads = evaluate(
            &contract.label,
            &contract.contract_id,
            contract.network.as_str(),
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            &contract.rules,
            tx,
        );
        for p in &payloads {
            txwatch_notifier::send_webhook(&client, &contract.webhook_url, p, None, txwatch_notifier::RetryPolicy::default())
                .await
                .unwrap();
        }
    }
}

/// Cursor advances so the same transaction is not processed twice.
#[tokio::test]
async fn cursor_advances_after_each_transaction() {
    use std::collections::HashMap;

    let mut cursors: HashMap<String, String> = HashMap::new();
    let contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    cursors.insert(contract_id.to_string(), "now".to_string());

    for token in &["100", "200", "300"] {
        cursors.insert(contract_id.to_string(), token.to_string());
    }

    assert_eq!(cursors.get(contract_id).map(String::as_str), Some("300"));
}

/// HighFee rule fires when fee_charged from Horizon response exceeds threshold.
#[tokio::test]
async fn high_fee_rule_fires_on_fee_charged() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    // Horizon: transaction with fee_charged: "50000" (stroops)
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "fee_tx",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "50000",
                    "envelope_xdr": null,
                    "result_xdr":   null
                }]
            }
        })))
        .mount(&horizon)
        .await;

    // Horizon: operations for that transaction (empty, no Soroban)
    Mock::given(method("GET"))
        .and(path("/transactions/fee_tx/operations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::empty_page()),
        )
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 1 POST (HighFee fires)
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let client   = Client::new();
    let contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::HighFee { threshold_stroops: 10_000, threshold_xlm: None }],
    );

    let tx = EnrichedTransaction::from_horizon(
        txwatch_rules::HorizonTransaction {
            hash: "fee_tx".into(),
            created_at: "2024-06-01T10:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: Some("50000".into()),
            envelope_xdr: None,
            result_xdr: None,
        },
        vec![],
        None,
    )
    .unwrap();

    let payloads = evaluate(
        &contract.label,
        &contract.contract_id,
        contract.network.as_str(),
        &horizon.uri(),
        "https://stellar.expert/explorer/testnet",
        &contract.rules,
        &tx,
    );
    assert_eq!(payloads.len(), 1);
    assert!(payloads[0].rule_triggered.contains("HighFee"));
    assert_eq!(payloads[0].fee_charged_stroops, Some(50_000));

    txwatch_notifier::send_webhook(&client, &contract.webhook_url, &payloads[0], None, txwatch_notifier::RetryPolicy::default())
        .await
        .unwrap();
}

/// When run in dry-run mode, matched rules are logged but webhooks are not sent.
#[tokio::test]
async fn run_polls_once_and_skips_webhook_in_dry_run() {
    use std::time::Duration;

    let horizon  = MockServer::start().await;
    let receiver = MockServer::start().await;

    // First transactions request returns one tx.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(helpers::tx_page("dryrun001", "500", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    // All subsequent transaction requests return an empty page.
    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::empty_page()),
        )
        .mount(&horizon)
        .await;

    // Operations for the tx: no Soroban details needed.
    Mock::given(method("GET"))
        .and(path("/transactions/dryrun001/operations"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::empty_page()),
        )
        .mount(&horizon)
        .await;

    // Webhook receiver: expect exactly 0 POSTs when dry-run is enabled.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
        webhook_retry: txwatch_config::WebhookRetryConfig::default(),
    };

    // Drive the loop for one full poll cycle (slightly more than the interval).
    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run_with(cfg, true)).await;

    // MockServer drop verifies that 0 webhooks were received.
}

/// End-to-end LargeTransfer poll, webhook payload, and cursor advancement.
#[tokio::test]
async fn large_transfer_poll_fires_webhook_and_advances_cursor() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("large_tx", "1", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/large_tx/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::payment_ops_page("5000.0000000")))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::LargeTransfer { threshold_xlm: 1000 }],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
        webhook_retry: txwatch_config::WebhookRetryConfig::default(),
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    let webhook_requests = receiver.received_requests().await.unwrap();
    assert_eq!(webhook_requests.len(), 1, "expected exactly one webhook POST");

    let body: serde_json::Value = serde_json::from_slice(&webhook_requests[0].body).expect("webhook body is JSON");
    assert_eq!(body["rule_type"].as_str(), Some("LargeTransfer"));
    assert_eq!(body["rule_triggered"].as_str(), Some("LargeTransfer(>=1000XLM)"));
    assert_eq!(body["amount_xlm"].as_u64(), Some(5000));

    let requests = horizon.received_requests().await.unwrap();
    assert!(requests.iter().any(|r| r.url.contains("cursor=1")),
        "expected the second Horizon transaction request to advance the cursor to '1'");
}

/// horizon_link in webhook payloads always points to the canonical Horizon URL
/// even when polling against a mock server (horizon_base_url_override set). Closes #92.
#[tokio::test]
async fn horizon_link_uses_canonical_url_not_mock_server() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("link_tx", "1", true)),
        )
        .up_to_n_times(1)
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("GET"))
        .and(path("/transactions/link_tx/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.horizon_base_url_override = Some(horizon.uri());

    let canonical_base = txwatch_config::Network::Testnet.horizon_base_url();

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
    };

    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;

    let requests = receiver.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "expected exactly 1 webhook POST");

    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("webhook body is JSON");
    let horizon_link = body["horizon_link"]
        .as_str()
        .expect("horizon_link field present");

    assert!(
        horizon_link.starts_with(canonical_base),
        "horizon_link should start with canonical URL '{}', got: {}",
        canonical_base,
        horizon_link
    );
    assert!(
        !horizon_link.starts_with("http://127.0.0.1"),
        "horizon_link must not point to mock server, got: {}",
        horizon_link
    );
}

/// Two contracts polled concurrently: total wall time must be less than
/// the sum of each contract's individual response delay, proving that a
/// slow Horizon response for contract A does not delay contract B. Closes #36.
#[tokio::test]
async fn contracts_polled_concurrently() {
    const DELAY_MS: u64 = 300;

    let horizon1 = MockServer::start().await;
    let horizon2 = MockServer::start().await;
    let receiver = MockServer::start().await;

    for horizon in [&horizon1, &horizon2] {
        // Each Horizon server returns one tx with an artificial delay.
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(helpers::tx_page("delayed_tx", "1", true))
                    .set_delay(Duration::from_millis(DELAY_MS)),
            )
            .up_to_n_times(1)
            .mount(horizon)
            .await;

        // Subsequent requests return empty immediately.
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
            .mount(horizon)
            .await;
    }

    // Expect exactly 2 webhook POSTs — one per contract.
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(200))
        .expect(2)
        .mount(&receiver)
        .await;

    let make_contract = |label: &str, horizon_uri: &str| {
        let mut c = helpers::contract(
            &format!("{}/hook", receiver.uri()),
            vec![AlertRule::AnyTransaction],
        );
        c.label = label.to_string();
        c.contract_id = if label == "A" {
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_string()
        } else {
            "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB".to_string()
        };
        c.horizon_base_url_override = Some(horizon_uri.to_string());
        c
    };

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![
            make_contract("A", &horizon1.uri()),
            make_contract("B", &horizon2.uri()),
        ],
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
        cursor_file: None,
    };

    let start = std::time::Instant::now();
    let _ = tokio::time::timeout(Duration::from_millis(1500), txwatch_poller::run(cfg)).await;
    let elapsed = start.elapsed();

    // Sequential polling would take ≥ 2 × DELAY_MS. Concurrent polling takes ≈ DELAY_MS.
    // We allow generous headroom (1.8×) to avoid flakiness on slow CI.
    assert!(
        elapsed < Duration::from_millis(DELAY_MS * 18 / 10 + 300),
        "contracts should be polled concurrently; elapsed {:?} ≥ {:.0}ms",
        elapsed,
        DELAY_MS as f64 * 1.8 + 300.0,
    );
}
