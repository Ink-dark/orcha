//! LLM 客户端 trait 与 OpenAI 兼容端点实现。
//!
//! 覆盖 OpenAI / DeepSeek / Ollama `/v1/chat/completions` / vLLM 等。
//! 同步阻塞，用 `ureq`；不引入 tokio。

use std::collections::HashMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 聊天消息（OpenAI Chat 格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system` / `user` / `assistant` / `tool`
    pub role: String,
    pub content: String,
    /// assistant 消息可携带 tool_calls
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallRequest>>,
    /// tool 消息必须携带 tool_call_id
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: None,
        }
    }
    pub fn assistant_with_tool_calls(
        content: impl Into<String>,
        calls: Vec<ToolCallRequest>,
    ) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: Some(calls),
            tool_call_id: None,
        }
    }
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// LLM 工具定义（OpenAI function calling 格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// `"function"` —— 此处固定为 "function"
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDef,
}

impl ToolDefinition {
    pub fn new(name: &str, description: &str, parameters: serde_json::Value) -> Self {
        Self {
            tool_type: "function".into(),
            function: FunctionDef {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// LLM 返回的工具调用请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRequest {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// 工具调用的函数名与参数。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON 字符串形式的参数，调用方自行解析
    pub arguments: String,
}

impl FunctionCall {
    /// 解析 arguments 为键值对。
    pub fn parse_args(&self) -> Result<HashMap<String, serde_json::Value>, String> {
        serde_json::from_str(&self.arguments).map_err(|e| format!("解析工具参数失败: {e}"))
    }
}

/// LLM chat completion 的完整响应。
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCallRequest>,
}

impl ChatResponse {
    pub fn text(content: String) -> Self {
        Self {
            content: Some(content),
            tool_calls: vec![],
        }
    }
    pub fn has_tool_calls(&self) -> bool {
        !self.tool_calls.is_empty()
    }
}

/// LLM 客户端配置。通过 [`LlmConfig::from_env`] 从环境变量读取。
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// API 基址，例如 `https://api.openai.com/v1`。
    pub base_url: String,
    /// API Key。
    pub api_key: String,
    /// 模型名，例如 `gpt-4o-mini` / `deepseek-chat`。
    pub model: String,
    /// 单次请求超时（含网络）。LLM 调用慢，默认 120s。
    pub timeout: Duration,
    /// 采样温度。
    pub temperature: f32,
}

impl LlmConfig {
    /// 从环境变量读取配置。
    ///
    /// 必填：`ORCHA_LLM_API_KEY`。
    /// 可选：`ORCHA_LLM_BASE_URL`（默认 OpenAI）、`ORCHA_LLM_MODEL`（默认 gpt-4o-mini）。
    pub fn from_env() -> Result<Self, LlmError> {
        let api_key = std::env::var("ORCHA_LLM_API_KEY")
            .map_err(|_| LlmError::Config("ORCHA_LLM_API_KEY 未设置".into()))?;
        if api_key.trim().is_empty() {
            return Err(LlmError::Config("ORCHA_LLM_API_KEY 为空".into()));
        }
        let base_url = std::env::var("ORCHA_LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let model = std::env::var("ORCHA_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            timeout: Duration::from_secs(120),
            temperature: 0.2,
        })
    }
}

/// LLM 客户端统一接口。
///
/// 同步阻塞；Cycleround 在 `thread::spawn` 里调用即可并发。
pub trait LlmClient: Send + Sync {
    /// 跑一次 chat completion，返回 assistant 文本。
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError>;

    /// 跑一次 chat completion 并支持 tool calling。
    ///
    /// 默认实现降级为 `chat()`；支持 function calling 的端点应 override。
    fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        _tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        let text = self.chat(messages)?;
        Ok(ChatResponse::text(text))
    }
}

/// OpenAI 兼容端点客户端（`POST /v1/chat/completions`）。
pub struct OpenAiCompatibleClient {
    config: LlmConfig,
    agent: ureq::Agent,
}

