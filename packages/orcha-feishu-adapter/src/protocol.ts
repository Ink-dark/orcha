/**
 * Orcha Gateway ↔ Adapter IPC 协议（TypeScript 镜像）。
 *
 * 与 packages/orcha-gateway/src/protocol.rs 保持字段一致：
 * - enum tag 用 `type` 字段，snake_case（与 Rust `#[serde(tag="type", rename_all="snake_case")]` 对齐）
 * - 消息以紧凑 JSON + `\n` 分隔（JSON line）
 * - 一条 TCP/Unix 连接内多路复用多个会话（用 `session` 字段区分）
 *
 * 修改本文件时必须同步改 protocol.rs，否则跨语言解析会失败。
 */

// ============================================================
// Adapter → Gateway
// ============================================================

/** 触发来源元信息，用于鉴权与审计。 */
export interface TriggerSource {
  /** IM 平台：`feishu` / `qq` / `slack`... */
  platform: string;
  /** 触发用户标识（飞书 open_id / QQ openid）。 */
  user: string;
  /** 触发群标识（私聊为 null）。 */
  group: string | null;
  /** 触发消息原文（含 @Orcha 前缀），用于审计。 */
  raw: string;
}

/** 用户 @Orcha 触发任务（或回复"继续/取消"等命令）。 */
export interface TriggerMsg {
  type: 'trigger';
  /** 任务 ID（Adapter 侧生成；Gateway 收到后用它做 trace_id）。空则 Gateway 生成。 */
  task_id: string;
  /** 任务描述（如 "fix the login bug"）。 */
  description: string;
  /** IM 会话标识（飞书 chat_id / QQ group_openid），用于回推卡片。 */
  session: string;
  /** 触发来源元信息。 */
  source: TriggerSource;
}

/** 用户在已有任务上回复（如 Reviewer 拒绝后用户决定"放弃" / "重试"）。 */
export interface ReplyMsg {
  type: 'reply';
  task_id: string;
  /** 用户输入原文。 */
  content: string;
  session: string;
}

/** Adapter 启动时询问 Gateway 当前用户/群是否在白名单。 */
export interface AuthCheckMsg {
  type: 'auth_check';
  user: string;
  group: string | null;
}

/** 心跳，Adapter 每 30s 发一次。 */
export interface HeartbeatMsg {
  type: 'heartbeat';
  ts_ms: number;
}

/** Adapter → Gateway 的所有消息（discriminated union）。 */
export type AdapterToGateway =
  | TriggerMsg
  | ReplyMsg
  | AuthCheckMsg
  | HeartbeatMsg;

// ============================================================
// Gateway → Adapter
// ============================================================

/** 通知级别（snake_case，与 Rust enum 对齐）。 */
export type NotifyLevel = 'info' | 'warn' | 'error';

/** 卡片状态实时更新（patch 同一张卡片：观察中→规划中→写代码中→测试中→审核中）。 */
export interface CardUpdateMsg {
  type: 'card_update';
  task_id: string;
  session: string;
  /** 当前阶段标签（"🔍 观察中" / "📋 规划中" / "💻 写代码中" / ...）。 */
  phase: string;
  /** 详细进度（如 "已发现 3 个文件"）。 */
  detail: string;
  /** 进度 0-100，方便卡片进度条。 */
  progress: number;
}

/** 降级链通知（如 LLM 超时通知用户"AI 卡住，正在恢复"）。 */
export interface NotifyMsg {
  type: 'notify';
  task_id: string;
  session: string;
  level: NotifyLevel;
  message: string;
}

/** 任务终态结果。 */
export interface TaskResultMsg {
  type: 'task_result';
  task_id: string;
  session: string;
  /** `success` / `failed` / `panic`。 */
  outcome: string;
  /** 一句话总结。 */
  summary: string;
  /** 产物 URL 列表（如 `file:///...`），用于卡片可点击展开。 */
  artifacts: string[];
}

/** 鉴权回复。 */
export interface AuthResultMsg {
  type: 'auth_result';
  allowed: boolean;
  /** 拒绝原因（allowed=false 时填）。 */
  reason: string | null;
}

/** 心跳 ack。 */
export interface HeartbeatAckMsg {
  type: 'heartbeat_ack';
  ts_ms: number;
}

/** Gateway → Adapter 的所有消息（discriminated union）。 */
export type GatewayToAdapter =
  | CardUpdateMsg
  | NotifyMsg
  | TaskResultMsg
  | AuthResultMsg
  | HeartbeatAckMsg;

// ============================================================
// JSON line 编解码辅助
// ============================================================

/** 序列化为 JSON line：紧凑 JSON + 末尾 `\n`。 */
export function encodeMsg(msg: AdapterToGateway | GatewayToAdapter): string {
  // JSON.stringify 默认紧凑（无空格），与 Rust serde_json::to_string 一致。
  // 内部字符串中的 `\n` 会被转义为 `\\n`，不会破坏 line 协议。
  return JSON.stringify(msg) + '\n';
}

/** 解码单行 JSON。 */
export function decodeMsg(line: string): GatewayToAdapter {
  const obj = JSON.parse(line);
  if (obj === null || typeof obj !== 'object' || typeof obj.type !== 'string') {
    throw new Error(`invalid gateway message: missing 'type' field in: ${line}`);
  }
  return obj as GatewayToAdapter;
}
