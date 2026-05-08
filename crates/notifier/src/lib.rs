use anyhow::Result;
use reqwest::Client;
use txwatch_rules::AlertPayload;

const MAX_RETRIES: u32 = 3;

pub async fn send_webhook(client: &Client, url: &str, payload: &AlertPayload) -> Result<()> {
    let body = serde_json::to_string(payload)?;
    let mut last_err = None;

    for attempt in 1..=MAX_RETRIES {
        match client
            .post(url)
            .header("Content-Type", "application/json")
            .body(body.clone())
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => return Ok(()),
            Ok(resp) => {
                last_err = Some(anyhow::anyhow!("HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(e.into());
            }
        }

        if attempt < MAX_RETRIES {
            tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(attempt))).await;
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("webhook failed")))
}
