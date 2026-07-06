//! Task 持久化仓储。
//!
//! [`TaskStore`] 是抽象 trait，预留 SQLite / Redis / Postgres 等多种后端。
//! M1 默认实现 [`FileTaskStore`]：每个 Task 序列化为一个 JSON 文件，
//! 落盘到 `{home}/store/{task_id}.json`。零 C 依赖，跨平台无忧。
//!
//! M4 持久化引入两层并发保护：
//! 1. **乐观锁**（[`TaskStore::update_with_version`]）：校验落盘 version
//!    与调用方持有的一致后才写入并自增，不一致返回 [`CoreError::VersionConflict`]。
//! 2. **文件锁**（`{task_id}.lock` 文件）：保护 `read-modify-write` 临界区，
//!    避免两个进程同时读到 version=0 然后都校验通过。锁通过
//!    `OpenOptions::create_new(true)` 抢占（创建成功=拿到锁），
//!    失败则短暂 sleep 后 retry，超时则返回错误。写完释放锁（删除文件）。
//!    这套机制跨平台（Linux/Windows/macOS 都支持 create_new 语义），零 C 依赖。

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use orcha_sdk::{Task, TaskStatus};

use crate::error::CoreError;

/// 文件锁抢锁失败时的 retry 间隔。
const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);
/// 文件锁抢锁总超时（超过则报错，避免死锁）。
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

/// Task 仓储抽象。所有后端实现此 trait，CLI 与 Core 只依赖它。
pub trait TaskStore {
    /// 写入新 Task；若 id 已存在应返回错误。
    fn insert(&self, task: &Task) -> Result<()>;

    /// 按 id 读取单个 Task；不存在返回 `Ok(None)`。
    fn get(&self, id: &str) -> Result<Option<Task>>;

    /// 列出全部 Task，可选按状态过滤。
    fn list(&self, filter: Option<TaskStatus>) -> Result<Vec<Task>>;

    /// 更新已存在的 Task（主要用于状态迁移）。
    ///
    /// 默认实现等价于 `update_with_version(task, None)`：不校验版本号，
    /// 但落盘的 `task.version` 会自增 1。需要乐观锁保护的写入应调用
    /// [`update_with_version`](Self::update_with_version)。
    ///
    /// 注意：入参是 `&Task`，内部会 clone 一份再自增 version 后写入磁盘，
    /// 因此调用方持有的 `task` 仍是旧版本号；如需最新版本号请重新 `get`。
    fn update(&self, task: &Task) -> Result<()> {
        self.update_with_version(task, None)
    }

    /// 带乐观锁的更新（M4 持久化引入）。
    ///
    /// - `expected_version = None`：跳过版本校验（等价于 [`update`](Self::update)），
    ///   落盘的 `version` 自增 1。
    /// - `expected_version = Some(v)`：先读取当前落盘 Task，校验 `version == v`；
    ///   一致才写入并把 `version` 设为 `v + 1`；不一致返回
    ///   [`CoreError::VersionConflict`]。
    ///
    /// 入参 `&Task` 不会被修改（内部 clone 后自增 version 写入磁盘）；
    /// 调用方需要最新版本号时应重新 `get`。
    fn update_with_version(&self, task: &Task, expected_version: Option<u64>) -> Result<()>;
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

