use anyhow::{anyhow, Result};
use chrono::Utc;
use reqwest::Client;
use std::time::Duration;
use tracing::{error, info, warn};
use txwatch_rules::AlertPayload;

const MAX_RETRIES: u32 = 3;

/// POST `payload` to `url`, retrying up to `MAX_RETRIES` times with
/// exponential backoff (2 s → 4 s → 8 s). Logs each attempt.
/// If `secret` is Some, adds an `X-TxWatch-Secret` header to every request.
pub async fn send_webhook(
    client:  &Client,
    url:     &str,
    payload: &AlertPayload,
    secret:  Option<&str>,
) -> Result<()> {
    let body = serde_json::to_string(payload)?;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 1..=MAX_RETRIES {
        let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");

        let mut req = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("X-TxWatch-Version", env!("CARGO_PKG_VERSION"))
            .body(body.clone());
        if let Some(s) = secret {
            req = req.header("X-TxWatch-Secret", s);
        }
        match req.send().await
        {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    timestamp = %ts,
                    url       = %url,
                    rule      = %payload.rule_triggered,
                    tx        = %payload.transaction_hash,
                    "webhook delivered"
                );
                return Ok(());
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
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
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
    AlertPayload {
        label:            label.to_string(),
        contract_id:      "CTEST000000000000000000000000000000000000000000000000000".into(),
        network:          "testnet".into(),
        rule_triggered:   "TestWebhook".into(),
        transaction_hash: "0000000000000000000000000000000000000000000000000000000000000000".into(),
        function_name:    Some("test".into()),
        amount_xlm:       None,
        timestamp:        Utc::now().timestamp(),
        horizon_link:     format!(
            "https://horizon-testnet.stellar.org/transactions/\
             0000000000000000000000000000000000000000000000000000000000000000"
        ),
        explorer_link:    "https://stellar.expert/explorer/testnet/tx/0000000000000000000000000000000000000000000000000000000000000000".into(),
    }
    // suppress unused webhook_url warning — callers use it to POST
    // but we include it in the payload label for clarity
    .with_label(format!("{} (test-webhook to {})", label, webhook_url))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn sample_payload() -> AlertPayload {
        AlertPayload {
            label:            "Test Contract".into(),
            contract_id:      "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network:          "testnet".into(),
            rule_triggered:   "AnyTransaction".into(),
            transaction_hash: "abc123".into(),
            function_name:    None,
            amount_xlm:       None,
            timestamp:        1_700_000_000,
            horizon_link:     "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link:    "https://stellar.expert/explorer/testnet/tx/abc123".into(),
        }
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

        let client = Client::new();
        let url = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn retries_on_server_error_then_succeeds() {
        let server = MockServer::start().await;
        // First call returns 500, second returns 200
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

        let client = Client::new();
        let url = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn fails_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/hook"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let client = Client::new();
        let url = format!("{}/hook", server.uri());
        let result = send_webhook(&client, &url, &sample_payload(), None).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_payload_builds_without_panic() {
        let p = test_payload("My Contract", "https://example.com/hook");
        assert!(p.label.contains("My Contract"));
        assert_eq!(p.rule_triggered, "TestWebhook");
    }
}
