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

### 8. 开发路线图与可验证 Milestone (Roadmap & Verifiable Milestones)

> 原则：每个 Milestone 必须有**可执行、可观测**的验收命令或产物。未通过验收不得进入下一个 Milestone。
> **MVP 完成定义** = M0 → M3 全部验收通过（对应 Phase 1）。

#### 8.1 长线规划总览

| Milestone | 名称 | Phase | 核心可验证产物 |
| :--- | :--- | :--- | :--- |
| **M0** | 骨架与契约 | Phase 1 (MVP) | 仓库结构 + 数据模型 + CI |
| **M1** | Task 模型与本地 CLI | Phase 1 (MVP) | `orcha status` 可查询 |
| **M2** | 单步 Sub-Agent 执行 | Phase 1 (MVP) | 单任务跑通并产出 diff |
| **M3** | Cycleround 闭环 | Phase 1 (MVP) | 闭环修复样例 bug |
| **M4** | 状态持久化 | Phase 2 | 重启后任务可恢复 |
| **M5** | Gateway Shell + HTTP/CLI 触发 | Phase 2 | HTTP API 可触发任务 |
| **M6** | Slack Adapter | Phase 2 | Slack @Orcha 触发并回传 |
| **M7** | Plugin 子代理体系 | Phase 3 | 第三方可注册 Sub-Agent |
| **M8** | Self-Evolve | Phase 4 | Orcha 提交自身调度 PR |

---

#### 8.2 M0 — 骨架与契约 (Foundation)
**目标**：建立可运行工程骨架与统一数据契约，所有后续模块在此之上生长。

**交付物**：
- Monorepo 结构：`packages/orcha-core`、`packages/orcha-shell`、`packages/orcha-cli`、`packages/orcha-sdk`
- 核心数据模型（Pydantic）：`Task`、`OrchaEvent`、`OrchaResponse`、`Artifact`、`Step`、`StepResult`
- 测试框架（pytest）+ CI（lint + test）

**验收（可验证）**：
- [ ] `orcha --version` 打印版本号，退出码 `0`
- [ ] `pytest -q` 全绿，覆盖率 ≥ 60%（`pytest --cov`）
- [ ] CI 在 PR 上自动运行 lint + test（`.github/workflows/ci.yml` 存在且为 green）
- [ ] `orcha schema --export > schema.json` 输出全部模型的 JSON Schema
- [ ] `pre-commit run --all-files` 通过

---

#### 8.3 M1 — Task 模型与本地 CLI
**目标**：本地可创建、查询、流转任务状态机；不引入外部依赖（先用 SQLite/文件）。

**交付物**：
- Task 仓储（SQLite 实现，预留 Redis/PG 接口）
- 状态机：`PENDING → RUNNING → (BLOCKED ↔ RUNNING) → DONE | FAILED`
- CLI：`orcha init`、`orcha run "<desc>"`、`orcha status [id]`、`orcha list`

**验收（可验证）**：
- [ ] `orcha run "hello world"` 返回 `T-` 开头的 task_id
- [ ] `orcha status <id>` 输出 JSON，`status` 字段 ∈ 合法枚举
- [ ] 非法状态迁移抛出 `InvalidTransition`，且有单元测试覆盖
- [ ] `orcha list` 列出全部任务，支持 `--status` 过滤
- [ ] 重启进程后 `orcha list` 仍可查到历史任务（持久化生效）

---

#### 8.4 M2 — 单步 Sub-Agent 执行
**目标**：在沙箱内跑通 `观察 → 规划 → 执行` 最小链路（**单步，不闭环**）。

**交付物**：
- `SubAgent` 接口：`run(task, context) -> StepResult`
- 沙箱：最小 Docker 镜像内执行代码
- 三个最小实现：`Observer`、`Planner`、`Worker`
- 产物落盘到 `/tmp/orcha/{task_id}/`

**验收（可验证）**：
- [ ] 输入 "在 repo 中创建 hello.py 输出 hello"，真实产出文件 `hello.py`
- [ ] 产出被记录为 `Artifact(type=CODE_DIFF)`，含可应用的 patch
- [ ] `git apply` 该 patch 成功
- [ ] 每步写入结构化日志到 `task:{id}:history`
- [ ] 断网状态下沙箱仍可执行基础文件操作

