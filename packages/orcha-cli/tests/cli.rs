//! orcha-cli 二进制行为集成测试。
//!
//! 通过 `env!("CARGO_BIN_EXE_orcha")` 拿到 cargo 编译出的二进制路径，
//! 直接以子进程方式验证面向用户的 acceptance 命令。

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn orcha_bin() -> String {
    env!("CARGO_BIN_EXE_orcha").to_string()
}

/// 在指定 home 下运行 `orcha <args>`，返回子进程 Output。
/// 通过 ORCHA_HOME 环境变量隔离每个测试的存储。
fn run_orcha(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(orcha_bin())
        .env("ORCHA_HOME", home)
        .args(args)
        .output()
        .expect("failed to spawn orcha")
}

/// 提取子进程 stdout 为已 trim 的 String。
fn stdout(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

// ============================================================
// M1 acceptance: init / run / status / list + persistence
// ============================================================

const VALID_STATUSES: &[&str] = &["PENDING", "RUNNING", "BLOCKED", "DONE", "FAILED"];

#[test]
fn run_returns_t_prefix_task_id_and_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_orcha(dir.path(), &["run", "hello world"]);
    assert!(
        out.status.success(),
        "run should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = stdout(&out);
    assert!(
        id.starts_with("T-"),
        "task_id should start with 'T-', got: {id}"
    );
    // T- 后应跟随一个合法的 uuid 段。
    assert!(id.len() > "T-".len(), "task_id should have a uuid suffix");
}

#[test]
fn status_outputs_json_with_valid_status_enum() {
    let dir = tempfile::tempdir().unwrap();
    let run_out = run_orcha(dir.path(), &["run", "demo task"]);
    assert!(run_out.status.success());
    let id = stdout(&run_out);

    let status_out = run_orcha(dir.path(), &["status", &id]);
    assert!(
        status_out.status.success(),
        "status should exit 0, stderr: {}",
        String::from_utf8_lossy(&status_out.stderr)
    );

    let v: Value = serde_json::from_str(&stdout(&status_out)).expect("status output must be JSON");
    assert_eq!(v["id"], id);
    assert_eq!(v["description"], "demo task");
    let status_str = v["status"].as_str().expect("status must be a string");
    assert!(
        VALID_STATUSES.contains(&status_str),
        "status must be a legal enum value, got: {status_str}"
    );
}

#[test]
fn list_shows_all_tasks_and_supports_status_filter() {
    let dir = tempfile::tempdir().unwrap();
    // 创建 2 个任务。
    let id1 = stdout(&run_orcha(dir.path(), &["run", "first"]));
    let _id2 = stdout(&run_orcha(dir.path(), &["run", "second"]));

    // list 无过滤 -> 应有 2 个。
    let all = run_orcha(dir.path(), &["list"]);
    assert!(all.status.success(), "list should exit 0");
    let arr: Vec<Value> =
        serde_json::from_str(&stdout(&all)).expect("list output must be a JSON array");
    assert_eq!(arr.len(), 2, "list should return both tasks");
    assert!(
        arr.iter().any(|t| t["id"] == id1),
        "first task should be present"
    );

    // list --status pending（小写）-> 新任务都是 PENDING，应仍有 2 个。
    let pending = run_orcha(dir.path(), &["list", "--status", "pending"]);
    assert!(pending.status.success(), "list --status should exit 0");
    let arr: Vec<Value> =
        serde_json::from_str(&stdout(&pending)).expect("filtered list must be JSON array");
    assert_eq!(arr.len(), 2, "both tasks are PENDING");
    for t in &arr {
        assert_eq!(t["status"], "PENDING");
    }

    // list --status DONE -> 0 个（没人迁移过）。
    let done = run_orcha(dir.path(), &["list", "--status", "DONE"]);
    assert!(done.status.success());
    let arr: Vec<Value> =
        serde_json::from_str(&stdout(&done)).expect("DONE filter must be JSON array");
    assert!(arr.is_empty(), "no DONE tasks yet");
}

#[test]
fn persistence_survives_process_restart() {
    // 核心验收项：进程 A 创建任务，进程 B（独立调用）仍能查到。
    // 这模拟"重启进程后 orcha list 仍可查到历史任务"。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().to_path_buf();

    // 进程 A：run。
    let id = stdout(&run_orcha(&home, &["run", "survive restart"]));
    assert!(id.starts_with("T-"));

    // 进程 B：list 应能看到该任务。
    let list_out = run_orcha(&home, &["list"]);
    assert!(list_out.status.success());
    let arr: Vec<Value> =
        serde_json::from_str(&stdout(&list_out)).expect("list must be JSON array");
    assert_eq!(arr.len(), 1, "task should survive across processes");
    assert_eq!(arr[0]["id"], id);
    assert_eq!(arr[0]["description"], "survive restart");

    // 进程 C：status <id> 也能查到。
    let status_out = run_orcha(&home, &["status", &id]);
    assert!(status_out.status.success());
    let v: Value = serde_json::from_str(&stdout(&status_out)).expect("status must be JSON");
    assert_eq!(v["id"], id);
}

#[test]
fn status_nonexistent_id_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_orcha(dir.path(), &["status", "T-does-not-exist"]);
    assert!(
        !out.status.success(),
        "status on missing id must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not found"),
        "stderr should mention 'not found': {stderr}"
    );
}

#[test]
fn list_invalid_status_filter_errors() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_orcha(dir.path(), &["list", "--status", "BOGUS"]);
    assert!(
        !out.status.success(),
        "invalid status filter must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid status"),
        "stderr should mention invalid status: {stderr}"
    );
}

#[test]
fn init_creates_store_dir_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("nested").join("orcha-home");
    // home 尚不存在，init 应创建嵌套目录。
    let out = run_orcha(&home, &["init"]);
    assert!(out.status.success(), "init should succeed");
    let printed = stdout(&out);
    assert!(
        printed.ends_with("store"),
        "init should print store path: {printed}"
    );
    assert!(
        home.join("store").exists(),
        "store dir should exist on disk"
    );

    // 第二次 init 幂等，不报错。
    let out2 = run_orcha(&home, &["init"]);
    assert!(out2.status.success(), "init must be idempotent");
}

#[test]
fn run_implicitly_initializes_home() {
    // 不先 init，直接 run 也应工作（CLI 每个写命令自动 init）。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("fresh");
    assert!(!home.exists());
    let out = run_orcha(&home, &["run", "no explicit init"]);
    assert!(out.status.success(), "run should implicitly init home");
    assert!(stdout(&out).starts_with("T-"));
    assert!(home.join("store").exists(), "store dir should be created");
}
