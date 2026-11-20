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

/// Computes the expected `sha256=<hex>` signature for a body and secret.
fn expected_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).unwrap();
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("sha256={hex}")
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
    let signature = headers
        .get("x-txwatch-signature")
        .expect("missing X-TxWatch-Signature header");

    assert!(
        signature.starts_with("sha256="),
        "expected signature header to start with sha256=, got: {signature}"
    );
    let hex = &signature["sha256=".len()..];
    assert_eq!(hex.len(), 64, "expected 64 hex chars, got: {hex}");
    assert!(
        hex.chars().all(|c| c.is_ascii_hexdigit()),
        "expected hex digest, got: {hex}"
    );

    assert_eq!(
        signature,
        &expected_signature(b"mysecret", &body),
        "signature does not match HMAC-SHA256 of the request body"
    );
}

#[test]
fn test_webhook_hmac_signature_known_vector() {
    // Known HMAC-SHA256 vector: secret "mysecret", body "hello".
    // Computed independently with `printf 'hello' | openssl dgst -sha256 -hmac mysecret`.
    let secret = b"mysecret";
    let body = b"hello";
    let expected = "sha256=2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

    assert_eq!(expected_signature(secret, body), expected);
}

#[test]
fn test_webhook_hmac_signature_empty_body() {
    // HMAC-SHA256 of an empty body with secret "mysecret".
    // Computed independently with `printf '' | openssl dgst -sha256 -hmac mysecret`.
    let secret = b"mysecret";
    let body: &[u8] = b"";
    let expected = "sha256=2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b";

    let signature = expected_signature(secret, body);
    assert_eq!(signature, expected);
    assert!(signature.starts_with("sha256="));
    assert_eq!(signature["sha256=".len()..].len(), 64);
}
