// Orcha CLI entry point.
// See docs/ROADMAP.md M0 for the contract this crate fulfills.

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

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
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
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
        Some(Command::Schema { export }) => {
            if !export {
                anyhow::bail!("`orcha schema` requires --export");
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
