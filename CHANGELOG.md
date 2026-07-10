# Changelog

本项目所有重要变更记录于此。格式参考 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [Semantic Versioning](https://semver.org/lang/zh-CN/)。

beta / alpha 版本为预发布，API 与行为可能在正式版前调整。

---

## [v0.1.2-beta] - 2026-07-10

安全加固版本：按高危优先级修复 9 个安全 issue（4 CRITICAL + 5 HIGH）。
所有修复均有回归测试覆盖，CI 双平台（ubuntu/windows）fmt + clippy(-D warnings) + test 全绿。

### CRITICAL 修复

- **命令超时未杀子进程（#7）**：`execute_run_command` 超时只 kill 直接子进程，
  孙进程成为孤儿继续运行。改为 `process_group(0)` + `killpg(SIGKILL)` 杀整个进程组。
  新增测试验证孙进程确被杀死（`#[cfg(unix)]` 编译期门控，Windows 不编译）。
- **HTTP server 无认证（#16）**：任意本机进程可读写任务/记忆。新增可选访问令牌
  认证，支持 Bearer / Cookie / `?token=` 三种凭证；`None` 保持旧行为（向后兼容）。
  CLI 增加 `--token` / `ORCHA_SHELL_TOKEN`。
- **运行时白名单仅匹配 basename（#20）**：批准 `src/main.rs` 后 `tests/main.rs`、
  `vendor/main.rs` 等同名文件均命中白名单越权写/删。改为按完整规范化相对路径
  （统一分隔符、消去 `.`、解析 `..`）匹配。顺带清理同 crate 内 `queue.rs` 的
  clippy `useless_borrows_in_formatting` 预存 lint。
- **CORS 设为 `*`（#23）**：任意网站可跨域读取 API。移除 `cors()` 与所有
  `Access-Control-Allow-Origin: *` 响应头，回归默认同源策略。

### HIGH 修复

- **Unix socket 权限过松（#17）**：默认 umask（常 0755/0777），同机其他用户可连入。
  bind 后 `set_permissions(0o600)`，失败则清理 socket 并报错。
- **审批 action_id 可预测（#22）**：用 `SystemTime` 纳秒 + `AtomicU64` 计数器，
  可推算后伪造 `ApprovalResponse` 越权。改用 `uuid::Uuid::new_v4()`（CSPRNG）。
- **IPC TCP 无认证/加密（#26）**：任意本机进程可注入伪造触发/审批、读取全量流量。
  TCP 后端新增共享密钥握手：客户端首行 `AUTH <secret>\n`，服务端定长比较
  （`constant_time_eq` 防 timing attack）通过才进 JSON-line；Unix socket 靠 0600
  保护跳过握手。配置 `config.ipc.tcp_secret` 或 `ORCHA_IPC_TCP_SECRET` 环境变量。
- **Feishu adapter 无界内存增长（#24）**：三个 `Map`（cardMsgIds /
  approvalCardMsgIds / approvalActions）只增不减，超时审批/孤儿卡片永不清理 → OOM。
  新增 `TTLMap`（TTL 1h + LRU maxSize 5000 + 节流 cleanup）替换。
- **API 错误响应泄露内部细节（#27）**：`{"error":"{e}"}` 泄露文件路径/DB 路径/
  模块结构。改返回 `{"error":"internal_error","correlation_id":"<uuid v4>"}`，
  详细错误记服务端日志。

### CI / 工程改进

- release.yml 的 release body_path 从写死的 `RELEASE_v0.1.0-beta.md` 改为
  动态 `RELEASE_${{ github.ref_name }}.md`，避免每次发版都要改 workflow。

### 测试

完整 workspace 测试 `cargo test --workspace --features orcha-core/llm` 全绿：
orcha-core 223 · orcha-gateway 67 · orcha-llm 46 · orcha-shell 13 ·
orcha-cli 40 · orcha-sdk 14，共 400+ tests，0 failed。
TS 端 `tsc --noEmit` 无错误。

---

## [v0.1.0-beta] - 2026-07-04

首个预发布版本。92 次提交，从零到飞书联调闭环。

详见 [RELEASE_v0.1.0-beta.md](RELEASE_v0.1.0-beta.md)。
