/**
 * One database handle per repository root, for a process that serves more than one repository.
 *
 * The pool is deliberately generic and knows nothing about SQLite: `open` and `close` are the
 * caller's, so the same shape holds one reader, a prepared-statement cache, or a fake. What it
 * owns is the accounting: a canonical root names exactly one entry, and an entry unused for
 * {@link POOL_IDLE_TIMEOUT_MS} can be closed to give its file descriptors back.
 *
 * There is no timer here on purpose. A repeating timer would keep the process alive and would
 * fire while a call is in flight; instead the composition root drives
 * {@link StorePool.releaseIdle} between calls, so eviction happens when the process is already
 * awake and the pool can never close a handle that is mid-read.
 */
import { resolve } from "node:path";

/** How long a handle may sit untouched before `releaseIdle` closes it. */
export const POOL_IDLE_TIMEOUT_MS = 60_000;

/** One live handle and its accounting; `root` is the canonical (resolved) path. */
export interface PoolEntry<T> {
  readonly root: string;
  readonly value: T;
  readonly openedAt: number;
  readonly lastUsedAt: number;
}

export interface StorePool<T> {
  /** The handle for `root`, opening it on first use and bumping `lastUsedAt` otherwise. */
  acquire(root: string): T;
  /** Stores a refreshed handle the caller opened itself; never calls `open` or `close`. */
  replace(root: string, value: T): void;
  /** Closes and forgets every entry idle for at least the timeout; returns the closed roots. */
  releaseIdle(now?: number): readonly string[];
  closeAll(): void;
  entries(): readonly PoolEntry<T>[];
  readonly size: number;
}

export interface StorePoolOptions<T> {
  readonly open: (root: string) => T;
  readonly close: (value: T) => void;
  readonly idleTimeoutMs?: number;
  /** Injected clock: this module reads no wall clock of its own, so tests own the timeline. */
  readonly now?: () => number;
}

export function createStorePool<T>(options: StorePoolOptions<T>): StorePool<T> {
  const idleTimeoutMs = options.idleTimeoutMs ?? POOL_IDLE_TIMEOUT_MS;
  const clock = options.now ?? Date.now;
  const entriesByRoot = new Map<string, PoolEntry<T>>();

  function acquire(root: string): T {
    const canonical = resolve(root);
    const existing = entriesByRoot.get(canonical);
    if (existing !== undefined) {
      // A new snapshot rather than a mutation, so an `entries()` array already handed out stays
      // a record of what was true when it was read.
      entriesByRoot.set(canonical, { ...existing, lastUsedAt: clock() });
      return existing.value;
    }
    // Open before recording: a throwing `open` must leave no half-entry, so the next call retries
    // instead of returning a handle that was never built.
    const value = options.open(canonical);
    const at = clock();
    entriesByRoot.set(canonical, { root: canonical, value, openedAt: at, lastUsedAt: at });
    return value;
  }

  function replace(root: string, value: T): void {
    const canonical = resolve(root);
    const at = clock();
    // Closing the old handle is the caller's job: `refreshStore()` publishes first, then hands the
    // live handle over, and only after that is the previous one safe to discard.
    const openedAt = entriesByRoot.get(canonical)?.openedAt ?? at;
    entriesByRoot.set(canonical, { root: canonical, value, openedAt, lastUsedAt: at });
  }

  function releaseIdle(now: number = clock()): readonly string[] {
    const closedRoots: string[] = [];
    for (const entry of [...entriesByRoot.values()]) {
      if (now - entry.lastUsedAt < idleTimeoutMs) {
        continue;
      }
      entriesByRoot.delete(entry.root);
      options.close(entry.value);
      closedRoots.push(entry.root);
    }
    return closedRoots;
  }

  function closeAll(): void {
    // Forget each entry before closing it: a throwing `close` then propagates without leaving the
    // entry behind to be closed a second time.
    for (const entry of [...entriesByRoot.values()]) {
      entriesByRoot.delete(entry.root);
      options.close(entry.value);
    }
  }

  return {
    acquire,
    replace,
    releaseIdle,
    closeAll,
    entries: () => [...entriesByRoot.values()],
    get size() {
      return entriesByRoot.size;
    },
  };
}
