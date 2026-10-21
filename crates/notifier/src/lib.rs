#![forbid(unsafe_code)]

//! txwatch-notifier delivers webhook payloads over HTTP with retry and backoff,
//! exposing `send_webhook` and `test_payload` helpers for webhook delivery.

use anyhow::{anyhow, Result};
use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::oneshot;
use tracing::{debug, error, info, span, warn, Level};
use txwatch_rules::AlertPayload;

const MAX_RETRIES: u32 = 3;

/// Structured result returned by a successful `send_webhook` call.
#[derive(Debug, PartialEq)]
pub struct DeliveryResult {
    /// Number of attempts made (1 = delivered on first try).
    pub attempts: u32,
    /// HTTP status code of the successful response.
    pub final_status: u16,
}

/// Build a shared HTTP client with sensible defaults.
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {}", e))
}

/// POST `payload` to `url`, retrying up to `MAX_RETRIES` times with
/// exponential backoff (2 s → 4 s → 8 s). Logs each attempt.
/// If `secret` is Some, adds an `X-TxWatch-Secret` header to every request.
///
/// Delivery semantics: HTTP 2xx = success. The response body is logged at
/// debug level but is otherwise ignored — a 200 OK with an error body is
/// still treated as a successful delivery (#24).
///
/// Returns a [`DeliveryResult`] describing how many attempts were needed.
pub async fn send_webhook(
    client: &Client,
    url: &str,
    payload: &AlertPayload,
    secret: Option<&str>,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<DeliveryResult> {
    let span = span!(Level::INFO, "send_webhook", contract = %payload.label, rule = %payload.rule_triggered);
    let _enter = span.enter();

    let body = serde_json::to_string(payload)?;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_else(|_| Duration::from_secs(0))
            .as_secs();

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-TxWatch-Version", env!("CARGO_PKG_VERSION"))
            .body(body.clone());
        if let Some(s) = secret {
            let mut mac = Hmac::<Sha256>::new_from_slice(s.as_bytes())
                .expect("HMAC accepts any key length");
            mac.update(body.as_bytes());
            let sig = hex::encode(mac.finalize().into_bytes());
            req = req.header("X-TxWatch-Signature", format!("sha256={}", sig));
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                let final_status = resp.status().as_u16();
                // Issue #24: log response body at debug level.
                // HTTP 2xx = delivery success regardless of body content.
                // Some receivers return 200 OK with an error body; we treat
                // any 2xx as a successful delivery (body is informational only).
                let body_text = resp.text().await.unwrap_or_default();
                debug!(
                    timestamp     = %ts,
                    url           = %url,
                    status        = final_status,
                    response_body = %body_text,
                    "webhook 2xx response body"
                );
                info!(
                    timestamp = %ts,
                    url       = %url,
                    rule      = %payload.rule_triggered,
                    tx        = %payload.transaction_hash,
                    attempts  = attempt,
                    "webhook delivered"
                );
                return Ok(DeliveryResult { attempts: attempt, final_status });
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    status    = %status,
                    "webhook attempt failed with HTTP error"
                );
                last_err = Some(anyhow!("HTTP {}", status));
            }
            Err(e) => {
                warn!(
                    timestamp = %ts,
                    attempt   = attempt,
                    url       = %url,
                    error     = %e,
                    "webhook attempt failed with network error"
                );
                last_err = Some(e.into());
            }
        }

        if attempt < MAX_RETRIES {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))) => {}
                _ = &mut shutdown => {
                    return Err(anyhow!("webhook retry aborted: shutdown signal received"));
                }
            }
        }
    }

    let err = last_err.unwrap_or_else(|| anyhow!("webhook failed after {} retries", MAX_RETRIES));
    error!(
        url  = %url,
        rule = %payload.rule_triggered,
        tx   = %payload.transaction_hash,
        "webhook delivery failed permanently: {}",
        err
    );
    Err(err)
}

/// Build a synthetic `AlertPayload` suitable for `test-webhook`.
pub fn test_payload(label: &str, webhook_url: &str) -> AlertPayload {
    test_payload_with_network(label, webhook_url, "testnet", "https://horizon-testnet.stellar.org")
}

