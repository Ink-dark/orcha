use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// `OrchaResponse.status` 的取值（README §3.2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseStatus {
    Streaming,
    Final,
    Error,
}

impl std::fmt::Display for ResponseStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            ResponseStatus::Streaming => "STREAMING",
            ResponseStatus::Final => "FINAL",
            ResponseStatus::Error => "ERROR",
        };
        f.write_str(s)
    }
}

/// `OrchaResponse.artifacts[].type` 的取值，预留更多产物类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResponseArtifactType {
    Diff,
    Log,
    Url,
    FileRef,
}

/// 响应中携带的产物引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ResponseArtifact {
    /// 产物类型。
    #[serde(rename = "type")]
    pub artifact_type: ResponseArtifactType,
    /// 产物可访问 URL。
    pub url: String,
}

/// Orcha 对外返回的响应（README §3.2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OrchaResponse {
    /// 对应触发该响应的 `OrchaEvent.event_id`。
    pub event_id: String,
    /// 当前响应阶段。
    pub status: ResponseStatus,
    /// 面向用户的文本内容（流式时为增量，final 时为终态）。
    pub content: String,
    /// 关联产物。
    pub artifacts: Vec<ResponseArtifact>,
}

impl OrchaResponse {
    /// 构造一个空的 final 响应。
    pub fn final_empty(event_id: impl Into<String>) -> Self {
        Self {
            event_id: event_id.into(),
            status: ResponseStatus::Final,
            content: String::new(),
            artifacts: Vec::new(),
        }
    }
}
