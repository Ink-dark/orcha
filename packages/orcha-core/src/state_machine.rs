use orcha_sdk::{Task, TaskStatus};

use crate::error::CoreError;

/// 判断状态迁移是否合法。
///
/// 对应 README §4.1：
/// ```text
/// PENDING -> RUNNING -> (BLOCKED <-> RUNNING) -> DONE
///                        |
///                        └-> FAILED
/// ```
///
/// `DONE` 与 `FAILED` 为终态，无任何出边。
pub fn is_legal_transition(from: TaskStatus, to: TaskStatus) -> bool {
    use TaskStatus::*;
    matches!(
        (from, to),
        (Pending, Running)
            | (Running, Blocked)
            | (Running, Done)
            | (Running, Failed)
            | (Blocked, Running)
    )
}

/// 对一个任务执行状态迁移。
///
/// 成功时更新 `task.status` 与 `task.updated_at`；失败时返回
/// [`CoreError::InvalidTransition`] 或 [`CoreError::TerminalTask`]。
pub fn transition(task: &mut Task, to: TaskStatus) -> Result<(), CoreError> {
    if task.status == to {
        // 同状态 no-op 视为合法，方便幂等调用。
        return Ok(());
    }
    if is_terminal(task.status) {
        return Err(CoreError::TerminalTask(task.id.clone(), task.status));
    }
    if !is_legal_transition(task.status, to) {
        return Err(CoreError::InvalidTransition {
            task_id: task.id.clone(),
            from: task.status,
            to,
        });
    }
    task.status = to;
    task.updated_at = chrono::Utc::now();
    Ok(())
}

/// 状态是否为终态（无出边）。
pub fn is_terminal(status: TaskStatus) -> bool {
    matches!(status, TaskStatus::Done | TaskStatus::Failed)
}
