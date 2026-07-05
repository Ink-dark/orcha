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

// ============================================================
// M4 acceptance: crash recovery + concurrent writes + 3-class queries
// ============================================================

/// 把 store 里某 Task 的 JSON 文件 status 字段强制改成 `RUNNING`，
/// 模拟"进程在 RUNNING 状态崩溃"。
fn force_task_running(home: &Path, task_id: &str) {
    let task_file = home.join("store").join(format!("{task_id}.json"));
    let content = std::fs::read_to_string(&task_file).unwrap();
    let mut v: Value = serde_json::from_str(&content).unwrap();
    v["status"] = serde_json::json!("RUNNING");
    std::fs::write(&task_file, serde_json::to_string_pretty(&v).unwrap()).unwrap();
}

#[test]
fn crash_recovery_recovers_running_task_to_blocked_via_subprocess() {
    // M4 验收项 1：kill 进程后重启，RUNNING 中断的 Task 自动恢复并继续。
    // 端到端：进程 A 创建 Task + 直接改文件把 status 设 RUNNING（模拟崩溃），
    // 进程 B 跑 `orcha recover --strategy block`，应把 Task 迁到 BLOCKED。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let id = stdout(&run_orcha(home, &["run", "crashed task"]));
    assert!(id.starts_with("T-"));

    force_task_running(home, &id);

    let recover_out = run_orcha(home, &["recover", "--strategy", "block"]);
    assert!(
        recover_out.status.success(),
        "recover should exit 0, stderr: {}",
        String::from_utf8_lossy(&recover_out.stderr)
    );
    let report: Value =
        serde_json::from_str(&stdout(&recover_out)).expect("recover output must be JSON");
    assert_eq!(report["recovered_count"], 1);
    assert_eq!(report["strategy"], "block");
    assert_eq!(report["recovered_task_ids"][0], id);

    // 验证 Task 状态已是 BLOCKED。
    let status_out = run_orcha(home, &["status", &id]);
    assert!(status_out.status.success());
    let v: Value = serde_json::from_str(&stdout(&status_out)).unwrap();
    assert_eq!(v["status"], "BLOCKED");
}

#[test]
fn crash_recovery_fail_strategy_moves_running_to_failed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let id = stdout(&run_orcha(home, &["run", "crashed fail"]));
    force_task_running(home, &id);

    let out = run_orcha(home, &["recover", "--strategy", "fail"]);
    assert!(out.status.success());
    let report: Value = serde_json::from_str(&stdout(&out)).unwrap();
    assert_eq!(report["recovered_count"], 1);
    assert_eq!(report["strategy"], "fail");

    let v: Value = serde_json::from_str(&stdout(&run_orcha(home, &["status", &id]))).unwrap();
    assert_eq!(v["status"], "FAILED");
}

#[test]
fn recover_idempotent_second_run_finds_no_running() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let id = stdout(&run_orcha(home, &["run", "idempotent"]));
    force_task_running(home, &id);

    let _ = run_orcha(home, &["recover", "--strategy", "block"]);
    let out2 = run_orcha(home, &["recover", "--strategy", "block"]);
    assert!(out2.status.success());
    let report: Value = serde_json::from_str(&stdout(&out2)).unwrap();
    assert_eq!(report["recovered_count"], 0, "二次 recover 应无 RUNNING");
    assert_eq!(report["skipped_count"], 1, "Task 现在是 BLOCKED，被跳过");
}

#[test]
fn recover_rejects_invalid_strategy_via_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_orcha(dir.path(), &["recover", "--strategy", "bogus"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("invalid strategy"),
        "stderr should mention invalid strategy: {stderr}"
    );
}

