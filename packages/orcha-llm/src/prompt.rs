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

当前 workspace 摘要（项目类型 + 目录结构 + 关键文件）已在下方给出。
你不需要调用任何工具，直接基于摘要制定计划。

**关键规则**：
- 严格遵守项目已有的技术栈！如果项目是 Rust，你只能改 .rs 文件和 Cargo.toml；
  如果项目是 Go，只能改 .go 文件和 go.mod；绝不能引入新语言或框架。
- edit 用于修改已存在文件，create 用于新建文件。先用 read_file 确认文件存在再选择动作。
- 如果 workspace 已有完整的项目结构，优先在现有文件中定位并修改，不要创建不相关的新文件。

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
- 不要输出任何工具调用，不要输出解释性文字，第一个字符必须是 '{'
- JSON 字符串值中不得包含字面换行符或制表符；多行内容必须用 \\n 转义（例如 "line1\\nline2"），不得直接在字符串中换行"#;

/// Worker 的 system 指令。
pub const WORKER_SYSTEM: &str = r#"你是 Orcha 的执行者。按 Planner 的计划在 workspace 中执行文件操作。

你可以使用以下工具来了解 workspace：
- read_file：读取文件内容（带行号）
- grep：在文件中搜索文本
- list_dir：列出目录内容
- glob：按模式查找文件

先用 read_file 读取要编辑的文件，确认 search 文本存在且唯一，再执行操作。

最终输出格式（仅 JSON）：
{
  "steps": [
    {"action": "edit", "path": "<相对路径>", "search": "<原文>", "replace": "<新文>"},
    {"action": "create", "path": "<相对路径>", "content": "<完整文件内容>"},
    {"action": "delete", "path": "<相对路径>"}
  ],
  "summary": "<一句话说明>"
}

约束：
- 优先使用 edit（search/replace）修改已存在文件，避免整文件重写
- search 字段必须在原文件中精确匹配且唯一（否则 apply 失败）
- create 仅用于新文件；已存在文件必须用 edit 修改
- 多处修改用多个 step，每个 step 一个 search/replace 对
- 完整删除一段内容用 replace: ""
- path 不得含 ".." 或绝对路径
- 仅输出 JSON，第一个字符必须是 '{'
- JSON 字符串值中不得包含字面换行符或制表符；多行内容必须用 \\n 转义（例如 "line1\\nline2"），不得直接在字符串中换行

兼容格式（仅当无法用 edit 时使用，不推荐）：
{"files":[{"path":"<路径>","content":"<完整文件内容>"}]}"#;

/// Worker 输出的单步动作。
///
/// 与 `orcha-core::PlanStep` 字段对齐，但独立定义以避免 `orcha-llm` → `orcha-core`
/// 的循环依赖。`LlmWorker` 在调用方把 `WorkerStep` 转成 `PlanStep` 后调 `apply_step`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStep {
    /// `"edit"` / `"create"` / `"delete"`。
    pub action: String,
    /// 相对 workspace 的 POSIX 路径。
    pub path: String,
    /// `create` 模式：完整文件内容。
    pub content: Option<String>,
    /// `edit` 模式：原文件中需精确唯一匹配的字符串。
    pub search: Option<String>,
    /// `edit` 模式：替换 `search` 的新内容。
    pub replace: Option<String>,
}

