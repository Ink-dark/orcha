/**
 * 飞书接入：mock 模式 + webhook server + 卡片推送抽象。
 *
 * 三层职责：
 * 1. **接收**：飞书事件回调（webhook POST）→ 解析 @Orcha 触发 → 转成 TriggerMsg
 * 2. **发送**：收到 CardUpdate/Notify/TaskResult → 调飞书 API patch 卡片 / 发消息
 * 3. **mock 模式**：`ORCHA_ADAPTER_MOCK=1` 时所有动作用 console.log，便于本地开发
 *
 * M7 骨架阶段：
 * - MockFeishuClient 完整可用
 * - HttpFeishuClient 是占位骨架（TODO M8：接入飞书 OpenAPI token 刷新 + 卡片 schema）
 * - webhook server 用 node 内置 http，零依赖
 *
 * 飞书事件回调 JSON 结构（v2 简化版，仅含本 Adapter 需要的字段）：
 * ```json
 * { "header": { "event_type": "im.message.receive_v1" },
 *   "event": { "message": { "chat_id": "oc_xxx", "sender": { "sender_id": { "open_id": "ou_xxx" } },
 *                            "content": "{\"text\":\"@_user_1 fix login\"}" } } }
 * ```
 */

import * as http from 'http';
import { TriggerSource } from './protocol';

/** 飞书触发事件（已解析）。 */
export interface FeishuEvent {
  /** 飞书 chat_id，作为 IPC session 字段。 */
  session: string;
  /** 触发用户 open_id。 */
  user: string;
  /** 群标识（私聊为 null）。 */
  group: string | null;
  /** 任务描述（去掉 @Orcha 前缀）。 */
  description: string;
  /** 消息原文（含 @Orcha）。 */
  raw: string;
}

/** 飞书事件回调 handler。 */
export interface FeishuWebhookHandlers {
  /** 收到 @Orcha 触发。 */
  onTrigger: (event: FeishuEvent) => void;
}

/** 飞书客户端推送接口（mock 与真实共用）。 */
export interface FeishuClient {
  /** 卡片状态更新。 */
  pushCardUpdate(
    session: string,
    taskId: string,
    phase: string,
    detail: string,
    progress: number,
  ): Promise<void>;
  /** 降级通知。 */
  pushNotify(
    session: string,
    taskId: string,
    level: 'info' | 'warn' | 'error',
    message: string,
  ): Promise<void>;
  /** 任务终态结果。 */
  pushTaskResult(
    session: string,
    taskId: string,
    outcome: string,
    summary: string,
    artifacts: string[],
  ): Promise<void>;
}

// ============================================================
// Mock 实现
// ============================================================

/** Mock 飞书客户端：所有动作用 console.log，用于本地开发 + 测试。 */
export class MockFeishuClient implements FeishuClient {
  async pushCardUpdate(
    session: string,
    taskId: string,
    phase: string,
    detail: string,
    progress: number,
  ): Promise<void> {
    console.log(
      `[mock][card] session=${session} task=${taskId} phase=${phase} progress=${progress}% detail=${detail}`,
    );
  }
  async pushNotify(
    session: string,
    taskId: string,
    level: 'info' | 'warn' | 'error',
    message: string,
  ): Promise<void> {
    console.log(
      `[mock][notify] session=${session} task=${taskId} level=${level} msg=${message}`,
    );
  }
  async pushTaskResult(
    session: string,
    taskId: string,
    outcome: string,
    summary: string,
    artifacts: string[],
  ): Promise<void> {
    console.log(
      `[mock][result] session=${session} task=${taskId} outcome=${outcome} summary=${summary} artifacts=${artifacts.join(',')}`,
    );
  }
}

// ============================================================
// Http 实现（占位，M8 接入飞书 OpenAPI）
// ============================================================

/** Http 飞书客户端配置。 */
export interface HttpFeishuConfig {
  /** 飞书 app_id。 */
  appId: string;
  /** 飞书 app_secret。 */
  appSecret: string;
  /** 飞书 OpenAPI 基址，默认 https://open.feishu.cn/open-apis。 */
  baseUrl?: string;
}

