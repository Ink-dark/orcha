// Orcha CLI entry point.
// See docs/ROADMAP.md M0/M1/M3 for the contract this crate fulfills.

use std::env;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use orcha_core::{
    transition, CycleConfig, CycleOutcome, Cycleround, FailureReason, FileHistoryStore,
    FileTaskStore, TaskStore,
};
use orcha_sdk::{Artifact, Task, TaskStatus};

/// `orcha` 命令行根定义。
#[derive(Parser, Debug)]
#[command(
    name = "orcha",
    bin_name = "orcha",
    version,
    about = "Orcha - automated coding operating system for sub-agents",
    long_about = "Orcha Control Plane + Data Plane CLI. See docs/ROADMAP.md for milestones."
)]
struct Cli {
    /// Orcha home 目录（默认 ./orcha 或 $ORCHA_HOME）。
    #[arg(long, global = true, env = "ORCHA_HOME")]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// 初始化 orcha home 目录（创建 store 子目录），幂等。
    Init,

    /// 创建一个新任务并打印其 task_id。
    Run {
        /// 任务的自然语言描述。
        description: String,
    },

    /// 查看单个任务的完整状态，输出 JSON。
    Status {
        /// 任务 id，形如 `T-...`。
        id: String,
    },

    /// 列出全部任务，输出 JSON 数组。
    List {
        /// 按状态过滤，大小写不敏感。
        #[arg(long)]
        status: Option<String>,
    },

    /// 导出全部数据模型的 JSON Schema 到 stdout。
    Schema {
        /// 触发导出。`orcha schema --export > schema.json`。
        #[arg(long)]
        export: bool,
    },

    /// 在 workspace 跑 Cycleround 闭环（Plan → Code → Test → Review → Fix）。
    ///
    /// 报名帖熔断参数默认值：max_rounds=10 / max_retries=3 / cool_down=60s。
    /// 成功退出码 0，失败退出码 1。每轮 RoundRecord 持久化到
    /// `{home}/history/{task_id}.jsonl`，Task 状态迁移 PENDING → RUNNING → DONE/FAILED。
    Fix {
        /// 任务的自然语言描述，例如 "创建 hello.py 输出 hello"。
        description: String,

        /// workspace 路径（Orcha 会原地修改该目录）。
        /// 默认当前目录。
        #[arg(long, default_value = ".")]
        workspace: PathBuf,

        /// 最大轮次（默认 10，对齐报名帖熔断）。
        #[arg(long, default_value_t = 10)]
        max_rounds: u32,

        /// 最大重试次数（默认 3，对齐报名帖熔断）。
        #[arg(long, default_value_t = 3)]
        max_retries: u32,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Init) => {
            let store = FileTaskStore::new(resolve_home(cli.home.as_deref()));
            let dir = store.init()?;
            println!("{}", dir.display());
        }
        Some(Command::Run { description }) => {
            let store = FileTaskStore::new(resolve_home(cli.home.as_deref()));
            store.init()?;
            let task = Task::new(Task::generate_id(), description);
            store.insert(&task)?;
            println!("{}", task.id);
        }
        Some(Command::Status { id }) => {
            let store = FileTaskStore::new(resolve_home(cli.home.as_deref()));
            match store.get(&id)? {
                Some(task) => println!("{}", serde_json::to_string_pretty(&task)?),
                None => bail!("task not found: {id}"),
            }
        }
        Some(Command::List { status }) => {
            let store = FileTaskStore::new(resolve_home(cli.home.as_deref()));
            let filter = match status {
                Some(ref s) => Some(parse_status(s)?),
                None => None,
            };
            let tasks = store.list(filter)?;
            println!("{}", serde_json::to_string_pretty(&tasks)?);
        }
        Some(Command::Schema { export }) => {
            if !export {
                bail!("`orcha schema` requires --export");
            }
            let schema = orcha_sdk::schema_for_all();
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Some(Command::Fix {
            description,
            workspace,
            max_rounds,
            max_retries,
        }) => {
            let exit_code = run_fix(
                &resolve_home(cli.home.as_deref()),
                &workspace,
                description,
                max_rounds,
                max_retries,
            )?;
            // 直接 exit 以保证调用方能区分成功/失败（脚本/CI 用 $? 判断）。
            std::process::exit(exit_code);
        }
        None => {
            // 无子命令时打印简短帮助；clap 在 --help 时已自行处理。
            Cli::command().print_help()?;
        }
    }
    Ok(())
}

