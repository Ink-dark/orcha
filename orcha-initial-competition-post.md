【标签】#学习工作

【标题】学习工作-Orcha —— 让 AI 自己闭环的编码副手

【正文】

## 1. Demo 简介

**是什么：** Orcha 是一个基于命令行的自动化编码操作系统，通过飞书 IM 长连接接入，用户在聊天窗口里 @Orcha 一句话发起编码任务，系统自动拆解、执行、测试、审核，把改动 commit 并推到 Git 新分支。后端为 Rust（Cargo workspace，7 个 crate），飞书接入层为 TypeScript 独立进程。Web UI 仅作为开发期观测面板，初赛交付主体是飞书 IM 交互。

**面向谁：** 中小团队的技术负责人、独立开发者、开源项目维护者——需要频繁处理"小而杂"的编码任务，但不想在工具切换和重复执行上浪费时间的人。

**主要功能：**

1. **飞书一句话触发 → AI 自主闭环** — @Orcha 发任务后，AI 自主调度 6 个 Sub-Agent（Observer → Planner → Worker → Tester → Reviewer → Exit），由 LLM 决定每一步调谁、何时结束、失败怎么重试（最多 10 轮 + 每步 3 次重试）。任务进行中飞书卡片持续 patch 更新进度（🚀 启动 → 🔍 观察中 → 📋 规划中 → 💻 写代码 → ✅ 完成）。

2. **写文件 / 跑命令 / 删文件前推飞书审批卡片** — Worker 在执行敏感操作前，经 GatewayApprovalHook 推审批卡片到飞书，用户点【批准】/【拒绝】按钮，回调走 `card.action.trigger` 事件回到 Gateway。三类操作有独立白名单，未配置即 fail-closed，超时 30 分钟自动拒绝。

3. **GitWorktree 隔离 + 自动落分支** — 每个任务在 `git worktree add` 创建的独立工作区改代码，原 repo 工作区不被污染。任务成功后由 LLM 生成 commit message 和 `orcha/{slug}` 分支名，自动 commit 并把改动持久化到原 repo 的新分支（worktree 清理后分支与 commit 不丢）。

> 截图位置：
> - 【占位：截图 1 - 飞书群 @Orcha 发起任务 + 卡片启动状态】
> - 【占位：截图 2 - 任务执行中卡片 patch 更新（含审批卡片按钮）】
> - 【占位：截图 3 - 任务完成卡片，显示分支名 + commit hash + commit message】

---

## 2. Demo 创作思路

**灵感来源：** 灵感来自交响乐团指挥——Orchestra。复杂乐章需要指挥协调不同乐手配合完成；同样，复杂编码任务也需要一个"指挥"来调度多个 AI Sub-Agent 分工协作，并通过循环验证不断修正。这就是 Cycleround 循环工作流的核心思路。

**想解决的问题：** 现有 AI 编码工具大多是"你说一句，我改一行"的被动模式，遇到复杂需求时开发者仍需自己拆解步骤、协调工具、反复验证。一个"小需求"也要经历：手动拆任务 → 打开 IDE → 写代码 → 跑测试 → 发现问题 → 再改再测 → 切回 IM 同步进度，实际写代码时间可能只占总时长的 30%。Orcha 想让开发者只需一句话描述目标，系统自动拆解、执行、验证、循环优化，直到真正完成。

**为什么做这个方向：** 判断与取舍有三点：

- **聚焦 IM 交互而非 Web UI**：开发者真正的工作入口是 IM（飞书 / Slack），不是另一个浏览器标签页。Roadmap 里明确 M5 Web UI 不纳入初赛验收，把所有精力压在 M7 IM 接入与智能守护上。
- **AI 驱动调度而非硬编码流水线**：早期确定性 Cycleround（Observer→Planner→...→Fixer 固定顺序）只能跑 demo，遇到真实 repo 就僵化。M7 P0 把调度权交给 LLM（`AiDrivenCycleround` + `decide_next_agent` tool calling），让 AI 自己决定下一步调谁。
- **声明式写边界 + 人工审批双保险**：让 LLM 自由写真实 repo 风险太高。M4 设计了 7 层写边界（路径规范化 / 写白名单 / action 权限 / 读保护 / 危险路径黑名单 / diff 范围校验 / workspace 隔离），M7 P1 又叠加飞书审批卡片人工确认，敏感操作必须人点【批准】才执行。

---

## 3. Demo 体验地址

