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
}
