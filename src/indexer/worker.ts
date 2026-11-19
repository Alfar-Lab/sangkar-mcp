/**
 * The index worker: the one process that owns the graph's writer (`TECH-SPEC.md` §10.2).
 *
 * Its whole reporting surface is a status byte. {@link runWorker} returns 0 published, 2 nothing
 * to do, 75 another indexer holds the lock, 1 failure, and writes nothing to stdout — stdout is the
 * MCP protocol (`TECH-SPEC.md` §11), so a child that printed a count would corrupt the session it
 * was spawned from. Everything else goes to stderr through `#shared/log`.
 *
 * An incremental run still writes the whole discovered unit set. `persistUnit` deletes and
 * re-inserts a file's nodes, and `edge.dst_id` cascades, so re-persisting only the changed files
 * would drop every unchanged caller's edge into a re-written file. The hash comparison therefore
 * decides *whether* to publish, not *how much*; the price is a full write per generation, and
 * {@link restoreEdges} closes the remaining hole (a caller persisted before the file it calls has
 * its edge cascaded away when that file's nodes are replaced).
 *
 * A directory that is not a git repository is normal, not an error: change detection degrades to
 * the `file.sha256` comparison (`#shared/domain/diff` reports `degraded`), and a first run over
 * such a tree publishes instead of failing.
 */
import { Database } from "bun:sqlite";
import { randomBytes } from "node:crypto";
import { existsSync, statSync } from "node:fs";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { type ChangeSet, changedSince } from "#shared/domain/diff";
import { isAppError } from "#shared/errors";
import { log } from "#shared/log";
import { normalizeRel } from "#shared/paths";
import { type Conn, openReader, prepared } from "#shared/store/db";
import {
  acquireLock,
  heartbeat,
  type LockHandle,
  release,
  STALE_AFTER_MS,
} from "#shared/store/lock";
import { type MigrateResult, migrate } from "#shared/store/migrate";
import { beginWrite, type WriteTx, withStage } from "#shared/store/publish";
import type { IndexRunSpec } from "#shared/telemetry/record";
import { nullTelemetry, type Telemetry } from "#shared/telemetry/telemetry";
import { type DiscoveredFile, type DiscoverResult, discover } from "./discover";
import { extractFile } from "./extract";
import { specFor } from "./lang/registry";
import { parseFile } from "./parse";
import {
  finalizeGeneration,
  type GenerationResult,
  type NotIndexedEntry,
  type ParseStatus,
  PENDING_GENERATION,
  type ProjectIdentity,
  persistUnit,
} from "./persist";
import {
  type BindResult,
  bind,
  type FileUnit,
  fileUnit,
  type ResolvedEdge,
  SymbolIndex,
  type UnresolvedBinding,
} from "./resolve";

/** `DATABASE.md` §3.0: one database per repo, inside the data dir. */
const DATABASE_FILE = "graph.db";

/**
 * `DATABASE.md` §3.0: the data dir ignores itself, so a target repo's status stays clean and no
 * tracked file is ever touched. A single star, nothing else.
 */
const DATA_DIR_GITIGNORE = "*\n";

/** `meta` keys the worker owns (`DATABASE.md` §3.1, §5). */
const META_GENERATION = "generation";
const META_INDEXER_STATE = "indexer_state";
const META_CONSECUTIVE_FAILURES = "consecutive_failures";
const META_REBUILD_REQUIRED = "rebuild_required";

/** `meta.indexer_state`; `indexing` is the server's claim while a child is alive, not ours. */
const STATE_IDLE = "idle";
const STATE_FAILED = "failed";

/** Four heartbeats per staleness window, so a slow run renews the lock it holds. */
const HEARTBEAT_INTERVAL_MS = Math.floor(STALE_AFTER_MS / 4);

/** Bound on one git invocation; the same reasoning as `#shared/domain/diff`'s bound. */
const GIT_TIMEOUT_MS = 10_000;

/** The one `meta` upsert; every state write below goes through it. */
const META_UPSERT =
  "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/**
 * Records the live nodes of one removed path as tombstones *before* the file row is dropped: the
 * cascade would take the node rows the statement reads, and a tombstone carries no foreign key
 * precisely so it can outlive the file (`DATABASE.md` §3.3).
 */
const TOMBSTONE_FILE =
  "INSERT INTO tombstone (qn, kind, file_id, last_seen_generation, removed_in_generation," +
  " had_callers) SELECT n.qn, n.kind, n.file_id, ?, ?, (SELECT count(*) FROM edge e" +
  " WHERE e.dst_id = n.id AND e.kind NOT IN ('contains', 'tested_by')) FROM node n" +
  " JOIN file f ON f.id = n.file_id WHERE f.path = ?" +
  " ON CONFLICT(qn) DO UPDATE SET kind = excluded.kind, file_id = excluded.file_id," +
  " last_seen_generation = excluded.last_seen_generation," +
  " removed_in_generation = excluded.removed_in_generation," +
  " had_callers = excluded.had_callers";