由于 Orcha 是飞书 IM 机器人系统（长连接接入，无公网 URL），不适合部署"可公开访问的体验链接"或"交互式 HTML 文件"。采用演示视频方案：

**演示视频链接：** 【占位：请上传演示视频到第三方平台（B 站 / YouTube / 腾讯视频等）后在此填写公开链接】

> 视频建议内容：
> 1. 飞书群 @Orcha 发起任务 `在 utils/mod.rs 末尾追加 reverse_string 函数`
> 2. 飞书卡片实时更新（启动 → 执行中 → 审批卡片 → 完成）
> 3. 点【批准】按钮触发 Worker 写文件
> 4. 任务完成后卡片显示分支名 `orcha/add-reverse-string` + commit hash `ed4fc73`
> 5. 终端演示 `git -C D:/AdapterGit show orcha/add-reverse-string` 查看自动生成的 commit

---

## 4. TRAE 实践过程

### 4.1 完整开发流程

整个 Orcha 项目从创意到初赛交付，全程在 TRAE IDE 中完成，关键节点如下：

| 阶段 | 里程碑 | TRAE 完成内容 |
| :--- | :--- | :--- |
| 创意 | 报名 | 用 TRAE Work 生成完整创意产物 HTML（含系统架构、核心概念、网关规范、调度机制、安全策略、开发路线图） |
| M0-M3 | MVP 闭环 | 用 TRAE IDE 搭 Cargo workspace 骨架（7 crate）、数据模型（serde + schemars 15 个模型）、确定性 Cycleround（Observer/Planner/Worker/Tester/Reviewer/Fixer）、3 个 golden tasks 端到端验证 |
| M4 | 写边界 | 用 TRAE IDE 实现 `path_guard.rs`（路径规范化 + 逃逸防御）、`plan.rs`（Plan schema + search-and-replace 编辑引擎）、改造 LlmWorker / LlmReviewer 越界检查、7 条端到端测试 |
| M4 | 真实编辑能力 | 用 TRAE IDE 升级 Worker prompt 支持 `steps` 格式（edit/create/delete action）、新增 `WorkerStep` / `WorkerOutput` / `parse_worker_output_with_steps`、`apply_step` 实现 search 唯一匹配 + unified diff 生成 |
| M7 P0 | AI 驱动调度 | 用 TRAE IDE 实现 `ai_cycleround.rs`（把调度权交给 LLM via `decide_next_agent` tool calling）、8 单元测试、CLI 集成 `orcha fix --ai --llm` |
| M7 P1 | 审批管理 | 用 TRAE IDE 设计 `[approval]` 三类白名单（write/command/delete）、`GatewayApprovalHook` fail-closed 逻辑、飞书 `card.action.trigger` 事件处理、卡片状态 patch |
| M7 P1 | LLM 重试降级 | 用 TRAE IDE 给 `orcha-llm/client.rs` 加 `max_retries` / `retry_base_ms`、429/5xx 指数退避（1s→2s→4s，最多 3 次）、`config.rs` 同步字段 |
| 联调 | 真实仓库冒烟 | 用 TRAE IDE 在 `bit-torch/adaptergit` 真实仓库跑端到端冒烟（reverse_string 任务 2 轮成功），修复 3 个阻塞 bug：LLM JSON 输出带前导文本、Windows CRLF 行尾不匹配、Windows UNC 路径解析失败 |
| 联调 | 飞书端到端 | 用 TRAE IDE 完成飞书联调：鉴权白名单 `user = "*"`、卡片 schema 1.0 顶层 `elements`、订阅 `card.action.trigger`、`WorkspaceConfig` + GitWorktree 集成、心跳线程每 30s 推 CardUpdate |
| 验收 | 端到端任务 | 任务 T-6bbd2024 7 轮闭环成功，自动 commit 到分支 `orcha/add-reverse-string`（commit `ed4fc73`），commit message `feat(utils): add reverse_string function` 由 LLM 生成，作者 `Orcha Bot` |

### 4.2 开发关键步骤截图

> 截图位置（不少于 3 张）：
> - 【占位：截图 1 - TRAE IDE 中 `ai_cycleround.rs` 实现 AI 驱动调度的代码视图】
> - 【占位：截图 2 - TRAE IDE 中跑 `cargo test` 全绿（orcha-llm 25 测试 / orcha-core 217 测试 / clippy 0 警告）的终端输出】
> - 【占位：截图 3 - TRAE IDE 中飞书联调日志，显示 Gateway + Adapter 双进程启动 + 收到 @Orcha 触发】
> - 【占位：截图 4（可选）- TRAE IDE 中 `git show orcha/add-reverse-string` 显示 Orcha Bot 自动生成的 commit】

