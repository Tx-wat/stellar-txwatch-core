//! Integration test for `--dry-run` (Issue #524): matched rules are logged with a
//! `[dry-run]` marker, but no webhook is ever sent.
mod helpers;

use std::io::Write;
use std::sync::{Arc, Mutex};

use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_config::AppConfig;

/// Captures everything the tracing subscriber writes so the test can assert on it.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl LogBuffer {
    fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Builds a validated config whose single contract posts to `webhook_url` and
/// polls the mock Horizon at `horizon_url`.
fn dry_run_config(webhook_url: &str, horizon_url: &str) -> AppConfig {
    let raw = format!(
        r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Dry Run Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "{webhook_url}"
allow_internal_webhook_urls = true

  [[contracts.rules]]
  type = "AnyTransaction"
"#
    );
    let path = std::env::temp_dir().join(format!("txwatch_dry_run_{}.toml", std::process::id()));
    std::fs::write(&path, raw).unwrap();
    let mut cfg = AppConfig::from_file(&path).expect("config should be valid");
    let _ = std::fs::remove_file(&path);

    cfg.contracts[0].test_horizon_base_url = Some(horizon_url.to_string());
    cfg
}

#[tokio::test]
async fn dry_run_logs_matches_but_sends_no_webhooks() {
    let horizon = MockServer::start().await;
    let webhook = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path_regex("/accounts/.*/transactions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(helpers::tx_page("dry001", "700", true)),
        )
        .mount(&horizon)
        .await;
    Mock::given(method("GET"))
        .and(path("/transactions/dry001/operations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(helpers::empty_page()))
        .mount(&horizon)
        .await;

    // The webhook receiver must never be called in dry-run mode.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&webhook)
        .await;

    let cfg = dry_run_config(&format!("{}/hook", webhook.uri()), &horizon.uri());

    let logs = LogBuffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(logs.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let failed = txwatch_poller::run_once(cfg, true)
        .await
        .expect("run_once should succeed");
    assert_eq!(failed, 0, "the contract poll should succeed");

    let received = webhook.received_requests().await.unwrap_or_default();
    assert_eq!(received.len(), 0, "dry-run must not send any webhook requests");

    let output = logs.contents();
    assert!(
        output.contains("[dry-run]"),
        "expected a [dry-run] log line for the matched rule, got:\n{}",
        output
    );
    assert!(output.contains("dry001"), "log should reference the matched tx:\n{}", output);
}