/** `generation.mode`: `full` rebuilds from an empty stage, `fast` publishes incrementally. */
export type WorkerMode = "full" | "fast";

/**
 * The four outcomes, as exit codes. 75 is `EX_TEMPFAIL`, the code `code-review-graph` uses for
 * "another indexer holds the write lock" — a contended caller retries instead of failing hard.
 */
export const WORKER_EXIT = {
  published: 0,
  failed: 1,
  nothingToDo: 2,
  locked: 75,
} as const;

export interface WorkerOptions {
  /** The working tree to index; absolute or resolvable against the caller's cwd. */
  readonly repoRoot: string;
  /** Where `graph.db`, `write.lock` and the data dir's `.gitignore` live. */
  readonly dataDir: string;
  /** Rebuild from an empty stage and publish even when the hashes say nothing changed. */
  readonly force?: boolean;
  /** `generation.mode`; defaults to `full` on a run that is forced and `fast` otherwise. */
  readonly mode?: WorkerMode;
  /**
   * The recorder the run's one `index_run` goes to. Absent when the caller passed none — a direct
   * {@link runWorker} call — and then the run writes no record at all.
   */
  readonly telemetry?: Telemetry;
}

/**
 * What went past in one run, counted from facts the run already holds: the walk, the change plan,
 * the parse results and the bind result. Filled by {@link indexGraph} and read once, on the way
 * out, so the record describes the same numbers the generation was written from.
 */
interface RunSummary {
  filesSeen: number;
  filesIndexed: number;
  filesSkipped: number;
  filesFailed: number;
  nodesByKind: Record<string, number>;
  edgesByKindTier: Record<string, number>;
  unresolvedCount: number;
  languages: Record<string, number>;
  filesChangedInRun: number;
}

/** Zeroed: the run's early exits report the fields they never got to compute as zero. */
function emptySummary(): RunSummary {
  return {
    filesSeen: 0,
    filesIndexed: 0,
    filesSkipped: 0,
    filesFailed: 0,
    nodesByKind: {},
    edgesByKindTier: {},
    unresolvedCount: 0,
    languages: {},
    filesChangedInRun: 0,
  };
}

/** One run's fixed inputs, assembled after the lock is held and the schema is current. */
interface RunContext {
  readonly repoRoot: string;
  readonly livePath: string;
  readonly force: boolean;
  readonly mode: WorkerMode;
  readonly rebuildRequired: boolean;
  readonly startedAt: number;
  /** The run's counters, shared with the exit path that writes the record. */
  readonly summary: RunSummary;
}

/** What the change comparison decided: which paths to write, and whether there is anything to do. */
interface ChangePlan {
  readonly changed: readonly string[];
  readonly removed: readonly string[];
  readonly nothingToDo: boolean;
}

/** The working tree's identity at index time, as far as git can tell it. */
interface Revision {
  /** `HEAD`, or `null` when the tree is not a git repository (or git is unusable). */
  readonly head: string | null;
  /** `true` only when git answered and reported pending changes to tracked files. */
  readonly dirty: boolean;
}

/** One discovered file, parsed or not, with the manifest facts its `file` row records. */
interface ParsedFile {
  readonly file: DiscoveredFile;
  readonly lineCount: number;
  readonly parseStatus: ParseStatus;
  readonly errorRanges: readonly string[];
  /** The unit to persist, or `null` when no tree was produced. */
  readonly unit: FileUnit | null;
  /** The coverage row a failed parse contributes, or `null` when the file parsed. */
  readonly failure: NotIndexedEntry | null;
}

/** Everything one generation's transaction needs. */
interface GenerationWrite {
  readonly parsed: readonly ParsedFile[];
  readonly removed: readonly string[];
  readonly discovered: DiscoverResult;
  readonly resolved: BindResult;
  readonly generationId: string;
  readonly revision: Revision;
  readonly identity: ProjectIdentity;
  readonly mode: WorkerMode;
  readonly durationMs: number;
  readonly rebuildRequired: boolean;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as { code: unknown }).code)
    : undefined;
}

/**
 * The index run: lock, data dir, schema, change plan, stage, publish — or a failure code with the
 * reason on stderr. Never throws for a repository-level problem; a thrown error becomes exit 1 with
 * the failure recorded in `meta`, because the caller reads the code and not an exception.
 */
