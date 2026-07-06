# Orcha 本地运行配置指南

> 面向开发者本地起 Orcha 全栈（Gateway + Feishu Adapter）跑通一次任务。
> 生产部署（systemd / NSSM / 反向代理 / TLS）见 [DEPLOYMENT_TOPOLOGY.md](./DEPLOYMENT_TOPOLOGY.md)。

---

## 0. 前置依赖

| 工具 | 最低版本 | 验证命令 | 用途 |
|---|---|---|---|
| Rust | 1.75（MSRV） | `rustc --version` | 编译所有 Rust crate |
| Cargo | 随 Rust | `cargo --version` | 构建管理 |
| Node.js | 20 | `node --version` | 跑 Feishu Adapter |
| npm | 10 | `npm --version` | 装 Adapter 依赖 |
| Python | 3.x（可选） | `python3 --version` | Cycleround Tester Agent 跑 `test.py` 用 |

Rust 工具链建议用 [rustup](https://rustup.rs/) 管理。无需额外装 SQLite，`rusqlite` 默认 bundled 源码编译。

---

## 1. 拉代码 + 编译

```bash
git clone https://github.com/Ink-dark/orcha.git
cd orcha

# 编译所有 Rust crate（含 sqlite + llm feature）
cargo build --workspace --all-features

# 编译 Feishu Adapter TS
cd packages/orcha-feishu-adapter
npm install
npm run build          # 产物在 dist/
cd ../..
```

**编译验证**（全量测试 + clippy + fmt）：

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --all --check
cd packages/orcha-feishu-adapter && npx tsc --noEmit && cd ../..
```

---

## 2. `config.toml` 全字段详解

Gateway 启动时按以下顺序找配置文件：

1. `--config <path>` 命令行参数（最高优先级）
2. `$ORCHA_HOME/config.toml`（`ORCHA_HOME` 环境变量，默认 `.orcha`）
3. 文件不存在 → 用代码内默认配置（进程能起，但 IPC 端口、白名单等需后续手动配）

### 2.1 完整模板（拷贝即用）

```toml
# ~/.orcha/config.toml

home = "~/.orcha"          # Orcha 数据根目录（store/history/memory/sessions 都在这下面）
workers = 2                 # worker 线程数（M7 单 worker 串行，留 2 为 M8 池化预留）

[ipc]
kind = "auto"               # auto: Linux/macOS 用 Unix socket，Windows 用 TCP
                            # 显式： "unix" / "tcp"
port = 7422                 # kind=tcp 或 Windows 下用这个端口

[cycle]
max_rounds = 10             # Cycleround 最多跑几轮（超则 Failed(MaxRoundsExceeded)）
max_retries = 3             # 单步 Sub-Agent 失败重试次数
cool_down_secs = 60         # 熔断冷却时间（秒），连续失败后等这么久再试

[llm]
api_key_env = "ORCHA_LLM_API_KEY"   # 从哪个环境变量读 API key（值不写进 toml 防泄密）
base_url = "https://api.openai.com/v1"  # LLM API 基址
model = "gpt-4o"            # 模型名

# IM 凭证（M7 阶段可留空，Adapter 侧用环境变量配）
[im.feishu]
app_id_env = "ORCHA_FEISHU_APP_ID"
app_secret_env = "ORCHA_FEISHU_APP_SECRET"

# 鉴权白名单（至少配一条，否则所有触发被拒）
[[auth.whitelist]]
platform = "feishu"
user = "ou_your_open_id"    # 你的飞书 open_id（私聊白名单）
                            # 或留空 user，填 group 允许整个群
# group = "oc_your_chat_id"
```

### 2.2 字段速查

| 字段 | 默认值 | 说明 |
|---|---|---|
| `home` | `.orcha`（或 `$ORCHA_HOME`） | 数据根目录。`store/` `history/` `memory/` `sessions/` 都在它下面 |
| `workers` | `2` | worker 线程数。M7 实际只用 1 个，其余处于 idle |
| `ipc.kind` | `auto` | IPC 传输方式。`auto` / `unix` / `tcp` |
| `ipc.port` | `7422` | TCP 端口（kind=tcp 或 Windows 用） |
| `db_path` | `orcha.db` | SQLite 数据库路径（相对 `home`；绝对路径原样保留）。未启用 `sqlite` feature 时此项忽略，回退 FileTaskStore |
| `cycle.max_rounds` | `10` | Cycleround 最大轮数 |
| `cycle.max_retries` | `3` | Sub-Agent 重试次数 |
| `cycle.cool_down_secs` | `60` | 熔断冷却秒数 |
| `llm.api_key_env` | `ORCHA_LLM_API_KEY` | 读哪个环境变量拿 API key |
| `llm.base_url` | `https://api.openai.com/v1` | LLM API 基址 |
| `llm.model` | `gpt-4o` | 模型名 |
| `auth.whitelist` | `[]`（空，全拒） | 白名单条目数组 |

### 2.3 白名单匹配规则

参考 [auth.rs](../packages/orcha-gateway/src/auth.rs)：

- **群聊触发**（`group` 非空）：按 `(platform, group)` 匹配，群内**任何人** @Orcha 都允许
- **私聊触发**（`group` 为空）：按 `(platform, user)` 匹配，仅白名单用户私聊允许
- `platform` 必须精确匹配（`feishu` / `qq` / `slack`）
- 大小写敏感（飞书 ID 是定长字符串，无歧义）
- 白名单为空 → 所有触发被拒，Gateway 日志出现 `白名单为空`

**速配示例**：

```toml
# 允许飞书群 oc_xxx 里所有人触发
[[auth.whitelist]]
platform = "feishu"
group = "oc_xxxxxxxxxxxxxxxx"

# 允许飞书用户 ou_yyy 私聊触发
[[auth.whitelist]]
platform = "feishu"
user = "ou_yyyyyyyyyyyyyyyy"

# 允许任意飞书来源（* 通配，仅开发调试用）
[[auth.whitelist]]
platform = "feishu"
group = "*"
```

---

## 3. 环境变量清单

### 3.1 Gateway 进程

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `ORCHA_HOME` | 否 | `.orcha` | 数据根目录。`config.toml` 也从这里找 |
| `ORCHA_LLM_API_KEY` | 看场景 | — | LLM API key。**不设 → Gateway 仍能启动**，但任务触发后 worker 失败并广播 `Notify`（CI smoke 友好） |
| `ORCHA_LLM_BASE_URL` | 否 | `https://api.openai.com/v1` | 覆盖 config.toml 的 `llm.base_url` |
| `ORCHA_LLM_MODEL` | 否 | `gpt-4o` | 覆盖 config.toml 的 `llm.model` |

### 3.2 Feishu Adapter 进程

| 变量 | 必填 | 默认 | 说明 |
|---|---|---|---|
| `ORCHA_GATEWAY_ENDPOINT` | 是 | — | Gateway IPC 地址。`tcp://host:port` 或 `unix:///path/to/sock` |
| `ORCHA_ADAPTER_WEBHOOK_PORT` | 否 | `7099` | webhook server 监听端口 |
| `ORCHA_ADAPTER_MOCK` | 否 | `1`（即默认 mock） | `1`=mock 模式（飞书动作用 console.log），`0`=生产模式（调飞书 OpenAPI，M8 实现） |
| `ORCHA_FEISHU_APP_ID` | mock=0 时必填 | — | 飞书自建应用 app_id |
| `ORCHA_FEISHU_APP_SECRET` | mock=0 时必填 | — | 飞书自建应用 app_secret |

---

## 4. 三种本地运行场景

### 场景 A：最小 smoke（验证进程能起）

**目标**：不接 LLM、不接飞书，验证 Gateway + Adapter 进程能起、IPC 能连、心跳正常。

```bash
# 终端 1：起 Gateway（无 LLM key，进程能起，任务触发后 worker 会优雅失败）
cd /path/to/orcha
cargo run -p orcha-gateway --release
# 日志应出现：IPC listening on ... / worker started

# 终端 2：起 Adapter（mock 模式）
cd /path/to/orcha/packages/orcha-feishu-adapter
export ORCHA_GATEWAY_ENDPOINT="tcp://127.0.0.1:7422"
export ORCHA_ADAPTER_WEBHOOK_PORT=7099
node dist/main.js
# 日志应出现：
#   [adapter] 使用 MockFeishuClient
#   [adapter] IPC 已连接到 Gateway
#   [adapter] webhook server 监听 :7099/webhook/feishu
#   [adapter] 启动完成，等待飞书事件

# 终端 3：模拟一次飞书触发（不真实调 LLM，看链路）
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
# Adapter 日志：[adapter] 触发已发送: session=oc_smoke desc=hello world
# Gateway 日志：worker 启动 / 任务失败（无 LLM key）/ Adapter 收到 pushTaskResult
```

> 此时白名单未配置（默认空），Gateway 会拒绝触发。给 `~/.orcha/config.toml` 加：
> ```toml
> [[auth.whitelist]]
> platform = "feishu"
> group = "*"
> ```
> 然后重启 Gateway。

### 场景 B：接 LLM 跑真实任务

**目标**：用真实 LLM（OpenAI / DeepSeek / Ollama）跑一次 `@Orcha fix xxx`，看完整 Cycleround 流程。

#### B1. 用 OpenAI

```bash
export ORCHA_LLM_API_KEY="sk-..."
# 其余用默认（base_url = api.openai.com/v1，model = gpt-4o）
cargo run -p orcha-gateway --release
```

#### B2. 用 DeepSeek

```bash
export ORCHA_LLM_API_KEY="sk-..."
export ORCHA_LLM_BASE_URL="https://api.deepseek.com/v1"
export ORCHA_LLM_MODEL="deepseek-chat"
cargo run -p orcha-gateway --release
```

> 也可以把这三项写进 `~/.orcha/config.toml` 的 `[llm]` 段，但 `api_key` **永远不写进 toml**，只通过 `api_key_env` 指定的环境变量传。

#### B3. 用本地 Ollama

```bash
# 先启动 ollama 服务（默认 :11434）
ollama serve &
ollama pull llama3

# 配 Orcha
export ORCHA_LLM_API_KEY="ollama"   # Ollama 不校验 key，随便填非空
export ORCHA_LLM_BASE_URL="http://localhost:11434/v1"
export ORCHA_LLM_MODEL="llama3"
cargo run -p orcha-gateway --release
```

#### B4. 触发任务

```bash
# 起 Adapter（mock 模式即可，看 console.log 输出 CardUpdate/TaskResult）
cd packages/orcha-feishu-adapter
export ORCHA_GATEWAY_ENDPOINT="tcp://127.0.0.1:7422"
node dist/main.js &

# 模拟触发
curl -s -X POST http://127.0.0.1:7099/webhook/feishu \
  -H 'Content-Type: application/json' \
  -d '{
    "schema":"2.0",
    "header":{"event_type":"im.message.receive_v1"},
    "event":{
      "sender":{"sender_id":{"open_id":"ou_smoke"}},
      "message":{"chat_id":"oc_smoke","chat_type":"group","message_type":"text","content":"{\"text\":\"@_user_1 在 README.md 末尾加一行 hello\"}"}
    }
  }'
```

Adapter 日志会按顺序出现（mock 模式 console.log）：

```
[mock][card] phase=🚀 任务启动 progress=5
[mock][card] phase=🔍 观察中 progress=20
[mock][card] phase=📋 规划中 progress=40
[mock][card] phase=💻 写代码中 progress=60
[mock][card] phase=🧪 测试中 progress=80
[mock][card] phase=✅ 完成 progress=100
[mock][result] outcome=success summary=完成
```

产物落在 `~/.orcha/sessions/{task_id}/`。

### 场景 C：接真实飞书

**目标**：飞书群里 @机器人 真实触发任务，结果回推到飞书卡片。

> ⚠️ M7 阶段 `HttpFeishuClient` 是占位骨架（`pushCardUpdate` 等方法抛 `NotImplemented`）。完整飞书卡片推送需 M8 实现。本场景只能验证「飞书 webhook → Adapter → Gateway → Cycleround」前半链路，**回推到飞书卡片会失败**。

#### C1. 飞书开放平台配置

1. 登录 [飞书开放平台](https://open.feishu.cn/) → 创建**自建应用**
2. **应用能力** → 开通「机器人」
3. **权限管理** → 添加：
   - `im:message`（接收消息）
   - `im:chat`（获取群信息）
   - `im:message:send_as_bot`（发消息，M8 用）
4. **事件订阅**：
   - 请求地址：`http://<你的公网 IP>:7099/webhook/feishu`
   - 订阅事件：`im.message.receive_v1`
   - M7 骨架**不做** Encrypt Key / Verification Token 校验，可留空
5. **版本管理** → 创建版本 → 申请发布 → 安装到企业

#### C2. 本地内网穿透（无公网 IP 时）

飞书事件订阅需要公网可达的 URL。本地开发用 ngrok / cloudflared 暴露：

```bash
# 方式 1：ngrok
ngrok http 7099
# 拿到 https://xxx.ngrok.io，填到飞书事件订阅请求地址

# 方式 2：cloudflared（免费，无需注册）
cloudflared tunnel --url http://localhost:7099
```

#### C3. 启动 + 测试

```bash
# 终端 1：Gateway（接 LLM）
export ORCHA_LLM_API_KEY="sk-..."
cargo run -p orcha-gateway --release

# 终端 2：内网穿透（暴露 7099）
cloudflared tunnel --url http://localhost:7099 &

# 终端 3：Adapter（生产模式，但 M7 会抛 NotImplemented）
cd packages/orcha-feishu-adapter
export ORCHA_GATEWAY_ENDPOINT="tcp://127.0.0.1:7422"
export ORCHA_ADAPTER_WEBHOOK_PORT=7099
export ORCHA_ADAPTER_MOCK=0
export ORCHA_FEISHU_APP_ID="cli_xxx"
export ORCHA_FEISHU_APP_SECRET="..."
node dist/main.js

# 在飞书群里 @机器人 发消息：@Orcha 在 README.md 末尾加一行 hello
```

链路验证（M7 能跑通的部分）：

| 阶段 | 状态 |
|---|---|
| 飞书 → Adapter webhook | ✅ 收到事件 |
| Adapter 解析 `@_user_1` → `@Orcha` | ✅ 还原 |
| Adapter → Gateway Trigger | ✅ 发送 |
| Gateway 鉴权 + worker 启动 | ✅ |
| Cycleround 跑完 | ✅ |
| Gateway → Adapter CardUpdate/TaskResult | ✅ |
| Adapter → 飞书 patch 卡片 | ❌ M7 抛 NotImplemented（M8 实现） |

> 看 Adapter 日志的 `[adapter] 鉴权结果` 行确认白名单是否通过；看 `[adapter] 致命错误` 行确认是否触发了 NotImplemented。

---

## 5. 目录布局

跑过几次任务后，`$ORCHA_HOME`（默认 `~/.orcha`）会长这样：

```
~/.orcha/
├── config.toml                     # 你的配置
├── orcha.db                        # SQLite（启用 sqlite feature 时）
├── store/                          # FileTaskStore（未启用 sqlite 时）
│   └── {task_id}.json
├── history/
│   └── {task_id}.jsonl             # 每轮 round 记录（耗时/token/产物引用）
├── memory/
│   └── {task_id}.jsonl             # LLM 对话历史（planner/worker/reviewer 各 agent）
└── sessions/
    └── {task_id}/                  # Cycleround workspace，工作产物在这里
        ├── README.md              # 修改后的文件
        └── ...
```

**清理重来**：

```bash
rm -rf ~/.orcha/store/* ~/.orcha/history/* ~/.orcha/memory/* ~/.orcha/sessions/*
rm -f ~/.orcha/orcha.db     # SQLite 也清掉
```

---

## 6. 常见问题

### Q1: Gateway 启动报 `ORCHA_LLM_API_KEY 未设置`？

Gateway 进程本身**不需要**这个 key 也能启动（CI smoke test 友好）。如果你看到这个错误，可能是：
- 跑了某个测试（测试代码自己 set/unset，已修，见 commit `f45a0a1`）
- 或在 worker 真正跑任务时（这时确实需要 key，没 key 任务会失败并广播 `Notify`）

如果只是想验证进程能起，**不需要**设 key。

### Q2: Adapter 日志一直 `IPC 断开：connect ECONNREFUSED`？

Gateway 没起 / 端口不对。检查：

```bash
# Gateway 进程是否在
pgrep -f orcha-gateway

# 端口监听（Linux）
ss -tlnp | grep 7422

# Windows
netstat -ano | findstr 7422

# 确认 ORCHA_GATEWAY_ENDPOINT 与 config.toml 的 ipc.kind/port 一致
echo $ORCHA_GATEWAY_ENDPOINT
# 应为 tcp://127.0.0.1:7422 或对应 unix:///path
```

### Q3: 飞书 webhook 收不到回调？

1. 飞书开放平台 → 事件订阅 → 看请求日志，是否有 4xx/5xx
2. 内网穿透是否正常：`curl -X POST https://xxx.ngrok.io/webhook/feishu -d '{}'`，应返回 `not found`（404，说明端口活着）
3. 飞书消息内容是否带 `@_user_1`：飞书 content 里的 @ 实际是 `@_user_N` 占位，Adapter 会还原成 `@Orcha`；如果不是 `@Orcha fix ...` 开头，会被 `parseTrigger` 拒掉

### Q4: 任务触发后 Adapter 收到 `Notify{warn}` "AI 调度失败"？

worker 跑 `LlmCycleround` 时 LLM 调用失败。检查：

```bash
# 1. key 是否设了
echo $ORCHA_LLM_API_KEY

# 2. key 是否有效（手动调一次 API）
curl https://api.openai.com/v1/chat/completions \
  -H "Authorization: Bearer $ORCHA_LLM_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}]}'

# 3. 看 Gateway 日志的 LLM 错误详情（HttpStatus / Network / Parse）
```

### Q5: 触发被拒，Adapter 日志 `鉴权结果: allowed=false reason=白名单为空`？

`config.toml` 没配 `[[auth.whitelist]]` 或 whitelist 为空。加一条允许你的来源：

```toml
[[auth.whitelist]]
platform = "feishu"
user = "ou_你的_open_id"     # 飞书开放平台 → 个人资料 → open_id
```

然后重启 Gateway（白名单**不支持热更新**）。

### Q6: Gateway 日志 `heartbeat timeout, disconnecting`？

Adapter 30s 心跳断。原因：
- Adapter 进程卡死（看 Adapter 日志是否有异常）
- 网络抖动（Gateway watchdog 是 35s timeout × 3 次重试 = 约 105s 无心跳才断）
- 重启 Adapter 即可，Gateway 会自动重连

### Q7: 想看 LLM 实际请求/响应？

LLM 调用在 `packages/orcha-llm/src/client.rs` 的 `OpenAiCompatibleClient::chat`。临时加日志：

```rust
eprintln!("[llm] req: {} msgs", messages.len());
let resp = ... ;
eprintln!("[llm] resp: {}", content);
```

或开 `RUST_LOG=debug`（M7 未接 tracing，需 M8 引入）。

### Q8: 想完全重置环境？

```bash
# 删数据
rm -rf ~/.orcha/

# 删编译产物
cargo clean
rm -rf packages/orcha-feishu-adapter/dist/
rm -rf packages/orcha-feishu-adapter/node_modules/

# 重新编译 + 装依赖
cargo build --workspace --all-features
cd packages/orcha-feishu-adapter && npm install && npm run build
```

---

## 7. 快速验证清单

本地跑通后，逐项打勾确认：

- [ ] `cargo build --workspace --all-features` 无错误
- [ ] `cargo test --workspace --all-features` 全过
- [ ] Feishu Adapter `npx tsc --noEmit` 无错误
- [ ] Gateway 启动，无 panic
- [ ] Adapter 启动，日志出现 `IPC 已连接到 Gateway`
- [ ] Gateway 日志每 30s 出现 `heartbeat`
- [ ] `curl -X POST /webhook/feishu` 触发，Adapter 日志出现 `触发已发送`
- [ ] 配 LLM key 后，任务跑完出现 `pushTaskResult`
- [ ] `~/.orcha/sessions/{task_id}/` 下有工作产物
- [ ] `kill -TERM <adapter_pid>` 后 1s 内进程退出
- [ ] 重启 Adapter 后自动重连 Gateway

---

## 8. 下一步

- 生产部署（systemd / NSSM / TLS / 反向代理）：[DEPLOYMENT_TOPOLOGY.md](./DEPLOYMENT_TOPOLOGY.md)
- 架构设计与决策：[ARCHITECTURE_ANALYSIS.md](./ARCHITECTURE_ANALYSIS.md)
- 路线图与里程碑：[ROADMAP.md](./ROADMAP.md)
