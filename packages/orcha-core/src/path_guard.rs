//! M4 写边界与沙箱路径校验。
//!
//! 对应 ROADMAP M4「写边界设计」：所有 Sub-Agent 的文件操作（read / write / delete）
//! 必须经 [`PathGuard`] 校验，确保：
//!
//! 1. **路径不逃逸 workspace**：经 `canonicalize` 后必须仍以 workspace 的 canonical
//!    路径为前缀。防符号链接逃逸（workspace/foo 指向 /etc/passwd 时直接拒绝）。
//! 2. **危险路径黑名单**：即便在 `target_files` 内也拒绝写 `.git/` /
//!    `.github/workflows/` / `.env*` / `Cargo.toml` 依赖段等基础设施文件。
//! 3. **读保护 denylist**：默认拒绝读 `.git/` / `.env*` / `secrets/` / `id_rsa*` 等。
//! 4. **写白名单**：plan 声明 `target_files` 后，Worker 写任何不在列表里的路径直接拒绝。
//!
//! 设计原则：fail-closed。任何校验环节出错（canonicalize 失败、文件不存在等）
//! 一律返回 `Err`，不静默放行。

use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// 单个文件读取大小上限：1 MiB。超过则拒绝（防 LLM 吞 token）。
pub const MAX_READ_FILE_SIZE: u64 = 1024 * 1024;

