//! 任务队列 + worker（M6 骨架 → M7 接入 AiDrivenCycleround）。
//!
//! worker 线程收到任务后：
//! 1. `task_store.update` → RUNNING
//! 2. fanout `CardUpdate{phase:"🚀 启动"}`
//! 3. 构造 `AiDrivenCycleround`（按 `approval_config.enabled()` 选 with_approval / with_memory）
//! 4. 跑 `run_with_history`
//! 5. 根据 `CycleOutcome`：
//!    - `Success` → `store.update(Done)` + fanout `TaskResult{success}`
//!    - `Failed`  → `store.update(Failed)` + fanout `TaskResult{failed}` + `Notify`
//!
//! 流式深入到每 step 留到 M8（需给 `AiDrivenCycleround` 加 `run_streaming`）。
//! M7 仅发"开始/结束"两条 `CardUpdate` + 终态 `TaskResult`。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use orcha_core::{
    transition, AiDrivenCycleround, CycleConfig, CycleOutcome, FailureReason, HistoryStore,
    MemoryStore, TaskStore,
};
use orcha_llm::LlmClient;
use orcha_sdk::{Task, TaskStatus};

use crate::adapter_registry::AdapterRegistry;
use crate::approval_hook::{GatewayApprovalHook, PendingMap};
use crate::config::{ApprovalConfig, WorkspaceConfig};
use crate::protocol::{GatewayToAdapter, NotifyLevel, TriggerSource};

/// worker 之间传递的任务消息。
///
/// `Run` 用 `Box<RunPayload>` 装：避免 `Task` 字段让整个 enum 变大
/// （clippy::large_enum_variant），mpsc channel 每次传指针而非整个 Task。
enum TaskMessage {
    Run(Box<RunPayload>),
    Shutdown,
}

/// `TaskMessage::Run` 的载荷（独立结构便于 `Box` 包装）。
struct RunPayload {
    task: Task,
    session: String,
    source: TriggerSource,
}

/// 任务提交器（可 Clone，给 IPC reader 线程持有）。
#[derive(Clone)]
pub struct TaskSubmitter {
    tx: mpsc::Sender<TaskMessage>,
}

impl TaskSubmitter {
    /// 入队一个任务（非阻塞）。
    pub fn enqueue(&self, task: Task, session: String, source: TriggerSource) -> Result<()> {
        self.tx
            .send(TaskMessage::Run(Box::new(RunPayload {
                task,
                session,
                source,
            })))
            .map_err(|_| anyhow::anyhow!("worker 线程已关闭"))?;
        Ok(())
    }
}

/// 任务队列：入队 + 后台 worker。
///
/// M7 单 worker 串行执行（worker 池见 M8）。
pub struct TaskQueue {
    submitter: TaskSubmitter,
    handle: Option<thread::JoinHandle<()>>,
}

impl TaskQueue {
    /// 构建队列并启动后台 worker。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        task_store: Arc<dyn TaskStore>,
        history_store: Arc<dyn HistoryStore>,
        memory_store: Arc<dyn MemoryStore>,
        llm_client: Option<Arc<dyn LlmClient>>,
        cycle_config: CycleConfig,
        home: PathBuf,
        registry: AdapterRegistry,
        pending: PendingMap,
        approval_config: Arc<ApprovalConfig>,
        workspace_config: Arc<WorkspaceConfig>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<TaskMessage>();
        let submitter = TaskSubmitter { tx };

        let handle = thread::Builder::new()
            .name("orcha-worker".into())
            .spawn(move || {
                for msg in rx {
                    match msg {
                        TaskMessage::Run(payload) => {
                            let RunPayload {
                                task,
                                session,
                                source,
                            } = *payload;
                            run_task(
                                &task_store,
                                &history_store,
                                &memory_store,
                                llm_client.as_ref(),
                                &cycle_config,
                                &home,
                                &registry,
                                &pending,
                                &approval_config,
                                &workspace_config,
                                task,
                                session,
                                source,
                            );
                        }
                        TaskMessage::Shutdown => break,
                    }
                }
                eprintln!("[gateway] worker 已退出");
            })
            .expect("spawn worker thread");

