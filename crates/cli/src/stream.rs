use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tracing::{error, info};

/// Webhook delivery status carried on the SSE stream (Issue #545).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStatus {
    /// The webhook was delivered successfully.
    Success,
    /// The webhook delivery failed (may be retried).
    Failed,
}

impl DeliveryStatus {
    /// Returns the SSE event name for this delivery status.
    pub fn event_name(&self) -> &'static str {
        match self {
            DeliveryStatus::Success => "delivery:success",
            DeliveryStatus::Failed => "delivery:failed",
        }
    }
}

/// An SSE event emitted on the stream.
///
/// Alert events are emitted as plain `data:` frames, while webhook delivery
/// status events are emitted as named events (`delivery:success` /
/// `delivery:failed`) with a JSON payload containing `delivery_status`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// A raw alert payload, emitted as `data: <payload>\n\n`.
    Alert(String),
    /// A webhook delivery status event, emitted as
    /// `event: delivery:success\ndata: {...}\n\n` (or the failed variant).
    Delivery {
        /// Delivery status of the webhook.
        delivery_status: DeliveryStatus,
        /// Optional webhook identifier.
        webhook_id: Option<String>,
        /// Optional human-readable detail (e.g. error message).
        detail: Option<String>,
    },
}

impl SseEvent {
    /// Creates a successful delivery event.
    pub fn delivery_success(webhook_id: Option<String>, detail: Option<String>) -> Self {
        SseEvent::Delivery {
            delivery_status: DeliveryStatus::Success,
            webhook_id,
            detail,
        }
    }

    /// Creates a failed delivery event.
    pub fn delivery_failed(webhook_id: Option<String>, detail: Option<String>) -> Self {
        SseEvent::Delivery {
            delivery_status: DeliveryStatus::Failed,
            webhook_id,
            detail,
        }
    }

    /// Renders this event as an SSE frame.
    pub fn to_sse_frame(&self) -> String {
        match self {
            SseEvent::Alert(payload) => format!("data: {}\n\n", payload),
            SseEvent::Delivery {
                delivery_status,
                webhook_id,
                detail,
            } => {
                let status = match delivery_status {
                    DeliveryStatus::Success => "success",
                    DeliveryStatus::Failed => "failed",
                };
                let mut data = format!("{{\"delivery_status\":\"{}\"", status);
                if let Some(id) = webhook_id {
                    data.push_str(&format!(",\"webhook_id\":{}", json_string(id)));
                }
                if let Some(detail) = detail {
                    data.push_str(&format!(",\"detail\":{}", json_string(detail)));
                }
                data.push('}');
                format!("event: {}\ndata: {}\n\n", delivery_status.event_name(), data)
            }
        }
    }
}

/// Minimal JSON string escaping for SSE payloads.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Runs an HTTP Server-Sent Events (SSE) streaming server (Issue #355).
/// Clients can connect via `GET /stream` to receive real-time alert events.
///
/// Event types emitted on the stream (Issue #545):
/// - Default (unnamed) events: `data: <alert payload>\n\n` for alert events.
/// - `delivery:success`: `event: delivery:success\ndata: {"delivery_status":"success",...}\n\n`
/// - `delivery:failed`: `event: delivery:failed\ndata: {"delivery_status":"failed",...}\n\n`
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
                    let sse_msg = format!("data: {}\n\n", msg);
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

/// Runs an HTTP Server-Sent Events (SSE) streaming server that emits both
/// alert events and webhook delivery status events (Issue #545).
///
/// Alert payloads are emitted as plain `data:` frames, while delivery status
/// events are emitted as named `delivery:success` / `delivery:failed` events.
pub async fn run_sse_server_with_delivery(
    addr: SocketAddr,
    expected_token: Option<String>,
    mut event_rx: broadcast::Receiver<SseEvent>,
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
            let mut client_rx = event_rx.resubscribe();

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

                while let Ok(event) = client_rx.recv().await {
                    let sse_msg = event.to_sse_frame();
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
    }

    #[test]
    fn alert_event_renders_as_data_frame() {
        let event = SseEvent::Alert("{\"alert\":\"test\"}".to_string());
        assert_eq!(event.to_sse_frame(), "data: {\"alert\":\"test\"}\n\n");
    }

    #[test]
    fn delivery_success_event_renders_named_frame() {
        let event = SseEvent::delivery_success(Some("wh-1".to_string()), None);
        let frame = event.to_sse_frame();
        assert!(frame.starts_with("event: delivery:success\ndata: "));
        assert!(frame.contains("\"delivery_status\":\"success\""));
        assert!(frame.contains("\"webhook_id\":\"wh-1\""));
        assert!(frame.ends_with("\n\n"));
    }

    #[test]
    fn delivery_failed_event_renders_named_frame() {
        let event = SseEvent::delivery_failed(None, Some("timeout".to_string()));
        let frame = event.to_sse_frame();
        assert!(frame.starts_with("event: delivery:failed\ndata: "));
        assert!(frame.contains("\"delivery_status\":\"failed\""));
        assert!(frame.contains("\"detail\":\"timeout\""));
    }

    #[tokio::test]
    async fn sse_server_streams_delivery_events() {
        let (tx, rx) = broadcast::channel(16);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        run_sse_server_with_delivery(addr, None, rx).await.unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /stream HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .await
            .unwrap();

        tx.send(SseEvent::Alert("{\"alert\":\"test\"}".to_string()))
            .unwrap();
        tx.send(SseEvent::delivery_success(Some("wh-1".to_string()), None))
            .unwrap();
        tx.send(SseEvent::delivery_failed(None, Some("timeout".to_string())))
            .unwrap();

        let mut resp = String::new();
        let mut buf = [0u8; 512];
        // Read until we have seen both delivery events.
        for _ in 0..8 {
            let n = client.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            resp.push_str(&String::from_utf8_lossy(&buf[..n]));
            if resp.contains("delivery:success") && resp.contains("delivery:failed") {
                break;
            }
        }

        assert!(resp.contains("200 OK"));
        assert!(resp.contains("text/event-stream"));
        assert!(resp.contains("data: {\"alert\":\"test\"}"));
        assert!(resp.contains("event: delivery:success"));
        assert!(resp.contains("\"delivery_status\":\"success\""));
        assert!(resp.contains("event: delivery:failed"));
        assert!(resp.contains("\"delivery_status\":\"failed\""));
    }
}
