use std::path::Path;

use orcha_llm::{ChatMessage, LlmClient, LlmError, ToolDefinition};

use crate::audit::AuditLogger;
use crate::path_guard::PathGuard;

/// agent loop 最大工具调用轮次。
pub const MAX_TOOL_TURNS: usize = 5;

/// 构建所有可用工具的定义。
pub fn all_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "read_file",
            "读取 workspace 中的文件内容。返回带行号的完整文件内容。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对于 workspace 根目录的文件路径，例如 src/main.py"
                    }
                },
                "required": ["path"]
            }),
        ),
        ToolDefinition::new(
            "grep",
            "在 workspace 文件中搜索匹配文本。返回匹配行及其文件路径和行号。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "搜索的文本模式（子串匹配）"
                    },
                    "path": {
                        "type": "string",
                        "description": "可选：限定搜索的目录或文件路径。不提供则搜索全部文件。"
                    }
                },
                "required": ["pattern"]
            }),
        ),
        ToolDefinition::new(
            "glob",
            "按 glob 模式查找文件。返回匹配的文件路径列表。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "glob 模式，例如 **/*.py 或 src/**/*.rs"
                    }
                },
                "required": ["pattern"]
            }),
        ),
        ToolDefinition::new(
            "list_dir",
            "列出目录中的文件和子目录。返回文件/目录名列表。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "可选：相对于 workspace 根目录的路径。不提供则列出根目录。"
                    }
                },
                "required": []
            }),
        ),
    ]
}

/// 执行单个工具调用并返回结果文本。
///
/// 返回值直接作为 `tool` 消息的 content 传回 LLM。
/// `agent_name` 用于审计日志标注来源。
/// `audit` 为可选审计日志器。
pub fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
) -> String {
    match name {
        "read_file" => execute_read_file(args, workspace, guard, agent_name, audit),
        "grep" => execute_grep(args, workspace, guard, agent_name, audit),
        "glob" => execute_glob(args, workspace),
        "list_dir" => execute_list_dir(args, workspace, guard, agent_name, audit),
        _ => format!("未知工具: {name}"),
    }
}

fn execute_read_file(
    args: &serde_json::Value,
    _workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
) -> String {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return "错误: 缺少 path 参数".to_string(),
    };

    let resolved = match guard.validate_read(path_str) {
        Ok(p) => {
            if let Some(a) = audit {
                a.log_read(agent_name, path_str, &p, true, "");
            }
            p
        }
        Err(e) => {
            let reason = e.to_string();
            if let Some(a) = audit {
                a.log_read(agent_name, path_str, Path::new(""), false, &reason);
            }
            return format!("读取拒绝: {e}");
        }
    };

    let content = match std::fs::read_to_string(&resolved) {
        Ok(c) => c,
        Err(e) => return format!("读取失败: {e}"),
    };

    if PathGuard::is_binary_content(content.as_bytes()) {
        return format!("拒绝: {} 检测为二进制文件", path_str);
    }

    let numbered: String = content
        .lines()
        .enumerate()
        .map(|(i, line)| format!("{:>6}|{}", i + 1, line))
        .collect::<Vec<_>>()
        .join("\n");

    if numbered.is_empty() {
        format!("{} (空文件)", path_str)
    } else {
        format!("{}:\n{numbered}", path_str)
    }
}