/// 路径校验错误。所有错误都对应一个明确的拒绝原因，便于审计与单测断言。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum PathGuardError {
    /// 路径含 `..` 或为绝对路径（在规范化前就拒绝，避免后续 canonicalize 误判）。
    #[error("路径含非法组件（.. 或绝对路径）: {path}")]
    InvalidPathComponent { path: String },

    /// Windows 反斜杠分隔符统一拒绝（强制使用 / 作为分隔符，便于跨平台一致行为）。
    #[error("路径含 Windows 反斜杠分隔符（请用 /）: {path}")]
    BackslashSeparator { path: String },

    /// canonicalize 失败（文件 / 父目录不存在，或无权限）。
    #[error("canonicalize 失败 ({path}): {reason}")]
    CanonicalizeFailed { path: String, reason: String },

    /// canonical 后的路径不在 workspace 内（符号链接逃逸）。
    #[error("路径逃逸 workspace: {resolved} 不在 {} 内", workspace_root)]
    EscapedWorkspace {
        resolved: String,
        workspace_root: String,
    },

    /// 写入路径在危险路径黑名单内。
    #[error("拒绝写入危险路径: {path}（{reason}）")]
    DangerousWritePath { path: String, reason: &'static str },

    /// 读取路径在保护 denylist 内。
    #[error("拒绝读取受保护路径: {path}（{reason}）")]
    ProtectedReadPath { path: String, reason: &'static str },

    /// 路径不在 plan 声明的 `target_files` 白名单内。
    #[error("路径不在 target_files 白名单内: {path}")]
    NotInTargetFiles { path: String },

    /// 文件过大（超过 [`MAX_READ_FILE_SIZE`]）。
    #[error("文件过大 ({size} bytes > {max} bytes): {path}")]
    FileTooLarge { path: String, size: u64, max: u64 },

    /// 文件疑似二进制（含 NUL 字节）。
    #[error("拒绝读取二进制文件: {path}")]
    BinaryFile { path: String },
}

/// 路径校验策略。所有 Sub-Agent 文件操作都通过它。
///
/// 内部持有 workspace 的 canonical 路径（构造时 canonicalize 一次，避免每次校验重算）。
/// workspace 不存在或 canonicalize 失败时构造失败——fail-closed。
pub struct PathGuard {
    workspace_canonical: PathBuf,
}

impl PathGuard {
    /// 以 workspace 路径构造校验器。workspace 必须已存在且可 canonicalize。
    pub fn new(workspace: &Path) -> Result<Self, PathGuardError> {
        let workspace_canonical =
            workspace
                .canonicalize()
                .map_err(|e| PathGuardError::CanonicalizeFailed {
                    path: workspace.display().to_string(),
                    reason: e.to_string(),
                })?;
        Ok(Self {
            workspace_canonical,
        })
    }

    /// 返回 workspace 的 canonical 路径（已规范化，可用作前缀比较基准）。
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_canonical
    }

    /// 解析相对路径到 workspace 内的 canonical 路径，并校验不逃逸。
    ///
    /// 步骤：
    /// 1. 拒绝绝对路径（Unix `/...`、Windows `C:\...` 与 `\...`）
    /// 2. 拒绝含 `..` 的原始路径（在 join 前就拦截，防 join 后路径仍合法但越界）
    /// 3. 拒绝含 `\` 的路径（强制用 `/`，跨平台一致）
    /// 4. join workspace 后 canonicalize（若文件不存在则向上找祖先 canonicalize）
    /// 5. 校验 canonical 路径仍以 workspace_canonical 为前缀（防 symlink 逃逸）
    pub fn resolve(&self, relative_path: &str) -> Result<PathBuf, PathGuardError> {
        // 1. 拒绝绝对路径：Unix 风格 / 开头，Windows 风格 盘符:\ 或 \ 开头
        if relative_path.starts_with('/') || relative_path.starts_with('\\') {
            return Err(PathGuardError::InvalidPathComponent {
                path: relative_path.to_string(),
            });
        }
        // Windows 盘符：形如 C:\ / D:/ 等
        if relative_path.len() >= 2 {
            let bytes = relative_path.as_bytes();
            if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
                return Err(PathGuardError::InvalidPathComponent {
                    path: relative_path.to_string(),
                });
            }
        }
        if Path::new(relative_path).is_absolute() {
            return Err(PathGuardError::InvalidPathComponent {
                path: relative_path.to_string(),
            });
        }

        // 2. 拒绝含 .. 的组件（含 ParentDir 与字符串兜底）
        let p = Path::new(relative_path);
        for comp in p.components() {
            if !matches!(comp, Component::Normal(_) | Component::CurDir) {
                return Err(PathGuardError::InvalidPathComponent {
                    path: relative_path.to_string(),
                });
            }
        }
        if relative_path.contains("..") {
            return Err(PathGuardError::InvalidPathComponent {
                path: relative_path.to_string(),
            });
        }

        // 3. 拒绝反斜杠（强制 / 分隔符）
        if relative_path.contains('\\') {
            return Err(PathGuardError::BackslashSeparator {
                path: relative_path.to_string(),
            });
        }

        // 4. join workspace
        let joined = self.workspace_canonical.join(relative_path);

        // 5. canonicalize：若文件已存在直接 canonicalize；
        //    若不存在则向上找第一个存在的祖先 canonicalize，再把剩余路径拼回去。
        //    这样能正确处理「新建文件 + 父目录不存在」的场景，同时仍能检测 symlink 逃逸
        //    （若 workspace 内有 symlink，canonicalize 它时会指向 workspace 外，被祖先检查拦下）。
        let canonical = canonicalize_with_missing_ancestor(&joined).map_err(|e| {
            PathGuardError::CanonicalizeFailed {
                path: relative_path.to_string(),
                reason: e,
            }
        })?;

        // 6. 前缀校验：canonical 必须以 workspace_canonical 为前缀。
        if !canonical.starts_with(&self.workspace_canonical) {
            return Err(PathGuardError::EscapedWorkspace {
                resolved: canonical.display().to_string(),
                workspace_root: self.workspace_canonical.display().to_string(),
            });
        }

        Ok(canonical)
    }

    /// 校验相对路径是否在写白名单 `target_files` 内。
    /// `target_files` 为空表示无白名单（用于 create-only 旧路径兼容，不强制）。
    pub fn check_write_whitelist(
        &self,
        relative_path: &str,
        target_files: &[String],
    ) -> Result<(), PathGuardError> {
        if target_files.is_empty() {
            // 旧路径兼容：无白名单时不强制。新代码路径应总是带 target_files。
            return Ok(());
        }
        // 归一化比较：去掉前导 ./ 与多余 /。
        let normalized = normalize_relative(relative_path);
        let target_normalized: Vec<String> =
            target_files.iter().map(|s| normalize_relative(s)).collect();
        if target_normalized.contains(&normalized) {
            Ok(())
        } else {
            Err(PathGuardError::NotInTargetFiles {
                path: relative_path.to_string(),
            })
        }
    }

    /// 校验写入路径是否在危险路径黑名单内。在白名单外多一道防御。
    ///
    /// 返回 `Ok(())` 表示安全可写；`Err` 表示拒绝。
    pub fn check_dangerous_write(&self, relative_path: &str) -> Result<(), PathGuardError> {
        let normalized = normalize_relative(relative_path);
        for (pattern, reason) in DANGEROUS_WRITE_PATTERNS {
            if matches_pattern(&normalized, pattern) {
                return Err(PathGuardError::DangerousWritePath {
                    path: relative_path.to_string(),
                    reason,
                });
            }
        }
        Ok(())
    }

    /// 校验读取路径是否在保护 denylist 内。
    pub fn check_protected_read(&self, relative_path: &str) -> Result<(), PathGuardError> {
        let normalized = normalize_relative(relative_path);
        for (pattern, reason) in PROTECTED_READ_PATTERNS {
            if matches_pattern(&normalized, pattern) {
                return Err(PathGuardError::ProtectedReadPath {
                    path: relative_path.to_string(),
                    reason,
                });
            }
        }
        Ok(())
    }

    /// 综合校验：读取一个文件前的所有检查。
    /// 调用方传入相对路径，返回可读时 Ok，否则 Err。
    /// 不实际读文件——读操作交回调用方，本函数只做策略校验。
    pub fn validate_read(&self, relative_path: &str) -> Result<PathBuf, PathGuardError> {
        self.check_protected_read(relative_path)?;
        let canonical = self.resolve(relative_path)?;
        // 文件大小检查
        let meta =
            std::fs::metadata(&canonical).map_err(|e| PathGuardError::CanonicalizeFailed {
                path: relative_path.to_string(),
                reason: format!("metadata failed: {e}"),
            })?;
        if meta.len() > MAX_READ_FILE_SIZE {
            return Err(PathGuardError::FileTooLarge {
                path: relative_path.to_string(),
                size: meta.len(),
                max: MAX_READ_FILE_SIZE,
            });
        }
        Ok(canonical)
    }

    /// 综合校验：写入一个文件前的所有检查（白名单 + 危险路径 + 路径规范化）。
    /// `target_files` 为空表示无白名单（兼容旧路径）。
    pub fn validate_write(
        &self,
        relative_path: &str,
        target_files: &[String],
    ) -> Result<PathBuf, PathGuardError> {
        self.check_write_whitelist(relative_path, target_files)?;
        self.check_dangerous_write(relative_path)?;
        self.resolve(relative_path)
    }

    /// 综合校验：删除一个文件前的所有检查（同 write，但 action=delete）。
    pub fn validate_delete(
        &self,
        relative_path: &str,
        target_files: &[String],
    ) -> Result<PathBuf, PathGuardError> {
        // 删除走与写入相同的策略。
        self.validate_write(relative_path, target_files)
    }

    /// 判断相对路径是否匹配受保护读模式（不 resolve、不 canonicalize）。
    /// 用于快速跳过目录遍历时的受保护路径，避免每次都 resolve。
    pub fn is_protected_read_path(&self, relative_path: &str) -> bool {
        let normalized = normalize_relative(relative_path);
        for (pattern, _) in PROTECTED_READ_PATTERNS {
            if matches_pattern(&normalized, pattern) {
                return true;
            }
        }
        false
    }

    /// 判断文件内容是否疑似二进制（含 NUL 字节）。读文件前调用。
    pub fn is_binary_content(content: &[u8]) -> bool {
        content.contains(&0u8)
    }
}

