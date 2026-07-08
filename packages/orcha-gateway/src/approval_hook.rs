//! M7 P1：Gateway 端人工审批 Hook。
//!
//! 把 [`orcha_core::ApprovalHook`] 接到飞书卡片：Worker 发起审批请求 →
//! Gateway 通过 IPC 让 Adapter 推飞书卡片 → 管理员点按钮 → Adapter 回
//! `ApprovalResponse` → Gateway 查白名单后通过 oneshot 回 Worker。
//!
//! # 架构
//!
//! ```text
//! Worker 线程 (GatewayApprovalHook.request)        Adapter
//! ---------                                       -------
//! ├─ action_id = uuid
//! ├─ tx,rx = sync_channel(1)
//! ├─ pending.lock().insert(action_id, tx)
//! ├─ registry.broadcast(ApprovalRequest) ──────────→ push 飞书卡片
//! │                                                  带 [批准][拒绝] 按钮
//! │                                                  ↓ 管理员点击
//! │ Reader 线程 ←── IPC recv ApprovalResponse ────── card.action.trigger
//! │ ├─ pending.lock().remove(action_id)
//! │ ├─ config.authenticator_for(action).check(operator)
//! │ ├─ final = merge(adapter_decision, whitelist_result)
//! │ ├─ entry.tx.send(final)  // 通过 channel 回传 worker
//! │ └─ registry.broadcast(ApprovalResult) ───────→ patch 卡片 "✅ 已批准"
//! ←─ rx.recv_timeout(timeout) ──┘
//! └─ return final_decision
//! ```
//!
//! # fail-closed
//!
//! 任何环节失败（超时 / 白名单不通过 / IPC 断开 / 非法 action_id）都返回
//! `Rejected`，让 Worker 标记 step 失败、不执行副作用。
//!
//! # 细粒度白名单
//!
//! Gateway 收到 `ApprovalResponse` 时，按 `action` 类型查
//! `[approval.write]` / `[approval.command]` / `[approval.delete]` 对应
//! 白名单。白名单为空 = 该类全拒；非空 = 走 `Authenticator::check`。

use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use orcha_core::{ApprovalAction, ApprovalDecision, ApprovalHook};

use crate::adapter_registry::AdapterRegistry;
use crate::config::ApprovalConfig;
use crate::protocol::{ApprovalActionDto, ApprovalDecisionDto, GatewayToAdapter};
use crate::runtime_whitelist::SharedRuntimeWhitelist;

/// Pending map：action_id → PendingEntry。
/// 由 Worker 线程 insert，Reader 线程 remove。两者共享 `Arc<Mutex<...>>`。
pub type PendingMap = Arc<Mutex<HashMap<String, PendingEntry>>>;

/// Reader 线程从 pending map 取出的 entry：含回传 worker 的 channel sender。
pub struct PendingEntry {
    pub action: ApprovalAction,
    pub tx: SyncSender<ApprovalDecision>,
}

/// Gateway 端审批 Hook 实现。
///
/// Worker 在 `request()` 里：
/// 1. 生成 action_id + sync_channel
/// 2. insert 到 `pending` map
/// 3. `registry.broadcast(ApprovalRequest)` 推给 Adapter
/// 4. 阻塞 `rx.recv_timeout(timeout)` 等决策
///
/// Reader 线程在收到 `ApprovalResponse` 时调 [`handle_approval_response`，
/// 它会从 map 取出 entry、过白名单、通过 channel 回传决策。
pub struct GatewayApprovalHook {
    /// 共享 pending map（与 reader 线程共享）。
    pending: PendingMap,
    /// 广播 IPC 消息给所有已连 Adapter（M7 单 Adapter）。
    registry: AdapterRegistry,
    /// 当前 task_id（worker 一次只处理一个 task）。
    task_id: String,
    /// 当前 session（飞书 chat_id）。
    session: String,
    /// 超时时长。超时自动 Rejected（fail-closed）。
    timeout: Duration,
    /// 运行时白名单（跨任务共享，命中则直接放行不发起审批）。
    runtime_whitelist: SharedRuntimeWhitelist,
}

