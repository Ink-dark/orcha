//! Task 执行历史记录的持久化抽象。
//!
//! [`HistoryStore`] 是与 [`crate::store::TaskStore`] 平行的 trait，
//! 专门负责 Cycleround 每轮 `RoundRecord` 的追加与读取。
//! M3 默认实现 [`FileHistoryStore`]：每个 Task 一份 JSONL 文件，
//! 每行一个 `RoundRecord`，便于增量追加与流式读取。
//!
//! 布局：
//! - `{home}/history/{task_id}.jsonl` —— 单个 Task 的全部 round 记录
//!
//! 设计取舍：
//! - JSONL（每行一个 JSON 对象）而非单个 JSON 数组：追加写入只需 `append`，
//!   不必读取 / 反序列化 / 重写整个文件，符合 Cycleround 「每轮追加」语义。
//! - 每行末尾带 `\n`，最后一行也带：与 `git apply` 等工具的 trailing newline
//!   约定一致，避免某些编辑器 / 工具的「No newline at end of file」警告。
//! - 写入失败不阻断 Cycleround 执行（详见 [`crate::Cycleround::run_with_history`]）。
//!
//! [`RoundRecord`] 含 `started_at` / `finished_at`（耗时）、`tokens_used`（token）、
//! `artifacts`（产物引用），满足 ROADMAP M3 验收项「每一轮 round 在 history 中
//! 可追溯（含耗时、token、产物引用）」。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::RoundRecord;

/// Task 执行历史持久化抽象。所有后端实现此 trait。
///
/// 与 [`crate::store::TaskStore`] 的关系：
/// - `TaskStore` 存 Task 的状态机（PENDING / RUNNING / ...）。
/// - `HistoryStore` 存 Cycleround 每轮的执行记录（RoundRecord 序列）。
/// - 二者共用同一 `home` 目录但子目录不同（`store/` vs `history/`）。
pub trait HistoryStore: Send + Sync {
    /// 追加一轮执行记录。
    /// 调用方负责保证 `task_id` 存在（FileHistoryStore 不校验）。
    fn append_round(&self, task_id: &str, record: &RoundRecord) -> Result<()>;

    /// 读取某 Task 的全部历史记录，按 round 升序。
    /// 不存在时返回空 `Vec`（不视为错误）。
    fn list_history(&self, task_id: &str) -> Result<Vec<RoundRecord>>;

    /// 清空某 Task 的历史记录。不存在时返回 `Ok(())`。
    fn clear_history(&self, task_id: &str) -> Result<()>;
}

/// 基于 JSONL 文件的 history 仓储。
///
/// 每个 Task 一份 `{home}/history/{task_id}.jsonl` 文件，每行一个 `RoundRecord`。
pub struct FileHistoryStore {
    home: PathBuf,
}

impl FileHistoryStore {
    /// 以给定 home 目录构造。`init()` 必须先被调用以创建目录结构。
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// 创建 `{home}/history` 目录，幂等。对应 `orcha init`。
    pub fn init(&self) -> Result<PathBuf> {
        let dir = self.history_dir();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create history dir: {}", dir.display()))?;
        Ok(dir)
    }

    /// 当前 home 引用（不可变）。
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn history_dir(&self) -> PathBuf {
        self.home.join("history")
    }

    fn history_file(&self, task_id: &str) -> PathBuf {
        // task_id 形如 `T-{uuid}`，作为文件名是安全的。
        // 即便含特殊字符，文件系统也只把它当作普通文件名。
        self.history_dir().join(format!("{task_id}.jsonl"))
    }
}

