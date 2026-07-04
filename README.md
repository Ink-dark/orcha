# Orcha System Specification (v0.1)
## 面向 Sub-Agent 的自动化编码操作系统规范

> **Slogan**: Let the Orcha play.

---

### 1. 系统概览 (System Overview)

Orcha 是一个 **Control Plane (控制面)** 与 **Data Plane (数据面)** 分离的自动化 Coding 系统。

- **Orcha Core**: 大脑，负责运行 Cycleround 工作流。
- **Orcha Shell**: 外壳（Gateway），负责对接 IM（Slack/Discord/Telegram）和 API。
- **Sub-Agents**: 乐手，负责执行具体的 Plan / Code / Test / Review。

```
┌─────────────────────────────────────────────┐
│                Orcha Shell                  │
│  (Slack / API / Webhook / CLI)              │
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
3. **Human-in-the-loop**: 涉及 Git Push / Deploy 的操作必须经过 `Reviewer Agent` 或人工确认。

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
| M6 | Slack Adapter | Phase 2 | — |
| M7 | Plugin 子代理体系 | Phase 3 | — |
| M8 | Self-Evolve | Phase 4 | — |

> 各 Milestone 的目标、交付物、可验证验收清单及 DoD/追踪约定见 **[docs/ROADMAP.md](docs/ROADMAP.md)**。