impl GatewayApprovalHook {
    pub fn new(
        pending: PendingMap,
        registry: AdapterRegistry,
        task_id: String,
        session: String,
        timeout: Duration,
        runtime_whitelist: SharedRuntimeWhitelist,
    ) -> Self {
        Self {
            pending,
            registry,
            task_id,
            session,
            timeout,
            runtime_whitelist,
        }
    }
}

impl ApprovalHook for GatewayApprovalHook {
    fn request(&self, action: &ApprovalAction) -> ApprovalDecision {
        // 0. 先检查运行时白名单：命中则直接放行，不发起审批
        if let Ok(wl) = self.runtime_whitelist.lock() {
            if wl.matches(action) {
                eprintln!("[gateway] 审批操作命中运行时白名单，自动放行: {action:?}");
                return ApprovalDecision::Approved;
            }
        }

        let action_id = uuid_v4_simple();
        let (tx, rx) = mpsc::sync_channel::<ApprovalDecision>(1);

        // 1. 注册到 pending map
        {
            let mut map = match self.pending.lock() {
                Ok(m) => m,
                Err(e) => {
                    return ApprovalDecision::Rejected(format!("pending map 锁 poisoned: {e}"))
                }
            };
            map.insert(
                action_id.clone(),
                PendingEntry {
                    action: action.clone(),
                    tx,
                },
            );
        }

        // 2. 广播 ApprovalRequest 给 Adapter（推飞书卡片）
        self.registry.broadcast(&GatewayToAdapter::ApprovalRequest {
            action_id: action_id.clone(),
            task_id: self.task_id.clone(),
            session: self.session.clone(),
            action: ApprovalActionDto::from(action),
        });

        // 3. 阻塞等 reader 线程通过 channel 回传决策
        let decision = match rx.recv_timeout(self.timeout) {
            Ok(d) => d,
            Err(mpsc::RecvTimeoutError::Timeout) => ApprovalDecision::Rejected(format!(
                "审批超时（{} 秒无响应）",
                self.timeout.as_secs()
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => ApprovalDecision::Rejected(
                "Gateway reader 线程未回决策（channel 断开）".to_string(),
            ),
        };

        // 4. 兜底清理：若超时/断开，reader 没机会 remove，这里保证 map 不泄漏
        if matches!(decision, ApprovalDecision::Rejected(_)) {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&action_id);
            }
        }

        decision
    }
}

