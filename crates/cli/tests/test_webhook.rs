use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    path::PathBuf,
    process::{Command, Output},
    thread::{self, JoinHandle},
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
