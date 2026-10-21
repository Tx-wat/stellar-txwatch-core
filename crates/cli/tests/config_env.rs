use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Env Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/env"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

fn write_config(name: &str) -> std::path::PathBuf {
    let path = env::temp_dir().join(name);
    fs::write(&path, VALID_CONFIG).unwrap();
    path
}

#[test]
fn txwatch_config_env_var_is_used_when_flag_is_absent() {
    let path = write_config("txwatch_env_fallback_test.toml");

    let output = txwatch_bin()
        .env("TXWATCH_CONFIG", &path)
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Config is valid."));
}

#[test]
fn config_flag_takes_precedence_over_txwatch_config_env_var() {
    let valid = write_config("txwatch_flag_precedence_test.toml");
    let missing = env::temp_dir().join("txwatch_flag_precedence_missing.toml");
    let _ = fs::remove_file(&missing);

    let output = txwatch_bin()
        .env("TXWATCH_CONFIG", &missing)
        .args(["--config", valid.to_str().unwrap(), "validate"])
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "--config should win over TXWATCH_CONFIG; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn missing_config_without_flag_or_env_falls_back_to_default_path() {
    let output = txwatch_bin()
        .env_remove("TXWATCH_CONFIG")
        .current_dir(env::temp_dir())
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("config/example.toml"),
        "expected default path in error, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn help_text_mentions_txwatch_config_env_var() {
    let output = txwatch_bin().arg("--help").output().expect("failed to run txwatch");

    assert!(String::from_utf8_lossy(&output.stdout).contains("TXWATCH_CONFIG"));
}