/// Gateway main 线程：处理 Adapter 回复的 ApprovalResponse。
///
/// 1. 通过 action_id 查 pending map
/// 2. 按 action 类型查白名单
/// 3. 白名单通过 → 用 Adapter 给的决策；不通过 → 强制 Rejected
/// 4. 若决策是 ApproveAndWhitelist，把操作加入运行时白名单并持久化
/// 5. 通过 oneshot 把最终决策回传 worker
/// 6. 返回 (action, final_decision) 用于回推 ApprovalResult 给 Adapter
#[allow(clippy::too_many_arguments)]
pub fn handle_approval_response(
    pending: &PendingMap,
    config: &ApprovalConfig,
    action_id: &str,
    operator_open_id: &str,
    operator_chat_id: Option<&str>,
    adapter_decision: ApprovalDecisionDto,
    runtime_whitelist: &SharedRuntimeWhitelist,
    wl_path: &std::path::Path,
) -> Option<(ApprovalAction, ApprovalDecision, Option<String>)> {
    // 1. 取出 pending entry
    let entry = {
        let mut map = pending.lock().ok()?;
        map.remove(action_id)?
    };

    // 2. 查对应类型的白名单
    let authenticator = config.authenticator_for(&entry.action);
    let final_decision = match authenticator {
        None => {
            // 该类未配白名单 = 不需要审批（直接放行 Adapter 给的决策）
            // 注：这种情况其实不应该发生（Hook 不会发起请求），但兜底
            adapter_decision.into()
        }
        Some(auth) => {
            // 用 Authenticator 校验 operator
            let src = crate::protocol::TriggerSource {
                platform: "feishu".into(), // M7 P1 仅支持飞书
                user: operator_open_id.to_string(),
                group: operator_chat_id.map(|s| s.to_string()),
                raw: String::new(),
            };
            match auth.check(&src) {
                crate::auth::AuthResult::Allowed => {
                    // 白名单通过：用 Adapter 给的决策
                    let core_decision: ApprovalDecision = adapter_decision.clone().into();
                    // 若是 ApproveAndWhitelist：把操作加入运行时白名单
                    if matches!(core_decision, ApprovalDecision::ApproveAndWhitelist) {
                        if let Ok(mut wl) = runtime_whitelist.lock() {
                            let added = wl.add(&entry.action);
                            if let Err(e) = wl.save(wl_path) {
                                eprintln!("[gateway] 运行时白名单保存失败: {e}");
                            } else if added {
                                eprintln!(
                                    "[gateway] 操作已加入运行时白名单并持久化: {:?}",
                                    entry.action
                                );
                            }
                        }
                        // 对 worker 返回 Approved（执行该操作）
                        ApprovalDecision::Approved
                    } else {
                        core_decision
                    }
                }
                crate::auth::AuthResult::Denied { reason: _ } => {
                    // 白名单不通过：强制 Rejected，忽略 Adapter 决策
                    ApprovalDecision::Rejected(format!(
                        "操作员 {} 不在审批白名单",
                        operator_open_id
                    ))
                }
            }
        }
    };

    // 3. 回传 worker
    let operator = if matches!(final_decision, ApprovalDecision::Approved) {
        Some(operator_open_id.to_string())
    } else {
        None
    };
    let _ = entry.tx.send(final_decision.clone());

    Some((entry.action, final_decision, operator))
}

