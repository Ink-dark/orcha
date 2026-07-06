//! LLM 驱动的 Sub-Agent 集合（feature = "llm"）。
//!
//! 与确定性 `sub_agents` 并存：Observer/Tester/Fixer 复用确定性实现
//! （它们不需要 LLM——Observer 列文件，Tester 跑命令，Fixer 检测测试框架）。
//! Planner/Worker/Reviewer 改调真 LLM。
//!
//! [`LlmCycleround`] 跑与 [`crate::Cycleround`] 相同的 Plan→Code→Test→Review→Fix
//! 闭环，但 Planner/Worker/Reviewer 用 LLM 版本。熔断配置同。
//!
//! # Memory（D2）
//!
//! 每个可选 `MemoryStore` 让 LLM 多轮之间引用前序对话：Planner 在第 N 轮
//! 能看到第 N-1 轮 Reviewer 的拒绝原因，避免重复犯同样的错。
//! Memory 在 [`LlmCycleround::with_memory`] 注入；不注入时退化为无记忆
//! （单轮也能跑，适合 CI 确定性测试）。
//!
//! 不引入异步运行时；LLM 调用同步阻塞，复用 ureq。

use std::path::Path;
use std::sync::Arc;

use orcha_llm::{
    build_reviewer_prompt, parse_planner_output, parse_reviewer_output, parse_worker_output,
    ChatMessage, LlmClient, PLANNER_SYSTEM, WORKER_SYSTEM,
};
use orcha_sdk::{Artifact, ArtifactType, Task};

use crate::cycleround::{build_round, persist_round};
use crate::history::HistoryStore;
use crate::memory::{MemoryEntry, MemoryStore};
use crate::path_guard::PathGuard;
use crate::plan::check_diff_scope;
use crate::sub_agent::{StepContext, StepOutput, SubAgent};
use crate::sub_agents::{list_workspace_files, Fixer, Observer, Tester};
use crate::tools::{all_tools, run_agent_loop, MAX_TOOL_TURNS};
use crate::{CycleConfig, CycleOutcome, FailureReason, RoundRecord};

/// LLM 驱动的 Planner：调 LLM 产出 JSON 计划。
pub struct LlmPlanner {
    client: Arc<dyn LlmClient>,
    memory: Option<Arc<dyn MemoryStore>>,
}

impl LlmPlanner {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            memory: None,
        }
    }

    /// 注入 MemoryStore，启用多轮对话记忆。
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    /// Memory-aware 执行。`round` 是 1-based 轮次，仅用于 MemoryEntry 标记。
    ///
    /// 当 memory 为 None 时退化为无记忆调用，与 `SubAgent::run` 等价。
    pub fn run_at(&self, ctx: &StepContext, round: u32) -> StepOutput {
        let files = list_workspace_files(&ctx.workspace);
        let files_str = if files.is_empty() {
            "（workspace 为空）".to_string()
        } else {
            files.join(", ")
        };

        let guard = match PathGuard::new(&ctx.workspace) {
            Ok(g) => g,
            Err(e) => {
                return StepOutput::failure("S-planner", format!("PathGuard 初始化失败: {e}"));
            }
        };

        let mut msgs = vec![
            ChatMessage::system(PLANNER_SYSTEM),
            ChatMessage::user(format!(
                "任务：{}\n\n当前 workspace 文件：{files_str}\n\n先用工具探索 workspace，然后输出 JSON 计划。",
                ctx.task.description
            )),
        ];
        inject_memory(&mut msgs, &self.memory, &ctx.task.id);

        let tools = all_tools();
        let resp = match run_agent_loop(
            self.client.as_ref(),
            msgs,
            &tools,
            &ctx.workspace,
            &guard,
            "planner",
            None,
            MAX_TOOL_TURNS,
        ) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-planner", format!("LLM 调用失败: {e}"));
            }
        };

        append_memory(&self.memory, &ctx.task.id, round, "planner", &resp);

        match parse_planner_output(&resp) {
            Ok(plan_json) => {
                let artifact = Artifact {
                    artifact_id: next_artifact_id(&ctx.prior_artifacts, "planner"),
                    artifact_type: ArtifactType::Report,
                    commit_sha: None,
                    patch: None,
                    url: None,
                };
                StepOutput::success(
                    "S-planner",
                    format!("LLM 计划已生成（{} 字节）", plan_json.len()),
                )
                .with_artifacts(vec![artifact])
            }
            Err(e) => {
                StepOutput::failure("S-planner", format!("解析 LLM 输出失败: {e}; raw={resp}"))
            }
        }
    }
}

