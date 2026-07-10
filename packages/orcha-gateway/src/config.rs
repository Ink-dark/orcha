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

    /// 鉴权配置（白名单，M7）。
    #[serde(default)]
    pub auth: AuthConfig,

    /// 人工审批配置（M7 P1，飞书卡片审批）。
    /// 不配此段 = 审批关闭（NullApprovalHook 放行）。
    #[serde(default)]
    pub approval: ApprovalConfig,

    /// 工作区配置（M7 P1，飞书触发任务的默认 repo 路径）。
    /// 不配 = 用空目录（LLM 无 repo 可改，仅适合纯生成任务）。
    #[serde(default)]
    pub workspace: WorkspaceConfig,
}

fn default_home() -> PathBuf {
    PathBuf::from(std::env::var("ORCHA_HOME").unwrap_or_else(|_| ".orcha".to_string()))
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

    /// #26：TCP 后端共享密钥。
    ///
    /// - `None`（默认）：查 `ORCHA_IPC_TCP_SECRET` 环境变量；仍为空则 TCP 不做认证
    ///   （仅 dev/CI 用，Gateway 启动时打警告：任意本机进程可连入伪造触发/审批）。
    /// - `Some(s)`：TCP 连接建立后，客户端必须先发 `AUTH <s>\n`，服务端校验通过才
    ///   进入 JSON-line 协议；不匹配立即断开。
    ///
    /// Unix socket 后端靠文件系统权限 0600 保护（见 #17），不需要此字段。
    #[serde(default)]
    pub tcp_secret: Option<String>,

    /// Smoke 测试端口（纯文本 TCP，一行 = 一个任务）。0 = 禁用。默认 7423。
    /// 仅 `smoke` feature 启用时编译。
    #[cfg(feature = "smoke")]
    #[serde(default = "default_smoke_port")]
    pub smoke_port: u16,
}

impl IpcConfig {
    /// 实际生效的 TCP 密钥：config 字段优先，否则查 `ORCHA_IPC_TCP_SECRET` 环境变量。
    /// 两处都未配置返回 `None`（TCP 不做认证，dev 模式）。
    pub fn effective_tcp_secret(&self) -> Option<String> {
        self.tcp_secret.clone().or_else(|| {
            std::env::var("ORCHA_IPC_TCP_SECRET")
                .ok()
                .filter(|s| !s.is_empty())
        })
    }
}

fn default_ipc_kind() -> String {
    "auto".to_string()
}

fn default_ipc_port() -> u16 {
    7422
}
#[cfg(feature = "smoke")]
fn default_smoke_port() -> u16 {
    7423
}

impl Default for IpcConfig {
    fn default() -> Self {
        Self {
            kind: default_ipc_kind(),
            port: default_ipc_port(),
            tcp_secret: None,
            #[cfg(feature = "smoke")]
            smoke_port: default_smoke_port(),
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
    // AI 驱动模式下每步 = 1 次 LLM 决策 + 1 次 agent 执行，
    // 一个完整的 Plan→Code→Test→Review→exit 链路约需 6-8 步，
    // 含重试和回退需要更多余量。默认 30 步保证复杂任务有足够空间。
    30
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

/// 鉴权配置（白名单，M7）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default)]
    pub whitelist: Vec<crate::auth::WhitelistEntry>,
}

/// 人工审批配置（M7 P1）。
///
/// 细粒度分三类：写文件 / 跑命令 / 删文件。每类独立配白名单，
/// 与 `auth.whitelist` 同结构（`WhitelistEntry`）。
///
/// # 语义
///
/// - 不配 `[approval]` 段 → 审批关闭（`NullApprovalHook` 放行，CI / 默认行为）
/// - 配了 `[approval]` 但某类没配 → 该类操作不需要审批（放行）
/// - 配了 `[approval]` 且某类为空数组 → 该类操作全拒（fail-closed）
/// - 配了 `[approval]` 且某类非空 → 走白名单校验
///
/// # 配置示例
///
/// ```toml
/// [approval]
/// timeout_secs = 300  # 超时秒数（默认 300，超时自动 reject）
///
/// [[approval.write]]        # 可审批写文件的人
/// platform = "feishu"
/// user = "ou_xxx"
///
/// [[approval.command]]      # 可审批跑命令的人
/// platform = "feishu"
/// user = "ou_yyy"
/// ```
///
/// # 白名单校验位置
///
/// 校验在 Gateway 侧（Rust），不在 Adapter 侧（TS）。Adapter 只负责
/// UI 与事件转发：把按钮点击事件通过 IPC 转给 Gateway，Gateway 查
/// 白名单后回最终决策。这样配置统一在 config.toml，TS 侧不重复配置。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalConfig {
    /// 审批超时秒数。Worker 发起审批后等这么久，超时自动 Rejected（fail-closed）。
    /// 默认 300（5 分钟）。
    #[serde(default = "default_approval_timeout")]
    pub timeout_secs: u64,

    /// 可审批「写文件」的白名单。
    /// - None（不配）= 该类不需要审批
    /// - Some(vec![]) = 该类全拒
    /// - Some(非空) = 走白名单
    #[serde(default)]
    pub write: Option<Vec<crate::auth::WhitelistEntry>>,

    /// 可审批「跑命令」的白名单。语义同 `write`。
    #[serde(default)]
    pub command: Option<Vec<crate::auth::WhitelistEntry>>,

    /// 可审批「删文件」的白名单。语义同 `write`。
    #[serde(default)]
    pub delete: Option<Vec<crate::auth::WhitelistEntry>>,
}

fn default_approval_timeout() -> u64 {
    1800
}

/// 工作区配置（M7 P1）。
///
/// 飞书触发任务时的默认 repo 路径。Gateway 会在该 repo 下创建
/// GitWorktree 作为每任务隔离工作区，改动不污染原 repo。
///
/// 不配 `[workspace]` = 用空目录（LLM 无 repo 可改，仅适合纯生成任务）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// 默认 repo 根路径（绝对路径或相对 cwd）。
    /// 飞书触发任务时，Gateway 在此 repo 下 `git worktree add` 创建隔离工作区。
    /// 非绝对路径会相对 Gateway cwd 解析。
    #[serde(default)]
    pub repo: Option<PathBuf>,

