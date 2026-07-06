//! AI 驱动的 Cycleround（M7 P0）。
//!
//! 与 [`crate::Cycleround`] / [`crate::LlmCycleround`] 的核心差异：
//! - **不再硬编码** Observer→Planner→Worker→Tester→Reviewer→Fixer 顺序
//! - 每"步"由调度 LLM 决定下一步调哪个 SubAgent，调用后再让 LLM 决定下一步
//! - LLM 看到 task + 所有前序 StepResult 摘要，自然避免重复犯错
//!   （**解决 ARCHITECTURE_ANALYSIS.md Gap 9**：Reviewer 跨轮拒绝记忆——
//!   调度 LLM 拿到全部前序失败摘要，自己能避免重蹈覆辙）
//! - LLM 决策 `exit` 时认为任务完成，返回 `Success`
//!
//! ## 熔断（保留）
//!
//! - `max_rounds`：**总决策步数上限**（每步 = 1 次 AI 决策 + 1 次 agent 执行）
//! - `max_retries`：**同一 agent 连续失败次数上限**（不再 Fixer 专属；
//!   AI 反复调同一 agent 但一直失败时触发）
//!
//! ## 事件流（保留）
//!
//! [`Self::run_streaming`] 与 [`crate::Cycleround::run_streaming`] 同样基于
//! [`mpsc::Receiver<RoundEvent>`]，让 Gateway / IM 卡片实时消费进度。
//!
//! ## 语义差异说明
//!
//! M3 的 "round" = 5 步固定链路；M7 的 "round" = 1 步 AI 决策。`max_rounds`
//! 的语义因此从「循环尝试次数」变为「总决策步数」。调用方需相应调大
//! `max_rounds`（例如 20~30 才够一个 Plan→Code→Test→Review→Fix→exit 链路）。

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use orcha_llm::{ChatMessage, ChatResponse, LlmClient, LlmError, ToolDefinition};
use orcha_sdk::{Artifact, StepResult, Task};
use serde::{Deserialize, Serialize};

use crate::approval::{ApprovalHook, NullApprovalHook};
use crate::cycleround::{
    build_round, persist_round, CycleConfig, CycleOutcome, FailureReason, RoundEvent, RoundRecord,
};
use crate::history::HistoryStore;
use crate::llm_agents::{LlmPlanner, LlmReviewer, LlmWorker};
use crate::memory::MemoryStore;
use crate::sub_agent::{StepContext, StepOutput, SubAgent};
use crate::sub_agents::{Fixer, Observer, Tester};

/// 调度 LLM 的 system prompt。
const SCHEDULER_SYSTEM: &str = r#"你是 Orcha 的调度大脑。根据任务和已完成的步骤，决定下一步调用哪个 Sub-Agent。

可用 Agent：
- observer：列出 workspace 文件，了解当前状态（只读，不调 LLM）
- planner：根据任务生成执行计划（调 LLM 产出 Plan JSON）
- worker：按计划写文件（调 LLM 产出文件内容并写入 workspace）
- tester：跑测试，验证 Worker 产出是否正确（确定性，跑 cargo/pytest/test.py）
- reviewer：审核 Worker 产出的代码是否满足任务要求（调 LLM 审核）
- fixer：当 Tester 失败时，创建测试框架或修复测试基础设施
- exit：任务已完成，结束调度

决策原则：
- 第一轮通常先调 observer 了解 workspace
- 然后 planner 生成计划
- worker 写代码
- tester 验证
- tester 通过后 reviewer 审核
- reviewer 通过则 exit
- 任何步骤失败，思考是重新执行（重新 planner / worker）还是调 fixer
- 避免重复犯错：仔细看前序步骤的失败原因，换思路而非换写法

输出：调用 decide_next_agent 工具，给出 agent 名和决策理由。"#;

/// AI 决策产出的下一步动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiDecision {
    /// 选中的 agent 名：`observer` / `planner` / `worker` / `tester` /
    /// `reviewer` / `fixer` / `exit`
    pub agent: String,
    /// LLM 给出的决策理由（用于审计 / 调试）。
    pub reason: String,
}

