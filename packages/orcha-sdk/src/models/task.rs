use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 任务状态机取值，对应 README §4.1。
///
/// 合法迁移由 `orcha-core` 的状态机模块强制；这里只声明枚举本身，
/// 使得 sdk 可以独立序列化/反序列化而不引入迁移逻辑。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TaskStatus {
    Pending,
    Running,
    Blocked,
    Done,
    Failed,
}

impl TaskStatus {
    /// 全部合法状态，按生命周期顺序排列。
    pub fn all() -> &'static [TaskStatus] {
        &[
            TaskStatus::Pending,
            TaskStatus::Running,
            TaskStatus::Blocked,
            TaskStatus::Done,
            TaskStatus::Failed,
        ]
    }
}

impl std::fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 与 serde 的 SCREAMING_SNAKE_CASE 保持一致，便于日志/CLI 输出。
        let s = match self {
            TaskStatus::Pending => "PENDING",
            TaskStatus::Running => "RUNNING",
            TaskStatus::Blocked => "BLOCKED",
            TaskStatus::Done => "DONE",
            TaskStatus::Failed => "FAILED",
        };
        f.write_str(s)
    }
}

/// Orcha 系统的最小执行单元（README §2.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Task {
    /// 形如 `T-{uuid}`，由创建方分配。
    pub id: String,
    /// 人类可读的自然语言描述，例如 "fix the bug in auth.py"。
    pub description: String,
    /// 当前生命周期状态。
    pub status: TaskStatus,
    /// 任务创建时间（UTC）。
    pub created_at: DateTime<Utc>,
    /// 最近一次状态变更时间（UTC）。
    pub updated_at: DateTime<Utc>,
}

impl Task {
    /// 以给定 id 与描述构造一个 `Pending` 任务，时间戳取当前 UTC。
    pub fn new(id: String, description: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            description,
            status: TaskStatus::Pending,
            created_at: now,
            updated_at: now,
        }
    }

    /// 生成下一个 `T-{uuid}` 形式的 id。
    pub fn generate_id() -> String {
        format!("T-{}", Uuid::new_v4())
    }
}
