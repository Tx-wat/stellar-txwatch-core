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
fn global_webhook_headers_parse_from_config() {
    let toml = r#"
        poll_interval_seconds = 10

        [webhook_headers]
        Authorization = "Bearer global-token"
        X-Env = "prod"

        [[contracts]]
        id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
        threshold_xlm = 100
    "#;

    let cfg = AppConfig::from_str(toml).expect("global webhook_headers should parse");

    assert_eq!(
        cfg.webhook_headers.get("Authorization").map(String::as_str),
        Some("Bearer global-token")
    );
    assert_eq!(
        cfg.webhook_headers.get("X-Env").map(String::as_str),
        Some("prod")
    );
}

#[test]
fn per_contract_webhook_headers_take_precedence_over_global() {
    let toml = r#"
        poll_interval_seconds = 10

        [webhook_headers]
        Authorization = "Bearer global-token"
        X-Env = "prod"

        [[contracts]]
        id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAD2KM"
        threshold_xlm = 100

        [contracts.webhook_headers]
        Authorization = "Bearer contract-token"
    "#;

    let cfg = AppConfig::from_str(toml).expect("merged webhook_headers should parse");
    let contract = cfg.contracts.first().expect("contract should exist");

    let merged = cfg.effective_webhook_headers(contract);

    assert_eq!(
        merged.get("Authorization").map(String::as_str),
        Some("Bearer contract-token"),
        "per-contract header should override the global default"
    );
    assert_eq!(
        merged.get("X-Env").map(String::as_str),
        Some("prod"),
        "global header should be inherited when not overridden"
    );
}