/// AI 驱动的 Cycleround（M7）。
///
/// 调度权交给 LLM：每步由 LLM 决定下一步调哪个 SubAgent。熔断与事件流保留，
/// 适配 M7「AI 主导调度，Cycleround 是脚手架」的目标架构（决策 2）。
pub struct AiDrivenCycleround {
    config: CycleConfig,
    client: Arc<dyn LlmClient>,
    observer: Observer,
    planner: LlmPlanner,
    worker: LlmWorker,
    tester: Tester,
    reviewer: LlmReviewer,
    fixer: Fixer,
    memory: Option<Arc<dyn MemoryStore>>,
    /// M7 P1：人工审批 hook。Worker 写文件 / Tester 跑命令前会调它。
    /// 默认 [`crate::NullApprovalHook`]（直接放行）。
    approval: Arc<dyn ApprovalHook>,
}

impl AiDrivenCycleround {
    /// 用指定熔断参数 + LLM client 构造。
    pub fn new(config: CycleConfig, client: Arc<dyn LlmClient>) -> Self {
        Self::build(config, client, None, None)
    }

    /// 用 LLM client + MemoryStore 构造。Memory 同时供 SubAgent 与调度 LLM 使用。
    pub fn with_memory(
        config: CycleConfig,
        client: Arc<dyn LlmClient>,
        memory: Arc<dyn MemoryStore>,
    ) -> Self {
        Self::build(config, client, Some(memory), None)
    }

    /// M7 P1：注入人工审批 hook。Worker 写文件 / Tester 跑命令前会调它。
    /// 适合 `orcha fix --ai --approve` 走 stdin 询问管理员。
    pub fn with_approval(
        config: CycleConfig,
        client: Arc<dyn LlmClient>,
        memory: Option<Arc<dyn MemoryStore>>,
        approval: Arc<dyn ApprovalHook>,
    ) -> Self {
        Self::build(config, client, memory, Some(approval))
    }

    /// 用默认熔断参数 + LLM client 构造。
    pub fn with_defaults(client: Arc<dyn LlmClient>) -> Self {
        Self::new(CycleConfig::default(), client)
    }

    fn build(
        config: CycleConfig,
        client: Arc<dyn LlmClient>,
        memory: Option<Arc<dyn MemoryStore>>,
        approval: Option<Arc<dyn ApprovalHook>>,
    ) -> Self {
        // 闭包需 own 一份 client.clone()，避免借用 client 阻止后续 move。
        let client_for_planner = client.clone();
        let client_for_worker = client.clone();
        let client_for_reviewer = client.clone();
        let mk_planner = || match &memory {
            Some(m) => LlmPlanner::new(client_for_planner.clone()).with_memory(m.clone()),
            None => LlmPlanner::new(client_for_planner.clone()),
        };
        let mk_worker = || match &memory {
            Some(m) => LlmWorker::new(client_for_worker.clone()).with_memory(m.clone()),
            None => LlmWorker::new(client_for_worker.clone()),
        };
        let mk_reviewer = || match &memory {
            Some(m) => LlmReviewer::new(client_for_reviewer.clone()).with_memory(m.clone()),
            None => LlmReviewer::new(client_for_reviewer.clone()),
        };
        Self {
            config,
            client,
            observer: Observer,
            planner: mk_planner(),
            worker: mk_worker(),
            tester: Tester,
            reviewer: mk_reviewer(),
            fixer: Fixer,
            memory,
            approval: approval.unwrap_or_else(|| Arc::new(NullApprovalHook)),
        }
    }

    /// 跑闭环（不持久化 history）。
    pub fn run(&self, task: &Task, workspace: &Path) -> CycleOutcome {
        self.run_inner(task, workspace, None)
    }

    /// 跑闭环并持久化 history。若构造时注入了 memory，也会一并清空 task 的 memory。
    pub fn run_with_history(
        &self,
        task: &Task,
        workspace: &Path,
        history: &dyn HistoryStore,
    ) -> CycleOutcome {
        if let Err(e) = history.clear_history(&task.id) {
            eprintln!("warn: clear_history({}) failed: {}", task.id, e);
        }
        if let Some(m) = &self.memory {
            if let Err(e) = m.clear(&task.id) {
                eprintln!("warn: memory.clear({}) failed: {}", task.id, e);
            }
        }
        self.run_inner(task, workspace, Some(history))
    }

