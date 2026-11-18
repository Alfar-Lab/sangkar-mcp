/**
 * The publish protocol of `DATABASE.md` §5: `stage` → write → `seal` → `rename`.
 *
 * The point is that a partial graph is unpublishable *by construction*, not by discipline:
 * nothing ever writes to the live database file. A run copies the live file (`VACUUM INTO`) or
 * starts an empty one, writes into that copy, and only a sealed copy is renamed over the live
 * path. Any failure before the rename ends in {@link discard}, so the live file keeps serving
 * the previous generation and two generators cannot interleave into one file.
 *
 * Each seal step closes a different hole:
 * - `wal_checkpoint(TRUNCATE)` empties the WAL, so the file about to be renamed is the whole
 *   database and not a database plus a log the rename would leave behind;
 * - `journal_mode=DELETE` + `synchronous=FULL` leaves no `-wal`/`-shm` beside the stage, so the
 *   renamed file is the entire generation; a reader cannot be handed a log from the old one;
 * - `quick_check(1)` is the last chance to refuse a structurally broken file: a corrupt graph is
 *   discarded before it can replace a working one.
 *
 * Handle ownership, because three handles touch the stage:
 * - {@link stage} opens its own throwaway handle for the seed and closes it before returning;
 * - {@link beginWrite}'s handle belongs to the caller and is released by `scope.close()`, which
 *   checkpoints the WAL and drops the connection to `journal_mode=DELETE` *before* closing. It
 *   has to: `bun:sqlite` defers the real `sqlite3_close` while a statement prepared inside a
 *   transaction is still referenced (Drizzle's `db.transaction` prepares one per statement), so
 *   a closed-but-lingering connection keeps `-wal`/`-shm` in place and makes the journal-mode
 *   change from another connection fail with `SQLITE_BUSY`. Doing it on the connection that
 *   legitimately owns the file, while it is uncontended, is what makes `seal` reachable at all;
 * - {@link seal} opens its own handle, runs all four steps anyway and reads every value back, so
 *   it verifies that state instead of trusting `close()` — a caller that forgot `close()` gets
 *   `AppError` `internal` (`database is locked`), not a half-sealed graph.
 */
import { Database } from "bun:sqlite";
import { randomBytes } from "node:crypto";
import { existsSync, mkdirSync, renameSync, rmSync, statSync } from "node:fs";
import { dirname } from "node:path";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";
import { type Conn, db, type GraphDb, openWriter } from "#shared/store/db";

/** The `live.stage.<rand>` infix; {@link publish} refuses any path that does not carry it. */
export const STAGE_INFIX = ".stage.";

/** `SQLite` value of `synchronous = FULL`; `NORMAL` is 1. */
const SYNCHRONOUS_FULL = 2;

/** What `PRAGMA journal_mode = DELETE` must answer for a stage to be publishable. */
const SEALED_JOURNAL_MODE = "delete";

/** Sidecars a database file may own; all of them belong to the file they are named after. */
const SIDECARS = ["-wal", "-shm", "-journal"] as const;

export interface StageOptions {
  /**
   * Start from an empty file instead of a `VACUUM INTO` copy of the live graph, the way a forced
   * full index does: the result no longer carries the previous generation's rows, tombstones or
   * free pages.
   */
  readonly forceFull?: boolean;
}

/** A stage file beside the live one, plus what it was seeded from. */
export interface Stage {
  /** The live database this stage will be renamed over. */
  readonly livePath: string;
  /** `live.stage.<rand>`; the only path {@link publish} accepts as its source. */
  readonly path: string;
  /** `"live"` after a `VACUUM INTO` seed, `"empty"` on a forced full index. */
  readonly seededFrom: "live" | "empty";
}

/** `PRAGMA wal_checkpoint(TRUNCATE)`'s report. */
export interface CheckpointReport {
  /** `1` when another connection still holds the WAL; a sealed stage needs `0`. */
  readonly busy: number;
  /** WAL frames in the log and frames moved into the database; `-1` on a non-WAL file. */
  readonly log: number;
  readonly checkpointed: number;
}

/** What {@link seal} observed, read back rather than assumed. */
export interface SealReport {
  readonly checkpoint: CheckpointReport;
  /** `PRAGMA journal_mode` read back: `"delete"`. */
  readonly journalMode: string;
  /** `PRAGMA synchronous` read back: `2`, i.e. `FULL`. */
  readonly synchronous: number;
  /** `PRAGMA quick_check(1)` rows; a sound database reports exactly `["ok"]`. */
  readonly quickCheck: readonly string[];
}

