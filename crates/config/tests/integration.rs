use std::path::PathBuf;
use txwatch_config::{AppConfig, TlsConfig, WatchedContract};

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
fn validate_rejects_unparseable_tls_pem_files() {
    // Locate a non-PEM fixture shipped with the notifier crate.
    let not_a_cert = project_root()
        .join("crates")
        .join("notifier")
        .join("tests")
        .join("fixtures")
        .join("not-a-certificate.txt");
    assert!(
        not_a_cert.is_file(),
        "expected non-PEM fixture at {}",
        not_a_cert.display()
    );

    let contract = WatchedContract {
        id: "CTEST".to_string(),
        tls: Some(TlsConfig {
            client_cert: Some(not_a_cert.to_string_lossy().into_owned()),
            client_key: Some(not_a_cert.to_string_lossy().into_owned()),
            ca_cert: None,
        }),
        ..WatchedContract::default()
    };

    let err = contract
        .validate()
        .expect_err("validate() should reject unparseable TLS PEM files");
    let msg = err.to_string();

    assert!(
        msg.to_lowercase().contains("tls"),
        "expected validation error to mention the TLS field, got: {}",
        msg
    );
}