impl SubAgent for LlmPlanner {
    fn name(&self) -> &'static str {
        "llm-planner"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        // SubAgent trait 不暴露 round；非 Cycleround 调用方用 round=0。
        self.run_at(ctx, 0)
    }
}

/// LLM 驱动的 Worker：调 LLM 按 plan 产出文件并写入 workspace。
pub struct LlmWorker {
    client: Arc<dyn LlmClient>,
    memory: Option<Arc<dyn MemoryStore>>,
}

impl LlmWorker {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            memory: None,
        }
    }

    /// 注入 MemoryStore，启用多轮对话记忆。
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn run_at(&self, ctx: &StepContext, round: u32) -> StepOutput {
        let plan_text = ctx
            .prior_artifacts
            .iter()
            .rev()
            .find(|a| a.artifact_id.contains("planner"))
            .map(|a| a.artifact_id.clone())
            .unwrap_or_default();

        let plan_summary = if plan_text.is_empty() {
            "(无前序 plan，请直接产出文件)".to_string()
        } else {
            format!("参考 planner artifact {}", plan_text)
        };

        let guard = match PathGuard::new(&ctx.workspace) {
            Ok(g) => g,
            Err(e) => {
                return StepOutput::failure("S-worker", format!("PathGuard 初始化失败: {e}"));
            }
        };

        let mut msgs = vec![
            ChatMessage::system(WORKER_SYSTEM),
            ChatMessage::user(format!(
                "任务：{}\n\n计划摘要：{plan_summary}\n\n先用工具读取需要修改的文件，确认内容后再输出 JSON。",
                ctx.task.description
            )),
        ];
        inject_memory(&mut msgs, &self.memory, &ctx.task.id);

        let tools = all_tools();
        let resp = match run_agent_loop(
            self.client.as_ref(),
            msgs,
            &tools,
            &ctx.workspace,
            &guard,
            "worker",
            None,
            MAX_TOOL_TURNS,
        ) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-worker", format!("LLM 调用失败: {e}"));
            }
        };

        append_memory(&self.memory, &ctx.task.id, round, "worker", &resp);

        let files = match parse_worker_output(&resp) {
            Ok(f) => f,
            Err(e) => {
                return StepOutput::failure(
                    "S-worker",
                    format!("解析 LLM 输出失败: {e}; raw={resp}"),
                );
            }
        };

        if files.is_empty() {
            return StepOutput::failure("S-worker", "LLM 未产出任何文件");
        }

        // M4：使用已构造的 PathGuard + target_files 白名单。
        // target_files 直接由 LLM 输出的 file 列表派生（每个 file 即声明要写的路径）。
        // PathGuard 强制：路径不逃逸 workspace + 不在危险路径黑名单内 + 在 target_files 白名单内。
        let target_files: Vec<String> = files.iter().map(|(p, _)| p.clone()).collect();

        let mut artifacts = Vec::new();
        let mut written = Vec::new();
        let mut combined_patch = String::new();
        for (path, content) in &files {
            // M4：统一走 PathGuard::validate_write，强制写边界。
            let resolved = match guard.validate_write(path, &target_files) {
                Ok(p) => p,
                Err(e) => {
                    return StepOutput::failure("S-worker", format!("写边界拒绝 {path}: {e}"));
                }
            };
            // validate_write 已校验路径，但仍走 create_dir_all + write 流程（保留 M2 行为）。
            if let Some(parent) = resolved.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return StepOutput::failure("S-worker", format!("创建目录失败: {e}"));
                }
            }
            let normalized = ensure_trailing_newline(content);
            if let Err(e) = std::fs::write(&resolved, &normalized) {
                return StepOutput::failure("S-worker", format!("写文件失败 {path}: {e}"));
            }
            let patch = make_create_diff(path, &normalized).unwrap_or_default();
            combined_patch.push_str(&patch);
            artifacts.push(Artifact {
                artifact_id: next_artifact_id(&ctx.prior_artifacts, "worker"),
                artifact_type: ArtifactType::CodeDiff,
                commit_sha: None,
                patch: Some(patch),
                url: Some(format!("file:///{}", resolved.display())),
            });
            written.push(path.clone());
        }

        // M4：自检 patch 改动文件集合 ⊆ target_files（防 patch 被篡改含未声明文件）。
        if let Err(e) = check_diff_scope(&combined_patch, &target_files) {
            return StepOutput::failure("S-worker", format!("diff 范围校验失败: {e}"));
        }

        StepOutput::success(
            "S-worker",
            format!("LLM 写入 {} 个文件: {}", written.len(), written.join(", ")),
        )
        .with_artifacts(artifacts)
    }
}