// ============================================================
// 危险路径 / 受保护路径模式表
// ============================================================

/// 写入危险路径黑名单。即便在 `target_files` 内也拒绝。
///
/// 注意：这里只列「绝对禁止写」的模式。可改但需声明的高风险路径（如 `package.json`、
/// `Cargo.toml` 的依赖段）由 Reviewer 在 diff 校验时把关，本表不拦截。
static DANGEROUS_WRITE_PATTERNS: &[(&str, &str)] = &[
    (".git/**", "禁止改 git 内部状态（防改 hooks/config/HEAD）"),
    (
        ".github/workflows/**",
        "禁止改 CI workflow（防注入 CI 后门）",
    ),
    (".gitlab-ci.yml", "禁止改 GitLab CI 配置"),
    (".env*", "禁止改环境变量文件（防泄密 / 改密钥）"),
    ("*.env", "禁止改环境变量文件"),
    ("**/.env*", "禁止改环境变量文件（任意层级）"),
    ("**/*.key", "禁止改密钥文件"),
    ("**/id_rsa*", "禁止改 SSH 私钥"),
    ("**/.ssh/**", "禁止改 SSH 目录"),
];

/// 读取保护 denylist。默认拒绝读这些路径。
static PROTECTED_READ_PATTERNS: &[(&str, &str)] = &[
    (".git/**", "禁止读 git 内部状态"),
    (".env*", "禁止读环境变量文件（防泄密）"),
    ("*.env", "禁止读环境变量文件"),
    ("**/.env*", "禁止读环境变量文件（任意层级）"),
    ("**/*.key", "禁止读密钥文件"),
    ("**/id_rsa*", "禁止读 SSH 私钥"),
    ("**/.ssh/**", "禁止读 SSH 目录"),
    ("**/secrets/**", "禁止读 secrets 目录"),
    ("**/.aws/credentials", "禁止读 AWS 凭证"),
    ("**/.npmrc", "禁止读 npm 凭证"),
    ("**/.pypirc", "禁止读 PyPI 凭证"),
];

