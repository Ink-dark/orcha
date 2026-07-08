/**
 * Orcha Feishu Adapter 入口。
 *
 * 启动流程：
 * 1. 读环境变量配置
 * 2. 构造 FeishuClient（mock 或 SDK 实现）
 * 3. 构造 IpcClient 连接 Gateway
 * 4. 启动飞书 WebSocket 长连接（接收 @Orcha 触发；mock 模式跳过）
 * 5. IPC ↔ Feishu 双向路由：
 *    - 长连接 onTrigger → IPC Trigger 消息
 *    - IPC CardUpdate/Notify/TaskResult → FeishuClient.pushXxx
 * 6. SIGTERM/SIGINT 优雅关闭：关长连接 + stop IPC + 退出
 *
 * 默认 mock 模式（`ORCHA_ADAPTER_MOCK=1` 或缺省），所有飞书动作用 console.log，
 * 不启动长连接（避免无凭证时连飞书失败刷日志）。
 * 生产模式需提供：
 *   - ORCHA_FEISHU_APP_ID + ORCHA_FEISHU_APP_SECRET（必填）
 *   长连接模式下无需 Verification Token / Encrypt Key / 公网 URL / 内网穿透。
 */

import { IpcClient, IpcClientOptions } from './ipc';
import {
  AdapterToGateway,
  ApprovalResponseMsg,
  GatewayToAdapter,
} from './protocol';
import {
  FeishuClient,
  FeishuEvent,
  HttpFeishuClient,
  LongConnectionHandle,
  MockFeishuClient,
  eventToTriggerSource,
  startLongConnection,
} from './feishu';

// ============================================================
// 配置
// ============================================================

interface AdapterConfig {
  /** Gateway IPC 地址。 */
  gatewayEndpoint: string;
  /** 是否 mock 模式。 */
  mock: boolean;
  /** 飞书 app_id（非 mock 模式必填）。 */
  feishuAppId?: string;
  /** 飞书 app_secret（非 mock 模式必填）。 */
  feishuAppSecret?: string;
}

/** 从环境变量读配置。失败时抛错并退出。 */
function loadConfig(): AdapterConfig {
  const gatewayEndpoint = process.env.ORCHA_GATEWAY_ENDPOINT;
  if (!gatewayEndpoint) {
    console.error('缺少必填环境变量 ORCHA_GATEWAY_ENDPOINT（如 tcp://127.0.0.1:7422 或 unix:///tmp/orcha.sock）');
    process.exit(2);
  }

  const mock = process.env.ORCHA_ADAPTER_MOCK !== '0';
  const feishuAppId = process.env.ORCHA_FEISHU_APP_ID;
  const feishuAppSecret = process.env.ORCHA_FEISHU_APP_SECRET;

  if (!mock) {
    if (!feishuAppId || !feishuAppSecret) {
      console.error('非 mock 模式必须提供 ORCHA_FEISHU_APP_ID 和 ORCHA_FEISHU_APP_SECRET');
      process.exit(2);
    }
  }

  return {
    gatewayEndpoint,
    mock,
    feishuAppId,
    feishuAppSecret,
  };
}

// ============================================================
// 主流程
// ============================================================

async function main(): Promise<void> {
  const cfg = loadConfig();
  console.log(`[adapter] 启动中 mock=${cfg.mock} gateway=${cfg.gatewayEndpoint}`);

  const feishuClient = createFeishuClient(cfg);
  const ipcClient = createIpcClient(cfg, feishuClient);

  // 长连接仅在非 mock 模式启动（mock 模式无凭证，连不上飞书）
  let longConn: LongConnectionHandle | null = null;
  if (!cfg.mock) {
    longConn = startLongConnection({
      appId: cfg.feishuAppId!,
      appSecret: cfg.feishuAppSecret!,
      handlers: {
        onTrigger: (event: FeishuEvent) => {
          onWebhookTrigger(event, ipcClient);
        },
        onApprovalAction: (
          actionId: string,
          operatorOpenId: string,
          operatorChatId: string | null,
          decision,
        ) => {
          onApprovalCardAction(
            actionId,
            operatorOpenId,
            operatorChatId,
            decision,
            ipcClient,
          );
        },
      },
    });
    console.log('[adapter] 飞书长连接已启动（SDK 内部自动重连 + 签名校验）');
  } else {
    console.log('[adapter] mock 模式：不启动长连接（无飞书凭证）');
  }

  // 信号处理：优雅关闭
  let shuttingDown = false;
  const shutdown = async (signal: NodeJS.Signals) => {
    if (shuttingDown) {
      console.log(`[adapter] 已在关闭中，再次收到 ${signal}，强制退出`);
      process.exit(1);
    }
    shuttingDown = true;
    console.log(`[adapter] 收到 ${signal}，开始优雅关闭`);
    if (longConn) {
      longConn.close();
    }
    await ipcClient.stop();
    console.log('[adapter] 已关闭');
    process.exit(0);
  };
  process.on('SIGTERM', shutdown);
  process.on('SIGINT', shutdown);

  ipcClient.start();
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
  const trimmed = event.description.trim();
  // /stop / /cancel：发 Reply 消息让 Gateway 取消任务，不发 Trigger
  if (trimmed === '/stop' || trimmed === '/cancel' || trimmed.startsWith('/stop ') || trimmed.startsWith('/cancel ')) {
    const replyMsg: AdapterToGateway = {
      type: 'reply',
      task_id: '',
      content: event.description,
      session: event.session,
    };
    const ok = ipcClient.send(replyMsg);
    console.log(`[adapter] 取消命令已转发: session=${event.session} content=${event.description} ok=${ok}`);
    return;
  }

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

/** M7 P1：审批卡片按钮点击 → 转 IPC ApprovalResponse 消息。 */
function onApprovalCardAction(
  actionId: string,
  operatorOpenId: string,
  operatorChatId: string | null,
  decision: ApprovalResponseMsg['decision'],
  ipcClient: IpcClient,
): void {
  const msg: ApprovalResponseMsg = {
    type: 'approval_response',
    action_id: actionId,
    operator_open_id: operatorOpenId,
    operator_chat_id: operatorChatId,
    decision,
  };
  const ok = ipcClient.send(msg);
  if (!ok) {
    console.warn(`[adapter] IPC 断开，审批回复丢失: action_id=${actionId}`);
  } else {
    console.log(`[adapter] 审批回复已发送: action_id=${actionId} operator=${operatorOpenId}`);
  }
}

/** IPC 消息 → 飞书推送。 */
async function onGatewayMessage(
  msg: GatewayToAdapter,
  feishuClient: FeishuClient,
): Promise<void> {
  console.log(`[adapter] IPC 收到消息 type=${msg.type} task_id=${(msg as any).task_id ?? '-'}`);
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
    case 'approval_request':
      console.log(`[adapter] 收到审批请求 action_id=${msg.action_id} task=${msg.task_id} action=${JSON.stringify(msg.action)}`);
      await feishuClient.pushApprovalCard(
        msg.session,
        msg.task_id,
        msg.action_id,
        msg.action,
      );
      console.log(`[adapter] 审批卡片已推送 action_id=${msg.action_id}`);
      return;
    case 'approval_result':
      await feishuClient.patchApprovalResult(
        msg.action_id,
        msg.decision,
        msg.operator_open_id,
      );
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
