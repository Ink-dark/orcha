//! M2/M3 内置 Sub-Agent 集合：Observer / Planner / Worker / Tester / Reviewer / Fixer。
//!
//! 这些实现都是**确定性最小实现**，用于打通单步执行管线与 Cycleround 闭环；
//! 后续可替换为基于 LLM 的版本，但 trait 不变。
//!
//! 典型 M2 任务描述：`"在 repo 中创建 hello.py 输出 hello"`，
//! Worker 会真实写出 `hello.py` 并生成可 `git apply` 的 unified diff。
//!
//! M3 Commit 4：新增 [`Fixer`]，在 Worker/Tester/Reviewer 失败时尝试产出修复，
//! 让下一轮可以重新执行；触达 `max_retries` 后由 Cycleround 返回
//! `Failed(MaxRetriesExceeded)`。

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use orcha_sdk::{Artifact, ArtifactType};

use crate::{StepContext, StepOutput, SubAgent};

/// 观察者：扫描 workspace，报告当前已存在哪些文件。
pub struct Observer;

impl SubAgent for Observer {
    fn name(&self) -> &'static str {
        "observer"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let files = list_files(&ctx.workspace).unwrap_or_default();
        let summary = if files.is_empty() {
            "workspace 为空，无已有文件".to_string()
        } else {
            format!(
                "workspace 已有 {} 个文件: {}",
                files.len(),
                files.join(", ")
            )
        };

        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "observer"),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };
        StepOutput::success("S-observer", summary).with_artifacts(vec![artifact])
    }
}

/// 规划者：从任务描述解析目标文件名与期望内容，产出执行计划。
///
/// 支持的最简语法：`"创建 <filename> 输出 <content>"`，
/// 例如 `"创建 hello.py 输出 hello"`。
/// 解析失败时返回 failure，由调度器决定是否进入 Fixer（M3）。
pub struct Planner;

impl SubAgent for Planner {
    fn name(&self) -> &'static str {
        "planner"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        match parse_create_plan(&ctx.task.description) {
            Ok(plan) => {
                let summary = format!(
                    "计划：在 workspace 创建 {} 并写入内容（{} 字节）",
                    plan.filename,
                    plan.content.len()
                );
                let artifact = Artifact {
                    artifact_id: next_artifact_id(&ctx.prior_artifacts, "planner"),
                    artifact_type: ArtifactType::Report,
                    commit_sha: None,
                    patch: None,
                    url: None,
                };
                StepOutput::success("S-planner", summary).with_artifacts(vec![artifact])
            }
            Err(e) => StepOutput::failure("S-planner", format!("解析任务失败: {e}")),
        }
    }
}

/// 执行者：按 Planner 的计划在 workspace 写文件，并生成 unified diff。
///
/// diff 的 `a/<filename>` / `b/<filename>` 路径相对 workspace 根，
/// 因此 `git apply` 时应在 workspace 目录下执行。
pub struct Worker;

impl SubAgent for Worker {
    fn name(&self) -> &'static str {
        "worker"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let plan = match parse_create_plan(&ctx.task.description) {
            Ok(p) => p,
            Err(e) => return StepOutput::failure("S-worker", format!("无法解析计划: {e}")),
        };

        let target_path = ctx.workspace.join(&plan.filename);
        if let Some(parent) = target_path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return StepOutput::failure("S-worker", format!("创建目录失败: {e}"));
            }
        }
        // unified diff 约定文件末尾需有换行（否则 git apply 会警告 "no newline at end of file"，
        // 且 apply 后内容会多出一个隐式换行）。这里规范化：写文件时确保以 \n 结尾，
        // 与 patch 内容保持一致。
        let normalized = ensure_trailing_newline(&plan.content);
        if let Err(e) = fs::write(&target_path, &normalized) {
            return StepOutput::failure(
                "S-worker",
                format!("写文件失败 {}: {e}", target_path.display()),
            );
        }

        let patch = match make_create_diff(&plan.filename, &normalized) {
            Ok(p) => p,
            Err(e) => return StepOutput::failure("S-worker", format!("生成 diff 失败: {e}")),
        };

        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "worker"),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: None,
            patch: Some(patch),
            url: Some(format!("file:///{}", target_path.display())),
        };
        StepOutput::success(
            "S-worker",
            format!(
                "已写入 {} ({} 字节) 并生成 diff",
                plan.filename,
                plan.content.len()
            ),
        )
        .with_artifacts(vec![artifact])
    }
}

