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
