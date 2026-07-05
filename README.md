# Orcha System Specification (v0.1)
## 面向 Sub-Agent 的自动化编码操作系统规范

> **Slogan**: Let the Orcha play.

---

### 1. 系统概览 (System Overview)

Orcha 是一个**基于命令行的自动化编码操作系统**，采用 **Control Plane (控制面)** 与 **Data Plane (数据面)** 分离架构，通过 Shell 网关接入飞书 / Slack 等 IM 机器人。用户在聊天窗口一句话发起复杂编码任务，系统自动拆解、执行、验证、循环优化，直到真正完成。

**目标用户**：中小团队的技术负责人、独立开发者、开源项目维护者——需要频繁处理"小而杂"的编码任务，但不想在工具切换和重复执行上浪费时间的人。

- **Orcha Core**: 大脑，负责运行 Cycleround 工作流。
- **Orcha Shell**: 外壳（Gateway），负责对接 IM（飞书 / Slack 等）和 API。
- **Sub-Agents**: 乐手，负责执行具体的 Plan / Code / Test / Review。

```
┌─────────────────────────────────────────────┐
│                Orcha Shell                  │
│  (飞书 / Slack / API / Webhook / CLI)       │
└─────────────────────┬───────────────────────┘
                      │ Dispatch
┌─────────────────────▼───────────────────────┐
│               Orcha Core                    │
│  ┌─────────────────────────────────────┐    │
│  │          Cycleround Loop             │    │
│  │  Plan → Code → Test → Review → Fix  │    │
│  └─────────────────────────────────────┘    │
│                     │                       │
│            ┌────────▼────────┐              │
│            │   Orcha State   │              │
│            │ (Artifacts/Logs)│              │
│            └─────────────────┘              │
└─────────────────────────────────────────────┘
```

---

### 2. 核心概念 (Core Concepts)

#### 2.1 Task (任务)
系统的最小执行单元。
- **ID**: `T-{uuid}`
- **Status**: `PENDING | RUNNING | BLOCKED | DONE | FAILED`

#### 2.2 Cycleround (循环工作流)
固定的执行范式，直到任务成功或达到最大轮次。
```
LOOP:
  1. Observe Context
  2. Plan Steps
  3. Execute Sub-Agent
  4. Validate (Test/Review)
  5. If Fail -> Fixer Agent -> Goto 3
  6. If Success -> Exit Loop
```

#### 2.3 Sub-Agent (子代理)
遵循 **OPC (Observer-Planner-Coder)** 模式的独立执行单元。
- **Observer**: 读取 Repo / Logs / Context。
- **Planner**: 生成 DAG 执行计划。
- **Worker (Coder/Tester/Reviewer)**: 执行具体操作。

---

### 3. Orcha Shell 网关规范 (Gateway Spec)

为了让机器人接入更省事，Shell 必须标准化输入与输出。

#### 3.1 事件模型 (Event Model)
所有进入 Orcha 的消息必须转换为 `OrchaEvent`。

```json
{
  "event_id": "EVT-xxxx",
  "source": "slack",
  "user_id": "U123",
  "timestamp": 1700000000,
  "type": "USER_PROMPT",
  "payload": {
    "raw_text": "@Orcha fix the bug in auth.py",
    "attachments": []
  }
}
```

#### 3.2 响应模型 (Response Model)
```json
{
  "event_id": "EVT-xxxx",
  "status": "STREAMING | FINAL | ERROR",
  "content": "正在修复 auth.py...",
  "artifacts": [
    { "type": "diff", "url": "..." }
  ]
}
```

#### 3.3 Adapter 接口 (伪代码)
所有机器人只需实现此接口：

```python
class ShellAdapter:
    def normalize_incoming(self, raw_event) -> OrchaEvent:
        """将外部消息转为 OrchaEvent"""
        pass

    def dispatch_outgoing(self, response: OrchaResponse):
        """将 Orcha 结果发回给用户"""
        pass
```

---

### 4. Orcha Core 调度规范 (Core Spec)

#### 4.1 状态机 (State Machine)
```
PENDING -> RUNNING -> (BLOCKED <-> RUNNING) -> DONE
                       |
                       └-> FAILED
```

#### 4.2 资源隔离
- **Workspace**: 每个 Task 拥有独立的 `/tmp/orcha/{task_id}`。
- **Context Window**: 限制每个 Sub-Agent 的最大 Token 消耗。

#### 4.3 熔断机制 (Circuit Breaker)
为了防止死循环或 API 耗尽：
- Max Rounds per Task: `10`
- Max Retries per Step: `3`
- Cool-down on Rate Limit: `60s`

---

### 5. 数据规范 (Data Spec)

#### 5.1 Orcha State (原 Orch-data)
存储位置：`Redis / Postgres`

| Key | Type | Description |
| :--- | :--- | :--- |
| `task:{id}:state` | JSON | 当前任务状态 |
| `task:{id}:history` | List | 执行日志 |
| `task:{id}:artifacts` | List | 生成的代码/Diff |

