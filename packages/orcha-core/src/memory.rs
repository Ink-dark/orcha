//! Task 对话历史持久化（Memory 层）。
//!
//! 与 [`crate::history::HistoryStore`]（执行 round 记录）平行，
//! [`MemoryStore`] 专门存 LLM 对话消息，让 Cycleround 多轮之间 LLM 能引用前序失败原因。
//!
//! 布局：`{home}/memory/{task_id}.jsonl`，每行一条 [`MemoryEntry`]。
//!
//! 设计取舍：
//! - 与 history 分目录：history 给人看（耗时/token），memory 给 LLM 看（对话）
//! - 每条带 `agent` 字段标记来源（planner/worker/reviewer/fixer），
//!   LLM prompt 构造时可选择性注入
//! - 同样 JSONL + 立即 flush，崩溃前已落盘

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// 一条对话记忆条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEntry {
    /// 1 开始的轮次。
    pub round: u32,
    /// 来源 agent：`planner` / `worker` / `reviewer` / `fixer`。
    pub agent: String,
    /// 角色：`system` / `user` / `assistant`。
    pub role: String,
    /// 文本内容。
    pub content: String,
}

/// Task 对话历史持久化抽象。
pub trait MemoryStore {
    /// 追加一条对话。
    fn append(&self, task_id: &str, entry: &MemoryEntry) -> Result<()>;
    /// 读全部对话，按追加顺序（即时间序）。
    fn list(&self, task_id: &str) -> Result<Vec<MemoryEntry>>;
    /// 清空某 Task 的对话历史。
    fn clear(&self, task_id: &str) -> Result<()>;
}

/// 基于 JSONL 文件的 memory 仓储。
pub struct FileMemoryStore {
    home: PathBuf,
}

impl FileMemoryStore {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    pub fn init(&self) -> Result<PathBuf> {
        let dir = self.memory_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create memory dir: {}", dir.display()))?;
        Ok(dir)
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    fn memory_dir(&self) -> PathBuf {
        self.home.join("memory")
    }

    fn memory_file(&self, task_id: &str) -> PathBuf {
        self.memory_dir().join(format!("{task_id}.jsonl"))
    }
}

impl MemoryStore for FileMemoryStore {
    fn append(&self, task_id: &str, entry: &MemoryEntry) -> Result<()> {
        let path = self.memory_file(task_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create memory parent dir: {}", parent.display())
            })?;
        }
        let mut line = serde_json::to_string(entry)
            .with_context(|| format!("failed to serialize MemoryEntry for task {task_id}"))?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open memory file: {}", path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("failed to write memory file: {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush memory file: {}", path.display()))?;
        Ok(())
    }

    fn list(&self, task_id: &str) -> Result<Vec<MemoryEntry>> {
        let path = self.memory_file(task_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read memory file: {}", path.display()))?;
        let mut out = Vec::new();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let e: MemoryEntry = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse memory line {} of {}",
                    i + 1,
                    path.display()
                )
            })?;
            out.push(e);
        }
        Ok(out)
    }

    fn clear(&self, task_id: &str) -> Result<()> {
        let path = self.memory_file(task_id);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove memory file: {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (FileMemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMemoryStore::new(dir.path());
        store.init().unwrap();
        (store, dir)
    }

    fn entry(round: u32, agent: &str, role: &str, content: &str) -> MemoryEntry {
        MemoryEntry {
            round,
            agent: agent.into(),
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn append_then_list_round_trips() {
        let (store, _dir) = fresh();
        store
            .append("T-1", &entry(1, "planner", "user", "plan A"))
            .unwrap();
        store
            .append("T-1", &entry(1, "planner", "assistant", "ok"))
            .unwrap();
        store
            .append("T-1", &entry(2, "worker", "user", "fix bug"))
            .unwrap();

        let got = store.list("T-1").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].round, 1);
        assert_eq!(got[0].agent, "planner");
        assert_eq!(got[1].content, "ok");
        assert_eq!(got[2].round, 2);
    }

    #[test]
    fn list_missing_returns_empty() {
        let (store, _dir) = fresh();
        assert!(store.list("T-missing").unwrap().is_empty());
    }

    #[test]
    fn clear_removes_file() {
        let (store, _dir) = fresh();
        store.append("T-1", &entry(1, "p", "user", "x")).unwrap();
        assert_eq!(store.list("T-1").unwrap().len(), 1);
        store.clear("T-1").unwrap();
        assert!(store.list("T-1").unwrap().is_empty());
    }

    #[test]
    fn per_task_isolation() {
        let (store, _dir) = fresh();
        store.append("T-1", &entry(1, "p", "user", "x")).unwrap();
        store.append("T-2", &entry(1, "p", "user", "y")).unwrap();
        assert_eq!(store.list("T-1").unwrap().len(), 1);
        assert_eq!(store.list("T-2").unwrap().len(), 1);
    }

    #[test]
    fn append_creates_dir_if_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = FileMemoryStore::new(dir.path());
        // 不调 init()
        store.append("T-1", &entry(1, "p", "user", "x")).unwrap();
        assert_eq!(store.list("T-1").unwrap().len(), 1);
    }

    #[test]
    fn persists_across_instance() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let s = FileMemoryStore::new(&path);
            s.append("T-p", &entry(1, "p", "user", "x")).unwrap();
        }
        let s = FileMemoryStore::new(&path);
        assert_eq!(s.list("T-p").unwrap().len(), 1);
    }
}
