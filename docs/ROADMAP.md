# Orcha 开发路线图与可验证 Milestone

> **Slogan**: Let the Orcha play.
>
> **原则**：每个 Milestone 必须有**可执行、可观测**的验收命令或产物。未通过验收不得进入下一个 Milestone。
>
> **MVP 完成定义** = M0 → M3 全部验收通过（对应 Phase 1）。

---

## 1. 长线规划总览

| Milestone | 名称 | Phase | MVP | 核心可验证产物 |
| :--- | :--- | :--- | :---: | :--- |
| **M0** | [骨架与契约](#m0--骨架与契约-foundation) | Phase 1 | ✅ | 仓库结构 + 数据模型 + CI |
| **M1** | [Task 模型与本地 CLI](#m1--task-模型与本地-cli) | Phase 1 | ✅ | `orcha status` 可查询 |
| **M2** | [单步 Sub-Agent 执行](#m2--单步-sub-agent-执行) | Phase 1 | ✅ | 单任务跑通并产出 diff |
| **M3** | [Cycleround 闭环](#m3--cycleround-闭环-mvp-完成) | Phase 1 | ✅ | 闭环修复样例 bug |
| **M5** | [Web UI Shell + HTTP API](#m5--web-ui-shell--http-api) | Phase 2 | — | Web UI + JSON API 可观测任务 |
| **M6** | [Gateway 基础设施与跨进程通信](#m6--gateway-基础设施与跨进程通信) | Phase 2 | — | Gateway 进程 + IPC + SQLite + 事件流 |
| **M7** | [IM 接入与智能守护](#m7--im-接入与智能守护) | Phase 2 | — | 飞书/QQ 长连接 + AI 调度 + 降级链 |
| **M8** | [Plugin 子代理体系](#m8--plugin-子代理体系) | Phase 3 | — | 第三方可注册 Sub-Agent |
| **M9** | [Self-Evolve](#m9--self-evolve) | Phase 4 | — | Orcha 提交自身调度 PR |

> **存储方案统一**：原 M4（Redis/PG 持久化）已合并进 M6（SqliteTaskStore）。M4 验收虚标已修正——代码实际只有 FileTaskStore，无 redis/pg 依赖。
> **平台边界**：M0-M3 全平台（含 Windows MSVC 零 C 依赖）；M6+ Gateway/IPC 层经 trait 抽象支持 Unix Socket / Named Pipe / localhost TCP，仍跨平台。

---

## M0 — 骨架与契约 (Foundation)

**目标**：建立可运行工程骨架与统一数据契约，所有后续模块在此之上生长。

**技术栈**：Rust (stable), Cargo workspace, serde + schemars, clap。

**交付物**：
- Monorepo 结构（Cargo workspace）：`packages/orcha-core`、`packages/orcha-shell`、`packages/orcha-cli`、`packages/orcha-sdk`
- 核心数据模型（serde + schemars）：`Task`、`OrchaEvent`、`OrchaResponse`、`Artifact`、`Step`、`StepResult`
- 测试框架（`cargo test`）+ CI（fmt + clippy + test）

**验收（可验证）**：
- [x] `orcha --version` 打印版本号 `orcha 0.1.0`，退出码 `0`
- [x] `cargo test --all` 全绿（当前 31 个测试通过）
- [x] CI 在 PR/push 上自动运行 fmt + clippy + test（`.github/workflows/ci.yml` 存在）
- [x] CI 在 GitHub Actions 上首次运行结果为 green（ubuntu + windows MSVC 矩阵）
- [x] `orcha schema --export > schema.json` 输出全部 15 个模型的 JSON Schema
- [x] `cargo fmt --all --check` 通过
- [x] `cargo clippy --all-targets -- -D warnings` 0 警告

---

## M1 — Task 模型与本地 CLI

**目标**：本地可创建、查询、流转任务状态机；不引入外部服务依赖（先用本地文件存储）。

**交付物**：
- Task 仓储：`TaskStore` trait + `FileTaskStore`（JSON 文件落盘），trait 预留 `SqliteTaskStore`/Redis/PG 接口。M1 用纯 Rust 文件存储以保 Windows MSVC 零 C 依赖；M4 持久化升级时再引入 rusqlite。
- **FileTaskStore 并发保护**：内存队列 + 单线程串行落盘，防文件截断（M1 阶段就做，不等 M4）
- 状态机：`PENDING → RUNNING → (BLOCKED ↔ RUNNING) → DONE | FAILED`（M0 已实现）
- CLI：`orcha init`、`orcha run "<desc>"`、`orcha status [id]`、`orcha list [--status <STATUS>]`
- Home 解析：`--home` > `$ORCHA_HOME` > `./.orcha`

**验收（可验证）**：
- [x] `orcha run "hello world"` 返回 `T-` 开头的 task_id
- [x] `orcha status <id>` 输出 JSON，`status` 字段 ∈ 合法枚举
- [x] 非法状态迁移抛出 `InvalidTransition`，且有单元测试覆盖（M0 state_machine 测试 + store update 测试）
- [x] `orcha list` 列出全部任务，支持 `--status` 过滤（大小写不敏感）
- [x] 重启进程后 `orcha list` 仍可查到历史任务（持久化生效；`persistence_survives_process_restart` 集成测试跨独立子进程验证）
- [ ] FileTaskStore 并发写入不截断（内存队列 + 串行落盘的单测覆盖）
- [ ] CI 在 GitHub Actions 上跑通本次新增测试（ubuntu + windows MSVC）— 待 push 后由 Actions 确认

---

## M2 — 单步 Sub-Agent 执行

**目标**：在沙箱内跑通 `观察 → 规划 → 执行` 最小链路（**单步，不闭环**）。

**交付物**：
- `SubAgent` trait：`run(&StepContext) -> StepOutput`（M2 单步执行）
- 沙箱：`FsSandbox`（文件系统隔离），`DockerSandbox` 推迟到 M3
- 三个最小确定性实现：`Observer`、`Planner`、`Worker`（无外部 LLM 依赖）
- 产物落盘到 workspace（由 `FsSandbox::prepare(task_id)` 隔离）

**验收（可验证）**：
- [x] 输入 "在 repo 中创建 hello.py 输出 hello"，真实产出文件 `hello.py`
  （`tests/m2_pipeline.rs::worker_creates_file_and_diff_for_hello_py` + 端到端测试）
- [x] 产出被记录为 `Artifact(type=CODE_DIFF)`，含可应用的 patch
  （`StepOutput::with_artifacts` 自动回填 `artifact_id`，类型 `ArtifactType::CodeDiff`）
- [x] `git apply` 该 patch 成功
  （`end_to_end_pipeline_produces_file_and_git_applyable_patch` 在真实 git 仓库中 apply 并校验 hello.py 内容）
- [x] 每步写入结构化日志到 `task:{id}:history`
  （由 M3 实现：`Cycleround::run_with_history` 把每轮 `RoundRecord` 追加写入 `FileHistoryStore`，落盘到 `{home}/history/{task_id}.jsonl`。M2 单步执行无调度器，此项在 M3 完成后回溯确认）
- [x] 断网状态下沙箱仍可执行基础文件操作
  （三个 Sub-Agent 均为确定性最小实现，零网络/LLM 依赖）

---

## M3 — Cycleround 闭环 (MVP 完成)

**目标**：打通 `Plan → Code → Test → Review → Fix` 循环，**MVP 达成**。

**交付物**：
- Cycleround 调度器，含熔断参数（`max_rounds=10`、`max_retries=3`、`cool_down=60s`，**可经 `config.toml` per-task / 全局配置，不硬编码**）
- `Tester Agent`（跑测试）+ `Reviewer Agent`（静态检查 / diff 审核）
- `Fixer Agent`：失败后回到执行环节
- 端到端 golden tasks 集合（≥ 3 个，建议 ≥ 10 个以扩覆盖面）

**验收（可验证）**：
- [x] 给定含已知 bug 的样例 repo，`orcha fix` 闭环修复且测试通过
  （`orcha fix --workspace <dir> "创建 hello.py 输出 hello"`：成功路径 GT-1 单轮闭环；
  Fixer 修复路径 GT-2 第 2 轮闭环；CLI 端到端测试 `fix_cli_succeeds_*` 全绿）
- [x] 触达 `max_rounds` 时任务转 `FAILED`，**不死循环**
  （`cycleround_fails_with_max_rounds_exceeded_on_unparseable_task` 验证 max_rounds 路径；
  GT-3 验证 max_retries 路径，均触达熔断即停）
- [x] 每一轮 round 在 history 中可追溯（含耗时、token、产物引用）
  （GT-1/2/3 各自断言 `RoundRecord.started_at`/`finished_at`/`tokens_used`/`artifacts` 落盘可读；
  `history.rs` 9 单测覆盖 JSONL 格式 / 跨 task 隔离 / 重启存活）
- [x] 3 个 golden tasks 全部通过（`cargo test --test m3_golden_tasks`）
- [x] `./scripts/mvp-demo.sh` 一键复现完整闭环
  （脚本跑通 3 条 demo：成功路径 / Fixer 修复 / 熔断不死循环；并校验 JSONL history 落盘可追溯）

---

## M5 — Web UI Shell + HTTP API

> **初赛 Demo 约束**：初赛只搞 IM 交互（M7），M5 Web UI **不纳入初赛验收范围**。M5 已实现并保留作为开发期观测面板，但不作为初赛交付。

**目标**：面向用户的可视化面板与对话窗口。提供 Web UI 与 JSON API 展示任务状态/历史/memory；任务触发主入口见 M7 IM Gateway，Shell 本身不做 IM 接入。

**交付物**：
- 基于 `tiny_http` 的同步 Web UI server（`orcha-shell` crate，无异步运行时；CSS/JS 经 `include_str!` 编入二进制，单文件部署）
- 多页面前端（任务列表 / 任务详情 / history 时间线 / LLM memory 标签页，原生 JS 无构建）
- JSON API 端点（`/api/tasks`、`/api/tasks/summary`、`/api/tasks/:id`、`/api/tasks/:id/history`、`/api/tasks/:id/memory`）
- `orcha shell --port` 一键启动 + `scripts/web-demo.sh` 一键演示脚本

**验收（可验证）**：
- [x] `orcha shell --port 7421` 启动后浏览器可访问任务列表 / 任务详情 / history 时间线
  （`packages/orcha-shell/src/server.rs` 路由 `/`、`/tasks/:id`、`/tasks/:id/history`）
- [x] `GET /api/tasks` 返回任务列表 JSON，`GET /api/tasks/summary` 返回状态统计
  （`server::tests::list_tasks_serializes_rows`、`server::tests::stats_counts_by_status` 单测覆盖）
- [x] `GET /api/tasks/:id/history` 返回 `Vec<RoundRecord>`，`GET /api/tasks/:id/memory` 返回 LLM 对话历史
  （`server::tests::memory_endpoint_returns_empty_for_unknown_task` 覆盖空 task 不报错路径）
- [x] 未知路由返回 404
  （`server::tests::route_returns_404_for_unknown_path` 单测覆盖）
- [x] `./scripts/web-demo.sh` 一键跑起 Web UI 并种出 2 个 DONE task + history + artifacts

---

## M6 — Gateway 基础设施与跨进程通信

> **本里程碑不含 IM 接入**（IM 接入与守护见 M7）。M6 只搭"管道"：Gateway 进程、跨进程 IPC、SQLite 存储、任务队列、事件流。合并了原 M4（状态持久化）的存储迁移。

**目标**：建立 Gateway 独立进程与 Core daemon，提供跨进程 IPC（支持 Windows）、SQLite 共享存储、非阻塞任务队列、Cycleround 事件流管道。为 M7 的 IM 接入与智能守护打地基。

**交付物**：
- 新建 `orcha-gateway` crate（独立 `main`，独立进程，与 Shell 故障域隔离）
- **Core 改为常驻 daemon**：`orcha-core` 仍为 lib，Gateway 进程常驻调用，不再走 CLI 单次阻塞调用
- **IPC trait 抽象**（跨平台）：`IpcTransport` trait，实现 Unix Socket（Linux/macOS）/ Named Pipe（Windows）/ localhost TCP（fallback），跨平台编译
- **`SqliteTaskStore`**（feature gate，WAL + 跨进程锁，替代 `FileTaskStore`；Shell 只读连接，Gateway 独占写；保留 `FileTaskStore` 给 cli dev 模式）
- **Gateway 任务队列 + worker 池**（非阻塞：任务入队后后台 worker 跑 Cycleround，Gateway 不被单个任务卡死）
- **Cycleround 事件流改造**：`run` 返回 `mpsc::Receiver<RoundEvent>`，每步吐 `AgentStarted`/`AgentFinished`（调度顺序本里程碑暂不变，仅通事件管道；AI 驱动调度见 M7）
- **`config.toml` + 密钥管理**：统一配置 LLM key / SQLite 路径 / 端口 / IM 凭证占位，密钥不硬编码
- **trace ID**：每个任务一个 ID 贯穿 Gateway → Core → history，跨进程可追溯

**验收（可验证）**：
- [ ] `orcha-gateway` crate 独立进程可启动，Core 作为 daemon 常驻
- [ ] `IpcTransport` trait 实现 Unix Socket + Named Pipe + localhost TCP 三后端，跨平台编译通过（CI `windows-latest` 仍绿）
- [ ] `SqliteTaskStore` 替换 `FileTaskStore`，Gateway 与 Shell 分进程读写同一 SQLite 不损坏
- [ ] Gateway 任务队列 + worker 池：任务入队后非阻塞执行，一个任务跑期间可继续收新任务
- [ ] Cycleround `run` 返回 `mpsc::Receiver<RoundEvent>`，Gateway 可消费每步事件
- [ ] `config.toml` 可配置 LLM key / SQLite 路径 / 端口，密钥不硬编码
- [ ] 每个任务有 trace ID，贯穿 Gateway/Core/history 可追溯
- [ ] kill Gateway 后重启，`RUNNING` 中断的 Task 自动恢复

---

## M7 — IM 接入与智能守护

> **架构对标**：本里程碑的每项关键决策都对应 OpenClaw 的一个失败坑（详见 `docs/ARCHITECTURE_ANALYSIS.md` 第 7.5 节）。核心规避：单进程一崩全崩→分进程+watchdog、国产 IM 水土不服→fork 官方 TS 插件、盲跑→带状态闸门、token 雪崩→降级链。
>
> **初赛 Demo 核心**：M7 是初赛交付主体（M5 Web UI 不纳入初赛）。

**目标**：**触发主入口（一等公民）**。通过各平台官方 SDK 的**长连接**接入 IM，提供实时交互体验（体感对标 OpenClaw）。@Orcha 触发任务后，进度经长连接实时推送回原会话。Cycleround 调度改为 AI 驱动，叠加双向守护与降级链保障可用性。

**交付物**：
- **Feishu Adapter 独立 TS 进程**：fork OpenClaw 官方插件 `larksuite/openclaw-lark`（MIT），作为独立 Node/TS 进程经 IPC 接 Rust Gateway，不 Rust 重撸 WS
- 飞书 SDK 长连接接入（经 Adapter TS 进程，WebSocket 接收事件，免公网 webhook）
- QQ SDK 长连接接入
- **Cycleround 改为 AI 驱动调度**：硬编码 Observer→Planner→...→Fixer 改为 AI 每步决策下一步调谁，带熔断（确定性 Cycleround 保留为 dev/test 假实现与 golden tasks 回归集）
- **Test&QA 带状态闸门**：Reviewer 多轮循环中记住上一轮拒了哪几点，AI 不能靠换写法蒙混
- **双向守护与自愈**：Core ↔ Gateway 互为 watchdog；Gateway AI 常驻做智能诊断（读 crash log → 针对性恢复）
- **降级链**：LLM 超时 → IM 通知用户"AI 卡住，正在恢复" → 预设脚本 restart → 恢复后再通知"已恢复"
- **启动顺序**：外层 init 保进程 → Core 先起 → Gateway 后起 → Adapter 最后
- 飞书卡片实时更新（patch card：执行中持续更新同一张卡片 "🔍 观察中… → 📋 规划中… → 💻 写代码中…"）
- 鉴权与白名单群校验
- **部署拓扑文档**：每进程的启动顺序 / 健康检查方式 / 崩溃重启策略 / graceful shutdown 行为

**验收（可验证）**：
- [ ] Feishu Adapter（fork `openclaw-lark`）TS 进程可启动并经 IPC 接入 Gateway
- [ ] 飞书通过 Adapter 长连接接收 `@Orcha fix <issue>` 并触发 Cycleround
- [ ] QQ 通过 SDK 长连接接收并触发任务
- [ ] Cycleround 调度由 AI 驱动（不再硬编码 Observer→Planner→...→Fixer 顺序）
- [ ] Reviewer 跨轮拒绝记忆生效：AI 换写法重复犯错会被加重拒绝
- [ ] Core ↔ Gateway 互为 watchdog：一方崩溃，另一方检测并触发恢复
- [ ] LLM 超时触发降级链（IM 通知 → restart → 恢复通知），不 token 雪崩
- [ ] 启动顺序正确（外层 init → Core 就绪 → Gateway 后起 → Adapter 最后）
- [ ] 飞书卡片在任务进行中持续 patch 更新（"🔍 观察中…" → "📋 规划中…" → "💻 写代码中…"）
- [ ] 执行进度经长连接实时回传原会话（非 HTTP 轮询，体感对标 OpenClaw）
- [ ] 产物 / Artifact 链接在 IM 中可点击展开
- [ ] 私有群权限校验生效（机器人仅在白名单群内响应）
- [ ] 多会话并发互不干扰
- [ ] 部署拓扑文档完整（启动顺序/健康检查/重启策略/graceful shutdown）
- [ ] （可选扩展）Slack 等其他 IM 复用同一长连接接入模式

---

## M8 — Plugin 子代理体系

> **前置 ADR**：开工前需先出 Architecture Decision Record，选定插件机制——语言（Rust / WASM / 任意可执行文件）、加载方式（动态链接 / WASM 沙箱 / 子进程 spawn + JSON-RPC）、沙箱隔离级别、版本兼容策略。

**目标**：第三方可注册自定义 Sub-Agent。

**验收（可验证）**：
- [ ] ADR 完成，选定插件机制并记录权衡
- [ ] `orcha plugin add <path>` 注册成功并出现在 `orcha plugin list`
- [ ] Plugin 实现 `SubAgent` 接口即可被 Cycleround 调度
- [ ] Plugin 在沙箱内运行，无越权访问宿主文件系统
- [ ] Plugin 崩溃不影响 Core 稳定性（隔离验证）

---

## M9 — Self-Evolve

**目标**：Orcha 可在受控前提下修改自身调度代码。

**验收（可验证）**：
- [ ] 给定 "优化 Cycleround 熔断策略" 任务，Orcha 产出可合入的 PR
- [ ] 变更必须经 `Reviewer Agent` + 人工确认（Human-in-the-loop）双签
- [ ] 一键回滚脚本可用，回滚后系统恢复到变更前行为
- [ ] Self-Evolve 操作全程审计日志可查

---

## 验收与追踪约定 (Verification & Tracking)

- **DoD (Definition of Done)**：一个 Milestone 完成当且仅当其全部 `- [ ]` 验收项被勾选并附证据（命令输出 / 产物链接）。
- **证据存放**：`docs/acceptance/M{n}.md`，含验收命令的实际输出截图或日志。
- **进度追踪**：每个 Milestone 对应一个 GitHub Milestone，验收项拆为 Issue 打 label `acceptance`。
- **回归保护**：任一 Milestone 完成后将其 golden tasks 纳入 CI 回归集，禁止后续 Milestone 破坏前序验收。
