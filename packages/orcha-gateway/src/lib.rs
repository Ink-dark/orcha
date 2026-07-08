//! Orcha Gateway — 独立进程，任务队列与跨进程通信入口（M6→M7）。
//!
//! M7 职责：
//! - 常驻进程，经 Adapter 接 IM 触发（如 Feishu Adapter）
//! - IPC server（Unix Socket / TCP），接收 [`protocol::AdapterToGateway`] 消息
//! - 任务队列 + worker 跑 [`orcha_core::LlmCycleround`]
//! - 鉴权（白名单）+ 卡片流式更新（开始/结束 `CardUpdate` + 终态 `TaskResult`）
//! - watchdog：reader 35s read 超时，连续 3 次无心跳断开连接
//!
//! 详见 docs/ROADMAP.md M7。

pub mod adapter_registry;
pub mod approval_hook;
pub mod auth;
pub mod config;
pub mod ipc;
pub mod protocol;
pub mod queue;
pub mod runtime_whitelist;

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};

use orcha_core::{FileHistoryStore, FileMemoryStore, HistoryStore, MemoryStore, TaskStore};
use orcha_llm::LlmClient;
use orcha_sdk::Task;

use crate::adapter_registry::AdapterRegistry;
use crate::approval_hook::{handle_approval_response, PendingMap};
use crate::auth::Authenticator;
use crate::config::{ApprovalConfig, GatewayConfig};
use crate::ipc::{IpcAddr, IpcListener, IpcStream};
use crate::protocol::{
    AdapterToGateway, ApprovalDecisionDto, GatewayToAdapter, NotifyLevel, TriggerSource,
};
use crate::queue::{TaskQueue, TaskSubmitter};
use crate::runtime_whitelist::{RuntimeWhitelist, SharedRuntimeWhitelist};

/// reader read 超时（略大于 Adapter 心跳间隔 30s）。
const READ_TIMEOUT_SECS: u64 = 35;
/// 连续 read 超时次数阈值（≈105s 无心跳）视为 Adapter 死亡。
const MAX_HEARTBEAT_MISSES: u32 = 3;

