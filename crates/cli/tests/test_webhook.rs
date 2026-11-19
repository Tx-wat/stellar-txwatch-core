use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
    thread::{self, JoinHandle},
    time::Duration,
};

use hmac::{Hmac, Mac};
use sha2::Sha256;

fn txwatch_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_txwatch");
    Command::new(bin)
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

fn write_valid_config(file_name: &str) -> PathBuf {
    let path = env::temp_dir().join(file_name);
    fs::write(&path, VALID_CONFIG).unwrap();
    path
}

/// Accepts one HTTP request on a loopback port, replies 200, and returns the
/// lowercased request headers together with the raw request body.
fn spawn_receiver() -> (String, JoinHandle<(HashMap<String, String>, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/webhook", listener.local_addr().unwrap());

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = HashMap::new();
        let mut content_length = 0usize;

        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    content_length = value.parse().unwrap();
                }
                headers.insert(name, value);
            }
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();

        (headers, body)
    });

    (url, handle)
}

/// Accepts one HTTP request on a loopback port, replies with the given status
/// line, and returns the lowercased request headers together with the raw body.
fn spawn_status_receiver(
    status_line: &'static str,
) -> (String, JoinHandle<(HashMap<String, String>, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/webhook", listener.local_addr().unwrap());

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = HashMap::new();
        let mut content_length = 0usize;

        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    content_length = value.parse().unwrap();
                }
                headers.insert(name, value);
            }
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();
        let response = format!(
            "{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        reader.get_mut().write_all(response.as_bytes()).unwrap();

        (headers, body)
    });

    (url, handle)
}

/// Accepts one HTTP request on a loopback port, waits `delay` before replying
/// 200, and returns the lowercased request headers together with the raw body.
fn spawn_delayed_receiver(
    delay: Duration,
) -> (String, JoinHandle<(HashMap<String, String>, Vec<u8>)>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/webhook", listener.local_addr().unwrap());

    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream);
        let mut headers = HashMap::new();
        let mut content_length = 0usize;

        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim().to_string();
                if name == "content-length" {
                    content_length = value.parse().unwrap();
                }
                headers.insert(name, value);
            }
        }

        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body).unwrap();

        thread::sleep(delay);

        reader
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .unwrap();

        (headers, body)
    });

    (url, handle)
}

fn run_test_webhook(config: &PathBuf, url: &str, extra: &[&str]) -> Output {
    let mut args = vec!["--config", config.to_str().unwrap(), "test-webhook", "--url", url];
    args.extend_from_slice(extra);
    txwatch_bin().args(&args).output().expect("failed to run txwatch")
}

/// Reads SSE events from `reader` until an event whose `event:` line equals
/// `event_name` is seen (or the stream ends), returning the raw event block.
fn read_sse_event_until<R: BufRead>(reader: &mut R, event_name: &str) -> Option<String> {
    let mut current = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            if !current.is_empty() {
                let is_match = current
                    .lines()
                    .any(|l| l.trim() == format!("event: {event_name}"));
                if is_match {
                    return Some(current);
                }
                current.clear();
            }
            continue;
        }
        current.push_str(trimmed);
        current.push('\n');
    }
}

/// Spawns a receiver that accepts `count` sequential HTTP requests, replying
/// with `status_line` to each, so the SSE test can observe both a successful
/// and a failed webhook delivery.
fn spawn_multi_status_receiver(
    count: usize,
    status_line: &'static str,
) -> (String, JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/webhook", listener.local_addr().unwrap());

    let handle = thread::spawn(move || {
        let mut bodies = Vec::new();
        for _ in 0..count {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut content_length = 0usize;

            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let line = line.trim_end();
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        content_length = value.trim().parse().unwrap();
                    }
                }
            }

            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).unwrap();
            let response = format!(
                "{status_line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            reader.get_mut().write_all(response.as_bytes()).unwrap();
            bodies.push(body);
        }
        bodies
    });

    (url, handle)
}