#[test]
fn concurrent_recover_processes_dont_double_recover() {
    // M4 验收项 3：并发写入无冲突（带乐观锁/版本号）。
    // 5 个并发 `orcha recover` 同时跑同一 home（含 1 个 RUNNING Task）。
    // 由于文件锁 + 状态机校验，只有 1 个进程能 recovered_count=1，
    // 其余 4 个应 recovered_count=0（Task 已变成 BLOCKED，跳过）。
    // 最终 Task 状态应是 BLOCKED，文件不损坏。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let id = stdout(&run_orcha(home, &["run", "concurrent recover"]));
    force_task_running(home, &id);

    // 5 个并发进程。
    let home_clone = home.to_path_buf();
    let mut handles = Vec::new();
    for _ in 0..5 {
        let h = home_clone.clone();
        handles.push(std::thread::spawn(move || {
            run_orcha(&h, &["recover", "--strategy", "block"])
        }));
    }
    let outputs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    // 每个 recover 都应 exit 0（即使没 recover 到也不算错误）。
    for (i, out) in outputs.iter().enumerate() {
        assert!(
            out.status.success(),
            "recover #{i} should exit 0, stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // 统计 recovered_count=1 的进程数（应恰好 1 个）。
    let recovered_ones = outputs
        .iter()
        .filter(|out| {
            let v: Value = match serde_json::from_str(&stdout(out)) {
                Ok(v) => v,
                Err(_) => return false,
            };
            v["recovered_count"] == 1
        })
        .count();
    assert_eq!(
        recovered_ones, 1,
        "只有 1 个进程应成功 recover，实际 {recovered_ones}"
    );

    // 最终 Task 状态应是 BLOCKED。
    let v: Value = serde_json::from_str(&stdout(&run_orcha(home, &["status", &id]))).unwrap();
    assert_eq!(v["status"], "BLOCKED");
}

#[test]
fn three_class_queries_state_history_artifacts() {
    // M4 验收项 2：`task:{id}:state` / `:history` / `:artifacts` 三类键可查。
    // 端到端：`orcha run` 创建 Task → 直接写一份 history JSONL（绕开 orcha fix，
    // 避免 Python 依赖）→ 三个查询命令都应返回预期数据。
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let id = stdout(&run_orcha(home, &["run", "three classes"]));
    assert!(id.starts_with("T-"));

    // 直接写一份 history JSONL，模拟 fix 跑了一轮产出 1 个 artifact。
    let history_dir = home.join("history");
    std::fs::create_dir_all(&history_dir).unwrap();
    let history_file = history_dir.join(format!("{id}.jsonl"));
    let round_record = serde_json::json!({
        "round": 1,
        "started_at": "2026-07-05T12:00:00Z",
        "finished_at": "2026-07-05T12:00:01Z",
        "steps": [],
        "artifacts": [{
            "artifact_id": "ART-test-1",
            "type": "REPORT",
            "commit_sha": null,
            "patch": null,
            "url": null
        }],
        "tokens_used": 100
    });
    std::fs::write(&history_file, format!("{round_record}\n")).unwrap();

    // 1. `orcha status` → state 键。
    let status_out = run_orcha(home, &["status", &id]);
    assert!(status_out.status.success());
    let v: Value = serde_json::from_str(&stdout(&status_out)).unwrap();
    assert_eq!(v["id"], id);
    assert_eq!(v["status"], "PENDING");

    // 2. `orcha history` → history 键。
    let history_out = run_orcha(home, &["history", &id]);
    assert!(history_out.status.success());
    let arr: Vec<Value> = serde_json::from_str(&stdout(&history_out)).unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["round"], 1);
    assert_eq!(arr[0]["tokens_used"], 100);

    // 3. `orcha artifacts` → artifacts 键。
    let artifacts_out = run_orcha(home, &["artifacts", &id]);
    assert!(artifacts_out.status.success());
    let arr: Vec<Value> = serde_json::from_str(&stdout(&artifacts_out)).unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["artifact_id"], "ART-test-1");
    assert_eq!(arr[0]["type"], "REPORT");
}

#[test]
fn history_and_artifacts_missing_task_returns_empty_array() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();

    let history_out = run_orcha(home, &["history", "T-missing"]);
    assert!(history_out.status.success());
    let arr: Vec<Value> = serde_json::from_str(&stdout(&history_out)).unwrap();
    assert!(arr.is_empty(), "missing task history should be empty array");

    let artifacts_out = run_orcha(home, &["artifacts", "T-missing"]);
    assert!(artifacts_out.status.success());
    let arr: Vec<Value> = serde_json::from_str(&stdout(&artifacts_out)).unwrap();
    assert!(
        arr.is_empty(),
        "missing task artifacts should be empty array"
    );
}

// ============================================================
// M5 acceptance: shell serve / list end-to-end
// ============================================================