/// Build a synthetic `AlertPayload` with an explicit network name and Horizon base URL.
pub fn test_payload_with_network(
    label: &str,
    webhook_url: &str,
    network: &str,
    horizon_base_url: &str,
) -> AlertPayload {
    let now     = Utc::now();
    let tx_hash = "0000000000000000000000000000000000000000000000000000000000000000";
    AlertPayload {
        label:               label.to_string(),
        contract_id:         "CTEST000000000000000000000000000000000000000000000000000".into(),
        network:             network.to_string(),
        rule_type:           "TestWebhook".into(),
        rule_triggered:      "TestWebhook".into(),
        transaction_hash:    tx_hash.into(),
        function_name:       Some("test".into()),
        function_names:      vec!["test".into()],
        amount_xlm:          None,
        fee_charged_stroops: None,
        timestamp:           now.timestamp(),
        timestamp_iso:       now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        horizon_link:        format!("{}/transactions/{}", horizon_base_url, tx_hash),
        explorer_link:       format!("https://stellar.expert/explorer/{}/tx/{}", network, tx_hash),
    }
    .with_label(format!("{} (test-webhook to {})", label, webhook_url))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_payload() -> AlertPayload {
        AlertPayload {
            label:               "Test Contract".into(),
            contract_id:         "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:             "testnet".into(),
            rule_type:           "AnyTransaction".into(),
            rule_triggered:      "AnyTransaction".into(),
            transaction_hash:    "abc123".into(),
            function_name:       None,
            function_names:      vec![],
            amount_xlm:          None,
            fee_charged_stroops: None,
            timestamp:           1_700_000_000,
            timestamp_iso:       "2023-11-15T03:13:20Z".into(),
            horizon_link:        "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link:       "https://stellar.expert/explorer/testnet/tx/abc123".into(),
        }
    }

    fn dummy_shutdown() -> oneshot::Receiver<()> {
        oneshot::channel().1
    }

    #[tokio::test]
    async fn delivers_on_first_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
        let delivery = result.unwrap();
        assert_eq!(delivery.attempts, 1);
        assert_eq!(delivery.final_status, 200);
    }

    #[tokio::test]
    async fn retries_on_server_error_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn attempts_is_two_when_first_fails_and_second_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client   = Client::new();
        let url      = format!("{}/hook", server.uri());
        let delivery = send_webhook(&client, &url, &sample_payload(), None)
            .await
            .expect("should succeed on second attempt");
        assert_eq!(delivery.attempts, 2);
        assert_eq!(delivery.final_status, 200);
    }

    /// Issue #24: a 200 response with an error body must still be treated as success.
    /// The body is logged at debug level but does not affect delivery outcome.
    #[tokio::test]
    async fn success_with_error_body_is_still_treated_as_success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"error":"something went wrong"}"#),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client   = build_client().unwrap();
        let url      = format!("{}/hook", server.uri());
        let delivery = send_webhook(&client, &url, &sample_payload(), None)
            .await
            .expect("200 with error body should be treated as success");
        assert_eq!(delivery.final_status, 200);
        assert_eq!(delivery.attempts, 1);
    }

    #[tokio::test]
    async fn signature_header_present_when_provided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        send_webhook(&client, &url, &sample_payload(), Some("mysecret")).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.contains_key("x-txwatch-secret"));
        assert_eq!(requests[0].headers.get("x-txwatch-secret").unwrap(), "mysecret");
    }

    #[tokio::test]
    async fn signature_header_absent_when_not_provided() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        send_webhook(&client, &url, &sample_payload(), None).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("x-txwatch-secret"));
    }

    #[tokio::test]
    async fn signature_header_is_correct_hmac() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let payload = sample_payload();
        let body = serde_json::to_string(&payload).unwrap();
        let secret = "test-secret";

        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body.as_bytes());
        let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

        let client = build_client().unwrap();
        let url = format!("{}/hook", server.uri());
        send_webhook(&client, &url, &payload, Some(secret)).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let sig = requests[0].headers.get("x-txwatch-signature").unwrap();
        assert_eq!(sig, &expected);
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("shutdown"), "error should mention shutdown, got: {}", msg);
    }

    /// Issue #13: test_payload produces a structurally valid AlertPayload (56-char contract ID).
    #[test]
    fn test_payload_is_structurally_valid() {
        let p = test_payload("My Contract", "https://example.com/hook");
        assert!(p.label.contains("My Contract"));
        assert_eq!(p.rule_triggered, "TestWebhook");
        assert_eq!(p.contract_id.len(), 56, "contract_id must be 56 characters");
        assert!(p.contract_id.starts_with('C'), "contract_id must start with 'C'");
        assert!(p.horizon_link.contains("/transactions/"), "horizon_link must contain /transactions/");
        assert!(p.explorer_link.contains("stellar.expert"), "explorer_link must point to stellar.expert");
    }

    /// Issue #13: test_payload_with_network derives links from the supplied network config.
    #[test]
    fn test_payload_with_network_derives_links_from_config() {
        let p = test_payload_with_network(
            "Label",
            "https://example.com/hook",
            "mainnet",
            "https://horizon.stellar.org",
        );
        assert!(p.horizon_link.starts_with("https://horizon.stellar.org/transactions/"));
        assert!(p.explorer_link.contains("/mainnet/"));
    }

    #[tokio::test]
    async fn version_header_is_present_on_every_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("X-TxWatch-Version", env!("CARGO_PKG_VERSION")))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = build_client().unwrap();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn content_length_header_is_present_and_correct() {
        let server = MockServer::start().await;
        let body   = serde_json::to_string(&sample_payload()).unwrap();

        Mock::given(method("POST"))
            .and(path("/hook"))
            .and(header("content-length", body.len().to_string()))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let client = Client::new();
        let url    = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }
}
