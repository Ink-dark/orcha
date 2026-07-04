//! Task 持久化仓储。
//!
//! [`TaskStore`] 是抽象 trait，预留 SQLite / Redis / Postgres 等多种后端。
//! M1 默认实现 [`FileTaskStore`]：每个 Task 序列化为一个 JSON 文件，
//! 落盘到 `{home}/store/{task_id}.json`。零 C 依赖，跨平台无忧。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use orcha_sdk::{Task, TaskStatus};

/// Task 仓储抽象。所有后端实现此 trait，CLI 与 Core 只依赖它。
pub trait TaskStore {
    /// 写入新 Task；若 id 已存在应返回错误。
    fn insert(&self, task: &Task) -> Result<()>;

    /// 按 id 读取单个 Task；不存在返回 `Ok(None)`。
    fn get(&self, id: &str) -> Result<Option<Task>>;

    /// 列出全部 Task，可选按状态过滤。
    fn list(&self, filter: Option<TaskStatus>) -> Result<Vec<Task>>;

    /// 更新已存在的 Task（主要用于状态迁移）。
    fn update(&self, task: &Task) -> Result<()>;
}

/// 基于 JSON 文件的 Task 仓储。
///
/// 布局：
/// - `{home}/store/{task_id}.json` —— 单个 Task 的序列化形式
///
/// `home` 通常由 `ORCHA_HOME` 环境变量或 CLI `--home` 决定。
pub struct FileTaskStore {
    home: PathBuf,
}

impl FileTaskStore {
    /// 以给定 home 目录构造。`init()` 必须先被调用以创建目录结构。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// 创建 home/store 目录结构，幂等。对应 `orcha init`。
    pub fn init(&self) -> Result<PathBuf> {
        let store_dir = self.store_dir();
        fs::create_dir_all(&store_dir)
            .with_context(|| format!("failed to create store dir: {}", store_dir.display()))?;
        Ok(store_dir)
    }

    fn store_dir(&self) -> PathBuf {
        self.home.join("store")
    }

    fn task_file(&self, id: &str) -> PathBuf {
        // id 形如 `T-{uuid}`，作为文件名是安全的（无路径分隔符）。
        self.store_dir().join(format!("{id}.json"))
    }
}

impl TaskStore for FileTaskStore {
    fn insert(&self, task: &Task) -> Result<()> {
        let path = self.task_file(&task.id);
        if path.exists() {
            anyhow::bail!("task already exists: {}", task.id);
        }
        write_task(&path, task)
    }

    fn get(&self, id: &str) -> Result<Option<Task>> {
        let path = self.task_file(id);
        if !path.exists() {
            return Ok(None);
        }
        let data = fs::read_to_string(&path)
            .with_context(|| format!("failed to read task file: {}", path.display()))?;
        let task: Task = serde_json::from_str(&data)
            .with_context(|| format!("failed to parse task file: {}", path.display()))?;
        Ok(Some(task))
    }

    fn list(&self, filter: Option<TaskStatus>) -> Result<Vec<Task>> {
        let store_dir = self.store_dir();
        if !store_dir.exists() {
            return Ok(Vec::new());
        }
        let mut tasks = Vec::new();
        for entry in fs::read_dir(&store_dir)
            .with_context(|| format!("failed to read store dir: {}", store_dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let data = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            let task: Task = serde_json::from_str(&data)
                .with_context(|| format!("failed to parse {}", path.display()))?;
            if matches_filter(task.status, filter) {
                tasks.push(task);
            }
        }
        // 按 created_at 升序，保证输出稳定。
        tasks.sort_by_key(|t| t.created_at);
        Ok(tasks)
    }

    fn update(&self, task: &Task) -> Result<()> {
        let path = self.task_file(&task.id);
        if !path.exists() {
            anyhow::bail!("task not found: {}", task.id);
        }
        write_task(&path, task)
    }
}

fn matches_filter(status: TaskStatus, filter: Option<TaskStatus>) -> bool {
    match filter {
        None => true,
        Some(f) => status == f,
    }
}

fn write_task(path: &Path, task: &Task) -> Result<()> {
    let data = serde_json::to_string_pretty(task)?;
    // 先写临时文件再原子重命名，避免半写。
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &data).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_store() -> (FileTaskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTaskStore::new(dir.path());
        store.init().unwrap();
        (store, dir)
    }

    #[test]
    fn insert_then_get_round_trips() {
        let (store, _dir) = fresh_store();
        let task = Task::new(Task::generate_id(), "hello".into());
        store.insert(&task).unwrap();
        let got = store.get(&task.id).unwrap().expect("task should exist");
        assert_eq!(got.id, task.id);
        assert_eq!(got.description, "hello");
        assert_eq!(got.status, TaskStatus::Pending);
    }

    #[test]
    fn get_missing_returns_none() {
        let (store, _dir) = fresh_store();
        assert!(store.get("T-missing").unwrap().is_none());
    }

    #[test]
    fn insert_duplicate_id_errors() {
        let (store, _dir) = fresh_store();
        let task = Task::new("T-dup".into(), "x".into());
        store.insert(&task).unwrap();
        let err = store.insert(&task).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn list_returns_all_sorted_by_created_at() {
        let (store, _dir) = fresh_store();
        let t1 = Task::new("T-1".into(), "a".into());
        std::thread::sleep(std::time::Duration::from_millis(2));
        let t2 = Task::new("T-2".into(), "b".into());
        store.insert(&t1).unwrap();
        store.insert(&t2).unwrap();
        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "T-1");
        assert_eq!(all[1].id, "T-2");
    }

    #[test]
    fn list_filters_by_status() {
        let (store, _dir) = fresh_store();
        let mut t1 = Task::new("T-1".into(), "a".into());
        t1.status = TaskStatus::Running;
        let t2 = Task::new("T-2".into(), "b".into());
        store.insert(&t1).unwrap();
        store.insert(&t2).unwrap();
        let running = store.list(Some(TaskStatus::Running)).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, "T-1");
        let pending = store.list(Some(TaskStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "T-2");
    }

    #[test]
    fn update_changes_status() {
        let (store, _dir) = fresh_store();
        let mut task = Task::new("T-1".into(), "a".into());
        store.insert(&task).unwrap();
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        store.update(&task).unwrap();
        let got = store.get("T-1").unwrap().unwrap();
        assert_eq!(got.status, TaskStatus::Running);
    }

    #[test]
    fn update_missing_errors() {
        let (store, _dir) = fresh_store();
        let task = Task::new("T-missing".into(), "a".into());
        let err = store.update(&task).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn list_on_uninitialized_store_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileTaskStore::new(dir.path());
        // 不调用 init()，list 应优雅返回空而非 panic。
        assert!(store.list(None).unwrap().is_empty());
    }

    #[test]
    fn persistence_survives_new_store_instance() {
        // 模拟"重启进程"：drop 旧 store，用同 home 重建，数据应在。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = FileTaskStore::new(&path);
            store.init().unwrap();
            let task = Task::new("T-persist".into(), "survive".into());
            store.insert(&task).unwrap();
        }
        let store = FileTaskStore::new(&path);
        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "T-persist");
    }
}
