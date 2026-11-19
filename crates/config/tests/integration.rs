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
fn validated_constructor_rejects_poll_interval_over_3600() {
    let path = project_root().join("config").join("example.toml");
    let mut cfg = AppConfig::from_file(&path)
        .expect("example.toml should parse and validate successfully");

    // Programmatic mutation bypasses validation; the validating constructor
    // must catch the out-of-range poll interval regardless of construction path.
    cfg.poll_interval_seconds = 3601;

    let err = AppConfig::validated(cfg)
        .expect_err("poll_interval_seconds > 3600 must be rejected by validated()");
    let msg = err.to_string();

    assert!(
        msg.contains("poll_interval_seconds"),
        "expected a poll_interval_seconds validation failure, got: {}",
        msg
    );
}

#[test]
fn validated_constructor_accepts_poll_interval_at_boundary() {
    let path = project_root().join("config").join("example.toml");
    let mut cfg = AppConfig::from_file(&path)
        .expect("example.toml should parse and validate successfully");

    cfg.poll_interval_seconds = 3600;

    AppConfig::validated(cfg)
        .expect("poll_interval_seconds == 3600 should be accepted by validated()");
}
