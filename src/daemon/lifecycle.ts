/**
 * The daemon's lifecycle: one pid lock, one foreground loop, and the three verbs an operator needs
 * (`start`, `stop`, `status`). This module owns *when* a daemon runs; what it does per tick arrives
 * as `onTick`, so the reconcile loop (TASK-017) and the composition root (TASK-018) can be tested
 * without a timer, a socket or a database.
 *
 * Two rules make the singleton hold together:
 *
 * - **The acquirer releases.** `runDaemon` uses `options.lock` when it is given one and otherwise
 *   acquires the pid file itself, and it never releases a lock it did not acquire. The caller that
 *   took the lock releases it, so one handle is never released twice and a composition root can
 *   keep its `finally` as the only release site.
 * - **A daemon that cannot serve leaves no pid file.** A throwing first tick releases a
 *   self-acquired lock and propagates, so a refused start never blocks the next one for
 *   {@link PID_STALE_AFTER_MS}.
 *
 * The loop is a poll, not an event: `DEFAULT_DAEMON_TICK_MS` is short enough that a registry edit
 * is noticed while an operator is still in the shell, and the interval is `unref`'d so the tick
 * never keeps a process alive on its own — the daemon's watchers and report server do that.
 */
import { closeSync, existsSync, mkdirSync, openSync } from "node:fs";
import { join } from "node:path";
import { readGlobalConfig, repoDefaultsOf } from "#shared/global-config";
import type { Logger } from "#shared/log";
import { log } from "#shared/log";
import {
  acquirePidLock,
  heartbeatPidLock,
  PID_STALE_AFTER_MS,
  type PidLockHandle,
  readPidLock,
  releasePidLock,
} from "#shared/pidlock";
import { readRegistry } from "#shared/registry";
import { type RepoDefaults, readRepoConfig } from "#shared/repo-config";
import type { DaemonStatus, RepoRuntime } from "./types";

/** The daemon's tick: five seconds is the reconcile interval the plan fixes, restated here. */
export const DEFAULT_DAEMON_TICK_MS = 5_000;

/** The application root, from this file's own location: `src/daemon/` up two levels. */
const APP_ROOT = join(import.meta.dir, "..", "..");

/** The CLI entry the daemon mode lives behind, relative to {@link APP_ROOT}. */
const CLI_ENTRY = join("bin", "sangkar.ts");

/**
 * The three files the daemon owns in its home. They mirror the private spellings in
 * `#shared/config.ts` (`daemon.pid`, `daemon.log`, `sangkar.json`), which this module may not
 * import: `config.ts` computes them for a process that loaded the environment, and every function
 * here takes the home as an argument instead of reading one.
 */
const PID_FILE = "daemon.pid";
const LOG_FILE = "daemon.log";
const REGISTRY_FILE = "sangkar.json";

/** Mirrors the private `DATA_DIR_NAME` in `config.ts`, like `repo-config.ts` does. */
const DATA_DIR_NAME = ".sangkar";

/** How long `start` waits for the child to prove it bound the singleton, and `stop` for it to go. */
const START_TIMEOUT_MS = 5_000;
const STOP_TIMEOUT_MS = 5_000;

/** Fast enough that a healthy daemon answers in one poll, cheap enough to run for five seconds. */
const POLL_MS = 50;

export interface DaemonRunOptions {
  /** The machine-level home: `daemon.pid` is taken inside it. */
  readonly home: string;
  /** One unit of work. The first call is immediate; the rest run on the interval. */
  readonly onTick: () => Promise<void>;
  /** Tick period; defaults to {@link DEFAULT_DAEMON_TICK_MS}. */
  readonly intervalMs?: number;
  readonly log?: Logger;
  /** Test and caller seam: an abort stops the loop the way `SIGINT` does. */
  readonly signal?: AbortSignal;
  /**
   * A lock the caller already holds. Used as-is and never released here, so the handle is released
   * exactly once — by whoever acquired it.
   */
  readonly lock?: PidLockHandle;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

/** Polls `probe` until it answers `true` or the deadline passes; the answer is the return value. */
async function waitUntil(probe: () => Promise<boolean>, timeoutMs: number): Promise<boolean> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    if (await probe()) {
      return true;
    }
    if (Date.now() >= deadline) {
      return false;
    }
    await new Promise((resolve) => setTimeout(resolve, POLL_MS));
  }
}