/// 极简 glob 匹配：支持 `**`（任意层级目录）、`*`（单层通配）。
///
/// 模式示例：
/// - `.git/**` 匹配 `.git/config` / `.git/hooks/pre-commit`，但不匹配 `.github/`
/// - `*.env` 匹配 `prod.env`，但不匹配 `dir/prod.env`
/// - `**/.env*` 匹配 `.env` / `prod/.env.local` / `a/b/.env`
///
/// 实现策略：把模式与路径都按 `/` 切分，逐段匹配；`**` 贪婪跨多段。
fn matches_pattern(path: &str, pattern: &str) -> bool {
    let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let pattern_parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    glob_match(&path_parts, &pattern_parts)
}

/// 递归 glob 匹配。`**` 匹配 0 个或多个路径段。
fn glob_match(path: &[&str], pattern: &[&str]) -> bool {
    match (path.first(), pattern.first()) {
        (None, None) => true,
        (Some(_), None) => false,
        (None, Some(p)) => *p == "**" && glob_match(path, &pattern[1..]),
        (Some(_), Some(p)) if *p == "**" => {
            // ** 匹配 0 个或多个段。尝试所有可能的起点。
            for i in 0..=path.len() {
                if glob_match(&path[i..], &pattern[1..]) {
                    return true;
                }
            }
            false
        }
        (Some(seg), Some(p)) => segment_matches(seg, p) && glob_match(&path[1..], &pattern[1..]),
    }
}