/** The Drizzle transaction handle handed to a write callback. */
export type WriteTx = Parameters<Parameters<GraphDb["transaction"]>[0]>[0];

/**
 * The open writer on a stage file. The handle is the caller's to close: `beginWrite` opened it,
 * and `seal` will fail while it is still open, which is how a forgotten `close()` is caught
 * instead of silently sealing a WAL that still has this handle attached.
 */
export interface WriteScope {
  /** The stage this writer is attached to. */
  readonly path: string;
  /** The underlying handle, for the raw-SQL paths Drizzle cannot model. */
  readonly conn: Conn;
  /**
   * Runs `callback` inside one `BEGIN IMMEDIATE` transaction and returns its value.
   *
   * Drizzle's `db.transaction` does not defer the callback: it runs immediately (inside SQLite,
   * where a transaction cannot be async) and its return value is returned as-is. `immediate`
   * takes the write lock up front, so a contended writer fails before the first statement
   * instead of midway through the load, where a half-written stage would already exist.
   */
  transaction<T>(callback: (tx: WriteTx) => T): T;
  /**
   * Ends the write phase: `wal_checkpoint(TRUNCATE)`, then `journal_mode=DELETE`, then closes.
   * Idempotent; the scope is unusable afterwards. Throws `AppError` `internal` if the WAL cannot
   * be released — a run that cannot hand over a clean file must fail, not publish.
   */
  close(): void;
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** `bun:sqlite` cannot open a URI filename, and `renameSync` cannot rename one either. */
function assertFilesystemPath(path: string): void {
  if (path.startsWith("file:")) {
    throw new AppError("internal", `bun:sqlite cannot open the URI filename ${path}`, {
      hint: "pass a plain filesystem path",
    });
  }
}

function randomSuffix(): string {
  return randomBytes(6).toString("hex");
}

/**
 * The path a stage for `livePath` would get: `live.stage.<rand>`. The suffix is random, not a
 * pid, so two indexers that ignore the write lock still cannot write the same stage.
 */
export function stagePath(livePath: string): string {
  assertFilesystemPath(livePath);
  return `${livePath}${STAGE_INFIX}${randomSuffix()}`;
}

/** Removes one file if it is there; `force` makes a missing file a no-op, nothing else. */
function removeSidecar(path: string): void {
  try {
    rmSync(path, { force: true });
  } catch (e) {
    throw new AppError("internal", `cannot remove ${path}: ${messageOf(e)}`, {
      hint: "the data directory is not writable; a stale sidecar must never be left beside a published graph",
      cause: e,
    });
  }
}

/**
 * `wal_checkpoint(TRUNCATE)` plus `journal_mode=DELETE`, on the connection that owns the file.
 * Shared by {@link beginWrite}'s `close` and {@link seal}: `seal` runs it on a fresh handle, where
 * it succeeds when the stage was released and fails (`database is locked`) when it was not.
 */
function releaseWal(conn: Conn): void {
  conn.sqlite.query<CheckpointReport, []>("PRAGMA wal_checkpoint(TRUNCATE)").all();
  conn.sqlite.exec("PRAGMA journal_mode = DELETE");
}

/**
 * Cleanup that must run on the failure path without replacing the failure: the reason the run
 * died is what the caller can act on, so a cleanup error is reported (stderr, error level) and
 * the original error keeps its place.
 */
function discardQuietly(path: string): void {
  try {
    discard(path);
  } catch (e) {
    log.error("cannot discard stage", { path, reason: messageOf(e) });
  }
}

function seedFromLive(livePath: string, path: string): void {
  // Read-only: the source of a publish is never written by the publisher.
  const source = new Database(livePath, { readonly: true });
  try {
    // A bound parameter, not string interpolation: the path is data, and a quote in a data
    // directory name must not become SQL.
    source.prepare("VACUUM INTO ?").run(path);
  } finally {
    source.close();
  }
}

function seedEmpty(path: string): void {
  const handle = new Database(path, { create: true });
  handle.close();
}

/**
 * Creates the stage file a run writes into: `VACUUM INTO` from the live graph, or an empty file
 * on `forceFull` (and when there is no live graph yet, which is first run). The seed is packed
 * by `VACUUM INTO` rather than byte-copied, so free pages and the previous `-wal` do not travel
 * with it.
 *
 * An empty seed carries no schema: the caller migrates the stage (`migrate()` is the only
 * runner), exactly as it would on a fresh database.
 */
export function stage(livePath: string, options: StageOptions = {}): Stage {
  assertFilesystemPath(livePath);
  const path = stagePath(livePath);
  const hasGraph = existsSync(livePath) && statSync(livePath).size > 0;
  const seededFrom = options.forceFull === true || !hasGraph ? "empty" : "live";

  try {
    mkdirSync(dirname(path), { recursive: true });
    if (seededFrom === "live") {
      seedFromLive(livePath, path);
    } else {
      seedEmpty(path);
    }
  } catch (e) {
    // A half-seeded stage must not survive: it would be picked up by the next run as a graph.
    discardQuietly(path);
    throw new AppError("internal", `cannot stage ${livePath} from ${seededFrom}: ${messageOf(e)}`, {
      hint: "check that the data directory exists, is writable and holds a readable graph.db",
      cause: e,
    });
  }

  log.debug("stage created", { path, livePath, seededFrom });
  return { livePath, path, seededFrom };
}

/**
 * Opens a writer on a stage file. The scope's handle is closed by `scope.close()`, and must be
 * closed before {@link seal} runs.
 */
export function beginWrite(stageFile: string): WriteScope {
  const conn = openWriter(stageFile);
  const graph = db(conn);
  let closed = false;

  return {
    path: conn.path,
    conn,
    transaction<T>(callback: (tx: WriteTx) => T): T {
      // Drizzle runs the callback immediately (bun:sqlite has no asynchronous transaction) and
      // returns its value; `immediate` takes the write lock up front.
      return graph.transaction(callback, { behavior: "immediate" });
    },
    close(): void {
      if (closed) {
        return;
      }
      closed = true;
      try {
        releaseWal(conn);
      } catch (e) {
        throw new AppError("internal", `cannot release the WAL on ${stageFile}: ${messageOf(e)}`, {
          hint: "close every other handle on the stage; the graph stays unpublished",
          cause: e,
        });
      } finally {
        conn.close();
      }
    },
  };
}

function pragmaRow(handle: Database, sql: string): Record<string, unknown> {
  const row = handle.query<Record<string, unknown>, []>(sql).get();
  if (row === null) {
    throw new AppError("internal", `${sql} returned no row`);
  }
  return row;
}

/**
 * Makes the stage publishable: `DATABASE.md` §5's four steps, every value read back.
 *
 * Throws `AppError` `internal` when any step does not reach its expected value — including a
 * write scope that was never closed, where SQLite refuses the journal-mode change. The checkpoint
 * and the journal-mode change are cheap when the stage was released by `WriteScope.close()` (then
 * the read-back is the assertion) and are the work when a stage arrives still in WAL mode with no
 * connection holding it, e.g. after a previous run died.
 */
export function seal(stageFile: string): SealReport {
  assertFilesystemPath(stageFile);
  const handle = new Database(stageFile, { create: false });
  try {
    const checkpointRows = handle
      .query<CheckpointReport, []>("PRAGMA wal_checkpoint(TRUNCATE)")
      .all();
    const checkpoint = checkpointRows[0];
    if (checkpoint === undefined || checkpointRows.length !== 1) {
      throw new AppError("internal", "PRAGMA wal_checkpoint(TRUNCATE) returned no single row");
    }
    if (checkpoint.busy !== 0) {
      throw new AppError(
        "internal",
        `wal_checkpoint(TRUNCATE) is busy (${checkpoint.busy}) on ${stageFile}`,
        { hint: "another handle still holds the stage; close the write scope before sealing" },
      );
    }

    const journalMode = String(pragmaRow(handle, "PRAGMA journal_mode = DELETE").journal_mode);
    if (journalMode !== SEALED_JOURNAL_MODE) {
      throw new AppError(
        "internal",
        `journal_mode is ${journalMode}, expected ${SEALED_JOURNAL_MODE}, on ${stageFile}`,
        { hint: "another connection is still open on the stage; close it before sealing" },
      );
    }

    handle.exec("PRAGMA synchronous = FULL");
    const synchronous = Number(pragmaRow(handle, "PRAGMA synchronous").synchronous);
    if (synchronous !== SYNCHRONOUS_FULL) {
      throw new AppError("internal", `synchronous is ${synchronous}, expected ${SYNCHRONOUS_FULL}`);
    }

    const quickCheck = handle
      .query<{ quick_check: string }, []>("PRAGMA quick_check(1)")
      .all()
      .map((row) => row.quick_check);
    if (quickCheck.length !== 1 || quickCheck[0] !== "ok") {
      throw new AppError(
        "internal",
        `quick_check failed on ${stageFile}: ${quickCheck.join("; ")}`,
        {
          hint: "the stage is unusable; it is discarded rather than published",
        },
      );
    }

    const report: SealReport = { checkpoint, journalMode, synchronous, quickCheck };
    log.debug("stage sealed", { path: stageFile, ...report });
    return report;
  } catch (e) {
    if (e instanceof AppError && e.code === "internal") {
      throw e;
    }
    throw new AppError("internal", `cannot seal ${stageFile}: ${messageOf(e)}`, {
      hint: "a stage that fails to seal is discarded; the live graph is untouched",
      cause: e,
    });
  } finally {
    // The handle is the sealer's own; the journal mode and synchronous level stay on the file.
    handle.close();
  }
}

/**
 * Renames a sealed stage over the live file and drops the previous generation's sidecars.
 *
 * Order matters: the rename comes first, because deleting the old `-wal` before the swap would
 * throw away committed data if the swap then failed. The sidecars are removed after it because
 * they describe the file that is no longer there — the newly published file is in
 * `journal_mode=DELETE`, so any `-wal`/`-shm`/`-journal` beside it is a leftover and must not be
 * interpreted against it.
 */
export function publish(stageFile: string, livePath: string): void {
  assertFilesystemPath(livePath);
  assertFilesystemPath(stageFile);
  if (!stageFile.includes(STAGE_INFIX)) {
    throw new AppError("internal", `refusing to publish ${stageFile}: not a stage file`, {
      hint: `a publishable file is named ${livePath}${STAGE_INFIX}<rand>`,
    });
  }
  if (!existsSync(stageFile)) {
    throw new AppError("internal", `stage file ${stageFile} does not exist`, {
      hint: "stage, write and seal before publishing",
    });
  }

  try {
    renameSync(stageFile, livePath);
  } catch (e) {
    throw new AppError("internal", `cannot rename ${stageFile} over ${livePath}: ${messageOf(e)}`, {
      hint: "the stage and the live file must be in the same directory (same filesystem)",
      cause: e,
    });
  }

  for (const suffix of SIDECARS) {
    removeSidecar(`${livePath}${suffix}`);
  }

  log.info("graph published", { path: livePath, bytes: statSync(livePath).size });
}

/**
 * Removes a stage and its sidecars. Never touches the live file: the paths only differ by
 * {@link STAGE_INFIX}, and this only deletes the path it was given.
 */
export function discard(stageFile: string): void {
  assertFilesystemPath(stageFile);
  removeSidecar(stageFile);
  for (const suffix of SIDECARS) {
    removeSidecar(`${stageFile}${suffix}`);
  }
  log.debug("stage discarded", { path: stageFile });
}

/**
 * Runs `callback` against a fresh stage of `livePath` and publishes only if it returns normally:
 * `stage` → `callback` → `seal` → `publish`, with `discard` in a `finally`.
 *
 * The `finally` is the guarantee §5 asks for: a throw anywhere in the callback, or in `seal`,
 * reaches `discard` and never `publish`, so the live file (and its sha256) is unchanged and no
 * `-wal`/`-shm`/`.stage.` file is left in the data directory. After a successful publish the
 * same `discard` is a no-op on the stage path.
 *
 * The callback owns its write scope and must close it; `seal` fails loudly otherwise. The
 * callback's return value is passed through.
 */
export async function withStage<T>(
  livePath: string,
  callback: (stage: Stage) => T | Promise<T>,
  options: StageOptions = {},
): Promise<T> {
  const staged = stage(livePath, options);
  try {
    const value = await callback(staged);
    // Sealing here rather than trusting the callback to have done it is what makes the rename
    // itself the only path to the live file. `seal` is idempotent, so a callback that sealed to
    // verify is not penalised.
    seal(staged.path);
    publish(staged.path, livePath);
    return value;
  } finally {
    // The whole cleanup rests here. A cleanup failure is reported but never replaces the error
    // that aborted the run, and after a successful publish this is a no-op on the stage path.
    discardQuietly(staged.path);
  }
}