/**
 * The foreground loop: take the singleton, run one tick and then one per interval, and resolve 0
 * when a signal stops it.
 *
 * A tick after the first that throws is logged and forgotten: a reconcile pass that hit one
 * unreadable repository must not take down the daemon that serves the other four. The first tick
 * is the exception, because a daemon whose very first pass fails has nothing to serve — it
 * releases a self-acquired lock and lets the error reach the caller.
 */
export async function runDaemon(options: DaemonRunOptions): Promise<number> {
  const logger = options.log ?? log;
  const intervalMs = Math.max(1, Math.floor(options.intervalMs ?? DEFAULT_DAEMON_TICK_MS));
  const pidPath = join(options.home, PID_FILE);
  const acquired = options.lock === undefined;
  const handle = options.lock ?? (await acquirePidLock(pidPath));

  try {
    await options.onTick();
  } catch (error) {
    if (acquired) {
      await releasePidLock(handle);
    }
    throw error;
  }

  return await new Promise<number>((resolve) => {
    let timer: ReturnType<typeof setInterval> | null = null;
    let closed = false;
    let ticking = false;

    async function teardown(): Promise<void> {
      if (closed) {
        return;
      }
      closed = true;
      if (timer !== null) {
        clearInterval(timer);
        timer = null;
      }
      process.off("SIGINT", onSignal);
      process.off("SIGTERM", onSignal);
      options.signal?.removeEventListener("abort", onAbort);
      if (acquired) {
        await releasePidLock(handle);
      }
    }

    function finish(): void {
      void teardown().then(
        () => resolve(0),
        (error: unknown) => {
          // The loop is over either way; a release that failed must not turn a stop into a crash.
          logger.error("daemon shutdown failed", { message: messageOf(error) });
          resolve(0);
        },
      );
    }

    function onSignal(signal: NodeJS.Signals): void {
      logger.info("daemon.signal", { signal });
      finish();
    }

    function onAbort(): void {
      finish();
    }

    function tick(): void {
      if (ticking) {
        // One slow pass must not stack up behind itself; the heartbeat still runs on the next tick.
        logger.warn("daemon tick skipped: the previous one is still running", { intervalMs });
        return;
      }
      ticking = true;
      void (async () => {
        try {
          await heartbeatPidLock(handle);
          await options.onTick();
        } catch (error) {
          logger.error("daemon tick failed", { message: messageOf(error) });
        } finally {
          ticking = false;
        }
      })();
    }

    process.on("SIGINT", onSignal);
    process.on("SIGTERM", onSignal);
    options.signal?.addEventListener("abort", onAbort);

    if (options.signal?.aborted === true) {
      // Aborted before the loop began: the first tick already ran, so only the stop is left.
      finish();
      return;
    }

    timer = setInterval(tick, intervalMs);
    // An unref'd timer never keeps the process alive by itself: the watchers and the report server
    // are what a daemon lives on, so a daemon that has neither should exit instead of ticking.
    timer.unref();
  });
}

/**
 * One registered repo as `status` reports it: the directory, its config, and whether it still
 * exists. `defaults` is the machine file's answer, read once per call by {@link daemonStatus}.
 */
function runtimeOf(root: string, defaults: RepoDefaults): RepoRuntime {
  const config = readRepoConfig(root, { dataDir: join(root, DATA_DIR_NAME), defaults });
  return {
    root,
    exists: existsSync(root),
    watch: config.watch,
    watchIntervalMs: config.watchIntervalMs,
    telemetry: config.telemetry,
  };
}

/**
 * "Is the daemon up, and what is it serving": the pid file plus the registry, each answer built
 * from files so a stopped home reports `pid: null` rather than an error. A pid file whose process
 * is gone or whose heartbeat is aged is reported as no daemon, because that is what an operator
 * means by "running".
 */
