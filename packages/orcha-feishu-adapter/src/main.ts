/**
 * Orcha Feishu Adapter 入口。
 *
 * 启动流程：
 * 1. 读环境变量配置
 * 2. 构造 FeishuClient（mock 或 SDK 实现）
 * 3. 构造 IpcClient 连接 Gateway
 * 4. 启动飞书 webhook server（含 challenge 透传 + 签名校验，接收 @Orcha 触发）
 * 5. IPC ↔ Feishu 双向路由：
 *    - webhook onTrigger → IPC Trigger 消息
 *    - IPC CardUpdate/Notify/TaskResult → FeishuClient.pushXxx
 * 6. SIGTERM/SIGINT 优雅关闭：关 webhook + stop IPC + 退出
 *
 * 默认 mock 模式（`ORCHA_ADAPTER_MOCK=1` 或缺省），所有飞书动作用 console.log。
 * 生产模式需提供：
 *   - ORCHA_FEISHU_APP_ID + ORCHA_FEISHU_APP_SECRET（必填，调 OpenAPI）
 *   - ORCHA_FEISHU_VERIFICATION_TOKEN（可选，challenge 校验）
 *   - ORCHA_FEISHU_ENCRYPT_KEY（可选，事件签名校验）
 */

import { IpcClient, IpcClientOptions } from './ipc';
import {
  AdapterToGateway,
  GatewayToAdapter,
} from './protocol';
import {
  FeishuClient,
  FeishuEvent,
  HttpFeishuClient,
  MockFeishuClient,
  eventToTriggerSource,
  startWebhookServer,
} from './feishu';

// ============================================================
// 配置
// ============================================================

interface AdapterConfig {
  /** Gateway IPC 地址。 */
  gatewayEndpoint: string;
  /** 是否 mock 模式。 */
  mock: boolean;
  /** webhook server 监听端口。 */
  webhookPort: number;
  /** 飞书 app_id（非 mock 模式必填）。 */
  feishuAppId?: string;
  /** 飞书 app_secret（非 mock 模式必填）。 */
  feishuAppSecret?: string;
  /** 飞书 Verification Token（可选，challenge 校验）。 */
  feishuVerificationToken?: string;
  /** 飞书 Encrypt Key（可选，事件签名校验）。 */
  feishuEncryptKey?: string;
}

/** 从环境变量读配置。失败时抛错并退出。 */
function loadConfig(): AdapterConfig {
  const gatewayEndpoint = process.env.ORCHA_GATEWAY_ENDPOINT;
  if (!gatewayEndpoint) {
    console.error('缺少必填环境变量 ORCHA_GATEWAY_ENDPOINT（如 tcp://127.0.0.1:7422 或 unix:///tmp/orcha.sock）');
    process.exit(2);
  }

  const mock = process.env.ORCHA_ADAPTER_MOCK !== '0';
  const webhookPort = Number.parseInt(process.env.ORCHA_ADAPTER_WEBHOOK_PORT ?? '7099', 10);
  if (!Number.isFinite(webhookPort) || webhookPort <= 0 || webhookPort > 65535) {
    console.error(`ORCHA_ADAPTER_WEBHOOK_PORT 非法: ${process.env.ORCHA_ADAPTER_WEBHOOK_PORT}`);
    process.exit(2);
  }

  const feishuAppId = process.env.ORCHA_FEISHU_APP_ID;
  const feishuAppSecret = process.env.ORCHA_FEISHU_APP_SECRET;
  const feishuVerificationToken = process.env.ORCHA_FEISHU_VERIFICATION_TOKEN;
  const feishuEncryptKey = process.env.ORCHA_FEISHU_ENCRYPT_KEY;

  if (!mock) {
    if (!feishuAppId || !feishuAppSecret) {
      console.error('非 mock 模式必须提供 ORCHA_FEISHU_APP_ID 和 ORCHA_FEISHU_APP_SECRET');
      process.exit(2);
    }
  }

  return {
    gatewayEndpoint,
    mock,
    webhookPort,
    feishuAppId,
    feishuAppSecret,
    feishuVerificationToken,
    feishuEncryptKey,
  };
}

// ============================================================
// 主流程
// ============================================================