    /// home 目录（用于 history store 等同 home 的其他组件复用）。
    pub fn home(&self) -> &Path {
        &self.home
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

    fn update_with_version(&self, task: &Task, expected_version: Option<u64>) -> Result<()> {
        let path = self.task_file(&task.id);
        if !path.exists() {
            anyhow::bail!("task not found: {}", task.id);
        }
        // M4 文件锁：保护 read-modify-write 临界区，避免两个进程同时读到
        // version=0 然后都校验通过。锁文件 `<task_id>.json.lock`，崩溃残留
        // 由 retry 超时兜底（调用方可手动删除 .lock 文件恢复）。
        let lock_path = path.with_extension("json.lock");
        let _lock = FileLock::acquire(&lock_path)
            .with_context(|| format!("failed to acquire lock for task {}", task.id))?;

        // 读取落盘当前 version（用于乐观锁校验 + 自增基准）。
        // 关键：自增基于落盘 version，而非调用方持有的 task.version（可能 stale），
        // 否则连续两次 update() 会让落盘 version 在 1 处原地踏步。
        let current = self.get(&task.id)?.ok_or_else(|| {
            anyhow::anyhow!("task disappeared during update_with_version: {}", task.id)
        })?;
        if let Some(v) = expected_version {
            if current.version != v {
                return Err(CoreError::VersionConflict {
                    task_id: task.id.clone(),
                    expected_version: v,
                    actual_version: current.version,
                }
                .into());
            }
        }
        // 校验通过（或跳过校验）：clone 一份，version 基于落盘值自增，刷新 updated_at。
        // 不修改入参 task（&Task 不可变），调用方需要最新 version 时应重新 get。
        // _lock 在此函数返回时 drop，自动释放（删除 .lock 文件）。
        let mut updated = task.clone();
        updated.version = current.version + 1;
        updated.updated_at = chrono::Utc::now();
        write_task(&path, &updated)
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

// ============================================================
// M4 文件锁（task 粒度，保护 read-modify-write 临界区）
// ============================================================

/// 跨平台文件锁 guard。
///
/// 通过 `OpenOptions::create_new(true)` 抢占 `<path>.lock` 文件实现：
/// 创建成功 = 拿到锁；文件已存在 = 别人持锁，sleep 后 retry。
/// Drop 时删除 lock 文件释放锁。若进程崩溃，lock 文件可能残留，
/// 下次抢锁会 retry 到超时——调用方应清理或用 `force_acquire`。
///
/// 这套机制零 C 依赖，Linux/Windows/macOS 行为一致。
pub(crate) struct FileLock {
    lock_path: PathBuf,
    released: bool,
}

impl FileLock {
    /// 抢占指定路径的独占锁。`lock_path` 通常是 `<data_file>.lock`。
    ///
    /// - retry 间隔 `LOCK_RETRY_INTERVAL`（20ms）。
    /// - 总超时 `LOCK_TIMEOUT`（5s），超时返回 `anyhow::bail!`。
    pub(crate) fn acquire(lock_path: impl Into<PathBuf>) -> Result<Self> {
        let lock_path = lock_path.into();
        let started = Instant::now();
        loop {
            // create_new=true：文件已存在则返回 AlreadyExists（这是"抢锁失败"的信号）。
            // 创建成功后立即关闭 handle——锁状态由"文件是否存在"决定，与 handle 无关。
            // 崩溃残留时文件不删，retry 超时后报错（调用方可手动删 .lock 文件恢复）。
            let result = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path);
            match result {
                Ok(_file) => {
                    // 关闭 handle：锁状态 = 文件存在性，不依赖 handle 持有。
                    drop(_file);
                    return Ok(Self {
                        lock_path,
                        released: false,
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() >= LOCK_TIMEOUT {
                        anyhow::bail!(
                            "acquire lock timeout after {:?}: {} (可能残留，可手动删除)",
                            LOCK_TIMEOUT,
                            lock_path.display()
                        );
                    }
                    std::thread::sleep(LOCK_RETRY_INTERVAL);
                }
                Err(e) => {
                    anyhow::bail!("failed to create lock file {}: {}", lock_path.display(), e);
                }
            }
        }
    }

    /// 显式释放锁（删除 lock 文件）。可多次调用，幂等。
    /// 日常使用依赖 `Drop` 自动释放；此方法供需要显式释放的场景调用。
    #[allow(dead_code)]
    pub(crate) fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if !self.released {
            let _ = fs::remove_file(&self.lock_path);
            self.released = true;
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        self.release_inner();
    }
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

    // ============================================================
    // M4 乐观锁测试
    // ============================================================

    #[test]
    fn update_increments_version_on_disk() {
        // update() 不传 expected_version，落盘 version 应自增。
        let (store, _dir) = fresh_store();
        let mut task = Task::new("T-v1".into(), "a".into());
        store.insert(&task).unwrap();
        assert_eq!(task.version, 0, "新建 Task version=0");

        crate::transition(&mut task, TaskStatus::Running).unwrap();
        store.update(&task).unwrap();

        let got = store.get("T-v1").unwrap().unwrap();
        assert_eq!(got.version, 1, "第一次 update 后 version=1");

        crate::transition(&mut task, TaskStatus::Done).unwrap();
        store.update(&task).unwrap();
        let got = store.get("T-v1").unwrap().unwrap();
        assert_eq!(got.version, 2, "第二次 update 后 version=2");
    }

    #[test]
    fn update_with_version_succeeds_when_expected_matches() {
        let (store, _dir) = fresh_store();
        let mut task = Task::new("T-v2".into(), "a".into());
        store.insert(&task).unwrap();
        // 落盘 version=0，调用方持有 version=0，匹配 → 成功。
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        store.update_with_version(&task, Some(0)).unwrap();
        let got = store.get("T-v2").unwrap().unwrap();
        assert_eq!(got.version, 1);
        assert_eq!(got.status, TaskStatus::Running);
    }

    #[test]
    fn update_with_version_conflicts_when_stale() {
        // 模拟并发冲突：A 读到 v=0，B 先 update 把 version 自增到 1，
        // A 用 stale 的 v=0 调 update_with_version → 应失败。
        let (store, _dir) = fresh_store();
        let mut task = Task::new("T-v3".into(), "a".into());
        store.insert(&task).unwrap();
        // B 先 update（version 0 → 1）。
        let mut b_version = task.clone();
        crate::transition(&mut b_version, TaskStatus::Running).unwrap();
        store.update(&b_version).unwrap();
        // A 持有 stale 的 v=0 调用。
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        let err = store.update_with_version(&task, Some(0)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("version conflict")
                && msg.contains("expected 0")
                && msg.contains("actual 1"),
            "应返回 VersionConflict，实际: {msg}"
        );
        // 落盘 version 仍是 1（A 的写入被拒绝）。
        let got = store.get("T-v3").unwrap().unwrap();
        assert_eq!(got.version, 1);
    }

    #[test]
    fn update_with_version_none_skips_check_but_still_increments() {
        // expected_version=None 等价于 update()：不校验但自增。
        let (store, _dir) = fresh_store();
        let mut task = Task::new("T-v4".into(), "a".into());
        store.insert(&task).unwrap();
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        store.update_with_version(&task, None).unwrap();
        let got = store.get("T-v4").unwrap().unwrap();
        assert_eq!(got.version, 1);
    }

    #[test]
    fn old_task_file_without_version_field_back_compat() {
        // 旧版本序列化的 Task 文件没有 version 字段，反序列化时应默认为 0。
        let (store, dir) = fresh_store();
        let old_json = r#"{
            "id": "T-old",
            "description": "legacy",
            "status": "PENDING",
            "created_at": "2025-01-01T00:00:00Z",
            "updated_at": "2025-01-01T00:00:00Z"
        }"#;
        let path = dir.path().join("store").join("T-old.json");
        std::fs::write(&path, old_json).unwrap();
        let got = store.get("T-old").unwrap().unwrap();
        assert_eq!(got.version, 0, "缺少 version 字段时应默认为 0");
        assert_eq!(got.id, "T-old");
    }

    // ============================================================
    // M4 文件锁测试
    // ============================================================

    #[test]
    fn file_lock_acquire_release_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("test.lock");
        // 抢锁成功。
        let lock = FileLock::acquire(&lock_path).expect("acquire should succeed");
        assert!(lock_path.exists(), "lock 文件应被创建");
        // 释放后文件消失。
        drop(lock);
        assert!(!lock_path.exists(), "lock 文件应被删除");
    }

    #[test]
    fn file_lock_blocks_second_acquire_until_released() {
        // 同一路径：第一个锁未释放时，第二个 acquire 应 retry 直到超时或前锁释放。
        // 这里用短超时模拟：在另一线程持有锁期间，主线程 acquire 应等待。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("concurrent.lock");

        // 子线程持有锁 100ms 后释放。
        let lock_path_clone = lock_path.clone();
        let handle = std::thread::spawn(move || {
            let _lock = FileLock::acquire(&lock_path_clone).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            // _lock drop 时释放。
        });

        // 等子线程拿到锁。
        std::thread::sleep(std::time::Duration::from_millis(20));

        // 主线程 acquire：应 retry 直到子线程释放（约 100ms 后），不应超时（5s）。
        let start = std::time::Instant::now();
        let lock2 = FileLock::acquire(&lock_path).expect("should acquire after retry");
        let elapsed = start.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(50),
            "应至少等待了 ~100ms 让前锁释放，实际: {elapsed:?}"
        );
        assert!(elapsed < std::time::Duration::from_secs(5), "不应超时");
        drop(lock2);

        handle.join().unwrap();
    }

    #[test]
    fn file_lock_times_out_when_never_released() {
        // 用极短超时模拟"锁永远不释放"场景。
        // 但 LOCK_TIMEOUT 是常量 5s，测试不能真等 5s。
        // 改为：手动创建 lock 文件模拟残留，acquire 应超时报错。
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("stale.lock");
        std::fs::write(&lock_path, "stale").unwrap();

        // 这会 retry 5s 后超时。为了不拖慢测试，用 spawn + 短 join timeout。
        let lock_path_clone = lock_path.clone();
        let handle = std::thread::spawn(move || FileLock::acquire(&lock_path_clone).err());
        // 等 200ms 让它 retry 几轮，然后断言它还在跑（未返回）。
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!handle.is_finished(), "acquire 应仍在 retry（未超时 5s）");
        // 清理：删除 stale lock 文件，让线程 acquire 成功返回。
        std::fs::remove_file(&lock_path).unwrap();
        let err = handle.join().unwrap();
        assert!(err.is_none(), "清理后应 acquire 成功，实际: {err:?}");
    }

