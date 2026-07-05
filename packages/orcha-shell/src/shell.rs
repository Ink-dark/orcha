//! [`OrchaShell`] —— Shell 网关：注册 Adapter + 校验 API Key + 路由事件。
//!
//! 这是 M5 的核心结构，对应 README §3 的「Shell 层」：
//! 1. HTTP server 收到 `POST /run` → 解析 `OrchaEvent` → 校验 API Key →
//!    `Shell::submit_event` → 派发到匹配的 Adapter → 返回 `event_id`。
//! 2. HTTP server 收到 `GET /status/{id}` → `Shell::get_response` →
//!    查询 Adapter 的内部响应映射 → 返回 `OrchaResponse`。
//!
//! Adapter 选择策略（M5 MVP）：按 `OrchaEvent.source` 匹配 `AdapterInfo.source`；
//! 找不到则用第一个已注册的 Adapter 兜底。M6 引入 LarkShellAdapter 时可
//! 扩展更复杂的路由（如按事件类型分流）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use orcha_sdk::OrchaEvent;

use crate::adapter::{AdapterInfo, ShellAdapter};
use crate::error::{Result, ShellError};

/// Orcha Shell 网关。
///
/// 持有一组 `ShellAdapter` + API Key。线程安全（内部 `Mutex` 保护注册表
/// 与 event→adapter 映射）。
pub struct OrchaShell {
    adapters: Mutex<Vec<Arc<dyn ShellAdapter>>>,
    api_key: String,
    /// event_id → 处理它的 adapter name（便于 get_response 找对 adapter）。
    event_to_adapter: Mutex<HashMap<String, String>>,
}