export async function runWorker(options: WorkerOptions): Promise<number> {
  const repoRoot = resolve(options.repoRoot);
  const dataDir = resolve(options.dataDir);
  const livePath = join(dataDir, DATABASE_FILE);
  const startedAt = Date.now();
  // No recorder supplied (a direct call, as the worker tests make) means the seam is null and the
  // call sites below carry no conditional.
  const telemetry = options.telemetry ?? nullTelemetry();
  const summary = emptySummary();

  // The outcome of a path that leaves before {@link indexGraph} resolves the mode and the change
  // comparison decides `force`: exit 1, which is what a throw out of the setup means.
  let exitCode: number = WORKER_EXIT.failed;
  let mode: WorkerMode = options.mode ?? "full";
  let force = options.force === true;

  let lock: LockHandle | null = null;
  let beat: ReturnType<typeof setInterval> | null = null;

  try {
    // The lock is a file *in* the data dir, so the directory has to exist before it can be taken.
    await mkdir(dataDir, { recursive: true });

    lock = await acquireOrNull(dataDir);
    if (lock === null) {
      exitCode = WORKER_EXIT.locked;
    } else {
      beat = startHeartbeat(lock);

      await ensureDataDirIgnoreFile(dataDir);
      const migrated = migrateLive(livePath);
      // A store that never published has nothing to diff against: a "fast" label on a first build
      // would misdescribe both the generation row and the run's record.
      const firstBuild = !hasPublishedGeneration(livePath);
      force = options.force === true || migrated.rebuildRequired || firstBuild;
      mode = options.mode ?? (force ? "full" : "fast");

      exitCode = await indexGraph({
        repoRoot,
        livePath,
        force,
        mode,
        rebuildRequired: migrated.rebuildRequired,
        startedAt,
        summary,
      });
    }
  } catch (error) {
    log.error("worker.failed", {
      repoRoot,
      dataDir,
      reason: messageOf(error),
      code: isAppError(error) ? error.code : "internal",
    });
    recordFailure(livePath, error);
    exitCode = WORKER_EXIT.failed;
  } finally {
    if (beat !== null) {
      clearInterval(beat);
    }
    if (lock !== null) {
      try {
        await release(lock);
      } catch (error) {
        // The run's own outcome is already decided; a lock that cannot be removed is a warning.
        log.error("worker.lock_release_failed", { path: lock.path, reason: messageOf(error) });
      }
    }
    // Recorded only after the lock is gone: a slow sink must not hold the graph's write lock. The
    // `finally` is the run's single exit path, so exactly one record is written per run.
    const record: IndexRunSpec = {
      mode,
      exitCode,
      durationMs: Date.now() - startedAt,
      wasForced: force,
      filesSeen: summary.filesSeen,
      filesIndexed: summary.filesIndexed,
      filesSkipped: summary.filesSkipped,
      filesFailed: summary.filesFailed,
      nodesByKind: summary.nodesByKind,
      edgesByKindTier: summary.edgesByKindTier,
      unresolvedCount: summary.unresolvedCount,
      languages: summary.languages,
      dbSizeBytes: dbSizeOf(livePath),
      filesChangedInRun: summary.filesChangedInRun,
    };
    telemetry.recordIndexRun(record);
    telemetry.close();
  }

  return exitCode;
}

/**
 * The write lock, or `null` when another indexer holds it — the one refusal this run reports as
 * exit 75 instead of failing. Any other acquisition error is a real failure and is rethrown to
 * {@link runWorker}'s `catch`.
 */
async function acquireOrNull(dataDir: string): Promise<LockHandle | null> {
  try {
    return await acquireLock(dataDir);
  } catch (error) {
    if (!isAppError(error) || error.code !== "index_locked") {
      throw error;
    }
    log.warn("worker.locked", {
      dataDir,
      pid: process.pid,
      retryAfterMs: error.retryAfterMs,
      reason: messageOf(error),
    });
    return null;
  }
}

/**
 * Whether this store has ever published a generation. A store that has not is a first build: there
 * is nothing to diff against, so the run is `full` and `was_forced` is true even though the change
 * plan would only ever say "everything changed". {@link migrateLive} has just created the file.
 */
function hasPublishedGeneration(livePath: string): boolean {
  if (!existsSync(livePath)) {
    return false;
  }
  const conn = openReader(livePath);
  try {
    const statement = prepared(conn, "worker.publishedGeneration", (sqlite) =>
      sqlite.query<{ one: number }, []>(
        "SELECT 1 AS one FROM generation WHERE status = 'published' LIMIT 1",
      ),
    );
    // `bun:sqlite` answers `null` for no row, so this is the published check and not a truthiness test.
    return statement.get() !== null;
  } finally {
    conn.close();
  }
}

/** The published graph's size, or 0 when this run never created one. */
function dbSizeOf(livePath: string): number {
  try {
    return statSync(livePath).size;
  } catch {
    return 0;
  }
}

/**
 * Refreshes the lock's mtime while the run works. Unref'd so the timer never keeps the process
 * alive on its own, and cleared by {@link runWorker} once the run is over.
 */
