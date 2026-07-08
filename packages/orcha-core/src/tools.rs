use std::path::Path;
use std::process::Command;

use orcha_llm::{strip_xml_tool_calls, ChatMessage, LlmClient, LlmError, ToolDefinition};

use crate::approval::{ApprovalAction, ApprovalDecision, ApprovalHook};
use crate::audit::AuditLogger;
use crate::path_guard::PathGuard;

/// agent loop 最大工具调用轮次。
pub const MAX_TOOL_TURNS: usize = 10;

/// 构建只读工具（用于 Worker agent loop）。
/// Worker 只能读文件、搜索、浏览目录，不能跑命令。
/// 编译验证是 Tester 的职责，Worker 不应越权。
pub fn readonly_tools() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition::new(
            "read_file",
            "读取 workspace 中的文件内容。返回带行号的完整文件内容。",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对于 workspace 根目录的文件路径，例如 src/main.rs"
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
                        "description": "glob 模式，例如 **/*.rs 或 src/**/*.rs"
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

/// 构建所有可用工具的定义（含 run_command，供 Tester 等需要执行命令的阶段使用）。
pub fn all_tools() -> Vec<ToolDefinition> {
    let mut tools = readonly_tools();
    tools.push(ToolDefinition::new(
        "run_command",
        "在 workspace 中执行 shell 命令（如 cargo build / npm test / git diff），返回 stdout+stderr 与退出码。命令在 workspace 根目录执行，无法访问 workspace 外文件。每个命令需经人工审批（--approve 模式下会询问）。输出截断到 4000 字符。",
        serde_json::json!({
            "type": "object",
            "properties": {
                "program": {
                    "type": "string",
                    "description": "要执行的程序，例如 cargo / npm / git / python / pytest"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "参数列表，例如 [\"build\", \"--release\"] 或 [\"test\", \"-q\"]"
                }
            },
            "required": ["program"]
        }),
    ));
    tools
}

/// 执行单个工具调用并返回结果文本。
///
/// 返回值直接作为 `tool` 消息的 content 传回 LLM。
/// `agent_name` 用于审计日志标注来源。
/// `audit` 为可选审计日志器。
/// `approval` 为可选人工审批 hook；`run_command` 工具必须经审批才执行，
/// 无 hook 时 fail-closed 拒绝（防 LLM 在无审批环境下乱跑命令）。
#[allow(clippy::too_many_arguments)]
pub fn execute_tool(
    name: &str,
    args: &serde_json::Value,
    workspace: &Path,
    guard: &PathGuard,
    agent_name: &str,
    audit: Option<&AuditLogger>,
    approval: Option<&dyn ApprovalHook>,
) -> String {
    match name {
        "read_file" => execute_read_file(args, workspace, guard, agent_name, audit),
        "grep" => execute_grep(args, workspace, guard, agent_name, audit),
        "glob" => execute_glob(args, workspace),
        "list_dir" => execute_list_dir(args, workspace, guard, agent_name, audit),
        "run_command" => execute_run_command(args, workspace, approval, agent_name),
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

/// run_command 工具输出字符上限。超过则截断尾部，避免 LLM 上下文爆炸。
const MAX_COMMAND_OUTPUT_CHARS: usize = 4000;

/// Worker 工具执行的命令超时（秒）。编译/测试可能很久，给 10 分钟。
const COMMAND_TIMEOUT_SECS: u64 = 600;

fn execute_run_command(
    args: &serde_json::Value,
    workspace: &Path,
    approval: Option<&dyn ApprovalHook>,
    agent_name: &str,
) -> String {
    let program = match args.get("program").and_then(|v| v.as_str()) {
        Some(p) => p.to_string(),
        None => return "错误: 缺少 program 参数".to_string(),
    };
    let cmd_args: Vec<String> = args
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // 审批：无 hook 时 fail-closed 拒绝（防 LLM 在无审批环境下乱跑命令）。
    let action = ApprovalAction::RunCommand {
        program: program.clone(),
        args: cmd_args.clone(),
    };
    match approval {
        Some(hook) => match hook.request(&action) {
            ApprovalDecision::Approved | ApprovalDecision::ApproveAndWhitelist => {}
            ApprovalDecision::Rejected(reason) => {
                eprintln!(
                    "[{agent_name}] 命令被审批拒绝: {program} {} - {reason}",
                    cmd_args.join(" ")
                );
                return format!("命令被审批拒绝: {reason}");
            }
        },
        None => {
            return format!(
                "拒绝执行 {program}: 无审批 hook（fail-closed，需配置 --approve 或注入 ApprovalHook）"
            );
        }
    }

    eprintln!(
        "[{agent_name}] 执行命令: {program} {} (cwd: {}, timeout={COMMAND_TIMEOUT_SECS}s)",
        cmd_args.join(" "),
        workspace.display()
    );

    // 在新线程中执行命令，超时则杀进程
    let program_clone = program.clone();
    let cmd_args_clone = cmd_args.clone();
    let ws_clone = workspace.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = Command::new(&program_clone)
            .args(&cmd_args_clone)
            .current_dir(&ws_clone)
            .output();
        let _ = tx.send(result);
    });

    let output = match rx.recv_timeout(std::time::Duration::from_secs(COMMAND_TIMEOUT_SECS)) {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return format!("执行 {program} 失败: {e}");
        }
        Err(_timeout) => {
            return format!(
                "执行 {program} 超时（>{COMMAND_TIMEOUT_SECS} 秒），已终止。请简化命令或分步执行。"
            );
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit = output.status.code().unwrap_or(-1);

    let mut combined = String::new();
    if !stdout.is_empty() {
        combined.push_str("--- stdout ---\n");
        combined.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str("--- stderr ---\n");
        combined.push_str(&stderr);
    }
    if combined.is_empty() {
        combined = "(无输出)".to_string();
    }

    let truncated = if combined.chars().count() > MAX_COMMAND_OUTPUT_CHARS {
        let mut s: String = combined
            .chars()
            .take(MAX_COMMAND_OUTPUT_CHARS - 1)
            .collect();
        s.push('…');
        s.push_str(&format!(
            "\n(已截断，原始输出超过 {MAX_COMMAND_OUTPUT_CHARS} 字符)"
        ));
        s
    } else {
        combined
    };

    format!("exit code: {exit}\n{truncated}")
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
/// `approval` 为可选人工审批 hook，传入后 `run_command` 工具会经审批才执行；
/// 传 None 时 `run_command` 会 fail-closed 拒绝。
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
    approval: Option<&dyn ApprovalHook>,
) -> Result<String, LlmError> {
    let mut messages = initial_messages;

    for _turn in 0..max_turns {
        let response = match client.chat_with_tools(&messages, tools) {
            Ok(r) => r,
            Err(LlmError::Parse(_)) => {
                // Parse 错误（DSML 或 JSON 格式异常）：降级为不带 tools 的 chat 调用。
                // 无论第几轮都降级，避免第一轮 parse 失败直接报错退出。
                let text = client.chat(&messages)?;
                return Ok(text);
            }
            Err(e) => return Err(e),
        };

        // DeepSeek 可能在 content 中输出 XML 格式的 tool_calls 而非 OpenAI 标准格式。
        // 检测并解析这些 XML tool_calls，执行对应工具。
        let content = response.content.clone().unwrap_or_default();
        let dsml_calls = parse_dsml_tool_calls(&content);
        if !dsml_calls.is_empty() {
            eprintln!(
                "[{agent_name}] 检测到 DeepSeek XML tool_calls（{} 个），正在执行...",
                dsml_calls.len()
            );
            messages.push(ChatMessage::assistant(content.clone()));
            for (name, args_json) in &dsml_calls {
                let args: serde_json::Value =
                    serde_json::from_str(args_json).unwrap_or(serde_json::Value::Null);
                let result =
                    execute_tool(name, &args, workspace, guard, agent_name, audit, approval);
                messages.push(ChatMessage::tool(format!("dsml-{name}"), result));
            }
            continue;
        }

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
                approval,
            );
            messages.push(ChatMessage::tool(&call.id, result));
        }
    }

    let text = client.chat(&messages)?;
    let cleaned = strip_xml_tool_calls(&text);
    if cleaned != text {
        eprintln!(
            "[{agent_name}] 最终输出剥除了 DSML 块（{} → {} 字节）",
            text.len(),
            cleaned.len()
        );
    }
    Ok(cleaned)
}