impl OrchaShell {
    /// 以给定 API Key 构造空 Shell。
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            adapters: Mutex::new(Vec::new()),
            api_key: api_key.into(),
            event_to_adapter: Mutex::new(HashMap::new()),
        }
    }

    /// 注册一个 Adapter。后续 `submit_event` 按 source 路由。
    pub fn register_adapter(&self, adapter: Arc<dyn ShellAdapter>) {
        let mut adapters = self.adapters.lock().expect("adapters mutex poisoned");
        adapters.push(adapter);
    }

    /// 列出全部已注册 Adapter 的信息（`orcha shell list` 用）。
    pub fn list_adapters(&self) -> Vec<AdapterInfo> {
        let adapters = self.adapters.lock().expect("adapters mutex poisoned");
        adapters.iter().map(|a| a.info()).collect()
    }

    /// API Key 校验：传入 Authorization header 的值（如 `Bearer xxx`），
    /// 与内部存的 key 比对。空 / 不匹配返回 `Err(Unauthorized)`。
    pub fn check_api_key(&self, presented: Option<&str>) -> Result<()> {
        let presented = presented.ok_or(ShellError::Unauthorized)?;
        // 接受 `Bearer <key>` 或裸 `<key>` 两种形式。
        let key = presented
            .strip_prefix("Bearer ")
            .or_else(|| presented.strip_prefix("bearer "))
            .unwrap_or(presented);
        if key == self.api_key {
            Ok(())
        } else {
            Err(ShellError::Unauthorized)
        }
    }

    /// API Key 引用（用于 server 启动日志等只读场景）。
    pub fn api_key(&self) -> &str {
        &self.api_key
    }

    /// 按 source 选 Adapter：先精确匹配 `AdapterInfo.source`，否则用第一个兜底。
    fn pick_adapter(&self, source: &str) -> Result<Arc<dyn ShellAdapter>> {
        let adapters = self.adapters.lock().expect("adapters mutex poisoned");
        if adapters.is_empty() {
            return Err(ShellError::NoAdapter(source.into()));
        }
        // 先按 source 精确匹配。
        for a in adapters.iter() {
            if a.info().source == source {
                return Ok(a.clone());
            }
        }
        // 兜底：第一个。
        Ok(adapters[0].clone())
    }

    /// 提交一个事件：分配 / 校验 event_id → 派发到 Adapter → 返回 event_id。
    ///
    /// 如果 `event.event_id` 非空，直接用它；否则 Shell 分配一个 `EVT-{uuid}`。
    pub fn submit_event(&self, mut event: OrchaEvent) -> Result<String> {
        if event.event_id.is_empty() {
            event.event_id = format!("EVT-{}", uuid::Uuid::new_v4());
        }
        let adapter = self.pick_adapter(&event.source)?;
        let event_id = event.event_id.clone();
        let adapter_name = adapter.name();
        adapter.handle_event(event)?;

        // 记录 event_id → adapter 映射，便于 get_response 找对 adapter。
        let mut map = self
            .event_to_adapter
            .lock()
            .expect("event_to_adapter mutex poisoned");
        map.insert(event_id.clone(), adapter_name);
        Ok(event_id)
    }

    /// 查询某 event 的响应。先查 event→adapter 映射，再到对应 adapter 查。
    pub fn get_response(&self, event_id: &str) -> Result<orcha_sdk::OrchaResponse> {
        let map = self
            .event_to_adapter
            .lock()
            .expect("event_to_adapter mutex poisoned");
        let adapter_name = map
            .get(event_id)
            .ok_or_else(|| ShellError::EventNotFound(event_id.into()))?;
        let adapters = self.adapters.lock().expect("adapters mutex poisoned");
        let adapter = adapters
            .iter()
            .find(|a| a.info().name == *adapter_name)
            .ok_or_else(|| ShellError::EventNotFound(event_id.into()))?;
        adapter.get_response(event_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcha_sdk::{EventPayload, EventType, OrchaResponse, ResponseStatus};
    use std::sync::Mutex as StdMutex;

    /// 一个最简 mock adapter：handle_event 把 event_id 存进 response 映射。
    struct MockAdapter {
        name: String,
        source: String,
        responses: StdMutex<HashMap<String, OrchaResponse>>,
    }

    impl MockAdapter {
        fn new(name: &str, source: &str) -> Arc<Self> {
            Arc::new(Self {
                name: name.into(),
                source: source.into(),
                responses: StdMutex::new(HashMap::new()),
            })
        }
    }

    impl ShellAdapter for MockAdapter {
        fn info(&self) -> AdapterInfo {
            AdapterInfo {
                name: self.name.clone(),
                source: self.source.clone(),
                description: "mock".into(),
            }
        }
        fn handle_event(&self, event: OrchaEvent) -> Result<String> {
            let id = event.event_id.clone();
            let response = OrchaResponse {
                event_id: id.clone(),
                status: ResponseStatus::Final,
                content: event.payload.raw_text.clone(),
                artifacts: Vec::new(),
            };
            self.responses
                .lock()
                .expect("mock mutex poisoned")
                .insert(id.clone(), response);
            Ok(id)
        }
        fn get_response(&self, event_id: &str) -> Result<OrchaResponse> {
            self.responses
                .lock()
                .expect("mock mutex poisoned")
                .get(event_id)
                .cloned()
                .ok_or_else(|| ShellError::EventNotFound(event_id.into()))
        }
    }

    fn make_event(id: &str, source: &str, text: &str) -> OrchaEvent {
        OrchaEvent {
            event_id: id.into(),
            source: source.into(),
            user_id: "u".into(),
            timestamp: 0,
            event_type: EventType::UserPrompt,
            payload: EventPayload {
                raw_text: text.into(),
                attachments: Vec::new(),
            },
        }
    }

    #[test]
    fn check_api_key_accepts_bearer_prefix_and_plain() {
        let shell = OrchaShell::new("secret-key");
        // 裸 key。
        assert!(shell.check_api_key(Some("secret-key")).is_ok());
        // Bearer 前缀。
        assert!(shell.check_api_key(Some("Bearer secret-key")).is_ok());
        // 大小写不敏感前缀。
        assert!(shell.check_api_key(Some("bearer secret-key")).is_ok());
        // 缺失 / 错误 → 401。
        assert!(matches!(
            shell.check_api_key(None).unwrap_err(),
            ShellError::Unauthorized
        ));
        assert!(matches!(
            shell.check_api_key(Some("wrong")).unwrap_err(),
            ShellError::Unauthorized
        ));
    }

    #[test]
    fn list_adapters_returns_registered_in_order() {
        let shell = OrchaShell::new("k");
        assert!(shell.list_adapters().is_empty());
        shell.register_adapter(MockAdapter::new("cli", "cli"));
        shell.register_adapter(MockAdapter::new("lark", "lark"));
        let infos = shell.list_adapters();
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "cli");
        assert_eq!(infos[1].name, "lark");
    }

    #[test]
    fn submit_event_routes_by_source() {
        let shell = OrchaShell::new("k");
        let cli = MockAdapter::new("cli", "cli");
        let lark = MockAdapter::new("lark", "lark");
        shell.register_adapter(cli.clone());
        shell.register_adapter(lark.clone());

        // source=cli 应路由到 cli adapter。
        let id = shell
            .submit_event(make_event("E-1", "cli", "hello cli"))
            .unwrap();
        assert_eq!(id, "E-1");
        let r = shell.get_response("E-1").unwrap();
        assert_eq!(r.content, "hello cli");

        // source=lark 应路由到 lark adapter。
        let _id2 = shell
            .submit_event(make_event("E-2", "lark", "hello lark"))
            .unwrap();
        let r2 = shell.get_response("E-2").unwrap();
        assert_eq!(r2.content, "hello lark");

        // 验证路由到了不同 adapter：lark adapter 不应有 E-1。
        assert!(lark.get_response("E-1").is_err());
        assert!(cli.get_response("E-2").is_err());
    }

    #[test]
    fn submit_event_falls_back_to_first_adapter_when_source_unmatched() {
        let shell = OrchaShell::new("k");
        shell.register_adapter(MockAdapter::new("cli", "cli"));
        // source=unknown 应兜底到 cli（第一个）。
        let id = shell
            .submit_event(make_event("E-1", "unknown", "x"))
            .unwrap();
        assert!(shell.get_response(&id).is_ok());
    }

    #[test]
    fn submit_event_assigns_event_id_when_empty() {
        let shell = OrchaShell::new("k");
        shell.register_adapter(MockAdapter::new("cli", "cli"));
        let mut event = make_event("", "cli", "auto id");
        event.event_id.clear();
        let id = shell.submit_event(event).unwrap();
        assert!(
            id.starts_with("EVT-"),
            "auto id should be EVT-<uuid>, got {id}"
        );
    }

    #[test]
    fn submit_event_errors_when_no_adapter_registered() {
        let shell = OrchaShell::new("k");
        let err = shell
            .submit_event(make_event("E-1", "cli", "x"))
            .unwrap_err();
        assert!(matches!(err, ShellError::NoAdapter(_)));
    }

    #[test]
    fn get_response_missing_returns_event_not_found() {
        let shell = OrchaShell::new("k");
        shell.register_adapter(MockAdapter::new("cli", "cli"));
        let err = shell.get_response("E-missing").unwrap_err();
        assert!(matches!(err, ShellError::EventNotFound(_)));
    }
}