/// 解析 "创建 <filename> 输出 <content>" 形式的任务描述。
pub(crate) struct CreatePlan {
    pub filename: String,
    pub content: String,
}

// ============================================================
// Tester (M3 Commit 2)
// ============================================================

/// 测试执行者：检测 workspace 用的测试框架并执行测试命令。
///
/// 检测优先级：
/// 1. `Cargo.toml` → `cargo test --quiet`（Rust 项目）
/// 2. `pytest.ini` / `pyproject.toml` / `setup.py` → `pytest -q`（Python 项目）
/// 3. `test.py` 存在 → `python3 test.py`（或回退 `python test.py`）
/// 4. 其余 → 失败，提示未检测到测试框架
///
/// Tester 是 Cycleround 闭环里 `Plan → Code → Test` 的最后一步；
/// 失败时由 Fixer 决定是否进入下一轮（M3 Commit 4 接入）。
pub struct Tester;

impl SubAgent for Tester {
    fn name(&self) -> &'static str {
        "tester"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let cmd = match detect_test_command(&ctx.workspace) {
            Some(c) => c,
            None => {
                return StepOutput::failure(
                    "S-tester",
                    "未检测到测试框架（Cargo.toml / pytest.ini / test.py 均不存在）",
                );
            }
        };

        let output = match Command::new(&cmd.program)
            .args(&cmd.args)
            .current_dir(&ctx.workspace)
            .output()
        {
            Ok(o) => o,
            Err(e) => {
                return StepOutput::failure(
                    "S-tester",
                    format!("执行 {} 失败: {e}", cmd.display()),
                );
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let combined = if stdout.is_empty() {
            stderr.to_string()
        } else if stderr.is_empty() {
            stdout.to_string()
        } else {
            format!("{stdout}\n--- stderr ---\n{stderr}")
        };

        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "tester"),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };

        if output.status.success() {
            StepOutput::success(
                "S-tester",
                format!("{} 通过\n{}", cmd.display(), truncate(&combined, 512)),
            )
            .with_artifacts(vec![artifact])
        } else {
            // 失败时把 artifact 也带上（含输出摘要），便于 Fixer 阅读并产出新计划。
            StepOutput::failure(
                "S-tester",
                format!(
                    "{} 失败 (exit {:?}):\n{}",
                    cmd.display(),
                    output.status.code(),
                    truncate(&combined, 512)
                ),
            )
            .with_artifacts(vec![artifact])
        }
    }
}

/// 待执行的测试命令。
pub(crate) struct TestCommand {
    pub program: String,
    pub args: Vec<String>,
}

impl TestCommand {
    fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

/// 检测 workspace 应当跑哪个测试命令。返回 None 表示未识别出测试框架。
pub(crate) fn detect_test_command(workspace: &Path) -> Option<TestCommand> {
    if workspace.join("Cargo.toml").exists() {
        return Some(TestCommand {
            program: "cargo".into(),
            args: vec!["test".into(), "--quiet".into()],
        });
    }
    if workspace.join("pytest.ini").exists()
        || workspace.join("pyproject.toml").exists()
        || workspace.join("setup.py").exists()
    {
        return Some(TestCommand {
            program: "pytest".into(),
            args: vec!["-q".into()],
        });
    }
    if workspace.join("test.py").exists() {
        // 优先 python3，回退到 python（Windows 常见）。
        let py = find_python().unwrap_or_else(|| "python3".into());
        return Some(TestCommand {
            program: py,
            args: vec!["test.py".into()],
        });
    }
    None
}

/// 在 PATH 中寻找可用的 Python 解释器。M3 确定性实现：依次试 `python3` / `python`。
///
/// `pub` 以便集成测试与 CLI 层复用（判定环境是否支持运行 test.py）。
pub fn find_python() -> Option<String> {
    for cmd in ["python3", "python"] {
        let ok = Command::new(cmd)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(cmd.to_string());
        }
    }
    None
}

/// 把字符串截断到 max 字符（超出加 `…`）。用于测试输出摘要。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ============================================================
// Reviewer (M3 Commit 3)
// ============================================================

