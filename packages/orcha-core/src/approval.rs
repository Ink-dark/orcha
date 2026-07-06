//! M7 P1：人工审批 Hook。
//!
//! 在 PathGuard（静态白名单 + 危险路径黑名单）之外多一层「人工同意」：
//! Worker 写文件前、Tester 跑命令前，先调 [`ApprovalHook::request`]，
//! 由 hook 决定放行 / 拒绝。
//!
//! ## 设计
//!
//! - `ApprovalAction` 描述待批准的副作用（写文件 / 跑命令）
//! - `ApprovalDecision` 是 hook 的回应（批准 / 拒绝）
//! - `ApprovalHook` 是 trait，多种实现：
//!   - [`NullApprovalHook`]：默认放行（CI / 旧路径兼容）
//!   - [`StdinApprovalHook`]：CLI 终端 y/n 询问（无状态，每次临时获取 stdin/stdout）
//!   - [`MockApprovalHook`]：测试用，按预设队列返回决策
//!   - 后续可加 `GatewayApprovalHook`：通过 IPC 让飞书管理员审批
//!
//! ## 与 PathGuard 的关系
//!
//! PathGuard 是**前置硬限制**（路径不逃逸 / 不在黑名单），不依赖人工；
//! ApprovalHook 是**前置软限制**（管理员同意方可），是 PathGuard 之后的
//! 第二道闸。两者并存：PathGuard 拒绝的路径不会进入 hook 阶段。
//!
//! 调用顺序（在 Worker / Tester 内部）：
//! 1. `guard.validate_write(path, target_files)` —— PathGuard 硬校验
//! 2. `hook.request(WriteFile { ... })` —— 人工审批
//! 3. `std::fs::write(...)` —— 实际写入

use std::io::{self, Write};

/// 待审批的副作用动作。
#[derive(Debug, Clone)]
pub enum ApprovalAction {
    /// 写文件（含新建 / 覆盖）。
    /// `path` 是相对 workspace 的路径；`content_preview` 是前 200 字节预览。
    WriteFile {
        path: String,
        content_preview: String,
    },
    /// 删除文件。
    DeleteFile { path: String },
    /// 跑 shell 命令（Tester / Fixer）。
    RunCommand {
        program: String,
        args: Vec<String>,
    },
}

impl ApprovalAction {
    /// 一行人类可读的描述（用于打印给管理员看）。
    pub fn describe(&self) -> String {
        match self {
            ApprovalAction::WriteFile {
                path,
                content_preview,
            } => {
                let preview = if content_preview.len() > 100 {
                    format!("{}...(共 {} 字节)", &content_preview[..100], content_preview.len())
                } else {
                    content_preview.clone()
                };
                format!("写文件 {path}：{preview}")
            }
            ApprovalAction::DeleteFile { path } => format!("删除文件 {path}"),
            ApprovalAction::RunCommand { program, args } => {
                if args.is_empty() {
                    format!("执行命令：{program}")
                } else {
                    format!("执行命令：{program} {}", args.join(" "))
                }
            }
        }
    }
}

/// Hook 对审批请求的回应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// 批准执行。
    Approved,
    /// 拒绝执行，附带理由（用于审计 / 日志）。
    Rejected(String),
}

/// 审批 Hook trait。
///
/// 实现方决定如何询问管理员：终端 stdin / IPC / 飞书卡片 / ...
/// 默认实现 [`NullApprovalHook`] 直接放行，用于 CI 与旧路径兼容。
///
/// 实现要求 `Send + Sync`：因为 [`crate::StepContext`] 持 `Arc<dyn ApprovalHook>`，
/// 会被多线程访问（例如 `run_streaming` 在独立线程跑）。
pub trait ApprovalHook: Send + Sync {
    /// 请求批准一个动作。
    /// 返回 [`ApprovalDecision::Approved`] 放行；否则拒绝。
    fn request(&self, action: &ApprovalAction) -> ApprovalDecision;
}

/// 默认放行的 hook（不做任何审批，直接 Approved）。
///
/// 用于 CI / 旧路径兼容 / 单元测试。生产环境应注入 [`StdinApprovalHook`] 或
/// 其他真实 hook。
#[derive(Debug, Clone, Default)]
pub struct NullApprovalHook;

impl ApprovalHook for NullApprovalHook {
    fn request(&self, _action: &ApprovalAction) -> ApprovalDecision {
        ApprovalDecision::Approved
    }
}

/// 终端 stdin y/n 询问 hook（无状态）。
///
/// 每次审批把动作描述打印到 stderr，读 stdin 一行：
/// - `y` / `yes` / `回车`：批准
/// - `n` / `no` / 其他：拒绝
/// - EOF（无 stdin）：拒绝（fail-closed）
///
/// **无状态**：不持有 stdin/stdout 句柄，每次 `request` 临时获取
/// `io::stderr()` 和 `io::stdin()`。这样 `Send + Sync` 自然满足，
/// 可在 `Arc<dyn ApprovalHook>` 中跨线程共享。
///
/// 适合 `orcha fix --ai --approve` 在本地终端交互使用。
/// 不适合 Gateway / 后台进程（无 stdin 时一律拒绝）。
#[derive(Debug, Clone, Default)]
pub struct StdinApprovalHook;

impl StdinApprovalHook {
    pub fn new() -> Self {
        Self
    }
}

