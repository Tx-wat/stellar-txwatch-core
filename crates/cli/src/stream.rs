use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{error, info};

/// Builds the SSE event payload for an alert message, embedding the webhook
/// idempotency key (Issue #578) so SSE consumers can deduplicate events.
///
/// The idempotency key is formatted as
/// `{contract_id}:{transaction_hash}:{rule_type}`. When the alert message is a
/// JSON object containing those fields they are used directly; otherwise the
/// key falls back to a hash of the raw message so every event still carries a
/// stable, deduplicable identifier.
fn build_sse_event(msg: &str) -> String {
    let idempotency_key = extract_idempotency_key(msg);
    let payload = serde_json::json!({
        "idempotency_key": idempotency_key,
        "data": msg,
    });
    format!("event: alert\ndata: {}\n\n", payload)
}

/// Derives the idempotency key from an alert message.
fn extract_idempotency_key(msg: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(msg) {
        let contract_id = value.get("contract_id").and_then(|v| v.as_str());
        let transaction_hash = value.get("transaction_hash").and_then(|v| v.as_str());
        let rule_type = value.get("rule_type").and_then(|v| v.as_str());
        if let (Some(contract_id), Some(transaction_hash), Some(rule_type)) =
            (contract_id, transaction_hash, rule_type)
        {
            return format!("{}:{}:{}", contract_id, transaction_hash, rule_type);
        }
    }
    // Fallback: stable hash of the raw message.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    msg.hash(&mut hasher);
    format!("unknown:unknown:{:016x}", hasher.finish())
}

/// Runs an HTTP Server-Sent Events (SSE) streaming server (Issue #355).
/// Clients can connect via `GET /stream` to receive real-time alert events.
pub async fn run_sse_server(
    addr: SocketAddr,
    expected_token: Option<String>,
    alert_rx: broadcast::Receiver<String>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "SSE streaming server listening on GET /stream");

    let expected_token = expected_token.map(Arc::new);

    tokio::spawn(async move {
        loop {
            let (mut socket, _peer) = match listener.accept().await {
                Ok(conn) => conn,
                Err(e) => {
                    error!(error = %e, "SSE listener accept failed");
                    continue;
                }
            };

            let token = expected_token.clone();
            let mut client_rx = alert_rx.resubscribe();

            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(socket);
                let mut buf_reader = BufReader::new(reader);

                let mut request_line = String::new();
                if buf_reader.read_line(&mut request_line).await.is_err() || request_line.is_empty() {
                    return;
                }

                let mut auth_ok = token.is_none();
                let mut line = String::new();
                loop {
                    line.clear();
                    if buf_reader.read_line(&mut line).await.is_err() || line.trim().is_empty() {
                        break;
                    }
                    if let Some(ref expected) = token {
                        if line.to_lowercase().starts_with("authorization:") {
                            let value = line["authorization:".len()..].trim();
                            if value == format!("Bearer {}", expected) || value == expected.as_str() {
                                auth_ok = true;
                            }
                        }
                    }
                }

                if !auth_ok {
                    let _ = writer
                        .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nUnauthorized")
                        .await;
                    return;
                }

                if !request_line.starts_with("GET /stream") {
                    let _ = writer
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nNot Found")
                        .await;
                    return;
                }

                let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nAccess-Control-Allow-Origin: *\r\n\r\n";
                if writer.write_all(headers.as_bytes()).await.is_err() {
                    return;
                }
                let _ = writer.flush().await;

                while let Ok(msg) = client_rx.recv().await {
                    let sse_msg = build_sse_event(&msg);
                    if writer.write_all(sse_msg.as_bytes()).await.is_err() {
                        break;
                    }
                    if writer.flush().await.is_err() {
                        break;
                    }
                }
            });
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn sse_event_includes_idempotency_key() {
        let msg = r#"{"contract_id":"C123","transaction_hash":"abc","rule_type":"large_transfer"}"#;
        let event = build_sse_event(msg);
        assert!(event.contains("\"idempotency_key\":\"C123:abc:large_transfer\""));
    }

    #[test]
    fn sse_event_has_alert_type_and_single_data_line() {
        let event = build_sse_event(r#"{"contract_id":"C1"}"#);
        assert!(event.starts_with("event: alert\ndata: "));
        assert!(event.ends_with("\n\n"));
        assert_eq!(event.matches("data: ").count(), 1);
    }

    #[tokio::test]
    async fn sse_server_streams_alerts_with_auth() {
        let (tx, rx) = broadcast::channel(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        run_sse_server(addr, Some("secret123".into()), rx).await.unwrap();

        // 1. Unauthorized request
        let mut unauth_client = TcpStream::connect(addr).await.unwrap();
        unauth_client.write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
        let mut resp = [0u8; 128];
        let n = unauth_client.read(&mut resp).await.unwrap();
        let response_str = String::from_utf8_lossy(&resp[..n]);
        assert!(response_str.contains("401 Unauthorized"));

        // 2. Authorized request
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer secret123\r\n\r\n").await.unwrap();

        // Send an alert
        tx.send("{\"alert\":\"test\"}".to_string()).unwrap();

        let mut buf = [0u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(resp.contains("200 OK"));
        assert!(resp.contains("text/event-stream"));

        // The alert itself arrives as an `event: alert` frame (may be in the same read).
        let mut full = resp.to_string();
        if !full.contains("event: alert") {
            let n = client.read(&mut buf).await.unwrap();
            full.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        assert!(full.contains("event: alert\ndata: "));
        assert!(full.contains("idempotency_key"));
    }
}
