//! Adapter 连接注册表（M7）。
//!
//! Gateway 维护所有已连接 Adapter 的列表，每条连接一个 `mpsc::Sender`。
//! worker 线程产出 `CardUpdate` / `Notify` / `TaskResult` 时，通过
//! [`broadcast`](AdapterRegistry::broadcast) 发给所有连接。
//!
//! Adapter reader 线程退出时调 [`unregister`](AdapterRegistry::unregister)
//! 自动清理；`broadcast` 也会乐观清理已断开的 sender。
//!
//! 模型：一条 IPC 连接 = 一对线程
//! - **reader 线程**：从 stream 读 `AdapterToGateway`，分发到 queue / auth。
//!   持有本连接的 sender 副本，用于回定向消息（`AuthResult` / `HeartbeatAck`）。
//! - **writer 线程**：从 `mpsc::Receiver<GatewayToAdapter>` 读消息写到 stream。
//!   reader 退出时 drop sender 副本，writer 的 `recv()` 返回 Err 自然退出。
//!
//! `mpsc::Sender` 可 Clone（多 producer 单 consumer），所以 registry 内部持一份
//! 副本做 broadcast，reader 持另一份副本做定向回复，二者共享同一 channel。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use crate::protocol::GatewayToAdapter;

/// Adapter 连接 ID（自增，用于 unregister 时定位）。
pub type AdapterId = u64;

/// 一条 Adapter 连接的 sender 副本（registry 持有，用于 broadcast）。
struct AdapterConn {
    id: AdapterId,
    sender: mpsc::Sender<GatewayToAdapter>,
}

/// Adapter 连接注册表（线程安全，可 Clone 共享）。
#[derive(Clone, Default)]
pub struct AdapterRegistry {
    inner: Arc<Mutex<Vec<AdapterConn>>>,
    next_id: Arc<AtomicU64>,
}

impl AdapterRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// 注册一条新连接，返回 `(id, sender, receiver)`。
    ///
    /// - `sender`：reader 线程持有，用于回定向消息（`AuthResult` / `HeartbeatAck`）。
    /// - `receiver`：reader spawn writer 线程消费，写到 IPC stream。
    /// - registry 内部另持一份 sender 副本，供 [`broadcast`](Self::broadcast) 用。
    ///
    /// reader 线程退出时调 [`unregister`](Self::unregister) 清理。
    pub fn register(
        &self,
    ) -> (
        AdapterId,
        mpsc::Sender<GatewayToAdapter>,
        mpsc::Receiver<GatewayToAdapter>,
    ) {
        let (tx, rx) = mpsc::channel::<GatewayToAdapter>();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // registry 持 sender 副本做 broadcast；调用方持另一副本发定向消息。
        self.inner.lock().unwrap().push(AdapterConn {
            id,
            sender: tx.clone(),
        });
        (id, tx, rx)
    }

    /// 注销连接（reader 线程退出时调）。
    pub fn unregister(&self, id: AdapterId) {
        let mut conns = self.inner.lock().unwrap();
        conns.retain(|c| c.id != id);
    }

    /// 广播消息给所有连接。
    ///
    /// 乐观 send：失败的 sender（writer 已退出 / 连接断开）会被
    /// [`Vec::retain`] 清理，避免死连接堆积。
    pub fn broadcast(&self, msg: &GatewayToAdapter) {
        let mut conns = self.inner.lock().unwrap();
        conns.retain(|c| c.sender.send(msg.clone()).is_ok());
    }

    /// 当前连接数（诊断用）。
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().len()
    }

    /// 是否无连接。
    pub fn is_empty(&self) -> bool {
        self.inner.lock().unwrap().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_broadcast_delivers_to_all() {
        let reg = AdapterRegistry::new();
        let (_id1, _tx1, rx1) = reg.register();
        let (_id2, _tx2, rx2) = reg.register();
        assert_eq!(reg.len(), 2);

        let msg = GatewayToAdapter::HeartbeatAck { ts_ms: 1 };
        reg.broadcast(&msg);

        assert_eq!(rx1.recv().unwrap(), msg);
        assert_eq!(rx2.recv().unwrap(), msg);
    }

    #[test]
    fn unregister_stops_delivery() {
        let reg = AdapterRegistry::new();
        let (id1, _tx1, rx1) = reg.register();
        let (_id2, _tx2, rx2) = reg.register();

        reg.unregister(id1);
        assert_eq!(reg.len(), 1);

        let msg = GatewayToAdapter::HeartbeatAck { ts_ms: 2 };
        reg.broadcast(&msg);
        // rx1 不应再收到
        assert!(rx1.try_recv().is_err());
        // rx2 应收到
        assert_eq!(rx2.recv().unwrap(), msg);
    }

    #[test]
    fn broadcast_cleans_up_dead_senders() {
        let reg = AdapterRegistry::new();
        {
            let (_id, _tx, rx) = reg.register();
            drop(rx); // receiver drop，sender.send 会失败
        }
        assert_eq!(reg.len(), 1, "未清理前应还有 1 条死连接");

        let msg = GatewayToAdapter::HeartbeatAck { ts_ms: 3 };
        reg.broadcast(&msg);
        assert_eq!(reg.len(), 0, "broadcast 应清理掉死 sender");
    }

    #[test]
    fn reader_sender_delivers_directed_message() {
        // reader 持有的 sender 副本应能把定向消息投递到同一 receiver
        // （与 registry 的 broadcast 共享同一 channel）。
        let reg = AdapterRegistry::new();
        let (_id, tx, rx) = reg.register();

        let directed = GatewayToAdapter::AuthResult {
            allowed: true,
            reason: None,
        };
        tx.send(directed.clone()).unwrap();

        let received = rx.recv().unwrap();
        assert_eq!(received, directed);
    }

    #[test]
    fn empty_registry_is_empty() {
        let reg = AdapterRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        // 空注册表 broadcast 不应 panic
        reg.broadcast(&GatewayToAdapter::HeartbeatAck { ts_ms: 4 });
    }

    #[test]
    fn registry_clone_shares_state() {
        // Clone 后应共享同一份内部状态（Arc 语义）。
        let reg = AdapterRegistry::new();
        let reg2 = reg.clone();
        let (_id, _tx, rx) = reg.register();
        let msg = GatewayToAdapter::HeartbeatAck { ts_ms: 5 };
        reg2.broadcast(&msg); // 通过 clone 广播
        assert_eq!(rx.recv().unwrap(), msg);
        assert_eq!(reg2.len(), 1);
    }
}
