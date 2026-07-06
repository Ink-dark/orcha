//! 任务队列 + worker（M6 骨架 → M7 接入 LlmCycleround）。
//!
//! worker 线程收到任务后：
//! 1. `task_store.update` → RUNNING
//! 2. fanout `CardUpdate{phase:"🚀 启动"}`
//! 3. 构造 `LlmCycleround::with_memory`，跑 `run_with_history`
//! 4. 根据 `CycleOutcome`：
//!    - `Success` → `store.update(Done)` + fanout `TaskResult{success}`
//!    - `Failed`  → `store.update(Failed)` + fanout `TaskResult{failed}` + `Notify`
//!
//! 流式深入到每 step 留到 M8（需给 `LlmCycleround` 加 `run_streaming`）。
//! M7 仅发"开始/结束"两条 `CardUpdate` + 终态 `TaskResult`。

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;

use anyhow::Result;
use orcha_core::{
    transition, CycleConfig, CycleOutcome, FailureReason, HistoryStore, LlmCycleround, MemoryStore,
    TaskStore,
};
use orcha_llm::LlmClient;
use orcha_sdk::{Task, TaskStatus};

use crate::adapter_registry::AdapterRegistry;
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
    mut task: Task,
    session: String,
    source: TriggerSource,
) {
    let task_id = task.id.clone();

    // 1. RUNNING
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

    // 3. workspace（每任务独立目录，避免并发互相覆盖）
    let workspace = home.join("sessions").join(&task_id);
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

    // 4. 跑 Cycleround
    let outcome = match llm_client {
        Some(client) => {
            let cycle = LlmCycleround::with_memory(
                cycle_config.clone(),
                client.clone(),
                memory_store.clone(),
            );
            cycle.run_with_history(&task, &workspace, history_store.as_ref())
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

    // 5. 根据 outcome 更新状态 + fanout
    match outcome {
        CycleOutcome::Success {
            rounds, artifacts, ..
        } => {
            let _ = transition(&mut task, TaskStatus::Done);
            let _ = task_store.update(&task);
            let art_urls: Vec<String> = artifacts.iter().filter_map(|a| a.url.clone()).collect();
            registry.broadcast(&GatewayToAdapter::CardUpdate {
                task_id: task_id.clone(),
                session: session.clone(),
                phase: "✅ 完成".into(),
                detail: format!("共 {} 轮", rounds),
                progress: 100,
            });
            registry.broadcast(&GatewayToAdapter::TaskResult {
                task_id,
                session,
                outcome: "success".into(),
                summary: format!("任务完成，共 {} 轮", rounds),
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
        let queue = TaskQueue::new(
            task_store,
            history_store,
            memory_store,
            None, // 无 LLM client
            CycleConfig::default(),
            home.to_path_buf(),
            registry.clone(),
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