/// Gateway 启动入口。
///
/// 初始化配置 → 构造依赖 → 起 worker → 起 IPC accept → 常驻。
pub fn run(config: GatewayConfig) -> Result<()> {
    eprintln!("[gateway] orcha-gateway starting (M7)");

    // 1. 构造依赖
    let task_store: Arc<dyn TaskStore> = build_task_store_arc(&config)?;
    let history_store: Arc<dyn HistoryStore> = {
        let h = FileHistoryStore::new(&config.home);
        h.init().context("init history store")?;
        Arc::new(h)
    };
    let memory_store: Arc<dyn MemoryStore> = {
        let m = FileMemoryStore::new(&config.home);
        m.init().context("init memory store")?;
        Arc::new(m)
    };
    let llm_client: Option<Arc<dyn LlmClient>> = config.build_llm_client();
    if llm_client.is_none() {
        eprintln!(
            "[gateway] 警告：未配置 LLM API Key（{} 环境变量），任务触发后会失败",
            config.llm.api_key_env
        );
    }
    let authenticator = config.authenticator();
    if authenticator.is_empty() {
        eprintln!("[gateway] 警告：auth.whitelist 为空，所有触发都会被拒绝");
    }

    // M7 P1：审批 pending map（Worker insert / Reader remove，共享 Arc<Mutex>）
    let pending: PendingMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let approval_config = Arc::new(config.approval.clone());
    if approval_config.enabled() {
        eprintln!(
            "[gateway] 人工审批已启用（timeout={}s write={} cmd={} del={}）",
            approval_config.timeout_secs,
            approval_config.write.as_ref().map(|v| v.len()).unwrap_or(0),
            approval_config
                .command
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0),
            approval_config
                .delete
                .as_ref()
                .map(|v| v.len())
                .unwrap_or(0),
        );
    } else {
        eprintln!("[gateway] 人工审批未启用（[approval] 未配置）");
    }

    let registry = AdapterRegistry::new();

    // M7 P2：加载运行时审批白名单（由"批准并加入白名单"按钮持久化）
    let runtime_wl_path = config.home.join("runtime_whitelist.toml");
    let runtime_whitelist: SharedRuntimeWhitelist =
        Arc::new(std::sync::Mutex::new(RuntimeWhitelist::load(&runtime_wl_path)));
    {
        let wl = runtime_whitelist.lock().unwrap();
        if !wl.write.is_empty() || !wl.command.is_empty() || !wl.delete.is_empty() {
            eprintln!(
                "[gateway] 运行时白名单已加载（write={} cmd={} del={}）",
                wl.write.len(),
                wl.command.len(),
                wl.delete.len()
            );
        }
    }

    // 2. TaskQueue + worker
    let queue = TaskQueue::new(
        task_store,
        history_store,
        memory_store,
        llm_client,
        config.cycle_config(),
        config.home.clone(),
        registry.clone(),
        pending.clone(),
        approval_config.clone(),
        Arc::new(config.workspace.clone()),
        runtime_whitelist.clone(),
    );

    // 3. IPC server
    let ipc_addr = IpcAddr::from_kind(&config.ipc.kind, &config.home, config.ipc.port);
    let listener = ipc::bind(&ipc_addr).context("bind IPC listener")?;
    eprintln!("[gateway] IPC listening on {}", format_ipc_addr(&ipc_addr));

    let submitter = queue.submitter();
    #[cfg(feature = "smoke")]
    let smoke_cancel_map = queue.cancel_map().clone();
    let accept_cancel_map = queue.cancel_map().clone();
    let accept_registry = registry.clone();
    let accept_auth = authenticator;
    let accept_pending = pending.clone();
    let accept_approval = approval_config.clone();
    let accept_wl = runtime_whitelist.clone();
    let accept_wl_path = runtime_wl_path.clone();
    thread::Builder::new()
        .name("orcha-accept".into())
        .spawn(move || {
            accept_loop(
                listener,
                accept_registry,
                submitter,
                accept_auth,
                accept_pending,
                accept_approval,
                accept_wl,
                accept_wl_path,
                accept_cancel_map,
            )
        })
        .context("spawn accept thread")?;

    // 4. Smoke 测试端点（仅 `smoke` feature，纯文本 TCP，一行触发一个任务）
    #[cfg(feature = "smoke")]
    {
        let smoke_port = config.ipc.smoke_port;
        if smoke_port > 0 {
            let smoke_submitter = queue.submitter();
            thread::Builder::new()
                .name("orcha-smoke".into())
                .spawn(move || {
                    smoke_listen(smoke_port, smoke_submitter, smoke_cancel_map);
                })
                .context("spawn smoke listener thread")?;
        }
    }

    // 5. 阻塞主线程（Ctrl-C 由 systemd / launchd 处理，M7 不引入 ctrl-c crate）
    queue.blocking_serve();
    Ok(())
}

/// Smoke 测试 TCP 监听：接受纯文本连接，每行 = 一个任务描述，直接入队。
///
/// 用法：`echo "帮我修复 xxx" | nc 127.0.0.1 7423`
/// 无需认证、无需 IPC 协议，仅用于本地开发调试。
/// 仅 `smoke` feature 启用时编译。
#[cfg(feature = "smoke")]
fn smoke_listen(port: u16, submitter: TaskSubmitter, cancel_map: crate::queue::TaskCancelMap) {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[gateway] smoke 端口 {port} 绑定失败: {e}");
            return;
        }
    };
    eprintln!("[gateway] smoke 测试端点: nc 127.0.0.1 {port}（一行 = 一个任务，/stop 取消全部）");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let submitter = submitter.clone();
                let cancel_map = cancel_map.clone();
                thread::spawn(move || {
                    let peer = stream.peer_addr().unwrap_or_else(|_| "unknown".parse().unwrap());
                    let mut reader = BufReader::new(&stream);
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => { /* EOF */ }
                        Ok(_) => {
                            let desc = line.trim().to_string();
                            if desc.is_empty() {
                                return;
                            }
                            // /stop / /cancel：取消所有运行中任务
                            if desc == "/stop" || desc == "/cancel" {
                                let map = cancel_map.lock().unwrap();
                                let ids: Vec<String> = map.keys().cloned().collect();
                                for id in &ids {
                                    if let Some(t) = map.get(id) {
                                        t.store(true, std::sync::atomic::Ordering::SeqCst);
                                    }
                                }
                                let msg = if ids.is_empty() {
                                    "没有运行中的任务".to_string()
                                } else {
                                    format!("已取消 {} 个任务: {}", ids.len(), ids.join(", "))
                                };
                                eprintln!("[gateway] smoke /stop: {msg}");
                                let _ = writeln!(&stream, "{msg}");
                                return;
                            }
                            // /stop <id>：取消指定任务
                            if let Some(id) = desc.strip_prefix("/stop ") {
                                let map = cancel_map.lock().unwrap();
                                let msg = if let Some(t) = map.get(id) {
                                    t.store(true, std::sync::atomic::Ordering::SeqCst);
                                    format!("已取消 {id}")
                                } else {
                                    format!("未找到任务 {id}")
                                };
                                eprintln!("[gateway] smoke /stop {id}: {msg}");
                                let _ = writeln!(&stream, "{msg}");
                                return;
                            }
                            // 普通文本 → 入队任务
                            eprintln!("[gateway] smoke 触发: {desc}");
                            let task = Task::new(Task::generate_id(), desc.clone());
                            let source = TriggerSource {
                                platform: "smoke".into(),
                                user: "smoke-test".into(),
                                group: None,
                                raw: desc.clone(),
                            };
                            if let Err(e) = submitter.enqueue(task, "smoke".into(), source) {
                                eprintln!("[gateway] smoke enqueue 失败: {e}");
                            }
                        }
                        Err(e) => {
                            eprintln!("[gateway] smoke 读取失败 ({}): {e}", peer);
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("[gateway] smoke accept 失败: {e}");
            }
        }
    }
}