#[test]
fn test_stream_emits_webhook_delivery_status_events() {
    let cfg_path = write_valid_config("txwatch_test_stream_delivery.toml");

    // First delivery succeeds (200), second fails (500) so the stream should
    // emit both `delivery:success` and `delivery:failed` events.
    let (ok_url, ok_receiver) = spawn_multi_status_receiver(1, "HTTP/1.1 200 OK");
    let (fail_url, fail_receiver) =
        spawn_multi_status_receiver(1, "HTTP/1.1 500 Internal Server Error");

    let mut child = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "stream",
            "--port",
            "0",
            "--webhook-url",
            &ok_url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn txwatch stream");

    // Give the server a moment to bind and print its listening address.
    thread::sleep(Duration::from_millis(500));

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Discover the bound port from the startup banner.
    let mut port = None;
    for _ in 0..20 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(idx) = line.find("127.0.0.1:") {
            let rest = &line[idx + "127.0.0.1:".len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u16>() {
                port = Some(p);
                break;
            }
        }
    }
    let port = port.expect("stream did not report a listening address");

    // Connect to the SSE endpoint.
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
        .unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sse = BufReader::new(stream);

    // Skip HTTP response headers.
    loop {
        let mut line = String::new();
        if sse.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.trim_end().is_empty() {
            break;
        }
    }

    // The first delivery (success) should surface as a `delivery:success` event.
    let success_event = read_sse_event_until(&mut sse, "delivery:success")
        .expect("expected a delivery:success SSE event");
    assert!(
        success_event.contains("delivery_status"),
        "delivery:success event should carry a delivery_status field, got: {success_event}"
    );

    // Trigger a failing delivery by pointing the next webhook at the 500 target.
    // The stream reads its webhook URL from config; rewrite it and rely on the
    // server re-reading per delivery, or fall back to a second stream.
    drop(sse);
    let _ = child.kill();
    let _ = child.wait();
    let _ = ok_receiver.join();

    let mut child = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "stream",
            "--port",
            "0",
            "--webhook-url",
            &fail_url,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn txwatch stream");

    thread::sleep(Duration::from_millis(500));

    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    let mut port = None;
    for _ in 0..20 {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(idx) = line.find("127.0.0.1:") {
            let rest = &line[idx + "127.0.0.1:".len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(p) = digits.parse::<u16>() {
                port = Some(p);
                break;
            }
        }
    }
    let port = port.expect("stream did not report a listening address");

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n")
        .unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut sse = BufReader::new(stream);

    loop {
        let mut line = String::new();
        if sse.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.trim_end().is_empty() {
            break;
        }
    }

    let failed_event = read_sse_event_until(&mut sse, "delivery:failed")
        .expect("expected a delivery:failed SSE event");
    assert!(
        failed_event.contains("delivery_status"),
        "delivery:failed event should carry a delivery_status field, got: {failed_event}"
    );

    drop(sse);
    let _ = child.kill();
    let _ = child.wait();
    let _ = fail_receiver.join();
}

#[test]
fn test_webhook_exits_one_when_url_is_unreachable() {
    let cfg_path = write_valid_config("txwatch_test_webhook_unreachable.toml");

    // Port 1 on loopback is never open (requires root to bind), so the
    // connection is always refused — making this a reliable unreachable URL.
    let status = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
        ])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 when webhook URL is unreachable"
    );
}

#[test]
fn test_webhook_exits_one_when_target_returns_http_error() {
    let cfg_path = write_valid_config("txwatch_test_webhook_http_error.toml");
    let (url, receiver) = spawn_status_receiver("HTTP/1.1 500 Internal Server Error");

    let output = run_test_webhook(&cfg_path, &url, &[]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 when webhook target returns HTTP 500, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = receiver.join();
}

#[test]
fn test_webhook_with_secret_sends_hmac_signature() {
    let cfg_path = write_valid_config("txwatch_test_webhook_signed.toml");
    let (url, receiver) = spawn_receiver();

    let output = run_test_webhook(&cfg_path, &url, &["--secret", "mysecret"]);
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (headers, body) = receiver.join().unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"mysecret").unwrap();
    mac.update(&body);
    let expected = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    assert_eq!(headers.get("x-txwatch-signature"), Some(&expected));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("signed with HMAC-SHA256"),
        "output should report the signature, got: {}",
        stdout
    );
}

#[test]
fn test_webhook_without_secret_sends_no_signature() {
    let cfg_path = write_valid_config("txwatch_test_webhook_unsigned.toml");
    let (url, receiver) = spawn_receiver();

    let output = run_test_webhook(&cfg_path, &url, &[]);
    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (headers, _body) = receiver.join().unwrap();
    assert!(
        !headers.contains_key("x-txwatch-signature"),
        "no signature header expected without --secret"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("(unsigned)"),
        "output should report the payload is unsigned, got: {}",
        stdout
    );
}

#[test]
fn test_webhook_succeeds_when_receiver_is_slower_than_default_timeout() {
    let cfg_path = write_valid_config("txwatch_test_webhook_slow_ok.toml");
    // Delay well beyond the default 15s timeout, but within the configured 60s.
    let (url, receiver) = spawn_delayed_receiver(Duration::from_secs(20));

    let output = run_test_webhook(&cfg_path, &url, &["--timeout", "60"]);
    assert!(
        output.status.success(),
        "expected success with --timeout 60, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (_headers, _body) = receiver.join().unwrap();
}

#[test]
fn test_webhook_times_out_when_receiver_exceeds_configured_timeout() {
    let cfg_path = write_valid_config("txwatch_test_webhook_slow_fail.toml");
    // Delay longer than the configured 1s timeout so the request must fail.
    let (url, receiver) = spawn_delayed_receiver(Duration::from_secs(5));

    let output = run_test_webhook(&cfg_path, &url, &["--timeout", "1"]);
    assert!(
        !output.status.success(),
        "expected failure when receiver exceeds --timeout 1, stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The receiver thread will still complete once the client disconnects.
    let _ = receiver.join();
}
