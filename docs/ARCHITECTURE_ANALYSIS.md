# Orcha 架构现状分析（基于代码实现）

> 本文档基于对 `packages/orcha-core`、`orcha-cli`、`orcha-shell`、`orcha-llm`、`orcha-sdk` 全部核心代码的阅读，记录**当前实际实现**（不是目标架构），并标注与讨论中目标架构的 gap。
>
> 目的：作为架构演进的决策依据，避免后续开发被现有代码结构误导。

---

## 1. 当前分层结构（实际代码）

```
┌──────────────────────────────────────────────────────┐
│  orcha-cli  (入口)                                   │
│  ├─ orcha fix      → 直接调 Cycleround / LlmCycleround│
│  ├─ orcha shell    → 起 orcha-shell HTTP server      │
│  └─ orcha run/list/status/history/artifacts/recover  │
└──────────────┬───────────────────────────────────────┘
               │ 直接函数调用（同进程，无 IPC）
┌──────────────▼───────────────────────────────────────┐
│  orcha-core  (大脑)                                   │
│  ├─ Cycleround         确定性闭环（持有 Observer/Planner/Worker/Tester/Reviewer/Fixer 实例）
│  ├─ LlmCycleround      LLM 闭环（Planner/Worker/Reviewer 换成 LLM 版，Observer/Tester/Fixer 复用确定性）
│  ├─ sub_agents::*      确定性 Sub-Agent（直接 fs::write 改文件）
│  ├─ llm_agents::*      LLM Sub-Agent（调 orcha-llm，解析 JSON，再 fs::write 改文件）
│  ├─ memory             LLM 对话历史（JSONL，给 LLM 看的跨轮记忆）
│  ├─ history            RoundRecord 持久化（JSONL，给人看的执行记录）
│  ├─ store              TaskStore（JSON 文件，无锁）
│  └─ sandbox/recovery/state_machine                    │
└──────────────┬───────────────────────────────────────┘
               │ 直接依赖
┌──────────────▼───────────────────────────────────────┐
│  orcha-llm   (LLM 客户端)                            │
│  ├─ LlmClient trait     chat(&[ChatMessage]) -> String│
│  ├─ OpenAiCompatibleClient  ureq 同步阻塞 POST       │
│  └─ prompt builder + JSON parser                     │
└──────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────┐
│  orcha-shell  (只读 Web UI)                          │
│  ├─ HttpServer    tiny_http，每请求一线程             │
│  ├─ 路由全是 GET（/api/tasks, /:id/history, /:id/memory）│
│  └─ 复用 FileTaskStore/FileHistoryStore/FileMemoryStore（共享 home 目录）│
└──────────────────────────────────────────────────────┘
```

**关键事实**：
- `orcha-cli` 是唯一入口，通过**直接函数调用**驱动 Core，不是 IPC/进程间通信。
- `orcha-shell` 与 `orcha-cli` 是**两个独立进程**，但共享同一 `{home}` 目录（读同一份 JSON/JSONL 文件）。
- **没有** Gateway crate，**没有**任何 IM 接入代码。

---

## 2. Cycleround 调度逻辑（确定性版）