/// 解析 home：--home > $ORCHA_HOME > 默认 `./.orcha`。
///
/// clap 的 `env = "ORCHA_HOME"` 已把环境变量读进 `cli.home`，这里只补默认值。
fn resolve_home(cli_home: Option<&Path>) -> PathBuf {
    if let Some(h) = cli_home {
        return h.to_path_buf();
    }
    // 理论上 clap env 已处理，但保留兜底以防 env 为空字符串。
    if let Ok(h) = env::var("ORCHA_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    PathBuf::from("./.orcha")
}

/// 大小写不敏感地解析状态字符串。
fn parse_status(s: &str) -> Result<TaskStatus> {
    match s.to_ascii_uppercase().as_str() {
        "PENDING" => Ok(TaskStatus::Pending),
        "RUNNING" => Ok(TaskStatus::Running),
        "BLOCKED" => Ok(TaskStatus::Blocked),
        "DONE" => Ok(TaskStatus::Done),
        "FAILED" => Ok(TaskStatus::Failed),
        _ => bail!("invalid status: {s} (expected one of PENDING/RUNNING/BLOCKED/DONE/FAILED)"),
    }
}

// ============================================================
// `orcha fix` 实现（M3 Commit 6）
// ============================================================

/// 执行 `orcha fix` 闭环。
///
/// 步骤：
/// 1. 校验 workspace 是一个目录。
/// 2. 初始化 `FileTaskStore` + `FileHistoryStore`（同一 home，分别落 `store/` 与 `history/`）。
/// 3. 创建 Task（PENDING）并落盘，迁移到 RUNNING。
/// 4. 跑 [`Cycleround::run_with_history`]，每轮 `RoundRecord` 追加到 history 文件。
/// 5. 按结果迁移 Task 到 DONE / FAILED 并 update。
/// 6. 打印 JSON 结果到 stdout，返回退出码（0=成功，1=失败）。
///
/// 报名帖熔断参数：`max_rounds=10` / `max_retries=3` / `cool_down=60s`。
/// 调用方可通过 CLI 覆盖前两项；`cool_down` 当前确定性实现不真睡。
fn run_fix(
    home: &Path,
    workspace: &Path,
    description: String,
    max_rounds: u32,
    max_retries: u32,
) -> Result<i32> {
    if !workspace.is_dir() {
        bail!("workspace 不存在或不是目录: {}", workspace.display());
    }

    let task_store = FileTaskStore::new(home);
    task_store.init()?;
    let history_store = FileHistoryStore::new(home);
    history_store.init()?;

    // 创建 Task（PENDING）并迁移到 RUNNING。
    let mut task = Task::new(Task::generate_id(), description);
    task_store.insert(&task)?;
    transition(&mut task, TaskStatus::Running)?;
    task_store.update(&task)?;

    // 跑 Cycleround 闭环。cool_down 当前不真睡，仅作为配置存在。
    let config = CycleConfig {
        max_rounds,
        max_retries,
        cool_down: Duration::from_secs(60),
    };
    let cycle = Cycleround::new(config);
    let outcome = cycle.run_with_history(&task, workspace, &history_store);

    // 按结果迁移 Task 状态。
    let (status, exit_code) = match &outcome {
        CycleOutcome::Success { .. } => (TaskStatus::Done, 0),
        CycleOutcome::Failed { .. } => (TaskStatus::Failed, 1),
    };
    transition(&mut task, status)?;
    task_store.update(&task)?;

    // 打印 JSON 结果到 stdout。
    let result = build_fix_result(&task.id, &outcome, &history_store);
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(exit_code)
}

/// 从 `CycleOutcome` 聚合 artifacts。
///
/// - `Success`：直接用 outcome 里的 `artifacts`（已聚合全部轮）。
/// - `Failed`：outcome 不暴露 artifacts，从 history 各轮的 `artifacts` 聚合。
fn collect_artifacts(outcome: &CycleOutcome) -> Vec<Artifact> {
    match outcome {
        CycleOutcome::Success { artifacts, .. } => artifacts.clone(),
        CycleOutcome::Failed { history, .. } => {
            let mut all = Vec::new();
            for rec in history {
                all.extend(rec.artifacts.iter().cloned());
            }
            all
        }
    }
}

/// 构造 `orcha fix` 的 JSON 输出。
fn build_fix_result(
    task_id: &str,
    outcome: &CycleOutcome,
    history_store: &FileHistoryStore,
) -> serde_json::Value {
    let (outcome_name, rounds, reason, history_len) = match outcome {
        CycleOutcome::Success {
            rounds, history, ..
        } => ("Success", *rounds, None, history.len()),
        CycleOutcome::Failed {
            rounds,
            reason,
            history,
        } => (
            "Failed",
            *rounds,
            Some(match reason {
                FailureReason::MaxRoundsExceeded => "MaxRoundsExceeded",
                FailureReason::MaxRetriesExceeded => "MaxRetriesExceeded",
            }),
            history.len(),
        ),
    };

    let history_file = history_store
        .home()
        .join("history")
        .join(format!("{task_id}.jsonl"));

    serde_json::json!({
        "task_id": task_id,
        "outcome": outcome_name,
        "rounds": rounds,
        "reason": reason,
        "artifacts": collect_artifacts(outcome),
        "history_file": history_file.to_string_lossy(),
        "history_rounds": history_len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_core::{find_python, HistoryStore};
    use std::fs;
    use tempfile::tempdir;

    /// GT-CLI-1: 已有 test.py 的 workspace，`orcha fix` 单轮即成功，
    /// Task 落到 DONE，history 文件非空，退出码 0。
    #[test]
    fn fix_cli_succeeds_and_marks_task_done() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();

        let exit = run_fix(
            home.path(),
            ws.path(),
            "创建 hello.py 输出 hello".into(),
            5,
            3,
        )
        .expect("run_fix should not error");
        assert_eq!(exit, 0, "成功路径退出码应为 0");

        // hello.py 真实产出。
        let hello = ws.path().join("hello.py");
        assert!(hello.is_file(), "hello.py should be produced");
        assert_eq!(fs::read_to_string(&hello).unwrap(), "hello\n");

        // Task 状态 = DONE。
        let store = FileTaskStore::new(home.path());
        let tasks = store.list(None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Done);

        // history 文件存在且至少 1 条记录。
        let history = FileHistoryStore::new(home.path());
        let recs = history.list_history(&tasks[0].id).unwrap();
        assert!(!recs.is_empty(), "history 应有记录");
        assert_eq!(recs[0].round, 1);
    }

    /// GT-CLI-2: workspace 空白，Fixer 第 1 轮创建 test.py，第 2 轮才成功。
    #[test]
    fn fix_cli_succeeds_via_fixer_in_round_2() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        // workspace 完全空。

        let exit = run_fix(
            home.path(),
            ws.path(),
            "创建 greet.txt 输出 hi".into(),
            5,
            3,
        )
        .expect("run_fix should not error");
        assert_eq!(exit, 0);

        // greet.txt 真实产出。
        assert_eq!(
            fs::read_to_string(ws.path().join("greet.txt")).unwrap(),
            "hi\n"
        );
        // Fixer 创建的 test.py 应存在。
        assert!(ws.path().join("test.py").is_file());

        // history 应有 2 条记录。
        let store = FileTaskStore::new(home.path());
        let tasks = store.list(None).unwrap();
        let history = FileHistoryStore::new(home.path());
        let recs = history.list_history(&tasks[0].id).unwrap();
        assert_eq!(recs.len(), 2, "应有 2 轮 history");
    }

    /// GT-CLI-3: workspace 预置失败 test.py，Fixer 拒绝改写 → Failed → 退出码 1。
    #[test]
    fn fix_cli_fails_with_nonzero_exit_when_test_py_fails() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        fs::write(ws.path().join("test.py"), "assert False, 'intentional'\n").unwrap();

        let exit = run_fix(
            home.path(),
            ws.path(),
            "创建 hello.py 输出 hello".into(),
            5,
            2,
        )
        .expect("run_fix should not error");
        assert_eq!(exit, 1, "失败路径退出码应为 1");

        // Task 状态 = FAILED。
        let store = FileTaskStore::new(home.path());
        let tasks = store.list(None).unwrap();
        assert_eq!(tasks[0].status, TaskStatus::Failed);

        // Fixer 不应改写 test.py。
        let test_content = fs::read_to_string(ws.path().join("test.py")).unwrap();
        assert_eq!(
            test_content, "assert False, 'intentional'\n",
            "Fixer 不应改写已存在的 test.py"
        );
    }

    /// workspace 不存在时 run_fix 应返回 Err，而非 panic。
    #[test]
    fn fix_cli_rejects_missing_workspace() {
        let home = tempdir().unwrap();
        let err = run_fix(
            home.path(),
            Path::new("/nonexistent/workspace/path"),
            "x".into(),
            5,
            3,
        )
        .unwrap_err();
        assert!(err.to_string().contains("workspace 不存在"));
    }

    /// build_fix_result 在 Success / Failed 两种路径都应产出合法 JSON。
    #[test]
    fn build_fix_result_serializes_both_outcomes() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // Success path：构造一个最小 outcome。
        // 由于 CycleOutcome 字段较多，这里通过真实跑一遍来拿。
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-test-s".into(), "创建 hello.py 输出 hello".into());
        let hs = FileHistoryStore::new(home.path());
        hs.init().unwrap();
        let outcome = Cycleround::with_defaults().run_with_history(&task, ws.path(), &hs);
        let v = build_fix_result("T-test-s", &outcome, &hs);
        assert_eq!(v["outcome"], "Success");
        assert_eq!(v["rounds"], 1);
        assert!(v["reason"].is_null());
        assert!(v["history_file"]
            .as_str()
            .unwrap()
            .ends_with("T-test-s.jsonl"));

        // Failed path。
        let home2 = tempdir().unwrap();
        let ws2 = tempdir().unwrap();
        fs::write(ws2.path().join("test.py"), "assert False\n").unwrap();
        let task2 = Task::new("T-test-f".into(), "创建 hello.py 输出 hello".into());
        let hs2 = FileHistoryStore::new(home2.path());
        hs2.init().unwrap();
        let cfg = CycleConfig {
            max_rounds: 5,
            max_retries: 1,
            cool_down: Duration::from_secs(0),
        };
        let outcome2 = Cycleround::new(cfg).run_with_history(&task2, ws2.path(), &hs2);
        let v2 = build_fix_result("T-test-f", &outcome2, &hs2);
        assert_eq!(v2["outcome"], "Failed");
        assert_eq!(v2["reason"], "MaxRetriesExceeded");
    }
}
