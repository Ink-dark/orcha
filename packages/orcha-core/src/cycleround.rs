//! M3 Cycleround 调度器：闭环执行 `Plan → Code → Test → Review → Fix`。
//!
//! **本 commit 范围（M3 Commit 1）**：搭骨架与熔断参数。
//! - 类型：[`CycleConfig`] / [`CycleOutcome`] / [`FailureReason`] / [`RoundRecord`]。
//! - 调度：[`Cycleround::run`] 跑 `Observer → Planner → Worker` 单轮链路（复用 M2 实现）。
//!   Worker 成功即视为该轮成功；任一步失败或解析不出计划则进入下一轮。
//! - 熔断：触达 `max_rounds` 返回 `Failed(MaxRoundsExceeded)`，**不死循环**。
//!
//! 后续 commits 会依次接入 Tester / Reviewer / Fixer 与 history 持久化，
//! `run()` 的循环体也会随之扩展。
//!
//! 默认熔断参数对齐报名帖：`max_rounds=10`、`max_retries=3`、`cool_down=60s`。

use std::path::Path;
use std::time::Duration;

use orcha_sdk::{Artifact, Step, StepResult, Task};

use crate::{Observer, Planner, StepContext, SubAgent, Worker};

/// 熔断参数。
///
/// 字段含义：
/// - `max_rounds`：单个 Task 最多执行的循环轮次。
/// - `max_retries`：单步最多重试次数（M3 后续 commit 用于 Fixer 重试上限）。
/// - `cool_down`：触达速率限制后的冷却时间（M2/M3 确定性实现不真睡，仅作为配置存在）。
#[derive(Debug, Clone)]
pub struct CycleConfig {
    pub max_rounds: u32,
    pub max_retries: u32,
    pub cool_down: Duration,
}

impl Default for CycleConfig {
    fn default() -> Self {
        Self {
            max_rounds: 10,
            max_retries: 3,
            cool_down: Duration::from_secs(60),
        }
    }
}

/// 单轮执行的记录，写入 history 以便追溯。
#[derive(Debug, Clone)]
pub struct RoundRecord {
    /// 第几轮（从 1 开始）。
    pub round: u32,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    /// 本轮各 Sub-Agent 的 StepResult。
    pub steps: Vec<StepResult>,
    /// 本轮产出的 artifacts。
    pub artifacts: Vec<Artifact>,
    /// 本轮消耗的 token 数（M3 确定性实现为 0，留待 LLM 接入后填充）。
    pub tokens_used: u32,
}

impl RoundRecord {
    /// 本轮耗时。
    pub fn duration(&self) -> chrono::Duration {
        self.finished_at.signed_duration_since(self.started_at)
    }
}

/// Cycleround 终态。
#[derive(Debug, Clone)]
pub enum CycleOutcome {
    /// 任务在某轮成功完成。
    Success {
        /// 实际消耗的轮次（从 1 开始）。
        rounds: u32,
        /// 全部累积的 artifacts。
        artifacts: Vec<Artifact>,
        /// 每轮的执行记录。
        history: Vec<RoundRecord>,
    },
    /// 任务失败。
    Failed {
        /// 已执行的轮次。
        rounds: u32,
        /// 失败原因。
        reason: FailureReason,
        /// 每轮的执行记录（即使失败也保留，便于事后追溯）。
        history: Vec<RoundRecord>,
    },
}

/// 失败原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureReason {
    /// 触达 `max_rounds` 仍未成功。
    MaxRoundsExceeded,
    /// 触达 `max_retries`（M3 后续 commit 接入 Fixer 后启用）。
    MaxRetriesExceeded,
}

/// Cycleround 调度器。
///
/// 持有各 Sub-Agent 实例与熔断配置；同一 Cycleround 可被多次调用以跑多个 Task。
pub struct Cycleround {
    config: CycleConfig,
    observer: Observer,
    planner: Planner,
    worker: Worker,
}

impl Cycleround {
    /// 用指定熔断参数构造。
    pub fn new(config: CycleConfig) -> Self {
        Self {
            config,
            observer: Observer,
            planner: Planner,
            worker: Worker,
        }
    }

    /// 使用报名帖默认熔断参数（10/3/60s）。
    pub fn with_defaults() -> Self {
        Self::new(CycleConfig::default())
    }

    /// 当前熔断配置（不可变引用）。
    pub fn config(&self) -> &CycleConfig {
        &self.config
    }

