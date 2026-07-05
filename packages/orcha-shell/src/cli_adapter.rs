//! CLI Adapter：把 [`OrchaEvent`] 桥接到 `orcha-core` 的 Cycleround 闭环。
//!
//! 这是 M5 的「CLI 适配器」端到端验收实现：
//! 1. 从 `OrchaEvent.payload.raw_text` 取任务描述。
//! 2. 创建 Task 并迁移到 RUNNING，落盘到 FileTaskStore。
//! 3. 跑 `Cycleround::run_with_history`，每轮 RoundRecord 追加到 history。
//! 4. 把结果包装成 [`orcha_sdk::OrchaResponse`]（FINAL / ERROR）。
//! 5. 内部 `Mutex<HashMap<event_id, OrchaResponse>>` 供 `GET /status/{id}` 查询。
//!
//! M5 MVP 只支持 `EventType::UserPrompt`（raw_text 即任务描述）。
//! M7 / M8 会扩展更多事件类型与 Adapter。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use orcha_core::{
    transition, CycleConfig, CycleOutcome, Cycleround, FileHistoryStore, FileTaskStore, TaskStore,
};
use orcha_sdk::{
    EventType, OrchaEvent, OrchaResponse, ResponseArtifact, ResponseArtifactType, ResponseStatus,
    Task, TaskStatus,
};

use crate::adapter::{AdapterInfo, ShellAdapter};
use crate::error::{Result, ShellError};

/// CLI 适配器：把事件同步跑成 Cycleround 闭环。
///
/// `workspace` 是 Cycleround 原地修改的目录；`home` 是 orcha home（store +
/// history 落盘根）。两者都可由调用方指定，便于测试隔离。
pub struct CliAdapter {
    home: PathBuf,
    workspace: PathBuf,
    cycle_config: CycleConfig,
    /// event_id → OrchaResponse 映射，handle_event 写入，get_response 读取。
    responses: Mutex<HashMap<String, OrchaResponse>>,
}

impl CliAdapter {
    /// 以给定 home + workspace 构造，使用报名帖熔断默认值（10/3/60s）。
    pub fn new(home: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            workspace: workspace.into(),
            cycle_config: CycleConfig {
                max_rounds: 10,
                max_retries: 3,
                cool_down: Duration::from_secs(60),
            },
            responses: Mutex::new(HashMap::new()),
        }
    }

    /// 覆盖默认 Cycleround 配置（主要用于测试减小轮次）。
    pub fn with_cycle_config(mut self, config: CycleConfig) -> Self {
        self.cycle_config = config;
        self
    }

    /// home 目录引用。
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// workspace 引用。
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    /// 把 Cycleround 结果转成 OrchaResponse。
    fn outcome_to_response(event_id: &str, task_id: &str, outcome: &CycleOutcome) -> OrchaResponse {
        let (status, content) = match outcome {
            CycleOutcome::Success { rounds, .. } => {
                let content = format!("task {task_id} succeeded in {rounds} round(s)");
                (ResponseStatus::Final, content)
            }
            CycleOutcome::Failed { rounds, reason, .. } => {
                let content = format!("task {task_id} failed after {rounds} round(s): {reason:?}");
                (ResponseStatus::Error, content)
            }
        };
        // artifacts 初始为空；成功路径在 handle_event 里追加 history_file。
        OrchaResponse {
            event_id: event_id.to_string(),
            status,
            content,
            artifacts: Vec::new(),
        }
    }

    /// 把 OrchaResponse 写入内部映射。
    fn store_response(&self, response: OrchaResponse) {
        let event_id = response.event_id.clone();
        let mut map = self.responses.lock().expect("responses mutex poisoned");
        map.insert(event_id, response);
    }
}

