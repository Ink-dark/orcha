//! M3 golden tasks 端到端验收：覆盖 Cycleround 闭环的 3 条典型路径。
//!
//! 每个测试对应 ROADMAP M3 验收项之一：
//! - **GT-1**：成功路径（workspace 已有 test.py，单轮即闭环成功）。
//! - **GT-2**：Fixer 修复路径（workspace 空，第 1 轮 Tester 失败 →
//!   Fixer 创建 test.py → 第 2 轮 Tester 通过 → 成功）。
//! - **GT-3**：熔断路径（workspace 已有失败 test.py，Fixer 拒绝改写 →
//!   触达 `max_retries` → `Failed(MaxRetriesExceeded)`，**不死循环**）。
//!
//! 这三条路径共同验证 ROADMAP M3 验收项：
//! - `给定含已知 bug 的样例 repo，orcha fix 闭环修复且测试通过`
//! - `触达 max_rounds 时任务转 FAILED，不死循环`
//! - `每一轮 round 在 history 中可追溯（含耗时、token、产物引用）`

use std::fs;
use std::time::Duration;

use orcha_core::{
    CycleConfig, CycleOutcome, Cycleround, FailureReason, FileHistoryStore, HistoryStore,
};
use orcha_sdk::Task;

// ============================================================
// GT-1: 成功路径
// ============================================================

/// workspace 已预置 test.py 断言 `hello.py` 内容为 `'hello'`，
/// Cycleround 单轮即闭环成功，且 history 落盘可追溯。
#[test]
fn golden_task_1_create_with_preexisting_test_succeeds_in_round_1() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // 预置 test.py：断言 hello.py 内容（去 trailing newline 后）为 'hello'。
    fs::write(
        ws.path().join("test.py"),
        "assert open('hello.py').read().strip() == 'hello'\n",
    )
    .unwrap();

    let task = Task::new("T-gt1".into(), "创建 hello.py 输出 hello".into());
    let history_store = FileHistoryStore::new(home.path());
    history_store.init().unwrap();

    let cycle = Cycleround::new(CycleConfig {
        max_rounds: 5,
        max_retries: 3,
        cool_down: Duration::from_secs(0),
    });
    let outcome = cycle.run_with_history(&task, ws.path(), &history_store);

    let (rounds, artifacts, rounds_history) = match outcome {
        CycleOutcome::Success {
            rounds,
            artifacts,
            history,
        } => (rounds, artifacts, history),
        other => panic!("expected Success, got {other:?}"),
    };

    // —— 验收 1：单轮即成功 ——
    assert_eq!(rounds, 1, "应在第 1 轮就成功");
    assert!(!artifacts.is_empty(), "应产出 artifacts");

    // —— 验收 2：workspace 真实产出 hello.py ——
    let hello = ws.path().join("hello.py");
    assert!(hello.is_file(), "hello.py should be produced");
    assert_eq!(fs::read_to_string(&hello).unwrap(), "hello\n");

    // —— 验收 3：每轮 5 步全成功（observer/planner/worker/tester/reviewer） ——
    assert_eq!(
        rounds_history.len(),
        1,
        "in-memory history 应有 1 条 round 记录"
    );
    let r1 = &rounds_history[0];
    assert_eq!(r1.round, 1);
    assert_eq!(r1.steps.len(), 5, "成功轮应记录 5 个步骤");
    let step_ids: Vec<&str> = r1.steps.iter().map(|s| s.step_id.as_str()).collect();
    assert_eq!(
        step_ids,
        vec![
            "S-observer",
            "S-planner",
            "S-worker",
            "S-tester",
            "S-reviewer"
        ],
        "步骤顺序与名称应匹配"
    );
    for step in &r1.steps {
        assert!(step.success, "成功轮所有步骤应 success=true: {step:?}");
    }

    // —— 验收 4：history 文件持久化（含耗时 / token / 产物引用） ——
    let persisted = history_store.list_history("T-gt1").unwrap();
    assert_eq!(persisted.len(), 1, "落盘 history 应有 1 条记录");
    let pr1 = &persisted[0];
    assert_eq!(pr1.round, 1);
    assert_eq!(pr1.steps.len(), 5, "落盘记录应保留全部 steps");
    // 耗时字段：started_at < finished_at。
    assert!(
        pr1.finished_at >= pr1.started_at,
        "finished_at 应 >= started_at"
    );
    // token 字段存在（M3 确定性实现为 0，但字段必须可读）。
    assert_eq!(pr1.tokens_used, 0, "M3 确定性实现无 LLM，token 应为 0");
    // 产物引用：至少有 observer / planner / worker / tester / reviewer 的 Report artifact。
    assert!(!pr1.artifacts.is_empty(), "落盘记录应保留 artifacts 引用");
}

