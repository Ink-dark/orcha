/**
 * 飞书接入：mock 模式 + webhook server + 卡片推送。
 *
 * 三层职责：
 * 1. **接收**：飞书事件回调（webhook POST）→ 解析 @Orcha 触发 → 转成 TriggerMsg
 * 2. **发送**：收到 CardUpdate/Notify/TaskResult → 调飞书 API patch 卡片 / 发消息
 * 3. **mock 模式**：`ORCHA_ADAPTER_MOCK=1` 时所有动作用 console.log，便于本地开发
 *
 * M8 实现：
 * - 用 @larksuiteoapi/node-sdk 调飞书 OpenAPI（token 自动刷新 + 签名校验）
 * - webhook 加 url_verification challenge 透传
 * - webhook 加 X-Lark-Signature 签名校验（防伪造请求）
 * - im.message.create 发初始卡片，im.message.patch 更新卡片内容
 * - im.message.create 发 Notify/TaskResult 文本消息
 */

import * as http from 'http';
import * as crypto from 'crypto';
import * as lark from '@larksuiteoapi/node-sdk';
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
// Http 实现（M8：用 @larksuiteoapi/node-sdk）
// ============================================================

/** Http 飞书客户端配置。 */
export interface HttpFeishuConfig {
  /** 飞书 app_id。 */
  appId: string;
  /** 飞书 app_secret。 */
  appSecret: string;
  /** 飞书 OpenAPI 基址，默认 https://open.feishu.cn/open-apis（lark.Domain.Feishu）。 */
  domain?: string;
}

/** 卡片状态 emoji 映射，给用户更直观的进度感。 */
function phaseEmoji(progress: number): string {
  if (progress <= 5) return '\u{1F680}';   // 🚀
  if (progress <= 40) return '\u{1F50D}';  // 🔍
  if (progress <= 60) return '\u{1F4CB}';  // 📋
  if (progress <= 80) return '\u{1F4BB}';  // 💻
  if (progress < 100) return '\u{1F9EA}';  // 🧪
  return '\u2705';                          // ✅
}

/** 把 CardUpdate 参数构造成飞书 interactive card 的 content JSON。 */
function buildCardContent(
  taskId: string,
  phase: string,
  detail: string,
  progress: number,
): string {
  const emoji = phaseEmoji(progress);
  const bar = buildProgressBar(progress);
  return JSON.stringify({
    schema: '2.0',
    header: {
      title: { tag: 'plain_text', content: `${emoji} Orcha 任务` },
      template: progress >= 100 ? 'green' : 'blue',
    },
    body: {
      elements: [
        { tag: 'div', text: { tag: 'lark_md', content: `**任务 ID**\n${taskId}` } },
        { tag: 'div', text: { tag: 'lark_md', content: `**阶段**\n${phase}` } },
        { tag: 'div', text: { tag: 'lark_md', content: `**详情**\n${detail}` } },
        { tag: 'hr' },
        { tag: 'column_set', columns: [{ tag: 'column', width: 'weighted', weight: 1, elements: [{ tag: 'markdown', content: bar }] }] },
      ],
    },
  });
}

/** 生成 ASCII 进度条：`[█████░░░░░] 50%`。 */
function buildProgressBar(progress: number): string {
  const total = 10;
  const filled = Math.round((progress / 100) * total);
  const bar = '\u2588'.repeat(filled) + '\u2591'.repeat(total - filled);
  return `[${bar}] ${progress}%`;
}

/**
 * Http 飞书客户端（M8 用 @larksuiteoapi/node-sdk 实现）。
 *
 * - 用 lark.Client 自动管 tenant_access_token（缓存 + 过期自动刷新）
 * - CardUpdate：先 create 一条卡片消息，后续 patch 更新内容
 *   （session 内 task_id → message_id 映射，重复 push 复用同一条卡片）
 * - Notify / TaskResult：发文本消息到 chat_id（session）
 */
export class HttpFeishuClient implements FeishuClient {
  private readonly client: lark.Client;
  /** session:taskId → message_id 映射，patch 更新时复用。 */
  private readonly cardMsgIds = new Map<string, string>();

