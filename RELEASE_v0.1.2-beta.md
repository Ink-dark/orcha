# Orcha v0.1.2-beta

> 安全加固版本：按高危优先级修复 9 个安全 issue。

本次发版聚焦安全：4 个 CRITICAL + 5 个 HIGH，每个 issue 一个独立 commit，
commit message 均带工单号（`fix(#N)`）并附验证通过的证明。CI 双平台
（ubuntu-latest / windows-latest）fmt + clippy(`-D warnings`) + test 全绿。

---

## CRITICAL 修复

| # | 问题 | 修复 |
|---|------|------|
| #7 | 命令超时只 kill 主进程，孤儿子进程继续运行 | `run_with_timeout()` 用 `process_group(0)` + `killpg(SIGKILL)` 杀整个进程组 |
| #16 | HTTP server 无认证，任意本机进程可读写任务/记忆 | 可选访问令牌认证（Bearer / Cookie / `?token=`），`None` 向后兼容 |
| #20 | 运行时白名单只匹配 basename，批准 `src/main.rs` 会覆盖 `tests/main.rs` | 改用完整规范化相对路径匹配，杜绝同名文件越权 |
| #23 | CORS 设为 `*`，任意网站可跨域读取 API | 移除 `cors()` 与所有 `Access-Control-Allow-Origin` 响应头 |

## HIGH 修复

| # | 问题 | 修复 |
|---|------|------|
| #17 | Unix socket 默认 umask，同机其他用户可连入 | bind 后 `set_permissions(0o600)` |
| #22 | 审批 action_id 用时间戳+计数器，可推算伪造 | 改用 `uuid::Uuid::new_v4()`（CSPRNG） |
| #26 | IPC TCP 后端无认证，任意进程可注入伪造触发/审批 | TCP 共享密钥握手 `AUTH <secret>\n` + `constant_time_eq` |
| #24 | Feishu adapter 三个 Map 只增不减 → OOM | 新增 `TTLMap`（TTL 1h + LRU maxSize 5000）替换 |
| #27 | API 错误响应 `{"error":"{e}"}` 泄露内部细节 | 改返回 `{"error":"internal_error","correlation_id":"<uuid v4>"}` |

---

## 设计原则

- **向后兼容 Option 模式**：#16（auth token）、#26（tcp secret）均用 `Option<String>`，
  `None` 保留旧行为，未配置时打警告，不破坏现有部署/CI。
- **纵深防御**：Unix socket 用文件系统权限（#17）+ TCP 用共享密钥（#26），两条 IPC 路径都加锁。
- **fail-closed**：审批/白名单/认证任一环节失败都返回拒绝，不放行。
- **每个修复带回归测试**：新增测试覆盖修复点，保留全部既有测试。

---

## 验证

```
cargo test --workspace --features orcha-core/llm
orcha-core     223 passed
orcha-gateway   67 passed
orcha-llm       46 passed
orcha-shell     13 passed
orcha-cli       40 passed
orcha-sdk       14 passed
全部 0 failed
```

`cargo fmt --all -- --check` · `cargo clippy --workspace --features orcha-core/llm --all-targets -- -D warnings` → exit 0
TS：`npx tsc --noEmit` 无错误

---

## 升级须知

两个新增的可选配置（生产建议启用，dev/CI 不配也能跑）：

- **HTTP token**（#16）：`orcha shell --token <secret>` 或 `ORCHA_SHELL_TOKEN=<secret>`
- **IPC TCP secret**（#26）：`config.ipc.tcp_secret` 或 `ORCHA_IPC_TCP_SECRET=<secret>`

> 注：本次标签打在 `fix/security-high-priority` 分支（CI 已绿的 b07a683），
> 合并 PR #29 后 main 即包含全部修复。

---

**完整变更日志**：[CHANGELOG.md](CHANGELOG.md)
**PR**：https://github.com/Ink-dark/orcha/pull/29
