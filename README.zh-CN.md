# Orcha

> Let the Orcha play.

[English](README.md) · **简体中文**

Orcha 是一个由 AI 驱动、经飞书 IM 触发的编码 Agent：在飞书 @ 机器人发一句话任务，
它自主拆解、规划、写码、测试、审核，把改动推到 `orcha/*` 新分支——无需公网 URL，
无需手动切换工具。

---

## 它能做什么

```
你 @Orcha：在 utils/mod.rs 末尾追加 reverse_string 函数

Orcha：
  1. 鉴权入队
  2. AI 自主调度 6 个 Sub-Agent（Observer → Planner → Worker → Tester → Reviewer → Exit）
  3. Worker 写文件前推审批卡片 → 你点 [批准]
  4. Tester 跑 cargo test 验证
  5. 改动 commit 并推到 orcha/add-reverse-string 分支
  6. 飞书卡片回报：分支名 + commit hash + commit message
```

## 核心能力

- **飞书触发** — 长连接接入飞书，@ 机器人一句话发起任务，无需公网 URL。
- **AI 驱动调度** — LLM 在每一步决定下一步调哪个 Sub-Agent、何时结束、失败怎么
  重试（最多 10 轮）。
- **GitWorktree 隔离** — 每个任务在独立 worktree 改代码，原 repo 不被污染。
- **人工审批** — 写文件 / 跑命令 / 删文件前推飞书审批卡片，按钮回调。
- **自动落分支** — 任务成功后自动 commit 并推到 `orcha/*` 新分支。
- **熔断保护** — 最多 10 轮 + 每步 3 次重试，防止死循环。

---

## 架构 — AiDriven + 7 层

Orcha 是一个多进程协作的 ensemble。**AI 驱动调度层**（Layer 4）是大脑：LLM 在
每一步选择下一个 Sub-Agent，而非走硬编码流水线。下面 7 层从 IM 边缘到持久化存储
逐层隔离关注点。

```
┌──────────────────────────────────────────────────────────────────────────┐
│ L1  IM 平台层                飞书 / QQ（官方 SDK 长连接）                  │
│     ── 用户 @ 机器人，事件经 WebSocket 流入 ───────────────────────────── │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ WS 事件（im.message.receive_v1、
                                   │   card.action.trigger）
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L2  Adapter 层               orcha-feishu-adapter（Node.js / TS 进程）    │
│     ── fork 自 larksuite/openclaw-lark；协议翻译 + 自动重连               │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ IPC（Unix Socket / Named Pipe / TCP，
                                   │   JSON line + 30s 心跳）
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L3  Gateway 层               orcha-gateway（Rust 进程，一等公民）         │
│     鉴权 · 白名单 · 任务队列 · worker 池 · 审批编排                       │
│     watchdog：35s 读超时，连续 3 次未心跳 → 断开                          │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ Trigger + RoundEvent 事件流
                                   │   (mpsc::Receiver<RoundEvent>)
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L4  AI 驱动调度层            AiDrivenCycleround  ◀── 大脑                 │
│     ┌──────────────────────────────────────────────────────────────────┐ │
│     │  调度 LLM 看到 task + 全部前序 StepResult 摘要                     │ │
│     │  → 通过 `decide_next_agent` 工具调用决定下一步调谁                 │ │
│     │  → 调 SubAgent.run() → 把结果回喂 → 再决策                         │ │
│     │  → QA 通过后产出 `exit` 结束调度                                  │ │
│     │  熔断：max_rounds（总决策步数）                                   │ │
│     │        max_retries（同一 agent 连续失败次数）                     │ │
│     └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ 调用其中一个
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L5  Sub-Agent 层             Observer · Planner · Worker · Tester ·      │
│                              Reviewer · Fixer · Exit                     │
│     每个实现 SubAgent { name, run(&StepContext) -> StepOutput }          │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ 所有 fs / exec 都经过
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L6  沙箱与工具层             GitWorktree · FsSandbox · PathGuard ·        │
│                              ApprovalHook · Audit 日志                   │
│     PathGuard：canonicalize + 前缀校验，拒 .git/.env/*.key，              │
│                 读 1MB 上限，二进制检测                                  │
│     ApprovalHook：WriteFile / DeleteFile / RunCommand → 飞书卡片         │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ 持久化
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L7  存储与历史层             SQLite (WAL) · History JSONL · Memory JSONL │
│                              TaskStore · HistoryStore · MemoryStore      │
│     Gateway 独占写，Shell 只读连接                                       │
└──────────────────────────────────────────────────────────────────────────┘
```

### 为什么 AI 驱动（而非固定流水线）

早期 `Cycleround` 硬编码 `Observer → Planner → Worker → Tester → Reviewer →
Fixer` 顺序。`AiDrivenCycleround`（M7，feature gate）把调度权交给 LLM：

