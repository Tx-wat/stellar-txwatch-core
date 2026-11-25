use std::{env, path::PathBuf, process::Command};

use serde_json::Value;
use txwatch_config::AppConfig;

fn txwatch_bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_txwatch"));
    command.env_remove("TXWATCH_CONFIG");
    command.env("RUST_LOG", "trace");
    command
}

fn missing_config_path() -> PathBuf {
    // A regular file cannot contain a child, so this path is always missing.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("Cargo.toml")
        .join("missing.toml")
}

fn assert_schema_output(command: &mut Command) -> Vec<u8> {
    let output = command.output().expect("failed to run txwatch");
    assert!(
        output.status.success(),
        "schema command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let schema: Value = serde_json::from_slice(&output.stdout)
        .expect("stdout should contain only one JSON Schema document");
    let expected = serde_json::to_value(AppConfig::json_schema())
        .expect("config schema should serialize to JSON");
    assert_eq!(schema, expected);
    assert_eq!(schema["title"], "AppConfig");
    assert!(schema["$schema"].is_string());
    assert!(output.stdout.ends_with(b"\n"));
    assert!(output.stderr.is_empty(), "schema should not emit logs");
    output.stdout
}

#[test]
fn schema_prints_pretty_json_without_logs() {
    let stdout = assert_schema_output(txwatch_bin().arg("schema"));
    let expected = format!(
        "{}\n",
        serde_json::to_string_pretty(&AppConfig::json_schema())
            .expect("config schema should serialize to JSON")
    );
    assert_eq!(stdout, expected.as_bytes());
}

#[test]
fn schema_works_outside_the_repository_without_a_config_file() {
    assert_schema_output(txwatch_bin().current_dir(env::temp_dir()).arg("schema"));
}

#[test]
fn schema_ignores_a_missing_config_flag_path() {
    assert_schema_output(
        txwatch_bin()
            .arg("--config")
            .arg(missing_config_path())
            .arg("schema"),
    );
}

#[test]
fn schema_ignores_a_missing_config_environment_path() {
    assert_schema_output(
        txwatch_bin()
            .env("TXWATCH_CONFIG", missing_config_path())
            .arg("schema"),
    );
}

#[test]
fn schema_does_not_validate_config_contents() {
    // Cargo.toml exists but is not a valid AppConfig.
    assert_schema_output(
        txwatch_bin()
            .arg("--config")
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
            .arg("schema"),
    );
}

#[test]
fn schema_output_is_deterministic() {
    let first = assert_schema_output(txwatch_bin().arg("schema"));
    let second = assert_schema_output(txwatch_bin().arg("schema"));
    assert_eq!(first, second);
}

#[test]
fn schema_command_is_documented_in_help() {
    let output = txwatch_bin()
        .arg("--help")
        .output()
        .expect("failed to run txwatch");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("schema"));
    assert!(stdout.contains("Print JSON Schema for AppConfig"));

    let output = txwatch_bin()
        .args(["schema", "--help"])
        .output()
        .expect("failed to run txwatch");
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("no config file is required"));
}