/// 把 `config.build_task_store()` 的具体类型擦除为 `Arc<dyn TaskStore>`。
///
/// sqlite feature 启用时返回 `SqliteTaskStore`，否则 `FileTaskStore`，
/// 两者都 `impl TaskStore`，靠 unsized coercion 转 `Arc<dyn TaskStore>`。
fn build_task_store_arc(config: &GatewayConfig) -> Result<Arc<dyn TaskStore>> {
    let store = config.build_task_store()?;
    Ok(Arc::new(store))
}

/// accept 循环：每个连接 spawn 一个 reader 线程。
#[allow(clippy::too_many_arguments)]
fn accept_loop(
    listener: IpcListener,
    registry: AdapterRegistry,
    submitter: TaskSubmitter,
    auth: Authenticator,
    pending: PendingMap,
    approval_config: Arc<ApprovalConfig>,
    runtime_whitelist: SharedRuntimeWhitelist,
    wl_path: std::path::PathBuf,
    cancel_map: crate::queue::TaskCancelMap,
) {
    loop {
        match listener.accept() {
            Ok(stream) => {
                let registry = registry.clone();
                let submitter = submitter.clone();
                let auth = auth.clone();
                let pending = pending.clone();
                let approval_config = approval_config.clone();
                let runtime_whitelist = runtime_whitelist.clone();
                let wl_path = wl_path.clone();
                let cancel_map = cancel_map.clone();
                if let Err(e) = thread::Builder::new()
                    .name("orcha-adapter-conn".into())
                    .spawn(move || {
                        handle_connection(
                            stream,
                            registry,
                            submitter,
                            auth,
                            pending,
                            approval_config,
                            runtime_whitelist,
                            wl_path,
                            cancel_map,
                        )
                    })
                {
                    eprintln!("[gateway] spawn adapter conn thread 失败: {e}");
                }
            }
            Err(e) => {
                eprintln!("[gateway] accept 失败: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// 处理单条 Adapter 连接（reader 线程）。
///
/// - 注册到 registry，拿 `(id, sender, receiver)`
/// - 拆读写：writer 持原 stream，reader 持 `try_clone` 副本
/// - spawn writer 线程：receiver → `write_msg` 到 stream
/// - reader 循环：`read_msg` → 分发 `Trigger` / `AuthCheck` / `Heartbeat` / `Reply` / `ApprovalResponse`
/// - watchdog：read 超时计数，连续 3 次断开
#[allow(clippy::too_many_arguments)]
fn handle_connection(
    stream: Box<dyn IpcStream>,
    registry: AdapterRegistry,
    submitter: TaskSubmitter,
    auth: Authenticator,
    pending: PendingMap,
    approval_config: Arc<ApprovalConfig>,
    runtime_whitelist: SharedRuntimeWhitelist,
    wl_path: std::path::PathBuf,
    cancel_map: crate::queue::TaskCancelMap,
) {
    let (id, tx, rx) = registry.register();
    eprintln!(
        "[gateway] adapter 连接 id={id}（当前 {} 条）",
        registry.len()
    );

    // 拆读写：writer 持原 stream，reader 持 try_clone 副本
    let reader = match stream.try_clone() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gateway] adapter id={id} try_clone 失败: {e}");
            registry.unregister(id);
            return;
        }
    };
    let _ = reader.set_read_timeout(Some(Duration::from_secs(READ_TIMEOUT_SECS)));

    // writer 线程：从 receiver 读消息写到 stream
    let writer_handle = thread::Builder::new()
        .name(format!("orcha-adapter-writer-{id}"))
        .spawn(move || writer_loop(rx, stream))
        .expect("spawn writer thread");

    // reader 循环
    let mut reader = reader;
    let mut misses: u32 = 0;
    loop {
        match protocol::read_msg::<_, AdapterToGateway>(&mut reader) {
            Ok(Some(msg)) => {
                misses = 0;
                match msg {
                    AdapterToGateway::Trigger {
                        task_id,
                        description,
                        session,
                        source,
                    } => handle_trigger(
                        &tx,
                        &submitter,
                        &auth,
                        task_id,
                        description,
                        session,
                        source,
                    ),
                    AdapterToGateway::AuthCheck { user, group } => {
                        let r = auth.check_user_group(&user, group.as_deref());
                        let _ = tx.send(GatewayToAdapter::AuthResult {
                            allowed: r.allowed(),
                            reason: r.reason_str().map(String::from),
                        });
                    }
                    AdapterToGateway::Heartbeat { ts_ms } => {
                        let _ = tx.send(GatewayToAdapter::HeartbeatAck { ts_ms });
                    }
                    AdapterToGateway::Reply {
                        task_id,
                        content,
                        session,
                    } => {
                        let trimmed = content.trim();
                        if trimmed == "/stop" || trimmed == "/cancel" || trimmed.starts_with("/stop ") {
                            // 取消指定任务，或取消全部运行中任务
                            let target_id = if trimmed == "/stop" || trimmed == "/cancel" {
                                // 不指定 ID：取消全部运行中任务
                                if task_id.is_empty() {
                                    None // 取消全部
                                } else {
                                    Some(task_id.clone()) // 取消 Reply 携带的 task_id
                                }
                            } else {
                                // /stop <task_id>
                                let id = trimmed
                                    .strip_prefix("/stop ")
                                    .or_else(|| trimmed.strip_prefix("/cancel "))
                                    .unwrap_or("")
                                    .trim();
                                if id.is_empty() { None } else { Some(id.to_string()) }
                            };

                            eprintln!(
                                "[gateway] 收到取消命令 target={:?} session={session}",
                                target_id
                            );

                            let map = cancel_map.lock().unwrap();
                            let cancelled: Vec<String> = match target_id {
                                Some(ref id) => {
                                    if let Some(token) = map.get(id) {
                                        token.store(true, std::sync::atomic::Ordering::SeqCst);
                                        vec![id.clone()]
                                    } else {
                                        vec![]
                                    }
                                }
                                None => {
                                    // 取消全部运行中任务
                                    let ids: Vec<String> = map.keys().cloned().collect();
                                    for id in &ids {
                                        if let Some(token) = map.get(id) {
                                            token.store(true, std::sync::atomic::Ordering::SeqCst);
                                        }
                                    }
                                    ids
                                }
                            };

                            if cancelled.is_empty() {
                                let _ = tx.send(GatewayToAdapter::Notify {
                                    task_id: task_id.clone(),
                                    session: session.clone(),
                                    level: NotifyLevel::Warn,
                                    message: "未找到运行中的任务，可能已完成".into(),
                                });
                            } else {
                                let msg = if cancelled.len() == 1 {
                                    format!("⏳ 正在取消任务 {}…", cancelled[0])
                                } else {
                                    format!(
                                        "⏳ 正在取消 {} 个任务: {}…",
                                        cancelled.len(),
                                        cancelled.join(", ")
                                    )
                                };
                                let _ = tx.send(GatewayToAdapter::Notify {
                                    task_id: task_id.clone(),
                                    session: session.clone(),
                                    level: NotifyLevel::Warn,
                                    message: msg,
                                });
                            }
                        } else {
                            eprintln!(
                                "[gateway] 收到 Reply task_id={task_id} session={session} content=\"{content}\""
                            );
                        }
                    }
                    AdapterToGateway::ApprovalResponse {
                        action_id,
                        operator_open_id,
                        operator_chat_id,
                        decision,
                    } => {
                        // M7 P1：审批卡片回调。查 pending map → 过白名单 → 回传 worker
                        // → 广播 ApprovalResult 给 Adapter patch 卡片。
                        match handle_approval_response(
                            &pending,
                            &approval_config,
                            &action_id,
                            &operator_open_id,
                            operator_chat_id.as_deref(),
                            decision.clone(),
                            &runtime_whitelist,
                            &wl_path,
                        ) {
                            Some((_action, final_decision, operator)) => {
                                let _ = tx.send(GatewayToAdapter::ApprovalResult {
                                    action_id: action_id.clone(),
                                    decision: (&final_decision).into(),
                                    operator_open_id: operator,
                                });
                            }
                            None => {
                                // 未知 action_id（可能 worker 已超时清理）。
                                // 仍回 ApprovalResult 告知 Adapter，让卡片 patch 失败状态。
                                eprintln!(
                                    "[gateway] ApprovalResponse 收到未知 action_id={action_id}（可能已超时）"
                                );
                                let _ = tx.send(GatewayToAdapter::ApprovalResult {
                                    action_id,
                                    decision: ApprovalDecisionDto::Rejected {
                                        reason: "未知 action_id（可能已超时）".into(),
                                    },
                                    operator_open_id: None,
                                });
                            }
                        }
                    }
                }
            }
            Ok(None) => break, // EOF，对端关闭
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                misses += 1;
                eprintln!("[gateway] adapter id={id} 心跳超时 {misses}/{MAX_HEARTBEAT_MISSES}");
                if misses >= MAX_HEARTBEAT_MISSES {
                    eprintln!(
                        "[gateway] adapter id={id} 连续 {MAX_HEARTBEAT_MISSES} 次无心跳，断开"
                    );
                    break;
                }
            }
            Err(e) => {
                eprintln!("[gateway] adapter id={id} 读错误: {e}");
                break;
            }
        }
    }
    registry.unregister(id);
    // reader drop tx → writer 的 recv() 返回 Err 自然退出
    let _ = writer_handle.join();
    eprintln!("[gateway] adapter id={id} 连接已断开");
}

/// writer 线程：从 receiver 读 `GatewayToAdapter` 消息写到 stream。
fn writer_loop(rx: mpsc::Receiver<GatewayToAdapter>, mut stream: Box<dyn IpcStream>) {
    for msg in rx {
        if protocol::write_msg(&mut stream, &msg).is_err() {
            break;
        }
        if stream.flush().is_err() {
            break;
        }
    }
}

/// 处理 `Trigger` 消息：鉴权 → 入队。
#[allow(clippy::too_many_arguments)]
fn handle_trigger(
    tx: &mpsc::Sender<GatewayToAdapter>,
    submitter: &TaskSubmitter,
    auth: &Authenticator,
    task_id: String,
    description: String,
    session: String,
    source: TriggerSource,
) {
    // 1. 鉴权
    let r = auth.check(&source);
    if !r.allowed() {
        let reason = r.reason_str().map(String::from);
        eprintln!(
            "[gateway] 触发被拒绝 platform={} user={:?} group={:?} reason={}",
            source.platform,
            source.user,
            source.group,
            reason.as_deref().unwrap_or("未知")
        );
        let _ = tx.send(GatewayToAdapter::AuthResult {
            allowed: false,
            reason: reason.clone(),
        });
        let _ = tx.send(GatewayToAdapter::Notify {
            task_id: task_id.clone(),
            session,
            level: NotifyLevel::Warn,
            message: format!("触发被拒绝：{}", reason.unwrap_or_else(|| "未知".into())),
        });
        return;
    }

    // 2. 鉴权通过，回复 Adapter
    let _ = tx.send(GatewayToAdapter::AuthResult {
        allowed: true,
        reason: None,
    });

    // 3. 生成 task_id（若 Adapter 未带）
    let final_id = if task_id.trim().is_empty() {
        Task::generate_id()
    } else {
        task_id
    };
    let task = Task::new(final_id, description);

    // 4. 入队
    eprintln!(
        "[gateway] enqueue task_id={} desc={}",
        task.id, task.description
    );
    if let Err(e) = submitter.enqueue(task, session, source) {
        eprintln!("[gateway] enqueue 失败: {e}");
    }
}

/// 格式化 IPC 地址用于日志。
fn format_ipc_addr(addr: &IpcAddr) -> String {
    match addr {
        IpcAddr::Unix(p) => format!("unix:{}", p.display()),
        IpcAddr::Tcp(h, p) => format!("tcp://{h}:{p}"),
        #[cfg(windows)]
        IpcAddr::NamedPipe(n) => format!("pipe:{n}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::TriggerSource;

    /// 鉴权拒绝时，应发 AuthResult{false} + Notify，不入队。
    #[test]
    fn handle_trigger_denied_when_auth_fails() {
        use std::sync::mpsc;
        // 空白名单 → 任何触发都拒绝
        let auth = Authenticator::default();
        let (tx, rx) = mpsc::channel();
        let (submitter_tx, _submitter_rx) = mpsc::channel::<crate::queue::TaskSubmitter>();
        // TaskSubmitter 私有构造，这里用一个 trick：直接测 handle_trigger 逻辑
        // 用真实的 TaskSubmitter 需要走 TaskQueue::new，太重。
        // 改为白盒测试：直接构造一个能让 enqueue 失败的 submitter 不现实，
        // 这里改为只验证 auth 拒绝路径会发 AuthResult{false} + Notify。
        let _ = submitter_tx; // 占位避免 unused

        // 直接调 handle_trigger 需要 TaskSubmitter 实例。
        // TaskSubmitter::enqueue 走 mpsc::Sender<TaskMessage>，TaskMessage 私有。
        // 这里用一个最小 submitter：构造空 channel，enqueue 会成功但不消费。
        // 为了不依赖 TaskSubmitter 私有字段，跳过 enqueue 成功路径的断言，
        // 仅验证 auth 拒绝时 tx 收到 AuthResult{false} + Notify（这两条在 enqueue 之前 return）。
        let dummy_submitter = make_dummy_submitter();

        let source = TriggerSource {
            platform: "feishu".into(),
            user: "intruder".into(),
            group: None,
            raw: "@Orcha hack".into(),
        };
        handle_trigger(
            &tx,
            &dummy_submitter,
            &auth,
            "T-x".into(),
            "hack".into(),
            "s".into(),
            source,
        );

        // 应收到 AuthResult{false}
        let m1 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        match m1 {
            GatewayToAdapter::AuthResult { allowed, reason } => {
                assert!(!allowed, "空白名单应拒绝");
                assert!(reason.is_some(), "应带拒绝原因");
            }
            other => panic!("第一条应是 AuthResult，实际 {other:?}"),
        }
        // 应收到 Notify
        let m2 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(matches!(m2, GatewayToAdapter::Notify { .. }));
    }

    /// 构造一个会丢弃所有 TaskMessage 的 submitter（enqueue 成功但无人消费）。
    fn make_dummy_submitter() -> TaskSubmitter {
        // TaskSubmitter 内部字段私有，无法直接构造。
        // 通过 TaskQueue::new 创建真实队列，拿 submitter。
        // 但 TaskQueue::new 需要一堆依赖。这里用一个临时目录构造。
        let dir = tempfile::tempdir().unwrap();
        let store = orcha_core::FileTaskStore::new(dir.path());
        store.init().unwrap();
        let task_store: Arc<dyn TaskStore> = Arc::new(store);

        let history = orcha_core::FileHistoryStore::new(dir.path());
        history.init().unwrap();
        let history_store: Arc<dyn HistoryStore> = Arc::new(history);

        let memory = orcha_core::FileMemoryStore::new(dir.path());
        memory.init().unwrap();
        let memory_store: Arc<dyn MemoryStore> = Arc::new(memory);

        let registry = AdapterRegistry::new();
        let pending: PendingMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let approval_config: Arc<ApprovalConfig> = Arc::new(ApprovalConfig::default());
        let workspace_config: Arc<crate::config::WorkspaceConfig> =
            Arc::new(crate::config::WorkspaceConfig::default());
        let runtime_whitelist: SharedRuntimeWhitelist =
            Arc::new(std::sync::Mutex::new(RuntimeWhitelist::default()));
        let queue = TaskQueue::new(
            task_store,
            history_store,
            memory_store,
            None,
            orcha_core::CycleConfig::default(),
            dir.path().to_path_buf(),
            registry,
            pending,
            approval_config,
            workspace_config,
            runtime_whitelist,
        );
        // queue drop 会让 worker 退出，但 submitter 持 sender clone 仍可用
        queue.submitter()
    }
}
