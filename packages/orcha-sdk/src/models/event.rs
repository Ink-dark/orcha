use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// 进入 Orcha 的事件类型。预留扩展点，M0 仅支持 `UserPrompt`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    UserPrompt,
    SystemSignal,
    Webhook,
}

/// 事件载荷中可能携带的附件元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Attachment {
    /// MIME 类型，例如 `text/plain`、`image/png`。
    pub mime_type: String,
    /// 附件可访问 URL 或本地路径。
    pub url: String,
}

/// 所有进入 Orcha 的外部消息必须先归一化为 `OrchaEvent`（README §3.1）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchaEvent {
    /// 形如 `EVT-xxxx`，由入口分配。
    pub event_id: String,
    /// 来源渠道标识，例如 `slack`、`cli`、`http`。
    pub source: String,
    /// 发起方用户标识（IM 平台用户 id 或本地用户名）。
    pub user_id: String,
    /// Unix 时间戳（秒）。
    pub timestamp: i64,
    /// 事件类型。
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// 事件载荷。
    pub payload: EventPayload,
}

/// `OrchaEvent.payload` 的内容，与 `EventType` 解耦以便后续扩展。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventPayload {
    /// 原始文本，例如 "@Orcha fix the bug in auth.py"。
    pub raw_text: String,
    /// 附件列表，无则为空数组。
    pub attachments: Vec<Attachment>,
}

impl Default for EventPayload {
    fn default() -> Self {
        Self {
            raw_text: String::new(),
            attachments: Vec::new(),
        }
    }
}
