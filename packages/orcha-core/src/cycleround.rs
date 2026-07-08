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

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use orcha_sdk::{Artifact, Step, StepResult, Task};
use serde::{Deserialize, Serialize};

use crate::history::HistoryStore;
use crate::{
    Fixer, Observer, Planner, Reviewer, StepContext, StepOutput, SubAgent, Tester, Worker,
};

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
///
/// M6 起新增 `Serialize, Deserialize` 派生，便于 `RoundEvent::TaskCompleted`
/// 携带 outcome 通过事件流（Gateway / SSE / WebSocket）下发给前端。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
///
/// M6 起新增 `Serialize, Deserialize` 派生，配合 [`CycleOutcome`] 的派生；
/// 同时新增 [`FailureReason::Panic`]，专用于 `run_streaming` 在线程内捕获 panic 后
/// 通过 `TaskCompleted` 事件回告调用方「调度崩溃」语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureReason {
    /// 触达 `max_rounds` 仍未成功。
    MaxRoundsExceeded,
    /// 触达 `max_retries`（Fixer 累计调用次数达上限）仍未成功。
    MaxRetriesExceeded,
    /// `run_streaming` 在独立线程内捕获到 panic，调度异常终止。
    ///
    /// 仅事件流路径会出现此原因；同步 [`Cycleround::run`] /
    /// [`Cycleround::run_with_history`] 不捕获 panic，直接传播给调用方。
    Panic,
    /// 管理员通过 `/stop` 命令手动取消任务。
    Cancelled,
}

/// M6 事件流：`run_streaming` 在每步前后推送的事件，供 Gateway 实时消费进度。
///
/// - `AgentStarted` / `AgentFinished`：成对出现，包裹单个 Sub-Agent 调用；
///   `AgentFinished.success` 与对应 [`StepResult`] 的 `success` 字段一致，
///   `message` 取自其 `summary`，便于前端直接展示。
/// - `RoundFinished`：每轮结束时推送一次，携带完整的 [`RoundRecord`]，
///   供前端按轮聚合 / 重放历史。
/// - `TaskCompleted`：调度终态，仅在最后推送一次（含 panic 兜底路径）。
///
/// `Serialize` 派生使其可直接经 SSE / WebSocket 序列化下发；
/// `Clone` 便于测试中收集事件做断言。
#[derive(Debug, Clone, Serialize)]
pub enum RoundEvent {
    /// 单个 Sub-Agent 即将执行。
    AgentStarted {
        round: u32,
        /// Sub-Agent 角色名（取自 [`SubAgent::name`]，如 `observer` / `planner`）。
        agent: String,
    },
    /// 单个 Sub-Agent 执行完毕。
    AgentFinished {
        round: u32,
        agent: String,
        success: bool,
        /// 该步的 StepResult.summary 原文，便于直接展示。
        message: String,
    },
    /// 一轮调度结束。
    RoundFinished {
        round: u32,
        /// 本轮的完整记录（含所有步骤、artifacts、耗时）。
        record: RoundRecord,
    },
    /// 整个 Task 调度终态。事件流的最后一个事件。
    TaskCompleted {
        /// 终态：`Success` / `Failed(MaxRoundsExceeded|MaxRetriesExceeded|Panic)`。
        outcome: CycleOutcome,
    },
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

