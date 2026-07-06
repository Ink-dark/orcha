//! Workspace 与 Sandbox 抽象。
//!
//! 对应 README §4.2 资源隔离：每个 Task 拥有独立的 workspace 目录。
//!
//! M2 的沙箱策略：**文件系统级隔离**——每个 Task 在系统 tempdir 下
//! 分配一个独立目录作为 workspace，Sub-Agent 只能在此目录内读写。
//! Docker 容器级隔离（README §6.2）留给 M3 闭环阶段，避免 M2 引入
//! docker 依赖卡 CI；trait 已预留 `DockerSandbox` 扩展点。
//!
//! M4 新增 [`GitWorktree`]：真实 repo 接入时通过 `git worktree add` 创建
//! 独立工作区，所有改动落 worktree。经 Review 通过后才 apply 回原 repo。
//! 若 LLM 乱写，原 repo 工作区不被污染，回滚只需 `git worktree remove`。

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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

/// Git worktree 沙箱：从真实 repo 创建隔离工作区。
///
/// 构造时执行 `git worktree add --detach <temp_dir>`，
/// 所有 Sub-Agent 的读写操作落在 worktree 内。
/// Drop 时执行 `git worktree remove --force <temp_dir>` 清理。
///
/// 原 repo 工作区完全不受影响——即便 LLM 产出乱写或危险路径写入，
/// 被污染的只是 worktree 副本。回滚：drop 本结构体即可。
pub struct GitWorktree {
    source_repo: PathBuf,
    #[allow(dead_code)]
    worktree_temp: TempDir,
    worktree_path: PathBuf,
}

impl GitWorktree {
    /// 从 `source_repo` 创建独立 worktree。
    ///
    /// `source_repo` 必须是 git 仓库根目录（含 `.git`）。
    /// 构造失败（非 git repo / git 不可用 / 磁盘满等）返回 Err。
    pub fn new(source_repo: impl AsRef<Path>) -> Result<Self> {
        let source_repo: PathBuf = source_repo
            .as_ref()
            .canonicalize()
            .context("source repo 路径不存在或无法访问")?;

        if !source_repo.join(".git").exists() {
            anyhow::bail!("{} 不是 git 仓库（缺 .git）", source_repo.display());
        }

        // 检查 git 是否可用
        check_git_available()?;

        let worktree_temp = tempfile::tempdir().context("无法创建 worktree 临时目录")?;
        let worktree_path = worktree_temp.path().to_path_buf();

        // git worktree add --detach <path> <base-commit>
        // --detach 表示不在新 worktree 创建分支，避免污染原 repo 分支列表
        let output = Command::new("git")
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "worktree",
                "add",
                "--detach",
                &worktree_path.to_string_lossy(),
                "HEAD",
            ])
            .output()
            .context("执行 git worktree add 失败")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git worktree add 失败: {stderr}");
        }

        Ok(Self {
            source_repo,
            worktree_temp,
            worktree_path,
        })
    }

    /// worktree 根路径（即 workspace 路径）。
    pub fn path(&self) -> &Path {
        &self.worktree_path
    }

    /// 源 repo 路径。
    pub fn source_repo(&self) -> &Path {
        &self.source_repo
    }
}

impl Sandbox for GitWorktree {
    fn prepare(&self, _task_id: &str) -> Result<PathBuf> {
        Ok(self.worktree_path.clone())
    }
}

impl Drop for GitWorktree {
    fn drop(&mut self) {
        // git worktree remove --force 清理 worktree 记录
        // 即使 worktree 目录已被操作，--force 仍可移除
        let output = Command::new("git")
            .args([
                "-C",
                &self.source_repo.to_string_lossy(),
                "worktree",
                "remove",
                "--force",
                &self.worktree_path.to_string_lossy(),
            ])
            .output();

        match output {
            Ok(o) if o.status.success() => {}
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                eprintln!(
                    "warn: git worktree remove 失败 ({}): {}",
                    self.worktree_path.display(),
                    stderr
                );
            }
            Err(e) => {
                eprintln!(
                    "warn: 无法执行 git worktree remove ({}): {e}",
                    self.worktree_path.display()
                );
            }
        }
    }
}

fn check_git_available() -> Result<()> {
    let output = Command::new("git")
        .arg("--version")
        .output()
        .context("git 不可用，请确认已安装 git 并在 PATH 中")?;
    if !output.status.success() {
        anyhow::bail!("git --version 返回非零退出码");
    }
    Ok(())
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