/// 解析 DeepSeek 的 XML 格式 tool_calls。
///
/// 格式：
/// ```text
/// <｜｜DSML｜｜tool_calls>
/// <｜｜DSML｜｜invoke name="read_file">
/// <｜｜DSML｜｜parameter name="path" string="true">src/main.rs</｜｜DSML｜｜parameter>
/// </｜｜DSML｜｜invoke>
/// </｜｜DSML｜｜tool_calls>
/// ```
///
/// 返回 `(tool_name, arguments_json)` 列表。
fn parse_dsml_tool_calls(content: &str) -> Vec<(String, String)> {
    let marker = "<｜｜DSML｜｜tool_calls>";
    if !content.contains(marker) {
        return vec![];
    }

    let mut results = Vec::new();
    let mut remaining = content;

    while let Some(start) = remaining.find(marker) {
        let after_start = &remaining[start + marker.len()..];
        let end_marker = "</｜｜DSML｜｜tool_calls>";
        let end = match after_start.find(end_marker) {
            Some(e) => e,
            None => break,
        };
        let block = &after_start[..end];
        remaining = &after_start[end + end_marker.len()..];

        // 解析每个 <｜｜DSML｜｜invoke name="XXX"> 块
        let invoke_marker = "<｜｜DSML｜｜invoke name=\"";
        let mut block_remaining = block;
        while let Some(inv_start) = block_remaining.find(invoke_marker) {
            let after_inv = &block_remaining[inv_start + invoke_marker.len()..];
            let name_end = match after_inv.find("\">") {
                Some(e) => e,
                None => break,
            };
            let name = &after_inv[..name_end];
            let after_name = &after_inv[name_end + 2..];

            let invoke_end = "</｜｜DSML｜｜invoke>";
            let inv_end = match after_name.find(invoke_end) {
                Some(e) => e,
                None => break,
            };
            let invoke_body = &after_name[..inv_end];
            block_remaining = &after_name[inv_end + invoke_end.len()..];

            // 解析参数 <｜｜DSML｜｜parameter name="YYY" string="true">VALUE</｜｜DSML｜｜parameter>
            let mut params = serde_json::Map::new();
            let param_marker = "<｜｜DSML｜｜parameter name=\"";
            let mut param_remaining = invoke_body;
            while let Some(p_start) = param_remaining.find(param_marker) {
                let after_p = &param_remaining[p_start + param_marker.len()..];
                let p_name_end = match after_p.find("\"") {
                    Some(e) => e,
                    None => break,
                };
                let p_name = &after_p[..p_name_end];
                // 跳过 " string="true"> 或 ">
                let after_p_name = &after_p[p_name_end..];
                let val_start = match after_p_name.find(">") {
                    Some(e) => e + 1,
                    None => break,
                };
                let after_val_start = &after_p_name[val_start..];
                let param_end = "</｜｜DSML｜｜parameter>";
                let val_end = match after_val_start.find(param_end) {
                    Some(e) => e,
                    None => break,
                };
                let value = &after_val_start[..val_end];
                param_remaining = &after_val_start[val_end + param_end.len()..];

                // 去掉可能的前后空白
                let trimmed_value = value.trim();
                params.insert(
                    p_name.to_string(),
                    serde_json::Value::String(trimmed_value.to_string()),
                );
            }

            let args_json = serde_json::to_string(&serde_json::Value::Object(params))
                .unwrap_or_else(|_| "{}".to_string());
            results.push((name.to_string(), args_json));
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::{ApprovalDecision, MockApprovalHook, NullApprovalHook};
    use crate::path_guard::PathGuard;
    use std::fs;

    fn make_guard(ws: &Path) -> PathGuard {
        PathGuard::new(ws).expect("PathGuard init")
    }

    #[test]
    fn all_tools_includes_run_command() {
        let tools = all_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.function.name.as_str()).collect();
        assert!(
            names.contains(&"run_command"),
            "应包含 run_command 工具: {names:?}"
        );
        assert_eq!(tools.len(), 5, "应有 5 个工具");
    }

    #[test]
    fn run_command_fails_closed_without_approval_hook() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        let args = serde_json::json!({"program": "echo", "args": ["hi"]});
        let result = execute_tool(
            "run_command",
            &args,
            ws.path(),
            &guard,
            "tester",
            None,
            None, // 无 approval hook → fail-closed
        );
        assert!(result.contains("fail-closed"), "无 hook 应拒绝: {result}");
        assert!(result.contains("echo"), "应提及命令名: {result}");
    }

    #[test]
    fn run_command_executes_when_approved() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        // Windows 用 cmd /c echo，跨平台用 echo 不一定存在
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_string(), "echo hello".to_string()])
        } else {
            ("echo", vec!["hello".to_string()])
        };
        let args_json = serde_json::json!({"program": program, "args": args});
        let hook = NullApprovalHook;
        let result = execute_tool(
            "run_command",
            &args_json,
            ws.path(),
            &guard,
            "tester",
            None,
            Some(&hook),
        );
        assert!(result.contains("exit code: 0"), "应成功: {result}");
        assert!(result.contains("hello"), "应含输出: {result}");
    }

    #[test]
    fn run_command_rejected_by_hook() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        let args = serde_json::json!({"program": "echo", "args": ["hi"]});
        let hook = MockApprovalHook::new(vec![ApprovalDecision::Rejected("测试拒绝".into())]);
        let result = execute_tool(
            "run_command",
            &args,
            ws.path(),
            &guard,
            "tester",
            None,
            Some(&hook),
        );
        assert!(result.contains("测试拒绝"), "应含拒绝理由: {result}");
        assert!(result.contains("命令被审批拒绝"), "应提示被拒绝: {result}");
    }

    #[test]
    fn run_command_returns_error_when_missing_program() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        let args = serde_json::json!({"args": ["hi"]});
        let hook = NullApprovalHook;
        let result = execute_tool(
            "run_command",
            &args,
            ws.path(),
            &guard,
            "tester",
            None,
            Some(&hook),
        );
        assert!(result.contains("缺少 program 参数"), "应报错: {result}");
    }

    #[test]
    fn run_command_returns_error_for_nonexistent_program() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        let args = serde_json::json!({"program": "this-program-does-not-exist-12345"});
        let hook = NullApprovalHook;
        let result = execute_tool(
            "run_command",
            &args,
            ws.path(),
            &guard,
            "tester",
            None,
            Some(&hook),
        );
        assert!(
            result.contains("失败") || result.contains("exit code"),
            "应提示失败: {result}"
        );
    }

    #[test]
    fn run_command_truncates_long_output() {
        let ws = tempfile::tempdir().unwrap();
        let guard = make_guard(ws.path());
        // 生成超过 4000 字符的输出
        let long_arg = "A".repeat(5000);
        // 写一个脚本文件并执行
        if cfg!(windows) {
            let script = format!("@echo off\necho {long_arg}");
            fs::write(ws.path().join("long.bat"), script).unwrap();
            let args = serde_json::json!({"program": "cmd", "args": ["/C", "long.bat"]});
            let hook = NullApprovalHook;
            let result = execute_tool(
                "run_command",
                &args,
                ws.path(),
                &guard,
                "tester",
                None,
                Some(&hook),
            );
            assert!(
                result.contains("已截断") || result.contains("exit code"),
                "应截断或退出: {result}"
            );
        } else {
            let args = serde_json::json!({"program": "printf", "args": [format!("'{long_arg}'")]});
            let hook = NullApprovalHook;
            let result = execute_tool(
                "run_command",
                &args,
                ws.path(),
                &guard,
                "tester",
                None,
                Some(&hook),
            );
            assert!(result.contains("已截断"), "应截断: {result}");
        }
    }

    #[test]
    fn parse_dsml_tool_calls_parses_single_invoke() {
        let content = r#"让我先读取文件。

<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="read_file">
<｜｜DSML｜｜parameter name="path" string="true">src/main.rs</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>"#;
        let calls = parse_dsml_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "read_file");
        let args: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(args["path"], "src/main.rs");
    }

    #[test]
    fn parse_dsml_tool_calls_parses_multiple_invokes() {
        let content = r#"<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="grep">
<｜｜DSML｜｜parameter name="pattern" string="true">#[allow(dead_code)]</｜｜DSML｜｜parameter>
<｜｜DSML｜｜parameter name="path" string="true">src/lib.rs</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
<｜｜DSML｜｜invoke name="read_file">
<｜｜DSML｜｜parameter name="path" string="true">src/main.rs</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>"#;
        let calls = parse_dsml_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "grep");
        assert_eq!(calls[1].0, "read_file");
        let args0: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(args0["pattern"], "#[allow(dead_code)]");
        assert_eq!(args0["path"], "src/lib.rs");
    }

    #[test]
    fn parse_dsml_tool_calls_returns_empty_for_plain_text() {
        let calls = parse_dsml_tool_calls("普通文本，没有 tool_calls");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_dsml_tool_calls_handles_multiple_blocks() {
        let content = r#"第一轮

<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="read_file">
<｜｜DSML｜｜parameter name="path" string="true">a.rs</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>

第二轮

<｜｜DSML｜｜tool_calls>
<｜｜DSML｜｜invoke name="grep">
<｜｜DSML｜｜parameter name="pattern" string="true">fn main</｜｜DSML｜｜parameter>
</｜｜DSML｜｜invoke>
</｜｜DSML｜｜tool_calls>"#;
        let calls = parse_dsml_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "read_file");
        assert_eq!(calls[1].0, "grep");
    }
}
