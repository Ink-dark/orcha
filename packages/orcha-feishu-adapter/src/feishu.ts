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
import { ApprovalActionDto, ApprovalDecisionDto, TriggerSource } from './protocol';

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
  /** M7 P1：审批卡片按钮点击回调。 */
  onApprovalAction: (
    actionId: string,
    operatorOpenId: string,
    operatorChatId: string | null,
    decision: ApprovalDecisionDto,
  ) => void;
}

/** M7 P1：审批卡片点击事件（已解析）。 */
export interface ApprovalCardEvent {
  actionId: string;
  operatorOpenId: string;
  operatorChatId: string | null;
  decision: ApprovalDecisionDto;
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
  /** M7 P1：推审批卡片（带 [批准][拒绝] 按钮）。 */
  pushApprovalCard(
    session: string,
    taskId: string,
    actionId: string,
    action: ApprovalActionDto,
  ): Promise<void>;
  /** M7 P1：patch 审批卡片显示最终结果。 */
  patchApprovalResult(
    actionId: string,
    decision: ApprovalDecisionDto,
    operatorOpenId: string | null,
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
  async pushApprovalCard(
    session: string,
    taskId: string,
    actionId: string,
    action: ApprovalActionDto,
  ): Promise<void> {
    console.log(
      `[mock][approval-card] session=${session} task=${taskId} action_id=${actionId} action=${JSON.stringify(action)}`,
    );
  }
  async patchApprovalResult(
    actionId: string,
    decision: ApprovalDecisionDto,
    operatorOpenId: string | null,
  ): Promise<void> {
    console.log(
      `[mock][approval-result] action_id=${actionId} decision=${JSON.stringify(decision)} operator=${operatorOpenId ?? 'null'}`,
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

/** 把 CardUpdate 参数构造成飞书 interactive card 的 content JSON。
 *
 * 统一用 schema 1.0（顶层 elements），避免 schema 2.0 不再支持 action tag 的问题。
 */
function buildCardContent(
  taskId: string,
  phase: string,
  detail: string,
  progress: number,
): string {
  const emoji = phaseEmoji(progress);
  const bar = buildProgressBar(progress);
  return JSON.stringify({
    config: { wide_screen_mode: true },
    header: {
      title: { tag: 'plain_text', content: `${emoji} Orcha 任务` },
      template: progress >= 100 ? 'green' : 'blue',
    },
    elements: [
      { tag: 'div', text: { tag: 'lark_md', content: `**任务 ID**\n${taskId}` } },
      { tag: 'div', text: { tag: 'lark_md', content: `**阶段**\n${phase}` } },
      { tag: 'div', text: { tag: 'lark_md', content: `**详情**\n${detail}` } },
      { tag: 'hr' },
      { tag: 'div', text: { tag: 'lark_md', content: bar } },
    ],
  });
}

/** 生成 ASCII 进度条：`[█████░░░░░] 50%`。 */
function buildProgressBar(progress: number): string {
  const total = 10;
  const filled = Math.round((progress / 100) * total);
  const bar = '\u2588'.repeat(filled) + '\u2591'.repeat(total - filled);
  return `[${bar}] ${progress}%`;
}

/** M7 P1：把 ApprovalActionDto 渲染成人类可读的描述。 */
function formatApprovalAction(action: ApprovalActionDto): string {
  switch (action.type) {
    case 'write_file':
      return `\u270F\uFE0F 写文件: \`${action.path}\`\n内容预览:\n\`\`\`\n${action.content_preview}\n\`\`\``;
    case 'delete_file':
      return `\u{1F5D1}\uFE0F 删文件: \`${action.path}\``;
    case 'run_command':
      return `\u{1F4BB} 跑命令: \`${action.program} ${action.args.join(' ')}\``;
  }
}

/** M7 P1：构造审批卡片 content（带 [批准][拒绝] 按钮）。
 *
 * 飞书 schema 2.0 不再支持 `tag: action`（错误码 230099），
 * 改用 schema 1.0 的 `elements` + `tag: action`（仍受支持，且按钮交互兼容 card.action.trigger）。
 */
function buildApprovalCardContent(
  taskId: string,
  actionId: string,
  action: ApprovalActionDto,
): string {
  return JSON.stringify({
    config: { wide_screen_mode: true },
    header: {
      title: { tag: 'plain_text', content: '\u23F3 Orcha 审批请求' },
      template: 'orange',
    },
    elements: [
      { tag: 'div', text: { tag: 'lark_md', content: `**任务 ID**\n${taskId}` } },
      { tag: 'div', text: { tag: 'lark_md', content: `**动作**\n${formatApprovalAction(action)}` } },
      { tag: 'hr' },
      {
        tag: 'action',
        actions: [
          {
            tag: 'button',
            text: { tag: 'plain_text', content: '\u2705 批准' },
            type: 'primary',
            value: { action_id: actionId, decision: 'approve' },
          },
          {
            tag: 'button',
            text: { tag: 'plain_text', content: '\u{1F4CB} 批准并加入白名单' },
            type: 'primary',
            value: { action_id: actionId, decision: 'approve_and_whitelist' },
          },
          {
            tag: 'button',
            text: { tag: 'plain_text', content: '\u274C 拒绝' },
            type: 'danger',
            value: { action_id: actionId, decision: 'reject' },
          },
        ],
      },
    ],
  });
}

/** M7 P1：构造审批结果卡片 content（patch 用）。统一用 schema 1.0。 */
function buildApprovalResultContent(
  taskId: string,
  action: ApprovalActionDto,
  decision: ApprovalDecisionDto,
  operatorOpenId: string | null,
): string {
  const actionDesc = formatApprovalAction(action);
  const decisionDesc = decision.type === 'approved'
    ? '\u2705 已批准'
    : decision.type === 'approve_and_whitelist'
      ? '\u{1F4CB} 已批准并加入白名单'
      : `\u274C 已拒绝：${decision.reason}`;
  const operatorDesc = operatorOpenId ? `\n**审批人**: ${operatorOpenId}` : '';
  return JSON.stringify({
    config: { wide_screen_mode: true },
    header: {
      title: { tag: 'plain_text', content: decision.type === 'approved' || decision.type === 'approve_and_whitelist' ? '\u2705 审批通过' : '\u274C 审批拒绝' },
      template: decision.type === 'approved' || decision.type === 'approve_and_whitelist' ? 'green' : 'red',
    },
    elements: [
      { tag: 'div', text: { tag: 'lark_md', content: `**任务 ID**\n${taskId}` } },
      { tag: 'div', text: { tag: 'lark_md', content: `**动作**\n${actionDesc}` } },
      { tag: 'hr' },
      { tag: 'div', text: { tag: 'lark_md', content: `**结果**\n${decisionDesc}${operatorDesc}` } },
    ],
  });
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
  /** M7 P1：action_id → message_id 映射，审批结果 patch 时复用。 */
  private readonly approvalCardMsgIds = new Map<string, string>();
  /** M7 P1：action_id → { taskId, action } 缓存，patch 结果卡片时需要展示原动作。 */
  private readonly approvalActions = new Map<
    string,
    { taskId: string; action: ApprovalActionDto }
  >();

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

  async pushApprovalCard(
    session: string,
    taskId: string,
    actionId: string,
    action: ApprovalActionDto,
  ): Promise<void> {
    // 缓存 action 详情，patchApprovalResult 时需要展示原动作
    this.approvalActions.set(actionId, { taskId, action });

    const content = buildApprovalCardContent(taskId, actionId, action);
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
      this.approvalCardMsgIds.set(actionId, msgId);
    }
  }

  async patchApprovalResult(
    actionId: string,
    decision: ApprovalDecisionDto,
    operatorOpenId: string | null,
  ): Promise<void> {
    const msgId = this.approvalCardMsgIds.get(actionId);
    if (!msgId) {
      console.warn(`[feishu] patchApprovalResult: 未找到 action_id=${actionId} 对应卡片，跳过 patch`);
      return;
    }
    const cached = this.approvalActions.get(actionId);
    if (!cached) {
      console.warn(`[feishu] patchApprovalResult: 未找到 action_id=${actionId} 对应动作详情，跳过 patch`);
      return;
    }
    const content = buildApprovalResultContent(
      cached.taskId,
      cached.action,
      decision,
      operatorOpenId,
    );
    try {
      await this.client.im.message.patch({
        path: { message_id: msgId },
        data: { content },
      });
      // patch 成功后清理缓存
      this.approvalCardMsgIds.delete(actionId);
      this.approvalActions.delete(actionId);
    } catch (e) {
      console.warn(`[feishu] patch 审批结果失败: ${(e as Error).message}`);
    }
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
 * 连接状态通过 console.log/warn 实时打印，便于诊断：
 *   [feishu-ws] 已连接 / 重连中 / 重连成功 / 致命错误
 *
 * 返回句柄，调用方在退出时 close。
 */
export function startLongConnection(opts: LongConnectionOptions): LongConnectionHandle {
  console.log(`[feishu-ws] 启动长连接 appId=${opts.appId} domain=${opts.domain ?? 'feishu'}`);

  const wsClient = new lark.WSClient({
    appId: opts.appId,
    appSecret: opts.appSecret,
    domain: opts.domain ?? lark.Domain.Feishu,
    // debug 级别让 SDK 输出连接握手细节，方便诊断连接失败
    loggerLevel: lark.LoggerLevel.debug,
    onReady: () => {
      console.log('[feishu-ws] 长连接已建立，等待飞书事件');
    },
    onError: (err: Error) => {
      console.error(`[feishu-ws] 长连接致命错误: ${err.message}`);
      console.error('  可能原因：app_id/app_secret 错、应用未发布、未开长连接、网络不通');
    },
    onReconnecting: () => {
      console.warn('[feishu-ws] 连接断开，重连中...');
    },
    onReconnected: () => {
      console.log('[feishu-ws] 重连成功');
    },
  });

  const eventDispatcher = new lark.EventDispatcher({}).register({
    // im.message.receive_v1：用户在群/私聊发消息
    'im.message.receive_v1': async (data: unknown) => {
      console.log('[feishu-ws] 收到消息事件');
      // SDK 已处理签名/解密，data 是事件 payload 的 event 部分
      const event = parseFeishuEvent(data);
      if (event !== null) {
        opts.handlers.onTrigger(event);
      } else {
        console.log('[feishu-ws] 事件已忽略（非 @Orcha 触发或解析失败）');
      }
    },
    // M7 P1：card.action.trigger：用户点击审批卡片的 [批准]/[拒绝] 按钮
    'card.action.trigger': async (data: unknown) => {
      console.log('[feishu-ws] 收到卡片按钮事件');
      const cardEvent = parseApprovalCardEvent(data);
      if (cardEvent !== null) {
        opts.handlers.onApprovalAction(
          cardEvent.actionId,
          cardEvent.operatorOpenId,
          cardEvent.operatorChatId,
          cardEvent.decision,
        );
      } else {
        console.log('[feishu-ws] 卡片事件已忽略（非审批按钮或解析失败）');
      }
    },
  });

  // start 返回 Promise，但 SDK 内部已通过 onReady/onError 回调通知状态
  // 这里 catch 一下防止 unhandledRejection
  void wsClient.start({ eventDispatcher }).catch((err: unknown) => {
    console.error(`[feishu-ws] start 失败: ${(err as Error).message ?? err}`);
  });

  return {
    close(): void {
      console.log('[feishu-ws] 关闭长连接');
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

/**
 * M7 P1：解析飞书 `card.action.trigger` 事件的 event payload 为 ApprovalCardEvent。
 * 非审批按钮（value 里没 action_id/decision）返回 null。
 *
 * event 结构（简化）：
 * ```json
 * {
 *   "operator": { "open_id": "ou_xxx" },
 *   "action": { "value": { "action_id": "xxx", "decision": "approve" } },
 *   "context": { "open_chat_id": "oc_xxx" }
 * }
 * ```
 */
function parseApprovalCardEvent(payload: unknown): ApprovalCardEvent | null {
  if (typeof payload !== 'object' || payload === null) {
    return null;
  }
  const ev = payload as Record<string, unknown>;

  // operator.open_id
  const operator = ev.operator;
  if (typeof operator !== 'object' || operator === null) {
    return null;
  }
  const operatorOpenId = (operator as Record<string, unknown>).open_id;
  if (typeof operatorOpenId !== 'string' || operatorOpenId === '') {
    return null;
  }

  // action.value.{action_id, decision}
  const action = ev.action;
  if (typeof action !== 'object' || action === null) {
    return null;
  }
  const value = (action as Record<string, unknown>).value;
  if (typeof value !== 'object' || value === null) {
    return null;
  }
  const v = value as Record<string, unknown>;
  const actionId = v.action_id;
  if (typeof actionId !== 'string' || actionId === '') {
    return null;
  }
  const decisionRaw = v.decision;
  if (decisionRaw !== 'approve' && decisionRaw !== 'reject' && decisionRaw !== 'approve_and_whitelist') {
    return null;
  }

  // context.open_chat_id（可选，私聊可能没有）
  const context = ev.context;
  let operatorChatId: string | null = null;
  if (typeof context === 'object' && context !== null) {
    const chatId = (context as Record<string, unknown>).open_chat_id;
    if (typeof chatId === 'string' && chatId !== '') {
      operatorChatId = chatId;
    }
  }

  const decision: ApprovalDecisionDto =
    decisionRaw === 'approve'
      ? { type: 'approved' }
      : decisionRaw === 'reject'
        ? { type: 'rejected', reason: '管理员点击拒绝按钮' }
        : { type: 'approve_and_whitelist', reason: '管理员点击批准并加入白名单' };

  return {
    actionId,
    operatorOpenId,
    operatorChatId,
    decision,
  };
}
