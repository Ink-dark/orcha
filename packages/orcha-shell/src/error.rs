//! Orcha Shell 错误类型。
//!
//! [`ShellError`] 覆盖 Shell 层的全部错误：鉴权失败、事件缺失、
//! Adapter 执行失败、HTTP 解析失败等。实现 `IntoResponse` 让 HTTP 层
//! 直接把错误转成对应的 HTTP 状态码 + JSON body。

use thiserror::Error;

/// Shell 层错误。
#[derive(Debug, Error)]
pub enum ShellError {
    /// API Key 缺失或不匹配（HTTP 401）。
    #[error("unauthorized: api key missing or invalid")]
    Unauthorized,

    /// 请求 body 不是合法的 OrchaEvent JSON（HTTP 400）。
    #[error("invalid request body: {0}")]
    InvalidBody(String),

    /// 路径参数缺失或格式错误，例如 `/status/{id}` 缺 id（HTTP 400）。
    #[error("invalid path: {0}")]
    InvalidPath(String),

    /// 查询的 event_id 不存在（HTTP 404）。
    #[error("event not found: {0}")]
    EventNotFound(String),

    /// 找不到可处理该事件的 Adapter（HTTP 500）。
    #[error("no adapter available for source: {0}")]
    NoAdapter(String),

    /// Adapter 执行过程中出错（HTTP 500）。
    #[error("adapter error: {0}")]
    Adapter(String),

    /// HTTP 请求解析失败（HTTP 400）。
    #[error("http parse error: {0}")]
    HttpParse(String),

    /// IO 错误（HTTP 500）。
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化/反序列化错误（HTTP 400/500）。
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// orcha-core 状态机 / store 错误（HTTP 500）。
    #[error("core error: {0}")]
    Core(#[from] orcha_core::CoreError),

    /// 其他内部错误（HTTP 500）。
    #[error("{0}")]
    Other(String),
}

impl ShellError {
    /// 返回该错误对应的 HTTP 状态码。
    pub fn http_status(&self) -> u16 {
        match self {
            ShellError::Unauthorized => 401,
            ShellError::InvalidBody(_) | ShellError::InvalidPath(_) | ShellError::HttpParse(_) => {
                400
            }
            ShellError::EventNotFound(_) => 404,
            ShellError::NoAdapter(_) => 404,
            ShellError::Adapter(_) | ShellError::Io(_) | ShellError::Other(_) => 500,
            ShellError::Json(_) => 400,
            ShellError::Core(_) => 500,
        }
    }

    /// 把错误包装成 HTTP 响应 body（JSON）。
    pub fn to_response_body(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.to_string(),
            "status": self.http_status(),
        })
    }
}

impl From<anyhow::Error> for ShellError {
    fn from(e: anyhow::Error) -> Self {
        ShellError::Other(e.to_string())
    }
}

/// Shell 层 Result 别名。
pub type Result<T> = std::result::Result<T, ShellError>;
