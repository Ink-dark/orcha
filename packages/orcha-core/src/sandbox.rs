//! Workspace 与 Sandbox 抽象。
//!
//! 对应 README §4.2 资源隔离：每个 Task 拥有独立的 workspace 目录。
//!
//! M2 的沙箱策略：**文件系统级隔离**——每个 Task 在系统 tempdir 下
//! 分配一个独立目录作为 workspace，Sub-Agent 只能在此目录内读写。
//! Docker 容器级隔离（README §6.2）留给 M3 闭环阶段，避免 M2 引入
//! docker 依赖卡 CI；trait 已预留 `DockerSandbox` 扩展点。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::TempDir;

/// 一个被 `TempDir` 拥有的隔离工作区。
///
/// `Workspace` 持有底层 `TempDir`，drop 时自动清理；`root()` 返回工作区根路径，
/// `task_subdir()` 在其下创建并返回一个 task 专属子目录（对应 `/tmp/orcha/{task_id}`）。
pub struct Workspace {
    _tmp: TempDir,
    root: PathBuf,
}

impl Workspace {
    /// 在系统 tempdir 下创建一个独立工作区。
    pub fn ephemeral() -> Result<Self> {
        let tmp = tempfile::tempdir().context("failed to create ephemeral workspace")?;
        let root = tmp.path().to_path_buf();
        Ok(Self { _tmp: tmp, root })
    }

    /// 在指定 parent 下创建工作区（用于持久化场景或测试固定路径）。
    pub fn under(parent: impl AsRef<Path>) -> Result<Self> {
        let parent = parent.as_ref();
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create workspace parent: {}", parent.display()))?;
        let tmp = TempDir::new_in(parent).context("failed to create workspace under parent")?;
        let root = tmp.path().to_path_buf();
        Ok(Self { _tmp: tmp, root })
    }

    /// 工作区根路径。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 在工作区下为指定 task 创建子目录并返回其路径。
    /// 任务 id 形如 `T-{uuid}`，作为目录名安全。
    pub fn task_subdir(&self, task_id: &str) -> Result<PathBuf> {
        let dir = self.root.join(task_id);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create task subdir: {}", dir.display()))?;
        Ok(dir)
    }
}

/// Sandbox 抽象。M2 默认 [`FsSandbox`]；M3 可加 `DockerSandbox`。
pub trait Sandbox {
    /// 准备沙箱并返回其根路径。
    fn prepare(&self, task_id: &str) -> Result<PathBuf>;
}

/// 文件系统级沙箱：workspace 隔离在系统 tempdir。
pub struct FsSandbox {
    workspace: Workspace,
}

impl FsSandbox {
    pub fn new() -> Result<Self> {
        Ok(Self {
            workspace: Workspace::ephemeral()?,
        })
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }
}

impl Sandbox for FsSandbox {
    fn prepare(&self, task_id: &str) -> Result<PathBuf> {
        self.workspace.task_subdir(task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ephemeral_workspace_creates_root() {
        let ws = Workspace::ephemeral().unwrap();
        assert!(ws.root().exists(), "root should exist after ephemeral()");
        assert!(ws.root().is_dir());
    }

    #[test]
    fn task_subdir_creates_isolated_dir_per_task() {
        let ws = Workspace::ephemeral().unwrap();
        let a = ws.task_subdir("T-a").unwrap();
        let b = ws.task_subdir("T-b").unwrap();
        assert_ne!(a, b, "different tasks must get different dirs");
        assert!(a.is_dir());
        assert!(b.is_dir());
        assert!(a.starts_with(ws.root()));
        assert!(b.starts_with(ws.root()));
    }

    #[test]
    fn task_subdir_idempotent() {
        let ws = Workspace::ephemeral().unwrap();
        let first = ws.task_subdir("T-dup").unwrap();
        // 同一 task_id 二次调用应返回相同路径（已存在不报错）。
        let second = ws.task_subdir("T-dup").unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn fs_sandbox_prepare_returns_workspace_subdir() {
        let sandbox = FsSandbox::new().unwrap();
        let dir = sandbox.prepare("T-xyz").unwrap();
        assert!(dir.is_dir());
        assert!(dir.starts_with(sandbox.workspace().root()));
    }

    #[test]
    fn under_workspace_honors_parent() {
        let parent = tempfile::tempdir().unwrap();
        let ws = Workspace::under(parent.path()).unwrap();
        assert!(ws.root().starts_with(parent.path()));
    }
}
