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
//! - WriteFile / DeleteFile：按 path 的 basename（文件名）精确匹配，
//!   这样不同 worktree 路径前缀不影响匹配。
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

/// 计算路径指纹：取 basename（文件名），忽略目录前缀。
/// 这样不同 worktree 路径前缀不影响匹配。
fn fingerprint_path(path: &str) -> String {
    Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
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
    fn fingerprint_path_takes_basename() {
        assert_eq!(fingerprint_path("src/utils/mod.rs"), "mod.rs");
        assert_eq!(fingerprint_path("/tmp/worktree-x/src/lib.rs"), "lib.rs");
        assert_eq!(fingerprint_path("a.txt"), "a.txt");
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
    fn matches_write_file_by_basename() {
        let mut wl = RuntimeWhitelist::default();
        wl.write.push("mod.rs".into());

        // basename 匹配，路径前缀不同也算命中
        let action = orcha_core::ApprovalAction::WriteFile {
            path: "/tmp/worktree-abc/src/utils/mod.rs".into(),
            content_preview: "".into(),
        };
        assert!(wl.matches(&action));

        // 不同文件名不命中
        let action2 = orcha_core::ApprovalAction::WriteFile {
            path: "src/lib.rs".into(),
            content_preview: "".into(),
        };
        assert!(!wl.matches(&action2));
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
    fn matches_delete_file_by_basename() {
        let mut wl = RuntimeWhitelist::default();
        wl.delete.push("debug.log".into());

        let action = orcha_core::ApprovalAction::DeleteFile {
            path: "/some/path/debug.log".into(),
        };
        assert!(wl.matches(&action));
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
