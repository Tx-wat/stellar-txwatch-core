//! Integration test for `--dry-run` (#524): matched rules are logged with a
//! `[dry-run]` marker, but no webhook is ever sent.

mod helpers;

use tracing_test::traced_test;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_config::{AlertRule, AppConfig};

#[tokio::test]
#[traced_test]
async fn dry_run_logs_matches_but_sends_no_webhooks() {
    let horizon = MockServer::start().await;
    let receiver = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("dry001", "700", true)),
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
        .and(path("/transactions/dry001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // Any POST to the webhook receiver is a failure in dry-run mode.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&receiver)
        .await;

    let mut contract = helpers::contract(
        &format!("{}/hook", receiver.uri()),
        vec![AlertRule::AnyTransaction],
    );
    contract.test_horizon_base_url = Some(horizon.uri());

    let cfg = AppConfig {
        poll_interval_seconds: 1,
        contracts: vec![contract],
        cursor_file: None,
        max_alerts_per_cycle: None,
        http_pool_max_idle_per_host: None,
        http_tcp_keepalive_secs: None,
        http_connection_verbose: None,
    };

    let failed = txwatch_poller::run_once(cfg, true)
        .await
        .expect("dry-run poll cycle should succeed");
    assert_eq!(failed, 0);

    let received = receiver.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 0, "dry-run must not send any webhooks");
    assert!(logs_contain("[dry-run]"), "dry-run match should be logged");
}