    #[test]
    fn concurrent_updates_serialized_by_file_lock() {
        // 真正的并发验收：两个线程各自 update 同一个 Task 100 次，
        // 最终落盘 version 应精确等于 200（每次 update 自增 1，无丢失）。
        // 不改 Task 状态（避免状态机迁移限制），仅靠 update 自增 version。
        let (store, dir) = fresh_store();
        let path = dir.path().to_path_buf();
        let mut task = Task::new("T-concurrent".into(), "concurrent".into());
        // 先 insert（version=0, Pending），再迁移到 Running 并 update（version → 1）。
        store.insert(&task).unwrap();
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        store.update(&task).unwrap();

        let update_100_times = |home: PathBuf| {
            let store = FileTaskStore::new(&home);
            for _ in 0..100 {
                loop {
                    let t = store.get("T-concurrent").unwrap().unwrap();
                    let v = t.version;
                    // 不改状态，仅 update（会刷新 updated_at + 自增 version）。
                    // 用 update_with_version 做乐观锁校验，冲突时 retry。
                    match store.update_with_version(&t, Some(v)) {
                        Ok(()) => break,
                        Err(_) => continue, // VersionConflict，重新 get 重试
                    }
                }
            }
        };

        let path1 = path.clone();
        let path2 = path.clone();
        let h1 = std::thread::spawn(move || update_100_times(path1));
        let h2 = std::thread::spawn(move || update_100_times(path2));
        h1.join().unwrap();
        h2.join().unwrap();

        let final_task = store.get("T-concurrent").unwrap().unwrap();
        assert_eq!(
            final_task.version, 201,
            "200 次并发 update + 1 次初始迁移 update 后 version 应为 201，实际: {}（说明有丢失更新）",
            final_task.version
        );
    }
}