impl SubAgent for LlmWorker {
    fn name(&self) -> &'static str {
        "llm-worker"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        self.run_at(ctx, 0)
    }
}

/// LLM 驱动的 Reviewer：调 LLM 审核 Worker 产出。
pub struct LlmReviewer {
    client: Arc<dyn LlmClient>,
    memory: Option<Arc<dyn MemoryStore>>,
}

impl LlmReviewer {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self {
            client,
            memory: None,
        }
    }

    /// 注入 MemoryStore，启用多轮对话记忆。
    pub fn with_memory(mut self, memory: Arc<dyn MemoryStore>) -> Self {
        self.memory = Some(memory);
        self
    }

    pub fn run_at(&self, ctx: &StepContext, round: u32) -> StepOutput {
        // M4：收集 Worker 产出的 patch 与文件，先做 diff scope 校验，再交 LLM 审。
        // diff scope 校验不依赖 LLM 输出，是确定性前置检查：
        //   patch 改动文件集合必须 ⊆ plan 声明的 target_files
        // 由于 LlmWorker 的 target_files 由 LLM 输出文件列表派生，
        // 这里反向从 worker artifact 抽取改动文件，并从 ctx.task.description 派生期望文件集合。
        // 更严格的 target_files 比对留待 P1（需 Planner 产 Plan 后贯穿）。
        let worker_artifacts: Vec<&Artifact> = ctx
            .prior_artifacts
            .iter()
            .rev()
            .filter(|a| a.artifact_type == ArtifactType::CodeDiff)
            .collect();

        if worker_artifacts.is_empty() {
            return StepOutput::failure("S-reviewer", "无可审核的 Worker 产出");
        }

        // 收集 worker 产出的 patch 文本，做 diff scope 校验。
        // target_files 派生：从 task.description 抽取声明路径（兼容旧路径，无声明则不强制）。
        let combined_patch: String = worker_artifacts
            .iter()
            .filter_map(|a| a.patch.as_deref())
            .collect::<Vec<_>>()
            .join("\n");
        let target_files = derive_target_files_from_task(&ctx.task.description);
        if !target_files.is_empty() {
            if let Err(e) = check_diff_scope(&combined_patch, &target_files) {
                return StepOutput::failure("S-reviewer", format!("diff 范围越界: {e}"));
            }
        }

        let worker_files: Vec<(String, String)> = worker_artifacts
            .iter()
            .filter_map(|a| {
                let u = a.url.as_ref()?;
                let p = u.strip_prefix("file://")?;
                let path = Path::new(p);
                let rel = path.strip_prefix(&ctx.workspace).unwrap_or(path);
                let content = std::fs::read_to_string(path).unwrap_or_default();
                Some((rel.to_string_lossy().into_owned(), content))
            })
            .collect();

        let mut msgs = build_reviewer_prompt(&ctx.task.description, &worker_files);
        inject_memory(&mut msgs, &self.memory, &ctx.task.id);

        let resp = match self.client.chat(&msgs) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-reviewer", format!("LLM 调用失败: {e}"));
            }
        };

        append_memory(&self.memory, &ctx.task.id, round, "reviewer", &resp);

        let (approved, issues) = match parse_reviewer_output(&resp) {
            Ok(v) => v,
            Err(e) => {
                return StepOutput::failure(
                    "S-reviewer",
                    format!("解析 LLM 输出失败: {e}; raw={resp}"),
                );
            }
        };

        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "reviewer"),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };

        if approved {
            StepOutput::success("S-reviewer", "LLM 审核通过").with_artifacts(vec![artifact])
        } else {
            StepOutput::failure(
                "S-reviewer",
                format!("LLM 审核未通过: {}", issues.join("; ")),
            )
            .with_artifacts(vec![artifact])
        }
    }
}

/// 从 task.description 派生 target_files 期望集合。
///
/// 兼容 M2/M3 任务描述格式 `"创建 <filename> 输出 <content>"`：派生为 `[filename]`。
/// M4 新格式（待 Planner 产 Plan 后启用）：直接从 Plan.target_files 取。
/// 无法派生时返回空 Vec，表示不强制（兼容老测试）。
fn derive_target_files_from_task(desc: &str) -> Vec<String> {
    let desc = desc.trim();
    // 兼容旧格式
    if let Some(after) = desc
        .strip_prefix("创建 ")
        .or_else(|| desc.strip_prefix("create "))
    {
        if let Some(output_idx) = after.find(" 输出 ").or_else(|| after.find(" output ")) {
            let filename = after[..output_idx].trim().to_string();
            if !filename.is_empty() {
                return vec![filename];
            }
        }
    }
    Vec::new()
}