export async function daemonStatus(home: string): Promise<DaemonStatus> {
  const state = await readPidLock(join(home, PID_FILE));
  const registryPath = join(home, REGISTRY_FILE);
  const live = state.held && !state.stale;
  const defaults = repoDefaultsOf(readGlobalConfig(home));
  return {
    pid: live ? state.pid : null,
    startedAt: live ? state.startedAt : null,
    home,
    registryPath,
    repos: readRegistry(registryPath).repos.map((root) => runtimeOf(root, defaults)),
  };
}

/**
 * Starts the daemon as a detached child and waits for its pid file, which is the only proof that
 * the singleton was bound. The child inherits this process's environment, so it resolves the same
 * machine home this process did — `home` always comes from `loadConfig`, which is the only reader
 * of that variable — and re-passing it here would mean a third environment read outside
 * `#shared/config.ts`, which the plan's invariant forbids. The command line is resolved the way
 * `src/tools/index/handler.ts:workerInvocation` does it — the binary that is running now, plus the
 * CLI entry when it exists beside it.
 */
export async function startDaemon(home: string): Promise<number> {
  const pidPath = join(home, PID_FILE);
  const existing = await readPidLock(pidPath);
  if (existing.held && !existing.stale) {
    log.warn("daemon start refused: a daemon already holds the pid file", {
      path: pidPath,
      pid: existing.pid,
    });
    return 1;
  }

  mkdirSync(home, { recursive: true });
  const logPath = join(home, LOG_FILE);

  let logFd: number;
  try {
    logFd = openSync(logPath, "a");
  } catch (error) {
    log.error("daemon start failed: the log cannot be opened", {
      path: logPath,
      message: messageOf(error),
    });
    return 1;
  }

  let child: { unref(): void };
  try {
    const entry = join(APP_ROOT, CLI_ENTRY);
    const prefix = existsSync(entry) ? [process.execPath, "run", entry] : [process.execPath];
    child = Bun.spawn({
      cmd: [...prefix, "daemon", "run"],
      stdin: "ignore",
      // Both streams append to one file: the daemon is detached, so nobody is left to read a pipe.
      stdout: logFd,
      stderr: logFd,
      detached: true,
    });
  } catch (error) {
    log.error("daemon start failed: the child could not be spawned", {
      home,
      message: messageOf(error),
    });
    return 1;
  } finally {
    // The child holds its own copy of the descriptor; this process must not keep the log open.
    closeSync(logFd);
  }
  child.unref();

  const started = await waitUntil(async () => (await readPidLock(pidPath)).held, START_TIMEOUT_MS);
  if (!started) {
    log.error("daemon did not write its pid file in time", {
      path: pidPath,
      timeoutMs: START_TIMEOUT_MS,
      log: logPath,
    });
    return 1;
  }

  log.info("daemon started", { pid: (await readPidLock(pidPath)).pid, home });
  return 0;
}

/**
 * Stops the daemon by the pid it recorded, and waits for the pid file to go — a `stopped` printed
 * before the process has exited is the message an operator acts on too early. Returns 1 when there
 * was nothing to stop, which is the same answer `status` gives for the same home.
 */
export async function stopDaemon(home: string): Promise<number> {
  const pidPath = join(home, PID_FILE);
  const state = await readPidLock(pidPath);
  if (!state.held || state.pid === null) {
    log.info("daemon stop: not running", { path: pidPath });
    return 1;
  }

  try {
    process.kill(state.pid, "SIGTERM");
  } catch (error) {
    if (errorCode(error) === "ESRCH") {
      // The recorded process is gone, so the pid file is the remains of a crash, not a daemon.
      log.info("daemon stop: the recorded pid is gone", { path: pidPath, pid: state.pid });
      return 1;
    }
    throw error;
  }

  const stopped = await waitUntil(async () => !(await readPidLock(pidPath)).held, STOP_TIMEOUT_MS);
  if (!stopped) {
    log.warn("daemon did not remove its pid file in time", {
      path: pidPath,
      pid: state.pid,
      timeoutMs: STOP_TIMEOUT_MS,
      staleAfterMs: PID_STALE_AFTER_MS,
    });
    return 1;
  }

  log.info("daemon stopped", { pid: state.pid, home });
  return 0;
}