impl ShellAdapter for CliAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "cli".into(),
            source: "cli".into(),
            description: "Local CLI adapter — runs Cycleround synchronously".into(),
        }
    }

    fn handle_event(&self, mut event: OrchaEvent) -> Result<String> {
        // M5 只支持 UserPrompt 事件。
        if event.event_type != EventType::UserPrompt {
            return Err(ShellError::Adapter(format!(
                "unsupported event type: {:?} (only UserPrompt is supported in M5)",
                event.event_type
            )));
        }

        let description = event.payload.raw_text.clone();
        if description.trim().is_empty() {
            return Err(ShellError::InvalidBody("empty raw_text".into()));
        }

        // 初始化 store + history。
        let task_store = FileTaskStore::new(&self.home);
        task_store.init()?;
        let history_store = FileHistoryStore::new(&self.home);
        history_store.init()?;

        if !self.workspace.is_dir() {
            return Err(ShellError::Adapter(format!(
                "workspace not a directory: {}",
                self.workspace.display()
            )));
        }

        // 创建 Task（PENDING → RUNNING）。
        let mut task = Task::new(Task::generate_id(), description);
        task_store.insert(&task)?;
        transition(&mut task, TaskStatus::Running)?;
        task_store.update(&task)?;

        // 跑 Cycleround 闭环。
        let cycle = Cycleround::new(self.cycle_config.clone());
        let outcome = cycle.run_with_history(&task, &self.workspace, &history_store);

        // 迁移 Task 状态。
        let final_status = match &outcome {
            CycleOutcome::Success { .. } => TaskStatus::Done,
            CycleOutcome::Failed { .. } => TaskStatus::Failed,
        };
        transition(&mut task, final_status)
            .map_err(|e| ShellError::Adapter(format!("final transition failed: {e}")))?;
        task_store.update(&task)?;

        // 构造 response。
        let event_id = std::mem::take(&mut event.event_id);
        let mut response = Self::outcome_to_response(&event_id, &task.id, &outcome);

        // 成功路径把 history_file 作为 artifact url 暴露，便于外部追溯。
        if response.status == ResponseStatus::Final {
            let history_file = history_store
                .home()
                .join("history")
                .join(format!("{}.jsonl", task.id));
            response.artifacts.push(ResponseArtifact {
                artifact_type: ResponseArtifactType::FileRef,
                url: history_file.to_string_lossy().into_owned(),
            });
        }

        self.store_response(response);
        Ok(event_id)
    }

    fn get_response(&self, event_id: &str) -> Result<OrchaResponse> {
        let map = self.responses.lock().expect("responses mutex poisoned");
        map.get(event_id)
            .cloned()
            .ok_or_else(|| ShellError::EventNotFound(event_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_core::HistoryStore;
    use orcha_sdk::{EventPayload, EventType};
    use std::fs;
    use tempfile::tempdir;

    fn make_event(id: &str, text: &str) -> OrchaEvent {
        OrchaEvent {
            event_id: id.into(),
            source: "cli".into(),
            user_id: "test-user".into(),
            timestamp: 0,
            event_type: EventType::UserPrompt,
            payload: EventPayload {
                raw_text: text.into(),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn cli_adapter_info_has_correct_fields() {
        let adapter = CliAdapter::new("/tmp/orcha-test", "/tmp/ws");
        let info = adapter.info();
        assert_eq!(info.name, "cli");
        assert_eq!(info.source, "cli");
        assert!(!info.description.is_empty());
    }

    #[test]
    fn cli_adapter_rejects_non_user_prompt_event() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        let adapter = CliAdapter::new(home.path(), ws.path());

        let mut event = make_event("EVT-1", "test");
        event.event_type = EventType::SystemSignal;

        let err = adapter.handle_event(event).unwrap_err();
        assert!(matches!(err, ShellError::Adapter(_)));
    }

    #[test]
    fn cli_adapter_rejects_empty_raw_text() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        let adapter = CliAdapter::new(home.path(), ws.path());

        let event = make_event("EVT-1", "   ");
        let err = adapter.handle_event(event).unwrap_err();
        assert!(matches!(err, ShellError::InvalidBody(_)));
    }

    #[test]
    fn cli_adapter_rejects_missing_workspace() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        let missing = ws.path().join("does-not-exist");
        let adapter = CliAdapter::new(home.path(), &missing);

        let event = make_event("EVT-1", "do something");
        let err = adapter.handle_event(event).unwrap_err();
        assert!(matches!(err, ShellError::Adapter(_)));
    }

    #[test]
    fn cli_adapter_handle_event_success_returns_final_response() {
        // 跳过无 Python 环境的场景。
        if orcha_core::find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        // 预置 test.py，让 Cycleround 单轮成功。
        fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();

        let adapter = CliAdapter::new(home.path(), ws.path()).with_cycle_config(CycleConfig {
            max_rounds: 5,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let event_id = adapter
            .handle_event(make_event("EVT-success", "创建 hello.py 输出 hello"))
            .expect("handle_event should succeed");

        assert_eq!(event_id, "EVT-success");

        let response = adapter.get_response("EVT-success").unwrap();
        assert_eq!(response.event_id, "EVT-success");
        assert_eq!(response.status, ResponseStatus::Final);
        assert!(
            response.content.contains("succeeded"),
            "content should mention success: {}",
            response.content
        );
        // 成功路径应有 1 个 FileRef artifact（history_file）。
        assert_eq!(response.artifacts.len(), 1);
        assert_eq!(
            response.artifacts[0].artifact_type,
            ResponseArtifactType::FileRef
        );
        assert!(response.artifacts[0].url.ends_with(".jsonl"));

        // workspace 应产出 hello.py。
        assert!(ws.path().join("hello.py").is_file());
    }

    #[test]
    fn cli_adapter_handle_event_failure_returns_error_response() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        // 预置必失败的 test.py。
        fs::write(ws.path().join("test.py"), "assert False, 'intentional'\n").unwrap();

        let adapter = CliAdapter::new(home.path(), ws.path()).with_cycle_config(CycleConfig {
            max_rounds: 5,
            max_retries: 1,
            cool_down: Duration::from_secs(0),
        });

        let event_id = adapter
            .handle_event(make_event("EVT-fail", "创建 hello.py 输出 hello"))
            .expect("handle_event itself should not error");

        assert_eq!(event_id, "EVT-fail");

        let response = adapter.get_response("EVT-fail").unwrap();
        assert_eq!(response.status, ResponseStatus::Error);
        assert!(
            response.content.contains("failed"),
            "content should mention failure: {}",
            response.content
        );
        // 失败路径不带 artifact。
        assert!(response.artifacts.is_empty());
    }

    #[test]
    fn cli_adapter_get_response_missing_returns_event_not_found() {
        let home = tempdir().unwrap();
        let ws = tempdir().unwrap();
        let adapter = CliAdapter::new(home.path(), ws.path());

        let err = adapter.get_response("EVT-missing").unwrap_err();
        assert!(matches!(err, ShellError::EventNotFound(_)));
    }

    #[test]
    fn cli_adapter_creates_task_and_history_in_home() {
        if orcha_core::find_python().is_none() {
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

        let adapter = CliAdapter::new(home.path(), ws.path()).with_cycle_config(CycleConfig {
            max_rounds: 5,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let _ = adapter
            .handle_event(make_event("EVT-persist", "创建 hello.py 输出 hello"))
            .unwrap();

        // store 应有 1 个 Task，状态 DONE。
        let store = FileTaskStore::new(home.path());
        let tasks = store.list(None).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].status, TaskStatus::Done);

        // history 文件应存在。
        let history = FileHistoryStore::new(home.path());
        let recs = history.list_history(&tasks[0].id).unwrap();
        assert!(!recs.is_empty(), "history 应有记录");
    }
}