**位置**：[packages/orcha-core/src/cycleround.rs](file:///workspace/packages/orcha-core/src/cycleround.rs)

**结构体**（[cycleround.rs:106](file:///workspace/packages/orcha-core/src/cycleround.rs#L106)）：
```rust
pub struct Cycleround {
    config: CycleConfig,
    observer: Observer,   // 持有 6 个 Sub-Agent 实例
    planner: Planner,
    worker: Worker,
    tester: Tester,
    reviewer: Reviewer,
    fixer: Fixer,
}
```

**执行流程**（`run_inner`，[cycleround.rs:184](file:///workspace/packages/orcha-core/src/cycleround.rs#L184)）：
```
for round in 1..=max_rounds:
    1. Observer.run()        → 始终执行，列文件
    2. Planner.run()         → 失败则跳过后续，不调 Fixer
    3. Worker.run()          → 仅当 Planner 成功
    4. Tester.run()          → 仅当 Worker 成功
    5. Reviewer.run()        → 仅当 Tester 成功
    全成功 → return Success
    6. Fixer.run()           → 仅当 Planner 成功；累计 fix_attempts
       触达 max_retries → return Failed(MaxRetriesExceeded)
触达 max_rounds → return Failed(MaxRoundsExceeded)
```

**关键特征**：
- **顺序硬编码**：Observer→Planner→Worker→Tester→Reviewer→Fixer 的顺序写死在 `run_inner` 里，不是 AI 决定调谁。
- **同步阻塞**：整个循环跑完才返回 `CycleOutcome`，中间无 callback/channel/stream。
- **history 是旁路**：每轮 `RoundRecord` 落盘到 JSONL（[cycleround.rs:283](file:///workspace/packages/orcha-core/src/cycleround.rs#L283)），但不影响主流程，写入失败只打 warn。
- **熔断**：`max_rounds`（默认 10）+ `max_retries`（默认 3，指 Fixer 调用次数）。

---

## 3. Sub-Agent 抽象与实现

### 3.1 SubAgent trait（[sub_agent.rs:53](file:///workspace/packages/orcha-core/src/sub_agent.rs#L53)）

```rust
pub trait SubAgent {
    fn name(&self) -> &'static str;          // 角色名："observer"/"planner"/...
    fn run(&self, ctx: &StepContext) -> StepOutput;
}
```

**StepContext**（[sub_agent.rs:11](file:///workspace/packages/orcha-core/src/sub_agent.rs#L11)）：
```rust
pub struct StepContext {
    pub workspace: PathBuf,       // Sub-Agent 在此直接 fs::write
    pub task: Task,
    pub prior_artifacts: Vec<Artifact>,
    pub prior_steps: Vec<Step>,
}
```

### 3.2 确定性实现（[sub_agents/mod.rs](file:///workspace/packages/orcha-core/src/sub_agents/mod.rs)）

| Agent | 干什么 | 改文件？ |
|-------|--------|---------|
| Observer | 列 workspace 文件 | 否 |
| Planner | 正则解析 "创建 X 输出 Y" | 否 |
| Worker | `fs::write(target_path, content)` | **是**（[mod.rs:114](file:///workspace/packages/orcha-core/src/sub_agents/mod.rs#L114)） |
| Tester | 跑 `python3 test.py` / `cargo test` | 否 |
| Reviewer | 检查 patch 格式 + 路径穿越 | 否 |
| Fixer | 无测试框架时创建 `test.py` | **是**（[mod.rs:433](file:///workspace/packages/orcha-core/src/sub_agents/mod.rs#L433)） |

### 3.3 LLM 实现（[llm_agents.rs](file:///workspace/packages/orcha-core/src/llm_agents.rs)）

| Agent | 干什么 | 改文件？ |
|-------|--------|---------|
| LlmPlanner | 调 LLM 产出 JSON 计划 | 否 |
| LlmWorker | 调 LLM 产出文件列表，再 `fs::write` 每个文件 | **是**（[llm_agents.rs:181](file:///workspace/packages/orcha-core/src/llm_agents.rs#L181)） |
| LlmReviewer | 调 LLM 审核产出文件 | 否 |
| Observer/Tester/Fixer | **复用确定性实现** | 同上 |

**LlmCycleround**（[llm_agents.rs:307](file:///workspace/packages/orcha-core/src/llm_agents.rs#L307)）：
- 持有 `Observer / LlmPlanner / LlmWorker / Tester / LlmReviewer / Fixer` 6 个实例。
- 调度顺序与确定性 Cycleround **完全相同**（同样硬编码 Observer→Planner→...→Fixer）。
- 唯一区别：Planner/Worker/Reviewer 调 LLM 而非正则解析。
- Memory 层：`LlmPlanner/LlmWorker/LlmReviewer` 各自持有 `Option<Arc<dyn MemoryStore>>`，调 LLM 前注入前序对话，调完写 memory。

---

## 4. 数据层

### 4.1 三类持久化（全部 JSONL/JSON 文件，无锁）

| Store | trait | 文件实现 | 路径 | 并发安全 |
|-------|-------|---------|------|---------|
| TaskStore | `TaskStore` | `FileTaskStore` | `{home}/store/{task_id}.json` | **无锁**（[store.rs](file:///workspace/packages/orcha-core/src/store.rs)） |
| HistoryStore | `HistoryStore` | `FileHistoryStore` | `{home}/history/{task_id}.jsonl` | append-only，无锁 |
| MemoryStore | `MemoryStore` | `FileMemoryStore` | `{home}/memory/{task_id}.jsonl` | append-only，无锁 |

### 4.2 共享方式

`orcha-cli`（跑 Cycleround）和 `orcha-shell`（Web UI）是**两个独立进程**，通过共享 `{home}` 目录读写同一批文件。**没有任何锁机制**：
- `FileTaskStore::update` 是 read-modify-write 整个 JSON 文件，多进程并发写会丢数据。
- `FileHistoryStore` / `FileMemoryStore` 是 append，多进程并发 append 可能交错（但概率低）。

### 4.3 LLM 客户端（[orcha-llm/src/client.rs](file:///workspace/packages/orcha-llm/src/client.rs)）

```rust
pub trait LlmClient: Send + Sync {
    fn chat(&self, messages: &[ChatMessage]) -> Result<String, LlmError>;
}
```
- `OpenAiCompatibleClient`：`ureq` 同步阻塞 POST `/v1/chat/completions`。
- 配置从环境变量 `ORCHA_LLM_API_KEY` / `BASE_URL` / `MODEL` 读取。
- **无流式**（`"stream": false`），整响应等待。

---

## 5. CLI 入口（[orcha-cli/src/main.rs](file:///workspace/packages/orcha-cli/src/main.rs)）

**命令**：
| 命令 | 干什么 |
|------|--------|
| `orcha init` | 初始化 home 目录 |
| `orcha run "<desc>"` | 创建 Task（不执行） |
| `orcha fix "<desc>" [--workspace] [--llm]` | **阻塞**跑 Cycleround，跑完才返回 |
| `orcha shell --port 7421` | 起 Web UI HTTP server，阻塞 |
| `orcha history/artifacts/recover` | 观测/恢复 |

**关键**：`orcha fix` 是阻塞调用（[main.rs:304](file:///workspace/packages/orcha-cli/src/main.rs#L304)），在 Cycleround 跑完前，这个进程不响应任何其他输入。

---

## 6. Web UI（[orcha-shell/src/server.rs](file:///workspace/packages/orcha-shell/src/server.rs)）

- `tiny_http` 同步 server，每请求 spawn 一个 thread。
- 路由**全是 GET**：`/`、`/tasks/:id`、`/tasks/:id/history` + `/api/tasks` 等 JSON 端点。
- **无 POST**，不能触发任务。
- 每次请求新建 `FileTaskStore`/`FileHistoryStore`/`FileMemoryStore`（它们只是 path 持有者，构造廉价）。

---

## 7. 与目标架构的 Gap 清单

基于讨论中你提出的目标架构（Gateway 一等公民 / Cycleround 纯调 AI / Core 是脚手架 / agent 按权限分），当前实现的 gap：

### Gap 1：Sub-Agent 按角色分，不按权限分
- **现状**：`SubAgent::name()` 返回 `"observer"/"planner"/"worker"`，是**干什么活**的角色名。所有 agent 在 `ctx.workspace` 直接 `fs::write`，权限完全相同。
- **目标**：agent 按权限分（只读 / 可写 / 可执行），主 agent（AI）根据需要调对应权限的 sub-agent 当工具。
- **影响**：trait 需要加权限维度（capability 声明），StepContext 的 workspace 访问需按权限限制。

### Gap 2：Cycleround 硬编码调度顺序，AI 不主导
- **现状**：`run_inner` 写死 Observer→Planner→Worker→Tester→Reviewer→Fixer 顺序（[cycleround.rs:199-265](file:///workspace/packages/orcha-core/src/cycleround.rs#L199-L265)）。LlmCycleround 完全复制这个顺序。
- **目标**：主 agent（AI）决定调哪个 sub-agent、什么顺序。
- **影响**：调度逻辑应从 Cycleround 抽出，由 AI（LLM）驱动；Cycleround 退化为脚手架提供的工具集 + 熔断/记录。

### Gap 3：Worker "住在电脑里瞎改文件"
- **现状**：确定性 Worker 直接 `fs::write`（[sub_agents/mod.rs:114](file:///workspace/packages/orcha-core/src/sub_agents/mod.rs#L114)），LlmWorker 解析 LLM 输出后也直接 `fs::write`（[llm_agents.rs:181](file:///workspace/packages/orcha-core/src/llm_agents.rs#L181)）。
- **目标**：AI 调工具，不是 AI 直接改文件。工具（sub-agent）有受控的权限边界。
- **影响**：文件写入应经沙箱/工具接口，而非 Sub-Agent 内部直接 `std::fs`。

### Gap 4：无事件流，进度只能事后看
- **现状**：Cycleround 跑完才返回 `CycleOutcome`，中间无 channel/callback。history 落盘是旁路。
- **目标**：Gateway 长连接要实时回传进度，需要 Core 产出事件流。
- **影响**：Cycleround 需改造为发事件（mpsc::Receiver<RoundEvent>），或 Core 在独立线程跑 + channel。

### Gap 5：`orcha fix` 阻塞，Gateway 长连接会被卡死
- **现状**：`run_fix` 阻塞调用 `cycle.run_with_history`（[main.rs:304](file:///workspace/packages/orcha-cli/src/main.rs#L304)）。
- **目标**：Gateway 收到 @Orcha 后要能继续收新消息，不能被一个任务卡死。
- **影响**：Gateway 需任务队列 + worker 池（你已确认方向），每任务 spawn 线程跑 Cycleround。

### Gap 6：FileTaskStore 无并发写保护
- **现状**：`FileTaskStore::update` 是无锁 read-modify-write（[store.rs](file:///workspace/packages/orcha-core/src/store.rs)）。
- **目标**：Gateway 并发触发多任务时，多线程/多进程写同一 store 会损坏。
- **影响**：需加锁（Mutex 同进程 / flock 跨进程）或换 SQLite。你未定进程模型（同进程 vs 分进程）。

### Gap 7：Shell 全 GET，无触发能力（但目标是不需要）
- **现状**：Shell HTTP API 全是 GET 只读。
- **目标**（你已确认）：Shell 就是 watch，不加对话窗口，触发只走 Gateway。
- **状态**：**无 gap**，当前实现符合目标。文档已改回"可视化面板"。

### Gap 8：无 Gateway crate
- **现状**：完全没有 IM 接入代码，没有 `orcha-gateway` crate。
- **目标**：飞书/QQ SDK 长连接，Gateway 是触发主入口（一等公民）。
- **影响**：M6 需新建 crate，技术选型（SDK 直连 vs 统一抽象）你已确认暂不定。

---

## 8. 待你定夺的关键问题

1. **进程模型**：Gateway 与 Shell 同进程还是分进程？（决定 Gap 6 用 Mutex 还是 flock/SQLite）
2. **Cycleround 重构深度**：是渐进改造（保留确定性 Cycleround 当 dev 假实现，LlmCycleround 演进为主路径）还是重写（Cycleround 退化为纯脚手架工具集，调度完全交给 AI）？
3. **事件流改造**：Gap 4 优先做还是 M6 再做？（决定 M6 能不能实时回传）
4. **SubAgent trait 重构**：Gap 1+2 一起做还是分开？（权限维度和调度解耦耦合较深）

---

## 附：当前 crate 依赖图

```
orcha-sdk      (数据模型：Task/Artifact/Step/StepResult)
     ↑
orcha-core     (Cycleround/sub_agents/llm_agents/memory/history/store)
     ↑           ↑
     │           │
orcha-cli   orcha-llm  (LlmClient trait + OpenAI 兼容客户端)
     ↑           ↑
     └────┬──────┘
          │
     orcha-shell  (HttpServer，复用 core 的 Store)
```

- `orcha-cli` 依赖 `orcha-core` + `orcha-llm`（feature gate）+ `orcha-sdk`。
- `orcha-shell` 依赖 `orcha-core`（只用 Store trait）+ `orcha-sdk`。
- `orcha-core` 依赖 `orcha-llm`（仅 `llm` feature 下）。
- **无 `orcha-gateway` crate**。