/// 审核者：对最新 CodeDiff artifact 做静态检查。
///
/// M3 最小实现只做三项检查：
/// 1. 前序产物里存在 `CodeDiff` artifact，且 `patch` 字段非空。
/// 2. patch 含 `+++ b/` 行（统一 diff 标记），否则视为格式异常。
/// 3. patch 不含路径穿越（`../` 或 `..\`）。
///
/// 任一不满足则返回 failure 并附 Report artifact 记录问题。
/// 通过则返回 success。
///
/// Reviewer 跑在 Tester 之后；当 Tester 失败时 Cycleround 跳过 Reviewer
/// 直接进入下一轮（M3 Commit 4 接入 Fixer）。
pub struct Reviewer;

impl SubAgent for Reviewer {
    fn name(&self) -> &'static str {
        "reviewer"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "reviewer"),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };

        let patch = ctx
            .prior_artifacts
            .iter()
            .rev()
            .find(|a| a.artifact_type == ArtifactType::CodeDiff)
            .and_then(|a| a.patch.as_deref());

        let Some(patch) = patch else {
            return StepOutput::failure("S-reviewer", "无可审核的 patch（前序未产出 CodeDiff）")
                .with_artifacts(vec![artifact]);
        };

        let mut issues: Vec<String> = Vec::new();
        if patch.is_empty() {
            issues.push("patch 为空".into());
        }
        if !patch.contains("+++ b/") {
            issues.push("patch 缺少 `+++ b/` 统一 diff 标记".into());
        }
        if patch.contains("../") || patch.contains("..\\") {
            issues.push("patch 含路径穿越（../）".into());
        }

        if issues.is_empty() {
            StepOutput::success("S-reviewer", "patch 通过审核").with_artifacts(vec![artifact])
        } else {
            StepOutput::failure("S-reviewer", format!("审核未通过: {}", issues.join("; ")))
                .with_artifacts(vec![artifact])
        }
    }
}

// ============================================================
// Fixer (M3 Commit 4)
// ============================================================

/// 修复者：在 Worker/Tester/Reviewer 失败时尝试产出可让下一轮成功的修复。
///
/// M3 最小确定性实现支持一种修复策略：
/// - 若 workspace 当前**没有**任何测试框架（无 `Cargo.toml` / `pytest.ini` /
///   `pyproject.toml` / `setup.py` / `test.py`），且 task 描述可解析为
///   `创建 <filename> 输出 <content>`，则在 workspace 下创建 `test.py`，
///   断言 `<filename>` 的内容（去 trailing newline 后）等于 `<content>`。
///   这样下一轮 Tester 即可通过。
///
/// 其余失败场景（如 `test.py` 已存在但断言失败、Reviewer 审核失败等）
/// Fixer 无法修复，返回 failure；Cycleround 累计触达 `max_retries` 后
/// 返回 `Failed(MaxRetriesExceeded)`。
///
/// 注意：Fixer **不会**改写已存在的 `test.py`，避免「为了让测试通过而改测试」
/// 的作弊行为——若 `test.py` 已存在，则视为用户的测试约束，Fixer 不动它。
pub struct Fixer;

impl SubAgent for Fixer {
    fn name(&self) -> &'static str {
        "fixer"
    }

    fn run(&self, ctx: &StepContext) -> StepOutput {
        let artifact = Artifact {
            artifact_id: next_artifact_id(&ctx.prior_artifacts, "fixer"),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        };

        let plan = match parse_create_plan(&ctx.task.description) {
            Ok(p) => p,
            Err(e) => {
                return StepOutput::failure("S-fixer", format!("无法解析任务以生成修复: {e}"))
                    .with_artifacts(vec![artifact]);
            }
        };

        // 已有测试框架时 Fixer 不动 test.py（避免改测试让测试通过）。
        if has_test_framework(&ctx.workspace) {
            return StepOutput::failure(
                "S-fixer",
                "workspace 已有测试框架，Fixer 无法修复测试失败（不修改既有 test.py）",
            )
            .with_artifacts(vec![artifact]);
        }

        // 生成 test.py 内容：用 Rust Debug 格式产出 Python 字符串字面量，
        // 这样能正确转义换行 / 引号 / 反斜杠。
        let test_content = format!(
            "assert open({filename:?}).read().strip() == {content:?}\n",
            filename = plan.filename,
            content = plan.content,
        );
        let test_path = ctx.workspace.join("test.py");
        if let Err(e) = fs::write(&test_path, &test_content) {
            return StepOutput::failure(
                "S-fixer",
                format!("写 test.py 失败 {}: {e}", test_path.display()),
            )
            .with_artifacts(vec![artifact]);
        }

        StepOutput::success(
            "S-fixer",
            format!(
                "Fixer 已创建 test.py，断言 {} 内容为 {:?}",
                plan.filename, plan.content
            ),
        )
        .with_artifacts(vec![artifact])
    }
}