/**
 * Http 飞书客户端（M7 占位骨架）。
 *
 * M7 阶段所有方法抛 NotImplemented，避免被误用为已实现。
 * M8 接入真实飞书 OpenAPI：tenant_access_token 刷新 + 卡片 schema + patch 接口。
 */
export class HttpFeishuClient implements FeishuClient {
  private readonly cfg: Required<HttpFeishuConfig>;
  constructor(cfg: HttpFeishuConfig) {
    this.cfg = {
      appId: cfg.appId,
      appSecret: cfg.appSecret,
      baseUrl: cfg.baseUrl ?? 'https://open.feishu.cn/open-apis',
    };
  }
  async pushCardUpdate(): Promise<void> {
    throw new Error(`HttpFeishuClient.pushCardUpdate not implemented (M8); cfg=${JSON.stringify({ appId: this.cfg.appId, baseUrl: this.cfg.baseUrl })}`);
  }
  async pushNotify(): Promise<void> {
    throw new Error('HttpFeishuClient.pushNotify not implemented (M8)');
  }
  async pushTaskResult(): Promise<void> {
    throw new Error('HttpFeishuClient.pushTaskResult not implemented (M8)');
  }
}

// ============================================================
// 触发解析
// ============================================================

/**
 * 从飞书消息原文提取任务描述。
 *
 * 支持两种 @Orcha 形态：
 * - `@Orcha fix login bug` → description="fix login bug"
 * - `@_user_1 fix login`（飞书 content 里 @ 的实际占位）→ 同样提取 fix 后内容
 *
 * 不匹配返回 null（非触发消息，应忽略）。
 */
export function parseTrigger(raw: string): string | null {
  // 飞书 IM 消息 content 字段里的 @ 通常被替换成 @_user_N 占位，
  // 但本 Adapter 在 webhook 入口已经还原成 @Orcha 前缀（见 webhook 解析）。
  // 这里只处理 @Orcha 前缀，其他形态（@_user_N）由 webhook 入口预处理。
  const match = raw.match(/^@Orcha\s+(.+)$/);
  if (match === null) {
    return null;
  }
  const desc = match[1].trim();
  return desc === '' ? null : desc;
}

/**
 * 从解析后的 FeishuEvent 构造 TriggerSource（用于 IPC Trigger 消息）。
 */
export function eventToTriggerSource(event: FeishuEvent): TriggerSource {
  return {
    platform: 'feishu',
    user: event.user,
    group: event.group,
    raw: event.raw,
  };
}

// ============================================================
// Webhook server
// ============================================================

/**
 * 启动飞书事件回调 webhook server。
 *
 * 监听 POST `/webhook/feishu`，解析飞书事件 JSON：
 * - `im.message.receive_v1` → 提取 @Orcha 触发 → 调 handlers.onTrigger
 * - 其他事件类型：log 后忽略
 *
 * M7 骨架不做事件签名校验（飞书 Encrypt Key + Verification Token），
 * M8 接入生产时必须补上。
 *
 * 返回 http.Server 实例，调用方负责 close。
 */
export function startWebhookServer(
  port: number,
  handlers: FeishuWebhookHandlers,
): http.Server {
  const server = http.createServer((req, res) => {
    if (req.method !== 'POST' || req.url !== '/webhook/feishu') {
      res.statusCode = 404;
      res.end('not found');
      return;
    }
    let body = '';
    req.on('data', (chunk: Buffer) => {
      body += chunk.toString('utf8');
      // 防御：超长请求体直接拒绝
      if (body.length > 1024 * 1024) {
        res.statusCode = 413;
        res.end('payload too large');
        req.destroy();
      }
    });
    req.on('end', () => {
      let parsed: unknown;
      try {
        parsed = JSON.parse(body);
      } catch (e) {
        console.warn('[webhook] invalid JSON:', (e as Error).message);
        res.statusCode = 400;
        res.end('invalid json');
        return;
      }
      try {
        const event = parseFeishuEvent(parsed);
        if (event === null) {
          // 非触发事件（如 url 验证 challenge 或非 @Orcha 消息），ack 即可
          res.statusCode = 200;
          res.end('ok');
          return;
        }
        handlers.onTrigger(event);
        res.statusCode = 200;
        res.end('ok');
      } catch (e) {
        console.error('[webhook] handler error:', e);
        res.statusCode = 500;
        res.end('handler error');
      }
    });
    req.on('error', (err: Error) => {
      console.warn('[webhook] request error:', err.message);
      if (!res.headersSent) {
        res.statusCode = 400;
        res.end('bad request');
      }
    });
  });

  server.listen(port);
  return server;
}

