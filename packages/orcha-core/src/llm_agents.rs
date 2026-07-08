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
    build_reviewer_prompt, parse_planner_output, parse_reviewer_output,
    parse_worker_output_with_steps, ChatMessage, LlmClient, WorkerOutput, PLANNER_SYSTEM,
    WORKER_SYSTEM,
};
use orcha_sdk::{Artifact, ArtifactType, Task};

use crate::cycleround::{build_round, persist_round};
use crate::history::HistoryStore;
use crate::memory::{MemoryEntry, MemoryStore};
use crate::path_guard::PathGuard;
use crate::plan::{apply_step, check_diff_scope, extract_changed_files, PlanAction, PlanStep};
use crate::sub_agent::{StepContext, StepOutput, SubAgent};
use crate::sub_agents::{list_workspace_files, Fixer, Observer, Tester};
use crate::tools::{readonly_tools, run_agent_loop, MAX_TOOL_TURNS};
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

        let _guard = match PathGuard::new(&ctx.workspace) {
            Ok(g) => g,
            Err(e) => {
                return StepOutput::failure("S-planner", format!("PathGuard 初始化失败: {e}"));
            }
        };

        let mut msgs = vec![
            ChatMessage::system(PLANNER_SYSTEM),
            ChatMessage::user(format!(
                "任务：{}\n\n\
                 workspace 摘要（Observer 自动检测）：\n{files_str}\n\n\
                 注意：以上摘要已标明项目类型和语言，你只能在该技术栈范围内制定计划。\n\
                 直接输出 JSON 计划，不要调用工具，不要输出解释性文字。",
                ctx.task.description
            )),
        ];
        inject_memory(&mut msgs, &self.memory, &ctx.task.id);

        // Planner 直接用 chat()，不传 tools，避免 DeepSeek 进入 tool-calling 模式
        // 输出 XML 而非 JSON 计划。
        let resp = match self.client.chat(&msgs) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-planner", format!("LLM 调用失败: {e}"));
            }
        };

        append_memory(&self.memory, &ctx.task.id, round, "planner", &resp);

        match parse_planner_output(&resp) {
            Ok(plan_json) => {
                // 把 plan JSON 原文存到 artifact.patch，Worker 可以直接读到完整计划
                let artifact = Artifact {
                    artifact_id: next_artifact_id(&ctx.prior_artifacts, "planner"),
                    artifact_type: ArtifactType::Report,
                    commit_sha: None,
                    patch: Some(plan_json.clone()),
                    url: None,
                };
                StepOutput::success(
                    "S-planner",
                    format!("LLM 计划已生成（{} 字节）", plan_json.len()),
                )
                .with_artifacts(vec![artifact])
            }
            Err(first_err) => {
                // 重试一次：把解析错误喂回 LLM，让它修正 JSON 格式
                let retry_msgs = vec![
                    ChatMessage::system(PLANNER_SYSTEM),
                    ChatMessage::user(format!(
                        "你上一次的输出 JSON 解析失败，错误信息：{first_err}\n\n\
                         请检查 JSON 格式，确保：\n\
                         1. 字符串值中的换行符用 \\\\n 转义，不得出现字面换行\n\
                         2. 不要在 JSON 前后输出任何解释文字\n\
                         3. 第一个字符必须是 '{{'\n\n\
                         任务：{}\n当前 workspace 文件：{files_str}\n\n\
                         请重新输出正确的 JSON 计划。",
                        ctx.task.description
                    )),
                ];
                let retry_resp = match self.client.chat(&retry_msgs) {
                    Ok(s) => s,
                    Err(e) => {
                        return StepOutput::failure(
                            "S-planner",
                            format!("LLM 重试调用失败: {e}（首次错误: {first_err}）"),
                        );
                    }
                };
                append_memory(&self.memory, &ctx.task.id, round, "planner", &retry_resp);
                match parse_planner_output(&retry_resp) {
                    Ok(plan_json) => {
                        let artifact = Artifact {
                            artifact_id: next_artifact_id(&ctx.prior_artifacts, "planner"),
                            artifact_type: ArtifactType::Report,
                            commit_sha: None,
                            patch: Some(plan_json.clone()),
                            url: None,
                        };
                        StepOutput::success(
                            "S-planner",
                            format!("LLM 计划已生成（{} 字节，重试后成功）", plan_json.len()),
                        )
                        .with_artifacts(vec![artifact])
                    }
                    Err(retry_err) => {
                        StepOutput::failure(
                            "S-planner",
                            format!(
                                "解析 LLM 输出失败（重试后仍失败）: {retry_err}; 首次错误: {first_err}"
                            ),
                        )
                    }
                }
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
        // 从 Planner artifact 中提取 plan JSON 原文。
        // patch 字段存储的是 Planner 输出的完整 JSON 计划（含 target_files + steps）。
        let plan_json: Option<String> = ctx
            .prior_artifacts
            .iter()
            .rev()
            .find(|a| a.artifact_id.contains("planner"))
            .and_then(|a| a.patch.clone());

        let plan_section = match &plan_json {
            Some(json) => format!(
                "Planner 的详细计划（JSON）：\n```json\n{json}\n```\n\n\
                 请严格按照以上计划执行：每个 step 的 action / path / search / replace / content 都已指定。\
                 如果某 step 是 edit 但目标文件不存在，改为 create 并用 replace 作为 content。"
            ),
            None => "(无前序 plan，请基于任务描述和工具探查自行决定)。".to_string(),
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
                "任务：{}\n\n{plan_section}\n\n先用工具读取需要修改的文件，确认内容后再输出 JSON。",
                ctx.task.description
            )),
        ];
        inject_memory(&mut msgs, &self.memory, &ctx.task.id);

        // Worker 只用只读工具（read_file/grep/glob/list_dir），不能跑命令。
        // 编译验证是 Tester 的职责——Worker 的任务是读文件、理解代码、输出改动 JSON。
        let tools = readonly_tools();
        let resp = match run_agent_loop(
            self.client.as_ref(),
            msgs,
            &tools,
            &ctx.workspace,
            &guard,
            "worker",
            None,
            MAX_TOOL_TURNS,
            Some(ctx.approval.as_ref()),
        ) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-worker", format!("LLM 调用失败: {e}"));
            }
        };

        append_memory(&self.memory, &ctx.task.id, round, "worker", &resp);

        // 优先解析 steps（edit/create/delete，推荐）；无 steps 时回退 files（旧整文件格式）。
        // 解析失败时重试一次：把错误喂回 LLM 让它修正 JSON。
        let output = match parse_worker_output_with_steps(&resp) {
            Ok(o) => o,
            Err(first_err) => {
                let retry_msgs = vec![
                    ChatMessage::system(WORKER_SYSTEM),
                    ChatMessage::user(format!(
                        "你上一次的输出 JSON 解析失败，错误信息：{first_err}\n\n\
                         请检查 JSON 格式，确保：\n\
                         1. 字符串值中的换行符用 \\\\n 转义，不得出现字面换行\n\
                         2. 不要在 JSON 前后输出解释文字，第一个字符必须是 '{{'\n\
                         3. 使用 edit（search/replace）修改已存在文件\n\n\
                         任务：{}\n{plan_section}\n\n\
                         请重新输出正确的 JSON。",
                        ctx.task.description
                    )),
                ];
                let retry_resp = match self.client.chat(&retry_msgs) {
                    Ok(s) => s,
                    Err(e) => {
                        return StepOutput::failure(
                            "S-worker",
                            format!("LLM 重试调用失败: {e}（首次错误: {first_err}）"),
                        );
                    }
                };
                append_memory(&self.memory, &ctx.task.id, round, "worker", &retry_resp);
                match parse_worker_output_with_steps(&retry_resp) {
                    Ok(o) => o,
                    Err(retry_err) => {
                        return StepOutput::failure(
                            "S-worker",
                            format!(
                                "解析 LLM 输出失败（重试后仍失败）: {retry_err}; 首次错误: {first_err}"
                            ),
                        );
                    }
                }
            }
        };

        // target_files 由 LLM 输出派生：steps 取 step.path，files 取 file.0。
        let target_files: Vec<String> = match &output {
            WorkerOutput::Steps(steps) => steps.iter().map(|s| s.path.clone()).collect(),
            WorkerOutput::Files(files) => files.iter().map(|(p, _)| p.clone()).collect(),
        };

        let mut artifacts = Vec::new();
        let mut written: Vec<String> = Vec::new();
        let mut combined_patch = String::new();

        match output {
            WorkerOutput::Steps(steps) => {
                if steps.is_empty() {
                    return StepOutput::failure("S-worker", "LLM 未产出任何 step");
                }
                for ws in &steps {
                    let plan_action = match ws.action.as_str() {
                        "edit" => PlanAction::Edit,
                        "create" => PlanAction::Create,
                        "delete" => PlanAction::Delete,
                        other => {
                            return StepOutput::failure(
                                "S-worker",
                                format!(
                                    "未知 action {}: {}（仅支持 edit/create/delete）",
                                    other, ws.path
                                ),
                            );
                        }
                    };
                    // 路径校验：edit/create 走 validate_write，delete 走 validate_delete。
                    let path_check = match plan_action {
                        PlanAction::Create | PlanAction::Edit => {
                            guard.validate_write(&ws.path, &target_files)
                        }
                        PlanAction::Delete => guard.validate_delete(&ws.path, &target_files),
                    };
                    let resolved = match path_check {
                        Ok(p) => p,
                        Err(e) => {
                            return StepOutput::failure(
                                "S-worker",
                                format!("写边界拒绝 {}: {e}", ws.path),
                            );
                        }
                    };
                    // M7 P1：人工审批 hook。preview 取 content 或 replace 前 200 字符。
                    let preview: String = ws
                        .content
                        .as_deref()
                        .or(ws.replace.as_deref())
                        .map(|s| s.chars().take(200).collect())
                        .unwrap_or_default();
                    let action = crate::approval::ApprovalAction::WriteFile {
                        path: ws.path.clone(),
                        content_preview: preview,
                    };
                    match ctx.approval.request(&action) {
                        crate::approval::ApprovalDecision::Approved
                        | crate::approval::ApprovalDecision::ApproveAndWhitelist => {}
                        crate::approval::ApprovalDecision::Rejected(reason) => {
                            return StepOutput::failure(
                                "S-worker",
                                format!("人工审批拒绝写 {}: {reason}", ws.path),
                            );
                        }
                    }
                    // 转 PlanStep 调 apply_step：复用 plan.rs 的 search/replace 唯一匹配校验
                    // 与 unified diff 生成（M4 已实现并测试）。
                    let step = PlanStep {
                        action: plan_action,
                        path: ws.path.clone(),
                        content: ws.content.clone(),
                        search: ws.search.clone(),
                        replace: ws.replace.clone(),
                    };
                    let applied = match apply_step(&ctx.workspace, &step) {
                        Ok(a) => a,
                        Err(e) => {
                            // 自动修复：edit 失败因为文件不存在 → 降级为 create
                            let err_msg = e.to_string();
                            if plan_action == PlanAction::Edit
                                && err_msg.contains("文件不存在")
                                && ws.replace.is_some()
                            {
                                eprintln!(
                                    "[worker] edit 失败（文件不存在），自动降级为 create: {}",
                                    ws.path
                                );
                                let create_step = PlanStep {
                                    action: PlanAction::Create,
                                    path: ws.path.clone(),
                                    content: ws.replace.clone(),
                                    search: None,
                                    replace: None,
                                };
                                match apply_step(&ctx.workspace, &create_step) {
                                    Ok(a) => a,
                                    Err(e2) => {
                                        return StepOutput::failure(
                                            "S-worker",
                                            format!(
                                                "apply_step 失败 {} (降级 create 仍失败): {e2}",
                                                ws.path
                                            ),
                                        );
                                    }
                                }
                            } else {
                                return StepOutput::failure(
                                    "S-worker",
                                    format!(
                                        "apply_step 失败 {} ({:?}): {e}",
                                        ws.path, plan_action
                                    ),
                                );
                            }
                        }
                    };
                    combined_patch.push_str(&applied.diff);
                    artifacts.push(Artifact {
                        artifact_id: next_artifact_id(&ctx.prior_artifacts, "worker"),
                        artifact_type: ArtifactType::CodeDiff,
                        commit_sha: None,
                        patch: Some(applied.diff),
                        url: Some(format!("file:///{}", resolved.display())),
                    });
                    written.push(ws.path.clone());
                }
            }
            WorkerOutput::Files(files) => {
                if files.is_empty() {
                    return StepOutput::failure("S-worker", "LLM 未产出任何文件");
                }
                for (path, content) in &files {
                    // M4：统一走 PathGuard::validate_write，强制写边界。
                    let resolved = match guard.validate_write(path, &target_files) {
                        Ok(p) => p,
                        Err(e) => {
                            return StepOutput::failure(
                                "S-worker",
                                format!("写边界拒绝 {path}: {e}"),
                            );
                        }
                    };
                    // M7 P1：PathGuard 之后、写文件之前调人工审批 hook。
                    let preview: String = content.chars().take(200).collect();
                    let action = crate::approval::ApprovalAction::WriteFile {
                        path: path.clone(),
                        content_preview: preview,
                    };
                    match ctx.approval.request(&action) {
                        crate::approval::ApprovalDecision::Approved
                        | crate::approval::ApprovalDecision::ApproveAndWhitelist => {}
                        crate::approval::ApprovalDecision::Rejected(reason) => {
                            return StepOutput::failure(
                                "S-worker",
                                format!("人工审批拒绝写 {path}: {reason}"),
                            );
                        }
                    }
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
            }
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
        let combined_patch: String = worker_artifacts
            .iter()
            .filter_map(|a| a.patch.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        // target_files 派生优先级：
        // 1. 从 Planner plan JSON 中提取 target_files（新路径，M7）
        // 2. 从 task.description 解析（旧格式兼容）
        // 3. 上述都失败：从 patch 本身提取文件列表作为回退白名单
        let mut target_files = extract_target_files_from_plan(&ctx.prior_artifacts);
        if target_files.is_empty() {
            target_files = derive_target_files_from_task(&ctx.task.description);
        }
        if target_files.is_empty() {
            // 回退：从 patch 中提取改动的文件路径，至少确保不超出 Worker 自声明范围
            target_files = extract_changed_files(&combined_patch);
        }
        if !target_files.is_empty() {
            if let Err(e) = check_diff_scope(&combined_patch, &target_files) {
                return StepOutput::failure("S-reviewer", format!("diff 范围越界: {e}"));
            }
        }

        let worker_files: Vec<(String, String)> = worker_artifacts
            .iter()
            .filter_map(|a| {
                let u = a.url.as_ref()?;
                // 兼容 file:///（标准）和 file://（Windows UNC）两种前缀。
                // Windows worktree 路径可能是 \\?\C:\... 形式，url 构造时
                // file:/// + display() 产生 file:///\\?\C:\...，
                // strip "file:///" 后得到 \\?\C:\... 才是有效路径。
                let p = u
                    .strip_prefix("file:///")
                    .or_else(|| u.strip_prefix("file://"))?;
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

/// 从 task.description 和 Planner plan artifact 中派生 target_files 期望集合。
///
/// 优先级：
/// 1. 优先从 Planner artifact 的 patch 中解析 plan JSON，提取 target_files
/// 2. 兼容 M2/M3 旧格式 `"创建 <filename> 输出 <content>"`：派生为 `[filename]`
/// 3. 上述两种都失败时：从 Worker 本身的 diff 中提取改动文件列表作为回退白名单
///    （至少能校验 patch 没有超出 Worker 自己声明的范围）
fn derive_target_files_from_task(desc: &str) -> Vec<String> {
    // 兼容旧格式
    if let Some(files) = try_parse_old_format(desc) {
        return files;
    }
    Vec::new()
}

/// 从 Planner artifact 的 plan JSON 中提取 target_files。
///
/// Planner 把 plan JSON 存在 `artifact.patch` 字段（M7）。
/// 解析出 `target_files` 数组作为 Reviewer scope check 的白名单。
fn extract_target_files_from_plan(artifacts: &[Artifact]) -> Vec<String> {
    for a in artifacts.iter().rev() {
        if !a.artifact_id.contains("planner") {
            continue;
        }
        if let Some(json_str) = &a.patch {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(arr) = v.get("target_files").and_then(|f| f.as_array()) {
                    let files: Vec<String> = arr
                        .iter()
                        .filter_map(|f| f.as_str().map(String::from))
                        .collect();
                    if !files.is_empty() {
                        return files;
                    }
                }
            }
        }
    }
    Vec::new()
}

/// 从 task description 解析 M2/M3 旧格式 "创建 <filename> 输出 <content>"。
fn try_parse_old_format(desc: &str) -> Option<Vec<String>> {
    let desc = desc.trim();
    let after = desc
        .strip_prefix("创建 ")
        .or_else(|| desc.strip_prefix("create "))?;
    let output_idx = after.find(" 输出 ").or_else(|| after.find(" output "))?;
    let filename = after[..output_idx].trim().to_string();
    if filename.is_empty() {
        return None;
    }
    Some(vec![filename])
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

    // ============================================================
    // #2 search/replace edit action 端到端测试
    // ============================================================

    /// 辅助：构造一个带 planner artifact 的 StepContext。
    fn worker_ctx_with_plan(workspace: &Path, task_desc: &str) -> StepContext {
        let task = Task::new("T-edit".into(), task_desc.into());
        let plan_artifact = Artifact {
            artifact_id: "ART-planner-001".into(),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };
        StepContext::new(workspace, task).with_prior(
            orcha_sdk::Step {
                id: "S-planner".into(),
                name: "planner".into(),
                agent: "planner".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![plan_artifact],
        )
    }

    #[test]
    fn llm_worker_applies_edit_step() {
        // 预置 src/hello.py 存在，LLM 输出 edit step 把 print('hi') → print('hello')。
        let ws = tempfile::tempdir().unwrap();
        let hello = ws.path().join("hello.py");
        std::fs::create_dir_all(ws.path().join("src")).unwrap();
        std::fs::write(&hello, "print('hi')\n").unwrap();

        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"edit","path":"hello.py","search":"print('hi')","replace":"print('hello')"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "修改 hello.py 打印 hello");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        let updated = std::fs::read_to_string(&hello).unwrap();
        assert_eq!(updated, "print('hello')\n");
        // 旧内容应被替换，不应残留。
        assert!(!updated.contains("print('hi')"));
    }

    #[test]
    fn llm_worker_applies_create_step() {
        // workspace 中没有 new.py，LLM 输出 create step。
        let ws = tempfile::tempdir().unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"create","path":"new.py","content":"print(1)\n"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "创建 new.py");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        let content = std::fs::read_to_string(ws.path().join("new.py")).unwrap();
        assert_eq!(content, "print(1)\n");
    }

    #[test]
    fn llm_worker_applies_delete_step() {
        // 预置 old.py 存在，LLM 输出 delete step。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("old.py"), "deprecated\n").unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"delete","path":"old.py"}]}"#.into()
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "删除 old.py");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert!(!ws.path().join("old.py").exists(), "old.py 应被删除");
    }

    #[test]
    fn llm_worker_applies_multiple_steps_in_order() {
        // 一个 LLM 输出含 3 个 step：edit a.py + create b.py + delete c.py。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.py"), "x = 1\n").unwrap();
        std::fs::write(ws.path().join("c.py"), "to remove\n").unwrap();

        let client = MockLlmClient::new(vec![r#"{"steps":[
                {"action":"edit","path":"a.py","search":"x = 1","replace":"x = 2"},
                {"action":"create","path":"b.py","content":"new\n"},
                {"action":"delete","path":"c.py"}
            ]}"#
        .into()]);
        let ctx = worker_ctx_with_plan(ws.path(), "重构 a/b/c");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("a.py")).unwrap(),
            "x = 2\n"
        );
        assert_eq!(
            std::fs::read_to_string(ws.path().join("b.py")).unwrap(),
            "new\n"
        );
        assert!(!ws.path().join("c.py").exists());
    }

    #[test]
    fn llm_worker_edit_fails_when_search_not_found() {
        // search 字符串在原文件中不存在 → apply_step 应失败。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.py"), "print('hello')\n").unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"edit","path":"a.py","search":"no_such_text","replace":"x"}]}"#
                .into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("apply_step 失败"),
            "应报 apply_step 失败, got: {}",
            out.result.summary
        );
        // 文件未被改动。
        assert_eq!(
            std::fs::read_to_string(ws.path().join("a.py")).unwrap(),
            "print('hello')\n"
        );
    }

    #[test]
    fn llm_worker_edit_fails_when_search_matches_multiple() {
        // search 字符串在原文件中匹配多次 → apply_step 应失败（必须唯一）。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.py"), "x = 1\nx = 1\n").unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"edit","path":"a.py","search":"x = 1","replace":"x = 2"}]}"#
                .into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("apply_step 失败"),
            "应报 apply_step 失败（多次匹配）, got: {}",
            out.result.summary
        );
    }

    #[test]
    fn llm_worker_steps_reject_path_traversal() {
        // steps 中的 path 含 .. 应被 PathGuard 拦下。
        let ws = tempfile::tempdir().unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"create","path":"../evil.py","content":"x"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("写边界拒绝"),
            "应拒绝路径越界, got: {}",
            out.result.summary
        );
        assert!(!ws.path().join("../evil.py").exists());
    }

    #[test]
    fn llm_worker_steps_reject_dangerous_path() {
        // steps 中 path 是 .git/hooks/pre-commit 应被危险路径黑名单拒绝。
        let ws = tempfile::tempdir().unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"create","path":".git/hooks/pre-commit","content":"evil"}]}"#
                .into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("写边界拒绝"));
    }

    #[test]
    fn llm_worker_unknown_action_rejected() {
        // action 不是 edit/create/delete 之一应失败。
        let ws = tempfile::tempdir().unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"append","path":"a.py","content":"x"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(!out.result.success);
        assert!(
            out.result.summary.contains("未知 action"),
            "应报未知 action, got: {}",
            out.result.summary
        );
    }

    #[test]
    fn llm_worker_files_format_still_works() {
        // 兼容旧 {files:[...]} 格式：LlmWorker 应走整文件写路径。
        let ws = tempfile::tempdir().unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"files":[{"path":"legacy.py","content":"print('old')\n"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert_eq!(
            std::fs::read_to_string(ws.path().join("legacy.py")).unwrap(),
            "print('old')\n"
        );
    }

    #[test]
    fn llm_worker_edit_produces_unified_diff_artifact() {
        // edit step 产出的 artifact.patch 应是 unified diff 格式（含 @@ hunk 头）。
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("a.py"), "print('hi')\n").unwrap();
        let client = MockLlmClient::new(vec![
            r#"{"steps":[{"action":"edit","path":"a.py","search":"print('hi')","replace":"print('hello')"}]}"#.into(),
        ]);
        let ctx = worker_ctx_with_plan(ws.path(), "x");
        let out = LlmWorker::new(client).run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        // 至少一个 CodeDiff artifact，patch 含 @@ 标记。
        let diff_artifacts: Vec<_> = out
            .artifacts
            .iter()
            .filter(|a| a.artifact_type == ArtifactType::CodeDiff)
            .collect();
        assert!(!diff_artifacts.is_empty(), "应有 CodeDiff artifact");
        let patch = diff_artifacts[0].patch.as_ref().expect("patch 不为空");
        assert!(
            patch.contains("@@"),
            "patch 应含 unified diff hunk 头, got: {patch}"
        );
        assert!(patch.contains("-print('hi')"), "patch 应含删除行: {patch}");
        assert!(
            patch.contains("+print('hello')"),
            "patch 应含新增行: {patch}"
        );
    }
}
