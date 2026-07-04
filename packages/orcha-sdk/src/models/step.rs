use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 单个 Step 的执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StepStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Skipped,
}

/// Sub-Agent 执行链路中的一个步骤。
///
/// 对应 README §2.3 的 OPC 模式中由 Planner 产出的 DAG 节点。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Step {
    /// 步骤 id，建议 `S-<n>` 或 uuid。
    pub id: String,
    /// 步骤名称，例如 `observer`、`planner`、`coder`。
    pub name: String,
    /// 该步骤由哪种 Sub-Agent 承担。
    pub agent: String,
    /// 当前状态。
    pub status: StepStatus,
}

/// 一个步骤执行后的结构化结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StepResult {
    /// 对应的 `Step.id`。
    pub step_id: String,
    /// 是否成功。
    pub success: bool,
    /// 开始时间（UTC）。
    pub started_at: DateTime<Utc>,
    /// 结束时间（UTC）。
    pub finished_at: DateTime<Utc>,
    /// 执行产出的人类可读摘要。
    pub summary: String,
    /// 关联的 artifact id 列表。
    pub artifact_ids: Vec<String>,
}

impl StepResult {
    /// 计算执行耗时。
    pub fn duration(&self) -> chrono::Duration {
        self.finished_at - self.started_at
    }
}