/// RAII 守卫：drop 时 kill 子进程，避免 `orcha shell serve` 阻塞进程泄漏。
struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// 在子进程启动 `orcha shell serve --addr <addr> --api-key K --workspace ws`。
/// 返回的 ChildGuard 在 drop 时会自动 kill 子进程。
fn spawn_shell_serve(home: &Path, workspace: &Path, addr: &str, api_key: &str) -> ChildGuard {
    let child = Command::new(orcha_bin())
        .env("ORCHA_HOME", home)
        .args([
            "shell",
            "serve",
            "--addr",
            addr,
            "--api-key",
            api_key,
            "--workspace",
            workspace.to_str().expect("workspace path is utf-8"),
        ])
        // piped 避免子进程输出污染测试日志；serve 只打一行 stderr，不会堵。
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn orcha shell serve");
    ChildGuard(child)
}

/// 等待 server 起来：尝试 TCP 连接 addr，最多重试 50 次（约 1 秒）。
fn wait_for_server(addr: &str) {
    for _ in 0..50 {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("orcha shell serve did not come up at {addr}");
}

/// 发一个 HTTP/1.1 请求并返回 (status_code, body)。
/// 复用 http.rs 测试里的极简实现，避免引入 reqwest 重型依赖。
fn http_request(
    addr: &str,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(addr).expect("connect to shell serve");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: {a}\r\n"));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        req.push_str("Content-Type: application/json\r\n");
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let mut parts = response.splitn(2, "\r\n\r\n");
    let headers = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("");
    let status_line = headers.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, body.to_string())
}

/// 拿一个随机可用端口（bind 后 drop，把端口让给子进程）。
fn random_port_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("127.0.0.1:{port}")
}

#[test]
fn shell_list_via_subprocess_prints_cli_adapter() {
    // M5 验收项：「`orcha shell list` 列出已注册 Adapter」。
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let out = run_orcha(
        dir.path(),
        &[
            "shell",
            "list",
            "--api-key",
            "test-key",
            "--workspace",
            ws.path().to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "shell list should exit 0, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let arr: Vec<Value> =
        serde_json::from_str(&stdout(&out)).expect("shell list output must be JSON array");
    assert_eq!(arr.len(), 1, "应恰好 1 个 adapter");
    assert_eq!(arr[0]["name"], "cli");
    assert_eq!(arr[0]["source"], "cli");
    assert!(
        arr[0]["description"].as_str().unwrap().contains("CLI"),
        "description 应提及 CLI: {}",
        arr[0]["description"]
    );
}

#[test]
fn shell_list_rejects_missing_workspace_via_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_orcha(
        dir.path(),
        &[
            "shell",
            "list",
            "--api-key",
            "k",
            "--workspace",
            "/nonexistent/workspace/path",
        ],
    );
    assert!(
        !out.status.success(),
        "missing workspace must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("workspace 不存在"),
        "stderr should mention missing workspace: {stderr}"
    );
}

#[test]
fn shell_serve_http_without_api_key_returns_401() {
    // M5 验收项：「无 API Key 请求返回 401」——端到端通过子进程验证。
    // 不需要 Python：401 在 Cycleround 跑之前就拦截。
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let addr = random_port_addr();

    let _guard = spawn_shell_serve(dir.path(), ws.path(), &addr, "secret-key");
    wait_for_server(&addr);

    let event = serde_json::json!({
        "event_id": "",
        "source": "cli",
        "user_id": "u",
        "timestamp": 0,
        "type": "USER_PROMPT",
        "payload": {"raw_text": "test", "attachments": []}
    });
    let body = serde_json::to_string(&event).unwrap();

    // 无 auth → 401。
    let (status, resp_body) = http_request(&addr, "POST", "/run", None, Some(&body));
    assert_eq!(status, 401, "no api key should return 401: {resp_body}");
}

#[test]
fn shell_serve_http_wrong_api_key_returns_401() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let addr = random_port_addr();

    let _guard = spawn_shell_serve(dir.path(), ws.path(), &addr, "secret-key");
    wait_for_server(&addr);

    let event = serde_json::json!({
        "event_id": "",
        "source": "cli",
        "user_id": "u",
        "timestamp": 0,
        "type": "USER_PROMPT",
        "payload": {"raw_text": "test", "attachments": []}
    });
    let body = serde_json::to_string(&event).unwrap();

    let (status, _) = http_request(&addr, "POST", "/run", Some("Bearer wrong-key"), Some(&body));
    assert_eq!(status, 401, "wrong api key should return 401");
}