    /// 以事件流方式跑完整循环（M6）。
    ///
    /// 与 [`Self::run`] 的区别：不阻塞调用线程，而是在独立 OS 线程内跑闭环，
    /// 每调一个 Sub-Agent 前后通过 [`mpsc::Receiver`] 推送 [`RoundEvent`]，
    /// 让 Gateway / SSE / WebSocket 实时消费进度。
    ///
    /// 调用方拿到 `Receiver` 后阻塞或异步消费事件即可；当线程内任务结束
    /// （无论成功 / 失败 / panic），会推送一个 [`RoundEvent::TaskCompleted`]
    /// 作为终态事件，随后 sender 自动 drop，`rx.iter()` 自然终止。
    ///
    /// **不变项**：
    /// - 调度顺序、熔断策略、artifacts 累积规则与 `run_inner` 完全一致
    ///   （仅多包了事件发送；不影响结果正确性）。
    /// - 不持久化 history（与 [`Self::run`] 一致）；如需持久化请走
    ///   [`Self::run_with_history`] 的同步路径。
    /// - panic 兜底：线程内任意一步 panic 都会被捕获，发送一个
    ///   `TaskCompleted { outcome: Failed(Panic) }` 后退出，避免调用方阻塞。
    ///
    /// **`&mut self` 的语义**：当前 M3 实现下 Sub-Agent 是无状态 unit struct，
    /// 故 `run_streaming` 实际不修改 `self`；保留 `&mut self` 为 M7 AI 驱动调度
    /// （可能需要在调度中调整内部状态）留出扩展位。
    pub fn run_streaming(&mut self, task: &Task, workspace: &Path) -> mpsc::Receiver<RoundEvent> {
        let (tx, rx) = mpsc::channel::<RoundEvent>();
        // 复制必要数据到独立线程：Sub-Agent 是无状态 unit struct，直接在新线程构造；
        // config / task / workspace 均 Clone，可安全 move 进 'static 闭包。
        let config = self.config.clone();
        let task = task.clone();
        let workspace: PathBuf = workspace.to_path_buf();

        thread::spawn(move || {
            run_inner_streaming(config, task, workspace, tx);
        });

        rx
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

// ============================================================
// M6 事件流：`run_streaming` 内核与辅助
// ============================================================

/// 在调用 Sub-Agent 前后向 `tx` 推送 [`RoundEvent::AgentStarted`] /
/// [`RoundEvent::AgentFinished`]，并返回该步的 [`StepOutput`]。
///
/// 设计为泛型 `<A: SubAgent>` 是为了让编译器在编译期静态分发各 Sub-Agent，
/// 避免引入 `&dyn SubAgent` 的虚函数开销。
fn run_agent_with_events<A: SubAgent>(
    agent: &A,
    ctx: &StepContext,
    round: u32,
    tx: &mpsc::Sender<RoundEvent>,
) -> StepOutput {
    // 发送失败（消费端已 drop）不阻断调度——后续 send 也会失败，最终 TaskCompleted
    // 同样送不出去，但调度本身仍可正常完成。
    let _ = tx.send(RoundEvent::AgentStarted {
        round,
        agent: agent.name().to_string(),
    });
    let out = agent.run(ctx);
    let _ = tx.send(RoundEvent::AgentFinished {
        round,
        agent: agent.name().to_string(),
        success: out.result.success,
        message: out.result.summary.clone(),
    });
    out
}

/// `run_streaming` 的内核：在当前线程跑闭环，每步前后发事件，跑完发
/// [`RoundEvent::TaskCompleted`]。
///
/// 与 [`Cycleround::run_inner`] 的差异：
/// - 调度顺序、熔断条件完全一致（M6 不动调度，只加事件管道）；
/// - 不写 history store（事件流路径下，调用方若需持久化可在消费端按
///   `RoundFinished` 事件落盘）；
/// - 每轮末尾发 `RoundFinished`，与 `run_inner` 中 `history.push(rec)` 的位置一一对应；
/// - 整个调度被 `catch_unwind` 包裹，panic 时回告 `Failed(Panic)`，
///   防止线程静默崩溃、调用方无限阻塞在 `rx.recv()`。
///
/// 各 Sub-Agent 是无状态 unit struct，故在此函数内重新构造实例即可，
/// 无需把 `&Cycleround` 跨线程搬运（也避免 `&self` 生命周期的 `Send` 问题）。
fn run_inner_streaming(
    config: CycleConfig,
    task: Task,
    workspace: PathBuf,
    tx: mpsc::Sender<RoundEvent>,
) {
    // Sub-Agent 是无状态 unit struct，新线程内直接构造即可。
    let observer = Observer;
    let planner = Planner;
    let worker = Worker;
    let tester = Tester;
    let reviewer = Reviewer;
    let fixer = Fixer;

    // AssertUnwindSafe：闭包内捕获了 `tx`（Sender 本身 Send + Sync，但不自动 impl UnwindSafe）。
    // 调度逻辑不依赖任何跨 panic 不变量（每个变量都是局部、无外部副作用），
    // 故 AssertUnwindSafe 的语义安全。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut history: Vec<RoundRecord> = Vec::new();
        let mut artifacts: Vec<Artifact> = Vec::new();
        let mut fix_attempts: u32 = 0;

        for round in 1..=config.max_rounds {
            let started_at = chrono::Utc::now();
            let mut steps: Vec<StepResult> = Vec::new();
            let mut round_artifacts: Vec<Artifact> = Vec::new();

            // 1. Observer（始终执行；当前确定性实现不失败）
            let ctx = StepContext::new(&workspace, task.clone()).with_priors(&[], &artifacts);
            let obs_out = run_agent_with_events(&observer, &ctx, round, &tx);
            steps.push(obs_out.result.clone());
            round_artifacts.extend(obs_out.artifacts);

            // 2. Planner（始终执行；失败则本轮跳过后续步骤且不调 Fixer）
            let ctx = StepContext::new(&workspace, task.clone())
                .with_priors(&steps_as_steps(&steps), &round_artifacts);
            let plan_out = run_agent_with_events(&planner, &ctx, round, &tx);
            steps.push(plan_out.result.clone());
            round_artifacts.extend(plan_out.artifacts);
            let planner_succeeded = plan_out.result.success;

            // 3. Worker（仅当 Planner 成功时执行）
            let mut worker_succeeded = false;
            if planner_succeeded {
                let ctx = StepContext::new(&workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let work_out = run_agent_with_events(&worker, &ctx, round, &tx);
                steps.push(work_out.result.clone());
                round_artifacts.extend(work_out.artifacts);
                worker_succeeded = work_out.result.success;
            }

            // 4. Tester（仅当 Worker 成功时执行）
            let mut tester_succeeded = false;
            if worker_succeeded {
                let ctx = StepContext::new(&workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let test_out = run_agent_with_events(&tester, &ctx, round, &tx);
                steps.push(test_out.result.clone());
                round_artifacts.extend(test_out.artifacts);
                tester_succeeded = test_out.result.success;
            }

            // 5. Reviewer（仅当 Tester 成功时执行）
            let mut reviewer_succeeded = false;
            if tester_succeeded {
                let ctx = StepContext::new(&workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let rev_out = run_agent_with_events(&reviewer, &ctx, round, &tx);
                steps.push(rev_out.result.clone());
                round_artifacts.extend(rev_out.artifacts);
                reviewer_succeeded = rev_out.result.success;
            }

            // 全部成功 → 发 RoundFinished 并返回 Success
            if planner_succeeded && worker_succeeded && tester_succeeded && reviewer_succeeded {
                let rec = build_round(round, started_at, steps, round_artifacts.clone());
                let _ = tx.send(RoundEvent::RoundFinished {
                    round,
                    record: rec.clone(),
                });
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
                let ctx = StepContext::new(&workspace, task.clone())
                    .with_priors(&steps_as_steps(&steps), &round_artifacts);
                let fix_out = run_agent_with_events(&fixer, &ctx, round, &tx);
                steps.push(fix_out.result.clone());
                round_artifacts.extend(fix_out.artifacts);
                fix_attempts += 1;

                if fix_attempts >= config.max_retries {
                    let rec = build_round(round, started_at, steps, round_artifacts.clone());
                    let _ = tx.send(RoundEvent::RoundFinished {
                        round,
                        record: rec.clone(),
                    });
                    history.push(rec);
                    artifacts.extend(round_artifacts);
                    return CycleOutcome::Failed {
                        rounds: round,
                        reason: FailureReason::MaxRetriesExceeded,
                        history,
                    };
                }
            }

            // 本轮未成功但未触达 max_retries：发 RoundFinished 后进入下一轮
            let rec = build_round(round, started_at, steps, round_artifacts.clone());
            let _ = tx.send(RoundEvent::RoundFinished {
                round,
                record: rec.clone(),
            });
            history.push(rec);
            artifacts.extend(round_artifacts);
        }

        CycleOutcome::Failed {
            rounds: config.max_rounds,
            reason: FailureReason::MaxRoundsExceeded,
            history,
        }
    }));

    // 无论成功 / 失败 / panic，都送一个 TaskCompleted 出去。
    // panic 时构造一个 Failed(Panic) 兜底，调用方收到即可关闭 rx。
    let outcome = result.unwrap_or_else(|_| CycleOutcome::Failed {
        rounds: 0,
        reason: FailureReason::Panic,
        history: Vec::new(),
    });
    let _ = tx.send(RoundEvent::TaskCompleted { outcome });
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

#[cfg(test)]
mod streaming_tests {
    //! M6 事件流（`run_streaming`）单元测试。
    //!
    //! 覆盖三类断言：
    //! 1. 事件序列完整性：能收到 `AgentStarted` / `AgentFinished` / `RoundFinished` /
    //!    `TaskCompleted` 四类事件，且 `TaskCompleted` 永远是最后一条。
    //! 2. 成功路径：hello.py 任务以 `TaskCompleted { Success }` 收尾，且事件流中
    //!    可以重建出全部 5 步的 agent 调用对（Observer/Planner/Worker/Tester/Reviewer）。
    //! 3. 熔断路径：不可解析任务以 `TaskCompleted { Failed(MaxRoundsExceeded) }`
    //!    收尾，且 `RoundFinished` 数等于 `max_rounds`。
    use super::*;
    use crate::sub_agents::find_python;

    /// 辅助：把 `mpsc::Receiver<RoundEvent>` 中的事件全部收集到 `Vec<RoundEvent>`，
    /// 直到收到 `TaskCompleted`（含）为止。若线程异常导致 sender 提前 drop，
    /// `rx.iter()` 也会自然终止，函数返回当前已收集的事件。
    fn collect_events(rx: mpsc::Receiver<RoundEvent>) -> Vec<RoundEvent> {
        let mut events = Vec::new();
        for ev in rx.iter() {
            let is_terminal = matches!(ev, RoundEvent::TaskCompleted { .. });
            events.push(ev);
            if is_terminal {
                break;
            }
        }
        events
    }

    #[test]
    fn streaming_emits_all_four_event_kinds_in_order() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 准备：与同步成功路径测试相同的 workspace（已含 test.py 让 Tester 通过）。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-stream-1".into(), "创建 hello.py 输出 hello".into());
        let mut cycle = Cycleround::new(CycleConfig {
            max_rounds: 2,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let rx = cycle.run_streaming(&task, ws.path());
        let events = collect_events(rx);

        // 终态必须是 TaskCompleted，且必须是最后一个事件。
        let last = events.last().expect("事件流不应为空");
        assert!(
            matches!(last, RoundEvent::TaskCompleted { .. }),
            "最后一个事件应为 TaskCompleted，实际: {last:?}"
        );

        // 必须四类事件都出现过至少一次。
        let has_started = events
            .iter()
            .any(|e| matches!(e, RoundEvent::AgentStarted { .. }));
        let has_finished = events
            .iter()
            .any(|e| matches!(e, RoundEvent::AgentFinished { .. }));
        let has_round = events
            .iter()
            .any(|e| matches!(e, RoundEvent::RoundFinished { .. }));
        let has_task = events
            .iter()
            .any(|e| matches!(e, RoundEvent::TaskCompleted { .. }));
        assert!(has_started, "应至少有一个 AgentStarted 事件");
        assert!(has_finished, "应至少有一个 AgentFinished 事件");
        assert!(has_round, "应至少有一个 RoundFinished 事件");
        assert!(has_task, "应至少有一个 TaskCompleted 事件");

        // TaskCompleted 只应出现一次（作为终态）。
        let task_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::TaskCompleted { .. }))
            .count();
        assert_eq!(task_count, 1, "TaskCompleted 应仅出现一次");

        // 顺序约束：每个 AgentFinished 之前必有一个匹配的 AgentStarted（同 round+agent）。
        let mut started_pairs: Vec<(u32, String)> = Vec::new();
        for ev in &events {
            match ev {
                RoundEvent::AgentStarted { round, agent } => {
                    started_pairs.push((*round, agent.clone()));
                }
                RoundEvent::AgentFinished { round, agent, .. } => {
                    let idx = started_pairs
                        .iter()
                        .position(|(r, a)| r == round && a == agent)
                        .expect("AgentFinished 之前应有匹配的 AgentStarted");
                    started_pairs.remove(idx);
                }
                _ => {}
            }
        }
        assert!(
            started_pairs.is_empty(),
            "存在未配对的 AgentStarted: {started_pairs:?}"
        );
    }

    #[test]
    fn streaming_success_path_emits_task_completed_with_success() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // 与 `cycleround_succeeds_on_first_round_for_hello_task` 同样的成功场景，
        // 但走事件流路径。验证：
        // - 第一轮 5 个 agent（observer/planner/worker/tester/reviewer）的 AgentFinished
        //   全部 success=true；
        // - TaskCompleted.outcome 是 Success，rounds=1；
        // - hello.py 真实落盘。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == 'hello'\n",
        )
        .unwrap();
        let task = Task::new("T-stream-success".into(), "创建 hello.py 输出 hello".into());
        let mut cycle = Cycleround::new(CycleConfig {
            max_rounds: 2,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let rx = cycle.run_streaming(&task, ws.path());
        let events = collect_events(rx);

        // 收集第一轮所有 AgentFinished 的 agent 名（按出现顺序）。
        let first_round_agents: Vec<String> = events
            .iter()
            .filter_map(|ev| match ev {
                RoundEvent::AgentFinished {
                    round,
                    agent,
                    success,
                    ..
                } if *round == 1 => {
                    assert!(*success, "成功路径下第一轮每步都应 success=true");
                    Some(agent.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            first_round_agents,
            vec!["observer", "planner", "worker", "tester", "reviewer"],
            "第一轮应按 5 个 Sub-Agent 顺序完成，实际: {first_round_agents:?}"
        );

        // 应恰好有 1 个 RoundFinished（在 TaskCompleted 之前）。
        let round_finished_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::RoundFinished { .. }))
            .count();
        assert_eq!(round_finished_count, 1, "成功路径应只有 1 个 RoundFinished");

        // 终态：Success，rounds=1。
        let outcome = events
            .iter()
            .find_map(|e| match e {
                RoundEvent::TaskCompleted { outcome } => Some(outcome.clone()),
                _ => None,
            })
            .expect("应有 TaskCompleted 事件");
        match outcome {
            CycleOutcome::Success {
                rounds,
                artifacts,
                history,
            } => {
                assert_eq!(rounds, 1, "应在第一轮就成功");
                assert!(!artifacts.is_empty(), "应产出 artifacts");
                assert_eq!(history.len(), 1, "history 应有一条 round 记录");
            }
            other => panic!("expected Success, got {other:?}"),
        }

        // Worker 应真实产出 hello.py（含 trailing newline），与同步路径一致。
        let hello = ws.path().join("hello.py");
        assert!(hello.is_file(), "hello.py 应已落盘");
        assert_eq!(
            std::fs::read_to_string(&hello).unwrap(),
            "hello\n",
            "hello.py 内容应与同步路径一致"
        );
    }

    #[test]
    fn streaming_max_rounds_exceeded_emits_task_completed_with_failed() {
        // 不可解析任务：每轮 Planner 失败 → 跳过 Worker/Tester/Reviewer/Fixer →
        // 跑满 max_rounds=3 后回告 Failed(MaxRoundsExceeded)。
        // 验证：
        // - 每轮只有 observer + planner 两个 AgentFinished（planner 的 success=false）；
        // - RoundFinished 数等于 max_rounds；
        // - TaskCompleted.outcome 是 Failed(MaxRoundsExceeded)。
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new(
            "T-stream-fail".into(),
            "this is not a parseable plan".into(),
        );
        let max_rounds = 3;
        let mut cycle = Cycleround::new(CycleConfig {
            max_rounds,
            max_retries: 3,
            cool_down: Duration::from_secs(0),
        });

        let rx = cycle.run_streaming(&task, ws.path());
        let events = collect_events(rx);

        // 每轮的 AgentFinished 序列应是 [observer(success), planner(fail)]。
        for round in 1..=max_rounds {
            let agents: Vec<(&str, bool)> = events
                .iter()
                .filter_map(|ev| match ev {
                    RoundEvent::AgentFinished {
                        round: r,
                        agent,
                        success,
                        ..
                    } if *r == round => Some((agent.as_str(), *success)),
                    _ => None,
                })
                .collect();
            assert_eq!(
                agents.len(),
                2,
                "第 {round} 轮应只有 observer + planner 两步（Planner 失败跳过后续），实际: {agents:?}"
            );
            assert_eq!(agents[0].0, "observer", "第 {round} 轮第一步应为 observer");
            assert!(agents[0].1, "Observer 应始终成功");
            assert_eq!(agents[1].0, "planner", "第 {round} 轮第二步应为 planner");
            assert!(!agents[1].1, "Planner 应失败（任务不可解析）");
        }

        // RoundFinished 数应等于 max_rounds。
        let round_finished_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::RoundFinished { .. }))
            .count();
        assert_eq!(
            round_finished_count as u32, max_rounds,
            "应执行满 {max_rounds} 轮，RoundFinished 数: {round_finished_count}"
        );

        // 终态：Failed(MaxRoundsExceeded)。
        let outcome = events
            .iter()
            .find_map(|e| match e {
                RoundEvent::TaskCompleted { outcome } => Some(outcome.clone()),
                _ => None,
            })
            .expect("应有 TaskCompleted 事件");
        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(rounds, max_rounds, "应跑满 {max_rounds} 轮");
                assert_eq!(
                    reason,
                    FailureReason::MaxRoundsExceeded,
                    "失败原因应为 MaxRoundsExceeded"
                );
                assert_eq!(
                    history.len(),
                    max_rounds as usize,
                    "history 应有 {max_rounds} 条 round 记录"
                );
            }
            other => panic!("expected Failed(MaxRoundsExceeded), got {other:?}"),
        }
    }
}
