/**
 * #24：带 TTL + 最大容量的 Map，防止长期运行的 adapter 因无界 Map OOM。
 *
 * 旧实现 `HttpFeishuClient` 用三个原生 `Map`（cardMsgIds /
 * approvalCardMsgIds / approvalActions）只增不减：超时的审批、孤儿卡片、
 * 已结束任务的卡片映射永远不会被清理，长时间运行的高频 bot 会 OOM。
 *
 * 本 Map 提供：
 * - **TTL 过期**：每个条目带过期时间，`get` 时懒删除；`cleanup()` 主动扫描。
 * - **最大容量（近似 LRU）**：超过 `maxSize` 时按最早迭代顺序淘汰（`get`/
 *   `set` 会把条目重排到末尾，Map 自带插入顺序，故近似 LRU）。
 * - **节流清理**：`set` 时距上次 `cleanup()` 超过 `CLEANUP_INTERVAL_MS` 才
 *   全量扫描，避免每次写都 O(n)。
 *
 * API 与原生 `Map` 的 `get/set/delete` 子集兼容，便于直接替换。
 */

interface TTLMapEntry<V> {
  value: V;
  /** 过期时间（epoch ms）。 */
  expiresAt: number;
}

export interface TTLMapOptions {
  /** 单条目存活时长（ms），默认 1 小时。 */
  ttlMs?: number;
  /** 最大条目数，超过按最早迭代顺序淘汰，默认 10000。 */
  maxSize?: number;
}

/** cleanup() 全量扫描的节流间隔。 */
const CLEANUP_INTERVAL_MS = 60_000;

export class TTLMap<K, V> {
  private readonly map = new Map<K, TTLMapEntry<V>>();
  private readonly ttlMs: number;
  private readonly maxSize: number;
  private lastCleanup = 0;

  constructor(opts: TTLMapOptions = {}) {
    this.ttlMs = opts.ttlMs ?? 3_600_000;
    this.maxSize = opts.maxSize ?? 10_000;
  }

  /** 当前条目数（含可能已过期但未懒删除的，仅作粗略观察用）。 */
  get size(): number {
    return this.map.size;
  }

  /**
   * 取值。过期条目懒删除并返回 `undefined`。
   * 命中时把条目重排到迭代末尾（近似 LRU）。
   */
  get(key: K): V | undefined {
    const entry = this.map.get(key);
    if (entry === undefined) {
      return undefined;
    }
    if (Date.now() >= entry.expiresAt) {
      this.map.delete(key);
      return undefined;
    }
    // LRU：delete + set 把条目挪到 Map 迭代末尾
    this.map.delete(key);
    this.map.set(key, entry);
    return entry.value;
  }

  /** 设置条目（刷新 TTL 与迭代顺序）。超容量时淘汰最早条目。 */
  set(key: K, value: V): void {
    const expiresAt = Date.now() + this.ttlMs;
    this.map.delete(key);
    this.map.set(key, { value, expiresAt });
    this.maybeCleanup();
  }

  /** 删除条目，返回是否原本存在。 */
  delete(key: K): boolean {
    return this.map.delete(key);
  }

  /** 是否包含（且未过期）。 */
  has(key: K): boolean {
    return this.get(key) !== undefined;
  }

  /**
   * 主动扫描并删除所有已过期条目，返回清理数量。
   * 内部 `set` 会节流调用；外部也可周期性调用（如定时器）。
   */
  cleanup(): number {
    let removed = 0;
    const now = Date.now();
    for (const [k, entry] of this.map) {
      if (now >= entry.expiresAt) {
        this.map.delete(k);
        removed += 1;
      }
    }
    this.lastCleanup = now;
    return removed;
  }

  /** 节流清理过期 + 超容量淘汰。 */
  private maybeCleanup(): void {
    const now = Date.now();
    if (now - this.lastCleanup >= CLEANUP_INTERVAL_MS) {
      this.cleanup();
    }
    while (this.map.size > this.maxSize) {
      const oldest = this.map.keys().next();
      if (oldest.done) {
        break;
      }
      this.map.delete(oldest.value);
    }
  }
}