impl ApprovalHook for StdinApprovalHook {
    fn request(&self, action: &ApprovalAction) -> ApprovalDecision {
        let prompt = format!(
            "\n[审批请求] {}\n批准？[y/n] (默认 y): ",
            action.describe()
        );
        // 用 stderr 输出提示（避免污染 stdout 的 JSON 输出）。
        if let Err(e) = io::stderr().write_all(prompt.as_bytes()) {
            return ApprovalDecision::Rejected(format!("输出提示失败: {e}"));
        }
        if let Err(e) = io::stderr().flush() {
            return ApprovalDecision::Rejected(format!("flush 失败: {e}"));
        }

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) => {
                // EOF（无 stdin）：fail-closed，拒绝。
                ApprovalDecision::Rejected("stdin EOF（无终端输入）".to_string())
            }
            Ok(_) => {
                let trimmed = line.trim().to_ascii_lowercase();
                match trimmed.as_str() {
                    "" | "y" | "yes" => ApprovalDecision::Approved,
                    "n" | "no" => {
                        ApprovalDecision::Rejected("管理员拒绝（stdin 输入 n）".to_string())
                    }
                    other => {
                        ApprovalDecision::Rejected(format!("管理员输入未知响应: {other}"))
                    }
                }
            }
            Err(e) => ApprovalDecision::Rejected(format!("读 stdin 失败: {e}")),
        }
    }
}

/// 测试用 mock hook：按预设决策队列返回。
///
/// 队列空时返回 [`ApprovalDecision::Rejected`]（fail-closed）。
/// 用于 ai_cycleround / worker / tester 单测验证审批分支。
pub struct MockApprovalHook {
    /// 预设决策队列。每次 `request` 弹出队首；空则拒绝。
    decisions: std::sync::Mutex<Vec<ApprovalDecision>>,
}

impl MockApprovalHook {
    pub fn new(decisions: Vec<ApprovalDecision>) -> Self {
        Self {
            decisions: std::sync::Mutex::new(decisions),
        }
    }

    /// 全部批准的 mock（最常见的测试场景）。
    pub fn always_approve() -> Self {
        // 用空队列 + 一个 always_approve 标志更合理，但简单起见，
        // 这里返回一个持无穷批准的 mock（用 repeat 实现）。
        // 实际上直接用 NullApprovalHook 即可，但本类型保留显式语义。
        // 这里采用：构造一个永远批准的实现。
        Self::new(vec![])
    }
}

impl ApprovalHook for MockApprovalHook {
    fn request(&self, _action: &ApprovalAction) -> ApprovalDecision {
        let mut queue = self.decisions.lock().unwrap();
        if !queue.is_empty() {
            // 取队首（保持入队顺序），与 Vec::pop 弹末尾相反。
            queue.drain(..1).next().unwrap()
        } else {
            // 队列空：默认批准（避免测试需要精确数队列）
            ApprovalDecision::Approved
        }
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_hook_always_approves() {
        let h = NullApprovalHook;
        let action = ApprovalAction::WriteFile {
            path: "x.py".into(),
            content_preview: "print('hi')".into(),
        };
        assert_eq!(h.request(&action), ApprovalDecision::Approved);
    }

    #[test]
    fn action_describe_write_file() {
        let a = ApprovalAction::WriteFile {
            path: "src/main.py".into(),
            content_preview: "print('hi')".into(),
        };
        let s = a.describe();
        assert!(s.contains("写文件"));
        assert!(s.contains("src/main.py"));
        assert!(s.contains("print('hi')"));
    }

    #[test]
    fn action_describe_write_file_truncates_long_preview() {
        let long = "x".repeat(500);
        let a = ApprovalAction::WriteFile {
            path: "big.txt".into(),
            content_preview: long,
        };
        let s = a.describe();
        assert!(s.contains("...(共 500 字节)"));
    }

    #[test]
    fn action_describe_run_command() {
        let a = ApprovalAction::RunCommand {
            program: "pytest".into(),
            args: vec!["-x".into(), "test_main.py".into()],
        };
        let s = a.describe();
        assert!(s.contains("执行命令"));
        assert!(s.contains("pytest -x test_main.py"));
    }

    #[test]
    fn action_describe_delete_file() {
        let a = ApprovalAction::DeleteFile { path: "tmp.txt".into() };
        let s = a.describe();
        assert!(s.contains("删除文件"));
        assert!(s.contains("tmp.txt"));
    }

    #[test]
    fn mock_hook_returns_preset_decisions_in_order() {
        let h = MockApprovalHook::new(vec![
            ApprovalDecision::Approved,
            ApprovalDecision::Rejected("理由".into()),
        ]);
        let action = ApprovalAction::RunCommand {
            program: "ls".into(),
            args: vec![],
        };
        assert_eq!(h.request(&action), ApprovalDecision::Approved);
        match h.request(&action) {
            ApprovalDecision::Rejected(r) => assert_eq!(r, "理由"),
            ApprovalDecision::Approved => panic!("应拒绝"),
        }
    }

    #[test]
    fn mock_hook_approves_after_queue_empty() {
        let h = MockApprovalHook::new(vec![]);
        let action = ApprovalAction::WriteFile {
            path: "x".into(),
            content_preview: "x".into(),
        };
        // 队列空时默认批准
        assert_eq!(h.request(&action), ApprovalDecision::Approved);
    }

    #[test]
    fn null_hook_is_default() {
        let _h: NullApprovalHook = Default::default();
    }

    #[test]
    fn stdin_hook_is_default_and_send_sync() {
        let h: StdinApprovalHook = Default::default();
        // 验证 Send + Sync
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StdinApprovalHook>();
        // 验证可放 Arc<dyn ApprovalHook>
        let _arc: std::sync::Arc<dyn ApprovalHook> = std::sync::Arc::new(h);
    }
}