    /// 事件流版本：在独立线程跑，通过 `mpsc::Receiver<RoundEvent>` 推送进度。
    ///
    /// 与 [`crate::Cycleround::run_streaming`] 接口对齐，Gateway 可无差别消费。
    pub fn run_streaming(&mut self, task: &Task, workspace: &Path) -> mpsc::Receiver<RoundEvent> {
        let (tx, rx) = mpsc::channel::<RoundEvent>();
        let config = self.config.clone();
        let task = task.clone();
        let workspace: PathBuf = workspace.to_path_buf();
        let client = self.client.clone();
        let memory = self.memory.clone();
        let approval = self.approval.clone();

        thread::spawn(move || {
            let cycle = AiDrivenCycleround::build(config, client, memory, Some(approval));
            run_inner_streaming(cycle, task, workspace, tx);
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
        let mut steps_total: Vec<StepResult> = Vec::new();
        let mut last_failed_agent: Option<String> = None;
        let mut consecutive_failures: u32 = 0;

        for step_idx in 1..=self.config.max_rounds {
            let started_at = chrono::Utc::now();
            let decision = match self.ai_decide(task, &steps_total, step_idx) {
                Ok(d) => d,
                Err(e) => {
                    // 调度 LLM 决策失败：本步记失败，但继续下一轮，不死循环。
                    let fail_step = format!("S-scheduler-{step_idx}");
                    let summary = format!("调度 LLM 决策失败: {e}");
                    let fail = StepOutput::failure(&fail_step, &summary);
                    steps_total.push(fail.result.clone());
                    let rec = build_round(
                        step_idx,
                        started_at,
                        vec![fail.result.clone()],
                        Vec::new(),
                    );
                    persist_round(history_store, &task.id, &rec);
                    history.push(rec);
                    // 决策失败也算「调度器」连续失败，纳入 max_retries 计数。
                    if last_failed_agent.as_deref() == Some("scheduler") {
                        consecutive_failures += 1;
                    } else {
                        consecutive_failures = 1;
                        last_failed_agent = Some("scheduler".to_string());
                    }
                    if consecutive_failures >= self.config.max_retries {
                        return CycleOutcome::Failed {
                            rounds: step_idx,
                            reason: FailureReason::MaxRetriesExceeded,
                            history,
                        };
                    }
                    continue;
                }
            };

            // AI 决策 exit → 任务完成
            if decision.agent == "exit" {
                let rec = build_round(step_idx, started_at, Vec::new(), Vec::new());
                persist_round(history_store, &task.id, &rec);
                history.push(rec);
                return CycleOutcome::Success {
                    rounds: step_idx,
                    artifacts,
                    history,
                };
            }

            // 调对应 agent
            let ctx = StepContext::new(workspace, task.clone())
                .with_priors(&Vec::new(), &artifacts)
                .with_approval(self.approval.clone());
            let out = self.dispatch(&decision.agent, &ctx, step_idx);

            let one_step = vec![out.result.clone()];
            let rec = build_round(step_idx, started_at, one_step, out.artifacts.clone());
            persist_round(history_store, &task.id, &rec);
            history.push(rec);
            steps_total.push(out.result.clone());
            artifacts.extend(out.artifacts);

            // 连续失败计数：同一 agent 连续失败 N 次即熔断
            if !out.result.success {
                if last_failed_agent.as_deref() == Some(&decision.agent) {
                    consecutive_failures += 1;
                } else {
                    consecutive_failures = 1;
                    last_failed_agent = Some(decision.agent.clone());
                }
                if consecutive_failures >= self.config.max_retries {
                    return CycleOutcome::Failed {
                        rounds: step_idx,
                        reason: FailureReason::MaxRetriesExceeded,
                        history,
                    };
                }
            } else {
                consecutive_failures = 0;
                last_failed_agent = None;
            }
        }

        CycleOutcome::Failed {
            rounds: self.config.max_rounds,
            reason: FailureReason::MaxRoundsExceeded,
            history,
        }
    }

    /// 调用调度 LLM 产出下一步决策。
    fn ai_decide(
        &self,
        task: &Task,
        prior_steps: &[StepResult],
        step_idx: u32,
    ) -> Result<AiDecision, LlmError> {
        let prior_summary = summarize_steps(prior_steps);
        let msgs = vec![
            ChatMessage::system(SCHEDULER_SYSTEM),
            ChatMessage::user(format!(
                "任务：{}\n\n当前是第 {step_idx} 步决策。前序步骤摘要：\n{prior_summary}\n\n\
                 请调用 decide_next_agent 工具决定下一步调用哪个 Sub-Agent。",
                task.description
            )),
        ];

        let tools = vec![decide_next_agent_tool()];
        let resp = self.client.chat_with_tools(&msgs, &tools)?;
        parse_decision(&resp)
    }

    /// 按决策调对应 SubAgent。未知 agent 返回失败。
    fn dispatch(&self, agent: &str, ctx: &StepContext, round: u32) -> StepOutput {
        match agent {
            "observer" => self.observer.run(ctx),
            "planner" => self.planner.run_at(ctx, round),
            "worker" => self.worker.run_at(ctx, round),
            "tester" => self.tester.run(ctx),
            "reviewer" => self.reviewer.run_at(ctx, round),
            "fixer" => self.fixer.run(ctx),
            "exit" => StepOutput::success(format!("S-exit-{round}"), "调度结束"),
            other => StepOutput::failure(
                format!("S-unknown-{round}"),
                format!("未知 agent: {other}"),
            ),
        }
    }
}

/// `decide_next_agent` 工具定义。LLM 必须通过 tool calling 产出结构化决策。
fn decide_next_agent_tool() -> ToolDefinition {
    ToolDefinition::new(
        "decide_next_agent",
        "决定下一步调用哪个 Sub-Agent。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "enum": ["observer", "planner", "worker", "tester", "reviewer", "fixer", "exit"],
                    "description": "下一步调用的 agent 名"
                },
                "reason": {
                    "type": "string",
                    "description": "决策理由（一句话）"
                }
            },
            "required": ["agent", "reason"]
        }),
    )
}