#[test]
fn shell_serve_http_unknown_route_returns_400() {
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let addr = random_port_addr();

    let _guard = spawn_shell_serve(dir.path(), ws.path(), &addr, "secret-key");
    wait_for_server(&addr);

    // GET /unknown with valid key → 400（未知路由走 InvalidPath，状态码 400）。
    let (status, _) = http_request(&addr, "GET", "/unknown", Some("Bearer secret-key"), None);
    assert_eq!(status, 400, "unknown route should return 400 (InvalidPath)");
}

#[test]
fn shell_serve_http_end_to_end_post_run_and_get_status() {
    // M5 端到端验收：POST /run 触发 CliAdapter 跑 Cycleround，
    // GET /status/{event_id} 返回 FINAL 响应。
    // 需要 Python 才能让 Cycleround 单轮成功；无 Python 时跳过。
    if orcha_core::find_python().is_none() {
        eprintln!("skipping: no python interpreter on PATH");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    // 预置 test.py，让 Cycleround 单轮成功创建 hello.py。
    std::fs::write(
        ws.path().join("test.py"),
        "assert open('hello.py').read().strip() == 'hello'\n",
    )
    .unwrap();

    let addr = random_port_addr();
    let _guard = spawn_shell_serve(dir.path(), ws.path(), &addr, "secret-key");
    wait_for_server(&addr);

    // POST /run：构造合法 OrchaEvent。
    let event = serde_json::json!({
        "event_id": "",
        "source": "cli",
        "user_id": "test-user",
        "timestamp": 0,
        "type": "USER_PROMPT",
        "payload": {"raw_text": "创建 hello.py 输出 hello", "attachments": []}
    });
    let body = serde_json::to_string(&event).unwrap();

    let (status, resp_body) = http_request(
        &addr,
        "POST",
        "/run",
        Some("Bearer secret-key"),
        Some(&body),
    );
    assert_eq!(status, 200, "POST /run should return 200: {resp_body}");
    let v: Value = serde_json::from_str(&resp_body).expect("POST /run body must be JSON");
    let event_id = v["event_id"]
        .as_str()
        .expect("event_id must be a string")
        .to_string();
    assert!(
        event_id.starts_with("EVT-"),
        "event_id should be EVT-<uuid>, got: {event_id}"
    );

    // GET /status/{event_id}：应返回 FINAL 响应。
    let (status, resp_body) = http_request(
        &addr,
        "GET",
        &format!("/status/{event_id}"),
        Some("Bearer secret-key"),
        None,
    );
    assert_eq!(status, 200, "GET /status should return 200: {resp_body}");
    let v: Value = serde_json::from_str(&resp_body).expect("GET /status body must be JSON");
    assert_eq!(v["event_id"], event_id);
    assert_eq!(v["status"], "FINAL", "response status should be FINAL");
    assert!(
        v["content"].as_str().unwrap().contains("succeeded"),
        "content should mention success: {}",
        v["content"]
    );
    // 成功路径应有 1 个 FileRef artifact（history_file）。
    let artifacts = v["artifacts"].as_array().expect("artifacts must be array");
    assert_eq!(artifacts.len(), 1, "should have 1 FileRef artifact");
    assert_eq!(artifacts[0]["type"], "FILE_REF");
    assert!(
        artifacts[0]["url"].as_str().unwrap().ends_with(".jsonl"),
        "artifact url should point to history jsonl: {}",
        artifacts[0]["url"]
    );

    // workspace 应真实产出 hello.py。
    assert!(
        ws.path().join("hello.py").is_file(),
        "hello.py should be produced in workspace"
    );

    // home/store 应有 1 个 Task，状态 DONE。
    let list_out = run_orcha(dir.path(), &["list"]);
    assert!(list_out.status.success());
    let tasks: Vec<Value> = serde_json::from_str(&stdout(&list_out)).unwrap();
    assert_eq!(tasks.len(), 1, "should have 1 task in store");
    assert_eq!(tasks[0]["status"], "DONE");
}

#[test]
fn shell_serve_http_get_status_missing_returns_404() {
    // GET /status/<不存在的 event_id> → 404。
    let dir = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let addr = random_port_addr();

    let _guard = spawn_shell_serve(dir.path(), ws.path(), &addr, "secret-key");
    wait_for_server(&addr);

    let (status, _) = http_request(
        &addr,
        "GET",
        "/status/EVT-does-not-exist",
        Some("Bearer secret-key"),
        None,
    );
    assert_eq!(status, 404, "missing event_id should return 404");
}