  constructor(cfg: HttpFeishuConfig) {
    this.client = new lark.Client({
      appId: cfg.appId,
      appSecret: cfg.appSecret,
      // cfg.domain 可选覆盖，默认 lark.Domain.Feishu（https://open.feishu.cn）
      domain: cfg.domain ?? lark.Domain.Feishu,
    });
  }

  async pushCardUpdate(
    session: string,
    taskId: string,
    phase: string,
    detail: string,
    progress: number,
  ): Promise<void> {
    const key = `${session}:${taskId}`;
    const content = buildCardContent(taskId, phase, detail, progress);

    const existingMsgId = this.cardMsgIds.get(key);
    if (existingMsgId) {
      // patch 更新已有卡片
      try {
        await this.client.im.message.patch({
          path: { message_id: existingMsgId },
          data: { content },
        });
        return;
      } catch (e) {
        // patch 失败（消息被删 / 过期）→ 重新 create 一条
        console.warn(`[feishu] patch 失败，重新创建: ${(e as Error).message}`);
        this.cardMsgIds.delete(key);
      }
    }

    // create 新卡片
    const resp = await this.client.im.message.create({
      params: { receive_id_type: 'chat_id' },
      data: {
        receive_id: session,
        msg_type: 'interactive',
        content,
      },
    });
    const msgId = resp?.data?.message_id;
    if (msgId) {
      this.cardMsgIds.set(key, msgId);
    }
  }

  async pushNotify(
    session: string,
    taskId: string,
    level: 'info' | 'warn' | 'error',
    message: string,
  ): Promise<void> {
    const emoji = level === 'error' ? '\u274C' : level === 'warn' ? '\u26A0\uFE0F' : '\u2139\uFE0F';
    const content = JSON.stringify({
      text: `${emoji} [${level}] ${taskId}\n${message}`,
    });
    await this.client.im.message.create({
      params: { receive_id_type: 'chat_id' },
      data: {
        receive_id: session,
        msg_type: 'text',
        content,
      },
    });
  }

  async pushTaskResult(
    session: string,
    taskId: string,
    outcome: string,
    summary: string,
    artifacts: string[],
  ): Promise<void> {
    const emoji = outcome === 'success' ? '\u2705' : '\u274C';
    const lines = [
      `${emoji} 任务完成: ${taskId}`,
      `**结果**: ${summary}`,
    ];
    if (artifacts.length > 0) {
      lines.push(`**产物**:`);
      for (const a of artifacts) {
        lines.push(`- ${a}`);
      }
    }
    const content = JSON.stringify({ text: lines.join('\n') });
    await this.client.im.message.create({
      params: { receive_id_type: 'chat_id' },
      data: {
        receive_id: session,
        msg_type: 'text',
        content,
      },
    });
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
  const match = raw.match(/^@Orcha\s+(.+)$/);
  if (match === null) {
    return null;
  }
  const desc = match[1].trim();
  return desc === '' ? null : desc;
}

/** 从解析后的 FeishuEvent 构造 TriggerSource（用于 IPC Trigger 消息）。 */
export function eventToTriggerSource(event: FeishuEvent): TriggerSource {
  return {
    platform: 'feishu',
    user: event.user,
    group: event.group,
    raw: event.raw,
  };
}

// ============================================================
// Webhook server（含 challenge 透传 + 签名校验）
// ============================================================

/** webhook server 配置。 */
export interface WebhookServerOptions {
  /** 监听端口。 */
  port: number;
  /** 事件 handler。 */
  handlers: FeishuWebhookHandlers;
  /** 飞书 Verification Token（开放平台 → 事件订阅页获取，留空则不校验）。 */
  verificationToken?: string;
  /** 飞书 Encrypt Key（开放平台 → 事件订阅页获取，留空则不校验签名）。 */
  encryptKey?: string;
}

/**
 * 校验飞书事件签名。
 *
 * 飞书签名算法：HMAC-SHA256(timestamp + nonce + encryptKey + body)
 * header: X-Lark-Signature
 */
function verifySignature(
  timestamp: string,
  nonce: string,
  body: string,
  encryptKey: string,
  signature: string,
): boolean {
  const payload = timestamp + nonce + encryptKey + body;
  const expected = crypto.createHmac('sha256', encryptKey).update(payload).digest('hex');
  // 用 timingSafeEqual 防时序攻击
  try {
    return crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(signature));
  } catch {
    return false;
  }
}

