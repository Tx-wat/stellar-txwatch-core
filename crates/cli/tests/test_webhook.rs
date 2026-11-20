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

use flate2::read::GzDecoder;
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

/// Writes a config identical to `VALID_CONFIG` but with `webhook_compress`
/// set on the contract.
fn write_compressed_config(file_name: &str) -> PathBuf {
    let config = VALID_CONFIG.replace(
        "webhook_url = \"https://hooks.example.com/test\"",
        "webhook_url = \"https://hooks.example.com/test\"\nwebhook_compress = true",
    );
    let path = env::temp_dir().join(file_name);
    fs::write(&path, config).unwrap();
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

/// Accepts one HTTP request on a loopback port, replies with the given status
/// line and response body, and returns the lowercased request headers together
/// with the raw request body.
fn spawn_body_receiver(
    status_line: &'static str,
    response_body: &'static str,
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
            "{status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
            response_body.len()
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
fn test_webhook_logs_response_body_on_failure() {
    let cfg_path = write_valid_config("txwatch_test_webhook_error_body.toml");
    let (url, receiver) = spawn_body_receiver(
        "HTTP/1.1 500 Internal Server Error",
        "upstream rejected the payload",
    );

    let output = run_test_webhook(&cfg_path, &url, &[]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 when webhook target returns HTTP 500, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("upstream rejected the payload"),
        "expected response body to be logged on failure, got: {combined}"
    );

    let _ = receiver.join();
}

#[test]
fn test_webhook_truncates_long_response_body_on_failure() {
    let cfg_path = write_valid_config("txwatch_test_webhook_error_body_long.toml");
    let long_body = "x".repeat(1024);
    let long_body: &'static str = Box::leak(long_body.into_boxed_str());
    let (url, receiver) = spawn_body_receiver("HTTP/1.1 500 Internal Server Error", long_body);

    let output = run_test_webhook(&cfg_path, &url, &[]);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit code 1 when webhook target returns HTTP 500, stdout: {}, stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        combined.contains("..."),
        "expected truncated response body to end with '...', got: {combined}"
    );
    assert!(
        !combined.contains(&"x".repeat(513)),
        "expected response body to be truncated to 512 bytes, got: {combined}"
    );

    let _ = receiver.join();
}

#[test]
fn test_webhook_does_not_log_response_body_on_success() {
    let cfg_path = write_valid_config("txwatch_test_webhook_success_body.toml");
    let (url, receiver) = spawn_body_receiver("HTTP/1.1 200 OK", "success-body-marker");

    let output = run_test_webhook(&cfg_path, &url, &[]);

    assert!(
        output.status.success(),
        "expected success, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !combined.contains("success-body-marker"),
        "expected response body not to be logged on success, got: {combined}"
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
    let mut mac = Hmac::<Sha256>::new_from_slice(
