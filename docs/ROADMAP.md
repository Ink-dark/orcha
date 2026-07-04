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
| **M4** | [状态持久化](#m4--状态持久化) | Phase 2 | — | 重启后任务可恢复 |
| **M5** | [Gateway Shell + HTTP/CLI 触发](#m5--gateway-shell--httpcli-触发) | Phase 2 | — | HTTP API 可触发任务 |
| **M6** | [Slack Adapter](#m6--slack-adapter) | Phase 2 | — | Slack @Orcha 触发并回传 |
| **M7** | [Plugin 子代理体系](#m7--plugin-子代理体系) | Phase 3 | — | 第三方可注册 Sub-Agent |
| **M8** | [Self-Evolve](#m8--self-evolve) | Phase 4 | — | Orcha 提交自身调度 PR |

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
- 状态机：`PENDING → RUNNING → (BLOCKED ↔ RUNNING) → DONE | FAILED`（M0 已实现）
- CLI：`orcha init`、`orcha run "<desc>"`、`orcha status [id]`、`orcha list [--status <STATUS>]`
- Home 解析：`--home` > `$ORCHA_HOME` > `./.orcha`

**验收（可验证）**：
- [x] `orcha run "hello world"` 返回 `T-` 开头的 task_id
- [x] `orcha status <id>` 输出 JSON，`status` 字段 ∈ 合法枚举
- [x] 非法状态迁移抛出 `InvalidTransition`，且有单元测试覆盖（M0 state_machine 测试 + store update 测试）
- [x] `orcha list` 列出全部任务，支持 `--status` 过滤（大小写不敏感）
- [x] 重启进程后 `orcha list` 仍可查到历史任务（持久化生效；`persistence_survives_process_restart` 集成测试跨独立子进程验证）
- [ ] CI 在 GitHub Actions 上跑通本次新增测试（ubuntu + windows MSVC）— 待 push 后由 Actions 确认

---

## M2 — 单步 Sub-Agent 执行

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

## M3 — Cycleround 闭环 (MVP 完成)

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

## M4 — 状态持久化

**目标**：任务跨进程 / 重启可恢复，支持多实例。

**交付物**：Redis/Postgres 后端抽象 + 迁移脚本

**验收（可验证）**：
- [ ] kill 进程后重启，`RUNNING` 中断的 Task 自动恢复并继续
- [ ] `task:{id}:state` / `:history` / `:artifacts` 三类键在 Redis/PG 中可查
- [ ] 并发写入无冲突（带乐观锁/版本号）

---

## M5 — Gateway Shell + HTTP/CLI 触发

**目标**：标准化外部入口，与 IM 解耦。

**交付物**：`orcha-shell` HTTP server + `ShellAdapter` 接口 + 鉴权中间件

**验收（可验证）**：
- [ ] `POST /run`（携带 API Key）接收 `OrchaEvent`，返回 `event_id`
- [ ] `GET /status/{event_id}` 返回 `OrchaResponse`（`STREAMING | FINAL | ERROR`）
- [ ] 无 API Key 请求返回 `401`
- [ ] 至少一个 Adapter（CLI 适配器）端到端跑通
- [ ] `orcha shell list` 列出已注册 Adapter

---

## M6 — Slack Adapter

**目标**：真实 IM 接入验证 Shell 抽象。

**验收（可验证）**：
- [ ] Slack 频道 `@Orcha fix <issue>` 触发任务
- [ ] 流式进度与最终响应回传到原频道
- [ ] 产物 / Artifact 链接在 Slack 中可点击展开
- [ ] 私有频道权限校验生效

---

## M7 — Plugin 子代理体系

**目标**：第三方可注册自定义 Sub-Agent。

**验收（可验证）**：
- [ ] `orcha plugin add <path>` 注册成功并出现在 `orcha plugin list`
- [ ] Plugin 实现 `SubAgent` 接口即可被 Cycleround 调度
- [ ] Plugin 在沙箱内运行，无越权访问宿主文件系统
- [ ] Plugin 崩溃不影响 Core 稳定性（隔离验证）

---

## M8 — Self-Evolve

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