// ============================================================
// GT-2: Fixer 修复路径
// ============================================================

/// workspace 完全空白：第 1 轮 Worker 写出 `greet.txt`，但 Tester 因无测试框架失败 →
/// Fixer 创建 `test.py`（断言 greet.txt 内容为 'hi'）→ 第 2 轮 Tester 通过 → 成功。
///
/// 这条路径验证 Cycleround 的多轮迭代能力 + Fixer 的修复效果。
#[test]
fn golden_task_2_fixer_creates_test_py_succeeds_in_round_2() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // workspace 空：无 test.py / Cargo.toml / pytest。
    let task = Task::new("T-gt2".into(), "创建 greet.txt 输出 hi".into());
    let history_store = FileHistoryStore::new(home.path());
    history_store.init().unwrap();

    let cycle = Cycleround::new(CycleConfig {
        max_rounds: 5,
        max_retries: 3,
        cool_down: Duration::from_secs(0),
    });
    let outcome = cycle.run_with_history(&task, ws.path(), &history_store);

    let (rounds, rounds_history) = match outcome {
        CycleOutcome::Success {
            rounds, history, ..
        } => (rounds, history),
        other => panic!("expected Success, got {other:?}"),
    };

    // —— 验收 1：第 2 轮才成功（第 1 轮 Fixer 创建 test.py） ——
    assert_eq!(rounds, 2, "Fixer 在第 1 轮创建 test.py 后第 2 轮才成功");
    assert_eq!(rounds_history.len(), 2, "应有 2 条 round 记录");

    // —— 验收 2：第 1 轮应包含 Fixer step ——
    let r1 = &rounds_history[0];
    let r1_step_ids: Vec<&str> = r1.steps.iter().map(|s| s.step_id.as_str()).collect();
    assert!(
        r1_step_ids.contains(&"S-fixer"),
        "第 1 轮应调用 Fixer，实际 steps: {r1_step_ids:?}"
    );

    // 第 1 轮 Tester 应失败（无测试框架），Fixer 应成功。
    let tester_step = r1
        .steps
        .iter()
        .find(|s| s.step_id == "S-tester")
        .expect("应有 tester step");
    assert!(!tester_step.success, "第 1 轮 Tester 应失败（无测试框架）");
    let fixer_step = r1
        .steps
        .iter()
        .find(|s| s.step_id == "S-fixer")
        .expect("应有 fixer step");
    assert!(fixer_step.success, "第 1 轮 Fixer 应成功创建 test.py");

    // —— 验收 3：第 2 轮 5 步全成功 ——
    let r2 = &rounds_history[1];
    assert_eq!(r2.round, 2);
    assert_eq!(r2.steps.len(), 5, "第 2 轮应记录 5 个成功步骤");
    for step in &r2.steps {
        assert!(step.success, "第 2 轮所有步骤应 success=true: {step:?}");
    }

    // —— 验收 4：workspace 真实产出 greet.txt + Fixer 创建的 test.py ——
    let greet = ws.path().join("greet.txt");
    assert!(greet.is_file(), "greet.txt 应被产出");
    assert_eq!(fs::read_to_string(&greet).unwrap(), "hi\n");
    let test_py = ws.path().join("test.py");
    assert!(test_py.is_file(), "Fixer 创建的 test.py 应存在");
    let test_content = fs::read_to_string(&test_py).unwrap();
    assert!(
        test_content.contains("greet.txt") && test_content.contains("hi"),
        "Fixer 创建的 test.py 应断言 greet.txt 内容为 hi: {test_content}"
    );

    // —— 验收 5：history 落盘可追溯两轮 ——
    let persisted = history_store.list_history("T-gt2").unwrap();
    assert_eq!(persisted.len(), 2, "落盘 history 应有 2 条记录");
    assert_eq!(persisted[0].round, 1);
    assert_eq!(persisted[1].round, 2);
}

