# Orcha M7 部署拓扑

> 适用范围：M7（IM 接入与智能守护）后的 Gateway + Feishu Adapter 双进程部署。
> 与 [ROADMAP.md M7](./ROADMAP.md#m7--im-接入与智能守护) 对应。

---

## 1. 进程拓扑

```
┌─────────────────────────────────────────────────────────────────────┐
│                         飞书开放平台                                  │
│            (事件订阅 → POST /webhook/feishu)                         │
└──────────────────────────┬──────────────────────────────────────────┘
                           │ HTTPS（飞书回调）
                           ▼
┌─────────────────────────────────────────────────────────────────────┐
│                orcha-feishu-adapter（Node.js 进程）                  │
│                                                                     │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────────┐   │
│  │ webhook srv  │───▶│ 触发解析      │───▶│ IpcClient            │   │
│  │ :7099        │    │ @_user_1 →    │    │ (JSON line + 心跳)   │   │
│  │ /webhook/    │    │ @Orcha fix.. │    │                      │   │
│  │ feishu       │    └──────────────┘    └─────────┬────────────┘   │
│  └──────────────┘                                   │                │
│         ▲                                            │                │
│         │ MockFeishuClient / HttpFeishuClient        │                │
│         │ （回推卡片 / 通知 / 结果）                   │                │
│         │                                            │                │
│  ┌──────┴───────────────────────────────────────────┴──────────┐    │
│  │              IPC ↔ Feishu 路由层（main.ts）                 │    │
│  │  webhook onTrigger → IPC Trigger                            │    │
│  │  IPC CardUpdate/Notify/TaskResult → FeishuClient.pushXxx    │    │
│  └────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────┬───────────────────────────────┘
                                      │ TCP / Unix Socket（JSON line）
                                      ▼
┌─────────────────────────────────────────────────────────────────────┐
│                   orcha-gateway（Rust 进程）                         │
│                                                                     │
│  ┌──────────────┐   ┌──────────────────┐   ┌────────────────────┐   │
│  │ accept_loop  │──▶│ handle_connection│──▶│ AdapterRegistry   │   │
│  │ (TCP/Unix    │   │ - reader 线程     │   │ (broadcast fanout)│   │
│  │  listener)   │   │ - writer 线程     │   │                    │   │
│  └──────────────┘   │ - watchdog        │   └─────────┬──────────┘   │
│                     │   35s timeout     │             │              │
│                     │   3 次未心跳断开  │             │              │
│                     └─────────┬────────┘             │              │
│                               │ Trigger               │              │
│                               ▼                       │              │
│                     ┌──────────────────┐              │              │
│                     │ Authenticator    │              │              │
│                     │ (白名单校验)      │              │              │
│                     └─────────┬────────┘              │              │
│                               │ AuthResult            │              │
│                               ▼                       │              │
│                     ┌──────────────────┐   fanout     │              │
│                     │ TaskQueue         │◀────────────┘              │
│                     │ (worker 线程)     │                            │
│                     │   run_with_history│                            │
│                     │   (LlmCycleround)│                            │
│                     └─────────┬────────┘                            │
│                               │                                      │
│                     ┌─────────▼──────────────────────────────┐      │
│                     │ TaskStore / HistoryStore / MemoryStore│      │
│                     │ (SQLite / FileTaskStore，feature gate)│      │
│                     └────────────────────────────────────────┘      │
└─────────────────────────────────────────────────────────────────────┘
                                      │
                                      ▼
                           ┌──────────────────────┐
                           │  LLM API             │
                           │  (OpenAI / DeepSeek) │
                           └──────────────────────┘
```

### 关键设计

- **一条 IPC 长连接**复用多个会话：用消息的 `session` 字段区分不同 IM 会话，Adapter 进程通常只需一个 `IpcClient` 实例
- **双向消息流**：Adapter 发 Trigger/Heartbeat，Gateway 发 CardUpdate/Notify/TaskResult/AuthResult/HeartbeatAck
- **watchdog**：Gateway reader 设 35s read timeout（略大于 Adapter 心拍间隔 30s），连续 3 次超时（约 105s 无心跳）断开连接
- **自动重连**：Adapter IPC 断开后指数退避重连（1s → 2s → 4s → ... → 30s 封顶）
- **无 LLM 也能启动**：Gateway `build_llm_client()` 在缺 key 时返回 `None`，进程正常起，任务触发后 worker 失败并广播 `Notify`

---

## 2. 启动顺序

正确顺序：**Gateway 先，Adapter 后**。Adapter 启动时会重连 Gateway，Gateway 不在也不崩。

### 2.1 启动 Gateway

```bash
# 1. 准备配置（默认读 $ORCHA_HOME/config.toml，不存在则用默认配置）
mkdir -p ~/.orcha
cat > ~/.orcha/config.toml <<'EOF'
home = "~/.orcha"
workers = 2

[ipc]
kind = "auto"          # auto: Linux/macOS 选 unix，Windows 选 tcp
port = 7422            # tcp 时用

[cycle]
max_rounds = 10
max_retries = 3
cool_down_secs = 60

[llm]
api_key_env = "ORCHA_LLM_API_KEY"   # key 从环境变量读，不写进 toml
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"

[[auth.whitelist]]
platform = "feishu"
user = "ou_your_open_id"            # 飞书 open_id（精确匹配）
group = "*"                          # * 表示任意群
EOF

# 2. 导出 LLM key（不写进 config.toml 防泄密）
export ORCHA_LLM_API_KEY="sk-..."

# 3. 启动 Gateway
cargo run -p orcha-gateway --release
# 或：./target/release/orcha-gateway --config /path/to/config.toml
```

### 2.2 启动 Feishu Adapter

```bash
cd packages/orcha-feishu-adapter
npm install && npm run build

# 1. 导出环境变量
export ORCHA_GATEWAY_ENDPOINT="tcp://127.0.0.1:7422"
#    # Unix socket（Linux/macOS，需与 Gateway 的 ipc.kind=unix 对应）：
#    # export ORCHA_GATEWAY_ENDPOINT="unix:///tmp/orcha.sock"
export ORCHA_ADAPTER_WEBHOOK_PORT=7099

# 2a. Mock 模式（本地开发，飞书动作用 console.log）
ORCHA_ADAPTER_MOCK=1 npm start

# 2b. 生产模式（M8 接入真实飞书 OpenAPI；M7 阶段方法抛 NotImplemented）
export ORCHA_FEISHU_APP_ID="cli_xxx"
export ORCHA_FEISHU_APP_SECRET="..."
ORCHA_ADAPTER_MOCK=0 node dist/main.js
```

### 2.3 飞书开放平台配置

1. 在飞书开放平台创建自建应用，开通 **机器人** 能力
2. **事件订阅** → 请求地址 `https://<你的公网域名>:7099/webhook/feishu`
3. 订阅事件 `im.message.receive_v1`（接收消息）
4. 添加 `im:message`、`im:chat` 权限
5. 发布版本，安装到企业

> M7 骨架**不做**飞书 `Encrypt Key` / `Verification Token` 校验，M8 接入生产时必须补上。

---

## 3. 健康检查

### 3.1 Gateway

M7 阶段 Gateway **没有 HTTP 健检查口**（M5 的 orcha-shell 是独立进程，可选）。健康检查靠：

| 检查项 | 命令 | 期望 |
|---|---|---|
| 进程存活 | `pgrep -f orcha-gateway` | 返回 PID |
| IPC 端口监听 | `ss -tlnp \| grep 7422`（Linux） / `netstat -ano \| findstr 7422`（Windows） | 端口 LISTEN |
| Unix socket 存在 | `ls -la /tmp/orcha.sock` | 文件存在 |
| 日志无 panic | `journalctl -u orcha-gateway -n 100` 或 `tail -f ~/.orcha/gateway.log` | 无 panic backtrace |

### 3.2 Feishu Adapter

| 检查项 | 命令 | 期望 |
|---|---|---|
| 进程存活 | `pgrep -f "node dist/main.js"` | 返回 PID |
| webhook 端口监听 | `curl -s http://127.0.0.1:7099/webhook/feishu` | 返回 `not found`（404，因为用 GET 而非 POST；说明端口活着） |
| IPC 已连接 | 看日志 `[adapter] IPC 已连接到 Gateway` | 出现该行 |
| 心跳活跃 | 看 Gateway 日志或 Adapter 日志（每 30s 一次 `heartbeat`） | 持续输出 |

### 3.3 端到端 smoke

```bash
# 模拟一次飞书事件回调（mock 模式下，Adapter 会调 MockFeishuClient.pushXxx 打日志）
curl -s -X POST http://127.0.0.1:7099/webhook/feishu \
  -H 'Content-Type: application/json' \
  -d '{
    "schema":"2.0",
    "header":{"event_type":"im.message.receive_v1"},
    "event":{
      "sender":{"sender_id":{"open_id":"ou_smoke"}},
      "message":{"chat_id":"oc_smoke","chat_type":"group","message_type":"text","content":"{\"text\":\"@_user_1 hello world\"}"}
    }
  }'
# 期望响应：ok
# Adapter 日志应出现：[adapter] 触发已发送: session=oc_smoke desc=hello world
# 若 Gateway 在线且 LLM key 有效，后续会看到 [mock][card] / [mock][result] 输出
```

---

## 4. 重启策略

### 4.1 systemd（Linux 推荐）

**Gateway**：`/etc/systemd/system/orcha-gateway.service`

```ini
[Unit]
Description=Orcha Gateway
After=network.target
Wants=network.target

[Service]
Type=simple
User=orcha
WorkingDirectory=/opt/orcha
Environment=ORCHA_HOME=/var/lib/orcha
Environment=ORCHA_LLM_API_KEY=sk-...        # 或用 EnvironmentFile=/etc/orcha/env
ExecStart=/opt/orcha/bin/orcha-gateway --config /etc/orcha/config.toml
Restart=on-failure
RestartSec=2s
# 防止雪崩：连续失败 5 次后拉长间隔
StartLimitBurst=5
StartLimitIntervalSec=60

# 优雅关闭：SIGTERM 后给 15s 写完当前任务
KillSignal=SIGTERM
TimeoutStopSec=15s

# 资源限制
LimitNOFILE=65536
MemoryMax=2G

[Install]
WantedBy=multi-user.target
```

**Adapter**：`/etc/systemd/system/orcha-feishu-adapter.service`

```ini
[Unit]
Description=Orcha Feishu Adapter
After=orcha-gateway.service
Wants=orcha-gateway.service          # 弱依赖：Gateway 挂了 Adapter 也起，靠自动重连

[Service]
Type=simple
User=orcha
WorkingDirectory=/opt/orcha/feishu-adapter
EnvironmentFile=/etc/orcha/feishu-adapter.env
ExecStart=/usr/bin/node dist/main.js
Restart=on-failure
RestartSec=2s
StartLimitBurst=5
StartLimitIntervalSec=60
KillSignal=SIGTERM
TimeoutStopSec=10s

[Install]
WantedBy=multi-user.target
```

`/etc/orcha/feishu-adapter.env`：

```
ORCHA_GATEWAY_ENDPOINT=tcp://127.0.0.1:7422
ORCHA_ADAPTER_WEBHOOK_PORT=7099
ORCHA_ADAPTER_MOCK=0
ORCHA_FEISHU_APP_ID=cli_xxx
ORCHA_FEISHU_APP_SECRET=...
```

### 4.2 Windows

用 **NSSM**（Non-Sucking Service Manager）注册成服务：

```powershell
nssm install OrchaGateway "C:\opt\orcha\bin\orcha-gateway.exe"
nssm set OrchaGateway AppParameters "--config C:\opt\orcha\config.toml"
nssm set OrchaGateway AppEnvironmentExtra "ORCHA_HOME=C:\var\orcha" "ORCHA_LLM_API_KEY=sk-..."
nssm set OrchaGateway AppStdout "C:\var\orcha\log\gateway.log"
nssm set OrchaGateway AppStderr "C:\var\orcha\log\gateway.err.log"
nssm set OrchaGateway AppStopMethodConsole 15000   # SIGTERM 等价，给 15s 优雅关闭
nssm set OrchaGateway AppRestartDelay 2000
nssm start OrchaGateway

nssm install OrchaFeishuAdapter "C:\Program Files\nodejs\node.exe"
nssm set OrchaFeishuAdapter AppParameters "C:\opt\orcha\feishu-adapter\dist\main.js"
nssm set OrchaFeishuAdapter AppDirectory "C:\opt\orcha\feishu-adapter"
nssm set OrchaFeishuAdapter AppEnvironmentExtra "ORCHA_GATEWAY_ENDPOINT=tcp://127.0.0.1:7422" "ORCHA_ADAPTER_WEBHOOK_PORT=7099" "ORCHA_ADAPTER_MOCK=0" "ORCHA_FEISHU_APP_ID=cli_xxx" "ORCHA_FEISHU_APP_SECRET=..."
nssm start OrchaFeishuAdapter
```

### 4.3 重启顺序

| 场景 | 顺序 | 说明 |
|---|---|---|
| 全部重启 | Gateway → Adapter | Gateway 起好后 Adapter 重连即可 |
| 只重启 Adapter | kill Adapter → systemd/nssm 自动拉起 | Gateway 无感，watchdog 会感知断连但任务不丢（worker 已 enqueue） |
| 只重启 Gateway | kill Gateway → 等 systemd 拉起 → Adapter 自动重连 | Adapter 短暂重连失败，指数退避，Gateway 恢复后自动连上；正在跑的 worker 任务会随 Gateway 进程一起终止（M7 单进程内存状态） |

---

## 5. Graceful Shutdown

### 5.1 Gateway

收到 `SIGTERM` / `SIGINT`（或 systemd `KillSignal`）时：

1. **停止 accept 新连接**：IPC listener 关闭，新 Adapter 连不上
2. **等待 worker 完成当前任务**：worker 线程从 mpsc 收到 `Shutdown` 后退出循环；正在跑的 `LlmCycleround::run_with_history` 不中断（同步阻塞调用，等它返回再退出）
3. **持久化状态**：TaskStore 已在每轮 round 后落盘，进程退出无数据丢失
4. **退出进程**

> M7 阶段 Gateway 的 graceful shutdown 实现**不完整**：`run()` 当前是 `blocking_serve`，主线程阻塞在 `accept_loop` 上，SIGTERM 信号需要靠 worker 线程的 `TaskMessage::Shutdown` 才能完整传递。M8 计划引入 signal handler + channel 关闭，做更精确的 drain。当前 systemd 配置 `TimeoutStopSec=15s` 给 worker 完成时间，超时 systemd 强制 SIGKILL。

### 5.2 Feishu Adapter

[main.ts](file:///workspace/packages/orcha-feishu-adapter/src/main.ts) 已实现完整 graceful shutdown：

1. 收到 `SIGTERM` / `SIGINT`
2. 设 `shuttingDown = true` 标记，二次信号直接强退（防卡死）
3. `webhookServer.close()`：停止接受新 HTTP 请求
4. `await ipcClient.stop()`：发 `FIN` 关闭 IPC 连接，等对端 close
5. `process.exit(0)`

实测端到端：从 SIGTERM 到 `已关闭` 日志约 100ms。

### 5.3 注意事项

- **任务不丢**：飞书 webhook 触发已入队 Gateway 的 mpsc，Adapter 重启不丢触发；但**正在 Adapter 内存里、还没 send 出去的触发会丢**（Adapter crash 时）——M7 接受这个折衷，M8 引入 Adapter 侧持久化队列
- **正在跑的 LLM 任务**：worker 跑 `run_with_history` 是同步阻塞，Gateway SIGTERM 后 worker 不会立即退出，systemd 等 `TimeoutStopSec` 超时后强杀；已落盘的 round 记录在 `history/{task_id}.jsonl`，下次启动可恢复
- **白名单**：白名单配置变更需重启 Gateway 才生效（M7 不支持热更新）

---

## 6. 资源与容量

| 维度 | 推荐值 | 说明 |
|---|---|---|
| Gateway 内存 | 256MB ~ 2G | SQLite + worker 线程；LLM 大响应会增加内存 |
| Adapter 内存 | 64MB ~ 128MB | Node 进程 + JSON buffer |
| Gateway 文件描述符 | ≥ 1024 | 每 Adapter 一条 IPC + SQLite + LLM HTTP |
| 磁盘（home 目录） | ≥ 1GB | SQLite + history jsonl + memory jsonl + session workspace |
| LLM API 并发 | 看供应商 | worker 数 = `workers`（默认 2），每 worker 串行调 LLM |

### 6.1 目录布局

```
~/.orcha/                           # home 目录（ORCHA_HOME）
├── config.toml                     # 配置
├── orcha.db                        # SQLite（启用 sqlite feature 时）
├── store/                          # FileTaskStore（未启用 sqlite 时）
│   └── {task_id}.json
├── history/
│   └── {task_id}.jsonl             # 每轮 round 记录
├── memory/
│   └── {task_id}.jsonl             # LLM 对话历史
└── sessions/
    └── {task_id}/                  # Cycleround workspace
        └── ...                     # 工作产物
```

---

## 7. 故障排查

| 现象 | 可能原因 | 排查 |
|---|---|---|
| Adapter 日志 `IPC 断开：connect ECONNREFUSED` | Gateway 未启动 / 端口不对 | `pgrep orcha-gateway` / `ss -tlnp \| grep 7422` |
| Adapter 日志 `socket error: connect ETIMEDOUT` | 防火墙 / 网络 | 检查 `ORCHA_GATEWAY_ENDPOINT` 与 Gateway `ipc.kind/port` 是否一致 |
| Gateway 日志 `heartbeat timeout, disconnecting` | Adapter 卡死或网络断 | Adapter 30s 心跳断 → 重启 Adapter |
| 任务触发后 `worker fails gracefully without llm client` | `ORCHA_LLM_API_KEY` 未导出或为空 | `echo $ORCHA_LLM_API_KEY` |
| 飞书 webhook 收不到回调 | 公网域名 / 事件订阅地址错误 | 飞书开放平台 → 事件订阅 → 看请求日志 |
| 飞书收到 401 / 403 | 白名单未通过 | 检查 `config.toml` 的 `auth.whitelist`，确认 `platform` / `user` 匹配 |
| 卡片不更新 | Adapter 用 `HttpFeishuClient`（M7 占位）抛 NotImplemented | 切回 `ORCHA_ADAPTER_MOCK=1` 或等 M8 实现 |

---

## 8. 平台差异

| 项 | Linux | macOS | Windows |
|---|---|---|---|
| IPC 默认（`auto`） | Unix socket | Unix socket | TCP |
| 编译 SQLite | 需 `libsqlite3-dev` 或用 `bundled` | 同 Linux | 用 `bundled` 源码编译 |
| 信号 | SIGTERM / SIGINT | 同 Linux | 无 SIGTERM；用 `taskkill /PID` 或 NSSM 的 AppStopMethodConsole |
| systemd | ✓ | 用 launchd | 用 NSSM |
| 路径分隔 | `/` | `/` | `\`（`config.toml` 里用 `\\` 或正斜杠） |

> **Windows MSVC 零 C 依赖**：未启用 `sqlite` feature 时，Orcha 不依赖任何 C 代码，纯 Rust 编译。启用 `sqlite` 后会拉 `rusqlite` + `libsqlite3-sys`（bundled 源码编译），仍零系统库依赖。

---

## 9. 验收清单

部署完成后，逐项打勾：

- [ ] Gateway 进程启动，无 panic
- [ ] Adapter 进程启动，日志出现 `IPC 已连接到 Gateway`
- [ ] Gateway 日志每 30s 收到一次 `heartbeat`
- [ ] `curl -X POST /webhook/feishu` 模拟触发，Adapter 日志出现 `触发已发送`
- [ ] Gateway 日志出现 worker 启动 / `LlmCycleround` 调用
- [ ] 任务结束后 Adapter 日志出现 `pushTaskResult`（mock 模式）或飞书群收到卡片
- [ ] `kill -TERM <adapter_pid>` 后 1s 内进程退出，systemd 拉起重连
- [ ] `kill -TERM <gateway_pid>` 后 systemd 拉起，Adapter 自动重连成功
- [ ] LLM key 缺失时 Gateway 仍能启动，任务触发后 worker 失败并广播 `Notify`
- [ ] 白名单外的用户触发被拒，收到 `AuthResult{allowed:false}`
