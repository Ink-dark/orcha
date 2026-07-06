//! M4 plan schema 与文件编辑引擎。
//!
//! 升级 M2/M3 的 `CreatePlan`（只能新建文件）为支持 edit / create / delete 的
//! 通用 [`Plan`]。配合 [`PathGuard`](crate::PathGuard) 强制写边界：
//!
//! - `target_files` 是 plan 级别白名单，Worker 写任何不在列表里的路径直接拒绝
//! - 每个 step 声明 `action: "edit" | "create" | "delete"`，Worker 严格按 action 执行
//! - `edit` 模式用 search-and-replace：`search` 字段必须在原文件中精确匹配且唯一
//!
//! ## search-and-replace 编辑格式
//!
//! 与 Aider 的 SEARCH/REPLACE block 思路一致，但用 JSON 表达：
//!
//! ```json
//! {
//!   "action": "edit",
//!   "path": "src/hello.py",
//!   "search": "print('hello')",
//!   "replace": "print('hello, world')"
//! }
//! ```
//!
//! - `search` 必须在原文件中**精确匹配且唯一**，否则失败（防 LLM 模糊替换）
//! - 多次替换用多个 step，每个 step 一个 search/replace 对
//! - 完整删除一段内容用 `replace: ""`

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// 一次编辑动作。所有 path 都是相对 workspace 的 POSIX 路径（用 `/`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// `edit` / `create` / `delete`。
    pub action: PlanAction,
    /// 相对 workspace 的路径，必须 POSIX 风格（`/` 分隔，无 `\` / `..` / 绝对路径）。
    pub path: String,
    /// `create` / `edit` 模式：要写入或替换后的内容。
    /// `delete` 模式：忽略。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// `edit` 模式：要在原文件中查找的精确字符串。必须唯一匹配。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    /// `edit` 模式：替换 `search` 的新内容。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replace: Option<String>,
}

/// 单步动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanAction {
    /// 编辑已存在文件。需 `search` + `replace`。原文件中 `search` 必须唯一匹配。
    Edit,
    /// 创建新文件。需 `content`。文件必须不存在。
    Create,
    /// 删除已存在文件。
    Delete,
}

impl PlanAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanAction::Edit => "edit",
            PlanAction::Create => "create",
            PlanAction::Delete => "delete",
        }
    }
}

/// Plan：一组编辑动作 + 写白名单。
///
/// `target_files` 是 plan 级别白名单。Worker 会校验所有 step 的 path 都在
/// `target_files` 内（除非 `target_files` 为空，此时兼容旧路径不强制）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    /// 允许触碰的文件白名单。空表示不强制（兼容旧 CreatePlan 路径）。
    #[serde(default)]
    pub target_files: Vec<String>,
    /// 执行步骤。Worker 按 order 顺序执行。
    pub steps: Vec<PlanStep>,
}

impl Plan {
    /// 校验 plan 内部一致性：
    /// - 所有 step.path 都在 `target_files` 内（若 target_files 非空）
    /// - `edit` 必须有 search + replace
    /// - `create` 必须有 content
    /// - `delete` 不需要 content/search/replace
    /// - path 不含 `..` / `\` / 绝对路径前缀
    pub fn validate(&self) -> Result<()> {
        for step in &self.steps {
            validate_relative_path(&step.path)?;
            // 白名单校验（仅在 target_files 非空时强制）
            if !self.target_files.is_empty() {
                let normalized = normalize(&step.path);
                let allowed: Vec<String> = self.target_files.iter().map(|s| normalize(s)).collect();
                if !allowed.contains(&normalized) {
                    bail!(
                        "step path {} 不在 target_files 白名单内: {:?}",
                        step.path,
                        self.target_files
                    );
                }
            }
            match step.action {
                PlanAction::Edit => {
                    if step.search.is_none() {
                        bail!("edit step 缺 search 字段: {}", step.path);
                    }
                    if step.replace.is_none() {
                        bail!("edit step 缺 replace 字段: {}", step.path);
                    }
                }
                PlanAction::Create => {
                    if step.content.is_none() {
                        bail!("create step 缺 content 字段: {}", step.path);
                    }
                }
                PlanAction::Delete => {}
            }
        }
        Ok(())
    }