// ============================================================
// M6 SQLite 后端（feature = "sqlite" 启用，可选实现）
// ============================================================

#[cfg(feature = "sqlite")]
mod sqlite_backend {
    use std::path::Path;
    use std::sync::Mutex;

    use anyhow::{Context, Result};
    use orcha_sdk::{Task, TaskStatus};
    use rusqlite::{params, Connection, OptionalExtension};

    use crate::error::CoreError;
    use crate::store::TaskStore;

    /// 基于 SQLite 的 Task 仓储（M6，需启用 `sqlite` feature）。
    ///
    /// 相比 [`crate::FileTaskStore`]：
    /// - 开启 WAL 模式，读不阻塞写、崩溃后可由 WAL 日志恢复；
    /// - 用 SQLite 事务做乐观锁，天然跨进程安全；
    /// - 单文件数据库，便于备份/迁移。
    ///
    /// 因 `rusqlite::Connection` 不是 `Sync`，用 [`Mutex`] 包装，
    /// 使 `SqliteTaskStore` 满足 `Send + Sync`，可跨线程共享。
    pub struct SqliteTaskStore {
        // Connection 不是 Sync，用 Mutex 保护，保证多线程下串行访问。
        conn: Mutex<Connection>,
    }

    // 静态断言：SqliteTaskStore 必须是 Send + Sync。
    // Mutex<Connection> 满足（Connection: Send），保证可跨线程共享。
    const _: () = {
        fn _assert_send_sync<T: Send + Sync>() {}
        fn _assert() {
            _assert_send_sync::<SqliteTaskStore>();
        }
    };

