use thiserror::Error;

use orcha_sdk::TaskStatus;

/// Orcha Core 错误类型集合。
#[derive(Debug, Error)]
pub enum CoreError {
    /// 状态机收到非法迁移。
    #[error("invalid transition: {from} -> {to} (task {task_id})")]
    InvalidTransition {
        task_id: String,
        from: TaskStatus,
        to: TaskStatus,
    },

    /// 找不到指定任务。
    #[error("task not found: {0}")]
    TaskNotFound(String),

    /// 任务已达终态，无法继续迁移。
    #[error("task {0} is terminal ({1}); no further transitions allowed")]
    TerminalTask(String, TaskStatus),

    /// 乐观锁版本号冲突（M4 持久化引入）。
    ///
    /// 调用方持有的 `expected_version` 与落盘的 `actual_version` 不一致，
    /// 说明在调用方读取后、写入前，Task 已被另一个进程/线程修改。
    /// 调用方应重新 `get` 拿最新版本后重试。
    #[error(
        "version conflict on task {task_id}: expected {expected_version}, actual {actual_version}"
    )]
    VersionConflict {
        task_id: String,
        expected_version: u64,
        actual_version: u64,
    },
}