    /// 从 plan 派生 `target_files`（若为空，则用所有 step.path 去重填充）。
    /// 便于不显式声明 target_files 的简单 plan 也走白名单路径。
    pub fn effective_target_files(&self) -> Vec<String> {
        if !self.target_files.is_empty() {
            return self.target_files.clone();
        }
        let mut files: Vec<String> = self.steps.iter().map(|s| normalize(&s.path)).collect();
        files.sort();
        files.dedup();
        files
    }
}

/// 校验相对路径合法性：拒绝绝对路径、`..`、`\`。
pub fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("path 为空");
    }
    if path.starts_with('/') || path.starts_with('\\') {
        bail!("path 不得为绝对路径: {path}");
    }
    // Windows 盘符
    if path.len() >= 2 {
        let b = path.as_bytes();
        if b[0].is_ascii_alphabetic() && b[1] == b':' {
            bail!("path 不得含 Windows 盘符: {path}");
        }
    }
    if path.contains('\\') {
        bail!("path 不得含反斜杠分隔符（请用 /）: {path}");
    }
    if path.contains("..") {
        bail!("path 不得含 .. 组件: {path}");
    }
    Ok(())
}

/// 归一化相对路径：去前导 `./` 与多余 `/`。复用 path_guard 的同名逻辑。
fn normalize(p: &str) -> String {
    let mut s = p.trim().to_string();
    while s.starts_with("./") {
        s = s[2..].to_string();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    s
}

// ============================================================
// 文件编辑引擎：apply plan 到 workspace
// ============================================================

/// 一个 step 执行的结果：产出的 unified diff 片段 + 实际操作。
#[derive(Debug, Clone)]
pub struct AppliedStep {
    /// step 对应的 unified diff 片段（含 `diff --git` 行）。delete 模式可能为空字符串。
    pub diff: String,
    /// 操作类型，便于审计。
    pub action: PlanAction,
    /// 实际操作的目标路径（相对 workspace）。
    pub path: String,
}

/// 把单个 step 应用到 workspace，返回 diff 片段。
///
/// 行为：
/// - `create`：要求文件不存在；写入 content；产 `new file mode` diff
/// - `edit`：要求文件存在；用 search-and-replace 替换；产 unified diff（含上下文）
/// - `delete`：要求文件存在；删除文件；产 `deleted file` diff
///
/// 所有路径必须已通过 `PathGuard::validate_*` 校验（本函数只做语义）。
pub fn apply_step(workspace: &Path, step: &PlanStep) -> Result<AppliedStep> {
    let target = workspace.join(&step.path);
    match step.action {
        PlanAction::Create => apply_create(&target, step),
        PlanAction::Edit => apply_edit(&target, step),
        PlanAction::Delete => apply_delete(&target, step),
    }
}

fn apply_create(target: &Path, step: &PlanStep) -> Result<AppliedStep> {
    if target.exists() {
        bail!("create 失败：文件已存在 {}", target.display());
    }
    let content = step
        .content
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("create step 缺 content"))?;
    // 创建父目录
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir: {}", parent.display()))?;
    }
    let normalized = ensure_trailing_newline(content);
    std::fs::write(target, &normalized)
        .with_context(|| format!("write file: {}", target.display()))?;
    let diff = make_create_diff(&step.path, &normalized);
    Ok(AppliedStep {
        diff,
        action: PlanAction::Create,
        path: step.path.clone(),
    })
}

fn apply_edit(target: &Path, step: &PlanStep) -> Result<AppliedStep> {
    if !target.exists() {
        bail!("edit 失败：文件不存在 {}", target.display());
    }
    let original = std::fs::read_to_string(target)
        .with_context(|| format!("read file for edit: {}", target.display()))?;
    let search = step
        .search
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("edit step 缺 search"))?;
    let replace = step
        .replace
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("edit step 缺 replace"))?;

    // 唯一性校验：search 必须在原文件中精确匹配且唯一
    let occurrences = original.matches(search).count();
    if occurrences == 0 {
        bail!(
            "edit 失败：search 字符串在 {} 中未找到匹配。search={:?}",
            step.path,
            search
        );
    }
    if occurrences > 1 {
        bail!(
            "edit 失败：search 字符串在 {} 中匹配 {} 次（必须唯一）。search={:?}",
            step.path,
            occurrences,
            search
        );
    }

    let updated = original.replacen(search, replace, 1);
    std::fs::write(target, &updated)
        .with_context(|| format!("write edited file: {}", target.display()))?;

    let diff = make_edit_diff(&step.path, &original, &updated);
    Ok(AppliedStep {
        diff,
        action: PlanAction::Edit,
        path: step.path.clone(),
    })
}

