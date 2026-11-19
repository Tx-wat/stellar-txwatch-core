use std::path::PathBuf;
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
fn contract_with_max_alerts_per_minute_parses() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
label = "busy-contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100
max_alerts_per_minute = 30

[[contracts.rules]]
type = "any_transaction"
"#;
    let cfg = AppConfig::from_str(toml)
        .expect("config with max_alerts_per_minute should parse and validate");

    assert_eq!(cfg.contracts.len(), 1);
    assert_eq!(cfg.contracts[0].max_alerts_per_minute, Some(30));
}

#[test]
fn contract_with_zero_max_alerts_per_minute_is_rejected() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
label = "busy-contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100
max_alerts_per_minute = 0

[[contracts.rules]]
type = "any_transaction"
"#;
    let err = AppConfig::from_str(toml)
        .expect_err("max_alerts_per_minute = 0 should be rejected");
    let msg = err.to_string();

    assert!(
        msg.contains("max_alerts_per_minute"),
        "expected a max_alerts_per_minute validation failure, got: {}",
        msg
    );
}

#[test]
fn contract_with_cursor_alias_parses() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
label = "renamed-contract"
cursor_alias = "old-contract-label"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100

[[contracts.rules]]
type = "any_transaction"
"#;
    let cfg = AppConfig::from_str(toml)
        .expect("config with cursor_alias should parse and validate");

    assert_eq!(cfg.contracts.len(), 1);
    assert_eq!(
        cfg.contracts[0].cursor_alias.as_deref(),
        Some("old-contract-label")
    );
}

#[test]
fn contract_with_empty_cursor_alias_is_rejected() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
label = "renamed-contract"
cursor_alias = ""
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100

[[contracts.rules]]
type = "any_transaction"
"#;
    let err = AppConfig::from_str(toml)
        .expect_err("empty cursor_alias should be rejected");
    let msg = err.to_string();

    assert!(
        msg.contains("cursor_alias"),
        "expected a cursor_alias validation failure, got: {}",
        msg
    );
}

#[test]
fn contract_with_cursor_alias_equal_to_label_is_rejected() {
    let toml = r#"
poll_interval_seconds = 10

[[contracts]]
label = "same-label"
cursor_alias = "same-label"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
threshold_xlm = 100

[[contracts.rules]]
type = "any_transaction"
"#;
    let err = AppConfig::from_str(toml)
        .expect_err("cursor_alias equal to label should be rejected");
    let msg = err.to_string();

    assert!(
        msg.contains("cursor_alias"),
        "expected a cursor_alias validation failure, got: {}",
        msg
    );
}