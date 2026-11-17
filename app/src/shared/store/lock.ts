/**
 * The single-writer lock. `flock` does not exist on Bun, so the lock is a create-exclusive file:
 * `writeFile(path, …, { flag: "wx" })` fails with `EEXIST` when someone else holds it, and the
 * content says who. Nothing is shelled out — liveness is `process.kill(pid, 0)` and the heartbeat
 * is the file's own mtime, so the lock works on every platform Bun runs on.
 *
 * Two processes must never both believe they hold it, hence the reclaim rule is narrow: a lock is
 * stale only when its recorded pid is gone or its mtime is older than {@link STALE_AFTER_MS}. A
 * lock whose content cannot be parsed (its holder died between `open` and `write`) is reclaimed
 * by the age rule alone, so the microseconds-long window of a healthy acquire is never mistaken
 * for a stale lock.
 */
import { readFile, stat, unlink, utimes, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";

/** The lock file lives in the data dir, outside the database (`DATABASE.md` §4). */
const LOCK_FILE = "write.lock";

/**
 * How old the heartbeat may get before the lock is treated as abandoned. The indexer refreshes
 * the mtime with {@link heartbeat} while it works, so a longer run renews rather than expires.
 */
export const STALE_AFTER_MS = 60_000;

/** A contended call is worth retrying once the journal-mode lock inside SQLite has cleared. */
const RETRY_AFTER_MS = 1_000;

/** Reclaims are races by nature: re-check the file a few times before reporting contention. */
const ACQUIRE_ATTEMPTS = 3;

export interface LockHandle {
  readonly path: string;
  readonly pid: number;
  /** ISO-8601, from the holder's clock; kept for diagnostics, never compared across hosts. */
  readonly startedAt: string;
}

export interface LockState {
  readonly held: boolean;
  /** The recorded holder, or `null` when the file is absent or its content is unreadable. */
  readonly pid: number | null;
  readonly stale: boolean;
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

function parsePid(raw: string): number | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== "object" || parsed === null) {
      return null;
    }
    const { pid } = parsed as { pid?: unknown };
    return typeof pid === "number" && Number.isInteger(pid) && pid > 0 ? pid : null;
  } catch {
    // A torn write from a holder that died mid-create; `inspect` falls back to the age rule.
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

export function lockPath(dataDir: string): string {
  return join(dataDir, LOCK_FILE);
}

/**
 * Reports the current holder without creating or removing anything. Safe to call from any
 * process; the caller decides what a held lock means.
 */
export async function inspect(dataDir: string): Promise<LockState> {
  const path = lockPath(dataDir);

  let modifiedAt: number;
  try {
    modifiedAt = (await stat(path)).mtimeMs;
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      return { held: false, pid: null, stale: false };
    }
    throw error;
  }

  let raw: string;
  try {
    raw = await readFile(path, "utf8");
  } catch (error) {
    // The holder released between the `stat` and the `read`; that is an unheld lock, not a fault.
    if (errorCode(error) === "ENOENT") {
      return { held: false, pid: null, stale: false };
    }
    throw error;
  }

  const pid = parsePid(raw);
  const age = Date.now() - modifiedAt;
  const stale = pid === null ? age > STALE_AFTER_MS : !isAlive(pid) || age > STALE_AFTER_MS;
  return { held: true, pid, stale };
}

/**
 * Takes the lock, or throws `index_locked` when a live holder owns it. A stale lock is removed,
 * logged as a warning, and taken in the same call: a crashed indexer must not brick the repo.
 */
export async function acquireLock(dataDir: string): Promise<LockHandle> {
  const path = lockPath(dataDir);
  const handle: LockHandle = {
    path,
    pid: process.pid,
    startedAt: new Date().toISOString(),
  };

  for (let attempt = 1; attempt <= ACQUIRE_ATTEMPTS; attempt += 1) {
    try {
      // `wx` is the whole lock: the create is atomic, so exactly one process wins the race.
      await writeFile(path, JSON.stringify(handle), { flag: "wx", encoding: "utf8" });
      return handle;
    } catch (error) {
      if (errorCode(error) !== "EEXIST") {
        throw error;
      }
    }

    const state = await inspect(dataDir);
    if (!state.held) {
      // Released between our failed create and the inspect; try the create again.
      continue;
    }
    if (!state.stale) {
      throw new AppError(
        "index_locked",
        `another indexer holds ${path}${state.pid === null ? "" : ` (pid ${state.pid})`}`,
        {
          hint: "wait for the running indexer to finish, or delete the lock if you know it is dead",
          retryable: true,
          retryAfterMs: RETRY_AFTER_MS,
        },
      );
    }

    log.warn("reclaiming stale write lock", {
      path,
      holderPid: state.pid,
      staleAfterMs: STALE_AFTER_MS,
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
    hint: "another indexer keeps winning the create; retry once it exits",
    retryable: true,
    retryAfterMs: RETRY_AFTER_MS,
  });
}

/**
 * Releases the lock, but only while this handle's pid is still the recorded holder: if the lock
 * was reclaimed by someone else (this process was wrongly judged dead), removing it would give
 * two writers the same repo.
 */
export async function release(handle: LockHandle): Promise<void> {
  let raw: string;
  try {
    raw = await readFile(handle.path, "utf8");
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      return;
    }
    throw error;
  }

  if (parsePid(raw) !== handle.pid) {
    log.warn("write lock is held by another process; not removing it", {
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
 * Refreshes the lock's mtime, which is the heartbeat `inspect` reads. A holder that runs longer
 * than {@link STALE_AFTER_MS} must call this, or its own lock can be reclaimed underneath it.
 */
export async function heartbeat(handle: LockHandle): Promise<void> {
  const now = new Date();
  try {
    await utimes(handle.path, now, now);
  } catch (error) {
    if (errorCode(error) === "ENOENT") {
      log.warn("write lock disappeared while heartbeating", { path: handle.path });
      return;
    }
    throw error;
  }
}