// ============================================================
// GT-3: 熔断路径（不死循环）
// ============================================================

/// workspace 预置失败的 test.py（`assert False`）：
/// Fixer 检测到已有测试框架后拒绝改写（避免「改测试让测试通过」作弊）→
/// Cycleround 触达 `max_retries` → `Failed(MaxRetriesExceeded)`，**不死循环**。
///
/// 这条路径验证 ROADMAP M3 验收项「触达 max_rounds 时任务转 FAILED，不死循环」。
#[test]
fn golden_task_3_max_retries_exceeded_when_test_py_fails() {
    let ws = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();

    // 预置失败的 test.py（断言 1 == 2）。
    fs::write(
        ws.path().join("test.py"),
        "assert 1 == 2, 'intentional bug'\n",
    )
    .unwrap();

    let task = Task::new("T-gt3".into(), "创建 hello.py 输出 hello".into());
    let history_store = FileHistoryStore::new(home.path());
    history_store.init().unwrap();

    // 用较小的 max_retries=2 保证测试不会跑太久。
    let cycle = Cycleround::new(CycleConfig {
        max_rounds: 10,
        max_retries: 2,
        cool_down: Duration::from_secs(0),
    });
    let outcome = cycle.run_with_history(&task, ws.path(), &history_store);

    let (rounds, reason, rounds_history) = match outcome {
        CycleOutcome::Failed {
            rounds,
            reason,
            history,
        } => (rounds, reason, history),
        other => panic!("expected Failed(MaxRetriesExceeded), got {other:?}"),
    };

    // —— 验收 1：失败原因 = MaxRetriesExceeded ——
    assert_eq!(
        reason,
        FailureReason::MaxRetriesExceeded,
        "触达 max_retries=2 后应返回 MaxRetriesExceeded"
    );

    // —— 验收 2：不死循环，执行轮次应远小于 max_rounds=10 ——
    assert!(
        rounds <= 2,
        "应在触达 max_retries=2 后立即停止，实际执行了 {rounds} 轮（不应死循环）"
    );
    assert_eq!(
        rounds_history.len(),
        rounds as usize,
        "history 记录数应等于实际执行轮次"
    );

    // —— 验收 3：Fixer 不应改写已存在的 test.py ——
    let test_content = fs::read_to_string(ws.path().join("test.py")).unwrap();
    assert_eq!(
        test_content, "assert 1 == 2, 'intentional bug'\n",
        "Fixer 不应改写已存在的 test.py（避免作弊）"
    );

    // —— 验收 4：hello.py 仍应被 Worker 创建（Worker 在 Tester 失败前已执行） ——
    // 注意：每轮 Worker 都会尝试写 hello.py，所以它应该存在。
    assert!(
        ws.path().join("hello.py").is_file(),
        "Worker 应在第 1 轮就创建 hello.py"
    );

    // —— 验收 5：每轮 history 应记录 Fixer 的失败 ——
    for (i, rec) in rounds_history.iter().enumerate() {
        let fixer_step = rec
            .steps
            .iter()
            .find(|s| s.step_id == "S-fixer")
            .unwrap_or_else(|| panic!("第 {} 轮应有 fixer step", i + 1));
        assert!(
            !fixer_step.success,
            "Fixer 在第 {} 轮应失败（已有 test.py 拒绝改写）",
            i + 1
        );
    }

    // —— 验收 6：history 落盘可追溯 ——
    let persisted = history_store.list_history("T-gt3").unwrap();
    assert_eq!(
        persisted.len(),
        rounds as usize,
        "落盘 history 应与 in-memory history 一致"
    );
}