/**
 * 解析飞书事件 JSON 为 FeishuEvent。非触发消息返回 null。
 *
 * 飞书 v2 事件结构（简化）：
 * ```json
 * {
 *   "schema": "2.0",
 *   "header": { "event_type": "im.message.receive_v1" },
 *   "event": {
 *     "sender": { "sender_id": { "open_id": "ou_xxx" } },
 *     "message": {
 *       "chat_id": "oc_xxx",
 *       "chat_type": "group" | "p2p",
 *       "message_type": "text",
 *       "content": "{\"text\":\"@_user_1 fix login\"}"
 *     }
 *   }
 * }
 * ```
 *
 * 飞书 url 验证 challenge（添加事件订阅时飞书会发一个 challenge 请求）：
 * ```json
 * { "type": "url_verification", "challenge": "xxx", "token": "xxx" }
 * ```
 * 返回 null（上层会回 200，但没把 challenge 透出去——M8 接入时这里要返回
 * challenge 让上层写到 response）。
 */
function parseFeishuEvent(payload: unknown): FeishuEvent | null {
  if (typeof payload !== 'object' || payload === null) {
    return null;
  }
  const obj = payload as Record<string, unknown>;

  // url 验证 challenge：M7 骨架直接当非触发返回（不返回 challenge）
  if (obj.type === 'url_verification') {
    return null;
  }

  const header = obj.header;
  if (typeof header !== 'object' || header === null) {
    return null;
  }
  const eventType = (header as Record<string, unknown>).event_type;
  if (eventType !== 'im.message.receive_v1') {
    return null;
  }

  const event = obj.event;
  if (typeof event !== 'object' || event === null) {
    return null;
  }
  const ev = event as Record<string, unknown>;

  const sender = ev.sender;
  if (typeof sender !== 'object' || sender === null) {
    return null;
  }
  const senderId = (sender as Record<string, unknown>).sender_id;
  if (typeof senderId !== 'object' || senderId === null) {
    return null;
  }
  const user = (senderId as Record<string, unknown>).open_id;
  if (typeof user !== 'string' || user === '') {
    return null;
  }

  const message = ev.message;
  if (typeof message !== 'object' || message === null) {
    return null;
  }
  const msg = message as Record<string, unknown>;

  const chatId = msg.chat_id;
  if (typeof chatId !== 'string' || chatId === '') {
    return null;
  }
  const chatType = msg.chat_type;
  // group：保留 chatId 作为 group；p2p：group 为 null
  const group = chatType === 'group' ? chatId : null;

  if (msg.message_type !== 'text') {
    // 非文本消息（图片 / 富文本等）暂不处理
    return null;
  }

  const contentStr = msg.content;
  if (typeof contentStr !== 'string') {
    return null;
  }
  let contentObj: { text?: string };
  try {
    contentObj = JSON.parse(contentStr) as { text?: string };
  } catch {
    return null;
  }
  const text = contentObj.text ?? '';
  if (text === '') {
    return null;
  }

  // 飞书 content 里的 @ 实际是 @_user_N 占位，本骨架简化处理：
  // 把 @_user_1 还原成 @Orcha（假设第一个 @ 占位就是 @Orcha）。
  // M8 接入真实飞书 SDK 时用 mentions 字段精确还原。
  const raw = text.replace(/^@_user_\d+\s*/, '@Orcha ');
  const description = parseTrigger(raw);
  if (description === null) {
    return null;
  }

  return {
    session: chatId,
    user,
    group,
    description,
    raw,
  };
}
