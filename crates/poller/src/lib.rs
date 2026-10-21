//! txwatch-poller — Horizon polling loop, cursor tracking, and webhook dispatch.
//!
//! ## Module layout (issue #255)
//!
//! | Module | Responsibility |
//! |---|---|
//! | `horizon`    | Horizon REST API response shapes |
//! | `cursor`     | `CursorMap` newtype (issue #254) |
//! | `enrichment` | HTTP fallback for `/operations` fetch |
//! | `poller`     | `poll_contract`, `run`, `run_with`, `run_with_shutdown` |
//!
//! The public API is re-exported from this crate root so callers do not need
//! to know the internal module structure.

// ── Modules ───────────────────────────────────────────────────────────────────

pub mod cursor;
pub(crate) mod enrichment;
pub(crate) mod horizon;
pub mod poller;

// ── Optional Prometheus metrics ───────────────────────────────────────────────

#[cfg(feature = "metrics")]
pub mod metrics;

#[cfg(feature = "metrics")]
pub use metrics::serve_metrics;

// ── Public API re-exports ─────────────────────────────────────────────────────

pub use cursor::CursorMap;
pub use poller::{run, run_with, run_with_shutdown};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use reqwest::Client;
    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use txwatch_config::{AlertRule, AppConfig, Network, WatchedContract};

    use crate::cursor::CursorMap;
    use crate::enrichment::fetch_soroban_details;
    use crate::horizon::HorizonPage;
    use crate::poller::{poll_contract, startup_log_fields};

    fn empty_page() -> serde_json::Value {
        serde_json::json!({ "_embedded": { "records": [] } })
    }

    fn ops_page(function_name: &str) -> serde_json::Value {
        serde_json::json!({
            "_embedded": {
                "records": [{ "type": "invoke_host_function", "function": function_name }]
            }
        })
    }

    fn make_contract(
        label: &str,
        contract_id: &str,
        network: Network,
        webhook_url: &str,
        rules: Vec<AlertRule>,
        horizon_override: Option<String>,
    ) -> WatchedContract {
        WatchedContract {
            label: label.to_string(),
            contract_id: contract_id.to_string(),
            network,
            rules,
            webhook_url: webhook_url.to_string(),
            webhook_secret: None,
            horizon_base_url_override: horizon_override,
        }
    }

    const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

    /// Issue #23: when Horizon returns inline operations via join=operations,
    /// the poller must parse them correctly without making a separate /operations request.
    #[tokio::test]
    async fn inline_operations_parsed_correctly() {
        let server = MockServer::start().await;

        let tx_with_ops = serde_json::json!({
            "_embedded": {
                "records": [{
                    "hash":         "inlinetx1",
                    "created_at":   "2024-06-01T10:00:00Z",
                    "successful":   true,
                    "paging_token": "1",
                    "fee_charged":  "100",
                    "envelope_xdr": null,
                    "result_xdr":   null,
                    "operations": [
                        { "type": "invoke_host_function", "function": "withdraw" }
                    ]
                }]
            }
        });

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(tx_with_ops))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let receiver = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&receiver)
            .await;

        let client = Client::new();
        let contract = make_contract(
            "test", CONTRACT_ID, Network::Testnet,
            &format!("{}/hook", receiver.uri()),
            vec![AlertRule::FunctionCalled { function_name: "withdraw".into() }],
            Some(server.uri()),
        );
        let mut cursors = CursorMap::new_for_contracts(&[contract.clone()]);

        let (txs, alerts) = poll_contract(&client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 1);
        assert_eq!(alerts, 1, "FunctionCalled(withdraw) should fire from inline operations");

        let reqs = server.received_requests().await.unwrap();
        assert!(
            reqs.iter().all(|r| !r.url.path().contains("/operations")),
            "no separate /operations request should be made when inline ops are present"
        );
    }

    #[tokio::test]
    async fn poll_returns_ok_on_empty_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!(
            "{}/accounts/{}/transactions?cursor=now&order=asc&limit=200&join=operations",
            server.uri(), CONTRACT_ID
        );
        let page: HorizonPage = client.get(&url).send().await.unwrap().json().await.unwrap();
        assert!(page._embedded.records.is_empty());
    }

    #[tokio::test]
    async fn fetch_soroban_details_extracts_function_name() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops_page("withdraw")))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert_eq!(fn_names, vec!["withdraw"]);
        assert!(amount.is_none());
    }

    #[tokio::test]
    async fn fetch_soroban_details_extracts_payment_amount() {
        let server = MockServer::start().await;
        let ops = serde_json::json!({
            "_embedded": { "records": [{ "type": "payment", "amount": "1000.0000000" }] }
        });
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert!(fn_names.is_empty());
        assert_eq!(amount, Some(10_000_000_000));
    }

    #[tokio::test]
    async fn fetch_soroban_details_returns_none_on_empty_ops() {
        let server = MockServer::start().await;
        let ops = serde_json::json!({ "_embedded": { "records": [] } });
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(ops))
            .mount(&server)
            .await;

        let client = Client::new();
        let (fn_names, amount) =
            fetch_soroban_details(&client, &server.uri(), "abc123").await.unwrap();

        assert!(fn_names.is_empty());
        assert!(amount.is_none());
    }

    #[tokio::test]
    async fn horizon_429_returns_ok_after_backoff() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let client = Client::new();
        let mut cursors = CursorMap::new_for_contracts(&[make_contract(
            "test", CONTRACT_ID, Network::Testnet,
            "https://hooks.example.com/test", vec![], None,
        )]);
        let contract = make_contract(
            "test", CONTRACT_ID, Network::Testnet,
            "https://hooks.example.com/test",
            vec![AlertRule::AnyTransaction],
            Some(server.uri()),
        );

        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        assert!(result.is_ok(), "429 should return Ok after back-off");
        assert_eq!(result.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn poll_contract_does_not_advance_cursor_on_fetch_failure() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract = make_contract(
            "test", CONTRACT_ID, Network::Testnet,
            "https://hooks.example.com/test",
            vec![AlertRule::AnyTransaction],
            Some(server.uri()),
        );
        let mut cursors = CursorMap::new_for_contracts(&[contract.clone()]);

        let result = poll_contract(&client, &contract, &mut cursors, false).await;
        assert!(result.is_err(), "expected Err when Horizon returns 500");
        assert_eq!(
            cursors.get(CONTRACT_ID),
            "now",
            "cursor must not advance when the transactions fetch fails"
        );
    }

    #[test]
    fn startup_log_includes_version_contracts_list_and_networks() {
        let cfg = AppConfig {
            poll_interval_seconds: 10,
            http_pool_max_idle_per_host: None,
            http_tcp_keepalive_secs: None,
            http_connection_verbose: None,
            contracts: vec![
                make_contract(
                    "Contract A",
                    "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                    txwatch_config::Network::Testnet,
                    "https://hooks.example.com/a",
                    vec![txwatch_config::AlertRule::AnyTransaction],
                    None,
                ),
                make_contract(
                    "Contract B",
                    "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB",
                    txwatch_config::Network::Mainnet,
                    "https://hooks.example.com/b",
                    vec![txwatch_config::AlertRule::AnyTransaction],
                    None,
                ),
                make_contract(
                    "Contract C",
                    "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC",
                    txwatch_config::Network::Mainnet,
                    "https://hooks.example.com/c",
                    vec![txwatch_config::AlertRule::AnyTransaction],
                    None,
                ),
            ],
            cursor_file: None,
        };

        let (version, contracts_list, networks, horizon_urls) = startup_log_fields(&cfg);
        assert!(!version.is_empty());
        assert_eq!(contracts_list, "Contract A, Contract B, Contract C");
        assert_eq!(networks, "mainnet, testnet");
        assert!(horizon_urls.contains("mainnet=https://horizon.stellar.org"));
        assert!(horizon_urls.contains("testnet=https://horizon-testnet.stellar.org"));
    }

    #[tokio::test]
    async fn poll_handles_pagination_two_pages() {
        let server = MockServer::start().await;

        let mut records1 = Vec::new();
        for i in 1..=200u64 {
            records1.push(serde_json::json!({
                "hash": format!("tx{}", i),
                "created_at": "2020-01-01T00:00:00Z",
                "successful": true,
                "paging_token": format!("{}", i),
            }));
        }
        let page1 = serde_json::json!({ "_embedded": { "records": records1 } });
        let page2 = serde_json::json!({ "_embedded": { "records": [
            { "hash": "tx201", "created_at": "2020-01-01T00:00:01Z", "successful": true, "paging_token": "201" }
        ] } });

        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page1))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("/accounts/.*/transactions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page2))
            .mount(&server)
            .await;

        // Fallback /operations endpoint for transactions without inline ops
        Mock::given(method("GET"))
            .and(path_regex("/transactions/.*/operations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(empty_page()))
            .mount(&server)
            .await;

        let client = Client::new();
        let contract = make_contract(
            "Contract", CONTRACT_ID, Network::Testnet,
            &format!("{}/hooks", server.uri()),
            vec![AlertRule::AnyTransaction],
            Some(server.uri()),
        );
        let mut cursors = CursorMap::new_for_contracts(&[contract.clone()]);

        let (txs, alerts) = poll_contract(&client, &contract, &mut cursors, false).await.unwrap();
        assert_eq!(txs, 201);
        assert_eq!(alerts, 201);
    }

    // ── #256: warn when approaching the contract limit ────────────────────────

    /// Verifies that the warn_threshold calculation is correct: 80% of 100 = 80.
    #[test]
    fn warn_threshold_is_80_percent_of_max_contracts() {
        use txwatch_config::MAX_CONTRACTS;
        let warn_threshold = (MAX_CONTRACTS * 80) / 100;
        assert_eq!(warn_threshold, 80,
            "warn threshold should be 80 (80% of MAX_CONTRACTS=100)");
    }
}
