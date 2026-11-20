/**
 * The daemon's pid lock: the file that makes "exactly one daemon per home" provable. It mirrors
 * `./store/lock.ts` — a create-exclusive file (`flag: "wx"`), liveness by `process.kill(pid, 0)`
 * and a heartbeat carried by the file's own mtime — for the same reason: `flock` does not exist on
 * Bun, and a lock that shells out is a lock with a subprocess in front of it.
 *
 * The two files stay separate on purpose. `write.lock` guards one repository's writer; `daemon.pid`
 * guards the machine's singleton. One shared implementation would make the writer and the daemon
 * contend for the same file, which is the coupling this design removed.
 *
 * The reclaim rule is the writer's rule unchanged: a lock is stale only when its recorded pid is
 * gone or its mtime is older than {@link PID_STALE_AFTER_MS}. A file whose content cannot be
 * parsed (a holder that died between `open` and `write`) is reclaimed by age alone, so the
 * microseconds-long window of a healthy acquire is never mistaken for an abandoned daemon.
 */
import { mkdir, readFile, stat, unlink, utimes, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";

/**
 * How old the heartbeat may get before the pid is treated as abandoned. The daemon refreshes the
 * mtime through {@link heartbeatPidLock} on every tick, so a long-running daemon renews rather than
 * expires — and a machine that slept past this window is still rescued by the dead-pid rule.
 */
export const PID_STALE_AFTER_MS = 60_000;

/** A contended start is worth retrying once the previous daemon has finished exiting. */
const RETRY_AFTER_MS = 1_000;

/** The create, then one retry after a reclaim: the contract's "unlink then one retry". */
const ACQUIRE_ATTEMPTS = 2;

export interface PidLockHandle {
  readonly path: string;
  readonly pid: number;
  /** ISO-8601, from the holder's clock; kept for `status`, never compared across hosts. */
  readonly startedAt: string;
}

export interface PidLockState {
  readonly held: boolean;
  /** The recorded holder, or `null` when the file is absent or its content is unreadable. */
  readonly pid: number | null;
  readonly stale: boolean;
  /** The recorded start time, or `null` when the content could not be parsed. */
  readonly startedAt: string | null;
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

function parseRecord(raw: string): { pid: number; startedAt: string | null } | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) {
      return null;
    }
    const { pid, startedAt } = parsed as { pid?: unknown; startedAt?: unknown };
    if (typeof pid !== "number" || !Number.isInteger(pid) || pid <= 0) {
      return null;
    }
    return { pid, startedAt: typeof startedAt === "string" ? startedAt : null };
  } catch {
    // A torn write from a holder that died mid-create; `readPidLock` falls back to the age rule.
    return null;
  }
}

/**
 * `kill(pid, 0)` sends no signal and reports whether the process exists. `EPERM` means the pid
 * exists but belongs to another user, which still counts as alive.
 */
function isAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return errorCode(error) !== "ESRCH";
  }
}

/**
 * Reports the current holder without creating or removing anything, so `sangkar daemon status` is
 * safe on a home no daemon has ever touched.
 */
export async function readPidLock(path: string): Promise<PidLockState> {
  let modifiedAt: number;
  try {
    modifiedAt = (await stat(path)).mtimeMs;
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      return { held: false, pid: null, stale: false, startedAt: null };
    }
    throw error;
  }

  let raw: string;
  try {
    raw = await readFile(path, "utf8");
  } catch (error) {
    // The holder released between the `stat` and the `read`; that is an unheld lock, not a fault.
    if (errorCode(error) === "ENOENT") {
      return { held: false, pid: null, stale: false, startedAt: null };
    }
    throw error;
  }

  const record = parseRecord(raw);
  const age = Date.now() - modifiedAt;
  const stale =
    record === null ? age > PID_STALE_AFTER_MS : !isAlive(record.pid) || age > PID_STALE_AFTER_MS;
  return { held: true, pid: record?.pid ?? null, stale, startedAt: record?.startedAt ?? null };
}

/**
 * Takes the pid lock, or throws `index_locked` when a live daemon owns it. A stale lock is removed,
 * logged as a warning and taken in the same call: a daemon killed by a reboot must not brick the
 * next start. The parent directory is created because a fresh machine has no home yet.
 */
export async function acquirePidLock(path: string): Promise<PidLockHandle> {
  await mkdir(dirname(path), { recursive: true });

  for (let attempt = 1; attempt <= ACQUIRE_ATTEMPTS; attempt += 1) {
    const handle: PidLockHandle = {
      path,
      pid: process.pid,
      startedAt: new Date().toISOString(),
    };
    try {
      // `wx` is the whole lock: the create is atomic, so exactly one daemon wins the race. The
      // file carries only what a reader needs — who holds it and since when — never this path.
      await writeFile(path, JSON.stringify({ pid: handle.pid, startedAt: handle.startedAt }), {
        flag: "wx",
        encoding: "utf8",
      });
      return handle;
    } catch (error) {
      if (errorCode(error) !== "EEXIST") {
        throw error;
      }
    }

    const state = await readPidLock(path);
    if (!state.held) {
      // Released between our failed create and the read; try the create again.
      continue;
    }
    if (!state.stale) {
      throw new AppError(
        "index_locked",
        `another daemon holds ${path}${state.pid === null ? "" : ` (pid ${state.pid})`}`,
        {
          hint: "stop it with `sangkar daemon stop`, or delete the pid file if you know it is dead",
          retryable: true,
          retryAfterMs: RETRY_AFTER_MS,
        },
      );
    }

    log.warn("reclaiming stale pid lock", {
      path,
      holderPid: state.pid,
      staleAfterMs: PID_STALE_AFTER_MS,
    });
    try {
      await unlink(path);
    } catch (error) {
      // Another process reclaimed it first; the next attempt sees the real state.
      if (errorCode(error) !== "ENOENT") {
        throw error;
      }
    }
  }

  throw new AppError("index_locked", `could not take ${path} after ${ACQUIRE_ATTEMPTS} attempts`, {
    hint: "another process keeps winning the create; retry once it exits",
    retryable: true,
    retryAfterMs: RETRY_AFTER_MS,
  });
}

/**
 * Releases the lock, but only while this handle's pid is still the recorded holder: if the file was
 * reclaimed by another daemon (this one was wrongly judged dead), removing it would give two
 * daemons the same home. A missing file is success, so a release is safe to repeat.
 */
export async function releasePidLock(handle: PidLockHandle): Promise<void> {
  let raw: string;
  try {
    raw = await readFile(handle.path, "utf8");
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      return;
    }
    throw error;
  }

  if (parseRecord(raw)?.pid !== handle.pid) {
    log.warn("pid lock is held by another process; not removing it", {
      path: handle.path,
      pid: handle.pid,
    });
    return;
  }

  try {
    await unlink(handle.path);
  } catch (error) {
    if (errorCode(error) !== "ENOENT") {
      throw error;
    }
  }
}

/**
 * Refreshes the mtime, which is the heartbeat {@link readPidLock} reads. A daemon that runs longer
 * than {@link PID_STALE_AFTER_MS} must call this on every tick, or its own lock looks abandoned.
 */
export async function heartbeatPidLock(handle: PidLockHandle): Promise<void> {
  const now = new Date();
  try {
    await utimes(handle.path, now, now);
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      log.warn("pid lock disappeared while heartbeating", { path: handle.path });
      return;
    }
    throw error;
  }
}
