//! M3 Cycleround 调度器：闭环执行 `Plan → Code → Test → Review → Fix`。
//!
//! **当前进度（M3 Commit 5）**：
//! - 类型：[`CycleConfig`] / [`CycleOutcome`] / [`FailureReason`] / [`RoundRecord`]。
//! - 调度：[`Cycleround::run`] 跑 `Observer → Planner → Worker → Tester → Reviewer` 单轮链路。
//!   全部成功才视为该轮成功；任一步失败则跳过后续步骤，并在该轮末尾调用 `Fixer`
//!   尝试产出修复（让下一轮可以重新执行）。
//! - 熔断：触达 `max_rounds` 返回 `Failed(MaxRoundsExceeded)`；触达 `max_retries`
//!   （Fixer 累计调用次数）返回 `Failed(MaxRetriesExceeded)`。**不死循环**。
//! - history 持久化：[`Cycleround::run_with_history`] 把每轮 `RoundRecord` 追加写入
//!   [`HistoryStore`](crate::history::HistoryStore)（默认
//!   [`FileHistoryStore`](crate::FileHistoryStore) 落盘到
//!   `{home}/history/{task_id}.jsonl`，JSONL 格式）。
//!
//! 默认熔断参数对齐报名帖：`max_rounds=10`、`max_retries=3`、`cool_down=60s`。

use std::path::Path;
use std::time::Duration;

use orcha_sdk::{Artifact, Step, StepResult, Task};
use serde::{Deserialize, Serialize};

use crate::history::HistoryStore;
use crate::{Fixer, Observer, Planner, Reviewer, StepContext, SubAgent, Tester, Worker};

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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// 触达 `max_retries`（Fixer 累计调用次数达上限）仍未成功。
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
    tester: Tester,
    reviewer: Reviewer,
    fixer: Fixer,
}