---

#### 8.5 M3 — Cycleround 闭环 (MVP 完成)
**目标**：打通 `Plan → Code → Test → Review → Fix` 循环，**MVP 达成**。

**交付物**：
- Cycleround 调度器，含熔断参数（`max_rounds=10`、`max_retries=3`、`cool_down=60s`）
- `Tester Agent`（跑测试）+ `Reviewer Agent`（静态检查 / diff 审核）
- `Fixer Agent`：失败后回到执行环节
- 端到端 golden tasks 集合（≥ 3 个）

**验收（可验证）**：
- [ ] 给定含已知 bug 的样例 repo，`orcha fix` 闭环修复且测试通过
- [ ] 触达 `max_rounds` 时任务转 `FAILED`，**不死循环**
- [ ] 每一轮 round 在 history 中可追溯（含耗时、token、产物引用）
- [ ] 3 个 golden tasks 全部通过（`make e2e` 或 `pytest -m e2e`）
- [ ] `./scripts/mvp-demo.sh` 一键复现完整闭环

---

#### 8.6 M4 — 状态持久化
**目标**：任务跨进程 / 重启可恢复，支持多实例。

**交付物**：Redis/Postgres 后端抽象 + 迁移脚本

**验收（可验证）**：
- [ ] kill 进程后重启，`RUNNING` 中断的 Task 自动恢复并继续
- [ ] `task:{id}:state` / `:history` / `:artifacts` 三类键在 Redis/PG 中可查
- [ ] 并发写入无冲突（带乐观锁/版本号）

---

#### 8.7 M5 — Gateway Shell + HTTP/CLI 触发
**目标**：标准化外部入口，与 IM 解耦。

**交付物**：`orcha-shell` HTTP server + `ShellAdapter` 接口 + 鉴权中间件

**验收（可验证）**：
- [ ] `POST /run`（携带 API Key）接收 `OrchaEvent`，返回 `event_id`
- [ ] `GET /status/{event_id}` 返回 `OrchaResponse`（`STREAMING | FINAL | ERROR`）
- [ ] 无 API Key 请求返回 `401`
- [ ] 至少一个 Adapter（CLI 适配器）端到端跑通
- [ ] `orcha shell list` 列出已注册 Adapter

---

#### 8.8 M6 — Slack Adapter
**目标**：真实 IM 接入验证 Shell 抽象。

**验收（可验证）**：
- [ ] Slack 频道 `@Orcha fix <issue>` 触发任务
- [ ] 流式进度与最终响应回传到原频道
- [ ] 产物 / Artifact 链接在 Slack 中可点击展开
- [ ] 私有频道权限校验生效

---

#### 8.9 M7 — Plugin 子代理体系
**目标**：第三方可注册自定义 Sub-Agent。

**验收（可验证）**：
- [ ] `orcha plugin add <path>` 注册成功并出现在 `orcha plugin list`
- [ ] Plugin 实现 `SubAgent` 接口即可被 Cycleround 调度
- [ ] Plugin 在沙箱内运行，无越权访问宿主文件系统
- [ ] Plugin 崩溃不影响 Core 稳定性（隔离验证）

---

#### 8.10 M8 — Self-Evolve
**目标**：Orcha 可在受控前提下修改自身调度代码。

**验收（可验证）**：
- [ ] 给定 "优化 Cycleround 熔断策略" 任务，Orcha 产出可合入的 PR
- [ ] 变更必须经 `Reviewer Agent` + 人工确认（Human-in-the-loop）双签
- [ ] 一键回滚脚本可用，回滚后系统恢复到变更前行为
- [ ] Self-Evolve 操作全程审计日志可查

---

### 9. 验收与追踪约定 (Verification & Tracking)

- **DoD (Definition of Done)**：一个 Milestone 完成当且仅当其全部 `- [ ]` 验收项被勾选并附证据（命令输出 / 产物链接）。
- **证据存放**：`docs/acceptance/M{n}.md`，含验收命令的实际输出截图或日志。
- **进度追踪**：每个 Milestone 对应一个 GitHub Milestone，验收项拆为 Issue 打 label `acceptance`。
- **回归保护**：任一 Milestone 完成后将其 golden tasks 纳入 CI 回归集，禁止后续 Milestone 破坏前序验收。