impl HistoryStore for FileHistoryStore {
    fn append_round(&self, task_id: &str, record: &RoundRecord) -> Result<()> {
        let path = self.history_file(task_id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create history parent dir: {}", parent.display())
            })?;
        }
        // JSONL：每行一个 JSON 对象 + 换行。
        let mut line = serde_json::to_string(record)
            .with_context(|| format!("failed to serialize RoundRecord for task {task_id}"))?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open history file: {}", path.display()))?;
        file.write_all(line.as_bytes())
            .with_context(|| format!("failed to write history file: {}", path.display()))?;
        // 立即 flush，确保崩溃前已落盘。
        file.flush()
            .with_context(|| format!("failed to flush history file: {}", path.display()))?;
        Ok(())
    }

    fn list_history(&self, task_id: &str) -> Result<Vec<RoundRecord>> {
        let path = self.history_file(task_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let data = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read history file: {}", path.display()))?;
        let mut records = Vec::new();
        for (i, line) in data.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: RoundRecord = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse history line {} of {}",
                    i + 1,
                    path.display()
                )
            })?;
            records.push(record);
        }
        Ok(records)
    }

    fn clear_history(&self, task_id: &str) -> Result<()> {
        let path = self.history_file(task_id);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("failed to remove history file: {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_sdk::{Artifact, ArtifactType, StepResult};

    fn fresh_store() -> (FileHistoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = FileHistoryStore::new(dir.path());
        store.init().unwrap();
        (store, dir)
    }

    fn sample_round(round: u32) -> RoundRecord {
        let now = chrono::Utc::now();
        RoundRecord {
            round,
            started_at: now,
            finished_at: now,
            steps: vec![StepResult {
                step_id: format!("S-{round}"),
                success: true,
                started_at: now,
                finished_at: now,
                summary: format!("round {round} ok"),
                artifact_ids: vec!["ART-1".into()],
            }],
            artifacts: vec![Artifact {
                artifact_id: "ART-1".into(),
                artifact_type: ArtifactType::Report,
                commit_sha: None,
                patch: None,
                url: None,
            }],
            tokens_used: round * 100,
        }
    }

    #[test]
    fn append_then_list_round_trips() {
        let (store, _dir) = fresh_store();
        let r1 = sample_round(1);
        let r2 = sample_round(2);
        store.append_round("T-1", &r1).unwrap();
        store.append_round("T-1", &r2).unwrap();

        let got = store.list_history("T-1").unwrap();
        assert_eq!(got.len(), 2, "应读回 2 条记录");
        assert_eq!(got[0].round, 1, "应按写入顺序（升序）");
        assert_eq!(got[1].round, 2);
        assert_eq!(got[1].tokens_used, 200, "tokens_used 应保留");
        assert_eq!(got[1].steps.len(), 1, "steps 应保留");
        assert_eq!(got[1].steps[0].step_id, "S-2");
        assert_eq!(got[1].artifacts.len(), 1, "artifacts 应保留");
        assert_eq!(got[1].artifacts[0].artifact_type, ArtifactType::Report);
    }

    #[test]
    fn list_history_missing_returns_empty_vec() {
        let (store, _dir) = fresh_store();
        let got = store.list_history("T-missing").unwrap();
        assert!(got.is_empty(), "不存在的 task 应返回空 Vec");
    }

    #[test]
    fn clear_history_removes_file() {
        let (store, _dir) = fresh_store();
        store.append_round("T-1", &sample_round(1)).unwrap();
        assert!(store.list_history("T-1").unwrap().len() == 1);

        store.clear_history("T-1").unwrap();
        assert!(
            store.list_history("T-1").unwrap().is_empty(),
            "清空后应为空"
        );
    }

    #[test]
    fn clear_history_missing_is_ok() {
        let (store, _dir) = fresh_store();
        // 不存在的 task 清空不应报错。
        store.clear_history("T-missing").unwrap();
    }

    #[test]
    fn append_creates_history_dir_if_missing() {
        // 不调用 init()，直接 append 应自动创建目录。
        let dir = tempfile::tempdir().unwrap();
        let store = FileHistoryStore::new(dir.path());
        store.append_round("T-1", &sample_round(1)).unwrap();
        assert_eq!(store.list_history("T-1").unwrap().len(), 1);
    }

    #[test]
    fn history_file_is_jsonl_format() {
        // 直接读取文件内容，验证是 JSONL（每行一个 JSON 对象）。
        let (store, dir) = fresh_store();
        store.append_round("T-1", &sample_round(1)).unwrap();
        store.append_round("T-1", &sample_round(2)).unwrap();

        let path = dir.path().join("history").join("T-1.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "应有 2 行（每行一个 RoundRecord）");
        // 每行都应是合法 JSON。
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).expect("每行应是合法 JSON");
        }
        // 第一行应含 "round":1
        assert!(lines[0].contains("\"round\":1"));
        assert!(lines[1].contains("\"round\":2"));
    }

    #[test]
    fn history_isolates_per_task() {
        let (store, _dir) = fresh_store();
        store.append_round("T-1", &sample_round(1)).unwrap();
        store.append_round("T-2", &sample_round(1)).unwrap();

        assert_eq!(store.list_history("T-1").unwrap().len(), 1);
        assert_eq!(store.list_history("T-2").unwrap().len(), 1);
        // T-1 不应混入 T-2 的记录。
        let t1 = store.list_history("T-1").unwrap();
        assert_eq!(t1[0].steps[0].step_id, "S-1");
    }

    #[test]
    fn persistence_survives_new_store_instance() {
        // 模拟「重启进程」：drop 旧 store，用同 home 重建，数据应在。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        {
            let store = FileHistoryStore::new(&path);
            store.append_round("T-persist", &sample_round(1)).unwrap();
        }
        let store = FileHistoryStore::new(&path);
        let got = store.list_history("T-persist").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].round, 1);
    }

    #[test]
    fn append_round_preserves_artifacts_and_tokens() {
        // 验证 RoundRecord 全字段持久化（耗时 / token / 产物引用）。
        let (store, _dir) = fresh_store();
        let mut r = sample_round(1);
        r.tokens_used = 4096;
        r.artifacts.push(Artifact {
            artifact_id: "ART-2".into(),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: Some("abc123".into()),
            patch: Some("diff --git a/x b/x\n".into()),
            url: None,
        });
        store.append_round("T-1", &r).unwrap();

        let got = store.list_history("T-1").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].tokens_used, 4096, "tokens_used 应保留");
        assert_eq!(got[0].artifacts.len(), 2, "artifacts 应保留全部");
        assert_eq!(got[0].artifacts[1].artifact_type, ArtifactType::CodeDiff);
        assert_eq!(
            got[0].artifacts[1].commit_sha.as_deref(),
            Some("abc123"),
            "commit_sha 应保留"
        );
        assert!(
            got[0].artifacts[1]
                .patch
                .as_ref()
                .unwrap()
                .contains("diff --git"),
            "patch 内容应保留"
        );
    }
}
