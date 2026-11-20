use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use tempfile::TempDir;
use txwatch_config::AppConfig;

fn project_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crate parent missing")
        .parent()
        .expect("workspace root missing")
        .to_path_buf()
}

fn crate_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn example_toml_parses_and_validates_successfully() {
    let path = project_root().join("config").join("example.toml");
    let cfg = AppConfig::from_file(&path)
        .expect("example.toml should parse and validate successfully");

    assert_eq!(cfg.poll_interval_seconds, 10);
    assert!(!cfg.contracts.is_empty(), "example config should define contracts");
}

#[test]
fn broken_toml_fixture_fails_with_meaningful_error() {
    let path = crate_fixture_path("broken-example.toml");
    let err = AppConfig::from_file(&path).expect_err("broken TOML fixture should fail");
    let msg = err.to_string();

    assert!(
        msg.contains("threshold_xlm must be > 0"),
        "expected a validation failure, got: {}",
        msg
    );
}

#[test]
fn webhook_rule_filter_defaults_to_empty() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100.0
"#;
    let cfg = AppConfig::from_str(toml).expect("config should parse");
    assert_eq!(cfg.contracts.len(), 1);
    assert!(
        cfg.contracts[0].webhook_rule_filter.is_empty(),
        "webhook_rule_filter should default to an empty list"
    );
}

#[test]
fn webhook_rule_filter_parses_populated_list() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100.0
webhook_rule_filter = ["large_transfer", "admin_call"]
"#;
    let cfg = AppConfig::from_str(toml).expect("config should parse");
    assert_eq!(cfg.contracts.len(), 1);
    assert_eq!(
        cfg.contracts[0].webhook_rule_filter,
        vec!["large_transfer".to_string(), "admin_call".to_string()]
    );
}

#[test]
fn per_contract_horizon_url_parses() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100.0
horizon_url = "https://private-horizon.example.com"

[[contracts]]
contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
threshold_xlm = 50.0
"#;
    let cfg = AppConfig::from_str(toml).expect("config should parse");
    assert_eq!(cfg.contracts.len(), 2);
    assert_eq!(
        cfg.contracts[0].horizon_url.as_deref(),
        Some("https://private-horizon.example.com")
    );
    assert!(
        cfg.contracts[1].horizon_url.is_none(),
        "contract without horizon_url override should have None"
    );
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100
"#;

const VALID_CONFIG_UPDATED: &str = r#"
poll_interval_seconds = 20

[[contracts]]
id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100
"#;

fn write_config(dir: &TempDir, contents: &str) -> PathBuf {
    let path = dir.path().join("config.toml");
    std::fs::write(&path, contents).expect("failed to write config file");
    path
}

/// Build a valid 56-character base32 contract ID (Stellar strkey format).
///
/// The ID starts with the `C` version byte and is padded with `A` characters
/// (base32 value 0) so that every generated ID is syntactically valid.
fn contract_id(index: usize) -> String {
    // Encode the index into the trailing characters so IDs stay unique while
    // remaining valid base32 (A-Z, 2-7).
    let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut suffix = [b'A'; 8];
    let mut n = index;
    for slot in suffix.iter_mut().rev() {
        *slot = alphabet[n % alphabet.len()];
        n /= alphabet.len();
    }
    let suffix = std::str::from_utf8(&suffix).expect("base32 suffix is valid utf8");
    format!("C{}{}", "A".repeat(47), suffix)
}

fn cfg_with_n_contracts(n: usize) -> String {
    let mut cfg = String::from("poll_interval_seconds = 10\n");
    for i in 0..n {
        cfg.push_str(&format!(
            "\n[[contracts]]\nid = \"{}\"\nthreshold_xlm = 100\n",
            contract_id(i)
        ));
    }
    cfg
}

#[test]
fn accepts_exactly_max_contracts() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let path = write_config(&dir, &cfg_with_n_contracts(100));

    let cfg = AppConfig::from_file(&path)
        .expect("config with exactly MAX_CONTRACTS (100) should validate");
    assert_eq!(cfg.contracts.len(), 100);
}

#[test]
fn contract_at_index_99_is_valid() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let path = write_config(&dir, &cfg_with_n_contracts(100));

    let cfg = AppConfig::from_file(&path)
        .expect("config with exactly MAX_CONTRACTS (100) should validate");

    let last = cfg.contracts.last().expect("config should contain contracts");
    assert_eq!(last.id, contract_id(99));
    assert_eq!(last.id.len(), 56, "contract ID must be 56 characters");
    assert!(last.id.starts_with('C'), "contract ID must start with 'C'");
}

#[test]
fn watcher_emits_reload_on_write() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let path = write_config(&dir, VALID_CONFIG);

    let (tx, rx) = mpsc::channel();
    let _watcher = txwatch_config::watch_config(&path, move |result| {
        let _ = tx.send(result);
    })
    .expect("failed to start config watcher");

    // Give the watcher a moment to register with inotify.
    std::thread::sleep(Duration::from_millis(200));

    std::fs::write(&path, VALID_CONFIG_UPDATED).expect("failed to rewrite config file");

    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for reload event");

    let cfg = result.expect("reload event should carry a valid config");
    assert_eq!(cfg.poll_interval_seconds, 20);
}

#[test]
fn watcher_debounces_rapid_writes() {
    let dir = TempDir::new().expect("failed to create temp dir");
    let path = write_config(&dir, VALID_CONFIG);

    let (tx, rx) = mpsc::channel();
    let _watcher = txwatch_config::watch_config(&path, move |result| {
        let _ = tx.send(result);
    })
    .expect("failed to start config watcher");

    // Give the watcher a moment to register with inotify.
    std::thread::sleep(Duration::from_millis(200));

    // Perform several rapid successive writes.
    for _ in 0..5 {
        std::fs::write(&path, VALID_CONFIG_UPDATED).expect("failed to rewrite config file");
        std::thread::sleep(Duration::from_millis(10));
    }

    // First event should arrive within the timeout.
    let first = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("timed out waiting for first reload event");
    first.expect("reload event should carry a valid config");

    // After debouncing, no further events should arrive within a short window.
    let extra = rx.recv_timeout(Duration::from_millis(500));
    assert!(
        extra.is_err(),
        "expected rapid writes to be debounced into a single reload event, got an extra event"
    );
}