function startHeartbeat(lock: LockHandle): ReturnType<typeof setInterval> {
  const timer = setInterval(() => {
    void heartbeat(lock).catch((error: unknown) => {
      log.warn("worker.heartbeat_failed", { path: lock.path, reason: messageOf(error) });
    });
  }, HEARTBEAT_INTERVAL_MS);
  timer.unref();
  return timer;
}

/**
 * Writes the data dir's own `.gitignore`. A file that already holds exactly a single star is left
 * alone; anything else is replaced, because the data dir must never appear in the target repo's
 * status (`DATABASE.md` §3.0).
 */
async function ensureDataDirIgnoreFile(dataDir: string): Promise<void> {
  const path = join(dataDir, ".gitignore");
  try {
    if ((await readFile(path, "utf8")) === DATA_DIR_GITIGNORE) {
      return;
    }
  } catch (error) {
    if (errorCode(error) !== "ENOENT") {
      throw error;
    }
  }
  await writeFile(path, DATA_DIR_GITIGNORE, "utf8");
}

/**
 * Brings the live database to the current schema before anything reads it: the change comparison
 * reads `file.sha256`, and a stage seeded from the live file inherits its tables.
 *
 * A plain handle, not a write scope: this is DDL against the published graph, not a generation, and
 * `migrate` is the one runner (`DATABASE.md` §10).
 */
function migrateLive(livePath: string): MigrateResult {
  const handle = new Database(livePath, { create: true });
  try {
    return migrate(handle);
  } finally {
    handle.close();
  }
}

/** Discovers the tree, compares it against the published graph and decides whether to run. */
async function indexGraph(ctx: RunContext): Promise<number> {
  const discovered = await discover(ctx.repoRoot);
  const plan = await planRun(ctx.repoRoot, ctx.livePath, discovered, ctx.force);

  if (plan.nothingToDo) {
    collectSeen(ctx.summary, discovered, plan);
    log.info("worker.nothing_to_do", {
      repoRoot: ctx.repoRoot,
      files: discovered.files.length,
      changed: plan.changed.length,
      removed: plan.removed.length,
    });
    return WORKER_EXIT.nothingToDo;
  }

  const parsed = await parseAll(ctx.repoRoot, discovered);
  const units = parsed.flatMap((entry) => (entry.unit === null ? [] : [entry.unit]));
  // Every unit is bound against one index: a cross-file edge needs both ends, and a per-changed-file
  // index would silently turn every cross-file call into an unresolved site.
  const resolved = bind(units, SymbolIndex.from(units));
  const generationId = newGenerationId();
  const revision = await revisionOf(ctx.repoRoot);
  const identity = await projectIdentityOf(ctx.repoRoot);
  collectSummary(ctx.summary, discovered, plan, parsed, units, resolved);
  const durationMs = Date.now() - ctx.startedAt;

  await withStage(
    ctx.livePath,
    (stage) => {
      const scope = beginWrite(stage.path);
      try {
        // An empty seed carries no schema (a forced rebuild has no live file to copy), so the stage
        // is migrated exactly as a fresh database would be.
        if (stage.seededFrom === "empty") {
          migrate(scope.conn.sqlite);
        }
        scope.transaction((tx) => {
          writeGeneration(tx, scope.conn.sqlite, {
            parsed,
            removed: plan.removed,
            discovered,
            resolved,
            generationId,
            revision,
            identity,
            mode: ctx.mode,
            durationMs,
            rebuildRequired: ctx.rebuildRequired,
          });
        });
      } finally {
        scope.close();
      }
    },
    { forceFull: ctx.force },
  );

  log.info("worker.published", {
    generation: generationId,
    repoRoot: ctx.repoRoot,
    files: discovered.files.length,
    changed: plan.changed.length,
    removed: plan.removed.length,
    edges: resolved.edges.length,
    durationMs,
    mode: ctx.mode,
  });
  return WORKER_EXIT.published;
}

/**
 * The facts a run that publishes nothing still knows: the walk and the change plan. A
 * `nothingToDo` run writes no nodes, so every other counter stays zero.
 */
function collectSeen(summary: RunSummary, discovered: DiscoverResult, plan: ChangePlan): void {
  summary.filesSeen = discovered.files.length;
  summary.filesSkipped = discovered.skipped;
  summary.languages = countBy(discovered.files, (file) => file.lang);
  summary.filesChangedInRun = plan.changed.length;
}

/**
 * Fills the run's counters from what the run already holds: no second walk, no query, no re-parse.
 * `unresolvedCount` is both lists `persistUnit` inserts — the sites the extractor could not type
 * and the ones binding failed to resolve — so it is the row count the graph will show.
 */
