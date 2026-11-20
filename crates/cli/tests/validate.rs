use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    // `cargo test` sets CARGO_BIN_EXE_txwatch when the binary is declared in the same workspace.
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

const INVALID_CONTRACT_ID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "NOT_A_VALID_CONTRACT_ID"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

const MULTI_CONTRACT_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Alpha Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/alpha"

  [[contracts.rules]]
  type = "AnyTransaction"

[[contracts]]
label       = "Beta Contract"
contract_id = "CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
network     = "mainnet"
webhook_url = "https://hooks.example.com/beta"

  [[contracts.rules]]
  type = "TransactionFailed"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

#[test]
fn validate_exits_zero_for_valid_config() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_valid_test.toml");
    fs::write(&path, VALID_CONFIG).unwrap();

    let status = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(0),
        "expected exit code 0 for valid config"
    );
}

#[test]
fn validate_prints_all_contract_labels_ids_and_rule_counts() {
    let dir  = env::temp_dir();
    let path = dir.join("txwatch_validate_labels_test.toml");
    fs::write(&path, MULTI_CONTRACT_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success(), "expected exit code 0 for valid config");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Both contract labels must appear.
    assert!(stdout.contains("Alpha Contract"), "expected 'Alpha Contract' label in output");
    assert!(stdout.contains("Beta Contract"),  "expected 'Beta Contract' label in output");

    // Both contract IDs must appear.
    assert!(
        stdout.contains("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        "expected Alpha contract_id in output"
    );
    assert!(
        stdout.contains("CBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"),
        "expected Beta contract_id in output"
    );

    // Rule counts: Alpha has 1 rule, Beta has 2 rules.
    assert!(stdout.contains("rules        : 1"), "expected rule count 1 for Alpha");
    assert!(stdout.contains("rules        : 2"), "expected rule count 2 for Beta");
}

#[test]
fn validate_output_includes_rule_label_snapshot() {
    const SNAPSHOT_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
  [[contracts.rules]]
  type = "TransactionFailed"
"#;

    let dir = env::temp_dir();
    let path = dir.join("txwatch_validate_snapshot_test.toml");
    fs::write(&path, SNAPSHOT_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success(), "expected exit code 0 for valid config");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = concat!(
        "Config is valid.\n",
        "  poll_interval_seconds : 10\n",
        "  contracts             : 1\n",
        "\n",
        "  [Stellar Testnet] Test Contract\n",
        "    contract_id  : CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
        "    webhook_url  : https://hooks.example.com/test\n",
        "    secret       : none\n",
        "    rules        : 2\n",
        "      - AnyTransaction\n",
        "      - TransactionFailed\n",
        "    horizon      : https://horizon-testnet.stellar.org\n",
        "    explorer     : https://stellar.expert/explorer/testnet/contract/CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n"
    );

    assert_eq!(stdout, expected);
}

#[test]
fn validate_exits_one_for_invalid_config() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_invalid_test.toml");
    fs::write(&path, "this is not valid toml = = =").unwrap();

    let status = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 for invalid config"
    );
}

#[test]
fn validate_exits_one_for_invalid_contract_id() {
    let dir = env::temp_dir();
    let path = dir.join("txwatch_invalid_contract_id_test.toml");
    fs::write(&path, INVALID_CONTRACT_ID_CONFIG).unwrap();

    let status = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 for config with invalid contract ID"
    );
}

#[test]
fn version_flag_prints_cargo_pkg_version() {
    let output = txwatch_bin()
        .arg("--version")
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success(), "expected exit code 0 for --version");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "expected --version output to contain version {}, got: {}",
        env!("CARGO_PKG_VERSION"),
        stdout
    );
}

// ── Issue #530: validate --output json ────────────────────────────────────────

#[test]
fn validate_output_json_prints_contracts_array() {
    let path = env::temp_dir().join("txwatch_validate_json_test.toml");
    fs::write(&path, MULTI_CONTRACT_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate", "--output", "json"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success(), "expected exit code 0 for valid config");

    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout should be valid JSON");
    let contracts = json["contracts"].as_array().expect("contracts array");
    assert_eq!(contracts.len(), 2);

    assert_eq!(contracts[0]["label"], "Alpha Contract");
    assert_eq!(contracts[0]["webhook_url"], "https://hooks.example.com/alpha");
    assert_eq!(contracts[0]["rules"], serde_json::json!(["AnyTransaction"]));

    assert_eq!(contracts[1]["label"], "Beta Contract");
    assert_eq!(contracts[1]["webhook_url"], "https://hooks.example.com/beta");
    assert_eq!(
        contracts[1]["rules"],
        serde_json::json!(["TransactionFailed", "AnyTransaction"])
    );
}

#[test]
fn validate_default_output_is_text() {
    let path = env::temp_dir().join("txwatch_validate_default_text_test.toml");
    fs::write(&path, VALID_CONFIG).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("Config is valid."), "got: {}", stdout);
    assert!(serde_json::from_str::<serde_json::Value>(&stdout).is_err());
}

#[test]
fn validate_output_json_never_includes_webhook_secret() {
    let config = VALID_CONFIG.replace(
        "webhook_url = \"https://hooks.example.com/test\"",
        "webhook_url = \"https://hooks.example.com/test\"\nwebhook_secret = \"do-not-print-me\"",
    );
    let path = env::temp_dir().join("txwatch_validate_json_secret_test.toml");
    fs::write(&path, config).unwrap();

    let output = txwatch_bin()
        .args(["--config", path.to_str().unwrap(), "validate", "--output", "json"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("do-not-print-me") && !stderr.contains("do-not-print-me"));
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["contracts"][0]["secret"], "set");
}

// ── Issue #525: validate --check-contracts ────────────────────────────────────

fn config_with_horizon(horizon_url: &str) -> String {
    VALID_CONFIG.replace(
        "network     = \"testnet\"",
        &format!("network     = \"testnet\"\nhorizon_url = \"{}\"", horizon_url),
    )
}

const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

async fn run_check_contracts(status: u16, file_name: &str) -> std::process::Output {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let horizon = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("/accounts/{}", CONTRACT_ID)))
        .respond_with(ResponseTemplate::new(status).set_body_json(serde_json::json!({})))
        .expect(1)
        .mount(&horizon)
        .await;

    let config_path = env::temp_dir().join(file_name);
    fs::write(&config_path, config_with_horizon(&horizon.uri())).unwrap();

    let config_arg = config_path.to_str().unwrap().to_string();
    let output = tokio::task::spawn_blocking(move || {
        txwatch_bin()
            .args([
                "--config", &config_arg, "validate", "--check-contracts", "--output", "json",
            ])
            .output()
            .expect("failed to run txwatch")
    })
    .await
    .unwrap();
    let _ = fs::remove_file(&config_path);
    output
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_contracts_warns_but_succeeds_when_contract_not_found() {
    let output = run_check_contracts(404, "txwatch_check_contracts_404.toml").await;

    assert_eq!(output.status.code(), Some(0), "a missing contract must only warn");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not found"), "expected a not-found warning, got: {}", stderr);

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["contracts"][0]["contract_found"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn check_contracts_reports_found_contract() {
    let output = run_check_contracts(200, "txwatch_check_contracts_200.toml").await;

    assert_eq!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("not found"), "unexpected warning: {}", stderr);

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["contracts"][0]["contract_found"], true);
}
