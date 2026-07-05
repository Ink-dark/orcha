//! M2 端到端验收：Observer → Planner → Worker 单步链路，
//! 并用真实 `git apply` 验证 Worker 产出的 patch 可应用。

use std::fs;
use std::process::Command;

use orcha_core::{FsSandbox, Observer, Planner, Sandbox, StepContext, SubAgent, Worker};
use orcha_sdk::{ArtifactType, Task};

/// 真实跑一遍 Observer→Planner→Worker，验证文件产物与 patch 可被 git apply。
#[test]
fn end_to_end_pipeline_produces_file_and_git_applyable_patch() {
    let sandbox = FsSandbox::new().expect("FsSandbox::new");
    let task = Task::new(Task::generate_id(), "创建 hello.py 输出 hello".into());
    let task_dir = sandbox.prepare(&task.id).expect("prepare task dir");

    // —— Step 1: Observer ——
    let ctx_obs = StepContext::new(&task_dir, task.clone());
    let obs_out = Observer.run(&ctx_obs);
    assert!(
        obs_out.result.success,
        "observer failed: {}",
        obs_out.result.summary
    );
    assert_eq!(obs_out.artifacts.len(), 1);
    let obs_step = orcha_sdk::Step {
        id: "S-observer".into(),
        name: "observer".into(),
        agent: "observer".into(),
        status: orcha_sdk::StepStatus::Succeeded,
    };

    // —— Step 2: Planner ——
    let ctx_plan = StepContext::new(&task_dir, task.clone())
        .with_prior(obs_step.clone(), obs_out.artifacts.clone());
    let plan_out = Planner.run(&ctx_plan);
    assert!(
        plan_out.result.success,
        "planner failed: {}",
        plan_out.result.summary
    );
    let plan_step = orcha_sdk::Step {
        id: "S-planner".into(),
        name: "planner".into(),
        agent: "planner".into(),
        status: orcha_sdk::StepStatus::Succeeded,
    };

    // —— Step 3: Worker ——
    let ctx_work = StepContext::new(&task_dir, task.clone())
        .with_prior(obs_step.clone(), obs_out.artifacts.clone())
        .with_prior(plan_step.clone(), plan_out.artifacts.clone());
    let work_out = Worker.run(&ctx_work);
    assert!(
        work_out.result.success,
        "worker failed: {}",
        work_out.result.summary
    );

    // 验收 1：workspace 下真实产出 hello.py 且内容为 "hello\n"（规范化以与 patch 一致）。
    let hello = task_dir.join("hello.py");
    assert!(hello.is_file(), "hello.py should be produced in workspace");
    assert_eq!(fs::read_to_string(&hello).unwrap(), "hello\n");

    // 验收 2：产出 CodeDiff artifact，含 patch 文本。
    assert_eq!(work_out.artifacts.len(), 1);
    let art = &work_out.artifacts[0];
    assert_eq!(art.artifact_type, ArtifactType::CodeDiff);
    let patch = art
        .patch
        .as_ref()
        .expect("CodeDiff artifact must carry a patch")
        .clone();

    // 验收 3：在干净的 git 仓库里 `git apply` 该 patch 应成功产出同名文件。
    let applied = apply_patch_in_fresh_git_repo(&patch).expect("git apply should succeed");
    let produced = applied.join("hello.py");
    assert!(produced.is_file(), "git apply should produce hello.py");
    assert_eq!(fs::read_to_string(&produced).unwrap(), "hello\n");

    // 验收 4：链路里 artifacts 的 id 单调递增、来源可辨识。
    let mut all_ids: Vec<String> = Vec::new();
    all_ids.extend(obs_out.artifacts.iter().map(|a| a.artifact_id.clone()));
    all_ids.extend(plan_out.artifacts.iter().map(|a| a.artifact_id.clone()));
    all_ids.extend(work_out.artifacts.iter().map(|a| a.artifact_id.clone()));
    assert!(all_ids[0].starts_with("ART-observer-"));
    // Planner 看 prior=1，应生成 ART-planner-002（next_artifact_id 基于 prior 长度+1）。
    assert_eq!(all_ids[1], "ART-planner-002");
    // Worker 看 prior=2，应生成 ART-worker-003。
    assert_eq!(all_ids[2], "ART-worker-003");
}

/// 在一个临时 git 仓库里 apply 给定 patch，返回仓库根路径。
/// 用 `git` 命令而非第三方库，避免引入额外依赖。
fn apply_patch_in_fresh_git_repo(patch: &str) -> std::io::Result<std::path::PathBuf> {
    let repo_dir = tempfile::tempdir()?.keep();

    // git init + 设置一个最小身份（CI 环境可能没有全局 config）。
    let git = |args: &[&str]| -> std::io::Result<()> {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo_dir.to_str().expect("utf8 path"))
            .args(args)
            .output()?;
        if !status.status.success() {
            return Err(std::io::Error::other(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&status.stderr)
            )));
        }
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["config", "user.name", "orcha-test"])?;
    git(&["config", "user.email", "test@orcha.local"])?;
    // 禁用 autocrlf：Windows 默认 core.autocrlf=true 会把 LF 转为 CRLF，
    // 导致 apply 后文件内容变成 "hello\r\n"，与 patch 中 "hello\n" 不一致。
    git(&["config", "core.autocrlf", "false"])?;

    // 把 patch 写到临时文件后用 `git apply --check` 预校验，再真正 apply。
    let patch_path = repo_dir.join("worker.patch");
    fs::write(&patch_path, patch)?;

    // --check 不修改工作区，只校验 patch 是否合法。
    let check = Command::new("git")
        .arg("-C")
        .arg(repo_dir.to_str().unwrap())
        .args(["apply", "--check", "worker.patch"])
        .status()?;
    assert!(
        check.success(),
        "git apply --check failed; patch was:\n{patch}"
    );

    let apply = Command::new("git")
        .arg("-C")
        .arg(repo_dir.to_str().unwrap())
        .args(["apply", "worker.patch"])
        .status()?;
    assert!(apply.success(), "git apply failed; patch was:\n{patch}");

    // 删除 patch 文件，仓库保持"只有 hello.py"的纯净状态。
    let _ = fs::remove_file(&patch_path);
    Ok(repo_dir)
}