        Self {
            submitter,
            handle: Some(handle),
        }
    }

    /// 拿提交器（给 IPC reader 线程 enqueue 用）。
    pub fn submitter(&self) -> TaskSubmitter {
        self.submitter.clone()
    }

    /// 阻塞主线程（M7 用，Ctrl-C 由 systemd 处理）。
    pub fn blocking_serve(self) {
        eprintln!("[gateway] 任务队列已启动（Ctrl-C 退出）");
        loop {
            thread::park();
        }
    }

    /// 优雅关闭。
    pub fn shutdown(&mut self) {
        let _ = self.submitter.tx.send(TaskMessage::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// 单任务执行（worker 线程内同步）。
#[allow(clippy::too_many_arguments)]
fn run_task(
    task_store: &Arc<dyn TaskStore>,
    history_store: &Arc<dyn HistoryStore>,
    memory_store: &Arc<dyn MemoryStore>,
    llm_client: Option<&Arc<dyn LlmClient>>,
    cycle_config: &CycleConfig,
    home: &Path,
    registry: &AdapterRegistry,
    pending: &PendingMap,
    approval_config: &Arc<ApprovalConfig>,
    workspace_config: &Arc<WorkspaceConfig>,
    mut task: Task,
    session: String,
    source: TriggerSource,
) {
    let task_id = task.id.clone();
    eprintln!(
        "[gateway] run_task task_id={task_id} session={session} desc={}",
        task.description
    );

    // 1. 持久化 task（幂等：已存在则忽略）+ RUNNING
    let _ = task_store.insert(&task);
    let _ = transition(&mut task, TaskStatus::Running);
    let _ = task_store.update(&task);

    // 2. CardUpdate 启动
    registry.broadcast(&GatewayToAdapter::CardUpdate {
        task_id: task_id.clone(),
        session: session.clone(),
        phase: "🚀 任务启动".into(),
        detail: format!("来源: {} @ {}", source.platform, source.user),
        progress: 5,
    });

    // 3. workspace 选择：
    //    - 配了 [workspace] repo + worktree=true：在 repo 下创建 GitWorktree 隔离
    //    - 配了 [workspace] repo + worktree=false：原地修改（不推荐）
    //    - 未配 [workspace]：用空目录 home/sessions/{task_id}（纯生成任务）
    let workspace: PathBuf;
    let worktree_guard: Option<orcha_core::GitWorktree>; // 保持 guard 不 drop 直到任务结束
    match &workspace_config.repo {
        Some(repo) if workspace_config.worktree => {
            // 尝试 GitWorktree 隔离
            match orcha_core::GitWorktree::new(repo) {
                Ok(wt) => {
                    eprintln!(
                        "[gateway] task {task_id} worktree: {} (源 repo: {})",
                        wt.path().display(),
                        repo.display()
                    );
                    workspace = wt.path().to_path_buf();
                    worktree_guard = Some(wt);
                }
                Err(e) => {
                    eprintln!("[gateway] task {task_id} worktree 创建失败，回退到原地修改: {e}");
                    workspace = repo.clone();
                    worktree_guard = None;
                }
            }
        }
        Some(repo) => {
            // 原地修改模式
            workspace = repo.clone();
            worktree_guard = None;
        }
        None => {
            // 未配 repo：用空目录（纯生成任务）
            workspace = home.join("sessions").join(&task_id);
            if let Err(e) = std::fs::create_dir_all(&workspace) {
                finish_failed(
                    task_store,
                    &mut task,
                    &task_id,
                    &session,
                    registry,
                    format!("创建 workspace 失败: {e}"),
                );
                return;
            }
            worktree_guard = None;
        }
    }
    eprintln!(
        "[gateway] task {task_id} workspace = {}",
        workspace.display()
    );

    // 4. 起心跳线程：每 30 秒推一条 CardUpdate，让用户看到任务没卡死。
    //    覆盖 Observer/Planner/Worker/Tester/Reviewer 所有阶段的长耗时操作
    //    （特别是 cargo test 可能跑几分钟，期间无 RoundEvent 推送）。
    //    纯 Gateway 层方案，不改 orcha-core。
    let heartbeat_stop = Arc::new(AtomicBool::new(false));
    let heartbeat_handle = {
        let registry = registry.clone();
        let task_id_hb = task_id.clone();
        let session_hb = session.clone();
        let stop = heartbeat_stop.clone();
        thread::Builder::new()
            .name(format!("orcha-heartbeat-{task_id_hb}"))
            .spawn(move || {
                let mut elapsed = 0u64;
                while !stop.load(Ordering::SeqCst) {
                    // 分段 sleep 以便能快速响应 stop 信号
                    for _ in 0..30 {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        thread::sleep(Duration::from_secs(1));
                    }
                    if stop.load(Ordering::SeqCst) {
                        return;
                    }
                    elapsed += 30;
                    registry.broadcast(&GatewayToAdapter::CardUpdate {
                        task_id: task_id_hb.clone(),
                        session: session_hb.clone(),
                        phase: "⏳ 执行中".into(),
                        detail: format!("已运行 {} 秒", elapsed),
                        progress: 50,
                    });
                }
            })
            .expect("spawn heartbeat thread")
    };

    // 5. 跑 Cycleround（M7 P1：按 approval_config 选 with_approval / with_memory）
    let outcome = match llm_client {
        Some(client) => {
            if approval_config.enabled() {
                // 审批启用：构造 GatewayApprovalHook，worker 发起 → reader 回传
                let hook = GatewayApprovalHook::new(
                    pending.clone(),
                    registry.clone(),
                    task_id.clone(),
                    session.clone(),
                    approval_config.timeout(),
                );
                let cycle = AiDrivenCycleround::with_approval(
                    cycle_config.clone(),
                    client.clone(),
                    Some(memory_store.clone()),
                    Arc::new(hook),
                );
                cycle.run_with_history(&task, &workspace, history_store.as_ref())
            } else {
                // 审批未启用：直接走 with_memory（无 hook）
                let cycle = AiDrivenCycleround::with_memory(
                    cycle_config.clone(),
                    client.clone(),
                    memory_store.clone(),
                );
                cycle.run_with_history(&task, &workspace, history_store.as_ref())
            }
        }
        None => {
            // 无 LLM client（未配置 key）：直接失败，告知用户。
            registry.broadcast(&GatewayToAdapter::Notify {
                task_id: task_id.clone(),
                session: session.clone(),
                level: NotifyLevel::Error,
                message: "未配置 LLM API Key，无法执行任务".into(),
            });
            CycleOutcome::Failed {
                rounds: 0,
                reason: FailureReason::MaxRetriesExceeded,
                history: vec![],
            }
        }
    };

    // 停止心跳线程
    heartbeat_stop.store(true, Ordering::SeqCst);
    let _ = heartbeat_handle.join();

    // 6. 根据 outcome 更新状态 + fanout
    match outcome {
        CycleOutcome::Success {
            rounds, artifacts, ..
        } => {
            // 任务成功：把 worktree 改动推到原 repo 的新分支（orcha/{slug}）
            let branch_info = if let Some(wt) = worktree_guard.as_ref() {
                match persist_worktree_to_branch(wt, &task, llm_client) {
                    Ok(info) => {
                        eprintln!(
                            "[gateway] task {task_id} 改动已推到分支: {} (commit {})",
                            info.branch, info.commit_short
                        );
                        Some(info)
                    }
                    Err(e) => {
                        eprintln!(
                            "[gateway] task {task_id} 推分支失败，改动随 worktree 清理丢失: {e}"
                        );
                        None
                    }
                }
            } else {
                None
            };

            let _ = transition(&mut task, TaskStatus::Done);
            let _ = task_store.update(&task);
            let art_urls: Vec<String> = artifacts.iter().filter_map(|a| a.url.clone()).collect();

            // 广播完成卡片（含分支信息）
            let detail = match &branch_info {
                Some(info) => format!(
                    "共 {} 轮，分支: {} ({})",
                    rounds, info.branch, info.commit_short
                ),
                None => format!("共 {} 轮", rounds),
            };
            registry.broadcast(&GatewayToAdapter::CardUpdate {
                task_id: task_id.clone(),
                session: session.clone(),
                phase: "✅ 完成".into(),
                detail,
                progress: 100,
            });

            let summary = match &branch_info {
                Some(info) => format!(
                    "任务完成，共 {} 轮。改动已推到分支 `{}` (commit {})\n\n```\n{}\n```",
                    rounds, info.branch, info.commit_short, info.commit_message
                ),
                None => format!("任务完成，共 {} 轮", rounds),
            };
            registry.broadcast(&GatewayToAdapter::TaskResult {
                task_id,
                session,
                outcome: "success".into(),
                summary,
                artifacts: art_urls,
            });
        }
        CycleOutcome::Failed { rounds, reason, .. } => {
            let _ = transition(&mut task, TaskStatus::Failed);
            let _ = task_store.update(&task);
            registry.broadcast(&GatewayToAdapter::Notify {
                task_id: task_id.clone(),
                session: session.clone(),
                level: NotifyLevel::Warn,
                message: format!("任务失败（{} 轮）：{:?}", rounds, reason),
            });
            registry.broadcast(&GatewayToAdapter::TaskResult {
                task_id,
                session,
                outcome: "failed".into(),
                summary: format!("失败原因: {:?}", reason),
                artifacts: vec![],
            });
        }
    }
}

/// 失败收尾（workspace 创建失败等早期错误）。
#[allow(clippy::too_many_arguments)]
fn finish_failed(
    task_store: &Arc<dyn TaskStore>,
    task: &mut Task,
    task_id: &str,
    session: &str,
    registry: &AdapterRegistry,
    reason: String,
) {
    let _ = transition(task, TaskStatus::Failed);
    let _ = task_store.update(task);
    registry.broadcast(&GatewayToAdapter::TaskResult {
        task_id: task_id.to_string(),
        session: session.to_string(),
        outcome: "failed".into(),
        summary: reason,
        artifacts: vec![],
    });
}

/// 推到新分支的结果。
struct BranchInfo {
    /// 分支名（形如 `orcha/{slug}`）
    branch: String,
    /// commit 短 hash（前 7 位）
    commit_short: String,
    /// commit message
    commit_message: String,
}

/// 把 worktree 里的改动 commit 并推到原 repo 的新分支。
///
/// 流程：
/// 1. 在 worktree 里 `git add -A` 暂存所有改动
/// 2. 若无改动（worktree 干净），跳过
/// 3. 让 LLM 生成 commit message 和分支名（`orcha/{slug}`）
/// 4. 在 worktree 里 `git commit -m "{message}"`
/// 5. 在 worktree 里 `git checkout -b {branch}`
/// 6. 返回分支信息
///
/// worktree drop 时会 `git worktree remove --force`，但 commit 已写入原 repo
/// 的对象库（worktree 共享 .git），所以分支和 commit 不会丢。
fn persist_worktree_to_branch(
    wt: &orcha_core::GitWorktree,
    task: &Task,
    llm_client: Option<&Arc<dyn LlmClient>>,
) -> Result<BranchInfo> {
    use anyhow::Context;

    let worktree_path = wt.path();
    let worktree_str = worktree_path.to_string_lossy();

    // 1. git add -A
    let add_output = std::process::Command::new("git")
        .args(["-C", &worktree_str, "add", "-A"])
        .output()
        .with_context(|| "执行 git add -A 失败")?;
    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr);
        anyhow::bail!("git add -A 失败: {stderr}");
    }

    // 2. 检查是否有改动（git diff --cached --quiet 退出码 0 = 无改动）
    let diff_status = std::process::Command::new("git")
        .args(["-C", &worktree_str, "diff", "--cached", "--quiet"])
        .status()
        .with_context(|| "执行 git diff --cached 失败")?;
    if diff_status.success() {
        anyhow::bail!("worktree 无改动可提交");
    }

    // 3. 让 LLM 生成 commit message 和分支名；LLM 不可用时用 task description 退化
    let (commit_message, branch_slug) = match llm_client {
        Some(client) => generate_commit_info(client, &task.description).unwrap_or_else(|e| {
            eprintln!("[gateway] LLM 生成 commit 信息失败，回退到默认: {e}");
            fallback_commit_info(&task.description)
        }),
        None => fallback_commit_info(&task.description),
    };
    let branch = format!("orcha/{branch_slug}");

    // 4. git commit
    let commit_output = std::process::Command::new("git")
        .args([
            "-C",
            &worktree_str,
            "commit",
            "-m",
            &commit_message,
            "--author=Orcha Bot <orcha@local>",
        ])
        .output()
        .with_context(|| "执行 git commit 失败")?;
    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        anyhow::bail!("git commit 失败: {stderr}");
    }

    // 5. 取 commit 短 hash
    let rev_parse_output = std::process::Command::new("git")
        .args(["-C", &worktree_str, "rev-parse", "--short=7", "HEAD"])
        .output()
        .with_context(|| "执行 git rev-parse 失败")?;
    let commit_short = String::from_utf8_lossy(&rev_parse_output.stdout)
        .trim()
        .to_string();

    // 6. 在 worktree 里创建分支（指向当前 commit）
    //    git checkout -b 会创建分支并切换，但 worktree 即将 drop，切换与否无所谓
    let branch_output = std::process::Command::new("git")
        .args(["-C", &worktree_str, "branch", &branch])
        .output()
        .with_context(|| "执行 git branch 失败")?;
    if !branch_output.status.success() {
        let stderr = String::from_utf8_lossy(&branch_output.stderr);
        // 分支已存在？尝试带时间戳的备选名
        if stderr.contains("already exists") {
            let timestamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let alt_branch = format!("orcha/{branch_slug}-{timestamp}");
            eprintln!("[gateway] 分支 {branch} 已存在，改用 {alt_branch}");
            let alt_output = std::process::Command::new("git")
                .args(["-C", &worktree_str, "branch", &alt_branch])
                .output()
                .with_context(|| "执行 git branch (备选) 失败")?;
            if !alt_output.status.success() {
                let stderr2 = String::from_utf8_lossy(&alt_output.stderr);
                anyhow::bail!("git branch {alt_branch} 失败: {stderr2}");
            }
            return Ok(BranchInfo {
                branch: alt_branch,
                commit_short,
                commit_message,
            });
        }
        anyhow::bail!("git branch {branch} 失败: {stderr}");
    }

    Ok(BranchInfo {
        branch,
        commit_short,
        commit_message,
    })
}