### 4.3 关键任务对话 Session ID

以下 Session ID 在 TRAE 中可验证作品由 TRAE 开发完成：

1. **Session ID：`6a4b5cb87d466d3b8b137eab`**
   - 覆盖内容：M4 写边界设计与实现、search/replace edit action、M7 P0 AI 驱动 Cycleround、M7 P1 审批管理与 LLM 重试降级、AdapterGit 真实仓库端到端冒烟测试、飞书联调全流程（鉴权 / 卡片 schema / card.action.trigger / GitWorktree 集成）、最终任务 T-6bbd2024 端到端验收
   - 关键产物：`ai_cycleround.rs`、`path_guard.rs`、`plan.rs`、`GatewayApprovalHook`、`orcha-llm/client.rs` 重试逻辑、`orcha-gateway/queue.rs` worktree 集成与心跳线程

2. **Session ID：`6a4b504c7d466d3b8b137c46`**
   - 覆盖内容：分析 Orcha 当前编码 Agent 距离可用状态还缺哪些能力，按优先级梳理 8 项缺失（tool calling / 文件编辑 / agent loop / 上下文预算 / LLM 重试 / 写边界等），并把 M4 写入 ROADMAP（含 P0/P1 优先级与 7 层写边界设计）
   - 关键产物：`docs/ROADMAP.md` M4 章节、后续 M4-M7 实施的优先级基线

3. **Session ID：** 【占位：第三个 Session ID 待补充——建议填写 M0-M3 骨架搭建阶段（Cargo workspace / 数据模型 / 确定性 Cycleround MVP）或创意产物 HTML 生成阶段的会话】

---

## 5. 通过的社区报名帖链接

【占位：请填写社区审核通过的报名帖公开链接】

> 报名帖原文件已存于仓库 `orcha-registration-post.md`，标题为「学习工作-造个新解法 | Orcha —— 让 AI 自己闭环的编码副手」，标签 #学习工作。

---

## 附：经验总结与开发心得

**踩过的坑（已记录在 project_memory）：**

1. **LLM JSON 输出带前导文本**：DeepSeek 偶尔在 JSON 前输出解释性文字导致解析失败。修复方式：新增 `extract_json_object` 兜底提取第一个 `{...}` 块。
2. **Windows CRLF 行尾不匹配**：search-and-replace 在 Windows 上经常匹配失败。修复方式：`apply_edit` 匹配前归一化为 LF，写入时保留原始行尾风格。
3. **Windows UNC 路径解析失败**：`file:///\?:..` 类 URL 解析炸了。修复方式：Reviewer / `extract_files` 先剥 `file:///` 再剥 `file://` 前缀。
4. **飞书卡片 230099 错误**：卡片更新失败。修复方式：所有卡片（CardUpdate / ApprovalRequest / ApprovalResult）统一用 schema 1.0 顶层 `elements` 字段。
5. **空目录执行 MaxRoundsExceeded**：未配 `[workspace]` 时 LLM 在空目录里瞎转。修复方式：`config.toml` 强制要求 `[workspace] repo` + `worktree = true`，`queue.rs` 用 `GitWorktree` 隔离执行。
6. **审批粒度不足**：早期复用 `auth.whitelist` 导致审批粒度不够。修复方式：拆出独立 `[approval]` 段，write / command / delete 三类独立白名单，fail-closed 语义。

**架构心得：**

- **Rust 业务层 + TS UI/接入层**的分层在飞书场景下很舒服：Rust 吃 LLM 调用 / 状态机 / Git 操作的复杂度，TS 吃飞书 SDK 的 WebSocket 长连接 / 卡片渲染，IPC 用 JSON line 协议解耦。
- **GitWorktree 是 LLM 改真实 repo 的必备隔离**：即便 LLM 乱写，原 repo 工作区不被污染，回滚只需 `git worktree remove`。任务成功后 commit 写入原 repo 对象库，worktree 清理后分支与 commit 都不丢。
- **AI 驱动调度比硬编码流水线灵活得多**：早期确定性 Cycleround 只能跑 demo，换成 `AiDrivenCycleround` 后 LLM 能根据上一轮 Tester 的 stderr 决定是回到 Worker 修代码还是直接 Exit，体感更像"一个真人在改代码"。