function collectSummary(
  summary: RunSummary,
  discovered: DiscoverResult,
  plan: ChangePlan,
  parsed: readonly ParsedFile[],
  units: readonly FileUnit[],
  resolved: BindResult,
): void {
  collectSeen(summary, discovered, plan);
  summary.filesIndexed = parsed.filter((entry) => entry.unit !== null).length;
  summary.filesFailed = parsed.filter((entry) => entry.parseStatus === "failed").length;
  summary.nodesByKind = countBy(
    units.flatMap((unit) => unit.nodes),
    (node) => node.kind,
  );
  summary.edgesByKindTier = countBy(resolved.edges, (edge) => `${edge.kind}:${edge.tier}`);
  summary.unresolvedCount =
    units.flatMap((unit) => unit.unresolved).length + resolved.unresolved.length;
}

function countBy<T>(items: readonly T[], keyOf: (item: T) => string): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const item of items) {
    const key = keyOf(item);
    counts[key] = (counts[key] ?? 0) + 1;
  }
  return counts;
}

/**
 * The change plan: the discovered files whose digest differs from their `file` row (or which have
 * no row yet), the rows whose bytes are gone from the tree, plus whatever git additionally reports
 * through `#shared/domain/diff`. Nothing changed and not forced means exit 2 with no stage, which
 * keeps a watcher's idle polls free.
 */
async function planRun(
  repoRoot: string,
  livePath: string,
  discovered: DiscoverResult,
  force: boolean,
): Promise<ChangePlan> {
  const live = openReader(livePath);
  let digests: Map<string, string>;
  let previousBaseCommit: string | null;
  let changes: ChangeSet;
  try {
    digests = readDigests(live);
    previousBaseCommit = readPreviousBaseCommit(live);
    // `changedSince` degrades to the hash comparison when git cannot answer, so a plain directory
    // is not a special case here — it is the documented fallback.
    changes = await changedSince(repoRoot, previousBaseCommit ?? "", live);
  } finally {
    live.close();
  }

  const indexed = new Set(discovered.files.map((file) => file.path));
  const changed = new Set<string>();
  for (const file of discovered.files) {
    if (digests.get(file.path) !== file.sha256) {
      changed.add(file.path);
    }
  }
  for (const path of changes.changed) {
    if (indexed.has(path)) {
      changed.add(path);
    }
  }
  // A `file` row whose path discovery no longer returns is gone from the graph: deleted, newly
  // ignored, or no longer parseable. The row is the staleness manifest, so it cannot outlive the
  // bytes it describes.
  const removed = [...digests.keys()].filter((path) => !indexed.has(path)).sort();

  log.debug("worker.change_plan", {
    repoRoot,
    discovered: discovered.files.length,
    changed: changed.size,
    removed: removed.length,
    degraded: changes.degraded,
    degradedReason: changes.degradedReason,
  });

  return {
    changed: [...changed].sort(),
    removed,
    nothingToDo: !force && changed.size === 0 && removed.length === 0,
  };
}

/** Every indexed path with the digest the published graph stored for it. One query, no join. */
function readDigests(conn: Conn): Map<string, string> {
  const statement = prepared(conn, "worker.fileDigests", (sqlite) =>
    sqlite.query<{ path: string; sha256: string }, []>(
      "SELECT path, sha256 FROM file ORDER BY path",
    ),
  );
  return new Map(statement.all().map((row) => [row.path, row.sha256]));
}

/** The published generation's `base_commit`: the diff base the change tools default to. */
function readPreviousBaseCommit(conn: Conn): string | null {
  const statement = prepared(conn, "worker.previousBaseCommit", (sqlite) =>
    sqlite.query<{ base_commit: string | null }, []>(
      "SELECT base_commit FROM generation ORDER BY seq DESC LIMIT 1",
    ),
  );
  return statement.get()?.base_commit ?? null;
}

/**
 * Parses and extracts every discovered file, in discovery order. A file with no tree becomes a
 * coverage row instead of aborting the run: one unparseable file must not cost the whole graph.
 */
async function parseAll(
  repoRoot: string,
  discovered: DiscoverResult,
): Promise<readonly ParsedFile[]> {
  const parsed: ParsedFile[] = [];
  for (const file of discovered.files) {
    parsed.push(await parseOne(repoRoot, file));
  }
  return parsed;
}

async function parseOne(repoRoot: string, file: DiscoveredFile): Promise<ParsedFile> {
  const absolute = join(repoRoot, file.path);
  const source = await Bun.file(absolute).text();
  const lineCount = countLines(source);
  const result = await parseFile(absolute, source, file.lang);

  if (!result.ok) {
    log.warn("worker.file_not_parsed", {
      path: file.path,
      reason: result.reason,
      detail: result.detail,
    });
    return {
      file,
      lineCount,
      parseStatus: "failed",
      errorRanges: [],
      unit: null,
      failure: {
        path: file.path,
        reason: result.reason,
        detail: result.detail,
        language: file.lang,
      },
    };
  }

  try {
    const extracted = extractFile(result.tree, specFor(file.lang), file.path);
    return {
      file,
      lineCount,
      parseStatus: result.errorRanges.length === 0 ? "ok" : "partial",
      errorRanges: result.errorRanges,
      unit: fileUnit(file.path, extracted),
      failure: null,
    };
  } finally {
    // The tree is only an input to the extractor; a run that keeps every tree alive holds the whole
    // corpus in WASM memory.
    result.tree.delete();
  }
}

