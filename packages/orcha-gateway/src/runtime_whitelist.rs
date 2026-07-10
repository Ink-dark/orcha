//! 运行时审批白名单（M7 P2）。
//!
//! 由"批准并加入白名单"按钮自动写入。下次同类操作直接放行，
//! 不再发起审批请求。
//!
//! # 持久化
//!
//! 存储在 `{home}/runtime_whitelist.toml`，格式：
//! ```toml
//! write = ["mod.rs", "lib.rs"]
//! command = ["cargo:check --workspace", "cargo:test"]
//! delete = ["*.log"]
//! ```
//!
//! # 匹配规则
//!
//! - WriteFile / DeleteFile：按**完整规范化相对路径**精确匹配（#20），
//!   不再仅取 basename——否则批准 `src/main.rs` 后 LLM 可写任意目录下的
//!   `main.rs`（如 `tests/main.rs`、`.github/main.rs`），构成越权。
//!   路径会规范化（消去 `.`/`..`、统一分隔符），`src/./main.rs` 与
//!   `src/main.rs` 视作同一文件。
//! - RunCommand：按 `program:arg1 arg2 ...` 精确匹配。

use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// 共享运行时白名单（Arc<Mutex> 包装，跨线程共享）。
pub type SharedRuntimeWhitelist = Arc<Mutex<RuntimeWhitelist>>;

/// 运行时审批白名单。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeWhitelist {
    /// 自动放行的写文件操作（存 basename）。
    #[serde(default)]
    pub write: Vec<String>,
    /// 自动放行的命令（存 `program:args` 指纹）。
    #[serde(default)]
    pub command: Vec<String>,
    /// 自动放行的删文件操作（存 basename）。
    #[serde(default)]
    pub delete: Vec<String>,
}

impl RuntimeWhitelist {
    /// 从文件加载；文件不存在或解析失败返回默认空白名单。
    pub fn load(path: &Path) -> Self {
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
                eprintln!("[gateway] runtime_whitelist.toml 解析失败，使用空白名单: {e}");
                Self::default()
            }),
            Err(e) => {
                eprintln!("[gateway] runtime_whitelist.toml 读取失败，使用空白名单: {e}");
                Self::default()
            }
        }
    }

    /// 保存到文件。
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let content = toml::to_string_pretty(self).unwrap_or_default();
        std::fs::write(path, content)
    }

    /// 检查操作是否在运行时白名单中（命中 = 自动放行）。
    pub fn matches(&self, action: &orcha_core::ApprovalAction) -> bool {
        match action {
            orcha_core::ApprovalAction::WriteFile { path, .. } => {
                let fp = fingerprint_path(path);
                self.write.iter().any(|w| w == &fp)
            }
            orcha_core::ApprovalAction::RunCommand { program, args } => {
                let fp = fingerprint_command(program, args);
                self.command.iter().any(|c| c == &fp)
            }
            orcha_core::ApprovalAction::DeleteFile { path } => {
                let fp = fingerprint_path(path);
                self.delete.iter().any(|d| d == &fp)
            }
        }
    }

    /// 添加操作到运行时白名单（去重）。
    /// 返回 `true` 表示新增，`false` 表示已存在。
    pub fn add(&mut self, action: &orcha_core::ApprovalAction) -> bool {
        match action {
            orcha_core::ApprovalAction::WriteFile { path, .. } => {
                let fp = fingerprint_path(path);
                if !self.write.contains(&fp) {
                    self.write.push(fp);
                    true
                } else {
                    false
                }
            }
            orcha_core::ApprovalAction::RunCommand { program, args } => {
                let fp = fingerprint_command(program, args);
                if !self.command.contains(&fp) {
                    self.command.push(fp);
                    true
                } else {
                    false
                }
            }
            orcha_core::ApprovalAction::DeleteFile { path } => {
                let fp = fingerprint_path(path);
                if !self.delete.contains(&fp) {
                    self.delete.push(fp);
                    true
                } else {
                    false
                }
            }
        }
    }
}

/// 计算路径指纹：返回**完整规范化相对路径**（#20）。
///
/// 仅取 basename 会导致同名文件跨目录越权（批准 `src/main.rs` 后可写
/// `tests/main.rs`）。此处规范化整个相对路径，使不同目录的同名文件指纹不同。
fn fingerprint_path(path: &str) -> String {
    normalize_relative(path)
}

