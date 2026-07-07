# Orcha

> Let the Orcha play.

在飞书 @Orcha 发一句话任务，AI 自主拆解、执行、测试、审核，把改动推到 Git 新分支。

## 它能做什么

```
你 @Orcha：在 utils/mod.rs 末尾追加 reverse_string 函数

Orcha：
  1. 鉴权入队
  2. AI 自主调度 6 个 Sub-Agent（Observer→Planner→Worker→Tester→Reviewer→Exit）
  3. Worker 写文件前推审批卡片 → 你点 [批准]
  4. Tester 跑 cargo test 验证
  5. 改动 commit 并推到 orcha/add-reverse-string 分支
  6. 飞书卡片回报：分支名 + commit hash + commit message
```

## 核心能力

- **飞书触发** — 长连接接入飞书，@机器人一句话发起任务，无需公网 URL
- **AI 自主调度** — LLM 决定每一步调谁、何时结束、失败怎么重试（最多 10 轮）
- **GitWorktree 隔离** — 每个任务在独立 worktree 改代码，原 repo 不被污染
- **人工审批** — 写文件 / 跑命令 / 删文件前推飞书审批卡片，按钮回调
- **自动落分支** — 任务成功后自动 commit 并推到 `orcha/*` 新分支
- **熔断保护** — 最多 10 轮 + 每步 3 次重试，防止死循环

## 架构

```
飞书 IM ──→ feishu-adapter ──IPC──→ orcha-gateway ──→ orcha-core
                                  (鉴权/队列/审批)    (AiDrivenCycleround)
                                                              │
                                                              ▼
                                                    GitWorktree 隔离工作区
                                                    改动 commit → orcha/* 分支
```

## 仓库结构

```
packages/
├── orcha-sdk/             数据模型（Task / Artifact / Event / Step）
├── orcha-core/            大脑：Cycleround 调度 + Sub-Agent + GitWorktree
├── orcha-llm/             LLM 客户端（OpenAI 兼容，含 tool calling）
├── orcha-gateway/         Gateway：IPC server + 鉴权 + 任务队列 + 审批
├── orcha-shell/           Web UI + HTTP API（可视化面板）
├── orcha-cli/             CLI 入口（init / fix / shell / history）
└── orcha-feishu-adapter/  飞书 Adapter（TypeScript，长连接 + IPC）
scripts/                   start.ps1 / stop.ps1 / dev-env.ps1.example
docs/                      ROADMAP.md / SPEC.md / ARCHITECTURE_ANALYSIS.md
```

## 快速开始

### 前置依赖

- Rust 1.75+
- Node.js 20+
- Git
- PowerShell 5.1+（Linux/macOS 参考 [docs/LOCAL_RUN.md](docs/LOCAL_RUN.md)）

### 1. 配置

```powershell
copy scripts\dev-env.ps1.example scripts\dev-env.ps1
notepad scripts\dev-env.ps1
```

填入 LLM API key 和飞书凭证：
```powershell
$env:ORCHA_LLM_API_KEY = "sk-..."
$env:ORCHA_FEISHU_APP_ID = "cli_xxx"
$env:ORCHA_FEISHU_APP_SECRET = "..."
$env:ORCHA_ADAPTER_MOCK = "0"   # 0=真实飞书
```

### 2. 飞书应用

1. 飞书开放平台 → 创建企业自建应用
2. 事件订阅 → 选「使用长连接接收事件」（无需公网 URL）
3. 订阅事件：`im.message.receive_v1` + `card.action.trigger`
4. 权限：`im:message` / `im:message:send_as_bot` / `im:chat:readonly`
5. 应用发布并加到群聊

### 3. 配置 config.toml

编辑 `.orcha/config.toml`，关键项：
```toml
[llm]
api_key_env = "ORCHA_LLM_API_KEY"
base_url = "https://api.deepseek.com/v1"
model = "deepseek-chat"

[[auth.whitelist]]
platform = "feishu"
group = "*"             # 开发调试用通配，生产改具体 chat_id

[workspace]
repo = "D:/YourRepo"    # 飞书触发任务的默认 repo
worktree = true

[approval]
timeout_secs = 1800     # 30 分钟审批超时
```

完整配置参考 [docs/SPEC.md](docs/SPEC.md)。

### 4. 启动

```powershell
.\scripts\start.ps1
```

### 5. 发任务

飞书群或私聊 @Orcha：
```
@Orcha 在 utils/mod.rs 末尾追加 reverse_string 函数
```

任务完成后飞书卡片显示：
```
✅ 完成
共 7 轮，分支: orcha/add-reverse-string (ed4fc73)

feat(utils): add reverse_string function
```

### 6. 查看结果

```powershell
git -C D:/YourRepo branch --list 'orcha/*'
git -C D:/YourRepo show orcha/add-reverse-string
```

## 运维

```powershell
# 实时日志
Get-Content D:\orcha\logs\gateway.log.err -Wait -Tail 20 -Encoding UTF8
Get-Content D:\orcha\logs\adapter.log -Wait -Tail 20 -Encoding UTF8

# 停止 / 重启
.\scripts\stop.ps1
.\scripts\start.ps1 -Restart
```

## 开发

```powershell
cargo fmt --all
cargo test                              # 全 workspace
cargo clippy --all-targets -- -D warnings
```

## 文档

- [docs/SPEC.md](docs/SPEC.md) — 系统规范（v0.1）
- [docs/ROADMAP.md](docs/ROADMAP.md) — M0→M8 开发路线图
- [docs/ARCHITECTURE_ANALYSIS.md](docs/ARCHITECTURE_ANALYSIS.md) — 架构分析
- [docs/DEPLOYMENT_TOPOLOGY.md](docs/DEPLOYMENT_TOPOLOGY.md) — 部署拓扑
- [docs/LOCAL_RUN.md](docs/LOCAL_RUN.md) — 本地运行指南

## 里程碑

M0-M7 已完成，M8 Self-Evolve 进行中。详见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## License

MIT