/// 判断 workspace 是否已存在测试框架文件。
fn has_test_framework(workspace: &Path) -> bool {
    workspace.join("Cargo.toml").exists()
        || workspace.join("pytest.ini").exists()
        || workspace.join("pyproject.toml").exists()
        || workspace.join("setup.py").exists()
        || workspace.join("test.py").exists()
}

pub(crate) fn parse_create_plan(desc: &str) -> Result<CreatePlan> {
    let desc = desc.trim();
    // 同时支持 "创建" / "create"，关键词大小写不敏感。
    let lower = desc.to_lowercase();
    let (create_kw, output_kw) = if lower.starts_with("创建") {
        ("创建", "输出")
    } else if lower.starts_with("create ") {
        ("create ", "output ")
    } else {
        anyhow::bail!(
            "无法识别的任务描述，期望形如 '创建 <filename> 输出 <content>'，实际: {desc}"
        );
    };

    let after_create = &desc[create_kw.len()..].trim_start();
    // 用 output_kw 切分（注意 "输出" 在 ASCII 下不会和 "create" 混淆）。
    let output_idx = after_create
        .find(output_kw)
        .ok_or_else(|| anyhow::anyhow!("缺少 '{}' 关键字", output_kw.trim()))?;
    let filename = after_create[..output_idx].trim().to_string();
    let content = after_create[output_idx + output_kw.len()..]
        .trim()
        .to_string();

    if filename.is_empty() {
        anyhow::bail!("文件名为空");
    }
    // 简单防穿越：文件名不得包含路径分隔符或 ..
    if filename.contains('/')
        || filename.contains('\\')
        || filename == ".."
        || filename.contains("..")
    {
        anyhow::bail!("非法文件名（含路径分隔符或 ..）: {filename}");
    }
    Ok(CreatePlan { filename, content })
}

