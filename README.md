# Orcha

> Let the Orcha play.

**English** · [简体中文](README.zh-CN.md)

Orcha is an AI-driven coding agent triggered from Feishu IM: @-mention the bot
with one sentence, and it autonomously plans, codes, tests, reviews, and pushes
the change to a new `orcha/*` git branch — no public URL, no manual context
switching.

---

## What it does

```
You @Orcha: append a reverse_string function at the end of utils/mod.rs

Orcha:
  1. Authenticates and enqueues the task
  2. AI autonomously schedules 6 Sub-Agents (Observer → Planner → Worker → Tester → Reviewer → Exit)
  3. Before Worker writes a file, push an approval card → you click [Approve]
  4. Tester runs `cargo test` to verify
  5. Commit the change and push to branch `orcha/add-reverse-string`
  6. Feishu card reports back: branch name + commit hash + commit message
```

## Core capabilities

- **Feishu trigger** — long-lived connection into Feishu; @-mention the bot to
  start a task, no public URL required.
- **AI-driven scheduling** — an LLM decides at every step which Sub-Agent to
  call next, when to exit, and how to recover from failure (max 10 rounds).
- **GitWorktree isolation** — each task edits code in its own worktree, the
  source repo is never touched.
- **Human-in-the-loop approval** — before writing a file, running a command, or
  deleting a file, Orcha pushes a Feishu approval card and waits for the
  button callback.
- **Auto branch landing** — on success the change is committed and pushed to a
  fresh `orcha/*` branch.
- **Circuit breakers** — max 10 rounds + 3 retries per step, no infinite loops.

---

## Architecture — AiDriven + 7 layers

Orcha is a multi-process ensemble. The **AI-Driven Scheduler** (Layer 4) is the
brain: an LLM picks the next Sub-Agent at every step instead of following a
hard-coded pipeline. The 7 layers below isolate concerns from the IM edge down
to persistent storage.

```
┌──────────────────────────────────────────────────────────────────────────┐
│ L1  IM Platform Layer        Feishu / QQ (official SDK long connection)  │
│     ── user @-mentions bot, events flow in over WebSocket ────────────── │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ WS events (im.message.receive_v1,
                                   │   card.action.trigger)
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L2  Adapter Layer            orcha-feishu-adapter (Node.js / TS process) │
│     ── fork of larksuite/openclaw-lark; protocol translation & reconnect │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ IPC (Unix Socket / Named Pipe / TCP,
                                   │   JSON-line + 30s heartbeat)
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L3  Gateway Layer            orcha-gateway (Rust process, first-class)   │
│     auth · whitelist · task queue · worker pool · approval orchestrator  │
│     watchdog: 35s read timeout, 3 misses → disconnect                    │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ Trigger + RoundEvent stream
                                   │   (mpsc::Receiver<RoundEvent>)
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L4  AI-Driven Scheduler      AiDrivenCycleround  ◀── the brain           │
│     ┌──────────────────────────────────────────────────────────────────┐ │
│     │  scheduler LLM sees task + all prior StepResult summaries         │ │
│     │  → decides next agent via `decide_next_agent` tool call           │ │
│     │  → calls SubAgent.run() → feeds result back → decides again       │ │
│     │  → emits `exit` when QA passes                                    │ │
│     │  circuit breakers: max_rounds (total decision steps)              │ │
│     │                   max_retries (same-agent consecutive failures)   │ │
│     └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ invokes one of
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L5  Sub-Agent Layer          Observer · Planner · Worker · Tester ·      │
│                              Reviewer · Fixer · Exit                     │
│     each implements SubAgent { name, run(&StepContext) -> StepOutput }   │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ all fs / exec goes through
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L6  Sandbox & Tool Layer     GitWorktree · FsSandbox · PathGuard ·       │
│                              ApprovalHook · Audit log                    │
│     PathGuard: canonicalize + prefix-check, deny .git/.env/*.key,        │
│                 1MB read cap, binary detection                           │
│     ApprovalHook: WriteFile / DeleteFile / RunCommand → Feishu card      │
└──────────────────────────────────┬───────────────────────────────────────┘
                                   │ persist
┌──────────────────────────────────▼───────────────────────────────────────┐
│ L7  Storage & History Layer  SQLite (WAL) · History JSONL · Memory JSONL │
│                              TaskStore · HistoryStore · MemoryStore      │
│     Gateway = sole writer, Shell = read-only connection                  │
└──────────────────────────────────────────────────────────────────────────┘
```

### Why AI-driven (not a fixed pipeline)

The earlier `Cycleround` hard-coded `Observer → Planner → Worker → Tester →
Reviewer → Fixer`. `AiDrivenCycleround` (M7, feature-gated) hands scheduling
to an LLM:

```rust
loop {
    let decision = ai_decide_next_step(&task, &prior_steps); // { agent, reason }
    match decision.agent.as_str() {
        "observer" | "planner" | "worker" | "tester" |
        "reviewer" | "fixer" => agent.run(&ctx),
        "exit" => break,                                    // QA passed
        _ => /* unknown */,
    }
    if circuit_breaker_tripped() { break; }
}
```

Because the scheduler sees **all prior step summaries** every iteration, it
naturally avoids repeating the same mistake (solves the "Reviewer has no
cross-round memory" gap — AI cannot pass review just by rewriting the same
broken code in a different style).

### Layer responsibilities at a glance

