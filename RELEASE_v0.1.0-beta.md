# Orcha v0.1.0-beta

> Let the Orcha play.

**92 次提交 · 4 天 · 从零到飞书联调闭环**

---

## 起源

2026 年 7 月 4 日，一行 `Initial commit`。

四天后的现在，Orcha 已经是一个能在飞书接到一句话任务、AI 自主拆解执行、经人工审批写代码跑测试、最后把改动推到 Git 分支的全链路系统。

这不是 demo，这是实打实接了飞书长连接、跑了真实 Rust 项目的 AI Agent。

---

## 核心能力

### 🤖 AI 自主调度
LLM 决定每一步调哪个 Agent（Observer / Planner / Worker / Tester / Reviewer / Fixer），不再硬编码流程。调度权交 AI。

### 📱 飞书一键触发
@Orcha 发一句话，长连接实时接入，无需公网 URL。Agent 执行进度实时推送到飞书卡片——🔍 观察中 → 📋 规划中 → 💻 写代码 → 🧪 测试中 → 👀 审核中。

### 🛡️ 人工审批
写文件、跑命令、删文件前推送审批卡片，管理员点击 [批准] / [拒绝] 按钮。PathGuard 硬限制 + 审批 Hook 软限制双层防护。

### 🌿 Git Worktree 隔离
每个任务在独立 worktree 执行，原 repo 不受污染。任务成功自动 commit 并推到 `orcha/*` 新分支。

### 🔧 完整的 Sub-Agent 体系
- **Observer** — 自动检测项目类型（Rust / Go / Python / JS 等），按模块分组报告
- **Planner** — 基于 workspace 现状生成结构化执行计划
- **Worker** — 只读探查 + 编辑已有文件 / 创建新文件 / 删除
- **Tester** — 自动检测测试框架，跑 cargo test / pytest / go test…
- **Reviewer** — 审核 Worker 产出 + diff scope 校验
- **Fixer** — 修复测试基础设施

### 🛡️ 熔断与恢复
- 最多 30 步决策 + 连续 3 次失败强换策略
- 命令执行 10 分钟超时保护
- panic 自动恢复，不丢 task 状态
- `/stop` 命令随时取消运行中任务

---

## 里程碑（M0 → M7）

| Milestone | 名称 | 状态 |
|-----------|------|:----:|
| M0 | 骨架与契约 | ✅ |
| M1 | Task 模型与本地 CLI | ✅ |
| M2 | 单步 Sub-Agent 执行 | ✅ |
| M3 | Cycleround 闭环 MVP | ✅ |
| M4 | 编码 Agent 核心能力 + 写边界安全 | ✅ |
| M5 | Web UI Shell + HTTP API | ✅ |
| M6 | Gateway 基础设施与跨进程通信 | ✅ |
| M7 | IM 接入与智能守护 | ✅ |

242 个测试全部通过，clippy 零警告。

---

## 技术栈

| 层 | 技术 |
|----|------|
| 核心调度 | Rust (orcha-core) |
| LLM 客户端 | Rust (orcha-llm)，OpenAI 兼容协议 |
| Gateway | Rust (orcha-gateway)，IPC + 任务队列 |
| CLI | Rust (orcha-cli) |
| Web UI | Rust (orcha-shell)，tiny_http |
| 飞书 Adapter | TypeScript (orcha-feishu-adapter)，飞书官方 SDK |

跨平台：Windows / Linux / macOS，IPC 支持 Unix Socket / Named Pipe / localhost TCP。

---

## 仓库

[https://github.com/Ink-dark/orcha](https://github.com/Ink-dark/orcha)

```bash
git clone https://github.com/Ink-dark/orcha.git
cd orcha
.\scripts\start.ps1
```

---

> 自动化 AI-Agent 未来已来。