#### 5.2 Artifact 格式
```json
{
  "artifact_id": "ART-001",
  "type": "CODE_DIFF",
  "commit_sha": "...",
  "patch": "diff --git a/..."
}
```

---

### 6. 安全与权限 (Security)

1. **Secrets Management**: 永远不在 Prompt 中泄露 API Keys，通过 Env 注入。
2. **Sandbox**: Sub-Agent 的代码执行必须在 Docker 容器内。
3. **Human-in-the-loop**: 涉及 Git Push / Deploy 的操作必须经过 `Reviewer Agent` 或人工确认。疑似卡住时即时通知用户，用户可随时终止任意 Agent。
4. **原子性写入 + 声明式状态管理**: 避免多 Sub-Agent 并发写入冲突，状态变更以声明式提交，杜绝半写。
5. **Minicommit**: 每步修改独立提交，保证每步都可回退；任一环节出错可精确回滚到上一个 Minicommit。
6. **熔断机制**: 最多 10 轮 / 每步 3 次重试，防止资源耗尽或死循环（详见 §4.3）。

---

### 7. CLI 命令集 (Interface)

```bash
# 系统管理
orcha init <repo>
orcha status [task_id]

# 任务触发
orcha run "Implement user login"
orcha fix <issue_url>

# Shell 管理
orcha shell add slack --token xxx
orcha shell list
orcha shell serve [--port 7421]   # 启动 Web UI（D3）
```

---

### 8. 开发路线图 (Roadmap)

采用 **M0 → M8** 共 9 个 Milestone 推进，每个 Milestone 含可执行、可观测的验收清单。**MVP 完成定义** = M0 → M3 全部验收通过（对应 Phase 1）。

| Milestone | 名称 | Phase | MVP |
| :--- | :--- | :--- | :---: |
| M0 | 骨架与契约 | Phase 1 | ✅ |
| M1 | Task 模型与本地 CLI | Phase 1 | ✅ |
| M2 | 单步 Sub-Agent 执行 | Phase 1 | ✅ |
| M3 | Cycleround 闭环 | Phase 1 | ✅ |
| M4 | 状态持久化 | Phase 2 | — |
| M5 | Gateway Shell + HTTP/CLI 触发 | Phase 2 | — |
| M6 | 飞书 Adapter | Phase 2 | — |
| M7 | Plugin 子代理体系 | Phase 3 | — |
| M8 | Self-Evolve | Phase 4 | — |

> 各 Milestone 的目标、交付物、可验证验收清单及 DoD/追踪约定见 **[docs/ROADMAP.md](docs/ROADMAP.md)**。

---

### 9. Quick Start（D3 Web UI demo）

**前置依赖**：Rust 1.75+、Bash、Python 3（用于 Cycleround 确定性 Tester 子代理）。

**一键跑起来**：

```bash
./scripts/web-demo.sh
# 或指定端口 / release 构建：
./scripts/web-demo.sh --port 8000 --release
```

脚本会自动：
1. `cargo build --bin orcha`（首次约 1~2 分钟）
2. 在临时 home 跑两遍 `orcha fix` 确定性闭环（一个单轮成功、一个 Fixer 第 2 轮修复），种出 2 个 DONE task + history + artifacts
3. 启动 `orcha shell` Web UI 服务器（默认端口 7421）

浏览器打开 `http://127.0.0.1:7421/` 即可看到任务列表 → 任务详情 → history 时间线 / LLM memory 标签页。Ctrl-C 退出后自动清理临时目录。

**LLM 路径（可选，需 OpenAI 兼容 API Key）**：

```bash
export ORCHA_LLM_BASE_URL=https://api.openai.com/v1
export ORCHA_LLM_API_KEY=sk-...
export ORCHA_LLM_MODEL=gpt-4o-mini
./scripts/web-demo.sh --llm
```

`--llm` 会用 `--features orcha-cli/llm` 重编译，再追加一个 `orcha fix --llm` 任务；该任务的 **Memory 标签页**会展示 D2 注入 LLM prompt 的对话历史（per-agent / per-round）。

**手动跑（不用 demo 脚本）**：

```bash
cargo build --bin orcha
./target/debug/orcha init                 # 初始化 ./orcha home
./target/debug/orcha fix --workspace /tmp/ws \
    "创建 hello.py 输出 hello"             # 跑一遍 Cycleround
./target/debug/orcha shell --port 7421    # 启动 Web UI
```

**JSON API（供脚本 / 外部集成消费）**：

| 端点 | 返回 |
| :--- | :--- |
| `GET /api/tasks` | `[{ id, description, status, created_at }]` |
| `GET /api/tasks/summary` | `{ total, pending, running, blocked, done, failed }` |
| `GET /api/tasks/:id` | `{ task, history_count, artifacts }` |
| `GET /api/tasks/:id/history` | `Vec<RoundRecord>`（每轮 steps / artifacts / tokens） |
| `GET /api/tasks/:id/memory` | `Vec<MemoryEntry>`（D2 LLM 对话历史，仅 `--llm` 路径有数据） |

