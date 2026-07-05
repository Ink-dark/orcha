//! ShellAdapter 抽象：把外部来源（CLI / IM webhook / HTTP）归一为 OrchaEvent，
//! 并把 Orcha 的处理结果回传给来源。
//!
//! M5 提供 [`ShellAdapter`] trait + [`CliAdapter`]（CLI 适配器）。
//! M6 的 LarkShellAdapter 会复用同一 trait。
//!
//! trait 设计要点：
//! - 同步接口（项目其余部分均为同步 IO，避免引入 tokio 复杂度）。
//! - `handle_event` 由实现决定是同步执行完再返回，还是派发到后台线程。
//!   MVP 的 CliAdapter 同步执行；M6 的 LarkShellAdapter 可能后台执行。
//! - `get_response` 由 Shell 在 `GET /status/{id}` 时调用；Adapter 自己持有
//!   event→response 映射（不集中存 shell，避免 Shell 与 Adapter 状态耦合）。

use orcha_sdk::OrchaEvent;

use crate::error::Result;

/// Adapter 注册信息（`orcha shell list` 输出）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdapterInfo {
    /// Adapter 名称，如 `cli`、`lark`。
    pub name: String,
    /// 来源标识，对应 `OrchaEvent.source`，如 `cli`、`http`、`lark`。
    pub source: String,
    /// 一句话描述。
    pub description: String,
}

/// ShellAdapter trait：所有外部入口实现此接口。
///
/// 实现方负责：
/// 1. 接收 [`OrchaEvent`]（已被 Shell 分配好 `event_id`）。
/// 2. 把事件转发给 Core 处理（创建 Task、跑 Cycleround、产出 Artifact）。
/// 3. 在内部维护 event_id → [`orcha_sdk::OrchaResponse`] 映射，供
///    [`ShellAdapter::get_response`] 查询。
pub trait ShellAdapter: Send + Sync {
    /// 注册信息（name / source / description）。
    fn info(&self) -> AdapterInfo;

    /// Adapter 名称（`info().name` 的便捷方法）。
    fn name(&self) -> String {
        self.info().name
    }

    /// 处理一个事件，返回 event_id。
    ///
    /// 实现应：
    /// - 把 event_id 写入 OrchaResponse（关联请求与响应）。
    /// - 把 response 存入内部映射，供 `get_response` 查询。
    /// - 同步路径可在此函数内跑完整个 Cycleround；异步路径可派发到后台。
    fn handle_event(&self, event: OrchaEvent) -> Result<String>;

    /// 查询某 event 的当前响应。
    ///
    /// 不存在或尚未处理完时返回 `Err(EventNotFound)`。
    fn get_response(&self, event_id: &str) -> Result<orcha_sdk::OrchaResponse>;
}
