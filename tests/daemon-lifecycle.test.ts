import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, realpathSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Logger } from "#shared/log";
import { acquirePidLock, readPidLock, releasePidLock } from "#shared/pidlock";
import { writeRegistry } from "#shared/registry";
import {
  DEFAULT_DAEMON_TICK_MS,
  daemonStatus,
  runDaemon,
  stopDaemon,
} from "../src/daemon/lifecycle";

/**
 * The daemon's lifecycle with no daemon in it: the loop is driven by an injected `onTick`, a short
 * `intervalMs` and an injected `AbortSignal`, so nothing here spawns a child or waits on a real
 * poll. Every home and repo is a `mkdtemp` directory removed in `afterAll`; `SANGKAR_HOME` points
 * at the temp home so a path that resolves a home can never reach the operator's.
 */

const dirs: string[] = [];

/** A logger that drops every line: these cases are about files, not about stderr. */
const silent: Logger = {
  debug: () => {},
  info: () => {},
  warn: () => {},
  error: () => {},
};

let previousHome: string | undefined;

async function tempHome(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-daemon-"));
  dirs.push(dir);
  process.env.SANGKAR_HOME = dir;
  return dir;
}

/** A repo directory with an optional on-disk config, returned in its canonical spelling. */
async function tempRepo(
  home: string,
  name: string,
  config?: Record<string, unknown>,
): Promise<string> {
  const root = join(home, name);
  await mkdir(root, { recursive: true });
  if (config !== undefined) {
    await mkdir(join(root, ".sangkar"), { recursive: true });
    await writeFile(join(root, ".sangkar", "config.json"), JSON.stringify(config));
  }
  return realpathSync(root);
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Waits for a condition the loop reaches on its own, so no test sleeps for a guessed duration. */
async function until(condition: () => boolean, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (condition()) {
      return;
    }
    if (Date.now() >= deadline) {
      throw new Error("timed out waiting for the daemon loop");
    }
    await sleep(5);
  }
}

beforeAll(() => {
  previousHome = process.env.SANGKAR_HOME;
});

afterAll(async () => {
  if (previousHome === undefined) {
    delete process.env.SANGKAR_HOME;
  } else {
    process.env.SANGKAR_HOME = previousHome;
  }
  await Promise.all(dirs.map((dir) => rm(dir, { recursive: true, force: true })));
  dirs.length = 0;
});