/// 把前序 StepResult 列表压缩成调度 LLM 可读的摘要。
///
/// 格式：`{idx}. [✓|✗] {step_id}: {summary}`
fn summarize_steps(steps: &[StepResult]) -> String {
    if steps.is_empty() {
        return "（无）".to_string();
    }
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let status = if s.success { "✓" } else { "✗" };
            format!("{}. [{status}] {}: {}", i + 1, s.step_id, s.summary)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 解析 LLM 响应为 [`AiDecision`]。
///
/// 优先用 tool_calls（结构化）；兜底解析 content 为 JSON。
fn parse_decision(resp: &ChatResponse) -> Result<AiDecision, LlmError> {
    // 优先：tool_calls 中的 decide_next_agent
    for call in &resp.tool_calls {
        if call.function.name == "decide_next_agent" {
            let args: serde_json::Value = serde_json::from_str(&call.function.arguments)
                .map_err(|e| {
                    LlmError::Parse(format!("解析 decide_next_agent 参数失败: {e}"))
                })?;
            let agent = args
                .get("agent")
                .and_then(|v| v.as_str())
                .ok_or_else(|| LlmError::Parse("decide_next_agent 缺少 agent 字段".into()))?
                .to_string();
            let reason = args
                .get("reason")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            return Ok(AiDecision { agent, reason });
        }
    }

    // 兜底：解析 content 为 JSON
    let content = resp.content.as_deref().unwrap_or("");
    let trimmed = content.trim().trim_matches(|c: char| c == '`');
    let parsed: AiDecision = serde_json::from_str(trimmed).map_err(|e| {
        LlmError::Parse(format!(
            "decide_next_agent 未返回 tool_call，且 content 非 JSON: {e}; raw={content}"
        ))
    })?;
    Ok(parsed)
}

/// `run_streaming` 的内核：在当前线程跑 AI 驱动调度，每步前后发事件，
/// 跑完发 [`RoundEvent::TaskCompleted`]。
fn run_inner_streaming(
    cycle: AiDrivenCycleround,
    task: Task,
    workspace: PathBuf,
    tx: mpsc::Sender<RoundEvent>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut history: Vec<RoundRecord> = Vec::new();
        let mut artifacts: Vec<Artifact> = Vec::new();
        let mut steps_total: Vec<StepResult> = Vec::new();
        let mut last_failed_agent: Option<String> = None;
        let mut consecutive_failures: u32 = 0;

        for step_idx in 1..=cycle.config.max_rounds {
            let started_at = chrono::Utc::now();
            let decision = match cycle.ai_decide(&task, &steps_total, step_idx) {
                Ok(d) => d,
                Err(e) => {
                    let fail_step = format!("S-scheduler-{step_idx}");
                    let summary = format!("调度 LLM 决策失败: {e}");
                    let _ = tx.send(RoundEvent::AgentStarted {
                        round: step_idx,
                        agent: "scheduler".into(),
                    });
                    let _ = tx.send(RoundEvent::AgentFinished {
                        round: step_idx,
                        agent: "scheduler".into(),
                        success: false,
                        message: summary.clone(),
                    });
                    let fail = StepOutput::failure(&fail_step, &summary);
                    steps_total.push(fail.result.clone());
                    let rec = build_round(
                        step_idx,
                        started_at,
                        vec![fail.result],
                        Vec::new(),
                    );
                    let _ = tx.send(RoundEvent::RoundFinished {
                        round: step_idx,
                        record: rec.clone(),
                    });
                    history.push(rec);

                    if last_failed_agent.as_deref() == Some("scheduler") {
                        consecutive_failures += 1;
                    } else {
                        consecutive_failures = 1;
                        last_failed_agent = Some("scheduler".to_string());
                    }
                    if consecutive_failures >= cycle.config.max_retries {
                        return CycleOutcome::Failed {
                            rounds: step_idx,
                            reason: FailureReason::MaxRetriesExceeded,
                            history,
                        };
                    }
                    continue;
                }
            };

            if decision.agent == "exit" {
                let _ = tx.send(RoundEvent::AgentStarted {
                    round: step_idx,
                    agent: "exit".into(),
                });
                let _ = tx.send(RoundEvent::AgentFinished {
                    round: step_idx,
                    agent: "exit".into(),
                    success: true,
                    message: decision.reason.clone(),
                });
                let rec = build_round(step_idx, started_at, Vec::new(), Vec::new());
                let _ = tx.send(RoundEvent::RoundFinished {
                    round: step_idx,
                    record: rec.clone(),
                });
                history.push(rec);
                return CycleOutcome::Success {
                    rounds: step_idx,
                    artifacts,
                    history,
                };
            }

            let ctx = StepContext::new(&workspace, task.clone())
                .with_priors(&Vec::new(), &artifacts)
                .with_approval(cycle.approval.clone());
            let _ = tx.send(RoundEvent::AgentStarted {
                round: step_idx,
                agent: decision.agent.clone(),
            });
            let out = cycle.dispatch(&decision.agent, &ctx, step_idx);
            let _ = tx.send(RoundEvent::AgentFinished {
                round: step_idx,
                agent: decision.agent.clone(),
                success: out.result.success,
                message: out.result.summary.clone(),
            });

            let one_step = vec![out.result.clone()];
            let rec = build_round(step_idx, started_at, one_step, out.artifacts.clone());
            let _ = tx.send(RoundEvent::RoundFinished {
                round: step_idx,
                record: rec.clone(),
            });
            history.push(rec);
            steps_total.push(out.result.clone());
            artifacts.extend(out.artifacts);

            if !out.result.success {
                if last_failed_agent.as_deref() == Some(&decision.agent) {
                    consecutive_failures += 1;
                } else {
                    consecutive_failures = 1;
                    last_failed_agent = Some(decision.agent.clone());
                }
                if consecutive_failures >= cycle.config.max_retries {
                    return CycleOutcome::Failed {
                        rounds: step_idx,
                        reason: FailureReason::MaxRetriesExceeded,
                        history,
                    };
                }
            } else {
                consecutive_failures = 0;
                last_failed_agent = None;
            }
        }

        CycleOutcome::Failed {
            rounds: cycle.config.max_rounds,
            reason: FailureReason::MaxRoundsExceeded,
            history,
        }
    }));

    let outcome = result.unwrap_or_else(|_| CycleOutcome::Failed {
        rounds: 0,
        reason: FailureReason::Panic,
        history: Vec::new(),
    });
    let _ = tx.send(RoundEvent::TaskCompleted { outcome });
}

