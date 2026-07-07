//! `orcha-llm`：LLM 客户端抽象与 OpenAI 兼容端点实现。
//!
//! D1 核心：为 Cycleround 提供一个**同步阻塞**的 LLM 调用入口，
//! 覆盖所有 OpenAI 兼容服务（OpenAI / DeepSeek / Ollama / vLLM 等）。
//! 不引入异步运行时；ureq 足够。
//!
//! ## 配置（环境变量）
//! - `ORCHA_LLM_BASE_URL`：API 基址，默认 `https://api.openai.com/v1`
//! - `ORCHA_LLM_API_KEY`：API Key，必填
//! - `ORCHA_LLM_MODEL`：模型名，默认 `gpt-4o-mini`
//!
//! 调用 [`LlmConfig::from_env`] 读取，[`OpenAiCompatibleClient::new`] 构造。

pub mod client;
pub mod prompt;

pub use client::{
    ChatMessage, ChatResponse, FunctionCall, FunctionDef, LlmClient, LlmConfig, LlmError,
    OpenAiCompatibleClient, ToolCallRequest, ToolDefinition,
};
pub use prompt::{
    build_planner_prompt, build_reviewer_prompt, build_worker_prompt, parse_planner_output,
    parse_reviewer_output, parse_worker_output, parse_worker_output_with_steps, WorkerOutput,
    WorkerStep, PLANNER_SYSTEM, REVIEWER_SYSTEM, WORKER_SYSTEM,
};