/// Worker 输出解析结果：优先 `steps`（推荐），回退 `files`（兼容旧格式）。
#[derive(Debug)]
pub enum WorkerOutput {
    /// `{steps:[{action,path,search,replace,content}]}`
    Steps(Vec<WorkerStep>),
    /// `{files:[{path,content}]}`（旧格式，整文件重写）
    Files(Vec<(String, String)>),
}

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
            "任务：{task_desc}\n\n\
             workspace 摘要（Observer 自动检测）：\n{files}\n\n\
             注意：以上摘要已标明项目类型和语言，你只能在该技术栈范围内制定计划。\n\
             直接输出 JSON 计划，不要调用工具，不要输出解释性文字。"
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
///
/// 旧格式（整文件重写）：`{"files":[{"path","content"}]}`。
/// 推荐改用 [`parse_worker_output_with_steps`]，支持 edit/create/delete steps。
pub fn parse_worker_output(output: &str) -> Result<Vec<(String, String)>, String> {
    // 容忍 LLM 在 JSON 前后包了 markdown ```json fence。
    let trimmed = strip_markdown_fence(output);
    // 剥除可能残留的 DeepSeek XML tool_calls 块
    let cleaned = strip_xml_tool_calls(trimmed);
    // 修复 JSON 字符串值中的未转义字面换行符
    let escaped = escape_raw_control_chars_in_strings(&cleaned);
    let v: serde_json::Value = serde_json::from_str(&escaped)
        .or_else(|_| {
            // fallback：LLM 在 JSON 前有说明文字且无 fence，提取第一个 {...}
            let extracted = extract_json_object(&escaped);
            let extracted_escaped = escape_raw_control_chars_in_strings(extracted);
            serde_json::from_str(&extracted_escaped)
        })
        .map_err(|e| format!("解析 Worker JSON 失败: {e}"))?;
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

/// 从 Worker 的 JSON 输出解析出 steps（推荐）或 files（兼容旧格式）。
///
/// 优先尝试解析 `steps` 数组（edit/create/delete），无 `steps` 时回退到 `files`
/// 数组（旧整文件格式）。两种格式都没有时返回 `Err`。
///
/// `LlmWorker` 应根据返回的 [`WorkerOutput`] 变体选择执行路径：
/// - [`WorkerOutput::Steps`]：转 `PlanStep` 调 `apply_step`（search/replace edit）
/// - [`WorkerOutput::Files`]：直接 `fs::write` 整文件（兼容旧路径）
pub fn parse_worker_output_with_steps(output: &str) -> Result<WorkerOutput, String> {
    let trimmed = strip_markdown_fence(output);
    // 剥除可能残留的 DeepSeek XML tool_calls 块
    let cleaned = strip_xml_tool_calls(trimmed);
    // 修复 JSON 字符串值中的未转义字面换行符
    let escaped = escape_raw_control_chars_in_strings(&cleaned);
    let v: serde_json::Value = serde_json::from_str(&escaped)
        .or_else(|_| {
            // fallback：LLM 在 JSON 前有说明文字且无 fence，提取第一个 {...}
            let extracted = extract_json_object(&escaped);
            let extracted_escaped = escape_raw_control_chars_in_strings(extracted);
            serde_json::from_str(&extracted_escaped)
        })
        .map_err(|e| format!("解析 Worker JSON 失败: {e}"))?;

    if let Some(steps_v) = v.get("steps") {
        let arr = steps_v.as_array().ok_or("steps 不是数组")?;
        let mut steps = Vec::new();
        for s in arr {
            let action = s
                .get("action")
                .and_then(|x| x.as_str())
                .ok_or("step 缺 action")?
                .to_string();
            let path = s
                .get("path")
                .and_then(|x| x.as_str())
                .ok_or("step 缺 path")?
                .to_string();
            let content = s.get("content").and_then(|x| x.as_str()).map(String::from);
            let search = s.get("search").and_then(|x| x.as_str()).map(String::from);
            let replace = s.get("replace").and_then(|x| x.as_str()).map(String::from);
            steps.push(WorkerStep {
                action,
                path,
                content,
                search,
                replace,
            });
        }
        return Ok(WorkerOutput::Steps(steps));
    }

    if let Some(files_v) = v.get("files") {
        let arr = files_v.as_array().ok_or("files 不是数组")?;
        let mut files = Vec::new();
        for f in arr {
            let path = f
                .get("path")
                .and_then(|x| x.as_str())
                .ok_or("file 缺 path")?
                .to_string();
            let content = f
                .get("content")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            files.push((path, content));
        }
        return Ok(WorkerOutput::Files(files));
    }

    Err("输出缺 steps 或 files 数组".into())
}

/// 从 Planner 的 JSON 输出解析出 plan 文本（原样返回，留给 Worker 用）。
pub fn parse_planner_output(output: &str) -> Result<String, String> {
    let trimmed = strip_markdown_fence(output);
    // DeepSeek 可能在输出中夹带 XML 格式的 tool_calls（<｜｜DSML｜｜tool_calls>...），
    // 先剥掉这些块，再尝试解析 JSON。
    let cleaned = strip_xml_tool_calls(trimmed);
    // 修复 JSON 字符串值中的未转义字面换行符（DeepSeek 常见问题）。
    // serde_json 拒绝解析含字面控制字符的字符串，这里做一次容错预处理。
    let escaped = escape_raw_control_chars_in_strings(&cleaned);
    let v: serde_json::Value = serde_json::from_str(&escaped)
        .or_else(|_| {
            // fallback：LLM 在 JSON 前有说明文字且无 fence，提取第一个 {...}
            let extracted = extract_json_object(&escaped);
            let extracted_escaped = escape_raw_control_chars_in_strings(extracted);
            serde_json::from_str(&extracted_escaped)
        })
        .map_err(|e| format!("解析 Planner JSON 失败: {e}"))?;
    if v.get("steps").is_none() {
        return Err("输出缺 steps".into());
    }
    // 返回提取后的纯 JSON 文本（去掉前置说明文字），让 Worker 拿到干净 plan。
    Ok(serde_json::to_string(&v).unwrap_or_else(|_| escaped.clone()))
}

/// 从 Reviewer 的 JSON 输出解析是否通过 + 问题列表。
pub fn parse_reviewer_output(output: &str) -> Result<(bool, Vec<String>), String> {
    let trimmed = strip_markdown_fence(output);
    let v: serde_json::Value = serde_json::from_str(trimmed)
        .or_else(|_| {
            // fallback：LLM 在 JSON 前有说明文字且无 fence，提取第一个 {...}
            let extracted = extract_json_object(trimmed);
            serde_json::from_str(extracted)
        })
        .map_err(|e| format!("解析 Reviewer JSON 失败: {e}"))?;
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
///
/// 支持四种情况：
/// 1. 整段以 ```json 开头：去掉 fence
/// 2. 文本中含 ```json ... ``` 代码块（前面有自然语言说明）：提取块内容
/// 3. 都没有：原样返回（trim 后），让 serde 尝试解析
/// 4. 解析失败时由调用方 fallback 到 [`extract_json_object`]
fn strip_markdown_fence(s: &str) -> &str {
    let t = s.trim();
    // 1. 整段以 ```json 或 ``` 开头
    if let Some(rest) = t.strip_prefix("```json") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    if let Some(rest) = t.strip_prefix("```") {
        return rest.trim_start().trim_end_matches("```").trim();
    }
    // 2. 文本中间含 ```json ... ``` 块（LLM 在 JSON 前有说明文字）
    if let Some(start) = t.find("```json") {
        let after = &t[start + "```json".len()..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    if let Some(start) = t.find("```") {
        let after = &t[start + "```".len()..];
        if let Some(end) = after.find("```") {
            return after[..end].trim();
        }
    }
    t
}

/// 从混合文本中提取第一个 JSON 对象（`{` 到匹配的最后一个 `}`）。
///
/// 用于 LLM 在 JSON 前后有自然语言说明文字、且无 ``` fence 的情况。
/// 策略：找文本中第一个 `{`，再找最后一个 `}`，取中间子串。
/// 如果找不到完整 `{...}` 返回原文本。
fn extract_json_object(s: &str) -> &str {
    let t = s.trim();
    let Some(first_brace) = t.find('{') else {
        return t;
    };
    let Some(last_brace) = t.rfind('}') else {
        return t;
    };
    if last_brace <= first_brace {
        return t;
    }
    t[first_brace..=last_brace].trim()
}

/// 剥除 DeepSeek 等模型可能输出的 XML 格式 tool_calls 块。
///
/// 形如：
/// ```text
/// <｜｜DSML｜｜tool_calls>
/// <｜｜DSML｜｜invoke name="read_file">
/// <｜｜DSML｜｜parameter name="path" string="true">xxx</｜｜DSML｜｜parameter>
/// </｜｜DSML｜｜invoke>
/// </｜｜DSML｜｜tool_calls>
/// ```
///
/// 这些块不是 JSON，会污染解析。连续剥除所有匹配的块后返回剩余文本。
pub fn strip_xml_tool_calls(s: &str) -> String {
    let mut result = s.to_string();
    loop {
        // 找 tool_calls 开始标记
        if let Some(start) = result.find("<｜｜DSML｜｜tool_calls>") {
            // 找结束标记
            if let Some(end) = result[start..].find("</｜｜DSML｜｜tool_calls>") {
                let block_end = start + end + "</｜｜DSML｜｜tool_calls>".len();
                // 去掉整个 tool_calls 块（含前后换行）
                let before = &result[..start];
                let after = &result[block_end..];
                result = format!("{}{}", before.trim_end(), after.trim_start());
                continue;
            }
        }
        break;
    }
    result.trim().to_string()
}

/// 修复 JSON 字符串值中的未转义控制字符。
///
/// DeepSeek 等 LLM 有时会在 JSON 字符串值中输出字面换行符（`\n` 字符），
/// 这违反 JSON 规范（RFC 8259 要求控制字符必须转义）。serde_json 会拒绝
/// 解析这样的 JSON，报 "control character (\\n) found while parsing a string"
/// 或 "EOF while parsing a string"。
///
/// 本函数用状态机扫描输入，仅替换字符串值（`"..."` 内部）中的字面控制字符
/// 为转义形式，不影响 JSON 结构空白（键值对之间的换行）。
///
/// 处理的控制字符：
/// - `\n`（换行）→ `\\n`
/// - `\r`（回车）→ `\\r`
/// - `\t`（制表符）→ `\\t`
///
/// 正确处理 `\"` 转义引号，不会误判字符串边界。
fn escape_raw_control_chars_in_strings(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let chars = s.chars();
    let mut in_string = false;
    // 字符串内：上一个字符是否是反斜杠（用于判断 \" 是否是转义引号）
    let mut prev_was_backslash = false;

    for c in chars {
        if !in_string {
            result.push(c);
            if c == '"' {
                in_string = true;
                prev_was_backslash = false;
            }
        } else {
            // 当前在字符串值内
            if prev_was_backslash {
                // 前一个字符是反斜杠，当前字符是转义序列的一部分（如 n, ", \, / 等）
                result.push(c);
                // 只有反斜杠本身会延续转义状态（\\ 后面不再是转义）
                prev_was_backslash = c == '\\';
            } else if c == '\\' {
                result.push(c);
                prev_was_backslash = true;
            } else if c == '"' {
                result.push(c);
                in_string = false;
            } else if c == '\n' {
                result.push_str("\\n");
            } else if c == '\r' {
                result.push_str("\\r");
            } else if c == '\t' {
                result.push_str("\\t");
            } else {
                result.push(c);
            }
        }
    }

    result
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
            // 兼容 file:///（标准）和 file://（Windows UNC 路径）两种前缀。
            // Windows 上 worktree 路径可能是 \\?\C:\... 形式，url 构造时
            // 用 file:/// + display() 会产生 file:///\\?\C:\...，
            // strip "file:///" 后得到 \\?\C:\... 才是有效路径。
            let p = url
                .strip_prefix("file:///")
                .or_else(|| url.strip_prefix("file://"))
                .unwrap_or(url);
            let path = std::path::Path::new(p);
            let rel = path.strip_prefix(workspace).unwrap_or(path);
            let rel_str = rel.to_string_lossy().to_string();
            let content = std::fs::read_to_string(path).unwrap_or_default();
            out.push((rel_str, content));
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

    #[test]
    fn strip_fence_extracts_json_from_mixed_text() {
        // LLM 在 JSON 前有说明文字 + ```json fence
        let mixed = "让我分析一下文件。\n\n最终输出 JSON 计划：\n\n```json\n{\"steps\":[]}\n```";
        assert_eq!(strip_markdown_fence(mixed), "{\"steps\":[]}");
    }

    #[test]
    fn strip_fence_extracts_plain_fence_from_mixed_text() {
        let mixed = "说明文字\n```\n{\"a\":1}\n```\n结尾";
        assert_eq!(strip_markdown_fence(mixed), "{\"a\":1}");
    }

    #[test]
    fn parse_planner_output_tolerates_leading_text() {
        // 真实场景：Planner 在 JSON 前有分析文字
        let raw = "文件末尾在第13行。让我制定计划。\n\n```json\n{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}\n```";
        let parsed = parse_planner_output(raw).unwrap();
        assert!(parsed.contains("steps"));
    }

    #[test]
    fn parse_worker_output_with_steps_tolerates_leading_text() {
        let raw = "我已分析完文件。\n\n```json\n{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}\n```";
        match parse_worker_output_with_steps(raw).unwrap() {
            WorkerOutput::Steps(steps) => assert_eq!(steps[0].action, "edit"),
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_planner_output_extracts_bare_json_from_mixed_text() {
        // 真实场景：LLM 在 JSON 前有英文说明文字，无 ``` fence，裸 JSON
        let raw = "The file has 13 lines. I'll append the new function after line 13.\n\n{\n  \"target_files\": [\"a.py\"],\n  \"steps\": [{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]\n}";
        let parsed = parse_planner_output(raw).unwrap();
        assert!(parsed.contains("steps"));
        assert!(parsed.contains("target_files"));
    }

    #[test]
    fn parse_worker_output_with_steps_extracts_bare_json() {
        let raw = "Done analyzing. Here is my plan.\n\n{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}";
        match parse_worker_output_with_steps(raw).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps[0].action, "edit");
                assert_eq!(steps[0].path, "a.py");
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_reviewer_output_extracts_bare_json() {
        let raw = "我审核完文件，结论如下。\n\n{\"approved\":true,\"issues\":[]}";
        let (ok, _) = parse_reviewer_output(raw).unwrap();
        assert!(ok);
    }

    #[test]
    fn extract_json_object_finds_first_to_last_brace() {
        assert_eq!(extract_json_object("no json here"), "no json here");
        assert_eq!(extract_json_object("{\"a\":1}"), "{\"a\":1}");
        assert_eq!(extract_json_object("prefix {\"a\":1} suffix"), "{\"a\":1}");
        assert_eq!(
            extract_json_object("text {\"a\":{\"b\":2}} tail"),
            "{\"a\":{\"b\":2}}"
        );
    }

    #[test]
    fn strip_xml_tool_calls_removes_deepseek_xml_blocks() {
        let raw = "分析文件结构。\n\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">a.py</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>\n\n{\"steps\":[]}";
        let cleaned = strip_xml_tool_calls(raw);
        assert!(!cleaned.contains("tool_calls"), "应剥除 XML tool_calls 块");
        assert!(cleaned.contains("steps"), "应保留 JSON");
    }

    #[test]
    fn strip_xml_tool_calls_handles_multiple_blocks() {
        let raw = "<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>\n{\"steps\":[1]}\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"grep\">\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>";
        let cleaned = strip_xml_tool_calls(raw);
        assert!(!cleaned.contains("tool_calls"), "应剥除所有 XML tool_calls 块");
        assert!(cleaned.contains("steps"), "应保留 JSON");
    }

    #[test]
    fn strip_xml_tool_calls_passes_clean_text() {
        let raw = "{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}";
        let cleaned = strip_xml_tool_calls(raw);
        assert_eq!(cleaned, raw, "无 XML 块时应原样返回");
    }

    #[test]
    fn parse_planner_output_tolerates_xml_tool_calls() {
        let raw = "让我先读取文件。\n\n<｜｜DSML｜｜tool_calls>\n<｜｜DSML｜｜invoke name=\"read_file\">\n<｜｜DSML｜｜parameter name=\"path\" string=\"true\">a.py</｜｜DSML｜｜parameter>\n</｜｜DSML｜｜invoke>\n</｜｜DSML｜｜tool_calls>\n\n```json\n{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}\n```";
        let parsed = parse_planner_output(raw).unwrap();
        assert!(parsed.contains("steps"));
    }

    #[test]
    fn parse_planner_output_tolerates_unescaped_newlines_in_strings() {
        // 真实场景：DeepSeek 在 search/replace 字段值中输出字面换行符
        // （违反 JSON 规范，serde_json 会拒绝解析）。parse_planner_output 应做容错预处理。
        // 这里用 format! 构造含字面换行符的 JSON 字符串值。
        let raw = format!(
            "{{\n  \"target_files\": [\"a.py\"],\n  \"steps\": [\n    {{\n      \"action\": \"edit\",\n      \"path\": \"a.py\",\n      \"search\": \"line1\nline2\",\n      \"replace\": \"new1\nnew2\"\n    }}\n  ]\n}}"
        );
        let parsed = parse_planner_output(&raw).unwrap();
        assert!(parsed.contains("steps"));
        // 解析后的 JSON 应是合法的（serde_json::to_string 会正确转义）
        let reparsed: serde_json::Value = serde_json::from_str(&parsed).unwrap();
        let search = reparsed["steps"][0]["search"].as_str().unwrap();
        assert_eq!(search, "line1\nline2");
    }

    #[test]
    fn escape_control_chars_preserves_structure_whitespace() {
        // JSON 结构中的换行（键值对之间）应保留，字符串值内的换行应转义
        let input = "{\n  \"key\": \"value with\nnewline\",\n  \"num\": 1\n}";
        let escaped = escape_raw_control_chars_in_strings(input);
        // 结构换行保留
        assert!(escaped.starts_with("{\n"));
        // 字符串内换行被转义
        assert!(escaped.contains("value with\\nnewline"));
        // 应能被 serde_json 解析
        let v: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(v["key"].as_str().unwrap(), "value with\nnewline");
        assert_eq!(v["num"].as_i64().unwrap(), 1);
    }

    #[test]
    fn escape_control_chars_handles_escaped_quote_correctly() {
        // 字符串值中的 \" 不应被误判为字符串结束
        let input = "{\"k\": \"has \\\"quoted\\\" line\nbreak\"}";
        let escaped = escape_raw_control_chars_in_strings(input);
        let v: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(v["k"].as_str().unwrap(), "has \"quoted\" line\nbreak");
    }

    #[test]
    fn escape_control_chars_handles_backslash_n_already_escaped() {
        // 已正确转义的 \n（反斜杠+n 两个字符）不应被重复转义
        let input = "{\"k\": \"line1\\nline2\"}";
        let escaped = escape_raw_control_chars_in_strings(input);
        // 不应变（已经是合法 JSON）
        assert_eq!(escaped, input);
        let v: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(v["k"].as_str().unwrap(), "line1\nline2");
    }

    // ---- parse_worker_output_with_steps: edit/create/delete steps 解析 ----

    #[test]
    fn parse_worker_output_with_steps_parses_edit() {
        let out = r#"{"steps":[{"action":"edit","path":"src/a.py","search":"print('hi')","replace":"print('hello')"}]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps.len(), 1);
                assert_eq!(steps[0].action, "edit");
                assert_eq!(steps[0].path, "src/a.py");
                assert_eq!(steps[0].search.as_deref(), Some("print('hi')"));
                assert_eq!(steps[0].replace.as_deref(), Some("print('hello')"));
                assert!(steps[0].content.is_none());
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_parses_create() {
        let out = r#"{"steps":[{"action":"create","path":"new.py","content":"print(1)\n"}]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps[0].action, "create");
                assert_eq!(steps[0].content.as_deref(), Some("print(1)\n"));
                assert!(steps[0].search.is_none());
                assert!(steps[0].replace.is_none());
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_parses_delete() {
        let out = r#"{"steps":[{"action":"delete","path":"old.py"}]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps[0].action, "delete");
                assert_eq!(steps[0].path, "old.py");
                assert!(steps[0].content.is_none());
                assert!(steps[0].search.is_none());
                assert!(steps[0].replace.is_none());
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_parses_multiple_steps() {
        let out = r#"{"steps":[
            {"action":"edit","path":"a.py","search":"x","replace":"y"},
            {"action":"create","path":"b.py","content":"z\n"},
            {"action":"delete","path":"c.py"}
        ]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps.len(), 3);
                assert_eq!(steps[0].action, "edit");
                assert_eq!(steps[1].action, "create");
                assert_eq!(steps[2].action, "delete");
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_falls_back_to_files() {
        // 旧格式：无 steps 字段，有 files 数组。
        let out = r#"{"files":[{"path":"a.py","content":"print(1)\n"}]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Files(files) => {
                assert_eq!(files.len(), 1);
                assert_eq!(files[0].0, "a.py");
                assert_eq!(files[0].1, "print(1)\n");
            }
            other => panic!("expected Files, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_tolerates_markdown_fence() {
        let out = "```json\n{\"steps\":[{\"action\":\"edit\",\"path\":\"a.py\",\"search\":\"x\",\"replace\":\"y\"}]}\n```";
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => {
                assert_eq!(steps[0].action, "edit");
            }
            other => panic!("expected Steps, got {other:?}"),
        }
    }

    #[test]
    fn parse_worker_output_with_steps_fails_on_invalid_json() {
        assert!(parse_worker_output_with_steps("not json").is_err());
    }

    #[test]
    fn parse_worker_output_with_steps_fails_without_steps_or_files() {
        assert!(parse_worker_output_with_steps(r#"{"foo":1}"#).is_err());
    }

    #[test]
    fn parse_worker_output_with_steps_prefers_steps_over_files() {
        // 同时有 steps 和 files：优先返回 steps。
        let out = r#"{"steps":[{"action":"edit","path":"a.py","search":"x","replace":"y"}],"files":[{"path":"b.py","content":"z"}]}"#;
        match parse_worker_output_with_steps(out).unwrap() {
            WorkerOutput::Steps(steps) => assert_eq!(steps.len(), 1),
            WorkerOutput::Files(_) => panic!("应优先返回 Steps"),
        }
    }

    #[test]
    fn worker_system_prompt_documents_edit_format() {
        // 防退化：prompt 必须明确说明 edit/search/replace 格式，
        // 否则 LLM 不会自然产出 steps 格式。
        assert!(WORKER_SYSTEM.contains("edit"));
        assert!(WORKER_SYSTEM.contains("search"));
        assert!(WORKER_SYSTEM.contains("replace"));
        assert!(WORKER_SYSTEM.contains("create"));
        assert!(WORKER_SYSTEM.contains("delete"));
    }
}