async function main(): Promise<void> {
  const cfg = loadConfig();
  console.log(`[adapter] 启动中 mock=${cfg.mock} gateway=${cfg.gatewayEndpoint} webhook_port=${cfg.webhookPort}`);

  const feishuClient = createFeishuClient(cfg);
  const ipcClient = createIpcClient(cfg, feishuClient);
  const webhookServer = startWebhookServer({
    port: cfg.webhookPort,
    handlers: {
      onTrigger: (event: FeishuEvent) => {
        onWebhookTrigger(event, ipcClient);
      },
    },
    verificationToken: cfg.feishuVerificationToken,
    encryptKey: cfg.feishuEncryptKey,
  });

  // 信号处理：优雅关闭
  let shuttingDown = false;
  const shutdown = async (signal: NodeJS.Signals) => {
    if (shuttingDown) {
      console.log(`[adapter] 已在关闭中，再次收到 ${signal}，强制退出`);
      process.exit(1);
    }
    shuttingDown = true;
    console.log(`[adapter] 收到 ${signal}，开始优雅关闭`);
    webhookServer.close();
    await ipcClient.stop();
    console.log('[adapter] 已关闭');
    process.exit(0);
  };
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);

  ipcClient.start();
  console.log(`[adapter] webhook server 监听 :${cfg.webhookPort}/webhook/feishu`);
  if (!cfg.mock) {
    const sigOn = cfg.feishuEncryptKey ? 'on' : 'off';
    const tokenOn = cfg.feishuVerificationToken ? 'on' : 'off';
    console.log(`[adapter] 签名校验=${sigOn} token校验=${tokenOn}`);
  }
  console.log('[adapter] 启动完成，等待飞书事件');
}

function createFeishuClient(cfg: AdapterConfig): FeishuClient {
  if (cfg.mock) {
    console.log('[adapter] 使用 MockFeishuClient（所有动作用 console.log）');
    return new MockFeishuClient();
  }
  console.log('[adapter] 使用 HttpFeishuClient（@larksuiteoapi/node-sdk，token 自动刷新）');
  return new HttpFeishuClient({
    appId: cfg.feishuAppId!,
    appSecret: cfg.feishuAppSecret!,
  });
}

function createIpcClient(cfg: AdapterConfig, feishuClient: FeishuClient): IpcClient {
  const opts: IpcClientOptions = {
    endpoint: cfg.gatewayEndpoint,
    heartbeatIntervalMs: 30_000,
    reconnectBaseMs: 1_000,
    reconnectMaxMs: 30_000,
  };
  const handlers = {
    onMessage: (msg: GatewayToAdapter) => onGatewayMessage(msg, feishuClient),
    onConnect: () => console.log('[adapter] IPC 已连接到 Gateway'),
    onDisconnect: (reason: string) => console.warn(`[adapter] IPC 断开：${reason}，准备重连`),
  };
  return new IpcClient(opts, handlers);
}

// ============================================================
// IPC ↔ Feishu 路由
// ============================================================

/** 飞书触发 → 转 IPC Trigger 消息。 */
function onWebhookTrigger(event: FeishuEvent, ipcClient: IpcClient): void {
  const source = eventToTriggerSource(event);
  // task_id 留空，让 Gateway 用 Task::generate_id() 生成
  const msg: AdapterToGateway = {
    type: 'trigger',
    task_id: '',
    description: event.description,
    session: event.session,
    source,
  };
  const ok = ipcClient.send(msg);
  if (!ok) {
    console.warn(`[adapter] IPC 断开，触发丢失: session=${event.session} desc=${event.description}`);
  } else {
    console.log(`[adapter] 触发已发送: session=${event.session} desc=${event.description}`);
  }
}

/** IPC 消息 → 飞书推送。 */
async function onGatewayMessage(
  msg: GatewayToAdapter,
  feishuClient: FeishuClient,
): Promise<void> {
  switch (msg.type) {
    case 'card_update':
      await feishuClient.pushCardUpdate(
        msg.session,
        msg.task_id,
        msg.phase,
        msg.detail,
        msg.progress,
      );
      return;
    case 'notify':
      await feishuClient.pushNotify(
        msg.session,
        msg.task_id,
        msg.level,
        msg.message,
      );
      return;
    case 'task_result':
      await feishuClient.pushTaskResult(
        msg.session,
        msg.task_id,
        msg.outcome,
        msg.summary,
        msg.artifacts,
      );
      return;
    case 'auth_result':
      console.log(`[adapter] 鉴权结果: allowed=${msg.allowed} reason=${msg.reason ?? 'null'}`);
      return;
    case 'heartbeat_ack':
      // 心跳 ack 不做处理（仅在断开时由 watchdog 触发重连）
      return;
    default: {
      // exhaustiveness check：TS 在加新 case 时会编译报错
      const _exhaustive: never = msg;
      console.warn(`[adapter] 未知消息类型: ${JSON.stringify(_exhaustive)}`);
    }
  }
}

// 入口（直接调用 main，不 import 'main' 模式时跳过）
main().catch((err: unknown) => {
  console.error('[adapter] 致命错误:', err);
  process.exit(1);
});
