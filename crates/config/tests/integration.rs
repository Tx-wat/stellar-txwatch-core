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