impl Cycleround {
    /// 用指定熔断参数构造。
    pub fn new(config: CycleConfig) -> Self {
        Self {
            config,
            observer: Observer,
            planner: Planner,
            worker: Worker,
            tester: Tester,
            reviewer: Reviewer,
            fixer: Fixer,
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

    /// 跑一轮完整循环（不持久化 history）。
    ///
    /// 等价于 [`Self::run_with_history`] 传入 `None`。详见 [`Self::run_with_history`]。
    pub fn run(&self, task: &Task, workspace: &Path) -> CycleOutcome {
        self.run_inner(task, workspace, None)
    }

    /// 跑一轮完整循环，并把每轮 `RoundRecord` 持久化到 `history` store。
    ///
    /// **当前行为（M3 Commit 5）**：
    /// - 调用前先 `clear_history(task.id)`，确保本次执行的历史独立
    ///   （不与上次运行的残留混合）；清空失败不阻断执行。
    /// - 每轮结束时把 `RoundRecord` 追加写入 history store；
    ///   写入失败仅记录到 stderr，不阻断 Cycleround（history 是辅助追溯，
    ///   不影响主流程的正确性）。
    /// - 返回的 `CycleOutcome.history` 与持久化的 history 内容一致。
    ///
    /// **行为继承自 M3 Commit 4**：
    /// - 每轮依次执行 `Observer → Planner → Worker → Tester → Reviewer`；
    ///   任一步失败则跳过后续步骤（如 Tester 失败则不跑 Reviewer）。
    /// - 5 步全部成功才返回 `Success { rounds: 当前轮 }`。
    /// - 若 Planner 成功但后续步骤失败，则在该轮末尾调用 `Fixer` 尝试产出修复
    ///   （例如 workspace 缺 test.py 时 Fixer 创建之），并计入 `fix_attempts`。
    /// - 若 Planner 失败（任务描述不可解析），Fixer 无法修复，直接进入下一轮。
    /// - 触达 `max_retries`（Fixer 累计调用次数）返回 `Failed(MaxRetriesExceeded)`；
    ///   触达 `max_rounds` 仍未成功则返回 `Failed(MaxRoundsExceeded)`。
    ///
    /// Fixer 的修复策略详见 [`crate::Fixer`] 的文档注释。
    pub fn run_with_history(
        &self,
        task: &Task,
        workspace: &Path,
        history: &dyn HistoryStore,
    ) -> CycleOutcome {
        // 清空旧 history，确保本次执行的历史独立。失败不阻断。
        if let Err(e) = history.clear_history(&task.id) {
            eprintln!(
                "warn: clear_history({}) failed, appending to existing: {}",
                task.id, e
            );
        }
        self.run_inner(task, workspace, Some(history))
    }

    fn run_inner(
        &self,
        task: &Task,
        workspace: &Path,
        history_store: Option<&dyn HistoryStore>,
    ) -> CycleOutcome {
        let mut history: Vec<RoundRecord> = Vec::new();
        let mut artifacts: Vec<Artifact> = Vec::new();
        let mut fix_attempts: u32 = 0;

        for round in 1..=self.config.max_rounds {
            let started_at = chrono::Utc::now();
            let mut steps: Vec<StepResult> = Vec::new();
            let mut round_artifacts: Vec<Artifact> = Vec::new();

            // 1. Observer（始终执行；当前确定性实现不失败）
            let ctx = StepContext::new(workspace, task.clone()).with_priors(&[], &artifacts);
            let obs_out = self.observer.run(&ctx);
            steps.push(obs_out.result.clone());
            round_artifacts.extend(obs_out.artifacts);

            // 2. Planner（始终执行；失败则本轮跳过后续步骤且不调 Fixer）
            let ctx = StepContext::new(workspace, task.clone())
                .with_priors(&steps_as_steps(&steps), &round_artifacts);
            let plan_out = self.planner.run(&ctx);
            steps.push(plan_out.result.clone());
            round_artifacts.extend(plan_out.artifacts);
            let planner_succeeded = plan_out.result.success;

            // 3. Worker（仅当 Planner 成功时执行）
            let mut worker_succeeded = false;
            if planner_succeeded {
                let ctx = StepContext::new(workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let work_out = self.worker.run(&ctx);
                steps.push(work_out.result.clone());
                round_artifacts.extend(work_out.artifacts);
                worker_succeeded = work_out.result.success;
            }

            // 4. Tester（仅当 Worker 成功时执行）
            let mut tester_succeeded = false;
            if worker_succeeded {
                let ctx = StepContext::new(workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let test_out = self.tester.run(&ctx);
                steps.push(test_out.result.clone());
                round_artifacts.extend(test_out.artifacts);
                tester_succeeded = test_out.result.success;
            }

            // 5. Reviewer（仅当 Tester 成功时执行）
            let mut reviewer_succeeded = false;
            if tester_succeeded {
                let ctx = StepContext::new(workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let rev_out = self.reviewer.run(&ctx);
                steps.push(rev_out.result.clone());
                round_artifacts.extend(rev_out.artifacts);
                reviewer_succeeded = rev_out.result.success;
            }

            // 全部成功 → 返回 Success
            if planner_succeeded && worker_succeeded && tester_succeeded && reviewer_succeeded {
                let rec = build_round(round, started_at, steps, round_artifacts.clone());
                persist_round(history_store, &task.id, &rec);
                history.push(rec);
                artifacts.extend(round_artifacts);
                return CycleOutcome::Success {
                    rounds: round,
                    artifacts,
                    history,
                };
            }

            // 6. Fixer：仅当 Planner 成功（任务可解析）时调用。
            //    Planner 失败时 Fixer 也无法解析任务，跳过避免无效调用。
            if planner_succeeded {
                let ctx = StepContext::new(workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let fix_out = self.fixer.run(&ctx);
                steps.push(fix_out.result.clone());
                round_artifacts.extend(fix_out.artifacts);
                fix_attempts += 1;

                if fix_attempts >= self.config.max_retries {
                    let rec = build_round(round, started_at, steps, round_artifacts.clone());
                    persist_round(history_store, &task.id, &rec);
                    history.push(rec);
                    artifacts.extend(round_artifacts);
                    return CycleOutcome::Failed {
                        rounds: round,
                        reason: FailureReason::MaxRetriesExceeded,
                        history,
                    };
                }
            }

            let rec = build_round(round, started_at, steps, round_artifacts.clone());
            persist_round(history_store, &task.id, &rec);
            history.push(rec);
            artifacts.extend(round_artifacts);
        }

        CycleOutcome::Failed {
            rounds: self.config.max_rounds,
            reason: FailureReason::MaxRoundsExceeded,
            history,
        }
    }
}

/// 构造一轮的 `RoundRecord`（不写入任何 store，仅返回值）。
pub(crate) fn build_round(
    round: u32,
    started_at: chrono::DateTime<chrono::Utc>,
    steps: Vec<StepResult>,
    artifacts: Vec<Artifact>,
) -> RoundRecord {
    RoundRecord {
        round,
        started_at,
        finished_at: chrono::Utc::now(),
        steps,
        artifacts,
        // M3 确定性实现无 LLM，token 消耗恒为 0；LLM 接入后由调用方回填。
        tokens_used: 0,
    }
}

/// 把一轮记录写入 history store（若 `Some`）。写入失败仅打 stderr，不阻断主流程。
pub(crate) fn persist_round(
    history_store: Option<&dyn HistoryStore>,
    task_id: &str,
    rec: &RoundRecord,
) {
    if let Some(hs) = history_store {
        if let Err(e) = hs.append_round(task_id, rec) {
            eprintln!(
                "warn: append_round({}, round={}) failed: {}",
                task_id, rec.round, e
            );
        }
    }
}

/// 把 `Vec<StepResult>` 视为 `&[Step]` 用于 StepContext.prior_steps。
///
/// M3 当前实现：`StepResult` 不含足够信息重构 `Step`（缺 name / agent 字段），
/// 因此这里返回空 `Vec`。Sub-Agent 的确定性实现不依赖 `prior_steps`，
/// 仅依赖 `prior_artifacts` 与 `task`；LLM 接入后会真正构造 Step 列表。
fn steps_as_steps(_steps: &[StepResult]) -> Vec<Step> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sub_agents::find_python;

    /// 测试辅助：作为测试的第一条语句调用。若环境无 Python 解释器则提前 return
    /// （避免在无 Python 的机器上 Tester 执行失败导致本该 Success 的测试变成 Failed）。
    /// 调用处写成 `if find_python().is_none() { eprintln!("skipping: ..."); return; }`，
    /// 保留内联形式是因为 Rust 测试函数的 return 类型是 `()`，无法用 helper 统一 return。
    fn _require_python_guard_note() {}

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
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        // 预置 test.py：assert hello.py 内容为 'hello'。
        // 这样 Worker 写完 hello.py 后 Tester 跑 python3 test.py 才会通过。
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
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
                // 5 步全成功：observer / planner / worker / tester / reviewer。
                assert_eq!(
                    history[0].steps.len(),
                    5,
                    "成功轮应记录 5 个步骤，实际: {:?}",
                    history[0]
                        .steps
                        .iter()
                        .map(|s| &s.step_id)
                        .collect::<Vec<_>>()
                );
                for (i, step_id) in [
                    "S-observer",
                    "S-planner",
                    "S-worker",
                    "S-tester",
                    "S-reviewer",
                ]
                .iter()
                .enumerate()
                {
                    assert_eq!(
                        history[0].steps[i].step_id, *step_id,
                        "步骤 {} 的 id 应为 {}",
                        i, step_id
                    );
                    assert!(
                        history[0].steps[i].success,
                        "步骤 {} 应成功: {}",
                        step_id, history[0].steps[i].summary
                    );
                }
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
    fn cycleround_succeeds_after_fixer_creates_test_framework() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 没有 test.py / Cargo.toml / pytest.ini：第一轮 Tester 会因「未检测到测试框架」失败。
        // Fixer 应在第一轮末尾创建 test.py，第二轮 Tester 通过、Reviewer 通过、任务成功。
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-3".into(), "创建 hello.py 输出 hello".into());
        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 2,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success {
                rounds,
                history,
                artifacts,
            } => {
                assert_eq!(rounds, 2, "应在第二轮（Fixer 创建 test.py 后）成功");
                assert!(!artifacts.is_empty(), "应累积产出 artifacts");
                assert_eq!(history.len(), 2, "history 应有两条 round 记录");
                // 第一轮：Observer / Planner / Worker / Tester(fail) / Fixer = 5 步
                assert_eq!(
                    history[0].steps.len(),
                    5,
                    "第一轮应记录 5 步（含 Fixer），实际: {:?}",
                    history[0]
                        .steps
                        .iter()
                        .map(|s| &s.step_id)
                        .collect::<Vec<_>>()
                );
                assert!(history[0].steps[0].success, "Observer 应成功");
                assert!(history[0].steps[1].success, "Planner 应成功");
                assert!(history[0].steps[2].success, "Worker 应成功");
                assert!(
                    !history[0].steps[3].success,
                    "Tester 应失败（无测试框架）: {}",
                    history[0].steps[3].summary
                );
                assert_eq!(history[0].steps[3].step_id, "S-tester");
                assert_eq!(history[0].steps[4].step_id, "S-fixer");
                assert!(
                    history[0].steps[4].success,
                    "Fixer 应成功创建 test.py: {}",
                    history[0].steps[4].summary
                );
                // 第二轮：5 步全成功
                assert_eq!(
                    history[1].steps.len(),
                    5,
                    "第二轮应记录 5 步（无 Fixer），实际: {:?}",
                    history[1]
                        .steps
                        .iter()
                        .map(|s| &s.step_id)
                        .collect::<Vec<_>>()
                );
                for (i, step_id) in [
                    "S-observer",
                    "S-planner",
                    "S-worker",
                    "S-tester",
                    "S-reviewer",
                ]
                .iter()
                .enumerate()
                {
                    assert_eq!(history[1].steps[i].step_id, *step_id);
                    assert!(
                        history[1].steps[i].success,
                        "第二轮步骤 {step_id} 应成功: {}",
                        history[1].steps[i].summary
                    );
                }
                // 验证 test.py 被 Fixer 创建
                assert!(
                    ws.path().join("test.py").is_file(),
                    "Fixer 应在 workspace 创建 test.py"
                );
                // hello.py 应由 Worker 写入（两轮都写了）
                let hello = ws.path().join("hello.py");
                assert!(hello.is_file());
                assert_eq!(std::fs::read_to_string(&hello).unwrap(), "hello\n");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn cycleround_fails_with_max_retries_exceeded_when_fixer_cannot_fix() {
        // workspace 已有 test.py 但断言错误；Worker 写出正确的 hello.py，
        // 但 Tester 跑 test.py 仍会失败，Fixer 也不会改既有 test.py（避免作弊）。
        // 每轮 Tester 与 Fixer 都失败，触达 max_retries 后返回 MaxRetriesExceeded。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'wrong'\n",
        )
        .unwrap();
        let task = Task::new("T-4".into(), "创建 hello.py 输出 hello".into());
        // max_retries=2 让测试更快触达熔断。
        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 10,
            max_retries: 2,
            cool_down: Duration::from_secs(0),
        });

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(rounds, 2, "应在第二轮触达 max_retries");
                assert_eq!(reason, FailureReason::MaxRetriesExceeded);
                assert_eq!(history.len(), 2);
                for rec in &history {
                    // 每轮：Observer / Planner / Worker / Tester(fail) / Fixer(fail) = 5 步
                    assert_eq!(
                        rec.steps.len(),
                        5,
                        "每轮应有 5 步（含失败的 Fixer），实际: {:?}",
                        rec.steps.iter().map(|s| &s.step_id).collect::<Vec<_>>()
                    );
                    assert!(rec.steps[0].success, "Observer 应成功");
                    assert!(rec.steps[1].success, "Planner 应成功");
                    assert!(rec.steps[2].success, "Worker 应成功");
                    assert!(!rec.steps[3].success, "Tester 应失败（断言错误）");
                    assert_eq!(rec.steps[3].step_id, "S-tester");
                    assert!(
                        !rec.steps[4].success,
                        "Fixer 应失败（test.py 已存在）：{}",
                        rec.steps[4].summary
                    );
                    assert_eq!(rec.steps[4].step_id, "S-fixer");
                    assert!(rec.steps[4].summary.contains("已有测试框架"));
                }
                // test.py 内容应未被 Fixer 改写。
                let test_content = std::fs::read_to_string(ws.path().join("test.py")).unwrap();
                assert!(
                    test_content.contains("wrong"),
                    "test.py 不应被 Fixer 改写: {test_content}"
                );
            }
            other => panic!("expected Failed(MaxRetriesExceeded), got {other:?}"),
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

    // ============================================================
    // history 持久化 (M3 Commit 5)
    // ============================================================

    #[test]
    fn run_with_history_persists_rounds_for_successful_run() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 成功路径：第一轮 5 步全成功，应写入 1 条 RoundRecord。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-hist-1".into(), "创建 hello.py 输出 hello".into());

        let history_dir = tempfile::tempdir().unwrap();
        let hs = crate::FileHistoryStore::new(history_dir.path());
        hs.init().unwrap();

        let cycle = Cycleround::with_defaults();
        let outcome = cycle.run_with_history(&task, ws.path(), &hs);

        match outcome {
            CycleOutcome::Success {
                rounds, history, ..
            } => {
                assert_eq!(rounds, 1);
                assert_eq!(history.len(), 1, "in-memory history 应有 1 条");
                // 持久化的 history 应与 in-memory 一致。
                let persisted = hs.list_history("T-hist-1").unwrap();
                assert_eq!(persisted.len(), 1, "应持久化 1 条 RoundRecord");
                assert_eq!(persisted[0].round, 1);
                assert_eq!(persisted[0].steps.len(), 5, "应记录 5 步");
                // 验证 RoundRecord 含耗时 / token / 产物引用（M3 验收项）。
                assert!(
                    persisted[0].duration().num_milliseconds() >= 0,
                    "应有非负耗时"
                );
                assert_eq!(persisted[0].tokens_used, 0, "M3 确定性实现 token=0");
                assert!(!persisted[0].artifacts.is_empty(), "应含产物引用");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn run_with_history_persists_failed_rounds_with_fixer() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 失败路径：Tester 失败 → Fixer 创建 test.py → 第二轮成功。
        // 应持久化 2 条 RoundRecord，第一条含 Fixer 步骤。
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-hist-2".into(), "创建 hello.py 输出 hello".into());

        let history_dir = tempfile::tempdir().unwrap();
        let hs = crate::FileHistoryStore::new(history_dir.path());
        hs.init().unwrap();

        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 2,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });
        let outcome = cycle.run_with_history(&task, ws.path(), &hs);

        match outcome {
            CycleOutcome::Success {
                rounds, history, ..
            } => {
                assert_eq!(rounds, 2, "应在第二轮成功");
                assert_eq!(history.len(), 2);
                // 持久化应与 in-memory 一致。
                let persisted = hs.list_history("T-hist-2").unwrap();
                assert_eq!(persisted.len(), 2, "应持久化 2 条");
                // 第一轮应含 Fixer 步骤（5 步：Observer/Planner/Worker/Tester/Fixer）。
                assert_eq!(persisted[0].round, 1);
                assert_eq!(persisted[0].steps.len(), 5);
                assert_eq!(persisted[0].steps[4].step_id, "S-fixer");
                // 第二轮应 5 步全成功（无 Fixer）。
                assert_eq!(persisted[1].round, 2);
                assert_eq!(persisted[1].steps.len(), 5);
                assert!(persisted[1].steps.iter().all(|s| s.success));
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn run_with_history_clears_old_history_on_rerun() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 第二次 run_with_history 应清空第一次的 history，不混合。
        let ws1 = tempfile::tempdir().unwrap();
        std::fs::write(
            ws1.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-hist-3".into(), "创建 hello.py 输出 hello".into());

        let history_dir = tempfile::tempdir().unwrap();
        let hs = crate::FileHistoryStore::new(history_dir.path());
        hs.init().unwrap();

        let cycle = Cycleround::with_defaults();
        // 第一次跑：写入 1 条 history。
        let _ = cycle.run_with_history(&task, ws1.path(), &hs);
        assert_eq!(hs.list_history("T-hist-3").unwrap().len(), 1);

        // 第二次跑同一 task：应清空旧 history 后重新写入 1 条。
        let ws2 = tempfile::tempdir().unwrap();
        std::fs::write(
            ws2.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let _ = cycle.run_with_history(&task, ws2.path(), &hs);
        let persisted = hs.list_history("T-hist-3").unwrap();
        assert_eq!(
            persisted.len(),
            1,
            "第二次跑应清空旧 history，不应累积为 2 条"
        );
        assert_eq!(persisted[0].round, 1, "新 history 的 round 应重新从 1 开始");
    }

    #[test]
    fn run_with_history_persists_max_retries_exceeded_failure() {
        // 失败路径：Fixer 无法修复，触达 max_retries。
        // 应持久化 max_retries 条 RoundRecord，每条含失败的 Fixer。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'wrong'\n",
        )
        .unwrap();
        let task = Task::new("T-hist-4".into(), "创建 hello.py 输出 hello".into());

        let history_dir = tempfile::tempdir().unwrap();
        let hs = crate::FileHistoryStore::new(history_dir.path());
        hs.init().unwrap();

        let cycle = Cycleround::new(CycleConfig {
            max_rounds: 10,
            max_retries: 2,
            cool_down: Duration::from_secs(0),
        });
        let outcome = cycle.run_with_history(&task, ws.path(), &hs);

        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(rounds, 2);
                assert_eq!(reason, FailureReason::MaxRetriesExceeded);
                assert_eq!(history.len(), 2);
                // 持久化应与 in-memory 一致。
                let persisted = hs.list_history("T-hist-4").unwrap();
                assert_eq!(persisted.len(), 2, "应持久化 2 条失败 round");
                for rec in &persisted {
                    assert_eq!(rec.steps.len(), 5, "每轮 5 步（含失败 Fixer）");
                    assert!(!rec.steps[4].success, "Fixer 应失败");
                }
            }
            other => panic!("expected Failed(MaxRetriesExceeded), got {other:?}"),
        }
    }

    #[test]
    fn run_without_history_does_not_create_files() {
        // 普通 run()（不带 history store）不应在磁盘上创建任何 history 文件。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-no-hist".into(), "创建 hello.py 输出 hello".into());

        // 用 ws 自己的目录作为 home（不应被创建 history/ 子目录）。
        let cycle = Cycleround::with_defaults();
        let _ = cycle.run(&task, ws.path());
        assert!(
            !ws.path().join("history").exists(),
            "run() 不带 history store 时不应创建 history/ 目录"
        );
    }
}