/**
 * 启动飞书事件回调 webhook server。
 *
 * 监听 POST `/webhook/feishu`：
 * - url_verification → 返回 challenge（飞书开放平台添加事件订阅时校验）
 * - im.message.receive_v1 → 提取 @Orcha 触发 → 调 handlers.onTrigger
 * - 签名校验（如果配了 encryptKey）
 * - 其他事件 → log 后 ack
 *
 * 返回 http.Server 实例，调用方负责 close。
 */
export function startWebhookServer(opts: WebhookServerOptions): http.Server {
  const { port, handlers, verificationToken, encryptKey } = opts;
  const server = http.createServer((req, res) => {
    if (req.method !== 'POST' || req.url !== '/webhook/feishu') {
      res.statusCode = 404;
      res.end('not found');
      return;
    }

    // 收集 raw body（签名校验需要原始字节，不能用 JSON.parse 后的）
    const chunks: Buffer[] = [];
    req.on('data', (chunk: Buffer) => {
      chunks.push(chunk);
      if (Buffer.concat(chunks).length > 1024 * 1024) {
        res.statusCode = 413;
        res.end('payload too large');
        req.destroy();
      }
    });

    req.on('end', () => {
      const rawBody = Buffer.concat(chunks).toString('utf8');

      // 签名校验（配了 encryptKey 才校验）
      if (encryptKey) {
        const sig = req.headers['x-lark-signature'] as string | undefined;
        const ts = req.headers['x-lark-request-timestamp'] as string | undefined;
        const nonce = req.headers['x-lark-request-nonce'] as string | undefined;
        if (!sig || !ts || !nonce) {
          console.warn('[webhook] 拒绝：缺少签名头');
          res.statusCode = 401;
          res.end('missing signature');
          return;
        }
        if (!verifySignature(ts, nonce, rawBody, encryptKey, sig)) {
          console.warn('[webhook] 拒绝：签名校验失败');
          res.statusCode = 401;
          res.end('invalid signature');
          return;
        }
      }

      let parsed: unknown;
      try {
        parsed = JSON.parse(rawBody);
      } catch (e) {
        console.warn('[webhook] invalid JSON:', (e as Error).message);
        res.statusCode = 400;
        res.end('invalid json');
        return;
      }

      try {
        // url_verification：返回 challenge（飞书开放平台添加事件订阅时校验）
        const obj = parsed as Record<string, unknown>;
        if (obj.type === 'url_verification') {
          // 可选校验 token（飞书会带 token，但 challenge 阶段通常还没配 encryptKey）
          if (verificationToken && obj.token !== verificationToken) {
            console.warn('[webhook] challenge token 不匹配');
            res.statusCode = 401;
            res.end('invalid token');
            return;
          }
          const challenge = obj.challenge;
          if (typeof challenge !== 'string') {
            res.statusCode = 400;
            res.end('missing challenge');
            return;
          }
          res.statusCode = 200;
          res.setHeader('Content-Type', 'application/json');
          res.end(JSON.stringify({ challenge }));
          console.log('[webhook] url_verification 已响应');
          return;
        }

        const event = parseFeishuEvent(parsed);
        if (event === null) {
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
 *       "content": "{\"text\":\"@_user_1 fix login\"}",
 *       "mentions": [{ "key": "@_user_1", "id": { "open_id": "ou_bot" } }]
 *     }
 *   }
 * }
 * ```
 */
function parseFeishuEvent(payload: unknown): FeishuEvent | null {
  if (typeof payload !== 'object' || payload === null) {
    return null;
  }
  const obj = payload as Record<string, unknown>;

  // url 验证 challenge 在 webhook 入口已处理，到这里说明不是
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
  const group = chatType === 'group' ? chatId : null;

  if (msg.message_type !== 'text') {
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

  // 飞书 content 里的 @ 实际是 @_user_N 占位，把 @_user_1 还原成 @Orcha
  // （飞书开放平台 mentions 字段会带精确的 open_id，但这里简化处理：第一个 @ 占位就是 @Orcha）
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
