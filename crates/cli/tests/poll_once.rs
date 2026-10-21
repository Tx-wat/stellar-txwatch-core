use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

#[test]
fn watch_help_documents_poll_once_flag() {
    let output = txwatch_bin()
        .args(["watch", "--help"])
        .output()
        .expect("failed to run txwatch");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--poll-once"), "help should list --poll-once:\n{}", stdout);
}

#[test]
fn poll_once_still_validates_config_and_exits_one_when_invalid() {
    let path = env::temp_dir().join("txwatch_poll_once_invalid_config.toml");
    fs::write(&path, "poll_interval_seconds = 1\n").unwrap();

    let output = txwatch_bin()
        .arg("--config")
        .arg(&path)
        .args(["watch", "--poll-once"])
        .output()
        .expect("failed to run txwatch");
    let _ = fs::remove_file(&path);

    assert_eq!(output.status.code(), Some(1));
}
