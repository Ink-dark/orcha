/**
 * 飞书接入：mock 模式 + WebSocket 长连接 + 卡片推送。
 *
 * 三层职责：
 * 1. **接收**：飞书 WebSocket 长连接（lark.WSClient）主动拉事件 → 解析 @Orcha 触发 → 转成 TriggerMsg
 * 2. **发送**：收到 CardUpdate/Notify/TaskResult → 调飞书 API patch 卡片 / 发消息
 * 3. **mock 模式**：`ORCHA_ADAPTER_MOCK=1` 时所有动作用 console.log，便于本地开发
 *
 * M8 实现（长连接版）：
 * - 接收：用 @larksuiteoapi/node-sdk 的 WSClient 长连接，SDK 内部自动维护心跳 / 重连 / 事件签名校验 / 解密
 * - 发送：用 lark.Client 调 OpenAPI（token 自动刷新），im.message.create 发卡片 / 文本，im.message.patch 更新卡片
 * - 优势：不需要公网 URL、不需要 HTTPS 证书、不需要内网穿透、不需要配 Encrypt Key / Verification Token
 * - 限制：一个 app 同时只能有一个长连接实例（多副本请用 webhook 模式）
 */

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
export interface FeishuHandlers {
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
// WebSocket 长连接（lark.WSClient）
// ============================================================

/** 长连接启动选项。 */
export interface LongConnectionOptions {
  /** 事件 handler。 */
  handlers: FeishuHandlers;
  /** 飞书 app_id。 */
  appId: string;
  /** 飞书 app_secret。 */
  appSecret: string;
  /** 飞书 OpenAPI 域，默认 lark.Domain.Feishu（https://open.feishu.cn）。 */
  domain?: string;
}

/** 长连接句柄，调用方负责 close。 */
export interface LongConnectionHandle {
  /** 关闭长连接。 */
  close(): void;
}

/**
 * 启动飞书 WebSocket 长连接。
 *
 * - 用 lark.WSClient 主动连飞书服务器（不需要公网 URL / 内网穿透 / HTTPS 证书）
 * - SDK 内部自动维护心跳 / 重连 / 事件签名校验 / 解密（无需配 Encrypt Key / Verification Token）
 * - 注册 `im.message.receive_v1` 事件，收到消息后解析 @Orcha 触发
 * - 限制：一个 app 同时只能有一个长连接实例
 *
 * 返回句柄，调用方在退出时 close。
 */
export function startLongConnection(opts: LongConnectionOptions): LongConnectionHandle {
  const wsClient = new lark.WSClient({
    appId: opts.appId,
    appSecret: opts.appSecret,
    domain: opts.domain ?? lark.Domain.Feishu,
    loggerLevel: lark.LoggerLevel.info,
  });

  const eventDispatcher = new lark.EventDispatcher({}).register({
    // im.message.receive_v1：用户在群/私聊发消息
    'im.message.receive_v1': async (data: unknown) => {
      // SDK 已处理签名/解密，data 是事件 payload 的 event 部分
      const event = parseFeishuEvent(data);
      if (event !== null) {
        opts.handlers.onTrigger(event);
      }
    },
  });

  // start 是 async（返回 Promise<void>），不 await 让主流程继续
  // 失败时 SDK 内部会重连；fatal 错误由 onReconnecting / onError 回调通知（未来加）
  void wsClient.start({ eventDispatcher });

  return {
    close(): void {
      wsClient.close();
    },
  };
}

// ============================================================
// 事件解析（纯函数，可单测）
// ============================================================

/**
 * 解析飞书 im.message.receive_v1 事件的 event payload 为 FeishuEvent。
 * 非触发消息（非文本、无 @Orcha）返回 null。
 *
 * event 结构（简化）：
 * ```json
 * {
 *   "sender": { "sender_id": { "open_id": "ou_xxx" } },
 *   "message": {
 *     "chat_id": "oc_xxx",
 *     "chat_type": "group" | "p2p",
 *     "message_type": "text",
 *     "content": "{\"text\":\"@_user_1 fix login\"}"
 *   }
 * }
 * ```
 */
function parseFeishuEvent(payload: unknown): FeishuEvent | null {
  if (typeof payload !== 'object' || payload === null) {
    return null;
  }
  const ev = payload as Record<string, unknown>;

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