impl OpenAiCompatibleClient {
    pub fn new(config: LlmConfig) -> Self {
        let agent = ureq::AgentBuilder::new().timeout(config.timeout).build();
        Self { config, agent }
    }

    fn do_chat(
        &self,
        messages: &[ChatMessage],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<ChatResponse, LlmError> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": self.config.temperature,
            "stream": false,
        });
        if let Some(tools) = tools {
            if !tools.is_empty() {
                body["tools"] = serde_json::to_value(tools).unwrap_or_default();
                body["tool_choice"] = serde_json::json!("auto");
            }
        }

        let resp = self
            .agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", self.config.api_key))
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(LlmError::from_ureq)?;

        let parsed: ChatCompletionResponse = resp
            .into_json()
            .map_err(|e| LlmError::Parse(format!("解析响应失败: {e}")))?;

        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| LlmError::Parse("响应无 choices".into()))?;

        let content = choice.message.content.filter(|c| !c.is_empty());
        let tool_calls = choice.message.tool_calls.unwrap_or_default();

        Ok(ChatResponse {
            content,
            tool_calls,
        })
    }
}

impl LlmClient for OpenAiCompatibleClient {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError> {
        let resp = self.do_chat(messages, None)?;
        resp.content
            .map(|c| c.trim().to_string())
            .ok_or_else(|| LlmError::Parse("响应无 content".into()))
    }

    fn chat_with_tools(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse, LlmError> {
        self.do_chat(messages, Some(tools))
    }
}

/// OpenAI chat completion 响应（仅解析需要字段）。
#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ResponseMessage {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallRequest>>,
}

