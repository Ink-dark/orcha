//! LLM 客户端 trait 与 OpenAI 兼容端点实现。
//!
//! 覆盖 OpenAI / DeepSeek / Ollama `/v1/chat/completions` / vLLM 等。
//! 同步阻塞，用 `ureq`；不引入 tokio。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 聊天消息（OpenAI Chat 格式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// `system` / `user` / `assistant`
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
        }
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
}

impl LlmClient for OpenAiCompatibleClient {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError> {
        let url = format!("{}/chat/completions", self.config.base_url);
        let body = serde_json::json!({
            "model": self.config.model,
            "messages": messages,
            "temperature": self.config.temperature,
            "stream": false,
        });
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
        let content = parsed
            .choices
            .first()
            .map(|c| c.message.content.clone())
            .ok_or_else(|| LlmError::Parse("响应无 choices 或 content".into()))?;
        Ok(content.trim().to_string())
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
    content: String,
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

    /// 所有改 `ORCHA_LLM_*` 环境变量的测试必须先拿这把锁，
    /// 避免并行执行时互相踩 env（cargo 默认多线程跑测试）。
    /// 用 OnceLock 懒初始化避免 static 顺序问题。
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    fn env_lock() -> &'static Mutex<()> {
        ENV_LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn config_from_env_fails_without_api_key() {
        let _guard = env_lock().lock().unwrap();
        // 临时清空 env 验证报错。
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
        // trim_end_matches 去掉末尾 '/'
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
}