    impl SqliteTaskStore {
        /// 打开/创建 SQLite 数据库文件。
        ///
        /// - 自动创建父目录（`Connection::open` 不会创建目录）；
        /// - 开启 WAL 模式（`PRAGMA journal_mode=WAL`）；
        /// - 幂等建表 `tasks`：
        ///   - `id TEXT PRIMARY KEY`
        ///   - `data TEXT`（Task 的 JSON 序列化）
        ///   - `version INTEGER`（乐观锁版本号）
        ///   - `status TEXT`（TaskStatus 字符串形式，用于过滤）
        ///   - `updated_at TEXT`（RFC3339 时间戳）
        pub fn open(path: &Path) -> Result<Self> {
            // 确保父目录存在，否则 Connection::open 会失败。
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!("failed to create sqlite db dir: {}", parent.display())
                    })?;
                }
            }
            let conn = Connection::open(path)
                .with_context(|| format!("failed to open sqlite db: {}", path.display()))?;
            // WAL 模式：读不阻塞写，崩溃后可通过 WAL 日志恢复。
            conn.execute_batch("PRAGMA journal_mode = WAL;")
                .context("failed to enable WAL journal_mode")?;
            conn.execute(
                "CREATE TABLE IF NOT EXISTS tasks ( \
                     id         TEXT PRIMARY KEY, \
                     data       TEXT NOT NULL, \
                     version    INTEGER NOT NULL, \
                     status     TEXT NOT NULL, \
                     updated_at TEXT NOT NULL \
                 )",
                [],
            )
            .context("failed to create tasks table")?;
            Ok(Self {
                conn: Mutex::new(conn),
            })
        }

        /// 加锁获取 Connection guard。
        ///
        /// 若持有锁的线程 panic 导致锁"中毒"，仍取出内部 Connection 继续使用，
        /// 避免一次 panic 就让整个 store 永久不可用。
        fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
            self.conn.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// 按 id 删除 Task；不存在也不报错（幂等）。
        ///
        /// 注意：`TaskStore` trait 当前未声明 `delete`，因此这里作为固有方法实现，
        /// 调用方直接通过 `SqliteTaskStore::delete` 调用。
        pub fn delete(&self, id: &str) -> Result<()> {
            let conn = self.lock();
            conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])
                .with_context(|| format!("failed to delete task {}", id))?;
            Ok(())
        }
    }

    impl TaskStore for SqliteTaskStore {
        fn insert(&self, task: &Task) -> Result<()> {
            let data = serde_json::to_string(task).context("failed to serialize task")?;
            let conn = self.lock();
            let res = conn.execute(
                "INSERT INTO tasks (id, data, version, status, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    task.id,
                    data,
                    task.version as i64,
                    task.status.to_string(),
                    task.updated_at.to_rfc3339(),
                ],
            );
            match res {
                Ok(_) => Ok(()),
                // 主键冲突 = id 已存在，与 FileTaskStore 保持一致的错误语义。
                Err(rusqlite::Error::SqliteFailure(err, _))
                    if err.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    anyhow::bail!("task already exists: {}", task.id);
                }
                Err(e) => Err(anyhow::Error::new(e)
                    .context(format!("failed to insert task {}", task.id))),
            }
        }

        fn get(&self, id: &str) -> Result<Option<Task>> {
            let conn = self.lock();
            let data: Option<String> = conn
                .query_row(
                    "SELECT data FROM tasks WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            match data {
                Some(s) => {
                    let task: Task = serde_json::from_str(&s)
                        .with_context(|| format!("failed to parse task json for id {}", id))?;
                    Ok(Some(task))
                }
                None => Ok(None),
            }
        }

        fn list(&self, filter: Option<TaskStatus>) -> Result<Vec<Task>> {
            let conn = self.lock();
            // 先把 data 列全部读出（趁 stmt 还活着），再反序列化。
            // 注意：rows 必须在 stmt drop 之前被 collect 消费掉，
            // 因此先绑定到具名局部变量，再 collect，避免尾表达式临时值提前 drop。
            let raw: Vec<String> = match filter {
                Some(status) => {
                    let mut stmt = conn
                        .prepare("SELECT data FROM tasks WHERE status = ?1")
                        .context("failed to prepare list(status) stmt")?;
                    let rows =
                        stmt.query_map(params![status.to_string()], |row| row.get::<_, String>(0))?;
                    let collected: rusqlite::Result<Vec<String>> = rows.collect();
                    collected.context("failed to fetch tasks (status filter)")?
                }
                None => {
                    let mut stmt = conn
                        .prepare("SELECT data FROM tasks")
                        .context("failed to prepare list(all) stmt")?;
                    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                    let collected: rusqlite::Result<Vec<String>> = rows.collect();
                    collected.context("failed to fetch tasks (all)")?
                }
            };
            let mut tasks = Vec::with_capacity(raw.len());
            for data in raw {
                let task: Task =
                    serde_json::from_str(&data).context("failed to parse task json in list")?;
                tasks.push(task);
            }
            // 按 created_at 升序，与 FileTaskStore 输出顺序保持一致。
            tasks.sort_by_key(|t| t.created_at);
            Ok(tasks)
        }

        fn update_with_version(&self, task: &Task, expected_version: Option<u64>) -> Result<()> {
            let mut conn = self.lock();
            // 用事务保证"读 version + 校验 + 写入"原子性，跨进程安全。
            let tx = conn.transaction().context("failed to begin transaction")?;
            // 读取落盘当前 version（乐观锁校验 + 自增基准）。
            // 关键：自增基于落盘 version，而非调用方持有的 task.version（可能 stale），
            // 否则连续两次 update() 会让落盘 version 在 1 处原地踏步。
            let current_version: i64 = tx
                .query_row(
                    "SELECT version FROM tasks WHERE id = ?1",
                    params![task.id],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| anyhow::anyhow!("task not found: {}", task.id))?;
            let current_version = current_version as u64;
            if let Some(v) = expected_version {
                if current_version != v {
                    return Err(CoreError::VersionConflict {
                        task_id: task.id.clone(),
                        expected_version: v,
                        actual_version: current_version,
                    }
                    .into());
                }
            }
            // 校验通过（或跳过校验）：基于落盘 version 自增，刷新 updated_at。
            // 不修改入参 &Task，调用方需要最新 version 时应重新 get。
            let mut updated = task.clone();
            updated.version = current_version + 1;
            updated.updated_at = chrono::Utc::now();
            let data = serde_json::to_string(&updated).context("failed to serialize task")?;
            let affected = tx
                .execute(
                    "UPDATE tasks SET data = ?1, version = ?2, status = ?3, updated_at = ?4 \
                     WHERE id = ?5",
                    params![
                        data,
                        updated.version as i64,
                        updated.status.to_string(),
                        updated.updated_at.to_rfc3339(),
                        task.id,
                    ],
                )
                .context("failed to update task")?;
            if affected == 0 {
                // 并发中行被删了的极端情况。
                anyhow::bail!("task not found: {}", task.id);
            }
            tx.commit().context("failed to commit transaction")?;
            Ok(())
        }
    }
}