    /// 是否启用 worktree 隔离（默认 true，仅当 repo 存在 .git 时生效）。
    /// 设为 false 则直接在原 repo 原地修改（不推荐，并发会冲突）。
    #[serde(default = "default_worktree_enabled")]
    pub worktree: bool,
}

fn default_worktree_enabled() -> bool {
    true
}

impl WorkspaceConfig {
    /// 是否配置了 repo 路径。
    pub fn has_repo(&self) -> bool {
        self.repo
            .as_ref()
            .is_some_and(|p| !p.as_os_str().is_empty())
    }
}

impl ApprovalConfig {
    /// 审批是否启用（任何一类已配白名单即视为启用）。
    pub fn enabled(&self) -> bool {
        self.write.is_some() || self.command.is_some() || self.delete.is_some()
    }

    /// 超时 Duration。
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs)
    }

    /// 按动作类型取出对应白名单。
    /// 返回 `None` 表示该类不需要审批（放行）；
    /// 返回 `Some(auth)` 表示走白名单校验（auth 可能为空 → 全拒）。
    pub fn authenticator_for(
        &self,
        action: &orcha_core::ApprovalAction,
    ) -> Option<crate::auth::Authenticator> {
        let entries = match action {
            orcha_core::ApprovalAction::WriteFile { .. } => self.write.as_ref(),
            orcha_core::ApprovalAction::RunCommand { .. } => self.command.as_ref(),
            orcha_core::ApprovalAction::DeleteFile { .. } => self.delete.as_ref(),
        }?;
        Some(crate::auth::Authenticator::new(entries.clone()))
    }
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
    /// 含 init()：创建 `home/store/` 目录，幂等。
    #[cfg(not(feature = "sqlite"))]
    pub fn build_task_store(&self) -> Result<orcha_core::FileTaskStore> {
        let store = orcha_core::FileTaskStore::new(&self.home);
        store.init().context("init task store")?;
        Ok(store)
    }

    /// 从 `auth.whitelist` 构造 [`crate::auth::Authenticator`]（克隆条目）。
    pub fn authenticator(&self) -> crate::auth::Authenticator {
        crate::auth::Authenticator::new(self.auth.whitelist.clone())
    }

    /// 构造 LLM 客户端（M7，需启用 `llm` feature）。
    ///
    /// 从 `llm.api_key_env` 指定的环境变量读 API key，配合 `llm.base_url` /
    /// `llm.model` 构造 [`orcha_llm::OpenAiCompatibleClient`]。
    ///
    /// - 无 key 或 key 为空 → 返回 `None`：Gateway 仍能启动（CI smoke test 无 key），
    ///   但任务触发后 worker 会因无法调 LLM 失败（在 CardUpdate 里告知用户）。
    /// - 有 key → 返回 `Arc<dyn LlmClient>`，注入 worker 跑 `LlmCycleround`。
    #[cfg(feature = "llm")]
    pub fn build_llm_client(&self) -> Option<std::sync::Arc<dyn orcha_llm::LlmClient>> {
        let key = std::env::var(&self.llm.api_key_env).ok()?;
        if key.trim().is_empty() {
            return None;
        }
        let cfg = orcha_llm::LlmConfig {
            base_url: self.llm.base_url.trim_end_matches('/').to_string(),
            api_key: key,
            model: self.llm.model.clone(),
            timeout: std::time::Duration::from_secs(120),
            temperature: 0.2,
            max_tokens: Some(16384),
            max_retries: 3,
            retry_base_ms: 1000,
        };
        Some(std::sync::Arc::new(orcha_llm::OpenAiCompatibleClient::new(
            cfg,
        )))
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
            auth: AuthConfig::default(),
            approval: ApprovalConfig::default(),
            workspace: WorkspaceConfig::default(),
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
        assert_eq!(cfg.cycle.max_rounds, 30);
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
        // Windows 上 `Path::is_absolute()` 要求盘符前缀（`C:\...`），
        // Unix 风格的 `/var/...` 在 Windows 上不是绝对路径，会被当成相对路径 join 到 home。
        // 因此这里按平台用各自合法的绝对路径，避免 Windows CI 炸。
        let absolute_db = if cfg!(windows) {
            PathBuf::from(r"C:\var\data\orcha.db")
        } else {
            PathBuf::from("/var/data/orcha.db")
        };
        let cfg = GatewayConfig {
            home: PathBuf::from(if cfg!(windows) {
                r"C:\tmp\orcha"
            } else {
                "/tmp/orcha"
            }),
            db_path: absolute_db.clone(),
            ..Default::default()
        };
        assert_eq!(cfg.db_full_path(), absolute_db);
    }

    #[test]
    fn cycle_config_conversion() {
        let cfg = GatewayConfig::default();
        let cc = cfg.cycle_config();
        assert_eq!(cc.max_rounds, 30);
        assert_eq!(cc.max_retries, 3);
        assert_eq!(cc.cool_down, std::time::Duration::from_secs(60));
    }
}
