import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";
import { createStorePool, POOL_IDLE_TIMEOUT_MS, type StorePool } from "#shared/store/pool";

/**
 * The pool's accounting, against a fake handle and an injected clock — no database, because the
 * properties under test are "one handle per root", "the right entry is closed" and "a failed open
 * leaves nothing behind", none of which needs SQLite to be true.
 */

interface Fake {
  readonly pool: StorePool<string>;
  readonly opened: string[];
  readonly closed: string[];
  advance(ms: number): void;
}

function fake(idleTimeoutMs?: number): Fake {
  let nowMs = 1_000;
  const opened: string[] = [];
  const closed: string[] = [];
  const pool = createStorePool<string>({
    open: (root) => {
      opened.push(root);
      return `handle:${root}`;
    },
    close: (value) => {
      closed.push(value);
    },
    ...(idleTimeoutMs === undefined ? {} : { idleTimeoutMs }),
    now: () => nowMs,
  });
  return {
    pool,
    opened,
    closed,
    advance: (ms) => {
      nowMs += ms;
    },
  };
}

describe("one entry per canonical root", () => {
  test("the same root returns the identical handle and opens it once", () => {
    const { pool, opened } = fake();
    const first = pool.acquire("a");
    expect(pool.acquire("a")).toBe(first);
    expect(opened).toHaveLength(1);
    expect(pool.size).toBe(1);
  });

  test("a, a/ and ./a name one entry", () => {
    const { pool, opened } = fake();
    const first = pool.acquire("a");
    expect(pool.acquire("a/")).toBe(first);
    expect(pool.acquire("./a")).toBe(first);
    expect(pool.size).toBe(1);
    expect(opened).toEqual([resolve("a")]);
    expect(pool.entries()[0]?.root).toBe(resolve("a"));
  });

  test("three roots are three handles", () => {
    const { pool, opened } = fake();
    const handles = [pool.acquire("a"), pool.acquire("b"), pool.acquire("c")];
    expect(new Set(handles).size).toBe(3);
    expect(opened).toHaveLength(3);
    expect(pool.size).toBe(3);
    expect(pool.entries().map((entry) => entry.root)).toEqual(
      ["a", "b", "c"].map((root) => resolve(root)),
    );
  });

  test("acquiring bumps lastUsedAt and keeps openedAt", () => {
    const { pool, advance } = fake();
    pool.acquire("a");
    const openedAt = pool.entries()[0]?.openedAt;
    advance(500);
    pool.acquire("a");
    const [entry] = pool.entries();
    expect(entry?.openedAt).toBe(openedAt);
    expect(entry?.lastUsedAt).toBe((openedAt ?? 0) + 500);
  });
});

describe("idle eviction", () => {
  test("the default timeout is one minute", () => {
    expect(POOL_IDLE_TIMEOUT_MS).toBe(60_000);
    const { pool, closed, advance } = fake();
    pool.acquire("a");
    advance(POOL_IDLE_TIMEOUT_MS - 1);
    expect(pool.releaseIdle()).toEqual([]);
    expect(closed).toEqual([]);
    advance(1);
    expect(pool.releaseIdle()).toEqual([resolve("a")]);
    expect(closed).toEqual([`handle:${resolve("a")}`]);
  });

  test("before the timeout nothing is closed", () => {
    const { pool, closed, advance } = fake(1_000);
    pool.acquire("a");
    advance(999);
    expect(pool.releaseIdle()).toEqual([]);
    expect(closed).toEqual([]);
    expect(pool.size).toBe(1);
  });

  test("after it the idle root is closed, forgotten and reopened on the next acquire", () => {
    const { pool, opened, closed, advance } = fake(1_000);
    pool.acquire("a");
    advance(1_000);
    expect(pool.releaseIdle()).toEqual([resolve("a")]);
    expect(closed).toHaveLength(1);
    expect(pool.size).toBe(0);
    expect(pool.entries()).toEqual([]);

    pool.acquire("a");
    expect(opened).toHaveLength(2);
    expect(pool.size).toBe(1);
  });

  test("a just-used root survives while a stale one is evicted", () => {
    const { pool, closed, advance } = fake(1_000);
    pool.acquire("stale");
    advance(600);
    pool.acquire("fresh");
    advance(600);
    // "stale" has now been idle 1200ms, "fresh" only 600ms.
    expect(pool.releaseIdle()).toEqual([resolve("stale")]);
    expect(closed).toEqual([`handle:${resolve("stale")}`]);
    expect(pool.size).toBe(1);
    expect(pool.entries()[0]?.root).toBe(resolve("fresh"));
  });

  test("an explicit now overrides the injected clock", () => {
    const { pool, closed, advance } = fake(1_000);
    pool.acquire("a");
    advance(10);
    const openedAt = pool.entries()[0]?.openedAt ?? 0;
    expect(pool.releaseIdle(openedAt + 1_000)).toEqual([resolve("a")]);
    expect(closed).toHaveLength(1);
  });
});

describe("replace", () => {
  test("stores the refreshed handle without opening or closing anything", () => {
    const { pool, opened, closed } = fake();
    pool.acquire("a");
    pool.replace("a", "handle:refreshed");
    expect(pool.acquire("a")).toBe("handle:refreshed");
    expect(opened).toHaveLength(1);
    expect(closed).toEqual([]);
    expect(pool.size).toBe(1);
  });

  test("a root that was never acquired can be replaced in place", () => {
    const { pool, opened, closed } = fake();
    pool.replace("a", "handle:injected");
    expect(pool.acquire("a")).toBe("handle:injected");
    expect(opened).toEqual([]);
    expect(closed).toEqual([]);
    expect(pool.size).toBe(1);
  });
});

describe("shutdown", () => {
  test("closeAll closes each entry once, and a second call closes nothing", () => {
    const { pool, closed } = fake();
    pool.acquire("a");
    pool.acquire("b");
    pool.closeAll();
    expect(closed).toHaveLength(2);
    expect(pool.size).toBe(0);

    pool.closeAll();
    expect(closed).toHaveLength(2);
    expect(pool.releaseIdle(Number.MAX_SAFE_INTEGER)).toEqual([]);
    expect(closed).toHaveLength(2);
  });
});

describe("a failed open", () => {
  test("leaves no half-entry and the retry succeeds", () => {
    let attempts = 0;
    const closed: string[] = [];
    const pool = createStorePool<string>({
      open: () => {
        attempts += 1;
        if (attempts === 1) {
          throw new Error("database is locked");
        }
        return "handle:retry";
      },
      close: (value) => {
        closed.push(value);
      },
      now: () => 0,
    });

    expect(() => pool.acquire("a")).toThrow("database is locked");
    expect(pool.size).toBe(0);
    expect(pool.entries()).toEqual([]);
    expect(closed).toEqual([]);

    expect(pool.acquire("a")).toBe("handle:retry");
    expect(attempts).toBe(2);
    expect(pool.size).toBe(1);
  });
});