```rust
loop {
    let decision = ai_decide_next_step(&task, &prior_steps); // { agent, reason }
    match decision.agent.as_str() {
        "observer" | "planner" | "worker" | "tester" |
        "reviewer" | "fixer" => agent.run(&ctx),
        "exit" => break,                                    // QA 通过
        _ => /* 未知 */,
    }
    if circuit_breaker_tripped() { break; }
}
```

由于调度 LLM 每次迭代都能看到**全部前序步骤摘要**，自然不会重蹈覆辙
（解决"Reviewer 无跨轮记忆"的 gap——AI 无法靠换种写法蒙混过同一拒绝理由）。

### 各层职责一览

| 层 | Crate / 进程 | 负责的事 |
| :--- | :--- | :--- |
| L1 IM 平台 | （外部）飞书 / QQ | 用户侧消息收发 |
| L2 Adapter | `orcha-feishu-adapter` (TS) | WS 重连、事件解析、IPC 客户端 |
| L3 Gateway | `orcha-gateway` (Rust) | 鉴权、队列、worker 池、审批、IPC 服务端 |
| L4 AI 调度 | `orcha-core` `AiDrivenCycleround` | 每步 LLM 决策、熔断、事件流 |
| L5 Sub-Agent | `orcha-core` `sub_agents` / `llm_agents` | Observer/Planner/Worker/Tester/Reviewer/Fixer |
| L6 沙箱与工具 | `orcha-core` `sandbox` / `path_guard` / `approval` | worktree、路径安全、人工审批、审计 |
| L7 存储 | `orcha-core` `store` / `history` / `memory` | SQLite + JSONL 持久化 |

---

## 仓库结构

```
packages/
├── orcha-sdk/             数据模型（Task / Artifact / Event / Step）
├── orcha-core/            大脑：Cycleround + Sub-Agent + GitWorktree（L4–L7）
├── orcha-llm/             LLM 客户端（OpenAI 兼容，含 tool calling）
├── orcha-gateway/         Gateway：IPC server + 鉴权 + 任务队列 + 审批（L3）
├── orcha-shell/           Web UI + HTTP API（可视化面板，只读）
├── orcha-cli/             CLI 入口（init / fix / shell / history）
└── orcha-feishu-adapter/  飞书 Adapter（TypeScript，长连接 + IPC）（L2）
scripts/                   start.ps1 / stop.ps1 / dev-env.ps1.example
docs/                      ROADMAP.md / SPEC.md / ARCHITECTURE_ANALYSIS.md
```

---

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
$env:ORCHA_LLM_API_KEY       = "sk-..."
$env:ORCHA_FEISHU_APP_ID     = "cli_xxx"
$env:ORCHA_FEISHU_APP_SECRET = "..."
$env:ORCHA_ADAPTER_MOCK      = "0"   # 0=真实飞书
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
base_url    = "https://api.deepseek.com/v1"
model       = "deepseek-chat"

[[auth.whitelist]]
platform = "feishu"
group    = "*"             # 开发调试用通配，生产改具体 chat_id

[workspace]
repo     = "D:/YourRepo"   # 飞书触发任务的默认 repo
worktree = true

[approval]
timeout_secs = 1800        # 30 分钟审批超时
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

---

## 运维

```powershell
# 实时日志
Get-Content D:\orcha\logs\gateway.log.err -Wait -Tail 20 -Encoding UTF8
Get-Content D:\orcha\logs\adapter.log     -Wait -Tail 20 -Encoding UTF8

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

Feature flag：

- **`llm`** — `orcha-core` 可选开启，`orcha-gateway` 默认开启。启用
  `LlmPlanner` / `LlmWorker` / `LlmReviewer` / `AiDrivenCycleround`。不开启时
  所有 agent 都是确定性实现（无网络调用），CI 友好。
- **`sqlite`** — 可选开启。把 `FileTaskStore` 换成 `SqliteTaskStore`
  （rusqlite + bundled SQLite，WAL 模式 + 跨进程锁）。

## 文档

- [docs/SPEC.md](docs/SPEC.md) — 系统规范（v0.1）
- [docs/ROADMAP.md](docs/ROADMAP.md) — M0 → M9 开发路线图
- [docs/ARCHITECTURE_ANALYSIS.md](docs/ARCHITECTURE_ANALYSIS.md) — 架构分析
- [docs/DEPLOYMENT_TOPOLOGY.md](docs/DEPLOYMENT_TOPOLOGY.md) — 部署拓扑
- [docs/LOCAL_RUN.md](docs/LOCAL_RUN.md) — 本地运行指南

## 里程碑

M0–M7 已完成，M8 Plugin 子代理体系进行中。详见 [docs/ROADMAP.md](docs/ROADMAP.md)。

## License

MIT