/// LLM 调用错误。
#[derive(Debug, Error)]
pub enum LlmError {
    #[error("配置错误: {0}")]
    Config(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("HTTP 状态码 {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("解析错误: {0}")]
    Parse(String),
}

impl LlmError {
    /// 把 ureq::Error 转成 LlmError，区分 HTTP 状态码与网络错误。
    fn from_ureq(e: ureq::Error) -> LlmError {
        match e {
            ureq::Error::Status(status, resp) => {
                let body = resp.into_string().unwrap_or_default();
                LlmError::HttpStatus { status, body }
            }
            other => LlmError::Network(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn env_lock() -> &'static Mutex<()> {
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn config_from_env_fails_without_api_key() {
        let _guard = env_lock().lock().unwrap();
        let old_key = std::env::var("ORCHA_LLM_API_KEY").ok();
        std::env::remove_var("ORCHA_LLM_API_KEY");
        let err = LlmConfig::from_env().unwrap_err();
        assert!(matches!(err, LlmError::Config(_)));
        if let Some(v) = old_key {
            std::env::set_var("ORCHA_LLM_API_KEY", v);
        }
    }

    #[test]
    fn config_from_env_uses_defaults_when_only_key_set() {
        let _guard = env_lock().lock().unwrap();
        let old_key = std::env::var("ORCHA_LLM_API_KEY").ok();
        let old_url = std::env::var("ORCHA_LLM_BASE_URL").ok();
        let old_model = std::env::var("ORCHA_LLM_MODEL").ok();

        std::env::set_var("ORCHA_LLM_API_KEY", "test-key");
        std::env::remove_var("ORCHA_LLM_BASE_URL");
        std::env::remove_var("ORCHA_LLM_MODEL");
        let cfg = LlmConfig::from_env().unwrap();
        assert_eq!(cfg.api_key, "test-key");
        assert_eq!(cfg.base_url, "https://api.openai.com/v1");
        assert_eq!(cfg.model, "gpt-4o-mini");

        std::env::remove_var("ORCHA_LLM_API_KEY");
        if let Some(v) = old_key {
            std::env::set_var("ORCHA_LLM_API_KEY", v);
        }
        if let Some(v) = old_url {
            std::env::set_var("ORCHA_LLM_BASE_URL", v);
        } else {
            std::env::remove_var("ORCHA_LLM_BASE_URL");
        }
        if let Some(v) = old_model {
            std::env::set_var("ORCHA_LLM_MODEL", v);
        } else {
            std::env::remove_var("ORCHA_LLM_MODEL");
        }
    }

    #[test]
    fn config_from_env_respects_custom_base_url() {
        let _guard = env_lock().lock().unwrap();
        let old_key = std::env::var("ORCHA_LLM_API_KEY").ok();
        let old_url = std::env::var("ORCHA_LLM_BASE_URL").ok();
        let old_model = std::env::var("ORCHA_LLM_MODEL").ok();

        std::env::set_var("ORCHA_LLM_API_KEY", "k");
        std::env::set_var("ORCHA_LLM_BASE_URL", "http://localhost:11434/v1/");
        std::env::set_var("ORCHA_LLM_MODEL", "llama3");
        let cfg = LlmConfig::from_env().unwrap();
        assert_eq!(cfg.base_url, "http://localhost:11434/v1");
        assert_eq!(cfg.model, "llama3");

        std::env::remove_var("ORCHA_LLM_API_KEY");
        std::env::remove_var("ORCHA_LLM_BASE_URL");
        std::env::remove_var("ORCHA_LLM_MODEL");
        if let Some(v) = old_key {
            std::env::set_var("ORCHA_LLM_API_KEY", v);
        }
        if let Some(v) = old_url {
            std::env::set_var("ORCHA_LLM_BASE_URL", v);
        } else {
            std::env::remove_var("ORCHA_LLM_BASE_URL");
        }
        if let Some(v) = old_model {
            std::env::set_var("ORCHA_LLM_MODEL", v);
        } else {
            std::env::remove_var("ORCHA_LLM_MODEL");
        }
    }

    #[test]
    fn chat_message_helpers_set_role() {
        let s = ChatMessage::system("be helpful");
        assert_eq!(s.role, "system");
        let u = ChatMessage::user("hi");
        assert_eq!(u.role, "user");
    }

    #[test]
    fn tool_message_has_call_id() {
        let t = ChatMessage::tool("call_1", "result");
        assert_eq!(t.role, "tool");
        assert_eq!(t.tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn assistant_with_tool_calls() {
        let calls = vec![ToolCallRequest {
            id: "c1".into(),
            call_type: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"a.py"}"#.into(),
            },
        }];
        let m = ChatMessage::assistant_with_tool_calls("", calls);
        assert_eq!(m.role, "assistant");
        assert!(m.tool_calls.is_some());
    }

    #[test]
    fn function_call_parse_args() {
        let fc = FunctionCall {
            name: "f".into(),
            arguments: r#"{"path":"a.py"}"#.into(),
        };
        let args = fc.parse_args().unwrap();
        assert_eq!(args.get("path").and_then(|v| v.as_str()), Some("a.py"));
    }

    #[test]
    fn tool_definition_serializes_as_openai_format() {
        let td = ToolDefinition::new(
            "read_file",
            "read a file",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
        );
        let json = serde_json::to_value(&td).unwrap();
        assert_eq!(json["type"], "function");
        assert_eq!(json["function"]["name"], "read_file");
    }

    #[test]
    fn chat_response_text() {
        let r = ChatResponse::text("hello".into());
        assert_eq!(r.content.as_deref(), Some("hello"));
        assert!(r.tool_calls.is_empty());
        assert!(!r.has_tool_calls());
    }

    #[test]
    fn chat_response_has_tool_calls() {
        let r = ChatResponse {
            content: None,
            tool_calls: vec![ToolCallRequest {
                id: "c1".into(),
                call_type: "function".into(),
                function: FunctionCall {
                    name: "f".into(),
                    arguments: "{}".into(),
                },
            }],
        };
        assert!(r.has_tool_calls());
    }
}