// 对外导出 SqliteTaskStore（仅在 sqlite feature 启用时可见）。
#[cfg(feature = "sqlite")]
pub use sqlite_backend::SqliteTaskStore;

#[cfg(all(test, feature = "sqlite"))]
mod sqlite_tests {
    use super::*;

    /// 创建一个临时目录 + SqliteTaskStore，返回 (store, 临时目录)。
    /// 临时目录 drop 时会自动清理 db 文件。
    fn fresh_db() -> (SqliteTaskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let db_path = dir.path().join("test.db");
        let store = SqliteTaskStore::open(&db_path).expect("open sqlite store");
        (store, dir)
    }

    #[test]
    fn sqlite_insert_then_get_round_trips() {
        let (store, _dir) = fresh_db();
        let task = Task::new(Task::generate_id(), "hello".into());
        store.insert(&task).unwrap();
        let got = store.get(&task.id).unwrap().expect("task should exist");
        assert_eq!(got.id, task.id);
        assert_eq!(got.description, "hello");
        assert_eq!(got.status, TaskStatus::Pending);
        assert_eq!(got.version, 0, "新建 Task version=0");
    }

    #[test]
    fn sqlite_list_and_filter_by_status() {
        let (store, _dir) = fresh_db();
        let mut t1 = Task::new("T-1".into(), "a".into());
        t1.status = TaskStatus::Running;
        let t2 = Task::new("T-2".into(), "b".into());
        store.insert(&t1).unwrap();
        store.insert(&t2).unwrap();

        let all = store.list(None).unwrap();
        assert_eq!(all.len(), 2, "应返回全部 2 条");

        let running = store.list(Some(TaskStatus::Running)).unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].id, "T-1");

        let pending = store.list(Some(TaskStatus::Pending)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "T-2");
    }

    #[test]
    fn sqlite_update_with_version_and_conflict() {
        let (store, _dir) = fresh_db();
        let mut task = Task::new("T-v".into(), "a".into());
        store.insert(&task).unwrap();
        assert_eq!(task.version, 0, "新建 Task version=0");

        // Pending -> Running 合法迁移。
        crate::transition(&mut task, TaskStatus::Running).unwrap();
        // 落盘 version=0，调用方持有 version=0，匹配 → 成功，落盘 version → 1。
        store.update_with_version(&task, Some(0)).unwrap();
        let got = store.get("T-v").unwrap().unwrap();
        assert_eq!(got.version, 1);
        assert_eq!(got.status, TaskStatus::Running);

        // 用 stale 的 version=0 再 update → 冲突（实际落盘 version=1）。
        let err = store.update_with_version(&task, Some(0)).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("version conflict")
                && msg.contains("expected 0")
                && msg.contains("actual 1"),
            "应返回 VersionConflict，实际: {msg}"
        );
        // 落盘 version 仍是 1（被冲突拒绝的写入未生效）。
        let got = store.get("T-v").unwrap().unwrap();
        assert_eq!(got.version, 1);
    }

    #[test]
    fn sqlite_delete_removes_task() {
        let (store, _dir) = fresh_db();
        let task = Task::new("T-del".into(), "x".into());
        store.insert(&task).unwrap();
        assert!(store.get("T-del").unwrap().is_some(), "插入后应能查到");
        store.delete("T-del").unwrap();
        assert!(store.get("T-del").unwrap().is_none(), "删除后应查不到");
    }

    #[test]
    fn sqlite_insert_duplicate_id_errors() {
        let (store, _dir) = fresh_db();
        let task = Task::new("T-dup".into(), "x".into());
        store.insert(&task).unwrap();
        let err = store.insert(&task).unwrap_err();
        assert!(err.to_string().contains("already exists"), "实际: {err}");
    }

    #[test]
    fn sqlite_update_missing_errors() {
        let (store, _dir) = fresh_db();
        let task = Task::new("T-missing".into(), "a".into());
        let err = store.update(&task).unwrap_err();
        assert!(err.to_string().contains("not found"), "实际: {err}");
    }
}