#[cfg(test)]
mod tests {
    //! AI 驱动 Cycleround 单元测试。
    //!
    //! 用 `AiTestClient` 模拟 LLM 按预设序列响应：
    //! - 调度 LLM（带 decide_next_agent 工具）→ 返回 tool_call 决策
    //! - SubAgent 内部 LLM 调用 → 返回预设 JSON
    //!
    //! 覆盖三类断言：
    //! 1. happy path：AI 依次决策 observer→planner→worker→tester→reviewer→exit
    //! 2. 熔断 max_rounds：AI 永远不 exit，跑满 max_rounds
    //! 3. 熔断 max_retries：AI 反复调 planner 但 planner 一直失败
    use super::*;
    use orcha_llm::{FunctionCall, ToolCallRequest};
    use std::sync::Mutex;
    use std::time::Duration;

    /// 可编程的 mock LLM client：按调用序返回预设 [`ChatResponse`]。
    ///
    /// 调度 LLM 用 `chat_with_tools` → mock 返回 tool_call；
    /// SubAgent 内部用 `chat_with_tools`（planner/worker/reviewer）→ mock 返回 text。
    struct AiTestClient {
        responses: Mutex<std::collections::VecDeque<ChatResponse>>,
    }

    impl AiTestClient {
        fn new(responses: Vec<ChatResponse>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into()),
            })
        }

        fn tool_call(agent: &str, reason: &str) -> ChatResponse {
            let args = serde_json::json!({
                "agent": agent,
                "reason": reason
            })
            .to_string();
            ChatResponse {
                content: None,
                tool_calls: vec![ToolCallRequest {
                    id: "call_decide".into(),
                    call_type: "function".into(),
                    function: FunctionCall {
                        name: "decide_next_agent".into(),
                        arguments: args,
                    },
                }],
            }
        }

        fn text(content: &str) -> ChatResponse {
            ChatResponse {
                content: Some(content.to_string()),
                tool_calls: vec![],
            }
        }
    }

    impl LlmClient for AiTestClient {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, LlmError> {
            // 兜底路径（run_agent_loop 的最后一轮 max_turns 用尽时调用）：
            // 返回下一个响应的 content；若无则报错。
            let mut q = self.responses.lock().unwrap();
            if let Some(r) = q.pop_front() {
                Ok(r.content.unwrap_or_default())
            } else {
                Err(LlmError::Network("mock exhausted".into()))
            }
        }

        fn chat_with_tools(
            &self,
            _messages: &[ChatMessage],
            _tools: &[ToolDefinition],
        ) -> Result<ChatResponse, LlmError> {
            let mut q = self.responses.lock().unwrap();
            if let Some(r) = q.pop_front() {
                Ok(r)
            } else {
                Err(LlmError::Network("mock exhausted".into()))
            }
        }
    }

    /// 构造一条 happy path 的 LLM 响应序列：
    /// decide(observer) → (observer 不调 LLM) → decide(planner) → planner JSON
    /// → decide(worker) → worker JSON → decide(tester) → (tester 不调 LLM)
    /// → decide(reviewer) → reviewer JSON(approved) → decide(exit)
    fn happy_path_responses() -> Vec<ChatResponse> {
        vec![
            AiTestClient::tool_call("observer", "先看 workspace"),
            AiTestClient::tool_call("planner", "需要计划"),
            // planner 内部 run_agent_loop 第一次 chat_with_tools 调用：
            // 返回非 tool_call 的 text（即 plan JSON），run_agent_loop 直接退出。
            AiTestClient::text(
                r#"{"target_files":["hello.py"],"steps":[{"action":"create","path":"hello.py","content":"print('hello')\n"}]}"#,
            ),
            AiTestClient::tool_call("worker", "执行计划"),
            AiTestClient::text(
                r#"{"files":[{"path":"hello.py","content":"print('hello')\n"}],"summary":"ok"}"#,
            ),
            AiTestClient::tool_call("tester", "跑测试"),
            AiTestClient::tool_call("reviewer", "审核"),
            AiTestClient::text(r#"{"approved":true,"issues":[]}"#),
            AiTestClient::tool_call("exit", "任务完成"),
        ]
    }

    #[test]
    fn happy_path_succeeds_with_ai_driven_scheduling() {
        if crate::sub_agents::find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        // workspace 预置 test.py，让 Tester 跑通。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == \"hello\"\n",
        )
        .unwrap();

        let client = AiTestClient::new(happy_path_responses());
        let cycle = AiDrivenCycleround::new(
            CycleConfig {
                max_rounds: 30,
                max_retries: 3,
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let task = Task::new("T-ai-1".into(), "创建 hello.py 输出 hello".into());

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success {
                rounds,
                history,
                artifacts,
            } => {
                // 6 步决策：observer / planner / worker / tester / reviewer / exit
                assert_eq!(
                    rounds, 6,
                    "应在第 6 步（exit 决策）成功，实际: {rounds}"
                );
                assert!(!artifacts.is_empty(), "应累积 artifacts");
                assert_eq!(history.len(), 6, "history 应有 6 条 round 记录");
                // hello.py 应由 Worker 真实落盘
                let hello = ws.path().join("hello.py");
                assert!(hello.is_file(), "hello.py 应已落盘");
                assert!(
                    std::fs::read_to_string(&hello)
                        .unwrap()
                        .contains("print('hello')"),
                    "hello.py 内容应来自 Worker LLM 输出"
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn max_rounds_exceeded_when_ai_never_exits() {
        // AI 一直决策 observer，永不 exit，跑满 max_rounds=4。
        let ws = tempfile::tempdir().unwrap();
        let responses: Vec<ChatResponse> = (0..10)
            .map(|_| AiTestClient::tool_call("observer", "继续观察"))
            .collect();
        let client = AiTestClient::new(responses);
        let cycle = AiDrivenCycleround::new(
            CycleConfig {
                max_rounds: 4,
                max_retries: 5, // 不让 max_retries 先触发
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let task = Task::new("T-ai-2".into(), "随便".into());

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(rounds, 4, "应跑满 max_rounds=4");
                assert_eq!(reason, FailureReason::MaxRoundsExceeded);
                assert_eq!(history.len(), 4, "应记录 4 条 round");
            }
            other => panic!("expected Failed(MaxRoundsExceeded), got {other:?}"),
        }
    }

    #[test]
    fn max_retries_exceeded_on_repeated_agent_failure() {
        // AI 反复决策 planner，但 planner LLM 每次返回非法 JSON，连续失败 3 次。
        // 触达 max_retries=2 时熔断。
        let ws = tempfile::tempdir().unwrap();
        let mut responses = Vec::new();
        // 3 轮：decide(planner) + planner 返回非法 JSON
        for _ in 0..10 {
            responses.push(AiTestClient::tool_call("planner", "重试规划"));
            responses.push(AiTestClient::text("not a json"));
        }
        let client = AiTestClient::new(responses);
        let cycle = AiDrivenCycleround::new(
            CycleConfig {
                max_rounds: 20,
                max_retries: 2,
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let task = Task::new("T-ai-3".into(), "创建 a.py".into());

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Failed {
                rounds,
                reason,
                history,
            } => {
                assert_eq!(
                    reason,
                    FailureReason::MaxRetriesExceeded,
                    "应因 planner 连续失败熔断"
                );
                // 第 2 次 planner 失败即触发（max_retries=2）
                assert_eq!(
                    rounds, 2,
                    "应在第 2 次 planner 失败时熔断，实际: {rounds}"
                );
                assert_eq!(history.len(), 2);
            }
            other => panic!("expected Failed(MaxRetriesExceeded), got {other:?}"),
        }
    }

    #[test]
    fn unknown_agent_decision_records_failure_step() {
        // AI 决策一个未知 agent 名，dispatch 返回失败。
        // 不应 panic，应记一条失败 step。
        let ws = tempfile::tempdir().unwrap();
        let responses = vec![
            AiTestClient::tool_call("unknown_agent", "瞎试"),
            AiTestClient::tool_call("exit", "结束"),
        ];
        let client = AiTestClient::new(responses);
        let cycle = AiDrivenCycleround::new(
            CycleConfig {
                max_rounds: 5,
                max_retries: 3,
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let task = Task::new("T-ai-4".into(), "x".into());

        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success { rounds, history, .. } => {
                assert_eq!(rounds, 2, "应在第 2 步 exit 后成功");
                assert_eq!(history.len(), 2);
                // 第 1 步应是失败的未知 agent
                assert!(!history[0].steps[0].success, "未知 agent 应失败");
                assert!(
                    history[0].steps[0].summary.contains("未知 agent"),
                    "summary 应含「未知 agent」, got: {}",
                    history[0].steps[0].summary
                );
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    #[test]
    fn ai_decide_parses_tool_call_correctly() {
        let resp = AiTestClient::tool_call("planner", "需要规划");
        let decision = parse_decision(&resp).unwrap();
        assert_eq!(decision.agent, "planner");
        assert_eq!(decision.reason, "需要规划");
    }

    #[test]
    fn ai_decide_parses_json_fallback() {
        let resp = AiTestClient::text(r#"{"agent":"exit","reason":"done"}"#);
        let decision = parse_decision(&resp).unwrap();
        assert_eq!(decision.agent, "exit");
        assert_eq!(decision.reason, "done");
    }

    #[test]
    fn ai_decide_fails_on_invalid_response() {
        let resp = AiTestClient::text("not a json");
        let result = parse_decision(&resp);
        assert!(result.is_err());
    }

    #[test]
    fn streaming_emits_events_for_happy_path() {
        if crate::sub_agents::find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join("test.py"),
            "assert open('hello.py').read().strip() == \"hello\"\n",
        )
        .unwrap();

        let client = AiTestClient::new(happy_path_responses());
        let mut cycle = AiDrivenCycleround::new(
            CycleConfig {
                max_rounds: 30,
                max_retries: 3,
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let task = Task::new("T-ai-stream".into(), "创建 hello.py 输出 hello".into());

        let rx = cycle.run_streaming(&task, ws.path());
        let mut events = Vec::new();
        for ev in rx.iter() {
            let is_terminal = matches!(ev, RoundEvent::TaskCompleted { .. });
            events.push(ev);
            if is_terminal {
                break;
            }
        }

        // 终态必须是 TaskCompleted
        let last = events.last().expect("事件流不应为空");
        assert!(matches!(last, RoundEvent::TaskCompleted { .. }));

        // 至少应有 6 个 AgentStarted + 6 个 AgentFinished + 6 个 RoundFinished + 1 个 TaskCompleted
        let started_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::AgentStarted { .. }))
            .count();
        let finished_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::AgentFinished { .. }))
            .count();
        let round_count = events
            .iter()
            .filter(|e| matches!(e, RoundEvent::RoundFinished { .. }))
            .count();
        assert!(started_count >= 6, "应至少 6 个 AgentStarted, got {started_count}");
        assert!(finished_count >= 6, "应至少 6 个 AgentFinished, got {finished_count}");
        assert_eq!(round_count, 6, "应恰好 6 个 RoundFinished");

        // 终态应为 Success
        let outcome = events
            .iter()
            .find_map(|e| match e {
                RoundEvent::TaskCompleted { outcome } => Some(outcome.clone()),
                _ => None,
            })
            .expect("应有 TaskCompleted");
        match outcome {
            CycleOutcome::Success { rounds, .. } => {
                assert_eq!(rounds, 6, "应在第 6 步 exit 后成功");
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }
}
