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

### Gap 9：Reviewer 无跨轮记忆，可被"换写法蒙混"
- **现状**：[Reviewer](file:///workspace/packages/orcha-core/src/sub_agents/mod.rs) 每轮独立审核，不记得上一轮拒了哪几点。AI 可换种写法绕过同样的拒绝理由。
- **目标**：Test & QA 是"带状态闸门"——多轮循环中记住上一轮拒了哪几点，AI 不能靠换写法蒙混。
- **影响**：Reviewer 需注入前序拒绝记录（从 history/memory 读），审核时比对"是否重复犯同一类错"。

### Gap 10：无双向守护与自愈，单点崩全崩
- **现状**：`orcha-cli` 跑 Cycleround 是单进程，崩了就崩了，无 watchdog、无自动恢复。
- **目标**：Core ↔ Gateway 互为 watchdog；Gateway AI 常驻做智能诊断（读 crash log → 针对性恢复）；LLM 超时 → IM 通知 → 预设脚本 restart → 恢复再通知的 fallback 降级链。
- **影响**：M6 需加 watchdog 机制 + 降级链，当前完全没有。

---

## 7.5 底层依据：OpenClaw 反面教材对标

架构决策的底层依据来自 OpenClaw 的 8 个坑（反面教材），每条都映射到 Orcha 的规避设计：

| OpenClaw 坑 | 现象 | Orcha 规避 |
|-------------|------|-----------|
| 1. 单进程一崩全崩 | IM 连接和任务执行同进程，任务 panic 带崩 IM 连接 | Core ↔ Gateway 分进程 + 双向 watchdog（决策 5） |
| 2. npm 原子性 | `npm install` 中断留坏 node_modules，后续全错 | Minicommit + 回滚机制（Cycleround 已有） |
| 3. Markdown 记忆 | 用 .md 文件记上下文，解析易错且无结构 | JSONL MemoryStore（结构化，已实现） |
| 4. 盲跑 | 不测试就提交，AI 自欺"搞定了" | Test&QA 带状态闸门，多轮记拒绝点（决策 7） |
| 5. 路径错乱 | workspace 和执行目录不一致，改错文件 | FsSandbox 隔离 + workspace 绝对路径（M2 已有） |
| 6. reconnect loop | WS 断线疯狂重连被限流，雪崩 | Gateway 长连接退避 + 降级链（决策 5） |
| 7. 国产 IM 水土不服 | 飞书/QQ 非 Slack，协议差异大 | Feishu Adapter 独立 TS 进程 fork 官方插件（决策 6） |
| 8. token 雪崩 | LLM 超时不断重试，token 烧爆 | LLM 超时 → IM 通知 → restart 降级链（决策 5） |

**一句话**：Orcha 的每个架构选择都能在 OpenClaw 的失败案例里找到对应的"如果不这么做会怎样"。

---

## 8. 待定问题 → 已决策（2026-07-06）

7 项关键决策已定，M6 执行依据：

### 决策 1：进程模型 → 分进程，存储换 SQLite

- **结论**：Gateway 和 Shell 是两个独立进程，`FileTaskStore` → `SqliteTaskStore`。
- **理由**：设计文档已定 Shell 在用户机、Gateway 在云端，物理分进程。SQLite WAL + 跨进程锁解决 Gap 6，附带解决 JSON corruption 风险。`TaskStore` trait 已抽象，换实现不影响上层。
- **落地**：
  - M6 新建 `orcha-gateway` crate，独立 `main`
  - `orcha-core` 加 `SqliteTaskStore`（feature gate），保留 `FileTaskStore` 给 cli dev 模式
  - Shell 继续读 SQLite（只读连接），不碰写
- **对应 Gap**：Gap 6（无并发写保护）、Gap 8（无 Gateway crate）

### 决策 2：Cycleround 重构 → 渐进，调度权交 AI

- **结论**：保留确定性 Cycleround 当 dev/test 假实现，LlmCycleround 演进为主路径。硬编码顺序改为 **AI 每步决策下一步**。
- **Cycleround 不退化为纯脚手架**，仍然管：熔断、StepResult 记录落盘、Minicommit 触发、异常捕获回滚。但**不再决定调谁**。
- **目标调度模型**：
  ```rust
  loop {
      let decision = ai_decide_next_step(&context); // LLM 返回 { agent, params }
      match decision.agent {
          "observer" | "planner" | "worker" | "tester" | "reviewer" | "fixer" => agent.run(&ctx),
          "exit" => break, // QA 通过，正常退出
          _ => /* unknown */,
      }
      if should_stop(&ctx) { break; } // 熔断
  }
  ```
- **为什么不全重写**：9 天时间，确定性版本能跑通 happy path，留作单测 fixture 和离线调试工具。重写风险大收益小。
- **对应 Gap**：Gap 2（调度硬编码）、Gap 3（Worker 瞎改文件——调度交 AI 后，Worker 退化为工具调用）

### 决策 3：事件流 → M6 优先做，和调度重构同步

- **结论**：事件流不后置。M6 必须做，且和 Cycleround 重构同步完成。
- **理由**：飞书 IM 核心体验是实时反馈。用户 @Orcha 后 30 秒没动静会以为 bot 死了。飞书卡片支持 patch 更新（"🔍 观察中…" → "📋 规划中…" → "💻 写代码中…"），需 Core 持续吐事件。
- **改造方式**（工作量不大）：
  ```rust
  pub fn run(&mut self) -> mpsc::Receiver<RoundEvent> {
      let (tx, rx) = mpsc::channel();
      std::thread::spawn(move || {
          loop {
              tx.send(RoundEvent::AgentStarted { agent: "planner" }).ok();
              // ... 执行 ...
              tx.send(RoundEvent::AgentFinished { agent: "planner", result }).ok();
          }
      });
      rx
  }
  ```
  Gateway 拿 receiver 转成 IM 卡片更新推给用户。
- **对应 Gap**：Gap 4（无事件流）

### 决策 4：SubAgent trait → 先解耦调度，权限后置

- **结论**：Gap 2（调度）和 Gap 1（权限）**分开做**。M6 只做调度解耦，权限维度等初赛后再加。
- **理由**：当前 trait 极简（`name` + `run`），初赛阶段所有 agent 在同一 workspace 跑，权限隔离非痛点。强行加 capability 拖慢调度重构。
- **M6 trait 改动极小**：trait 本身不动，调度逻辑从 Cycleround 挪到 AI 决策侧。
- **初赛后再加**：
  ```rust
  pub trait SubAgent {
      fn name(&self) -> &'static str;
      fn capabilities(&self) -> Capabilities; // READ | WRITE | EXECUTE
      fn run(&self, ctx: &StepContext) -> StepOutput;
  }
  ```
- **对应 Gap**：Gap 1（部分——调度解耦做了，权限维度后置）

### 决策 5：双向守护与自愈 → Core ↔ Gateway 互为 watchdog + 降级链

- **结论**：Core ↔ Gateway 互为 watchdog；Gateway AI 常驻做智能诊断；LLM 超时触发降级链。
- **降级链**：LLM 超时 → IM 通知用户"AI 卡住，正在恢复" → 预设脚本 restart → 恢复后再通知"已恢复"。
- **启动顺序**：外层 init 保进程 → Core 先起 → Gateway 后起（Gateway 依赖 Core 的 Unix Socket 就绪）。
- **理由**：OpenClaw 单进程一崩全崩（坑 1）+ reconnect loop 雪崩（坑 6）+ token 雪崩（坑 8）的教训。分进程隔离故障域 + watchdog 自愈 + 降级链防雪崩。
- **对应 Gap**：Gap 10

### 决策 6：Feishu Adapter → 独立 TS 进程，fork 官方插件

- **结论**：飞书接入不 Rust 重撸 WS，而是 fork OpenClaw 官方插件（`larksuite/openclaw-lark`，MIT）当独立 Node/TS 进程，通过 Unix Socket / localhost TCP 接 Rust Gateway。
- **理由**：OpenClaw "国产 IM 水土不服"（坑 7）的教训——飞书/QQ 非 Slack，协议差异大，Rust 生态缺成熟 SDK，重撸风险高。fork 官方插件复用其 WS 重连/事件解析逻辑，Rust 侧只管业务。
- **架构**：
  ```
  飞书 WS ←→ Feishu Adapter (TS, fork openclaw-lark)
                     ↓ Unix Socket / localhost TCP
                orcha-gateway (Rust)
                     ↓
                orcha-core (Cycleround)
  ```
- **对应 Gap**：Gap 8（Gateway crate 仍要建，但飞书 SDK 接入改为 Adapter 进程）

### 决策 7：Test&QA 带状态闸门 → Reviewer 注入跨轮拒绝记忆

- **结论**：Reviewer 多轮循环中记住上一轮拒了哪几点，AI 不能靠换写法蒙混。
- **实现方向**：Reviewer 从 history/memory 读前序拒绝记录，审核时比对"是否重复犯同一类错"；重复犯错加重拒绝权重或触发 Fixer。
- **理由**：OpenClaw "盲跑"（坑 4）的教训——不测试就提交、AI 自欺"搞定了"。带状态闸门强制 AI 真正解决上次的问题，而非绕过。
- **对应 Gap**：Gap 9

---

## 9. M6 执行优先级

| 优先级 | 事项 | 对应 Gap / 决策 |
|--------|------|----------|
| P0 | 新建 `orcha-gateway` crate + Unix Socket 接 Feishu Adapter（TS） | Gap 8 / 决策 6 |
| P0 | Cycleround 改为 AI 驱动调度 + 事件流 | Gap 2, Gap 4 / 决策 2, 3 |
| P0 | Feishu Adapter TS 进程（fork `openclaw-lark`）接 Gateway | 决策 6 |
| P0 | 双向 watchdog + 降级链（LLM 超时 → 通知 → restart → 恢复） | Gap 10 / 决策 5 |
| P1 | SQLite 替换 FileTaskStore | Gap 6 / 决策 1 |
| P1 | Gateway 任务队列 + worker 池（非阻塞） | Gap 5 |
| P1 | Reviewer 跨轮拒绝记忆（带状态闸门） | Gap 9 / 决策 7 |
| P2 | 飞书卡片实时更新（patch card） | Gap 4(消费端) |
| P2 | SubAgent 调度解耦（trait 不改，调用方改） | Gap 1(部分) |

**一句话**：M6 的核心是让 Core 能"说话"（事件流）和"听话"（AI 调度），外加"不崩"（双向守护）和"不蒙混"（状态闸门），其余都是支撑设施。

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
