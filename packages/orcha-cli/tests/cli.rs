//! orcha-cli 二进制行为集成测试。
//!
//! 通过 `env!("CARGO_BIN_EXE_orcha")` 拿到 cargo 编译出的二进制路径，
//! 直接以子进程方式验证面向用户的 acceptance 命令。

use std::process::Command;

use serde_json::Value;

fn orcha_bin() -> String {
    env!("CARGO_BIN_EXE_orcha").to_string()
}

#[test]
fn version_flag_prints_orcha_and_version_and_exits_zero() {
    let out = Command::new(orcha_bin())
        .arg("--version")
        .output()
        .expect("failed to spawn orcha");
    assert!(out.status.success(), "exit should be 0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.starts_with("orcha "),
        "expected 'orcha <version>', got: {stdout:?}"
    );
    // 形如 `orcha 0.1.0`，第二段应是语义版本号。
    let version_part = stdout.trim().strip_prefix("orcha ").unwrap();
    let parts: Vec<&str> = version_part.split('.').collect();
    assert_eq!(parts.len(), 3, "version should be semver: {version_part}");
    assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit())));
}

#[test]
fn help_flag_exits_zero_and_mentions_schema() {
    let out = Command::new(orcha_bin())
        .arg("--help")
        .output()
        .expect("failed to spawn orcha --help");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("schema"),
        "--help should list schema subcommand"
    );
}

#[test]
fn no_subcommand_prints_help_and_exits_zero() {
    let out = Command::new(orcha_bin())
        .output()
        .expect("failed to spawn orcha");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.is_empty(),
        "should print something without subcommand"
    );
}

#[test]
fn schema_export_emits_valid_json_with_all_models() {
    let out = Command::new(orcha_bin())
        .args(["schema", "--export"])
        .output()
        .expect("failed to spawn orcha schema --export");
    assert!(
        out.status.success(),
        "schema --export should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&stdout).expect("stdout must be valid JSON");
    let obj = v.as_object().expect("schema root must be a JSON object");
    for key in [
        "Task",
        "TaskStatus",
        "OrchaEvent",
        "EventType",
        "EventPayload",
        "Attachment",
        "OrchaResponse",
        "ResponseStatus",
        "ResponseArtifact",
        "ResponseArtifactType",
        "Step",
        "StepStatus",
        "StepResult",
        "Artifact",
        "ArtifactType",
    ] {
        assert!(
            obj.contains_key(key),
            "schema --export missing model: {key}"
        );
    }
    assert_eq!(
        obj["TaskStatus"]["enum"],
        serde_json::json!(["PENDING", "RUNNING", "BLOCKED", "DONE", "FAILED"]),
    );
}

#[test]
fn schema_without_export_flag_errors_with_nonzero_exit() {
    let out = Command::new(orcha_bin())
        .arg("schema")
        .output()
        .expect("failed to spawn orcha schema");
    assert!(
        !out.status.success(),
        "schema without --export must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--export"),
        "error should hint at --export: {stderr}"
    );
}
