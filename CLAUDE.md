# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test

```powershell
# Build all Rust crates
cargo build

# Run all tests (deterministic, no LLM dependency by default)
cargo test

# Run tests with LLM feature enabled
cargo test --features llm

# Lint
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings

# TypeScript adapter
cd packages/orcha-feishu-adapter && npm install && npx tsc --noEmit
```

## Architecture

Orcha is an AI coding agent triggered via Feishu IM: @ the bot with a task description → it autonomously plans, codes, tests, reviews, and pushes changes to a new `orcha/*` git branch. The system runs as a multi-process ensemble coordinated by the Gateway.

### Package Map (dependency order)

| Crate | Role |
|---|---|
| `orcha-sdk` | Shared data models: `Task`, `Step`, `StepResult`, `Artifact`, `OrchaEvent`, `OrchaResponse`. Serde + schemars, no logic. |
| `orcha-llm` | OpenAI-compatible LLM client (`LlmClient` trait + `OpenAiCompatibleClient`). Synchronous (ureq), supports tool calling, retry with exponential backoff. |
| `orcha-core` | The brain. `Cycleround` scheduler, `SubAgent` trait + 6 built-in agents (Observer/Planner/Worker/Tester/Reviewer/Fixer), AI-driven variant (`AiDrivenCycleround`), approval hooks, sandbox (`GitWorktree`/`FsSandbox`), path guard, audit logging. |
| `orcha-shell` | Web UI + JSON HTTP API on `tiny_http` (no async runtime). Frontend compiled in via `include_str!`. |
| `orcha-gateway` | Standalone daemon process. IPC server (Unix Socket / TCP / Named Pipe), auth/whitelist, task queue + worker pool, approval orchestration (Feishu card → button callback → channel back to worker). |
| `orcha-cli` | Binary crate (`orcha` command). Subcommands: `init`, `run`, `fix`, `status`, `list`, `shell`, `schema`. |
| `orcha-feishu-adapter` | TypeScript Node.js process. Feishu SDK long-lived WebSocket connection → IPC to Rust Gateway. |

### Feature Flags

- **`llm`** (opt-in for `orcha-core`, default-on for `orcha-gateway`): Enables `orcha-llm` dependency + `LlmPlanner`/`LlmWorker`/`LlmReviewer`/`AiDrivenCycleround`. Without it, all agents are deterministic (no network calls) — CI safe.
- **`sqlite`** (opt-in): Swaps `FileTaskStore` for `SqliteTaskStore` (rusqlite with bundled SQLite). WAL mode + cross-process locking.

### Core Loop (Cycleround)

The deterministic `Cycleround` (no LLM) runs a fixed pipeline each round:

```
Observer → Planner → Worker → Tester → Reviewer
```

- Any step failure skips remaining steps for that round.
- If Planner succeeded but later steps failed, `Fixer` runs at round-end to repair the workspace (e.g., create missing `test.py`).
- Circuit breakers: `max_rounds=10`, `max_retries=3`.
- `Cycleround::run_streaming()` sends `RoundEvent` variants via `mpsc::Receiver` for real-time progress.

The LLM-driven `AiDrivenCycleround` (M7, feature-gated) replaces the fixed order: a scheduler LLM decides at each step which agent to invoke next, sees all prior step results, and calls `exit` when done.

### Sub-Agent Trait

All agents implement `SubAgent`:
```rust
fn name(&self) -> &'static str;
fn run(&self, ctx: &StepContext) -> StepOutput;
```

`StepContext` carries: `workspace` (PathBuf), `task` (Task), `prior_artifacts`, `prior_steps`, and `approval` (Arc\<dyn ApprovalHook\>).

### Approval System (M7 P1)

Before Worker writes a file or Tester runs a command, they call `ctx.approval.request(&action)`. Flow:
1. Worker thread → `GatewayApprovalHook` → IPC broadcast to Feishu adapter → card with [批准]/[拒绝] buttons
2. Admin clicks → `card.action.trigger` → adapter sends `ApprovalResponse` → Gateway resolves the pending channel
3. fail-closed: timeout/error → `Rejected`

`ApprovalAction` variants: `WriteFile`, `DeleteFile`, `RunCommand`.

### GitWorktree Isolation

When `workspace.worktree = true` in config, each task creates a `git worktree add` in a temp dir. Changes are committed and pushed to `orcha/<desc-slug>` only on success. The original repo working tree is never touched.

### IPC Protocol

Gateway ↔ Adapter messages are defined in `orcha-gateway/src/protocol.rs`:
- `AdapterToGateway`: `Trigger`, `AuthCheck`, `Heartbeat`, `Reply`, `ApprovalResponse`
- `GatewayToAdapter`: `AuthResult`, `Notify`, `CardUpdate`, `TaskResult`, `ApprovalRequest`, `ApprovalResult`

Serialized as JSON lines over the stream. Watchdog: 35s read timeout, 3 consecutive misses → disconnect.

### Config

`.orcha/config.toml` loaded by Gateway at startup. Key sections: `[llm]`, `[[auth.whitelist]]`, `[workspace]`, `[approval]`, `[ipc]`. See `docs/SPEC.md` for full reference.

LLM credentials come from env vars: `ORCHA_LLM_API_KEY`, `ORCHA_LLM_BASE_URL`, `ORCHA_LLM_MODEL`.

### Path Guard & Safety

`PathGuard` enforces hard limits before any file operation: canonicalize-and-verify within workspace, deny `.git/`/`.env*`/`*.key`/dangerous paths, max file size for reads (1MB), binary file detection. This is separate from the approval hook (soft gate requiring human consent).

### Milestone Status

M0–M7 complete. M8 (Plugin system) and M9 (Self-Evolve) are next. See `docs/ROADMAP.md` for detailed acceptance criteria per milestone.