| Layer | Crate / Process | Owns |
| :--- | :--- | :--- |
| L1 IM Platform | (external) Feishu / QQ | user-facing messaging |
| L2 Adapter | `orcha-feishu-adapter` (TS) | WS reconnect, event parse, IPC client |
| L3 Gateway | `orcha-gateway` (Rust) | auth, queue, worker pool, approval, IPC server |
| L4 AI Scheduler | `orcha-core` `AiDrivenCycleround` | per-step LLM decision, circuit breakers, event stream |
| L5 Sub-Agent | `orcha-core` `sub_agents` / `llm_agents` | Observer/Planner/Worker/Tester/Reviewer/Fixer |
| L6 Sandbox & Tool | `orcha-core` `sandbox` / `path_guard` / `approval` | worktree, path safety, human approval, audit |
| L7 Storage | `orcha-core` `store` / `history` / `memory` | SQLite + JSONL persistence |

---

## Repository layout

```
packages/
├── orcha-sdk/             Data models (Task / Artifact / Event / Step)
├── orcha-core/            Brain: Cycleround + Sub-Agent + GitWorktree (L4–L7)
├── orcha-llm/             LLM client (OpenAI-compatible, with tool calling)
├── orcha-gateway/         Gateway: IPC server + auth + task queue + approval (L3)
├── orcha-shell/           Web UI + HTTP API (visualization panel, read-only)
├── orcha-cli/             CLI entry (init / fix / shell / history)
└── orcha-feishu-adapter/  Feishu adapter (TypeScript, long connection + IPC) (L2)
scripts/                   start.ps1 / stop.ps1 / dev-env.ps1.example
docs/                      ROADMAP.md / SPEC.md / ARCHITECTURE_ANALYSIS.md
```

---

## Quick start

### Prerequisites

- Rust 1.75+
- Node.js 20+
- Git
- PowerShell 5.1+ (Linux/macOS: see [docs/LOCAL_RUN.md](docs/LOCAL_RUN.md))

### 1. Configure

```powershell
copy scripts\dev-env.ps1.example scripts\dev-env.ps1
notepad scripts\dev-env.ps1
```

Fill in the LLM API key and Feishu credentials:

```powershell
$env:ORCHA_LLM_API_KEY       = "sk-..."
$env:ORCHA_FEISHU_APP_ID     = "cli_xxx"
$env:ORCHA_FEISHU_APP_SECRET = "..."
$env:ORCHA_ADAPTER_MOCK      = "0"   # 0 = real Feishu
```

### 2. Feishu app

1. Feishu Open Platform → create an enterprise self-built app
2. Event subscription → choose "Use long connection to receive events" (no public URL)
3. Subscribe to: `im.message.receive_v1` + `card.action.trigger`
4. Permissions: `im:message` / `im:message:send_as_bot` / `im:chat:readonly`
5. Publish the app and add it to a group chat

### 3. Edit `config.toml`

Edit `.orcha/config.toml`, key entries:

```toml
[llm]
api_key_env = "ORCHA_LLM_API_KEY"
base_url    = "https://api.deepseek.com/v1"
model       = "deepseek-chat"

[[auth.whitelist]]
platform = "feishu"
group    = "*"             # wildcard for dev; use a real chat_id in production

[workspace]
repo      = "D:/YourRepo"  # default repo for Feishu-triggered tasks
worktree  = true

[approval]
timeout_secs = 1800        # 30-minute approval timeout
```

Full config reference: [docs/SPEC.md](docs/SPEC.md).

### 4. Start

```powershell
.\scripts\start.ps1
```

### 5. Send a task

In a Feishu group or DM, @-mention Orcha:

```
@Orcha append a reverse_string function at the end of utils/mod.rs
```

On completion the Feishu card shows:

```
✅ Done
7 rounds, branch: orcha/add-reverse-string (ed4fc73)

feat(utils): add reverse_string function
```

### 6. Inspect the result

```powershell
git -C D:/YourRepo branch --list 'orcha/*'
git -C D:/YourRepo show orcha/add-reverse-string
```

---

## Operations

```powershell
# Tail logs
Get-Content D:\orcha\logs\gateway.log.err -Wait -Tail 20 -Encoding UTF8
Get-Content D:\orcha\logs\adapter.log     -Wait -Tail 20 -Encoding UTF8

# Stop / restart
.\scripts\stop.ps1
.\scripts\start.ps1 -Restart
```

## Development

```powershell
cargo fmt --all
cargo test                              # whole workspace
cargo clippy --all-targets -- -D warnings
```

Feature flags:

- **`llm`** — opt-in for `orcha-core`, default-on for `orcha-gateway`. Enables
  `LlmPlanner` / `LlmWorker` / `LlmReviewer` / `AiDrivenCycleround`. Without
  it, all agents are deterministic (no network) — CI-safe.
- **`sqlite`** — opt-in. Swaps `FileTaskStore` for `SqliteTaskStore` (rusqlite
  with bundled SQLite, WAL mode + cross-process locking).

## Documentation

- [docs/SPEC.md](docs/SPEC.md) — system specification (v0.1)
- [docs/ROADMAP.md](docs/ROADMAP.md) — M0 → M9 roadmap
- [docs/ARCHITECTURE_ANALYSIS.md](docs/ARCHITECTURE_ANALYSIS.md) — architecture analysis
- [docs/DEPLOYMENT_TOPOLOGY.md](docs/DEPLOYMENT_TOPOLOGY.md) — deployment topology
- [docs/LOCAL_RUN.md](docs/LOCAL_RUN.md) — local run guide

## Milestones

M0–M7 are complete, M8 Plugin system is in progress. See
[docs/ROADMAP.md](docs/ROADMAP.md).

## License

MIT