fn apply_delete(target: &Path, step: &PlanStep) -> Result<AppliedStep> {
    if !target.exists() {
        bail!("delete 失败：文件不存在 {}", target.display());
    }
    let original = std::fs::read_to_string(target).ok();
    std::fs::remove_file(target).with_context(|| format!("delete file: {}", target.display()))?;
    let diff = make_delete_diff(&step.path, original.as_deref());
    Ok(AppliedStep {
        diff,
        action: PlanAction::Delete,
        path: step.path.clone(),
    })
}

// ============================================================
// unified diff 生成
// ============================================================

/// 为新建文件生成 unified diff（沿用 sub_agents 的格式，便于 git apply）。
pub fn make_create_diff(filename: &str, content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if content.is_empty() || lines.is_empty() {
        return format!(
            "diff --git a/{f} b/{f}\nnew file mode 100644\n--- /dev/null\n+++ b/{f}\n@@ -0,0 +1,0 @@\n",
            f = filename
        );
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
    out
}

/// 为已存在文件的修改生成 unified diff。包含上下文行（前后各 3 行，clamped）。
///
/// 简化实现：对原文件和新文件做行级 diff（基于 search/replace 的位置），
/// 产 @@ -old_start,old_len +new_start,new_len @@ hunk。
pub fn make_edit_diff(filename: &str, original: &str, updated: &str) -> String {
    let orig_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();

    // 找第一个不同的行 + 最后一个不同的行。
    let first_diff = orig_lines
        .iter()
        .zip(new_lines.iter())
        .position(|(a, b)| a != b);
    let last_diff_orig = (0..orig_lines.len())
        .rev()
        .zip((0..new_lines.len()).rev())
        .find(|(i, j)| orig_lines[*i] != new_lines[*j]);

    let (Some(first), Some(last)) = (first_diff, last_diff_orig) else {
        // 完全相同：返回空 diff（不应该发生）
        return format!("diff --git a/{f} b/{f}\n", f = filename);
    };

    // 上下文：前后各 3 行
    let ctx = 3;
    let ctx_start = first.saturating_sub(ctx);
    // 旧文件的结尾位置：last.0 是 orig 的索引，但替换可能让 new 变长/短，
    // 取 max(orig_last, new_last) 作为 hunk 结尾
    let orig_end = (last.0 + ctx).min(orig_lines.len().saturating_sub(1));
    let new_end = (last.1 + ctx).min(new_lines.len().saturating_sub(1));

    let mut out = String::new();
    out.push_str(&format!("diff --git a/{f} b/{f}\n", f = filename));
    out.push_str(&format!("--- a/{f}\n", f = filename));
    out.push_str(&format!("+++ b/{f}\n", f = filename));

    let orig_hunk_len = orig_end - ctx_start + 1;
    let new_hunk_len = new_end - ctx_start + 1;
    out.push_str(&format!(
        "@@ -{start},{old_len} +{start},{new_len} @@\n",
        start = ctx_start + 1, // 1-based
        old_len = orig_hunk_len,
        new_len = new_hunk_len,
    ));

    // 输出上下文 + 改动行
    for i in ctx_start..=orig_end {
        if i < first || i > last.0 {
            // 上下文行（与 new 相同的位置）
            let new_i = i.min(new_lines.len() - 1);
            out.push(' ');
            out.push_str(new_lines.get(new_i).unwrap_or(&""));
            out.push('\n');
        } else {
            // 改动行：先输出所有删除（-），后输出所有新增（+）
            // 简化：i 是 orig 索引，原文件在 [first..last.0] 范围内的行都是删除
            out.push('-');
            out.push_str(orig_lines.get(i).unwrap_or(&""));
            out.push('\n');
        }
    }
    // 输出新增行（new 文件中 [first..last.1] 范围内）
    for j in first..=last.1.min(new_end) {
        // 跳过与 orig 完全相同的行（已是上下文）
        let orig_j = j.min(orig_lines.len() - 1);
        if j > last.0 || orig_lines.get(orig_j) != Some(&new_lines[j]) {
            out.push('+');
            out.push_str(new_lines.get(j).unwrap_or(&""));
            out.push('\n');
        }
    }

    out
}

/// 为删除文件生成 unified diff。
pub fn make_delete_diff(filename: &str, original: Option<&str>) -> String {
    let mut out = String::new();
    out.push_str(&format!("diff --git a/{f} b/{f}\n", f = filename));
    out.push_str("deleted file mode 100644\n");
    out.push_str(&format!("--- a/{f}\n", f = filename));
    out.push_str("+++ /dev/null\n");
    let content = original.unwrap_or("");
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        out.push_str("@@ -0,0 +0,0 @@\n");
    } else {
        out.push_str(&format!("@@ -1,{n} +0,0 @@\n", n = lines.len()));
        for line in &lines {
            out.push('-');
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 从 patch 文本中抽取所有改动过的文件路径（`+++ b/<path>` 行）。
/// 用于 Reviewer diff 范围校验。
pub fn extract_changed_files(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        // +++ b/<path> 或 +++ /dev/null（删除时 +new 为 /dev/null，但 --- a/<path> 仍是文件）
        if let Some(rest) = line.strip_prefix("+++ b/") {
            files.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("--- a/") {
            // 删除文件时 +++ /dev/null，需要从 --- a/ 取
            if !files.contains(&rest.trim().to_string()) {
                files.push(rest.trim().to_string());
            }
        }
    }
    files.sort();
    files.dedup();
    files
}

/// 校验 patch 改动文件集合是否 ⊆ target_files。
/// 返回 Ok(()) 表示合规，Err 列出超范围文件。
pub fn check_diff_scope(diff: &str, target_files: &[String]) -> Result<()> {
    if target_files.is_empty() {
        return Ok(());
    }
    let changed = extract_changed_files(diff);
    let allowed: Vec<String> = target_files.iter().map(|s| normalize(s)).collect();
    let mut offenders = Vec::new();
    for f in changed {
        if !allowed.contains(&normalize(&f)) {
            offenders.push(f);
        }
    }
    if offenders.is_empty() {
        Ok(())
    } else {
        bail!(
            "patch 改动了未声明的文件: {}（target_files: {:?}）",
            offenders.join(", "),
            target_files
        )
    }
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

// ============================================================
// 兼容旧 CreatePlan：从 "创建 <filename> 输出 <content>" 派生 Plan
// ============================================================

/// 解析旧式任务描述并生成一个等价的 Plan。
/// 仅在 LLM 未启用或调试路径使用。生产路径走 LLM 产 Plan JSON。
pub fn plan_from_create_desc(desc: &str) -> Result<Plan> {
    let desc = desc.trim();
    let (create_kw, output_kw) = if desc.to_lowercase().starts_with("创建") {
        ("创建", "输出")
    } else if desc.to_lowercase().starts_with("create ") {
        ("create ", "output ")
    } else {
        bail!("无法识别的任务描述，期望形如 '创建 <filename> 输出 <content>'");
    };
    let after_create = &desc[create_kw.len()..].trim_start();
    let output_idx = after_create
        .find(output_kw)
        .ok_or_else(|| anyhow::anyhow!("缺少 '{}' 关键字", output_kw.trim()))?;
    let filename = after_create[..output_idx].trim().to_string();
    let content = after_create[output_idx + output_kw.len()..]
        .trim()
        .to_string();
    validate_relative_path(&filename)?;
    Ok(Plan {
        target_files: vec![filename.clone()],
        steps: vec![PlanStep {
            action: PlanAction::Create,
            path: filename,
            content: Some(content),
            search: None,
            replace: None,
        }],
    })
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    // ---- Plan schema ----

    #[test]
    fn plan_validate_passes_for_create() {
        let plan = Plan {
            target_files: vec!["a.py".into()],
            steps: vec![PlanStep {
                action: PlanAction::Create,
                path: "a.py".into(),
                content: Some("x".into()),
                search: None,
                replace: None,
            }],
        };
        plan.validate().unwrap();
    }

    #[test]
    fn plan_validate_rejects_path_outside_target_files() {
        let plan = Plan {
            target_files: vec!["a.py".into()],
            steps: vec![PlanStep {
                action: PlanAction::Create,
                path: "evil.py".into(),
                content: Some("x".into()),
                search: None,
                replace: None,
            }],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains("不在 target_files 白名单"));
    }

    #[test]
    fn plan_validate_rejects_edit_without_search() {
        let plan = Plan {
            target_files: vec![],
            steps: vec![PlanStep {
                action: PlanAction::Edit,
                path: "a.py".into(),
                content: None,
                search: None,
                replace: None,
            }],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains("缺 search"));
    }

    #[test]
    fn plan_validate_rejects_create_without_content() {
        let plan = Plan {
            target_files: vec![],
            steps: vec![PlanStep {
                action: PlanAction::Create,
                path: "a.py".into(),
                content: None,
                search: None,
                replace: None,
            }],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains("缺 content"));
    }

    #[test]
    fn plan_validate_rejects_parent_dir_in_path() {
        let plan = Plan {
            target_files: vec!["../evil.py".into()],
            steps: vec![PlanStep {
                action: PlanAction::Create,
                path: "../evil.py".into(),
                content: Some("x".into()),
                search: None,
                replace: None,
            }],
        };
        let err = plan.validate().unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    #[test]
    fn effective_target_files_derives_from_steps() {
        let plan = Plan {
            target_files: vec![],
            steps: vec![
                PlanStep {
                    action: PlanAction::Create,
                    path: "a.py".into(),
                    content: Some("x".into()),
                    search: None,
                    replace: None,
                },
                PlanStep {
                    action: PlanAction::Edit,
                    path: "b.py".into(),
                    content: None,
                    search: Some("old".into()),
                    replace: Some("new".into()),
                },
            ],
        };
        let files = plan.effective_target_files();
        assert_eq!(files, vec!["a.py", "b.py"]);
    }

    // ---- apply_step: create ----

    #[test]
    fn apply_create_writes_new_file() {
        let dir = tempdir().unwrap();
        let step = PlanStep {
            action: PlanAction::Create,
            path: "hello.py".into(),
            content: Some("print('hi')".into()),
            search: None,
            replace: None,
        };
        let applied = apply_step(dir.path(), &step).unwrap();
        let content = fs::read_to_string(dir.path().join("hello.py")).unwrap();
        assert_eq!(content, "print('hi')\n");
        assert!(applied.diff.contains("new file mode"));
        assert!(applied.diff.contains("+++ b/hello.py"));
        assert!(applied.diff.contains("+print('hi')"));
    }

    #[test]
    fn apply_create_fails_if_file_exists() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "existing").unwrap();
        let step = PlanStep {
            action: PlanAction::Create,
            path: "a.py".into(),
            content: Some("new".into()),
            search: None,
            replace: None,
        };
        let err = apply_step(dir.path(), &step).unwrap_err();
        assert!(err.to_string().contains("已存在"));
    }

    #[test]
    fn apply_create_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let step = PlanStep {
            action: PlanAction::Create,
            path: "src/deep/nested/a.py".into(),
            content: Some("x".into()),
            search: None,
            replace: None,
        };
        apply_step(dir.path(), &step).unwrap();
        assert!(dir.path().join("src/deep/nested/a.py").is_file());
    }

    // ---- apply_step: edit ----

    #[test]
    fn apply_edit_replaces_unique_match() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "print('hello')\n").unwrap();
        let step = PlanStep {
            action: PlanAction::Edit,
            path: "a.py".into(),
            content: None,
            search: Some("print('hello')".into()),
            replace: Some("print('hello, world')".into()),
        };
        let applied = apply_step(dir.path(), &step).unwrap();
        let content = fs::read_to_string(dir.path().join("a.py")).unwrap();
        assert_eq!(content, "print('hello, world')\n");
        assert!(applied.diff.contains("-print('hello')"));
        assert!(applied.diff.contains("+print('hello, world')"));
    }

    #[test]
    fn apply_edit_fails_when_search_not_found() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "print('hello')\n").unwrap();
        let step = PlanStep {
            action: PlanAction::Edit,
            path: "a.py".into(),
            content: None,
            search: Some("print('world')".into()),
            replace: Some("print('bye')".into()),
        };
        let err = apply_step(dir.path(), &step).unwrap_err();
        assert!(err.to_string().contains("未找到匹配"));
    }

    #[test]
    fn apply_edit_fails_when_search_matches_multiple() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "x = 1\nx = 1\n").unwrap();
        let step = PlanStep {
            action: PlanAction::Edit,
            path: "a.py".into(),
            content: None,
            search: Some("x = 1".into()),
            replace: Some("x = 2".into()),
        };
        let err = apply_step(dir.path(), &step).unwrap_err();
        assert!(err.to_string().contains("匹配 2 次"));
    }

    #[test]
    fn apply_edit_fails_when_file_missing() {
        let dir = tempdir().unwrap();
        let step = PlanStep {
            action: PlanAction::Edit,
            path: "missing.py".into(),
            content: None,
            search: Some("x".into()),
            replace: Some("y".into()),
        };
        let err = apply_step(dir.path(), &step).unwrap_err();
        assert!(err.to_string().contains("文件不存在"));
    }

    #[test]
    fn apply_edit_with_empty_replace_deletes_content() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "line1\nremove_me\nline3\n").unwrap();
        let step = PlanStep {
            action: PlanAction::Edit,
            path: "a.py".into(),
            content: None,
            search: Some("remove_me\n".into()),
            replace: Some("".into()),
        };
        apply_step(dir.path(), &step).unwrap();
        let content = fs::read_to_string(dir.path().join("a.py")).unwrap();
        assert_eq!(content, "line1\nline3\n");
    }

    // ---- apply_step: delete ----

    #[test]
    fn apply_delete_removes_file() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.py"), "content\n").unwrap();
        let step = PlanStep {
            action: PlanAction::Delete,
            path: "a.py".into(),
            content: None,
            search: None,
            replace: None,
        };
        let applied = apply_step(dir.path(), &step).unwrap();
        assert!(!dir.path().join("a.py").exists());
        assert!(applied.diff.contains("deleted file mode"));
    }

    #[test]
    fn apply_delete_fails_when_missing() {
        let dir = tempdir().unwrap();
        let step = PlanStep {
            action: PlanAction::Delete,
            path: "nope.py".into(),
            content: None,
            search: None,
            replace: None,
        };
        let err = apply_step(dir.path(), &step).unwrap_err();
        assert!(err.to_string().contains("文件不存在"));
    }

    // ---- diff 解析 ----

    #[test]
    fn extract_changed_files_from_create_diff() {
        let diff = make_create_diff("a.py", "x\n");
        let files = extract_changed_files(&diff);
        assert_eq!(files, vec!["a.py"]);
    }

    #[test]
    fn extract_changed_files_from_delete_diff() {
        let diff = make_delete_diff("a.py", Some("x\n"));
        let files = extract_changed_files(&diff);
        assert_eq!(files, vec!["a.py"]);
    }

    #[test]
    fn check_diff_scope_passes_when_all_in_target() {
        let diff = make_create_diff("a.py", "x\n");
        check_diff_scope(&diff, &["a.py".into()]).unwrap();
    }

    #[test]
    fn check_diff_scope_rejects_off_scope_files() {
        let diff = make_create_diff("evil.py", "x\n");
        let err = check_diff_scope(&diff, &["a.py".into()]).unwrap_err();
        assert!(err.to_string().contains("evil.py"));
    }

    // ---- plan_from_create_desc ----

    #[test]
    fn plan_from_create_desc_builds_single_step_plan() {
        let plan = plan_from_create_desc("创建 hello.py 输出 hello").unwrap();
        assert_eq!(plan.target_files, vec!["hello.py"]);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].action, PlanAction::Create);
        assert_eq!(plan.steps[0].path, "hello.py");
        assert_eq!(plan.steps[0].content.as_deref(), Some("hello"));
    }

    #[test]
    fn plan_from_create_desc_rejects_path_traversal() {
        let err = plan_from_create_desc("创建 ../evil.py 输出 x").unwrap_err();
        assert!(err.to_string().contains(".."));
    }

    // ---- validate_relative_path ----

    #[test]
    fn validate_relative_path_rejects_backslash() {
        let err = validate_relative_path("a\\b.py").unwrap_err();
        assert!(err.to_string().contains("反斜杠"));
    }

    #[test]
    fn validate_relative_path_rejects_drive_letter() {
        let err = validate_relative_path("C:/x.py").unwrap_err();
        assert!(err.to_string().contains("盘符"));
    }

    #[test]
    fn validate_relative_path_rejects_absolute() {
        assert!(validate_relative_path("/etc/passwd").is_err());
    }
}