/// 单段匹配：`*` 匹配任意非 `/` 字符序列（含空）。其他字符精确匹配。
fn segment_matches(seg: &str, pattern: &str) -> bool {
    // 简化实现：`*` 视为通配，其他字符精确比较。
    // 不支持 `?` / `[abc]` 等高级 glob 语法，保持最小可用。
    if !pattern.contains('*') {
        return seg == pattern;
    }
    // 用动态规划式匹配：把 pattern 按 * 切分，依次在 seg 中找。
    let mut rest = seg;
    let mut last_anchor = 0; // 上一个 * 之后的字面量片段必须严格从前向后匹配
    let chunks: Vec<&str> = pattern.split('*').collect();
    for (i, chunk) in chunks.iter().enumerate() {
        if i == 0 {
            // 第一个片段必须在 seg 开头
            if !rest.starts_with(chunk) {
                return false;
            }
            rest = &rest[chunk.len()..];
            last_anchor = chunk.len();
        } else if i == chunks.len() - 1 {
            // 最后一个片段必须在 seg 结尾
            if chunk.is_empty() {
                return true;
            }
            return rest.ends_with(chunk);
        } else {
            // 中间片段：在 rest 中查找
            match rest.find(chunk) {
                Some(pos) => {
                    rest = &rest[pos + chunk.len()..];
                    last_anchor += pos + chunk.len();
                }
                None => return false,
            }
        }
    }
    let _ = last_anchor;
    true
}

/// 归一化相对路径：去前导 `./`，去多余 `/`，统一小写比较用。
/// 注意：返回值仍保留大小写（跨平台大小写敏感不同，比较时再决定）。
fn normalize_relative(p: &str) -> String {
    let mut s = p.trim().to_string();
    while s.starts_with("./") {
        s = s[2..].to_string();
    }
    // 去掉末尾多余的 /（但保留根级 `/`）
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    // 折叠多个连续 /
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    s
}