/** `wc -l` semantics: one line per newline, and a non-empty unterminated last line counts. */
function countLines(source: string): number {
  if (source === "") {
    return 0;
  }
  let lines = 1;
  for (let at = source.indexOf("\n"); at !== -1; at = source.indexOf("\n", at + 1)) {
    lines += 1;
  }
  return source.endsWith("\n") ? lines - 1 : lines;
}

/** `g-yyyyyy`: the short id `DATABASE.md` §3.1 shows, unique enough to name one publish. */
function newGenerationId(): string {
  return `g-${randomBytes(3).toString("hex")}`;
}

/**
 * The tree a publish is about, for the `meta` identity keys (`DATABASE.md` §3.1): the name and root
 * come from the filesystem, so they are always known; the branch and remote come from git.
 *
 * Both git reads are best-effort and cannot throw: {@link gitOutput} answers `null` for a non-zero
 * exit, a missing binary or a timeout, and {@link gitValue} flattens that to the empty string. A
 * publish about a detached HEAD, an origin-less clone or a plain directory is still a publish.
 */
async function projectIdentityOf(repoRoot: string): Promise<ProjectIdentity> {
  return {
    name: basename(repoRoot),
    root: repoRoot,
    branch: await gitValue(repoRoot, ["symbolic-ref", "--short", "HEAD"]),
    remote: await gitValue(repoRoot, ["config", "--get", "remote.origin.url"]),
  };
}

/**
 * One git read as a trimmed string, `""` when git cannot answer. The `catch` is not redundant with
 * `gitOutput`'s internal guard: a pipe that fails after a successful spawn rejects the read, and a
 * missing branch name must cost the identity, never the generation.
 */
async function gitValue(repoRoot: string, args: readonly string[]): Promise<string> {
  try {
    return (await gitOutput(repoRoot, args))?.trim() ?? "";
  } catch (error) {
    log.debug("worker.git_unavailable", {
      repoRoot,
      args: args.join(" "),
      reason: messageOf(error),
    });
    return "";
  }
}

/**
 * `HEAD` plus whether tracked files differ from it, or `{head: null, dirty: false}` when git cannot
 * answer. Bounded and non-interactive for the same reasons as `#shared/domain/diff`: no credential
 * helper, no optional index lock, no unbounded child.
 */
async function revisionOf(repoRoot: string): Promise<Revision> {
  const head = (await gitOutput(repoRoot, ["rev-parse", "HEAD"]))?.trim() ?? "";
  if (head === "") {
    return { head: null, dirty: false };
  }
  const status = await gitOutput(repoRoot, [
    "status",
    "--porcelain",
    "--untracked-files=no",
    "--no-renames",
  ]);
  // `null` is "git could not answer"; a tree whose state cannot be read is not called dirty.
  return { head, dirty: status !== null && status.trim() !== "" };
}

