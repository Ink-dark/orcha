//! LLM 驱动的 Sub-Agent 集合（feature = "llm"）。
//!
//! 与确定性 `sub_agents` 并存：Observer/Tester/Fixer 复用确定性实现
//! （它们不需要 LLM——Observer 列文件，Tester 跑命令，Fixer 检测测试框架）。
//! Planner/Worker/Reviewer 改调真 LLM。
//!
//! [`LlmCycleround`] 跑与 [`crate::Cycleround`] 相同的 Plan→Code→Test→Review→Fix
//! 闭环，但 Planner/Worker/Reviewer 用 LLM 版本。熔断配置同。
//!
//! 不引入异步运行时；LLM 调用同步阻塞，复用 ureq。

use std::path::Path;
use std::sync::Arc;

use orcha_llm::{
    build_planner_prompt, build_reviewer_prompt, build_worker_prompt, parse_planner_output,
    parse_reviewer_output, parse_worker_output, LlmClient,
};
use orcha_sdk::{Artifact, ArtifactType, Task};

use crate::cycleround::{build_round, persist_round};
use crate::history::HistoryStore;
use crate::sub_agent::{StepContext, StepOutput, SubAgent};
use crate::sub_agents::{list_workspace_files, Fixer, Observer, Tester};
use crate::{CycleConfig, CycleOutcome, FailureReason, RoundRecord};

/// LLM 驱动的 Planner：调 LLM 产出 JSON 计划。
pub struct LlmPlanner {
    client: Arc<dyn LlmClient>,
}

impl LlmPlanner {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self { client }
    }
}

impl SubAgent for LlmPlanner {
    fn name(&self) -> &'static str {
        "llm-planner"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let files = list_workspace_files(&ctx.workspace);
        let msgs = build_planner_prompt(&ctx.task.description, &files);
        let resp = match self.client.chat(&msgs) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-planner", format!("LLM 调用失败: {e}"));
            }
        };
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

/// LLM 驱动的 Worker：调 LLM 按 plan 产出文件并写入 workspace。
pub struct LlmWorker {
    client: Arc<dyn LlmClient>,
}

impl LlmWorker {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self { client }
    }
}

impl SubAgent for LlmWorker {
    fn name(&self) -> &'static str {
        "llm-worker"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        // 从前序 Planner artifact 拿到 plan（存在 patch 字段或从 summary 提取）。
        // 简化：直接重新让 LLM 产出 files（plan_text 作为上下文）。
        // 这里从 prior_artifacts 找最近一个 planner Report 的 id 作为 plan 引用。
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

        let msgs = build_worker_prompt(&ctx.task.description, &plan_summary);
        let resp = match self.client.chat(&msgs) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-worker", format!("LLM 调用失败: {e}"));
            }
        };

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

        let mut artifacts = Vec::new();
        let mut written = Vec::new();
        for (path, content) in &files {
            // 防穿越
            if path.contains("..") || path.starts_with('/') {
                return StepOutput::failure("S-worker", format!("非法路径: {path}"));
            }
            let target = ctx.workspace.join(path);
            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return StepOutput::failure("S-worker", format!("创建目录失败: {e}"));
                }
            }
            let normalized = ensure_trailing_newline(content);
            if let Err(e) = std::fs::write(&target, &normalized) {
                return StepOutput::failure("S-worker", format!("写文件失败 {path}: {e}"));
            }
            let patch = make_create_diff(path, &normalized).unwrap_or_default();
            artifacts.push(Artifact {
                artifact_id: next_artifact_id(&ctx.prior_artifacts, "worker"),
                artifact_type: ArtifactType::CodeDiff,
                commit_sha: None,
                patch: Some(patch),
                url: Some(format!("file:///{}", target.display())),
            });
            written.push(path.clone());
        }

        StepOutput::success(
            "S-worker",
            format!("LLM 写入 {} 个文件: {}", written.len(), written.join(", ")),
        )
        .with_artifacts(artifacts)
    }
}

/// LLM 驱动的 Reviewer：调 LLM 审核 Worker 产出。
pub struct LlmReviewer {
    client: Arc<dyn LlmClient>,
}

impl LlmReviewer {
    pub fn new(client: Arc<dyn LlmClient>) -> Self {
        Self { client }
    }
}

impl SubAgent for LlmReviewer {
    fn name(&self) -> &'static str {
        "llm-reviewer"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let worker_files: Vec<(String, String)> = ctx
            .prior_artifacts
            .iter()
            .rev()
            .filter(|a| a.artifact_type == ArtifactType::CodeDiff)
            .filter_map(|a| {
                let u = a.url.as_ref()?;
                let p = u.strip_prefix("file://")?;
                let path = Path::new(p);
                let rel = path.strip_prefix(&ctx.workspace).unwrap_or(path);
                let content = std::fs::read_to_string(path).unwrap_or_default();
                Some((rel.to_string_lossy().into_owned(), content))
            })
            .collect();

        if worker_files.is_empty() {
            return StepOutput::failure("S-reviewer", "无可审核的 Worker 产出");
        }

        let msgs = build_reviewer_prompt(&ctx.task.description, &worker_files);
        let resp = match self.client.chat(&msgs) {
            Ok(s) => s,
            Err(e) => {
                return StepOutput::failure("S-reviewer", format!("LLM 调用失败: {e}"));
            }
        };

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
}

impl LlmCycleround {
    pub fn new(config: CycleConfig, client: Arc<dyn LlmClient>) -> Self {
        Self {
            config,
            observer: Observer,
            planner: LlmPlanner::new(client.clone()),
            worker: LlmWorker::new(client.clone()),
            tester: Tester,
            reviewer: LlmReviewer::new(client),
            fixer: Fixer,
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

    /// 跑闭环并持久化 history。
    pub fn run_with_history(
        &self,
        task: &Task,
        workspace: &Path,
        history: &dyn HistoryStore,
    ) -> CycleOutcome {
        if let Err(e) = history.clear_history(&task.id) {
            eprintln!("warn: clear_history({}) failed: {}", task.id, e);
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
            let plan_out = self.planner.run(&ctx);
            steps.push(plan_out.result.clone());
            round_artifacts.extend(plan_out.artifacts);
            let planner_succeeded = plan_out.result.success;

            // Worker（LLM）
            let mut worker_succeeded = false;
            if planner_succeeded {
                let ctx =
                    StepContext::new(workspace, task.clone()).with_priors(&[], &round_artifacts);
                let work_out = self.worker.run(&ctx);
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
                let rev_out = self.reviewer.run(&ctx);
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
        assert!(out.result.summary.contains("非法路径"));
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
}