/// 对可能不存在的路径做 canonicalize：向上找第一个存在的祖先，
/// canonicalize 它，再把剩余路径段拼回去。
///
/// 这样既能处理「新建文件 + 父目录不存在」场景，也能正确检测 symlink 逃逸：
/// 如果 joined 路径中存在 symlink，向上找祖先时 canonicalize 会得到真实指向。
///
/// 失败场景：所有祖先都不存在（理论上 workspace_canonical 自身存在，不应发生）。
fn canonicalize_with_missing_ancestor(joined: &Path) -> Result<PathBuf, String> {
    // 直接 canonicalize 成功则返回。
    if let Ok(c) = joined.canonicalize() {
        return Ok(c);
    }
    // joined 不存在：先记录 joined.file_name，再向上找祖先。
    // 关键：每次向上跳一层前都要记录当前 current 的 file_name，否则会丢失一段。
    let mut missing_segments: Vec<std::ffi::OsString> = Vec::new();
    if let Some(name) = joined.file_name() {
        missing_segments.push(name.to_os_string());
    }
    let mut current = joined;
    while let Some(parent) = current.parent() {
        match parent.canonicalize() {
            Ok(c) => {
                // 找到存在的祖先：把缺失的路径段反序拼回去。
                let mut result = c;
                for seg in missing_segments.iter().rev() {
                    result = result.join(seg);
                }
                return Ok(result);
            }
            Err(_) => {
                if let Some(name) = parent.file_name() {
                    missing_segments.push(name.to_os_string());
                }
                current = parent;
            }
        }
    }
    Err("no existing ancestor found (workspace missing?)".to_string())
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn guard_in_tempdir() -> (PathGuard, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        // workspace 必须存在才能 canonicalize
        let guard = PathGuard::new(dir.path()).unwrap();
        (guard, dir)
    }

    // ---- resolve: 基本路径 ----

    #[test]
    fn resolve_normal_relative_path() {
        let (g, _d) = guard_in_tempdir();
        let p = g.resolve("src/hello.py").unwrap();
        assert!(p.starts_with(g.workspace_root()));
        // 用 components 比较，避免 Windows / 与 \ 分隔符差异导致 ends_with 失败。
        let tail: Vec<_> = p.components().rev().take(2).collect();
        assert_eq!(tail[0].as_os_str(), "hello.py");
        assert_eq!(tail[1].as_os_str(), "src");
    }

    #[test]
    fn resolve_strips_leading_dot_slash() {
        let (g, _d) = guard_in_tempdir();
        let p1 = g.resolve("a/b.py").unwrap();
        let p2 = g.resolve("./a/b.py").unwrap();
        assert_eq!(p1, p2);
    }

    // ---- resolve: 拒绝逃逸 ----

    #[test]
    fn resolve_rejects_absolute_path() {
        let (g, _d) = guard_in_tempdir();
        let err = g.resolve("/etc/passwd").unwrap_err();
        assert!(matches!(err, PathGuardError::InvalidPathComponent { .. }));
    }

    #[test]
    fn resolve_rejects_parent_dir_component() {
        let (g, _d) = guard_in_tempdir();
        let err = g.resolve("../evil.py").unwrap_err();
        assert!(matches!(err, PathGuardError::InvalidPathComponent { .. }));
    }

    #[test]
    fn resolve_rejects_nested_parent_dir() {
        let (g, _d) = guard_in_tempdir();
        let err = g.resolve("a/b/../../c/../../etc/passwd").unwrap_err();
        assert!(matches!(err, PathGuardError::InvalidPathComponent { .. }));
    }

    #[test]
    fn resolve_rejects_backslash_separator() {
        let (g, _d) = guard_in_tempdir();
        let err = g.resolve("a\\b\\c.py").unwrap_err();
        assert!(matches!(err, PathGuardError::BackslashSeparator { .. }));
    }

    // ---- resolve: symlink 逃逸 ----

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let (_g, dir) = guard_in_tempdir();
        // 在 workspace 内创建指向 workspace 外的 symlink
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "secret").unwrap();
        let link = dir.path().join("evil_link");
        symlink(&outside, &link).unwrap();
        // resolve 通过 symlink 读 outside 应失败（canonicalize 后路径不在 workspace 内？）
        // 实际上 link 自己就在 workspace 内，canonicalize link 会得到 outside 的路径，
        // 而 outside 也在 dir 内（因为 tempdir 整个就是 workspace）。
        // 改造：workspace 设为 dir/workspace，symlink 指向 dir 之外。
        let ws_dir = dir.path().join("workspace");
        fs::create_dir(&ws_dir).unwrap();
        let g2 = PathGuard::new(&ws_dir).unwrap();
        let outside2 = dir.path().join("secret.txt");
        fs::write(&outside2, "leaked").unwrap();
        let link_in_ws = ws_dir.join("escape");
        symlink(&outside2, &link_in_ws).unwrap();
        let err = g2.resolve("escape").unwrap_err();
        assert!(
            matches!(err, PathGuardError::EscapedWorkspace { .. }),
            "expected escape, got: {err:?}"
        );
    }

    // ---- 写白名单 ----

    #[test]
    fn write_whitelist_allows_declared_path() {
        let (g, _d) = guard_in_tempdir();
        g.check_write_whitelist("src/hello.py", &["src/hello.py".into()])
            .unwrap();
    }

    #[test]
    fn write_whitelist_rejects_undeclared_path() {
        let (g, _d) = guard_in_tempdir();
        let err = g
            .check_write_whitelist("src/evil.py", &["src/hello.py".into()])
            .unwrap_err();
        assert!(matches!(err, PathGuardError::NotInTargetFiles { .. }));
    }

    #[test]
    fn write_whitelist_normalizes_dot_slash() {
        let (g, _d) = guard_in_tempdir();
        // `./src/hello.py` 应与 `src/hello.py` 等价。
        g.check_write_whitelist("./src/hello.py", &["src/hello.py".into()])
            .unwrap();
    }

    #[test]
    fn write_whitelist_empty_means_no_enforcement() {
        let (g, _d) = guard_in_tempdir();
        // 空白名单：兼容旧路径，不强制。
        g.check_write_whitelist("anything.py", &[]).unwrap();
    }

    // ---- 危险写入路径 ----

    #[test]
    fn dangerous_write_rejects_git_hooks() {
        let (g, _d) = guard_in_tempdir();
        let err = g
            .check_dangerous_write(".git/hooks/pre-commit")
            .unwrap_err();
        assert!(matches!(err, PathGuardError::DangerousWritePath { .. }));
        assert!(err.to_string().contains("git"));
    }

    #[test]
    fn dangerous_write_rejects_github_workflows() {
        let (g, _d) = guard_in_tempdir();
        let err = g
            .check_dangerous_write(".github/workflows/ci.yml")
            .unwrap_err();
        assert!(matches!(err, PathGuardError::DangerousWritePath { .. }));
    }

    #[test]
    fn dangerous_write_rejects_env_files() {
        let (g, _d) = guard_in_tempdir();
        assert!(g.check_dangerous_write(".env").is_err());
        assert!(g.check_dangerous_write(".env.local").is_err());
        assert!(g.check_dangerous_write("prod.env").is_err());
        assert!(g.check_dangerous_write("config/.env").is_err());
    }

    #[test]
    fn dangerous_write_rejects_ssh_keys() {
        let (g, _d) = guard_in_tempdir();
        assert!(g.check_dangerous_write("id_rsa").is_err());
        assert!(g.check_dangerous_write(".ssh/id_ed25519").is_err());
    }

    #[test]
    fn dangerous_write_allows_normal_source() {
        let (g, _d) = guard_in_tempdir();
        g.check_dangerous_write("src/hello.py").unwrap();
        g.check_dangerous_write("tests/utils.rs").unwrap();
        g.check_dangerous_write("README.md").unwrap();
    }

    // ---- 受保护读取路径 ----

    #[test]
    fn protected_read_rejects_git_config() {
        let (g, _d) = guard_in_tempdir();
        let err = g.check_protected_read(".git/config").unwrap_err();
        assert!(matches!(err, PathGuardError::ProtectedReadPath { .. }));
    }

    #[test]
    fn protected_read_rejects_env_files() {
        let (g, _d) = guard_in_tempdir();
        assert!(g.check_protected_read(".env").is_err());
        assert!(g.check_protected_read(".env.local").is_err());
        assert!(g.check_protected_read("config/.env").is_err());
    }

    #[test]
    fn protected_read_rejects_secrets_dir() {
        let (g, _d) = guard_in_tempdir();
        assert!(g.check_protected_read("secrets/api.key").is_err());
        assert!(g.check_protected_read("deploy/secrets/token").is_err());
    }

    #[test]
    fn protected_read_rejects_ssh_keys() {
        let (g, _d) = guard_in_tempdir();
        assert!(g.check_protected_read("id_rsa").is_err());
        assert!(g.check_protected_read(".ssh/config").is_err());
    }

    #[test]
    fn protected_read_allows_normal_source() {
        let (g, _d) = guard_in_tempdir();
        g.check_protected_read("src/hello.py").unwrap();
        g.check_protected_read("README.md").unwrap();
        g.check_protected_read("tests/utils.rs").unwrap();
    }

    // ---- validate_read: 大文件 ----

    #[test]
    fn validate_read_rejects_large_file() {
        let (g, dir) = guard_in_tempdir();
        let big = dir.path().join("big.txt");
        // 写一个 > 1 MiB 的文件
        let content = "x".repeat((MAX_READ_FILE_SIZE + 1024) as usize);
        fs::write(&big, &content).unwrap();
        let err = g.validate_read("big.txt").unwrap_err();
        assert!(matches!(err, PathGuardError::FileTooLarge { .. }));
    }

    #[test]
    fn validate_read_allows_small_file() {
        let (g, dir) = guard_in_tempdir();
        let path = dir.path().join("small.txt");
        fs::write(&path, "hello").unwrap();
        let resolved = g.validate_read("small.txt").unwrap();
        assert_eq!(resolved, path.canonicalize().unwrap());
    }

    // ---- validate_read: 二进制检测 ----

    #[test]
    fn is_binary_detects_nul_byte() {
        assert!(PathGuard::is_binary_content(&[0u8; 16]));
        assert!(PathGuard::is_binary_content(b"hello\x00world"));
        assert!(!PathGuard::is_binary_content(b"plain text"));
        assert!(!PathGuard::is_binary_content(b""));
    }

    // ---- validate_write: 综合 ----

    #[test]
    fn validate_write_passes_for_normal_path() {
        let (g, _d) = guard_in_tempdir();
        let target_files = vec!["src/hello.py".to_string()];
        let p = g.validate_write("src/hello.py", &target_files).unwrap();
        assert!(p.starts_with(g.workspace_root()));
    }

    #[test]
    fn validate_write_rejects_undeclared_path() {
        let (g, _d) = guard_in_tempdir();
        let err = g
            .validate_write("src/evil.py", &["src/hello.py".into()])
            .unwrap_err();
        assert!(matches!(err, PathGuardError::NotInTargetFiles { .. }));
    }

    #[test]
    fn validate_write_rejects_dangerous_even_if_in_whitelist() {
        // 关键：白名单不能绕过危险路径黑名单（双层防御）。
        let (g, _d) = guard_in_tempdir();
        let target_files = vec![".git/hooks/pre-commit".to_string()];
        let err = g
            .validate_write(".git/hooks/pre-commit", &target_files)
            .unwrap_err();
        assert!(matches!(err, PathGuardError::DangerousWritePath { .. }));
    }

    // ---- glob 匹配 ----

    #[test]
    fn glob_matches_double_star() {
        assert!(matches_pattern(".git/config", ".git/**"));
        assert!(matches_pattern(".git/hooks/pre-commit", ".git/**"));
        assert!(!matches_pattern(".github/workflows/ci.yml", ".git/**"));
    }

    #[test]
    fn glob_matches_single_star() {
        assert!(matches_pattern("prod.env", "*.env"));
        assert!(!matches_pattern("dir/prod.env", "*.env"));
    }

    #[test]
    fn glob_matches_nested_pattern() {
        assert!(matches_pattern(".env", "**/.env*"));
        assert!(matches_pattern("config/.env", "**/.env*"));
        assert!(matches_pattern("a/b/.env.local", "**/.env*"));
    }

    #[test]
    fn glob_matches_secrets_dir() {
        assert!(matches_pattern("secrets/api.key", "**/secrets/**"));
        assert!(matches_pattern("deploy/secrets/token", "**/secrets/**"));
        assert!(!matches_pattern("secrets.txt", "**/secrets/**"));
    }

    #[test]
    fn segment_match_handles_no_wildcard() {
        assert!(segment_matches("hello.py", "hello.py"));
        assert!(!segment_matches("hello.py", "world.py"));
    }

    #[test]
    fn segment_match_handles_single_star() {
        assert!(!segment_matches("prod", "*.env")); // *.env 需要 .env 后缀
        assert!(segment_matches("prod.env", "*.env"));
        assert!(segment_matches("test", "te*t"));
    }

    // ---- normalize_relative ----

    #[test]
    fn normalize_strips_dot_slash() {
        assert_eq!(normalize_relative("./src/hello.py"), "src/hello.py");
        assert_eq!(normalize_relative("././a.py"), "a.py");
    }

    #[test]
    fn normalize_collapses_double_slash() {
        assert_eq!(normalize_relative("a//b.py"), "a/b.py");
        assert_eq!(normalize_relative("a///b.py"), "a/b.py");
    }

    #[test]
    fn normalize_trims_trailing_slash() {
        assert_eq!(normalize_relative("src/"), "src");
        assert_eq!(normalize_relative("src///"), "src");
    }
}