async function gitOutput(repoRoot: string, args: readonly string[]): Promise<string | null> {
  let proc: ReturnType<typeof Bun.spawn<"ignore", "pipe", "pipe">>;
  try {
    proc = Bun.spawn({
      cmd: [
        "git",
        "--no-pager",
        "--no-optional-locks",
        "-c",
        "credential.helper=",
        "-c",
        "core.askpass=",
        "-C",
        repoRoot,
        ...args,
      ],
      cwd: repoRoot,
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
    });
  } catch {
    return null;
  }

  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<null>((report) => {
    timer = setTimeout(() => {
      proc.kill("SIGKILL");
      report(null);
    }, GIT_TIMEOUT_MS);
  });
  const collected = Promise.all([new Response(proc.stdout).text(), proc.exited]).then(
    ([stdout, code]): string | null => (code === 0 ? stdout : null),
  );
  try {
    return await Promise.race([collected, expired]);
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Writes one generation into the stage. The order is fixed and load-bearing:
 *
 * 1. tombstones and row drops for paths that left the tree — the tombstone reads the node rows, so
 *    it comes before the delete that cascades them;
 * 2. `file` rows for every discovered path, including the ones whose parse failed: the row is the
 *    staleness manifest, so a file that cannot be parsed is still a file we have seen;
 * 3. the previous rows of a failed parse are cleared, because that file no longer contributes;
 * 4. every unit is persisted, which replaces its own nodes, edges and unresolved sites;
 * 5. {@link restoreEdges} re-inserts the resolved edges whose ends the cascades in (4) removed;
 * 6. coverage is rewritten from discovery plus parse failures;
 * 7. `finalizeGeneration` derives metrics/FTS/tombstone names and records the row, and the run's
 *    own `meta` state is written in the same transaction so it publishes atomically.
 */
function writeGeneration(tx: WriteTx, sqlite: Database, write: GenerationWrite): GenerationResult {
  const lastSeen = readMeta(sqlite, META_GENERATION) ?? PENDING_GENERATION;

  const tombstoneFile = sqlite.query<unknown, [string, string, string]>(TOMBSTONE_FILE);
  const dropFile = sqlite.query<unknown, [string]>("DELETE FROM file WHERE path = ?");
  for (const path of write.removed) {
    tombstoneFile.run(lastSeen, PENDING_GENERATION, path);
    dropFile.run(path);
  }

  const upsertFile = sqlite.query<
    unknown,
    [string, string, string, number, number, string, string | null, string]
  >(
    "INSERT INTO file (path, language, sha256, size, line_count, parse_status," +
      " parse_error_ranges, indexed_generation) VALUES (?, ?, ?, ?, ?, ?, ?, ?)" +
      " ON CONFLICT(path) DO UPDATE SET language = excluded.language, sha256 = excluded.sha256," +
      " size = excluded.size, line_count = excluded.line_count," +
      " parse_status = excluded.parse_status," +
      " parse_error_ranges = excluded.parse_error_ranges," +
      " indexed_generation = excluded.indexed_generation",
  );
  for (const parsed of write.parsed) {
    upsertFile.run(
      parsed.file.path,
      parsed.file.lang,
      parsed.file.sha256,
      parsed.file.size,
      parsed.lineCount,
      parsed.parseStatus,
      parsed.errorRanges.length === 0 ? null : parsed.errorRanges.join(","),
      write.generationId,
    );
  }

  const dropNodes = sqlite.query<unknown, [string]>(
    "DELETE FROM node WHERE file_id = (SELECT id FROM file WHERE path = ?)",
  );
  const dropEdges = sqlite.query<unknown, [string]>(
    "DELETE FROM edge WHERE file_id = (SELECT id FROM file WHERE path = ?)",
  );
  const dropUnresolved = sqlite.query<unknown, [string]>(
    "DELETE FROM unresolved WHERE file_id = (SELECT id FROM file WHERE path = ?)",
  );
  for (const parsed of write.parsed) {
    if (parsed.unit !== null) {
      continue;
    }
    dropEdges.run(parsed.file.path);
    dropUnresolved.run(parsed.file.path);
    dropNodes.run(parsed.file.path);
  }

  // Every parsed unit, in discovery order, inside the one `BEGIN IMMEDIATE` the generation is.
  const bindings = bindingsByFile(write.resolved);
  for (const parsed of write.parsed) {
    const unit = parsed.unit;
    if (unit === null) {
      continue;
    }
    persistUnit(tx, unit, bindings.get(normalizeRel(unit.relPath)) ?? EMPTY_BINDING);
  }

  restoreEdges(sqlite, write.resolved);
  rewriteCoverage(sqlite, write);

  const result = finalizeGeneration(tx, {
    generationId: write.generationId,
    baseCommit: write.revision.head,
    mode: write.mode,
    durationMs: write.durationMs,
    worktreeDirty: write.revision.dirty,
    project: write.identity,
  });
  writeMeta(sqlite, META_INDEXER_STATE, STATE_IDLE);
  writeMeta(sqlite, META_CONSECUTIVE_FAILURES, "0");
  if (write.rebuildRequired) {
    // `migrate()` reported it and this full rebuild just honoured it; leaving the flag set would
    // force every later run to rebuild as well.
    writeMeta(sqlite, META_REBUILD_REQUIRED, "0");
  }

  return result;
}

const EMPTY_BINDING: BindResult = { edges: [], unresolved: [] };

interface FileBinding {
  readonly edges: ResolvedEdge[];
  readonly unresolved: UnresolvedBinding[];
}

/** Splits one whole-graph bind result into the per-unit shape `persistUnit` takes. */
function bindingsByFile(resolved: BindResult): Map<string, FileBinding> {
  const byFile = new Map<string, { edges: ResolvedEdge[]; unresolved: UnresolvedBinding[] }>();
  for (const edge of resolved.edges) {
    entryFor(byFile, edge.file).edges.push(edge);
  }
  for (const site of resolved.unresolved) {
    entryFor(byFile, site.file).unresolved.push(site);
  }
  return byFile;
}

function entryFor(
  byFile: Map<string, { edges: ResolvedEdge[]; unresolved: UnresolvedBinding[] }>,
  file: string,
): { edges: ResolvedEdge[]; unresolved: UnresolvedBinding[] } {
  const path = normalizeRel(file);
  const existing = byFile.get(path);
  if (existing !== undefined) {
    return existing;
  }
  const created = { edges: [] as ResolvedEdge[], unresolved: [] as UnresolvedBinding[] };
  byFile.set(path, created);
  return created;
}

/**
 * Re-inserts every resolved edge whose two ends exist, once the whole unit set is written.
 *
 * `persistUnit` deletes a file's nodes before it inserts the replacements, and `edge.dst_id`
 * cascades: an edge written by an earlier unit into a file persisted later is gone by the time that
 * file's nodes exist again. Re-inserting the full resolved set here — after every node row is
 * final, with `ON CONFLICT DO NOTHING` against the edge's unique key — makes the generation's edge
 * set independent of the order the units happened to be persisted in. An edge whose end is missing
 * (the file left the tree, or its parse failed) is dropped, never written dangling.
 */
function restoreEdges(sqlite: Database, resolved: BindResult): void {
  if (resolved.edges.length === 0) {
    return;
  }
  const nodeIds = new Map<string, number>();
  for (const row of sqlite.query<{ id: number; qn: string }, []>("SELECT id, qn FROM node").all()) {
    nodeIds.set(row.qn, row.id);
  }
  const fileIds = new Map<string, number>();
  for (const row of sqlite
    .query<{ id: number; path: string }, []>("SELECT id, path FROM file")
    .all()) {
    fileIds.set(row.path, row.id);
  }

  const insert = sqlite.query<
    unknown,
    [number, number, string, string, number, number, string, string]
  >(
    "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)" +
      " VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
  );
  for (const edge of resolved.edges) {
    const src = nodeIds.get(edge.src);
    const dst = nodeIds.get(edge.dst);
    const fileId = fileIds.get(normalizeRel(edge.file));
    if (src === undefined || dst === undefined || fileId === undefined) {
      continue;
    }
    insert.run(src, dst, edge.kind, edge.tier, fileId, edge.line, edge.localName, edge.evidence);
  }
}

/**
 * Rewrites `not_indexed` from this run's discovery walk and parse failures. Discovery always walks
 * the whole tree, so the replacement is exact: a path that became indexable loses its old reason
 * instead of keeping a coverage row that contradicts the graph.
 */
function rewriteCoverage(sqlite: Database, write: GenerationWrite): void {
  sqlite.query<unknown, []>("DELETE FROM not_indexed").run();
  const insert = sqlite.query<unknown, [string, string, string | null, string | null]>(
    "INSERT INTO not_indexed (path, reason, detail, language) VALUES (?, ?, ?, ?)" +
      " ON CONFLICT DO NOTHING",
  );

  for (const entry of write.discovered.notIndexed) {
    insert.run(entry.path, entry.reason, null, entry.language ?? null);
  }
  for (const parsed of write.parsed) {
    const failure = parsed.failure;
    if (failure === null) {
      continue;
    }
    insert.run(failure.path, failure.reason, failure.detail ?? null, failure.language ?? null);
  }
}

function readMeta(handle: Database, key: string): string | null {
  const row = handle
    .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
    .get(key);
  return row?.value ?? null;
}

function writeMeta(handle: Database, key: string, value: string): void {
  handle.query<unknown, [string, string]>(META_UPSERT).run(key, value);
}

/**
 * Records a failed run in the live graph's `meta`: `indexer_state = failed` and one more
 * `consecutive_failures`, which is what `status.health` reports even though no generation was
 * published (`DATABASE.md` §5). Best-effort by design — the run's exit code is already decided, and
 * a database that cannot be opened must not turn a clean 1 into a crash.
 */
function recordFailure(livePath: string, error: unknown): void {
  if (!existsSync(livePath)) {
    // Nothing was ever migrated here, so there is no `meta` to write and no graph to mislead.
    return;
  }

  // A plain handle, not a write scope: this is a two-row `meta` note about the published file, not
  // a generation. It must not switch the file's journal mode or leave a WAL beside it, and it must
  // not draw a `journal_mode = wal` refusal over a graph that is otherwise fine.
  let handle: Database | null = null;
  try {
    handle = new Database(livePath);
    // A reader that still holds the file must not make the note fail.
    handle.exec("PRAGMA busy_timeout = 5000");
    const table = handle
      .query<{ name: string }, []>(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
      )
      .get();
    if (table === null) {
      return;
    }
    const previous = handle
      .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
      .get(META_CONSECUTIVE_FAILURES);
    const parsed = Number.parseInt(previous?.value ?? "0", 10);
    const failures = (Number.isFinite(parsed) ? parsed : 0) + 1;
    const upsert = handle.query<unknown, [string, string]>(META_UPSERT);
    upsert.run(META_CONSECUTIVE_FAILURES, String(failures));
    upsert.run(META_INDEXER_STATE, STATE_FAILED);
    log.warn("worker.failure_recorded", { livePath, failures, reason: messageOf(error) });
  } catch (metaError) {
    log.error("worker.failure_not_recorded", { livePath, reason: messageOf(metaError) });
  } finally {
    handle?.close();
  }
}
