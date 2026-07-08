/**
 * IPC 客户端：连 Gateway 的 TCP/Unix socket，按 JSON line 协议读写消息。
 *
 * 设计要点：
 * - 用 node 内置 `net` 模块，零运行时依赖（除 @types/node）
 * - JSON line 读取：在 socket 上累积 Buffer，按 `\n` 切分（TCP 字节流可能
 *   一条消息拆多个 chunk 或多个消息合一个 chunk）
 * - 心跳：每 30s 发 Heartbeat，与 Rust 侧 watchdog（35s read timeout + 3 次重试）配合
 * - 自动重连：连接断开后指数退避重连（1s → 2s → 4s → ... → 30s）
 *
 * 与 packages/orcha-gateway/src/ipc.rs + lib.rs handle_connection 对应。
 */

import * as net from 'net';
import { URL } from 'url';
import { AdapterToGateway, GatewayToAdapter, encodeMsg, decodeMsg } from './protocol';

/** IPC 连接配置。 */
export interface IpcClientOptions {
  /**
   * Gateway 监听地址。两种格式：
   * - `tcp://host:port`：TCP 连接
   * - `unix:///path/to/sock`：Unix domain socket
   */
  endpoint: string;
  /** 心跳间隔，默认 30s（与 Rust 侧 watchdog 35s read timeout 配合）。 */
  heartbeatIntervalMs?: number;
  /** 重连初始退避，默认 1s。 */
  reconnectBaseMs?: number;
  /** 重连最大退避，默认 30s。 */
  reconnectMaxMs?: number;
}

/** IPC 客户端事件回调。 */
export interface IpcClientHandlers {
  /** 收到 Gateway 消息。 */
  onMessage: (msg: GatewayToAdapter) => void;
  /** 连接建立（含重连后）。 */
  onConnect: () => void;
  /** 连接断开。 */
  onDisconnect: (reason: string) => void;
}

const DEFAULT_HEARTBEAT_MS = 30_000;
const DEFAULT_RECONNECT_BASE_MS = 1_000;
const DEFAULT_RECONNECT_MAX_MS = 30_000;
/** 单行消息上限（与 Rust 侧 1MB 上限对齐）。 */
const MAX_LINE_BYTES = 1024 * 1024;

/**
 * IPC 客户端。一条到 Gateway 的长连接，自动重连 + 心跳。
 *
 * 一条连接内多路复用多个会话（用消息的 `session` 字段区分），
 * 因此 Adapter 进程通常只需一个 IpcClient 实例。
 */
export class IpcClient {
  private readonly opts: Required<IpcClientOptions>;
  private readonly handlers: IpcClientHandlers;

  private socket: net.Socket | null = null;
  private lineBuffer: Buffer = Buffer.alloc(0);
  private heartbeatTimer: NodeJS.Timeout | null = null;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private reconnectAttempts = 0;
  private stopped = false;

  constructor(opts: IpcClientOptions, handlers: IpcClientHandlers) {
    this.opts = {
      endpoint: opts.endpoint,
      heartbeatIntervalMs: opts.heartbeatIntervalMs ?? DEFAULT_HEARTBEAT_MS,
      reconnectBaseMs: opts.reconnectBaseMs ?? DEFAULT_RECONNECT_BASE_MS,
      reconnectMaxMs: opts.reconnectMaxMs ?? DEFAULT_RECONNECT_MAX_MS,
    };
    this.handlers = handlers;
  }

  /** 开始连接 + 自动重连循环。 */
  start(): void {
    this.stopped = false;
    this.connect();
  }

  /** 主动关闭，停止重连。返回 socket 关闭完成的 promise。 */
  async stop(): Promise<void> {
    this.stopped = true;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.stopHeartbeat();
    const sock = this.socket;
    if (sock !== null) {
      await new Promise<void>((resolve) => {
        sock.once('close', () => resolve());
        sock.end();
      });
    }
  }

  /** 发消息到 Gateway。连接断开时返回 false（消息丢失，由调用方决定是否缓存重试）。 */
  send(msg: AdapterToGateway): boolean {
    const sock = this.socket;
    if (sock === null || sock.destroyed || sock.writable !== true) {
      return false;
    }
    return sock.write(encodeMsg(msg));
  }

  // ----------------------------------------------------------
  // 内部：连接 + 重连
  // ----------------------------------------------------------