impl SubAgent for LlmReviewer {
    fn name(&self) -> &'static str {
        "llm-reviewer"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        self.run_at(ctx, 0)
    }
}

/// LLM 驱动的 Cycleround：跑与 [`crate::Cycleround`] 相同的闭环，
/// 但 Planner/Worker/Reviewer 用 LLM 版本。
pub struct LlmCycleround {
    config: CycleConfig,
    observer: Observer,
    planner: LlmPlanner,
    worker: LlmWorker,
    tester: Tester,
    reviewer: LlmReviewer,
    fixer: Fixer,
    memory: Option<Arc<dyn MemoryStore>>,
}

impl LlmCycleround {
    pub fn new(config: CycleConfig, client: Arc<dyn LlmClient>) -> Self {
        Self::build(config, client, None)
    }

    /// 用 LLM client + MemoryStore 构造。每轮 LLM 调用会读/写 memory，
    /// 让第 N 轮 Planner 能看到第 N-1 轮的失败原因。
    pub fn with_memory(
        config: CycleConfig,
        client: Arc<dyn LlmClient>,
        memory: Arc<dyn MemoryStore>,
    ) -> Self {
        Self::build(config, client, Some(memory))
    }

    fn build(
        config: CycleConfig,
        client: Arc<dyn LlmClient>,
        memory: Option<Arc<dyn MemoryStore>>,
    ) -> Self {
        let mk_planner = || match &memory {
            Some(m) => LlmPlanner::new(client.clone()).with_memory(m.clone()),
            None => LlmPlanner::new(client.clone()),
        };
        let mk_worker = || match &memory {
            Some(m) => LlmWorker::new(client.clone()).with_memory(m.clone()),
            None => LlmWorker::new(client.clone()),
        };
        let mk_reviewer = || match &memory {
            Some(m) => LlmReviewer::new(client.clone()).with_memory(m.clone()),
            None => LlmReviewer::new(client.clone()),
        };
        Self {
            config,
            observer: Observer,
            planner: mk_planner(),
            worker: mk_worker(),
            tester: Tester,
            reviewer: mk_reviewer(),
            fixer: Fixer,
            memory,
        }
    }