fn execute_grep(
    args: &serde_json::Value,
    workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
) -> String {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return "错误: 缺少 pattern 参数".to_string(),
    };

    if pattern.is_empty() {
        return "错误: pattern 不能为空".to_string();
    }

    let search_root = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => match guard.validate_read(p) {
            Ok(r) => {
                if let Some(a) = audit {
                    a.log_read(agent_name, p, &r, true, "");
                }
                r
            }
            Err(e) => {
                let reason = e.to_string();
                if let Some(a) = audit {
                    a.log_read(agent_name, p, Path::new(""), false, &reason);
                }
                return format!("路径拒绝: {e}");
            }
        },
        None => workspace.to_path_buf(),
    };

    let mut results: Vec<String> = Vec::new();
    let mut total_matches = 0usize;
    let max_total = 200usize;

    let walk_result = walk_files(&search_root, guard);
    for file_path in &walk_result.files {
        if total_matches >= max_total {
            break;
        }

        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        if content.len() > 500_000 {
            continue;
        }

        let rel = file_path
            .strip_prefix(workspace)
            .unwrap_or(file_path)
            .to_string_lossy()
            .to_string()
            .replace('\\', "/");

        let mut file_matches: Vec<String> = Vec::new();
        for (line_no, line) in content.lines().enumerate() {
            if total_matches >= max_total {
                break;
            }
            if line.contains(pattern) {
                file_matches.push(format!("{:>6}:{}", line_no + 1, line));
                total_matches += 1;
            }
        }

        if !file_matches.is_empty() {
            results.push(format!("--- {rel} ---"));
            results.extend(file_matches);
        }
    }

    for err in &walk_result.errors {
        results.push(format!("跳过: {err}"));
    }

    if results.is_empty() {
        format!("未找到匹配 \"{pattern}\" 的内容")
    } else {
        if total_matches >= max_total {
            results.push(format!("(达到上限 {max_total} 条，已截断)"));
        }
        results.join("\n")
    }
}

fn execute_glob(args: &serde_json::Value, workspace: &Path) -> String {
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return "错误: 缺少 pattern 参数".to_string(),
    };

    if pattern.is_empty() {
        return "错误: pattern 不能为空".to_string();
    }

    let mut matches: Vec<String> = Vec::new();
    let entries = collect_all_files(workspace, workspace);

    for entry in &entries {
        let rel = entry
            .strip_prefix(workspace)
            .unwrap_or(entry)
            .to_string_lossy()
            .to_string()
            .replace('\\', "/");

        if simple_glob_match(pattern, &rel) {
            matches.push(rel);
        }
    }

    if matches.is_empty() {
        format!("未找到匹配 \"{pattern}\" 的文件")
    } else {
        matches.sort();
        if matches.len() > 200 {
            let total = matches.len();
            matches.truncate(200);
            matches.push(format!("... 共 {total} 个文件，已截断至 200 条"));
        }
        matches.join("\n")
    }
}

fn execute_list_dir(
    args: &serde_json::Value,
    workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
) -> String {
    let target = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => match guard.validate_read(p) {
            Ok(r) => {
                if let Some(a) = audit {
                    a.log_read(agent_name, p, &r, true, "");
                }
                r
            }
            Err(e) => {
                let reason = e.to_string();
                if let Some(a) = audit {
                    a.log_read(agent_name, p, Path::new(""), false, &reason);
                }
                return format!("路径拒绝: {e}");
            }
        },
        None => workspace.to_path_buf(),
    };

    let dir = match std::fs::read_dir(&target) {
        Ok(d) => d,
        Err(e) => return format!("读取目录失败: {e}"),
    };

    let mut entries: Vec<String> = Vec::new();
    for entry in dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let prefix = if is_dir { "📁 " } else { "📄 " };
        entries.push(format!("{prefix}{name}"));
    }

    entries.sort();

    let rel = target
        .strip_prefix(workspace)
        .unwrap_or(&target)
        .to_string_lossy()
        .to_string()
        .replace('\\', "/");

    if entries.is_empty() {
        format!("{rel}/ (空目录)")
    } else {
        format!("{rel}/:\n{}", entries.join("\n"))
    }
}

struct WalkResult {
    files: Vec<std::path::PathBuf>,
    errors: Vec<String>,
}

fn walk_files(root: &Path, guard: &PathGuard) -> WalkResult {
    let mut result = WalkResult {
        files: Vec::new(),
        errors: Vec::new(),
    };
    walk_files_recursive(root, guard, &mut result);
    result
}

fn walk_files_recursive(dir: &Path, guard: &PathGuard, result: &mut WalkResult) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            result.errors.push(format!("{dir:?}: {err}"));
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();

        let rel_str = path.to_string_lossy().to_string().replace('\\', "/");

        if guard.is_protected_read_path(&rel_str) {
            continue;
        }

        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if name.starts_with('.') && name != "." && name != ".."
                || name == "node_modules"
                || name == "__pycache__"
                || name == "target"
            {
                continue;
            }
            walk_files_recursive(&path, guard, result);
        } else if path.is_file() {
            if path.metadata().map(|m| m.len() > 500_000).unwrap_or(true) {
                continue;
            }
            result.files.push(path);
        }
    }
}

