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
        /// 输出格式，目前仅支持 json。预留扩展。
        #[arg(long, default_value = "json")]
        format: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Schema { format }) => {
            if format != "json" {
                anyhow::bail!("unsupported schema format: {format} (only 'json' supported)");
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