/// Observer 在非空 workspace 下应报告已有文件。
#[test]
fn observer_reports_existing_files_when_workspace_not_empty() {
    let sandbox = FsSandbox::new().unwrap();
    let task = Task::new(Task::generate_id(), "创建 hello.py 输出 hello".into());
    let task_dir = sandbox.prepare(&task.id).unwrap();
    // 预置一个文件。
    fs::write(task_dir.join("preexisting.txt"), "old").unwrap();

    let ctx = StepContext::new(&task_dir, task);
    let out = Observer.run(&ctx);
    assert!(out.result.success);
    assert!(
        out.result.summary.contains("preexisting.txt"),
        "observer should list preexisting.txt: {}",
        out.result.summary
    );
}

/// Planner 对空描述应失败而非 panic。
#[test]
fn planner_handles_empty_description_gracefully() {
    let ws = tempfile::tempdir().unwrap();
    let task = Task::new(Task::generate_id(), String::new());
    let ctx = StepContext::new(ws.path(), task);
    let out = Planner.run(&ctx);
    assert!(!out.result.success, "empty desc should fail planning");
}

/// Worker 写入失败（workspace 路径不可写）应返回 failure 而非 panic。
#[test]
fn worker_returns_failure_when_workspace_unwritable() {
    // 用一个已存在的**文件**路径模拟不可写：
    // Worker 会对目标文件的父目录调用 create_dir_all，
    // 而父路径是一个已存在的文件时（不是目录），create_dir_all 在所有平台都会失败
    // （Linux 报 ENOTDIR，Windows 报 "The directory name is invalid"）。
    // 之前用 `/proc/...` 模拟不可写，只在 Linux 有效（Windows 会把 /proc 当作
    // 相对路径成功创建目录），跨平台做法是利用"路径上存在文件"这一矛盾。
    let tmp = tempfile::NamedTempFile::new().expect("NamedTempFile::new");
    let bogus_file = tmp.path().to_path_buf();
    // workspace 指向该文件下的子路径，即父目录是一个文件而非目录。
    let bogus = bogus_file.join("workspace");
    let task = Task::new(Task::generate_id(), "创建 hello.py 输出 hello".into());
    let ctx = StepContext::new(&bogus, task);
    let out = Worker.run(&ctx);
    assert!(!out.result.success, "should fail on unwritable workspace");
    assert!(
        out.result.summary.contains("写文件失败") || out.result.summary.contains("创建目录失败"),
        "summary should mention write/create-dir failure, got: {}",
        out.result.summary
    );
}

/// Artifact id 在跨 agent 链路中应保持唯一且可追溯。
#[test]
fn artifacts_accumulate_through_chain_with_unique_ids() {
    let sandbox = FsSandbox::new().unwrap();
    let task = Task::new(Task::generate_id(), "创建 a.txt 输出 hi".into());
    let dir = sandbox.prepare(&task.id).unwrap();

    let ctx_o = StepContext::new(&dir, task.clone());
    let o = Observer.run(&ctx_o);
    let o_step = orcha_sdk::Step {
        id: "S-1".into(),
        name: "observer".into(),
        agent: "observer".into(),
        status: orcha_sdk::StepStatus::Succeeded,
    };
    let ctx_p =
        StepContext::new(&dir, task.clone()).with_prior(o_step.clone(), o.artifacts.clone());
    let p = Planner.run(&ctx_p);

    // 在 Planner 上下文里，prior_artifacts 应包含 Observer 的产物。
    assert_eq!(ctx_p.prior_artifacts.len(), 1);
    assert_eq!(
        ctx_p.prior_artifacts[0].artifact_id,
        o.artifacts[0].artifact_id
    );

    // Planner 产出的新 artifact id 不应与 Observer 的重复。
    assert_ne!(
        p.artifacts[0].artifact_id, o.artifacts[0].artifact_id,
        "artifact ids must be unique across agents"
    );
}

/// 确保 patch 不包含多余的 "../" 路径穿越。
#[test]
fn worker_patch_does_not_escape_workspace() {
    let sandbox = FsSandbox::new().unwrap();
    let task = Task::new(Task::generate_id(), "创建 hello.py 输出 hello".into());
    let dir = sandbox.prepare(&task.id).unwrap();
    let ctx = StepContext::new(&dir, task);
    let out = Worker.run(&ctx);
    assert!(out.result.success);
    let patch = out.artifacts[0].patch.as_ref().unwrap();
    // 验证 patch 中 a/hello.py 与 b/hello.py 都是相对路径，不出现 ..。
    assert!(
        !patch.contains("../"),
        "patch must not escape workspace: {patch}"
    );
    assert!(patch.contains("a/hello.py"));
    assert!(patch.contains("b/hello.py"));
}