    /// 用默认熔断参数（10/3/60s）+ LLM client 构造。
    pub fn with_defaults(client: Arc<dyn LlmClient>) -> Self {
        Self::new(CycleConfig::default(), client)
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
            let mut steps: Vec<orcha_sdk::StepResult> = Vec::new();
            let mut round_artifacts: Vec<Artifact> = Vec::new();

            // Observer（确定性，无需 LLM）
            let ctx = StepContext::new(workspace, task.clone()).with_priors(&[], &artifacts);
            let obs_out = self.observer.run(&ctx);
            steps.push(obs_out.result.clone());
            round_artifacts.extend(obs_out.artifacts);

            // Planner（LLM）
            let ctx = StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
            let plan_out = self.planner.run_at(&ctx, round);
            steps.push(plan_out.result.clone());
            round_artifacts.extend(plan_out.artifacts);
            let planner_succeeded = plan_out.result.success;

            // Worker（LLM）
            let mut worker_succeeded = false;
            if planner_succeeded {
                let ctx =
                    StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
                let work_out = self.worker.run_at(&ctx, round);
                steps.push(work_out.result.clone());
                round_artifacts.extend(work_out.artifacts);
                worker_succeeded = work_out.result.success;
            }

            // Tester（确定性）
            let mut tester_succeeded = false;
            if worker_succeeded {
                let ctx =
                    StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
                let test_out = self.tester.run(&ctx);
                steps.push(test_out.result.clone());
                round_artifacts.extend(test_out.artifacts);
                tester_succeeded = test_out.result.success;
            }

            // Reviewer（LLM）
            let mut reviewer_succeeded = false;
            if tester_succeeded {
                let ctx =
                    StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
                let rev_out = self.reviewer.run_at(&ctx, round);
                steps.push(rev_out.result.clone());
                round_artifacts.extend(rev_out.artifacts);
                reviewer_succeeded = rev_out.result.success;
            }

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

            // Fixer（确定性）
            if planner_succeeded {
                let ctx =
                    StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
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
// Memory 辅助函数
// ============================================================

/// 把前序对话历史作为额外 user 消息注入 prompt。
///
/// 格式：每行 `[round N][agent][role] content`，让 LLM 知道之前发生了什么。
/// Memory 为 None / 读失败 / 为空时不注入。
fn inject_memory(
    msgs: &mut Vec<ChatMessage>,
    memory: &Option<Arc<dyn MemoryStore>>,
    task_id: &str,
) {
    let Some(m) = memory else {
        return;
    };
    let Ok(history) = m.list(task_id) else {
        return;
    };
    if history.is_empty() {
        return;
    }
    let mut s = String::from("前序对话历史（参考以避免重复犯错）：\n");
    for e in &history {
        s.push_str(&format!(
            "[round {}][{}][{}] {}\n",
            e.round, e.agent, e.role, e.content
        ));
    }
    msgs.push(ChatMessage::user(s));
}

/// 把 LLM 的 assistant 响应写入 memory。失败仅打 warn，不阻断流程。
fn append_memory(
    memory: &Option<Arc<dyn MemoryStore>>,
    task_id: &str,
    round: u32,
    agent: &str,
    content: &str,
) {
    if let Some(m) = memory {
        let entry = MemoryEntry {
            round,
            agent: agent.into(),
            role: "assistant".into(),
            content: content.into(),
        };
        if let Err(e) = m.append(task_id, &entry) {
            eprintln!("warn: memory.append({task_id}, {agent}) failed: {e}");
        }
    }
}

// ============================================================
// 辅助函数（与 sub_agents 模块对齐，但模块私有）
// ============================================================

fn next_artifact_id(prior: &[Artifact], agent: &str) -> String {
    let n = prior.len() + 1;
    format!("ART-{agent}-{n:03}")
}

fn ensure_trailing_newline(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if s.ends_with('\n') {
        s.to_string()
    } else {
        format!("{s}\n")
    }
}

fn make_create_diff(filename: &str, content: &str) -> Result<String, String> {
    let lines: Vec<&str> = content.lines().collect();
    if content.is_empty() || lines.is_empty() {
        return Ok(format!(
            "diff --git a/{f} b/{f}\nnew file mode 100644\n--- /dev/null\n+++ b/{f}\n@@ -0,0 +1,0 @@\n",
            f = filename
        ));
    }
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{f} b/{f}\n", f = filename));
    out.push_str("new file mode 100644\n");
    out.push_str("--- /dev/null\n");
    out.push_str(&format!("+++ b/{f}\n", f = filename));
    out.push_str(&format!("@@ -0,0 +1,{n} @@\n", n = lines.len()));
    for line in &lines {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FileMemoryStore;
    use orcha_llm::{ChatMessage, LlmError};
    use std::time::Duration;

    /// 一个可编程的 mock LLM client：按调用序返回预设响应。
    struct MockLlmClient {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
    }

    impl MockLlmClient {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(responses.into()),
            })
        }
    }

    impl LlmClient for MockLlmClient {
        fn chat(&self, _messages: &[ChatMessage]) -> Result<String, LlmError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::Network("mock exhausted".into()))
        }
    }

    /// 一个可记录所发消息的 mock client，用于断言 memory 是否被注入 prompt。
    struct RecordingMockClient {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        sent: std::sync::Mutex<Vec<Vec<ChatMessage>>>,
    }

    impl RecordingMockClient {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                responses: std::sync::Mutex::new(responses.into()),
                sent: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn sent_messages(&self) -> Vec<Vec<ChatMessage>> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl LlmClient for RecordingMockClient {
        fn chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError> {
            self.sent.lock().unwrap().push(messages.to_vec());
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| LlmError::Network("mock exhausted".into()))
        }
    }

    #[test]
    fn llm_planner_succeeds_on_valid_json() {
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"write_file","path":"a.py","content":"x"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 a.py".into());
        let ctx = StepContext::new(ws.path(), task);
        let planner = LlmPlanner::new(client);
        let out = planner.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
    }

    #[test]
    fn llm_planner_fails_on_invalid_json() {
        let client = MockLlmClient::new(vec!["not json".into()]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let planner = LlmPlanner::new(client);
        let out = planner.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("解析"));
    }

    #[test]
    fn llm_worker_writes_files_from_response() {
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":"hello.py","content":"print('hi')\n"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py".into());
        // 加一个 planner artifact 让 worker 找到 prior。
        let plan_artifact = Artifact {
            artifact_id: "ART-planner-001".into(),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-planner".into(),
                name: "planner".into(),
                agent: "planner".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![plan_artifact],
        );
        let worker = LlmWorker::new(client);
        let out = worker.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        let hello = ws.path().join("hello.py");
        assert!(hello.is_file());
        assert!(std::fs::read_to_string(&hello).unwrap().contains("print"));
    }

    #[test]
    fn llm_worker_rejects_path_traversal() {
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":"../evil.py","content":"x"}]}"#.into()
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let worker = LlmWorker::new(client);
        let out = worker.run(&ctx);
        assert!(!out.result.success);
        // M4：错误消息由 PathGuard 产，统一前缀「写边界拒绝」+ 具体原因（含非法组件 / 逃逸 workspace 等）。
        assert!(
            out.result.summary.contains("写边界拒绝"),
            "expected summary to mention write boundary rejection, got: {}",
            out.result.summary
        );
    }

    // ---- M4 写边界验收：LlmWorker 端到端 ----

    #[test]
    fn m4_llm_worker_rejects_writing_to_git_hooks() {
        // 危险路径黑名单：即便 LLM 输出 .git/hooks/pre-commit，也拒绝写入。
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":".git/hooks/pre-commit","content":"evil"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("写边界拒绝"),
            "got: {}",
            out.result.summary
        );
        assert!(
            out.result.summary.contains(".git/hooks/pre-commit"),
            "should name the offending path, got: {}",
            out.result.summary
        );
        // 文件未被写入。
        assert!(!ws.path().join(".git/hooks/pre-commit").exists());
    }

    #[test]
    fn m4_llm_worker_rejects_writing_env_file() {
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":".env","content":"SECRET=leaked"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("写边界拒绝"));
        assert!(!ws.path().join(".env").exists());
    }

    #[test]
    fn m4_llm_worker_rejects_writing_github_workflow() {
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":".github/workflows/ci.yml","content":"on: [push]"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("写边界拒绝"));
    }

    #[test]
    fn m4_llm_worker_rejects_absolute_path() {
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":"/etc/passwd","content":"x"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("写边界拒绝"));
    }

    #[test]
    fn m4_llm_worker_allows_normal_create() {
        // 正常路径：写 src/hello.py，应通过。
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":"src/hello.py","content":"print('hi')"}]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("src/hello.py")).unwrap(),
            "print('hi')\n"
        );
    }

    // ---- M4 写边界验收：LlmReviewer diff 范围 ----

    #[test]
    fn m4_llm_reviewer_rejects_off_scope_patch() {
        // task 声明写 hello.py，但 Worker artifact 的 patch 改了 evil.py。
        // Reviewer 应在调 LLM 前就拒绝（diff scope 越界）。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("evil.py"), "x\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 x".into());
        let evil_patch = "diff --git a/evil.py b/evil.py\nnew file mode 100644\n--- /dev/null\n+++ b/evil.py\n@@ -0,0 +1,1 @@\n+x\n";
        let diff_artifact = Artifact {
            artifact_id: "ART-worker-001".into(),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: None,
            patch: Some(evil_patch.into()),
            url: Some(format!("file://{}/evil.py", ws.path().display())),
        };
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![diff_artifact],
        );
        // LLM 不应被调用——确定性 diff scope 检查先拒绝。
        let client = MockLlmClient::new(vec![r#"{"approved":true,"issues":[]}"#.into()]);
        let out = LlmReviewer::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("diff 范围越界"),
            "expected scope rejection, got: {}",
            out.result.summary
        );
    }

    #[test]
    fn m4_llm_reviewer_allows_in_scope_patch() {
        // task 声明写 hello.py，patch 也只改 hello.py：应交 LLM 审。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("hello.py"), "print('hi')\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 print('hi')".into());
        let patch = "diff --git a/hello.py b/hello.py\nnew file mode 100644\n--- /dev/null\n+++ b/hello.py\n@@ -0,0 +1,1 @@\n+print('hi')\n";
        let diff_artifact = Artifact {
            artifact_id: "ART-worker-001".into(),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: None,
            patch: Some(patch.into()),
            url: Some(format!("file://{}/hello.py", ws.path().display())),
        };
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![diff_artifact],
        );
        let client = MockLlmClient::new(vec![r#"{"approved":true,"issues":[]}"#.into()]);
        let out = LlmReviewer::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
    }

    #[test]
    fn llm_reviewer_approves_clean_files() {
        // 预置 Worker 已写 hello.py。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("hello.py"), "print('hi')\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py".into());
        let diff_artifact = Artifact {
            artifact_id: "ART-worker-001".into(),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: None,
            patch: Some("diff".into()),
            url: Some(format!("file://{}/hello.py", ws.path().display())),
        };
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![diff_artifact],
        );
        let client = MockLlmClient::new(vec![r#"{"approved":true,"issues":[]}"#.into()]);
        let reviewer = LlmReviewer::new(client);
        let out = reviewer.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
    }

    #[test]
    fn llm_cycleround_end_to_end_with_mock_llm() {
        if crate::sub_agents::find_python().is_none() {
            eprintln!("skipping: no python");
            return;
        }
        // mock LLM 让 Worker 同时写 hello.py 和 test.py（断言 hello.py 内容）。
        // 这样确定性 Tester 能跑 test.py 通过，Reviewer（LLM）审核通过。
        let client = MockLlmClient::new(vec![
            // 第 1 轮：planner / worker / reviewer
            r#"{"steps":[{"action":"write_file","path":"hello.py","content":"print('hello')"}]}"#.into(),
            r#"{"files":[{"path":"hello.py","content":"print('hello')\n"},{"path":"test.py","content":"assert open('hello.py').read().strip() == \"print('hello')\"\n"}]}"#.into(),
            r#"{"approved":true,"issues":[]}"#.into(),
        ]);
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-llm-1".into(), "创建 hello.py 打印 hello".into());
        let cycle = LlmCycleround::new(
            CycleConfig {
                max_rounds: 3,
                max_retries: 3,
                cool_down: Duration::from_secs(0),
            },
            client,
        );
        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success { rounds, .. } => {
                assert_eq!(rounds, 1, "应在第 1 轮成功，实际 {rounds}");
                let hello = ws.path().join("hello.py");
                assert!(hello.is_file());
                assert!(ws.path().join("test.py").is_file());
            }
            other => panic!("expected Success, got {other:?}"),
        }
    }

    // ============================================================
    // D2 Memory 层测试
    // ============================================================

    #[test]
    fn planner_with_memory_appends_entry_on_call() {
        // 单测：LlmPlanner 注入 memory 后，调用一次 run_at 应在 memory 留 1 条。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileMemoryStore::new(dir.path()));
        store.init().unwrap();

        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"write_file","path":"a.py","content":"x"}]}"#.into(),
        ]);
        let planner = LlmPlanner::new(client).with_memory(store.clone());

        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-mem-1".into(), "创建 a.py".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = planner.run_at(&ctx, 1);
        assert!(out.result.success, "summary: {}", out.result.summary);

        let entries = store.list("T-mem-1").unwrap();
        assert_eq!(entries.len(), 1, "memory 应有 1 条");
        assert_eq!(entries[0].round, 1);
        assert_eq!(entries[0].agent, "planner");
        assert_eq!(entries[0].role, "assistant");
        assert!(entries[0].content.contains("steps"));
    }

    #[test]
    fn planner_without_memory_does_not_panic() {
        // 不注入 memory 时，run_at 应正常工作，不写任何东西。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileMemoryStore::new(dir.path()));
        store.init().unwrap();

        let client = MockLlmClient::new(vec![r#"{"steps":[]}"#.into()]);
        let planner = LlmPlanner::new(client); // 无 memory

        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-mem-2".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let _ = planner.run_at(&ctx, 1);

        // memory 目录应不存在该 task 的文件。
        assert!(store.list("T-mem-2").unwrap().is_empty());
    }

    #[test]
    fn planner_injects_prior_memory_into_prompt() {
        // 预置 memory：round 1 的 reviewer 拒绝原因。
        // 调用 planner.run_at，断言发给 LLM 的消息中包含该拒绝原因。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileMemoryStore::new(dir.path()));
        store.init().unwrap();
        store
            .append(
                "T-ctx",
                &MemoryEntry {
                    round: 1,
                    agent: "reviewer".into(),
                    role: "assistant".into(),
                    content: r#"{"approved":false,"issues":["hello.py 缺少换行"]}"#.into(),
                },
            )
            .unwrap();

        let client = RecordingMockClient::new(vec![r#"{"steps":[]}"#.into()]);
        let planner = LlmPlanner::new(client.clone()).with_memory(store);

        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-ctx".into(), "创建 hello.py".into());
        let ctx = StepContext::new(ws.path(), task);
        let _ = planner.run_at(&ctx, 2);

        let sent = client.sent_messages();
        assert_eq!(sent.len(), 1, "应调用 LLM 1 次");
        // 应有 3 条消息：system / user(task) / user(memory history)
        assert_eq!(sent[0].len(), 3, "应注入 memory 历史作为第 3 条消息");
        let memory_msg = &sent[0][2];
        assert_eq!(memory_msg.role, "user");
        assert!(
            memory_msg.content.contains("hello.py 缺少换行"),
            "memory 历史应包含前序 reviewer 的拒绝原因: {}",
            memory_msg.content
        );
        assert!(
            memory_msg.content.contains("[round 1][reviewer]"),
            "应标注来源 agent 与 round: {}",
            memory_msg.content
        );
    }

    #[test]
    fn empty_memory_not_injected_into_prompt() {
        // memory 为空时不应注入第 3 条消息（避免无意义 noise）。
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FileMemoryStore::new(dir.path()));
        store.init().unwrap();
        // 不写任何 entry。

        let client = RecordingMockClient::new(vec![r#"{"steps":[]}"#.into()]);
        let planner = LlmPlanner::new(client.clone()).with_memory(store);

        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-empty".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let _ = planner.run_at(&ctx, 1);

        let sent = client.sent_messages();
        assert_eq!(sent[0].len(), 2, "空 memory 不应注入第 3 条消息");
    }

    #[test]
    fn cycleround_with_memory_persists_conversation_across_rounds() {
        // e2e：2 轮场景。
        // round 1: worker 只写 hello.py（无 test.py）→ tester 失败 → fixer 解析不了（"打印"非"输出"）→ fix_attempts=1
        // round 2: worker 写 hello.py + test.py → tester 通过 → reviewer 通过 → Success
        // 断言 memory 在 run 后有 5 条（planner×2 + worker×2 + reviewer×1），
        // 且 round 2 的 planner prompt 包含 round 1 的 planner/worker 输出。
        if crate::sub_agents::find_python().is_none() {
            eprintln!("skipping: no python");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(FileMemoryStore::new(dir.path()));
        memory.init().unwrap();

        let client = RecordingMockClient::new(vec![
            // round 1
            r#"{"steps":[{"action":"write_file","path":"hello.py","content":"print('hello')"}]}"#.into(),
            r#"{"files":[{"path":"hello.py","content":"print('hello')\n"}],"summary":"wrote hello"}"#.into(),
            // round 2
            r#"{"steps":[{"action":"write_file","path":"hello.py","content":"print('hello')"}]}"#.into(),
            r#"{"files":[{"path":"hello.py","content":"print('hello')\n"},{"path":"test.py","content":"assert open('hello.py').read().strip() == \"print('hello')\"\n"}],"summary":"added test"}"#.into(),
            r#"{"approved":true,"issues":[]}"#.into(),
        ]);

        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-multi".into(), "实现 hello.py 打印 hello".into());
        let cycle = LlmCycleround::with_memory(
            CycleConfig {
                max_rounds: 3,
                max_retries: 3,
                cool_down: Duration::from_secs(0),
            },
            client.clone(),
            memory.clone(),
        );
        let outcome = cycle.run(&task, ws.path());
        match outcome {
            CycleOutcome::Success { rounds, .. } => {
                assert_eq!(rounds, 2, "应在第 2 轮成功，实际 {rounds}");
            }
            other => panic!("expected Success, got {other:?}"),
        }

        // memory 应有 5 条：planner r1 / worker r1 / planner r2 / worker r2 / reviewer r2
        let entries = memory.list("T-multi").unwrap();
        assert_eq!(entries.len(), 5, "memory 应有 5 条，实际 {}", entries.len());
        let agents: Vec<&str> = entries.iter().map(|e| e.agent.as_str()).collect();
        assert_eq!(
            agents,
            vec!["planner", "worker", "planner", "worker", "reviewer"]
        );
        assert_eq!(entries[0].round, 1);
        assert_eq!(entries[2].round, 2);
        assert_eq!(entries[4].round, 2);

        // round 2 的 planner（第 3 次 LLM 调用）应看到 round 1 的 planner/worker 输出。
        let sent = client.sent_messages();
        assert_eq!(sent.len(), 5, "应共调用 LLM 5 次");
        let planner_r2_msg = &sent[2];
        assert!(
            planner_r2_msg.len() >= 3,
            "round 2 planner prompt 应注入 memory 历史"
        );
        let memory_part = &planner_r2_msg[2].content;
        assert!(
            memory_part.contains("[round 1][planner]"),
            "round 2 prompt 应包含 round 1 planner 输出"
        );
        assert!(
            memory_part.contains("[round 1][worker]"),
            "round 2 prompt 应包含 round 1 worker 输出"
        );
    }
}