  private connect(): void {
    if (this.stopped) {
      return;
    }
    const { endpoint } = this.opts;
    const parsed = this.parseEndpoint(endpoint);
    if (parsed === null) {
      this.handlers.onDisconnect(`invalid endpoint: ${endpoint}`);
      this.scheduleReconnect();
      return;
    }

    const sock = parsed.kind === 'unix'
      ? net.createConnection({ path: parsed.path })
      : net.createConnection({ host: parsed.host, port: parsed.port });

    this.socket = sock;
    this.lineBuffer = Buffer.alloc(0);

    sock.setNoDelay(true);
    sock.setKeepAlive(true, 15_000);

    sock.once('connect', () => {
      this.reconnectAttempts = 0;
      this.startHeartbeat();
      this.handlers.onConnect();
    });

    sock.on('data', (chunk: Buffer) => this.onData(chunk));

    // 'error' 总是先于 'close'，先 log 到 onDisconnect，避免未处理 error 崩进程
    sock.on('error', (err: Error) => {
      this.handlers.onDisconnect(`socket error: ${err.message}`);
    });

    sock.once('close', (hadError: boolean) => {
      this.stopHeartbeat();
      this.socket = null;
      // 已经在 'error' 里通知过 onDisconnect 的话这里不重复，
      // 但 'close' 无 error 时（如对端正常 FIN）也需要通知。
      // 用 hadError 区分：有 error 时 onDisconnect 已在 error 事件里调用。
      if (!hadError) {
        this.handlers.onDisconnect('socket closed by peer');
      }
      this.scheduleReconnect();
    });
  }

  private scheduleReconnect(): void {
    if (this.stopped) {
      return;
    }
    const base = this.opts.reconnectBaseMs;
    const max = this.opts.reconnectMaxMs;
    // 指数退避：base * 2^attempts，封顶 max
    const delay = Math.min(base * Math.pow(2, this.reconnectAttempts), max);
    this.reconnectAttempts += 1;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
    }
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delay);
  }

  // ----------------------------------------------------------
  // 内部：JSON line 读取
  // ----------------------------------------------------------

  private onData(chunk: Buffer): void {
    this.lineBuffer = Buffer.concat([this.lineBuffer, chunk]);
    while (true) {
      const nl = this.lineBuffer.indexOf(0x0a); // '\n'
      if (nl === -1) {
        break;
      }
      const lineBytes = this.lineBuffer.subarray(0, nl);
      this.lineBuffer = this.lineBuffer.subarray(nl + 1);
      if (lineBytes.length === 0) {
        continue; // 空行（如连发两个 \n）
      }
      if (lineBytes.length > MAX_LINE_BYTES) {
        this.handlers.onDisconnect(
          `line exceeds ${MAX_LINE_BYTES} bytes (gateway protocol violation)`,
        );
        this.socket?.destroy();
        return;
      }
      let msg: GatewayToAdapter;
      try {
        msg = decodeMsg(lineBytes.toString('utf8'));
      } catch (e) {
        this.handlers.onDisconnect(
          `failed to decode gateway message: ${(e as Error).message}`,
        );
        this.socket?.destroy();
        return;
      }
      try {
        // onMessage 可能是 async 函数；用 Promise.resolve 统一处理，防止 unhandledRejection 崩进程
        Promise.resolve(this.handlers.onMessage(msg)).catch((e: unknown) => {
          console.error('[ipc] onMessage handler error:', e);
        });
      } catch (e) {
        // 同步错误也兜底，不影响连接
        console.error('[ipc] onMessage handler threw:', e);
      }
    }
    // 防御：buffer 累积超过 MAX_LINE_BYTES 仍无换行 → 视为协议异常
    if (this.lineBuffer.length > MAX_LINE_BYTES) {
      this.handlers.onDisconnect(
        `buffer exceeds ${MAX_LINE_BYTES} bytes without newline`,
      );
      this.socket?.destroy();
    }
  }

  // ----------------------------------------------------------
  // 内部：心跳
  // ----------------------------------------------------------

  private startHeartbeat(): void {
    this.stopHeartbeat();
    this.heartbeatTimer = setInterval(() => {
      this.send({ type: 'heartbeat', ts_ms: Date.now() });
    }, this.opts.heartbeatIntervalMs);
    // unref：心跳定时器不阻止 node 退出（用于测试 / 优雅关闭）
    this.heartbeatTimer.unref?.();
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer !== null) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
  }

  // ----------------------------------------------------------
  // 内部：endpoint 解析
  // ----------------------------------------------------------

  private parseEndpoint(endpoint: string): { kind: 'tcp'; host: string; port: number } | { kind: 'unix'; path: string } | null {
    try {
      const url = new URL(endpoint);
      if (url.protocol === 'tcp:') {
        const host = url.hostname || '127.0.0.1';
        const portStr = url.port;
        if (portStr === '') {
          return null;
        }
        const port = Number.parseInt(portStr, 10);
        if (!Number.isFinite(port) || port <= 0 || port > 65535) {
          return null;
        }
        return { kind: 'tcp', host, port };
      }
      if (url.protocol === 'unix:') {
        // unix:///tmp/orcha.sock → pathname = /tmp/orcha.sock
        const path = url.pathname;
        if (path === '' || path === '/') {
          return null;
        }
        return { kind: 'unix', path };
      }
      return null;
    } catch {
      return null;
    }
  }
}