fn collect_all_files(root: &Path, base: &Path) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    collect_files_recursive(root, base, &mut files);
    files
}

fn collect_files_recursive(dir: &Path, _base: &Path, files: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.is_dir() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            if name.starts_with('.') && name != "." && name != ".."
                || name == "node_modules"
                || name == "__pycache__"
                || name == "target"
            {
                continue;
            }
            collect_files_recursive(&path, _base, files);
        } else {
            files.push(path);
        }
    }
}

fn simple_glob_match(pattern: &str, path: &str) -> bool {
    let parts: Vec<&str> = pattern.split('/').collect();
    let path_parts: Vec<&str> = path.split('/').collect();
    glob_match_segments(&parts, &path_parts, 0, 0)
}

fn glob_match_segments(pattern: &[&str], path: &[&str], pi: usize, si: usize) -> bool {
    if pi == pattern.len() {
        return si == path.len();
    }

    if pattern[pi] == "**" {
        if glob_match_segments(pattern, path, pi + 1, si) {
            return true;
        }
        for next_si in si..path.len() {
            if glob_match_segments(pattern, path, pi + 1, next_si + 1) {
                return true;
            }
        }
        return false;
    }

    if si >= path.len() {
        return false;
    }

    if segment_matches(pattern[pi], path[si]) {
        return glob_match_segments(pattern, path, pi + 1, si + 1);
    }

    false
}

fn segment_matches(pat: &str, name: &str) -> bool {
    if pat == "*" {
        return true;
    }
    if !pat.contains('*') && !pat.contains('?') {
        return pat == name;
    }

    let pat_bytes = pat.as_bytes();
    let name_bytes = name.as_bytes();
    let mut pi = 0usize;
    let mut ni = 0usize;
    let mut star_idx = None;
    let mut match_idx = 0usize;

    while ni < name_bytes.len() {
        if pi < pat_bytes.len() && pat_bytes[pi] == b'*' {
            star_idx = Some(pi);
            match_idx = ni;
            pi += 1;
        } else if pi < pat_bytes.len() && (pat_bytes[pi] == b'?' || pat_bytes[pi] == name_bytes[ni])
        {
            pi += 1;
            ni += 1;
        } else if let Some(si) = star_idx {
            pi = si + 1;
            match_idx += 1;
            ni = match_idx;
        } else {
            return false;
        }
    }

    while pi < pat_bytes.len() && pat_bytes[pi] == b'*' {
        pi += 1;
    }

    pi == pat_bytes.len()
}

/// 运行 agent loop：LLM 与工具执行交替，直到 LLM 产出最终答案或触达最大轮次。
///
/// `initial_messages` 应包含 system + 首条 user 消息（及可选的 memory 注入）。
/// `agent_name` 用于审计日志标注来源。
/// `audit` 为可选审计日志器。
/// 返回 LLM 最终文本回复。
#[allow(clippy::too_many_arguments)]
pub fn run_agent_loop(
    client: &dyn LlmClient,
    initial_messages: Vec<ChatMessage>,
    tools: &[ToolDefinition],
    workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
    max_turns: usize,
) -> Result<String, LlmError> {
    let mut messages = initial_messages;

    for _turn in 0..max_turns {
        let response = match client.chat_with_tools(&messages, tools) {
            Ok(r) => r,
            Err(LlmError::Parse(_)) if _turn > 0 => {
                let text = client.chat(&messages)?;
                return Ok(text);
            }
            Err(e) => return Err(e),
        };

        if !response.has_tool_calls() {
            return Ok(response.content.unwrap_or_default());
        }

        let assistant_content = response.content.unwrap_or_default();
        let tool_calls = response.tool_calls.clone();
        messages.push(ChatMessage::assistant_with_tool_calls(
            assistant_content,
            tool_calls.clone(),
        ));

        for call in &tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
            let result = execute_tool(
                &call.function.name,
                &args,
                workspace,
                guard,
                agent_name,
                audit,
            );
            messages.push(ChatMessage::tool(&call.id, result));
        }
    }

    let text = client.chat(&messages)?;
    Ok(text)
}