/// 把相对 workspace 的路径规范化：统一分隔符为 '/'，消去 '.'，解析 '..'
/// （不允许越过根目录，越界的 '..' 被丢弃）。
fn normalize_relative(path: &str) -> String {
    let mut stack: Vec<&str> = Vec::new();
    for comp in path.split(['/', '\\']) {
        match comp {
            "" | "." => continue,
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// 计算命令指纹：`program:arg1 arg2 ...`
fn fingerprint_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program}:{}", args.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_path_normalizes_full_relative_path() {
        // 完整相对路径，不再是 basename（#20）
        assert_eq!(fingerprint_path("src/utils/mod.rs"), "src/utils/mod.rs");
        assert_eq!(fingerprint_path("a.txt"), "a.txt");
        // 规范化：消去 '.' 与 '..'，统一分隔符
        assert_eq!(fingerprint_path("src/./main.rs"), "src/main.rs");
        assert_eq!(fingerprint_path("src/../tests/main.rs"), "tests/main.rs");
        assert_eq!(fingerprint_path("src\\lib.rs"), "src/lib.rs");
    }

    #[test]
    fn fingerprint_command_joins_args() {
        assert_eq!(fingerprint_command("cargo", &[]), "cargo");
        assert_eq!(
            fingerprint_command("cargo", &["check".into(), "--workspace".into()]),
            "cargo:check --workspace"
        );
    }

    #[test]
    fn matches_write_file_by_full_relative_path() {
        let mut wl = RuntimeWhitelist::default();
        wl.write.push("src/main.rs".into());

        // 完整相对路径匹配
        let action = orcha_core::ApprovalAction::WriteFile {
            path: "src/main.rs".into(),
            content_preview: "".into(),
        };
        assert!(wl.matches(&action));

        // 规范化等价路径也命中
        let action_norm = orcha_core::ApprovalAction::WriteFile {
            path: "src/./main.rs".into(),
            content_preview: "".into(),
        };
        assert!(wl.matches(&action_norm));

        // 不同目录的同名文件不得命中（#20 的核心：防越权）
        let action_evil = orcha_core::ApprovalAction::WriteFile {
            path: "tests/main.rs".into(),
            content_preview: "".into(),
        };
        assert!(
            !wl.matches(&action_evil),
            "tests/main.rs 不得命中 src/main.rs"
        );

        // 不同文件名不命中
        let action2 = orcha_core::ApprovalAction::WriteFile {
            path: "src/lib.rs".into(),
            content_preview: "".into(),
        };
        assert!(!wl.matches(&action2));
    }

    #[test]
    fn whitelist_does_not_over_match_same_basename_different_dir() {
        // #20 回归测试：批准 src/main.rs 后，任意其它目录的 main.rs 都不得自动放行
        let mut wl = RuntimeWhitelist::default();
        wl.write.push("src/main.rs".into());
        for evil in ["tests/main.rs", "build/main.rs", ".github/main.rs"] {
            let action = orcha_core::ApprovalAction::WriteFile {
                path: evil.into(),
                content_preview: "".into(),
            };
            assert!(!wl.matches(&action), "{evil} 不应命中 src/main.rs 的白名单");
        }
    }

    #[test]
    fn matches_run_command_by_fingerprint() {
        let mut wl = RuntimeWhitelist::default();
        wl.command.push("cargo:check --workspace".into());

        let action = orcha_core::ApprovalAction::RunCommand {
            program: "cargo".into(),
            args: vec!["check".into(), "--workspace".into()],
        };
        assert!(wl.matches(&action));

        let action2 = orcha_core::ApprovalAction::RunCommand {
            program: "cargo".into(),
            args: vec!["test".into()],
        };
        assert!(!wl.matches(&action2));
    }

    #[test]
    fn matches_delete_file_by_full_relative_path() {
        let mut wl = RuntimeWhitelist::default();
        wl.delete.push("logs/debug.log".into());

        // 完整相对路径匹配
        let action = orcha_core::ApprovalAction::DeleteFile {
            path: "logs/debug.log".into(),
        };
        assert!(wl.matches(&action));

        // 不同目录的同名文件不得命中
        let action_evil = orcha_core::ApprovalAction::DeleteFile {
            path: "build/debug.log".into(),
        };
        assert!(
            !wl.matches(&action_evil),
            "build/debug.log 不得命中 logs/debug.log"
        );
    }

    #[test]
    fn add_deduplicates() {
        let mut wl = RuntimeWhitelist::default();
        let action = orcha_core::ApprovalAction::RunCommand {
            program: "cargo".into(),
            args: vec!["test".into()],
        };

        assert!(wl.add(&action), "首次添加应返回 true");
        assert!(!wl.add(&action), "重复添加应返回 false");
        assert_eq!(wl.command.len(), 1);
    }

    #[test]
    fn load_returns_default_when_file_missing() {
        let wl = RuntimeWhitelist::load(Path::new("/nonexistent/path/to/file.toml"));
        assert_eq!(wl, RuntimeWhitelist::default());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime_whitelist.toml");

        let mut wl = RuntimeWhitelist::default();
        wl.write.push("mod.rs".into());
        wl.command.push("cargo:check".into());
        wl.delete.push("temp.log".into());

        wl.save(&path).unwrap();
        assert!(path.exists());

        let loaded = RuntimeWhitelist::load(&path);
        assert_eq!(loaded, wl);
    }

    #[test]
    fn save_creates_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime_whitelist.toml");

        let mut wl = RuntimeWhitelist::default();
        wl.write.push("lib.rs".into());
        wl.command.push("cargo:test".into());

        wl.save(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("lib.rs"), "TOML 应包含 write 条目");
        assert!(content.contains("cargo:test"), "TOML 应包含 command 条目");
    }
}
