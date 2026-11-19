use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

#[test]
fn watch_help_documents_poll_once_flag() {
    let output = txwatch_bin()
        .args(["watch", "--help"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--poll-once"), "help should list --poll-once:\n{}", stdout);
}

#[test]
fn poll_once_still_validates_config_and_exits_one_when_invalid() {
    let path = env::temp_dir().join("txwatch_poll_once_invalid_config.toml");
    fs::write(&path, "poll_interval_seconds = 1\n").unwrap();

    let output = txwatch_bin()
        .arg("--config")
        .arg(&path)
        .args(["watch", "--poll-once"])
        .output()
        .expect("failed to run txwatch");
    let _ = fs::remove_file(&path);

    assert_eq!(output.status.code(), Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_receives_alert_fired_end_to_end() {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // Mock Horizon: return a single matching transaction for the account.
    let horizon = MockServer::start().await;
    let account = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF";
    let tx_hash = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

    let tx_json = serde_json::json!({
        "_links": {},
        "id": tx_hash,
        "paging_token": "1",
        "successful": true,
        "hash": tx_hash,
        "ledger": 1,
        "created_at": "2024-01-01T00:00:00Z",
        "source_account": account,
        "source_account_sequence": "1",
        "fee_account": account,
        "fee_charged": "100",
        "max_fee": "100",
        "operation_count": 1,
        "envelope_xdr": "",
        "result_xdr": "",
        "result_meta_xdr": "",
        "fee_meta_xdr": "",
        "memo_type": "none",
        "signatures": [],
        "valid_after": null,
        "valid_before": null,
        "fee_bump_transaction": null,
        "inner_transaction": null,
        "preconditions": null
    });

    let records = serde_json::json!({
        "_links": {},
        "_embedded": { "records": [tx_json] }
    });

    Mock::given(method("GET"))
        .and(path(format!("/accounts/{}/transactions", account)))
        .respond_with(ResponseTemplate::new(200).set_body_json(records))
        .mount(&horizon)
        .await;

    // Pick a free port for the SSE server.
    let probe = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream_addr = probe.local_addr().unwrap();
    drop(probe);

    // Write a config that points at the mocked Horizon and matches the account.
    let config_path = env::temp_dir().join(format!(
        "txwatch_stream_e2e_{}.toml",
        std::process::id()
    ));
    let config = format!(
        r#"
horizon_url = "{horizon}"
poll_interval_seconds = 1

[[rules]]
name = "e2e"
account = "{account}"
"#,
        horizon = horizon.uri(),
        account = account,
    );
    fs::write(&config_path, config).unwrap();

    let mut child = txwatch_bin()
        .arg("--config")
        .arg(&config_path)
        .args([
            "watch",
            "--poll-once",
            "--stream-addr",
            &stream_addr.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn txwatch");

    // Connect to the SSE endpoint, retrying until the server is up.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream: Option<TcpStream> = None;
    while Instant::now() < deadline {
        match TcpStream::connect(stream_addr) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut stream = stream.expect("could not connect to SSE server");

    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    // Read until we see a `data:` line (or timeout).
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    let read_deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_data = false;
    while Instant::now() < read_deadline {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if text.contains("data:") {
                    saw_data = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&config_path);

    let text = String::from_utf8_lossy(&buf);
    assert!(
        saw_data,
        "expected SSE client to receive a data: event, got:\n{}",
        text
    );
    assert!(
        text.contains(tx_hash),
        "expected SSE event to reference the fired transaction, got:\n{}",
        text
    );
}