/// 极简 UUID v4 生成（不引依赖，用时间戳 + 计数器 + 随机）。
/// 用于 action_id。不追求密码学安全，只追求全局唯一。
fn uuid_v4_simple() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let cnt = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{ts:016x}-{cnt:016x}")
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::WhitelistEntry;
    use crate::runtime_whitelist::RuntimeWhitelist;
    use std::sync::mpsc;

    /// 测试辅助：构造共享运行时白名单 + 临时文件路径。
    fn make_runtime_wl() -> (SharedRuntimeWhitelist, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runtime_whitelist.toml");
        // dir 在函数返回后 drop，但测试跑得快，path 仍可用
        std::mem::forget(dir);
        (Arc::new(Mutex::new(RuntimeWhitelist::default())), path)
    }

    #[test]
    fn uuid_v4_simple_is_unique() {
        let a = uuid_v4_simple();
        let b = uuid_v4_simple();
        assert_ne!(a, b, "连续生成的 UUID 应不同");
        assert!(a.len() > 10, "UUID 应有一定长度");
    }

    #[test]
    fn handle_response_returns_none_for_unknown_action_id() {
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig::default();
        let (wl, wl_path) = make_runtime_wl();
        let r = handle_approval_response(
            &pending,
            &cfg,
            "unknown-id",
            "ou_x",
            None,
            ApprovalDecisionDto::Approved,
            &wl,
            &wl_path,
        );
        assert!(r.is_none(), "未知 action_id 应返回 None");
    }

    #[test]
    fn handle_response_passes_through_when_no_whitelist_configured() {
        // 该类未配白名单（None）= 不需要审批 = 直接放行 Adapter 给的决策
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig::default(); // write/command/delete 全 None
        let (wl, wl_path) = make_runtime_wl();

        let (tx, rx) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-1".into(),
            PendingEntry {
                action: ApprovalAction::WriteFile {
                    path: "a".into(),
                    content_preview: "".into(),
                },
                tx,
            },
        );

        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-1",
            "ou_anyone",
            None,
            ApprovalDecisionDto::Approved,
            &wl,
            &wl_path,
        );
        let (action, decision, operator) = r.unwrap();
        assert!(matches!(action, ApprovalAction::WriteFile { .. }));
        assert_eq!(decision, ApprovalDecision::Approved);
        assert_eq!(operator, Some("ou_anyone".to_string()));
        // worker 端应收到 Approved
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            ApprovalDecision::Approved
        );
    }

    #[test]
    fn handle_response_rejects_when_whitelist_empty() {
        // 配了空数组 = 全拒
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig {
            timeout_secs: 60,
            write: Some(vec![]), // 空白名单
            command: None,
            delete: None,
        };
        let (wl, wl_path) = make_runtime_wl();

        let (tx, rx) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-2".into(),
            PendingEntry {
                action: ApprovalAction::WriteFile {
                    path: "a".into(),
                    content_preview: "".into(),
                },
                tx,
            },
        );

        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-2",
            "ou_anyone",
            None,
            ApprovalDecisionDto::Approved, // Adapter 给 Approved
            &wl,
            &wl_path,
        );
        let (_action, decision, operator) = r.unwrap();
        // Gateway 应强制 Rejected（白名单为空）
        match decision {
            ApprovalDecision::Rejected(reason) => {
                assert!(reason.contains("不在审批白名单"), "reason: {reason}")
            }
            ApprovalDecision::Approved => panic!("空白名单应拒绝"),
            ApprovalDecision::ApproveAndWhitelist => panic!("不应返回 ApproveAndWhitelist"),
        }
        assert!(operator.is_none(), "拒绝时不应有 operator");
        match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            ApprovalDecision::Rejected(_) => {}
            ApprovalDecision::Approved => panic!("worker 应收到 Rejected"),
            ApprovalDecision::ApproveAndWhitelist => panic!("worker 不应收到 ApproveAndWhitelist"),
        }
    }

    #[test]
    fn handle_response_checks_whitelist_for_write() {
        // 配 write 白名单 [ou_admin]，跑命令未配（None=不审批该类）
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig {
            timeout_secs: 60,
            write: Some(vec![WhitelistEntry {
                platform: "feishu".into(),
                user: Some("ou_admin".into()),
                group: None,
            }]),
            command: None,
            delete: None,
        };
        let (wl, wl_path) = make_runtime_wl();

        // 场景 1：白名单内用户 → 通过
        let (tx1, _rx1) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-write-1".into(),
            PendingEntry {
                action: ApprovalAction::WriteFile {
                    path: "a".into(),
                    content_preview: "".into(),
                },
                tx: tx1,
            },
        );
        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-write-1",
            "ou_admin",
            None,
            ApprovalDecisionDto::Approved,
            &wl,
            &wl_path,
        );
        let (_, decision, _) = r.unwrap();
        assert_eq!(decision, ApprovalDecision::Approved);

        // 场景 2：白名单外用户 → 拒绝
        let (tx2, _rx2) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-write-2".into(),
            PendingEntry {
                action: ApprovalAction::WriteFile {
                    path: "a".into(),
                    content_preview: "".into(),
                },
                tx: tx2,
            },
        );
        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-write-2",
            "ou_other",
            None,
            ApprovalDecisionDto::Approved, // 即使 Adapter 给 Approved
            &wl,
            &wl_path,
        );
        let (_, decision, _) = r.unwrap();
        match decision {
            ApprovalDecision::Rejected(reason) => {
                assert!(reason.contains("不在审批白名单"))
            }
            ApprovalDecision::Approved => panic!("白名单外应拒绝"),
            ApprovalDecision::ApproveAndWhitelist => panic!("不应返回 ApproveAndWhitelist"),
        }
    }

    #[test]
    fn handle_response_distinguishes_action_types() {
        // 配 write=[ou_admin]，command=[ou_ops]
        // 写文件给 ou_ops → 拒绝；跑命令给 ou_ops → 通过
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig {
            timeout_secs: 60,
            write: Some(vec![WhitelistEntry {
                platform: "feishu".into(),
                user: Some("ou_admin".into()),
                group: None,
            }]),
            command: Some(vec![WhitelistEntry {
                platform: "feishu".into(),
                user: Some("ou_ops".into()),
                group: None,
            }]),
            delete: None,
        };
        let (wl, wl_path) = make_runtime_wl();

        // ou_ops 审批写文件 → 拒绝（不在 write 白名单）
        let (tx1, _rx1) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-w".into(),
            PendingEntry {
                action: ApprovalAction::WriteFile {
                    path: "a".into(),
                    content_preview: "".into(),
                },
                tx: tx1,
            },
        );
        let (_, d, _) = handle_approval_response(
            &pending,
            &cfg,
            "id-w",
            "ou_ops",
            None,
            ApprovalDecisionDto::Approved,
            &wl,
            &wl_path,
        )
        .unwrap();
        assert!(matches!(d, ApprovalDecision::Rejected(_)));

        // ou_ops 审批跑命令 → 通过（在 command 白名单）
        let (tx2, _rx2) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-c".into(),
            PendingEntry {
                action: ApprovalAction::RunCommand {
                    program: "pytest".into(),
                    args: vec![],
                },
                tx: tx2,
            },
        );
        let (_, d, _) = handle_approval_response(
            &pending,
            &cfg,
            "id-c",
            "ou_ops",
            None,
            ApprovalDecisionDto::Approved,
            &wl,
            &wl_path,
        )
        .unwrap();
        assert_eq!(d, ApprovalDecision::Approved);
    }

    #[test]
    fn handle_response_approve_and_whitelist_adds_to_runtime_wl() {
        // 管理员点击"批准并加入白名单"：操作应加入 runtime_whitelist 并持久化
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig {
            timeout_secs: 60,
            command: Some(vec![WhitelistEntry {
                platform: "feishu".into(),
                user: Some("ou_admin".into()),
                group: None,
            }]),
            write: None,
            delete: None,
        };
        let (wl, wl_path) = make_runtime_wl();

        let (tx, _rx) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-wl".into(),
            PendingEntry {
                action: ApprovalAction::RunCommand {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                },
                tx,
            },
        );

        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-wl",
            "ou_admin",
            None,
            ApprovalDecisionDto::ApproveAndWhitelist,
            &wl,
            &wl_path,
        );
        let (_, decision, operator) = r.unwrap();
        // worker 收到 Approved（不是 ApproveAndWhitelist）
        assert_eq!(decision, ApprovalDecision::Approved);
        assert_eq!(operator, Some("ou_admin".to_string()));

        // runtime_whitelist 应包含该命令
        let wl_guard = wl.lock().unwrap();
        assert!(
            wl_guard.command.contains(&"cargo:test".to_string()),
            "runtime_whitelist 应包含 cargo:test，实际: {:?}",
            wl_guard.command
        );

        // 文件应已持久化
        assert!(wl_path.exists(), "runtime_whitelist.toml 应已创建");
    }

    #[test]
    fn handle_response_approve_and_whitelist_rejected_for_non_admin() {
        // 非白名单用户点击"批准并加入白名单"：应被拒绝，不加入 runtime_whitelist
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let cfg = ApprovalConfig {
            timeout_secs: 60,
            command: Some(vec![WhitelistEntry {
                platform: "feishu".into(),
                user: Some("ou_admin".into()),
                group: None,
            }]),
            write: None,
            delete: None,
        };
        let (wl, wl_path) = make_runtime_wl();

        let (tx, _rx) = mpsc::sync_channel(1);
        pending.lock().unwrap().insert(
            "id-wl2".into(),
            PendingEntry {
                action: ApprovalAction::RunCommand {
                    program: "cargo".into(),
                    args: vec!["test".into()],
                },
                tx,
            },
        );

        let r = handle_approval_response(
            &pending,
            &cfg,
            "id-wl2",
            "ou_other", // 非白名单用户
            None,
            ApprovalDecisionDto::ApproveAndWhitelist,
            &wl,
            &wl_path,
        );
        let (_, decision, _) = r.unwrap();
        assert!(
            matches!(decision, ApprovalDecision::Rejected(_)),
            "非白名单用户应被拒绝"
        );

        // runtime_whitelist 不应包含该命令
        let wl_guard = wl.lock().unwrap();
        assert!(
            wl_guard.command.is_empty(),
            "非白名单用户的操作不应加入 runtime_whitelist"
        );
    }
}
