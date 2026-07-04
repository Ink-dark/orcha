// Orcha CLI entry point.
// See docs/ROADMAP.md M0/M1 for the contract this crate fulfills.

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser, Subcommand};
use orcha_core::{FileTaskStore, TaskStore};
use orcha_sdk::{Task, TaskStatus};

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