/// 为新增文件生成 unified diff。
/// 形如：
/// ```text
/// diff --git a/<filename> b/<filename>
/// new file mode 100644
/// --- /dev/null
/// +++ b/<filename>
/// @@ -0,0 +1,<n> @@
/// +<line1>
/// +<line2>
/// ```
pub(crate) fn make_create_diff(filename: &str, content: &str) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    if content.is_empty() || lines.is_empty() {
        // 空内容时 diff 仍合法，只是 +0 行。
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

/// 列出 workspace 下所有相对路径文件（按字典序）。
/// 仅 `llm` feature 下由 `llm_agents` 模块使用；默认编译时允许 dead_code。
#[cfg_attr(not(feature = "llm"), allow(dead_code))]
pub(crate) fn list_workspace_files(root: &Path) -> Vec<String> {
    list_files(root).unwrap_or_default()
}

fn list_files(root: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, out)?;
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            out.push(rel.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

/// 基于前序 artifacts 的数量生成下一个 artifact id，形如 `ART-001`。
fn next_artifact_id(prior: &[Artifact], agent: &str) -> String {
    let n = prior.len() + 1;
    // 用 agent 名作为前缀片段，便于人工辨识来源。
    // 形如 ART-observer-001 / ART-worker-002。
    format!("ART-{agent}-{n:03}")
}

/// 确保 content 以 `\n` 结尾，与 unified diff 的 git apply 行为保持一致。
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

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_sdk::Task;

    #[test]
    fn observer_reports_empty_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Observer.run(&ctx);
        assert!(out.result.success);
        assert!(out.result.summary.contains("workspace 为空"));
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].artifact_type, ArtifactType::Report);
        assert!(out.artifacts[0].artifact_id.starts_with("ART-observer-"));
    }

    #[test]
    fn planner_parses_create_plan() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Planner.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert!(out.result.summary.contains("hello.py"));
    }

    #[test]
    fn planner_rejects_unparseable_description() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "do something unrelated".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Planner.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("解析任务失败"));
    }

    #[test]
    fn planner_rejects_path_traversal_filename() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 ../evil.py 输出 x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Planner.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("解析任务失败"));
    }

    #[test]
    fn worker_creates_file_and_diff_for_hello_py() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);

        let out = Worker.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);

        // 1. 真实产出 hello.py（规范化为以 \n 结尾，与 git apply 行为一致）。
        let hello = ws.path().join("hello.py");
        assert!(hello.is_file(), "hello.py should exist");
        assert_eq!(fs::read_to_string(&hello).unwrap(), "hello\n");

        // 2. 产出 CodeDiff artifact，含可 git apply 的 patch。
        assert_eq!(out.artifacts.len(), 1);
        let art = &out.artifacts[0];
        assert_eq!(art.artifact_type, ArtifactType::CodeDiff);
        let patch = art.patch.as_ref().expect("patch must be present");
        assert!(patch.contains("diff --git a/hello.py b/hello.py"));
        assert!(patch.contains("new file mode 100644"));
        assert!(patch.contains("+++ b/hello.py"));
        assert!(patch.contains("+hello"));
        // url 应指向真实产出文件。
        assert!(art.url.as_ref().unwrap().contains("hello.py"));
    }

    #[test]
    fn make_create_diff_has_valid_unified_format() {
        let patch = make_create_diff("a.txt", "line1\nline2").unwrap();
        assert!(patch.starts_with("diff --git a/a.txt b/a.txt\n"));
        assert!(patch.contains("@@ -0,0 +1,2 @@"));
        assert!(patch.contains("+line1\n"));
        assert!(patch.contains("+line2\n"));
    }

    #[test]
    fn make_create_diff_handles_empty_content() {
        let patch = make_create_diff("empty.txt", "").unwrap();
        assert!(patch.contains("@@ -0,0 +1,0 @@"));
    }

    #[test]
    fn parse_create_plan_supports_english_keyword() {
        let p = parse_create_plan("create main.rs output fn main(){}").unwrap();
        assert_eq!(p.filename, "main.rs");
        assert_eq!(p.content, "fn main(){}");
    }

    #[test]
    fn next_artifact_id_increments() {
        let prior = vec![Artifact {
            artifact_id: "ART-x".into(),
            artifact_type: ArtifactType::Report,
            commit_sha: None,
            patch: None,
            url: None,
        }];
        let id = next_artifact_id(&prior, "worker");
        assert_eq!(id, "ART-worker-002");
    }

    // ============================================================
    // Tester (M3 Commit 2)
    // ============================================================

    #[test]
    fn tester_detects_cargo_when_cargo_toml_present() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("Cargo.toml"), "").unwrap();
        let cmd = detect_test_command(ws.path()).expect("应检测到 cargo test");
        assert_eq!(cmd.program, "cargo");
        assert_eq!(cmd.args, vec!["test".to_string(), "--quiet".to_string()]);
    }

    #[test]
    fn tester_detects_pytest_when_pytest_ini_present() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("pytest.ini"), "[pytest]").unwrap();
        let cmd = detect_test_command(ws.path()).expect("应检测到 pytest");
        assert_eq!(cmd.program, "pytest");
        assert_eq!(cmd.args, vec!["-q".to_string()]);
    }

    #[test]
    fn tester_detects_pytest_when_pyproject_toml_present() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("pyproject.toml"), "[tool.pytest]").unwrap();
        let cmd = detect_test_command(ws.path()).expect("应检测到 pytest");
        assert_eq!(cmd.program, "pytest");
    }

    #[test]
    fn tester_detects_python_when_test_py_present() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("test.py"), "assert True").unwrap();
        let cmd = detect_test_command(ws.path()).expect("应检测到 python test.py");
        // python3 或 python（取决于环境），但参数必然是 test.py。
        assert!(cmd.program == "python3" || cmd.program == "python");
        assert_eq!(cmd.args, vec!["test.py".to_string()]);
    }

    #[test]
    fn tester_detects_nothing_for_empty_workspace() {
        let ws = tempfile::tempdir().unwrap();
        assert!(detect_test_command(ws.path()).is_none());
    }

    #[test]
    fn tester_returns_failure_when_no_framework_detected() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Tester.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("未检测到测试框架"));
    }

    #[test]
    fn tester_runs_python_test_py_successfully() {
        // 这个测试需要环境里有 python3 或 python；没有则跳过。
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        // test.py 通过：assert True。
        fs::write(ws.path().join("test.py"), "assert True\n").unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Tester.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert_eq!(out.artifacts.len(), 1);
        assert_eq!(out.artifacts[0].artifact_type, ArtifactType::Report);
    }

    #[test]
    fn tester_returns_failure_when_test_py_fails() {
        if find_python().is_none() {
            eprintln!("skipping: no python interpreter on PATH");
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        // test.py 失败：assert False，会抛 AssertionError，python 退出码非 0。
        fs::write(
            ws.path().join("test.py"),
            "def test_x():\n    assert False, 'intentional'\n\ntest_x()\n",
        )
        .unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Tester.run(&ctx);
        assert!(
            !out.result.success,
            "expected failure, got: {:?}",
            out.result
        );
        assert!(out.result.summary.contains("失败"));
        assert!(out.result.summary.contains("intentional"));
        assert_eq!(out.artifacts.len(), 1, "失败也应带 artifact");
    }

    #[test]
    fn tester_truncates_long_output_in_summary() {
        let long = "x".repeat(1024);
        let t = truncate(&long, 512);
        assert_eq!(t.chars().count(), 512);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn tester_truncate_returns_short_input_unchanged() {
        let t = truncate("short", 512);
        assert_eq!(t, "short");
    }

    // ============================================================
    // Reviewer (M3 Commit 3)
    // ============================================================

    fn make_diff_artifact(patch: &str) -> Artifact {
        Artifact {
            artifact_id: "ART-worker-001".into(),
            artifact_type: ArtifactType::CodeDiff,
            commit_sha: None,
            patch: Some(patch.into()),
            url: None,
        }
    }

    #[test]
    fn reviewer_returns_failure_when_no_patch_to_review() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Reviewer.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("无可审核的 patch"));
        assert_eq!(out.artifacts.len(), 1);
    }

    #[test]
    fn reviewer_passes_clean_patch() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let patch = "diff --git a/hello.py b/hello.py\nnew file mode 100644\n--- /dev/null\n+++ b/hello.py\n@@ -0,0 +1,1 @@\n+hello\n";
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![make_diff_artifact(patch)],
        );
        let out = Reviewer.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        assert!(out.result.summary.contains("通过审核"));
    }

    #[test]
    fn reviewer_rejects_patch_with_path_traversal() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        // patch 中 b/ 路径含 ../（构造越界 patch）。
        let patch = "diff --git a/../evil.py b/../evil.py\nnew file mode 100644\n--- /dev/null\n+++ b/../evil.py\n@@ -0,0 +1,1 @@\n+evil\n";
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![make_diff_artifact(patch)],
        );
        let out = Reviewer.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("路径穿越"));
    }

    #[test]
    fn reviewer_rejects_empty_patch() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            vec![make_diff_artifact("")],
        );
        let out = Reviewer.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("patch 为空"));
        assert!(out.result.summary.contains("`+++ b/`"));
    }

    #[test]
    fn reviewer_uses_latest_codediff_artifact() {
        // 同时存在两个 CodeDiff artifact，Reviewer 应使用最新的（最后追加的）。
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "x".into());
        let clean_patch =
            "diff --git a/a.py b/a.py\nnew file mode 100644\n--- /dev/null\n+++ b/a.py\n@@ -0,0 +1,1 @@\n+a\n";
        let bad_patch = "diff --git a/b.py b/b.py\nnew file mode 100644\n--- /dev/null\n+++ b/../b.py\n@@ -0,0 +1,1 @@\n+b\n";
        let mut artifacts = vec![make_diff_artifact(clean_patch)];
        // 修改 artifact_id 以区分。
        let mut second = make_diff_artifact(bad_patch);
        second.artifact_id = "ART-worker-002".into();
        artifacts.push(second);
        let ctx = StepContext::new(ws.path(), task).with_prior(
            orcha_sdk::Step {
                id: "S-worker".into(),
                name: "worker".into(),
                agent: "worker".into(),
                status: orcha_sdk::StepStatus::Succeeded,
            },
            artifacts,
        );
        let out = Reviewer.run(&ctx);
        assert!(
            !out.result.success,
            "应当审核最新的（bad）patch 而非第一个 clean 的"
        );
        assert!(out.result.summary.contains("路径穿越"));
    }

    // ============================================================
    // Fixer (M3 Commit 4)
    // ============================================================

    #[test]
    fn fixer_creates_test_py_when_no_framework_present() {
        let ws = tempfile::tempdir().unwrap();
        // 模拟 Worker 已成功：hello.py 已存在。
        fs::write(ws.path().join("hello.py"), "hello\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        // test.py 应被创建。
        let test_path = ws.path().join("test.py");
        assert!(test_path.is_file(), "test.py should be created");
        let content = fs::read_to_string(&test_path).unwrap();
        assert!(
            content.contains("hello.py"),
            "test.py 应引用目标文件名: {content}"
        );
        assert!(
            content.contains("hello"),
            "test.py 应包含期望内容: {content}"
        );
        assert_eq!(out.artifacts.len(), 1, "应产出 1 个 Report artifact");
        assert_eq!(out.artifacts[0].artifact_type, ArtifactType::Report);
    }

    #[test]
    fn fixer_fails_when_test_framework_already_present() {
        let ws = tempfile::tempdir().unwrap();
        // 已有 test.py（断言错误），Fixer 不应改写它。
        fs::write(ws.path().join("test.py"), "assert False\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("已有测试框架"));
        // test.py 内容应未被改写。
        let content = fs::read_to_string(ws.path().join("test.py")).unwrap();
        assert_eq!(content, "assert False\n", "test.py 不应被改写");
    }

    #[test]
    fn fixer_fails_when_task_unparseable() {
        let ws = tempfile::tempdir().unwrap();
        let task = Task::new("T-1".into(), "do something unrelated".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(!out.result.success);
        assert!(out.result.summary.contains("无法解析任务"));
        // 失败时也不应创建 test.py。
        assert!(!ws.path().join("test.py").exists());
    }

    #[test]
    fn fixer_creates_test_py_asserting_content_strips_trailing_newline() {
        // 验证 Fixer 生成的 test.py 用 .strip() 比较，
        // 这样 Worker 写出 hello\n 时仍能通过。
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("hello.py"), "hello\n").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(out.result.success);
        let test_content = fs::read_to_string(ws.path().join("test.py")).unwrap();
        assert!(
            test_content.contains(".strip()"),
            "test.py 应使用 .strip() 比较: {test_content}"
        );
    }

    #[test]
    fn fixer_detects_cargo_toml_as_existing_framework() {
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("Cargo.toml"), "").unwrap();
        let task = Task::new("T-1".into(), "创建 hello.py 输出 hello".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(!out.result.success, "Cargo.toml 也算测试框架，Fixer 应拒绝");
        assert!(out.result.summary.contains("已有测试框架"));
    }

    #[test]
    fn fixer_generates_valid_python_string_literal_for_content() {
        // 内容含空格 / 特殊字符时，test.py 仍应是合法 Python。
        let ws = tempfile::tempdir().unwrap();
        fs::write(ws.path().join("greet.txt"), "hello world\n").unwrap();
        let task = Task::new("T-1".into(), "创建 greet.txt 输出 hello world".into());
        let ctx = StepContext::new(ws.path(), task);
        let out = Fixer.run(&ctx);
        assert!(out.result.success, "summary: {}", out.result.summary);
        let test_path = ws.path().join("test.py");
        // 验证生成的 test.py 是合法 Python（语法检查）。
        // 注意：可能环境无 python，此时跳过语法检查，仅检查内容含 "hello world"。
        let content = fs::read_to_string(&test_path).unwrap();
        assert!(content.contains("hello world"));
        if let Some(py) = find_python() {
            let check = Command::new(&py)
                .arg("-c")
                .arg(format!(
                    "compile(open({}).read(), 'test.py', 'exec')",
                    "'test.py'"
                ))
                .current_dir(ws.path())
                .output();
            if let Ok(o) = check {
                assert!(
                    o.status.success(),
                    "test.py 应是合法 Python，stderr: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
        }
    }
}
