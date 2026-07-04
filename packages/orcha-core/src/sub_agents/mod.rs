//! M2 内置 Sub-Agent 集合：Observer / Planner / Worker。
//!
//! 三个实现都是**确定性最小实现**，用于打通单步执行管线；
//! M3 会替换为基于 LLM 的版本，但 trait 不变。
//!
//! 典型 M2 任务描述：`"在 repo 中创建 hello.py 输出 hello"`，
//! Worker 会真实写出 `hello.py` 并生成可 `git apply` 的 unified diff。

use std::fs;
use std::path::Path;

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
        if let Err(e) = fs::write(&target_path, &plan.content) {
            return StepOutput::failure(
                "S-worker",
                format!("写文件失败 {}: {e}", target_path.display()),
            );
        }

        let patch = match make_create_diff(&plan.filename, &plan.content) {
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

        // 1. 真实产出 hello.py。
        let hello = ws.path().join("hello.py");
        assert!(hello.is_file(), "hello.py should exist");
        assert_eq!(fs::read_to_string(&hello).unwrap(), "hello");

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
}
