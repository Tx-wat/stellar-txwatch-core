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