/// 让 LLM 生成 commit message 和分支名 slug。
///
/// 返回 `(commit_message, branch_slug)`，branch_slug 应是 kebab-case，
/// 只含小写字母/数字/连字符，不带 `orcha/` 前缀。
fn generate_commit_info(
    client: &Arc<dyn LlmClient>,
    task_description: &str,
) -> Result<(String, String)> {
    use anyhow::Context;
    use orcha_llm::{ChatMessage, ToolDefinition};

    let system = "你是 commit message 生成器。根据任务描述生成：(1) 一行 commit message（祈使句，英文，<=72 字符），(2) 分支名 slug（kebab-case，只含小写字母/数字/连字符，<=40 字符，不含 orcha/ 前缀）。\n\n示例：\n任务：在 utils/mod.rs 末尾追加 reverse_string 函数\n输出：{\"commit_message\": \"feat(utils): add reverse_string function\", \"branch_slug\": \"add-reverse-string\"}";
    let msgs = vec![
        ChatMessage::system(system),
        ChatMessage::user(format!("任务：{task_description}")),
    ];
    let tools = vec![ToolDefinition::new(
        "commit_info",
        "生成 commit message 和分支名",
        serde_json::json!({
            "type": "object",
            "properties": {
                "commit_message": {"type": "string", "description": "一行 commit message，祈使句，英文，<=72 字符"},
                "branch_slug": {"type": "string", "description": "分支名 slug，kebab-case，<=40 字符，不含 orcha/ 前缀"}
            },
            "required": ["commit_message", "branch_slug"]
        }),
    )];
    let resp = client
        .chat_with_tools(&msgs, &tools)
        .with_context(|| "调用 LLM 生成 commit info 失败")?;

    // 优先从 tool_calls 取
    for call in &resp.tool_calls {
        if call.function.name == "commit_info" {
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .with_context(|| "解析 commit_info 参数失败")?;
            let commit_message = args
                .get("commit_message")
                .and_then(|v| v.as_str())
                .unwrap_or("chore: apply orcha task changes")
                .to_string();
            let branch_slug = sanitize_branch_slug(
                args.get("branch_slug")
                    .and_then(|v| v.as_str())
                    .unwrap_or("task"),
            );
            return Ok((commit_message, branch_slug));
        }
    }

    // 兜底：用默认
    Ok(fallback_commit_info(task_description))
}

