//! 把 Orcha 任务与 workspace 上下文转成 LLM 消息。
//!
//! 每个 Sub-Agent (Planner / Worker / Reviewer) 的 prompt 都构造一组
//! `ChatMessage`：system 指令 + user 任务描述 + 前序步骤结果摘要。
//!
//! prompt 设计原则：
//! - 强约束输出格式（JSON 或单一文件块），便于后续解析
//! - 把 workspace 文件清单与失败摘要塞进上下文，让 LLM 知道当前状态

use crate::ChatMessage;
use orcha_sdk::{Artifact, ArtifactType};

/// Planner 的 system 指令。
pub const PLANNER_SYSTEM: &str = r#"你是 Orcha 的规划者。你的任务是理解用户需求与 workspace 现状，输出一个 JSON 计划。

你可以使用以下工具来探索 workspace：
- list_dir：列出目录内容
- read_file：读取文件内容（带行号）
- grep：在文件中搜索文本
- glob：按模式查找文件

先用工具充分了解 workspace 的代码结构和文件内容，再制定计划。

最终输出格式（仅 JSON，无多余文字）：
{
  "target_files": ["<将修改的文件路径>"],
  "steps": [
    {"action": "edit", "path": "<路径>", "search": "<原文>", "replace": "<新文>"},
    {"action": "create", "path": "<路径>", "content": "<完整文件内容>"},
    {"action": "delete", "path": "<路径>"}
  ]
}

约束：
- path 不得含 ".." 或绝对路径
- edit 步骤必须提供 search 和 replace，search 必须在原文件中精确唯一匹配
- create 步骤必须提供 content，content 末尾应有换行
- target_files 列出本计划涉及的所有文件路径
- 仅输出 JSON，第一个字符必须是 '{'"#;

/// Worker 的 system 指令。
pub const WORKER_SYSTEM: &str = r#"你是 Orcha 的执行者。你的任务是按 Planner 的计划在 workspace 中执行文件操作。

你可以使用以下工具来了解 workspace：
- read_file：读取文件内容（带行号）
- grep：在文件中搜索文本
- list_dir：列出目录内容

先用 read_file 读取要编辑的文件，确认 search 文本存在且唯一，再执行操作。

最终输出格式（仅 JSON）：
{
  "files": [{"path": "<相对路径>", "content": "<完整文件内容>"}],
  "summary": "<一句话说明>"
}

约束：
- 每个 file 的 content 必须是修改后的完整文件内容（不是 diff）
- path 不得含 ".."
- content 末尾应有换行
- 仅输出 JSON"#;

/// Reviewer 的 system 指令。
pub const REVIEWER_SYSTEM: &str = r#"你是 Orcha 的审核者。检查 Worker 产出的文件是否满足任务要求。

输出格式（仅 JSON）：
{
  "approved": true | false,
  "issues": ["<问题1>", "<问题2>"]
}

approved=true 表示通过；false 表示有阻塞性问题。仅输出 JSON。"#;

/// 构造 Planner 的 LLM 消息。
pub fn build_planner_prompt(task_desc: &str, workspace_files: &[String]) -> Vec<ChatMessage> {
    let files = if workspace_files.is_empty() {
        "（workspace 为空）".to_string()
    } else {
        workspace_files.join(", ")
    };
    vec![
        ChatMessage::system(PLANNER_SYSTEM),
        ChatMessage::user(format!(
            "任务：{task_desc}\n\n当前 workspace 文件：{files}\n\n输出 JSON 计划。"
        )),
    ]
}

/// 构造 Worker 的 LLM 消息。
///
/// `plan_text` 是 Planner 输出的 JSON 文本，原样塞进上下文。
pub fn build_worker_prompt(task_desc: &str, plan_text: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(WORKER_SYSTEM),
        ChatMessage::user(format!(
            "任务：{task_desc}\n\nPlanner 计划：\n{plan_text}\n\n按计划写出文件，输出 JSON。"
        )),
    ]
}

/// 构造 Reviewer 的 LLM 消息。
///
/// `worker_files` 是 Worker 写出的文件列表（path → content）。
pub fn build_reviewer_prompt(
    task_desc: &str,
    worker_files: &[(String, String)],
) -> Vec<ChatMessage> {
    let files_json = serde_json::to_string_pretty(worker_files).unwrap_or_default();
    vec![
        ChatMessage::system(REVIEWER_SYSTEM),
        ChatMessage::user(format!(
            "任务：{task_desc}\n\nWorker 产出的文件：\n{files_json}\n\n审核是否满足任务要求。"
        )),
    ]
}