describe("runDaemon", () => {
  test("takes the pid file, ticks once immediately and then per interval", async () => {
    const home = await tempHome();
    const pidPath = join(home, "daemon.pid");
    const controller = new AbortController();
    let ticks = 0;
    let pidOnFirstTick = false;

    const running = runDaemon({
      home,
      intervalMs: 5,
      signal: controller.signal,
      log: silent,
      onTick: async () => {
        ticks += 1;
        if (ticks === 1) {
          pidOnFirstTick = existsSync(pidPath);
        }
      },
    });

    await until(() => ticks >= 4);

    // The lock is taken before the first tick, so the pid file describes the work being done.
    expect(pidOnFirstTick).toBe(true);
    const state = await readPidLock(pidPath);
    expect(state.held).toBe(true);
    expect(state.pid).toBe(process.pid);
    expect(state.stale).toBe(false);
    expect(state.startedAt).toBeString();

    controller.abort();
    expect(await running).toBe(0);

    expect(existsSync(pidPath)).toBe(false);
    // The loop really stopped: no tick lands after the promise resolved.
    const settled = ticks;
    await sleep(40);
    expect(ticks).toBe(settled);
  }, 10_000);

  test("a throwing first tick releases the self-acquired lock and propagates", async () => {
    const home = await tempHome();
    const pidPath = join(home, "daemon.pid");

    await expect(
      runDaemon({
        home,
        log: silent,
        onTick: async () => {
          throw new Error("boom");
        },
      }),
    ).rejects.toThrow("boom");

    // A daemon that cannot serve leaves no pid file, so the next start is not blocked.
    expect(existsSync(pidPath)).toBe(false);
  }, 10_000);

  test("a caller-supplied lock is used as-is and not released on stop", async () => {
    const home = await tempHome();
    const pidPath = join(home, "daemon.pid");
    const lock = await acquirePidLock(pidPath);
    const controller = new AbortController();
    let ticks = 0;

    const running = runDaemon({
      home,
      lock,
      intervalMs: 5,
      signal: controller.signal,
      log: silent,
      onTick: async () => {
        ticks += 1;
      },
    });

    await until(() => ticks >= 2);
    controller.abort();
    expect(await running).toBe(0);

    // The acquirer releases: one handle is never released twice.
    expect((await readPidLock(pidPath)).pid).toBe(lock.pid);
    await releasePidLock(lock);
    expect(existsSync(pidPath)).toBe(false);
  }, 10_000);

  test("a throwing first tick keeps a caller-supplied lock in place", async () => {
    const home = await tempHome();
    const pidPath = join(home, "daemon.pid");
    const lock = await acquirePidLock(pidPath);

    await expect(
      runDaemon({
        home,
        lock,
        log: silent,
        onTick: async () => {
          throw new Error("first pass failed");
        },
      }),
    ).rejects.toThrow("first pass failed");

    expect((await readPidLock(pidPath)).pid).toBe(lock.pid);
    await releasePidLock(lock);
  }, 10_000);

  test("a later failing tick is logged and the loop keeps running", async () => {
    const home = await tempHome();
    const pidPath = join(home, "daemon.pid");
    const controller = new AbortController();
    const errorLines: string[] = [];
    let ticks = 0;

    const running = runDaemon({
      home,
      intervalMs: 5,
      signal: controller.signal,
      log: { ...silent, error: (msg) => errorLines.push(msg) },
      onTick: async () => {
        ticks += 1;
        if (ticks === 2) {
          throw new Error("a reconcile pass failed");
        }
      },
    });

    await until(() => ticks >= 4);
    controller.abort();
    expect(await running).toBe(0);
    expect(errorLines).toContain("daemon tick failed");
    expect(existsSync(pidPath)).toBe(false);
  }, 10_000);
});

describe("daemonStatus", () => {
  test("a stopped home reports no pid and one runtime per registered repo", async () => {
    const home = await tempHome();
    const paused = await tempRepo(home, "repo-paused", {
      watch: false,
      watch_interval: 5000,
      telemetry: false,
    });
    const defaults = await tempRepo(home, "repo-defaults");
    const gone = join(home, "repo-gone");
    writeRegistry(join(home, "sangkar.json"), { version: 1, repos: [paused, defaults, gone] });

    const status = await daemonStatus(home);

    expect(status.pid).toBeNull();
    expect(status.startedAt).toBeNull();
    expect(status.home).toBe(home);
    expect(status.registryPath).toBe(join(home, "sangkar.json"));
    expect(status.repos).toEqual([
      { root: paused, exists: true, watch: false, watchIntervalMs: 5000, telemetry: false },
      { root: defaults, exists: true, watch: true, watchIntervalMs: 2000, telemetry: true },
      { root: gone, exists: false, watch: true, watchIntervalMs: 2000, telemetry: true },
    ]);
  }, 10_000);

  test("an empty home reports no pid and no repos", async () => {
    const home = await tempHome();

    expect(await daemonStatus(home)).toEqual({
      pid: null,
      startedAt: null,
      home,
      registryPath: join(home, "sangkar.json"),
      repos: [],
    });
  }, 10_000);
});

describe("stopDaemon", () => {
  test("returns 1 on a home with no pid file", async () => {
    const home = await tempHome();

    expect(await stopDaemon(home)).toBe(1);
  }, 10_000);

  test("returns 1 when the recorded pid is already gone", async () => {
    const home = await tempHome();
    await writeFile(
      join(home, "daemon.pid"),
      JSON.stringify({ pid: 0x7ffffffe, startedAt: new Date().toISOString() }),
    );

    expect(await stopDaemon(home)).toBe(1);
  }, 10_000);
});

describe("constants", () => {
  test("the default tick is five seconds", () => {
    expect(DEFAULT_DAEMON_TICK_MS).toBe(5_000);
  });
});