/// 退化方案：从 task description 生成简单的 commit message 和 slug。
fn fallback_commit_info(task_description: &str) -> (String, String) {
    let slug = sanitize_branch_slug(&task_description.chars().take(40).collect::<String>());
    (
        format!("chore: apply orcha task ({})", &slug),
        if slug.is_empty() {
            "orcha-task".into()
        } else {
            slug
        },
    )
}

/// 清洗字符串为合法的 git 分支 slug（kebab-case）。
fn sanitize_branch_slug(s: &str) -> String {
    let lower: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    lower
        .trim_matches('-')
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_core::{FileHistoryStore, FileMemoryStore, FileTaskStore};
    use tempfile::tempdir;

    /// 构造测试用 TaskQueue + 依赖（无 LLM client，任务会直接 failed）。
    fn setup_queue(home: &Path) -> (TaskQueue, AdapterRegistry) {
        let store = FileTaskStore::new(home);
        store.init().unwrap();
        let task_store: Arc<dyn TaskStore> = Arc::new(store);

        let history = FileHistoryStore::new(home);
        history.init().unwrap();
        let history_store: Arc<dyn HistoryStore> = Arc::new(history);

        let memory = FileMemoryStore::new(home);
        memory.init().unwrap();
        let memory_store: Arc<dyn MemoryStore> = Arc::new(memory);

        let registry = AdapterRegistry::new();
        let pending: PendingMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let approval_config: Arc<ApprovalConfig> = Arc::new(ApprovalConfig::default());
        let workspace_config: Arc<WorkspaceConfig> = Arc::new(WorkspaceConfig::default());
        let queue = TaskQueue::new(
            task_store,
            history_store,
            memory_store,
            None, // 无 LLM client
            CycleConfig::default(),
            home.to_path_buf(),
            registry.clone(),
            pending,
            approval_config,
            workspace_config,
        );
        (queue, registry)
    }

    #[test]
    fn worker_fails_gracefully_without_llm_client() {
        // 无 LLM client 时，任务应直接 failed 并广播 TaskResult，
        // 而非 panic 或挂起。
        let dir = tempdir().unwrap();
        let (mut queue, registry) = setup_queue(dir.path());
        let (_id, _tx, rx) = registry.register();

        queue
            .submitter()
            .enqueue(
                Task::new("T-nollm".into(), "test".into()),
                "s".into(),
                TriggerSource {
                    platform: "p".into(),
                    user: "u".into(),
                    group: None,
                    raw: String::new(),
                },
            )
            .unwrap();

        // 收消息直到 TaskResult（无 client 路径：
        // CardUpdate(启动) + Notify(error) + Notify(warn) + TaskResult(failed)）
        let mut last_outcome = None;
        for _ in 0..8 {
            match rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(GatewayToAdapter::TaskResult { outcome, .. }) => {
                    last_outcome = Some(outcome);
                    break;
                }
                Ok(_) => continue,
                Err(_) => break,
            }
        }
        queue.shutdown();
        assert_eq!(
            last_outcome.as_deref(),
            Some("failed"),
            "无 LLM client 应以 failed 收尾"
        );
    }
}