/// 从 Worker 的 JSON 输出解析出文件列表。
pub fn parse_worker_output(output: &str) -> Result<Vec<(String, String)>, String> {
    // 容忍 LLM 在 JSON 前后包了 markdown ```json fence。
    let trimmed = strip_markdown_fence(output);
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("解析 Worker JSON 失败: {e}"))?;
    let arr = v
        .get("files")
        .and_then(|f| f.as_array())
        .ok_or("输出缺 files 数组")?;
    let mut out = Vec::new();
    for f in arr {
        let path = f
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or("file 缺 path")?
            .to_string();
        let content = f
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        out.push((path, content));
    }
    Ok(out)
}

/// 从 Planner 的 JSON 输出解析出 plan 文本（原样返回，留给 Worker 用）。
pub fn parse_planner_output(output: &str) -> Result<String, String> {
    let trimmed = strip_markdown_fence(output);
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("解析 Planner JSON 失败: {e}"))?;
    if v.get("steps").is_none() {
        return Err("输出缺 steps".into());
    }
    Ok(trimmed.to_string())
}

/// 从 Reviewer 的 JSON 输出解析是否通过 + 问题列表。
pub fn parse_reviewer_output(output: &str) -> Result<(bool, Vec<String>), String> {
    let trimmed = strip_markdown_fence(output);
    let v: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| format!("解析 Reviewer JSON 失败: {e}"))?;
    let approved = v.get("approved").and_then(|a| a.as_bool()).unwrap_or(false);
    let issues = v
        .get("issues")
        .and_then(|i| i.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok((approved, issues))
}

/// 去掉 LLM 常见的 ```json ... ``` 代码块围栏。
fn strip_markdown_fence(s: &str) -> &str {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    t
}

/// 从 artifacts 中提取 Worker 写出的文件列表（path → content）。
/// 用 CodeDiff artifact 的 url 字段（形如 `file:///path`）+ workspace 读文件。
pub fn extract_files_from_artifacts(
    artifacts: &[Artifact],
    workspace: &std::path::Path,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for a in artifacts {
        if a.artifact_type != ArtifactType::CodeDiff {
            continue;
        }
        if let Some(url) = &a.url {
            if let Some(p) = url.strip_prefix("file://") {
                let path = std::path::Path::new(p);
                let rel = path.strip_prefix(workspace).unwrap_or(path);
                let rel_str = rel.to_string_lossy().to_string();
                let content = std::fs::read_to_string(path).unwrap_or_default();
                out.push((rel_str, content));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_prompt_has_system_and_user() {
        let msgs = build_planner_prompt("创建 hello.py", &[]);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[1].role, "user");
        assert!(msgs[1].content.contains("创建 hello.py"));
    }

    #[test]
    fn worker_prompt_includes_plan() {
        let plan = r#"{"steps":[{"action":"write_file","path":"a.py","content":"x"}]}"#;
        let msgs = build_worker_prompt("任务", plan);
        assert!(msgs[1].content.contains(plan));
    }

    #[test]
    fn parse_worker_output_extracts_files() {
        let out = r#"{"files":[{"path":"a.py","content":"print(1)\n"}]}"#;
        let files = parse_worker_output(out).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].0, "a.py");
        assert_eq!(files[0].1, "print(1)\n");
    }

    #[test]
    fn parse_worker_output_tolerates_markdown_fence() {
        let out = "```json\n{\"files\":[{\"path\":\"x\",\"content\":\"y\"}]}\n```";
        let files = parse_worker_output(out).unwrap();
        assert_eq!(files[0].0, "x");
    }

    #[test]
    fn parse_planner_output_validates_steps() {
        let ok = parse_planner_output(r#"{"steps":[]}"#).unwrap();
        assert!(ok.contains("steps"));
        let err = parse_planner_output(r#"{"foo":1}"#);
        assert!(err.is_err());
    }

    #[test]
    fn parse_reviewer_output_extracts_approval() {
        let (ok, issues) = parse_reviewer_output(r#"{"approved":true,"issues":[]}"#).unwrap();
        assert!(ok);
        assert!(issues.is_empty());
        let (ok2, issues2) = parse_reviewer_output(r#"{"approved":false,"issues":["a"]}"#).unwrap();
        assert!(!ok2);
        assert_eq!(issues2, vec!["a".to_string()]);
    }

    #[test]
    fn strip_fence_handles_json_and_plain() {
        assert_eq!(strip_markdown_fence("```json\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_markdown_fence("```\n{\"a\":1}\n```"), "{\"a\":1}");
        assert_eq!(strip_markdown_fence("{\"a\":1}"), "{\"a\":1}");
    }
}
