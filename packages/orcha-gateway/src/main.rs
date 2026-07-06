//! orcha-gateway 独立进程入口（M6）。
//!
//! 启动流程：
//! 1. 加载 config.toml（或默认配置）
//! 2. 初始化任务存储（SQLite 或 FileTaskStore）
//! 3. 启动任务队列 + worker
//! 4. 阻塞常驻（M6 阶段，M7 接入 IM 后改为事件驱动）

use std::path::PathBuf;

use orcha_gateway::{config::GatewayConfig, run};

fn main() -> anyhow::Result<()> {
    // 配置文件路径：--config 参数 > ORCHA_HOME/config.toml > ./config.toml
    let config_path = std::env::args()
        .position(|a| a == "--config")
        .and_then(|i| std::env::args().nth(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var("ORCHA_HOME").unwrap_or_else(|_| ".orcha".to_string());
            PathBuf::from(home).join("config.toml")
        });

    let config = GatewayConfig::load(&config_path)?;
    run(config)
}
