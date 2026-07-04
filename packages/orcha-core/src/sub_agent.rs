//! Sub-Agent 抽象与执行上下文。
//!
//! 对应 README §2.3 的 OPC (Observer-Planner-Coder) 模式。
//! M2 只实现单步链路，不闭环；M3 才接 Cycleround 调度器与真实 LLM。

use std::path::PathBuf;

use orcha_sdk::{Artifact, Step, StepResult, StepStatus, Task};

/// 单步执行上下文。Sub-Agent 通过它读写任务相关数据。
#[derive(Debug, Clone)]
pub struct StepContext {
    /// 任务工作区根目录（隔离的 tempdir），Sub-Agent 在此读写文件。
    pub workspace: PathBuf,
    /// 当前任务。
    pub task: Task,
    /// 前序步骤产出的 artifacts（按时间序）。
    pub prior_artifacts: Vec<Artifact>,
    /// 前序步骤的 DAG 节点定义。
    pub prior_steps: Vec<Step>,
}

impl StepContext {
    /// 构造一个初始上下文（无前序产物）。
    pub fn new(workspace: impl Into<PathBuf>, task: Task) -> Self {
        Self {
            workspace: workspace.into(),
            task,
            prior_artifacts: Vec::new(),
            prior_steps: Vec::new(),
        }
    }

    /// 追加一个前序步骤及其产物。
    pub fn with_prior(mut self, step: Step, artifacts: Vec<Artifact>) -> Self {
        self.prior_steps.push(step);
        self.prior_artifacts.extend(artifacts);
        self
    }
}

/// Sub-Agent 的统一接口。
///
/// 每个 Sub-Agent（Observer / Planner / Worker / Tester / Reviewer / Fixer）
/// 实现此 trait，由调度器按 DAG 顺序调用。
pub trait SubAgent {
    /// 该 Agent 的角色名，例如 `observer` / `planner` / `worker`。
    fn name(&self) -> &'static str;

    /// 执行单步。返回 [`StepResult`] 与本步产出的 artifacts。
    ///
    /// 实现方应当：
    /// - 在 `ctx.workspace` 内读写文件，**不得**访问宿主其他路径；
    /// - 把可应用的产物（diff、文件等）包装为 `Artifact` 返回；
    /// - 失败时填 `success = false`，由调度器决定是否进入 Fixer。
    fn run(&self, ctx: &StepContext) -> StepOutput;
}

/// 一次 Sub-Agent 执行的输出。
#[derive(Debug, Clone)]
pub struct StepOutput {
    /// 与本步对应的 StepResult（含耗时、状态等）。
    pub result: StepResult,
    /// 本步产出的 artifacts（可能为空）。
    pub artifacts: Vec<Artifact>,
}

impl StepOutput {
    /// 构造一个成功输出，自动设置 `success = true` 与 `StepStatus::Succeeded`。
    pub fn success(step_id: impl Into<String>, summary: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            result: StepResult {
                step_id: step_id.into(),
                success: true,
                started_at: now,
                finished_at: now,
                summary: summary.into(),
                artifact_ids: Vec::new(),
            },
            artifacts: Vec::new(),
        }
        .with_artifact_ids_filled()
    }

    /// 构造一个失败输出。
    pub fn failure(step_id: impl Into<String>, summary: impl Into<String>) -> Self {
        let now = chrono::Utc::now();
        Self {
            result: StepResult {
                step_id: step_id.into(),
                success: false,
                started_at: now,
                finished_at: now,
                summary: summary.into(),
                artifact_ids: Vec::new(),
            },
            artifacts: Vec::new(),
        }
        .with_artifact_ids_filled()
    }

    /// 附加 artifacts，并回填它们的 id 到 result.artifact_ids。
    pub fn with_artifacts(mut self, artifacts: Vec<Artifact>) -> Self {
        self.artifacts = artifacts.clone();
        self.result.artifact_ids = artifacts.into_iter().map(|a| a.artifact_id).collect();
        self
    }

    fn with_artifact_ids_filled(mut self) -> Self {
        self.result.artifact_ids = self
            .artifacts
            .iter()
            .map(|a| a.artifact_id.clone())
            .collect();
        self
    }
}

/// 把一个 Step 标记为 Running 状态（用于调度器记录进度）。
pub fn mark_running(step: &mut Step) {
    step.status = StepStatus::Running;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_context_new_has_empty_priors() {
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new("/tmp/ws", task.clone());
        assert_eq!(ctx.workspace, PathBuf::from("/tmp/ws"));
        assert_eq!(ctx.task.id, "T-1");
        assert!(ctx.prior_artifacts.is_empty());
        assert!(ctx.prior_steps.is_empty());
    }

    #[test]
    fn step_context_with_prior_accumulates() {
        let task = Task::new("T-1".into(), "x".into());
        let step = Step {
            id: "S-1".into(),
            name: "observer".into(),
            agent: "observer".into(),
            status: StepStatus::Succeeded,
        };
        let art = Artifact {
            artifact_id: "ART-1".into(),
            artifact_type: orcha_sdk::ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };
        let ctx = StepContext::new("/tmp/ws", task).with_prior(step.clone(), vec![art.clone()]);
        assert_eq!(ctx.prior_steps.len(), 1);
        assert_eq!(ctx.prior_steps[0].id, "S-1");
        assert_eq!(ctx.prior_artifacts.len(), 1);
        assert_eq!(ctx.prior_artifacts[0].artifact_id, "ART-1");
    }

    #[test]
    fn step_output_success_marks_succeeded_via_artifact_ids() {
        let out = StepOutput::success("S-1", "ok");
        assert!(out.result.success);
        assert_eq!(out.result.step_id, "S-1");
        assert_eq!(out.result.summary, "ok");
        assert!(out.result.artifact_ids.is_empty());
    }

    #[test]
    fn step_output_with_artifacts_backfills_ids() {
        let arts = vec![
            Artifact {
                artifact_id: "ART-1".into(),
                artifact_type: orcha_sdk::ArtifactType::CodeDiff,
                commit_sha: None,
                patch: Some("diff".into()),
                url: None,
            },
            Artifact {
                artifact_id: "ART-2".into(),
                artifact_type: orcha_sdk::ArtifactType::FileRef,
                commit_sha: None,
                patch: None,
                url: Some("file:///x".into()),
            },
        ];
        let out = StepOutput::success("S-1", "ok").with_artifacts(arts);
        assert_eq!(out.artifacts.len(), 2);
        assert_eq!(out.result.artifact_ids, vec!["ART-1", "ART-2"]);
    }

    #[test]
    fn step_output_failure_marks_failed() {
        let out = StepOutput::failure("S-1", "boom");
        assert!(!out.result.success);
        assert_eq!(out.result.summary, "boom");
        assert!(out.result.artifact_ids.is_empty());
    }

    #[test]
    fn mark_running_sets_status() {
        let mut step = Step {
            id: "S-1".into(),
            name: "coder".into(),
            agent: "worker".into(),
            status: StepStatus::Pending,
        };
        mark_running(&mut step);
        assert_eq!(step.status, StepStatus::Running);
    }
}