    /// 跑一轮完整循环。
    ///
    /// **M3 Commit 1 行为**：
    /// - 每轮执行 `Observer → Planner → Worker`；
    /// - 任一步失败（例如 Planner 解析不出计划）则跳过该轮后续步骤，进入下一轮；
    /// - Worker 成功则返回 `Success { rounds: 当前轮 }`；
    /// - 触达 `max_rounds` 仍未成功则返回 `Failed(MaxRoundsExceeded)`。
    ///
    /// 后续 commit 会扩展为 `Observer → Planner → Worker → Tester → Reviewer`，
    /// 失败时进入 `Fixer` 重新规划，并把 `max_retries` 纳入判定。
    pub fn run(&self, task: &Task, workspace: &Path) -> CycleOutcome {
        let mut history: Vec<RoundRecord> = Vec::new();
        let mut artifacts: Vec<Artifact> = Vec::new();

        for round in 1..=self.config.max_rounds {
            let started_at = chrono::Utc::now();
            let mut steps: Vec<StepResult> = Vec::new();
            let mut round_artifacts: Vec<Artifact> = Vec::new();

            // 1. Observer
            let ctx = StepContext::new(workspace, task.clone()).with_priors(&[], &artifacts);
            let obs_out = self.observer.run(&ctx);
            steps.push(obs_out.result.clone());
            round_artifacts.extend(obs_out.artifacts);

            // 2. Planner
            let ctx = StepContext::new(workspace, task.clone())
                .with_priors(&steps_as_steps(&steps), &round_artifacts);
            let plan_out = self.planner.run(&ctx);
            steps.push(plan_out.result.clone());
            round_artifacts.extend(plan_out.artifacts);

            if !plan_out.result.success {
                // Planner 解析失败：本轮直接结束，进入下一轮。
                push_round(&mut history, round, started_at, steps, round_artifacts);
                continue;
            }

            // 3. Worker
            let ctx = StepContext::new(workspace, task.clone())
                .with_priors(&steps_as_steps(&steps), &round_artifacts);
            let work_out = self.worker.run(&ctx);
            steps.push(work_out.result.clone());
            round_artifacts.extend(work_out.artifacts);

            push_round(
                &mut history,
                round,
                started_at,
                steps,
                round_artifacts.clone(),
            );
            artifacts.extend(round_artifacts);

            if work_out.result.success {
                return CycleOutcome::Success {
                    rounds: round,
                    artifacts,
                    history,
                };
            }
        }

        CycleOutcome::Failed {
            rounds: self.config.max_rounds,
            reason: FailureReason::MaxRoundsExceeded,
            history,
        }
    }
}

fn push_round(
    history: &mut Vec<RoundRecord>,
    round: u32,
    started_at: chrono::DateTime<chrono::Utc>,
    steps: Vec<StepResult>,
    artifacts: Vec<Artifact>,
) {
    history.push(RoundRecord {
        round,
        started_at,
        finished_at: chrono::Utc::now(),
        steps,
        artifacts,
        tokens_used: 0,
    });
}

/// 把 `Vec<StepResult>` 视为 `&[Step]` 用于 StepContext.prior_steps。
///
/// M3 Commit 1 暂不维护独立的 Step 列表；后续 commit 接入 history 持久化时会
/// 真正构造 Step 列表。这里返回空切片以避免类型不匹配。
fn steps_as_steps(_steps: &[StepResult]) -> Vec<Step> {
    // StepResult 当前不含足够信息重构 Step（id/name/agent/status），
    // 后续 commit 会用真正的 Step 列表替换此桩。
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_sdk::TaskStatus;

    #[test]
    fn cycle_config_default_matches_registration_post() {
        let cfg = CycleConfig::default();
        assert_eq!(cfg.max_rounds, 10, "报名帖约定 max_rounds=10");
        assert_eq!(cfg.max_retries, 3, "报名帖约定 max_retries=3");
        assert_eq!(
            cfg.cool_down,
            Duration::from_secs(60),
            "报名帖约定 cool_down=60s"
        );
    }

    #[test]
    fn cycleround_succeeds_on_first_round_for_hello_task() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        // 单轮即可成功，max_rounds=2 足够。
        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 2,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success {
                rounds,
                artifacts,
                history,
            } => {
                assert_eq!(rounds, 1, "应在第一轮就成功");
                assert!(!artifacts.is_empty(), "应产出 artifacts");
                assert_eq!(history.len(), 1, "history 应有一条 round 记录");
                assert_eq!(history[0].round, 1);
                // Worker 应真实产出 hello.py（含 trailing newline）。
                let hello = ws.path().join("hello.py");
                assert!(hello.is_file());
                assert_eq!(std::fs::read_to_string(&hello).unwrap(), "hello\n");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn cycleround_fails_with_max_rounds_exceeded_on_unparseable_task() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-2".into(), "this is not a parseable plan".into());
        // 用小 max_rounds 加快测试。
        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 3,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(rounds, 3, "应执行满 3 轮");
                assert_eq!(reason, FailureReason::MaxRoundsExceeded);
                assert_eq!(history.len(), 3, "应记录 3 轮 history");
                for (i, rec) in history.iter().enumerate() {
                    assert_eq!(rec.round as usize, i + 1, "round 编号应从 1 递增");
                    // Planner 每轮都解析失败，所以只有 observer + planner 两步。
                    assert_eq!(rec.steps.len(), 2, "Planner 失败后本轮应只有 2 步");
                    assert!(rec.steps[1].summary.contains("解析任务失败"));
                }
            }
            other => panic!("expected Failed(MaxRoundsExceeded), got {other:?}"),
        }
    }

    #[test]
    fn cycleround_default_constructor_uses_registration_post_defaults() {
        let cycle = Cycleround::with_defaults();
        assert_eq!(cycle.config().max_rounds, 10);
        assert_eq!(cycle.config().max_retries, 3);
    }

    #[test]
    fn round_record_duration_is_non_negative() {
        let started = chrono::Utc::now();
        let finished = started + chrono::Duration::milliseconds(50);
        let rec = RoundRecord {
            round: 1,
            started_at: started,
            finished_at: finished,
            steps: Vec::new(),
            artifacts: Vec::new(),
            tokens_used: 0,
        };
        assert!(rec.duration().num_milliseconds() >= 0);
    }

    // 仅为消除 unused 警告：TaskStatus 在 StepResult 字段中使用，这里引用一下枚举值。
    #[test]
    fn task_status_pending_exists() {
        let _ = TaskStatus::Pending;
    }
}
