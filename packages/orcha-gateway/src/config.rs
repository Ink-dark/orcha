//! Gateway 配置与密钥管理（M6）。
//!
//! 统一 `config.toml` 加载 LLM key / SQLite 路径 / 端口 / IM 凭证占位。
//! 密钥不硬编码，从 config.toml 或环境变量读取。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use orcha_core::CycleConfig;
use serde::{Deserialize, Serialize};

/// Gateway 顶层配置（对应 config.toml）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    /// Orcha home 目录（默认 `~/.orcha`）。
    #[serde(default = "default_home")]
    pub home: PathBuf,

    /// 任务队列 worker 数（默认 2）。
    #[serde(default = "default_workers")]
    pub workers: usize,

    /// IPC 监听地址。
    #[serde(default)]
    pub ipc: IpcConfig,

    /// SQLite 数据库路径（相对 home，默认 `orcha.db`）。
    #[serde(default = "default_db_path")]
    pub db_path: PathBuf,

    /// 熔断参数（可 per-task 覆盖，M6 用全局默认）。
    #[serde(default)]
    pub cycle: CycleConfigToml,

    /// LLM 配置（占位，M7 接入时填充）。
    #[serde(default)]
    pub llm: LlmConfig,

    /// IM 凭证占位（M7 接入时填充）。
    #[serde(default)]
    pub im: ImConfig,
}

fn default_home() -> PathBuf {
    PathBuf::from(
        std::env::var("ORCHA_HOME").unwrap_or_else(|_| ".orcha".to_string()),
    )
}

fn default_workers() -> usize {
    2
}

fn default_db_path() -> PathBuf {
    PathBuf::from("orcha.db")
}

/// IPC 配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcConfig {
    /// 传输方式：`unix` / `tcp` / `auto`（默认 auto：Linux/macOS 选 unix，Windows 选 tcp）。
    #[serde(default = "default_ipc_kind")]
    pub kind: String,

    /// TCP 端口（kind=tcp 或 fallback 时使用，默认 7422）。
    #[serde(default = "default_ipc_port")]
    pub port: u16,
}

fn default_ipc_kind() -> String {
    "auto".to_string()
}

fn default_ipc_port() -> u16 {
    7422
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            kind: default_ipc_kind(),
            port: default_ipc_port(),
        }
    }
}

/// 熔断参数的 TOML 表示。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CycleConfigToml {
    #[serde(default = "default_max_rounds")]
    pub max_rounds: u32,
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_cool_down")]
    pub cool_down_secs: u64,
}

fn default_max_rounds() -> u32 {
    10
}
fn default_max_retries() -> u32 {
    3
}
fn default_cool_down() -> u64 {
    60
}

impl Default for CycleConfigToml {
    fn default() -> Self {
        Self {
            max_rounds: default_max_rounds(),
            max_retries: default_max_retries(),
            cool_down_secs: default_cool_down(),
        }
    }
}

/// LLM 配置（占位，从环境变量读取 key）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmConfig {
    /// 环境变量名（值不写进 config.toml，避免泄密）。
    #[serde(default = "default_llm_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_llm_base_url")]
    pub base_url: String,
    #[serde(default = "default_llm_model")]
    pub model: String,
}

fn default_llm_key_env() -> String {
    "ORCHA_LLM_API_KEY".to_string()
}
fn default_llm_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}
fn default_llm_model() -> String {
    "gpt-4o".to_string()
}

/// IM 凭证占位（M7 填充）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImConfig {
    #[serde(default)]
    pub feishu: Option<FeishuConfig>,
}

/// 飞书凭证占位。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeishuConfig {
    pub app_id_env: String,
    pub app_secret_env: String,
}

impl GatewayConfig {
    /// 从 config.toml 文件加载；文件不存在则返回默认配置。
    pub fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .with_context(|| format!("读取配置文件失败: {}", path.display()))?;
            let cfg: Self = toml::from_str(&content)
                .with_context(|| format!("解析 config.toml 失败: {}", path.display()))?;
            Ok(cfg)
        } else {
            Ok(Self::default())
        }
    }

    /// SQLite 数据库完整路径。
    pub fn db_full_path(&self) -> PathBuf {
        if self.db_path.is_absolute() {
            self.db_path.clone()
        } else {
            self.home.join(&self.db_path)
        }
    }

    /// 转为 core 的 CycleConfig。
    pub fn cycle_config(&self) -> CycleConfig {
        CycleConfig {
            max_rounds: self.cycle.max_rounds,
            max_retries: self.cycle.max_retries,
            cool_down: std::time::Duration::from_secs(self.cycle.cool_down_secs),
        }
    }

    /// 构建任务存储（M6 默认 SQLite，feature gate）。
    #[cfg(feature = "sqlite")]
    pub fn build_task_store(&self) -> Result<orcha_core::SqliteTaskStore> {
        orcha_core::SqliteTaskStore::open(&self.db_full_path())
    }

    /// 构建任务存储（非 sqlite feature 时回退 FileTaskStore）。
    #[cfg(not(feature = "sqlite"))]
    pub fn build_task_store(&self) -> Result<orcha_core::FileTaskStore> {
        Ok(orcha_core::FileTaskStore::new(&self.home))
    }
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            home: default_home(),
            workers: default_workers(),
            ipc: IpcConfig::default(),
            db_path: default_db_path(),
            cycle: CycleConfigToml::default(),
            llm: LlmConfig::default(),
            im: ImConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sensible_values() {
        let cfg = GatewayConfig::default();
        assert_eq!(cfg.workers, 2);
        assert_eq!(cfg.cycle.max_rounds, 10);
        assert_eq!(cfg.cycle.max_retries, 3);
        assert_eq!(cfg.cycle.cool_down_secs, 60);
    }

    #[test]
    fn db_full_path_joins_home() {
        let cfg = GatewayConfig {
            home: PathBuf::from("/tmp/orcha"),
            db_path: PathBuf::from("orcha.db"),
            ..Default::default()
        };
        assert_eq!(cfg.db_full_path(), PathBuf::from("/tmp/orcha/orcha.db"));
    }

    #[test]
    fn db_full_path_absolute_preserved() {
        let cfg = GatewayConfig {
            home: PathBuf::from("/tmp/orcha"),
            db_path: PathBuf::from("/var/data/orcha.db"),
            ..Default::default()
        };
        assert_eq!(cfg.db_full_path(), PathBuf::from("/var/data/orcha.db"));
    }

    #[test]
    fn cycle_config_conversion() {
        let cfg = GatewayConfig::default();
        let cc = cfg.cycle_config();
        assert_eq!(cc.max_rounds, 10);
        assert_eq!(cc.max_retries, 3);
        assert_eq!(cc.cool_down, std::time::Duration::from_secs(60));
    }
}
