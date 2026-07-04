//! orcha-core 状态机的合法/非法迁移测试。
//!
//! 覆盖 README §4.1 的全部边：
//! ```text
//! PENDING -> RUNNING -> (BLOCKED <-> RUNNING) -> DONE
//!                        |
//!                        └-> FAILED
//! ```

use chrono::Utc;
use orcha_core::{
    is_legal_transition, is_terminal, transition, CoreError,
};
use orcha_sdk::{Task, TaskStatus};

fn pending_task() -> Task {
    Task::new("T-test".into(), "desc".into())
}

#[test]
fn pending_to_running_is_legal() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    assert_eq!(task.status, TaskStatus::Running);
    assert!(task.updated_at >= task.created_at);
}

#[test]
fn running_to_blocked_then_back_to_running_is_legal() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Blocked).unwrap();
    assert_eq!(task.status, TaskStatus::Blocked);
    transition(&mut task, TaskStatus::Running).unwrap();
    assert_eq!(task.status, TaskStatus::Running);
}

#[test]
fn running_to_done_is_legal_and_terminal() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Done).unwrap();
    assert_eq!(task.status, TaskStatus::Done);
    assert!(is_terminal(task.status));
}

#[test]
fn running_to_failed_is_legal_and_terminal() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Failed).unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    assert!(is_terminal(task.status));
}

#[test]
fn pending_directly_to_done_is_illegal() {
    let mut task = pending_task();
    let err = transition(&mut task, TaskStatus::Done).unwrap_err();
    assert!(matches!(err, CoreError::InvalidTransition { .. }));
}

#[test]
fn pending_directly_to_failed_is_illegal() {
    let mut task = pending_task();
    let err = transition(&mut task, TaskStatus::Failed).unwrap_err();
    assert!(matches!(err, CoreError::InvalidTransition { .. }));
}

#[test]
fn blocked_directly_to_done_is_illegal_must_return_to_running() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Blocked).unwrap();
    let err = transition(&mut task, TaskStatus::Done).unwrap_err();
    assert!(matches!(err, CoreError::InvalidTransition { .. }));
    // 必须先回 RUNNING 才能 DONE。
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Done).unwrap();
}

#[test]
fn done_is_terminal_no_outgoing_transitions() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Done).unwrap();

    // 终态下，任何非同状态的目标都必须报 TerminalTask。
    // 同状态（Done->Done）是合法的幂等 no-op，不在本测试范围。
    for &to in TaskStatus::all() {
        if to == TaskStatus::Done {
            continue;
        }
        let err = transition(&mut task, to).unwrap_err();
        assert!(
            matches!(err, CoreError::TerminalTask { .. }),
            "Done -> {to:?} should be TerminalTask, got {err:?}"
        );
    }
}

#[test]
fn terminal_same_status_is_idempotent_noop() {
    let mut task = pending_task();
    transition(&mut task, TaskStatus::Running).unwrap();
    transition(&mut task, TaskStatus::Done).unwrap();
    // Done -> Done 是幂等 no-op，应成功。
    transition(&mut task, TaskStatus::Done).unwrap();
    assert_eq!(task.status, TaskStatus::Done);
}

#[test]
fn same_status_transition_is_legal_noop() {
    let mut task = pending_task();
    let before = task.updated_at;
    // 同状态 no-op，不应报错。
    transition(&mut task, TaskStatus::Pending).unwrap();
    assert_eq!(task.status, TaskStatus::Pending);
    let _ = before; // updated_at 在 no-op 下不强制变更。
}

#[test]
fn invalid_transition_error_carries_context() {
    let mut task = Task {
        id: "T-42".into(),
        description: "x".into(),
        status: TaskStatus::Pending,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    let err = transition(&mut task, TaskStatus::Done).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("T-42"), "error msg should contain task id: {msg}");
    assert!(msg.contains("PENDING"), "error msg should contain from: {msg}");
    assert!(msg.contains("DONE"), "error msg should contain to: {msg}");
}

#[test]
fn is_legal_transition_table_matches_spec() {
    use TaskStatus::*;
    // `is_legal_transition` 是纯边谓词：仅覆盖 README §4.1 的真实边。
    // 同状态 no-op 由 `transition()` 单独处理，不在此谓词范围内。
    let legal = vec![
        (Pending, Running),
        (Running, Blocked),
        (Running, Done),
        (Running, Failed),
        (Blocked, Running),
    ];
    for &from in TaskStatus::all() {
        for &to in TaskStatus::all() {
            let expected = legal.contains(&(from, to));
            assert_eq!(
                is_legal_transition(from, to),
                expected,
                "is_legal_transition({from:?}, {to:?}) mismatch"
            );
        }
    }
}
