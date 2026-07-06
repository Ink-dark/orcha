//! Orcha Gateway — 独立进程，任务队列与跨进程通信入口（M6）。
//!
//! M6 职责：
//! - 常驻进程，接 IM 触发（M7 接入飞书/QQ）
//! - 任务队列 + worker 池（非阻塞）
//! - 跨进程 IPC（`IpcTransport` trait，Unix Socket / Named Pipe / TCP）
//! - 共享 SQLite 存储（`SqliteTaskStore`）
//! - Cycleround 事件流消费（`mpsc::Receiver<RoundEvent>`）
//! - `config.toml` 统一配置与密钥管理
//! - trace ID 贯穿全链路
//!
//! 详见 docs/ROADMAP.md M6。

pub mod auth;
pub mod config;
pub mod ipc;
pub mod protocol;
pub mod queue;

use anyhow::Result;

/// Gateway 启动入口。
///
/// 初始化配置 → 启动 IPC 监听 → 起任务队列 + worker 池 → 常驻。
/// M6 阶段先跑通骨架，IM 接入见 M7。
pub fn run(config: config::GatewayConfig) -> Result<()> {
    tracing_gateway_init(&config);

    let store = config.build_task_store()?;
    let _store = store; // M6 骨架：store 暂不传入 queue，M7 接入后传
    let queue = queue::TaskQueue::new(config.cycle_config());

    // M6 阶段：阻塞主线程，M7 接入 IM 后改为事件驱动
    queue.blocking_serve();

    Ok(())
}

fn tracing_gateway_init(_config: &config::GatewayConfig) {
    // M6 阶段用 println，M7 接入 tracing crate
    eprintln!("[gateway] orcha-gateway starting (M6 skeleton)");
}
