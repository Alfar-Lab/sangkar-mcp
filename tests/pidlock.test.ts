import { afterAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, readFile, rm, stat, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import {
  acquirePidLock,
  heartbeatPidLock,
  PID_STALE_AFTER_MS,
  readPidLock,
  releasePidLock,
} from "#shared/pidlock";

/**
 * The pid lock is the daemon's singleton, so these cases are mostly refusals: a live holder is
 * never taken from, and a dead or silent one is always reclaimed. Both rules are exercised through
 * real files under `mkdtemp` — no daemon is started and nothing is written inside the repository.
 */

const dirs: string[] = [];

async function tempPath(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-pidlock-"));
  dirs.push(dir);
  return join(dir, "daemon.pid");
}

/** A pid that is certainly free: the test proves it before the file claims it holds the lock. */
const DEAD_PID = 0x7ffffffe;

function ageTheFile(path: string, ageMs: number): Promise<void> {
  const old = new Date(Date.now() - ageMs);
  return utimes(path, old, old);
}

afterAll(async () => {
  await Promise.all(dirs.map((dir) => rm(dir, { recursive: true, force: true })));
  dirs.length = 0;
});

describe("readPidLock", () => {
  test("an absent file is an unheld lock, not an error", async () => {
    const path = await tempPath();
    expect(await readPidLock(path)).toEqual({
      held: false,
      pid: null,
      stale: false,
      startedAt: null,
    });
  });

  test("a fresh file written by a live pid is held and not stale", async () => {
    const path = await tempPath();
    const startedAt = new Date().toISOString();
    await writeFile(path, JSON.stringify({ pid: process.pid, startedAt }));

    expect(await readPidLock(path)).toEqual({
      held: true,
      pid: process.pid,
      stale: false,
      startedAt,
    });
  });
});

describe("acquirePidLock", () => {
  test("the content is the documented shape, written create-exclusive", async () => {
    const path = await tempPath();
    const handle = await acquirePidLock(path);

    expect(handle.path).toBe(path);
    expect(handle.pid).toBe(process.pid);
    expect(JSON.parse(await readFile(path, "utf8"))).toEqual({
      pid: process.pid,
      startedAt: handle.startedAt,
    });
    await releasePidLock(handle);
  });

  test("a second acquire while the holder is alive refuses with index_locked", async () => {
    const path = await tempPath();
    const first = await acquirePidLock(path);

    let caught: unknown = null;
    try {
      await acquirePidLock(path);
    } catch (error) {
      caught = error;
    }

    expect(caught).toBeInstanceOf(AppError);
    const refusal = caught as AppError;
    expect(refusal.code).toBe("index_locked");
    expect(refusal.retryable).toBe(true);
    expect(refusal.message).toContain(path);
    expect(refusal.message).toContain(String(process.pid));
    // The refusal changes nothing: the first holder still owns the file.
    expect((await readPidLock(path)).pid).toBe(first.pid);
    await releasePidLock(first);
  });

  test("a dead pid is reclaimed", async () => {
    const path = await tempPath();
    // Prove the pid is really free before the file pretends to be held by it.
    expect(() => process.kill(DEAD_PID, 0)).toThrow();
    await writeFile(path, JSON.stringify({ pid: DEAD_PID, startedAt: new Date().toISOString() }));

    expect((await readPidLock(path)).stale).toBe(true);
    const handle = await acquirePidLock(path);

    expect(handle.pid).toBe(process.pid);
    expect((await readPidLock(path)).pid).toBe(process.pid);
    await releasePidLock(handle);
  });

  test("an aged heartbeat is reclaimed even though the pid is alive", async () => {
    const path = await tempPath();
    const startedAt = new Date().toISOString();
    await writeFile(path, JSON.stringify({ pid: process.pid, startedAt }));
    await ageTheFile(path, PID_STALE_AFTER_MS + 1_000);

    const aged = await readPidLock(path);
    expect(aged.held).toBe(true);
    expect(aged.stale).toBe(true);

    const before = Date.now();
    const handle = await acquirePidLock(path);

    expect(handle.pid).toBe(process.pid);
    // A taken lock starts now, not when the stale file was written.
    expect(Date.parse(handle.startedAt)).toBeGreaterThanOrEqual(before);
    await releasePidLock(handle);
  });

  test("an unparseable file is reclaimed by age alone", async () => {
    const path = await tempPath();
    await writeFile(path, "not json at all");

    // Young and unreadable: the microseconds of a healthy create, so it is honoured.
    expect(await readPidLock(path)).toEqual({
      held: true,
      pid: null,
      stale: false,
      startedAt: null,
    });
    let caught: unknown = null;
    try {
      await acquirePidLock(path);
    } catch (error) {
      caught = error;
    }
    expect((caught as AppError).code).toBe("index_locked");

    await ageTheFile(path, PID_STALE_AFTER_MS + 1_000);
    expect((await readPidLock(path)).stale).toBe(true);
    const handle = await acquirePidLock(path);
    expect(handle.pid).toBe(process.pid);
    await releasePidLock(handle);
  });
});

describe("releasePidLock", () => {
  test("removes the file, and a second release of the same handle succeeds", async () => {
    const path = await tempPath();
    const handle = await acquirePidLock(path);

    await releasePidLock(handle);
    expect(existsSync(path)).toBe(false);
    // Idempotent: an already-released handle is not an error.
    await releasePidLock(handle);
    expect(existsSync(path)).toBe(false);
  });

  test("leaves a file another process re-took, so one home never has two holders", async () => {
    const path = await tempPath();
    const handle = await acquirePidLock(path);
    await writeFile(path, JSON.stringify({ pid: DEAD_PID, startedAt: new Date().toISOString() }));

    await releasePidLock(handle);

    expect(existsSync(path)).toBe(true);
    expect((await readPidLock(path)).pid).toBe(DEAD_PID);
  });
});

describe("heartbeatPidLock", () => {
  test("moves the mtime forward", async () => {
    const path = await tempPath();
    const handle = await acquirePidLock(path);
    await ageTheFile(path, 30_000);
    const before = (await stat(path)).mtimeMs;

    await heartbeatPidLock(handle);
    const after = (await stat(path)).mtimeMs;

    expect(after).toBeGreaterThan(before);
    expect((await readPidLock(path)).stale).toBe(false);
    await releasePidLock(handle);
  });

  test("a file that vanished while heartbeating is a warning, not a throw", async () => {
    const path = await tempPath();
    const handle = await acquirePidLock(path);
    await releasePidLock(handle);

    await heartbeatPidLock(handle);

    expect(existsSync(path)).toBe(false);
  });
});
