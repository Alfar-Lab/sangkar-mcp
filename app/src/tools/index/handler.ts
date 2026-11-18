/**
 * `index`: spawn the worker and report what it published (`TOOLS-SPEC.md` §8).
 *
 * The tool never indexes. The writer lives in exactly one process (`TECH-SPEC.md` §8), so this
 * handler spawns `bin/codegraph.ts worker` and reads that child's stderr; a WASM parser that runs
 * out of memory kills the worker, not the MCP session that answered the call. That is also why
 * this file imports no indexer module and no database module: a tool may reach only `#shared/*`,
 * and the repair path is the last place to discover a boundary leak.
 *
 * The child's whole reporting surface is JSON log lines on stderr — its stdout stays empty because
 * stdout above it carries the protocol. So the report this tool reads is:
 *
 * - one `worker.published` line on success, carrying the new generation, the offered file count,
 *   the edge count, the duration and the mode;
 * - one `worker.nothing_to_do` line when the tree already matches the graph;
 * - one `worker.locked` line when another indexer holds the write lock;
 * - one `worker.file_not_parsed` warning per file the parser could not handle.
 *
 * Anything the child does not report is not invented here: it logs no `not_indexed` count and no
 * unresolved-edge count, so `coverage.complete` is always `false` on an `index` response and the
 * tool claims only the numbers it was told. `status` is the tool that answers coverage from the
 * database.
 */
import { existsSync } from "node:fs";
import { join } from "node:path";
import { emit } from "#shared/budget";
import {
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  ok,
  type ReadStatus,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import type { LogLevel } from "#shared/log";
import { lockPath } from "#shared/store/lock";
import type { ToolContext } from "#shared/store/read/context";
import { type IndexArgs, type IndexMode, input } from "./manifest";

/**
 * The application root, from this file's own location: `src/tools/index/` up three levels. Not
 * from `process.cwd()` (a server's cwd is not its install) and not from an environment variable (a
 * handler never reads one; the two paths that matter arrive in `ctx.config`).
 */
const APP_ROOT = join(import.meta.dir, "..", "..", "..");

/** The CLI entry the worker mode lives behind, relative to {@link APP_ROOT}. */
const CLI_ENTRY = join("bin", "codegraph.ts");

/** A child's last line is the only diagnosis there is; cap it so a stack trace cannot bloat. */
const MAX_ERROR_LINE_CHARS = 400;

export interface IndexFileCounts {
  /** Paths the discovery walk offered to the parser. */
  readonly scanned: number;
  /** Offered paths that produced a unit; the ones that did not logged `worker.file_not_parsed`. */
  readonly parsed: number;
  readonly failed: number;
  /** Paths discovery excluded; the child logs no count for those, so this is 0 (see module doc). */
  readonly skipped: number;
}

export interface IndexPayload {
  /** The mode the run published under, which can differ from the request after a forced rebuild. */
  readonly mode: IndexMode;
  readonly duration_ms: number;
  /** The generation that was live when the run started; `null` on a graph with no publish yet. */
  readonly previous_generation: string | null;
  readonly files: IndexFileCounts;
}

/** One log line from the child, already classified and forwarded to `ctx.log` by the runner. */
export interface WorkerEvent {
  readonly msg: string;
  readonly level: LogLevel;
  /** The line's fields without `ts`/`level`/`msg`, which are the envelope of the log line itself. */
  readonly fields: Record<string, unknown>;
  /** The line exactly as written, for the diagnosis an exit code cannot carry. */
  readonly raw: string;
}

export interface WorkerReport {
  readonly exitCode: number;
  /** Every stderr line, in arrival order: the child's report, not just its last line. */
  readonly events: readonly WorkerEvent[];
}

/** The resolved child invocation: an argv and the environment it must resolve the same config in. */
export interface WorkerInvocation {
  readonly cmd: readonly string[];
  readonly env: Record<string, string | undefined>;
}

/** A call's arguments after {@link input}'s defaults are applied. */
export interface WorkerArgs {
  readonly force: boolean;
  readonly mode: IndexMode;
}

/**
 * The spawn seam. `streamWorker` is the real one; a test supplies a runner that returns canned
 * events so the exit-code mapping is asserted without a child process. The signature carries
 * `ctx`, because streaming the child's stderr into the server's log is part of running it.
 */
export type WorkerRunner = (
  ctx: ToolContext,
  invocation: WorkerInvocation,
) => Promise<WorkerReport>;

/** The generation that is live *before* the child runs, read through the injected reader. */
interface LiveSnapshot {
  readonly generation: string | null;
  readonly indexedAt: string | null;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function resolveArgs(args: IndexArgs): WorkerArgs {
  return { force: args.force ?? input.force.default, mode: args.mode ?? input.mode.default };
}

/**
 * The child command, and the only place the two paths it acts on are decided.
 *
 * `[execPath, "run", bin/codegraph.ts, "worker", …]` is the source-tree form. In a compiled binary
 * (`bun build --compile`, `app/package.json`'s `build`) there is no `bin/codegraph.ts` on disk and
 * `execPath` *is* the server, so the same mode is addressed as `[execPath, "worker", …]`; the
 * entry's existence is what distinguishes the two, and that is a file check rather than a guess
 * about the build.
 *
 * The tree and the data dir travel as `--repo-root`/`--data-dir`, the two settings `loadConfig()`
 * already resolves, so the child can only ever index the repository the server is bound to — never
 * its own cwd. Inheriting the rest of the parent environment is not a config read: the child shells
 * out to `git` and needs `PATH`.
 */
export function workerInvocation(ctx: ToolContext, args: WorkerArgs): WorkerInvocation {
  const entry = join(APP_ROOT, CLI_ENTRY);
  const prefix = existsSync(entry) ? [process.execPath, "run", entry] : [process.execPath];
  const cmd = [
    ...prefix,
    "worker",
    "--repo-root",
    ctx.config.repoRoot,
    "--data-dir",
    ctx.config.dataDir,
    "--mode",
    args.mode,
  ];
  if (args.force) {
    cmd.push("--force");
  }
  return { cmd, env: { ...process.env } };
}

function asLevel(value: unknown): LogLevel {
  return value === "debug" || value === "info" || value === "warn" || value === "error"
    ? value
    : "info";
}

/**
 * One stderr line as an event. A line that is not JSON is kept whole: an uncaught panic or a Bun
 * banner is exactly the text an exit 1 has to report, and dropping it would leave the failure with
 * a code and no reason.
 */
function eventOf(line: string): WorkerEvent {
  let parsed: unknown = null;
  try {
    parsed = JSON.parse(line);
  } catch {
    parsed = null;
  }
  if (typeof parsed !== "object" || parsed === null) {
    return { msg: "worker.stderr", level: "warn", fields: { line }, raw: line };
  }

  const record = parsed as Record<string, unknown>;
  const msg = typeof record.msg === "string" && record.msg !== "" ? record.msg : "worker.log";
  const fields: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(record)) {
    if (key !== "ts" && key !== "level" && key !== "msg") {
      fields[key] = value;
    }
  }
  return { msg, level: asLevel(record.level), fields, raw: line };
}

/** Forwards one line under the child's own message name, so its report is greppable server-side. */
function forward(ctx: ToolContext, event: WorkerEvent): void {
  ctx.log[event.level](event.msg, event.fields);
}

/**
 * Spawns the worker, streams its stderr into `ctx.log` line by line, and returns the whole report.
 *
 * Streamed rather than buffered so an index that runs for a minute is visible while it runs: the
 * worker's progress lines are the only window into a child that takes 40 s on the reference
 * corpus. `stdout` is discarded on purpose — the child must not be able to write into the stream
 * the MCP protocol owns.
 */
export async function streamWorker(
  ctx: ToolContext,
  invocation: WorkerInvocation,
): Promise<WorkerReport> {
  const child: Bun.Subprocess<"ignore", "ignore", "pipe"> = Bun.spawn({
    cmd: [...invocation.cmd],
    stdin: "ignore",
    stdout: "ignore",
    stderr: "pipe",
    env: invocation.env,
  });

  const events: WorkerEvent[] = [];
  const decoder = new TextDecoder();
  const reader = (child.stderr as ReadableStream<Uint8Array>).getReader();
  let buffered = "";
  try {
    for (;;) {
      const chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      buffered += decoder.decode(chunk.value, { stream: true });
      let newline = buffered.indexOf("\n");
      while (newline !== -1) {
        const line = buffered.slice(0, newline).trimEnd();
        buffered = buffered.slice(newline + 1);
        if (line !== "") {
          const event = eventOf(line);
          events.push(event);
          forward(ctx, event);
        }
        newline = buffered.indexOf("\n");
      }
    }
  } finally {
    reader.releaseLock();
  }
  // A last line without its newline: the child was killed, or exited mid-line.
  const tail = `${buffered}${decoder.decode()}`.trim();
  if (tail !== "") {
    const event = eventOf(tail);
    events.push(event);
    forward(ctx, event);
  }

  return { exitCode: await child.exited, events };
}

/**
 * The generation that is live now, from the injected reader. Best effort by design: a graph broken
 * enough to need this tool is exactly when `metaFor()` throws (a missing table, a refused open),
 * and the repair path must not be blocked by the thing it was called to repair.
 */
function liveSnapshot(ctx: ToolContext): LiveSnapshot {
  try {
    const meta = ctx.reader.metaFor();
    return { generation: meta.generation, indexedAt: meta.indexedAt };
  } catch (error) {
    ctx.log.warn("index.live_generation_unavailable", { reason: messageOf(error) });
    return { generation: null, indexedAt: null };
  }
}

/**
 * `read_status` for a call that changed nothing: the snapshot the reader could give, marked stale
 * when the graph has no published generation at all (the one state where `status` answers
 * `bootstrapping`, and the honest reason a repair was asked for). An unpublished generation has no
 * id, so `generation` is empty rather than a made-up one.
 */
function readStatusOfLive(snapshot: LiveSnapshot, now: Date): ReadStatus {
  const status: ReadStatus = {
    generation: snapshot.generation ?? "",
    indexed_at: snapshot.indexedAt ?? "",
    stale: snapshot.generation === null,
    stale_checked_at: now.toISOString(),
  };
  if (snapshot.generation === null) {
    status.stale_reason = "the graph has no published generation yet";
  }
  return status;
}

/** `read_status` for a call whose run did not publish: the live generation, and why it is stale. */
function readStatusOfUnpublished(snapshot: LiveSnapshot, now: Date): ReadStatus {
  const status = readStatusOfLive(snapshot, now);
  status.stale = true;
  status.stale_reason = "the index run did not publish; the live generation is unchanged";
  return status;
}

/**
 * The run's own accounting as the envelope's coverage. `complete` is always `false`: the child
 * reports offered, failed and published files and nothing about `not_indexed` or unresolved edges,
 * so this response may claim neither a complete graph nor an empty gap list.
 *
 * A run that published nothing reports zeros, and `complete: false` is what makes them read as
 * "this response measured nothing" rather than "the graph is empty".
 */
function coverageOf(files: IndexFileCounts): Coverage {
  return {
    complete: false,
    indexed: files.parsed,
    skipped: files.skipped,
    failed: files.failed,
    unresolved_edges: 0,
    not_indexed: [],
  };
}

function numberIn(event: WorkerEvent | null, key: string): number | null {
  const value = event?.fields[key];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function stringIn(event: WorkerEvent | null, key: string): string | null {
  const value = event?.fields[key];
  return typeof value === "string" && value !== "" ? value : null;
}

/** The newest event with this `msg`; the child logs one per outcome, and the last one wins. */
function lastEvent(report: WorkerReport, msg: string): WorkerEvent | null {
  for (let index = report.events.length - 1; index >= 0; index -= 1) {
    const event = report.events[index];
    if (event !== undefined && event.msg === msg) {
      return event;
    }
  }
  return null;
}

function countEvents(report: WorkerReport, msg: string): number {
  let total = 0;
  for (const event of report.events) {
    if (event.msg === msg) {
      total += 1;
    }
  }
  return total;
}

/**
 * The line an exit 1 reports. The child's own `worker.failed` line carries the reason; failing
 * that, the last line it wrote is the crash text (`worker.stderr` holds a non-JSON line whole), and
 * failing that there is nothing to quote and the message says so.
 */
function lastErrorLine(report: WorkerReport): string {
  const failed = lastEvent(report, "worker.failed");
  const reason = stringIn(failed, "reason");
  if (reason !== null) {
    return reason.slice(0, MAX_ERROR_LINE_CHARS);
  }
  const source = failed ?? lastEventLevelError(report) ?? lastOf(report);
  return source === null
    ? "the worker exited without a line on stderr"
    : source.raw.slice(0, MAX_ERROR_LINE_CHARS);
}

function lastEventLevelError(report: WorkerReport): WorkerEvent | null {
  for (let index = report.events.length - 1; index >= 0; index -= 1) {
    const event = report.events[index];
    if (event !== undefined && event.level === "error") {
      return event;
    }
  }
  return null;
}

function lastOf(report: WorkerReport): WorkerEvent | null {
  return report.events.at(-1) ?? null;
}

/**
 * `emit` mints `next` only when it dropped an item, and this payload has no list to drop: its one
 * array is `coverage.not_indexed`, empty by construction, and `files` is an object of counts.
 * Sizing therefore either returns the envelope unchanged or cannot shrink it at all — a repair
 * report is one screen. The mint is unreachable, and it throws rather than inventing a token a
 * client could not resume: a list added to this payload later must decide about paging instead of
 * inheriting a meaningless cursor.
 */
function unreachableMint(position: number): string {
  throw new AppError(
    "internal",
    `index has no cursor, but ${position} item(s) were dropped from its payload`,
  );
}

/**
 * The budget rule for a payload without lists. `index` has no `budget` argument (`TOOLS-SPEC.md`
 * §10), so the configured fallback applies; `emit` is still the one place sizing happens, and it
 * returns the envelope unchanged when there is nothing to cut.
 */
function sized<T>(ctx: ToolContext, env: Envelope<T>): Envelope<T> | Envelope<null> {
  try {
    return emit(env, ctx.config.budgets.defaultTokens, { mint: unreachableMint });
  } catch (error) {
    // A refusal here (`budget_too_large` for a misconfigured ceiling) must still be an envelope.
    return fail(error, {
      summary: env.summary,
      read_status: env.read_status,
      coverage: env.coverage,
    });
  }
}

/** A string as the payload's mode, when the child named one the manifest knows. */
function asMode(value: string | null): IndexMode | null {
  return value === "full" || value === "fast" ? value : null;
}

function seconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)}s`;
}

/** A published run: the counts the child printed, and the generation it says it published. */
function published(
  ctx: ToolContext,
  report: WorkerReport,
  args: WorkerArgs,
  snapshot: LiveSnapshot,
): Envelope<IndexPayload | null> {
  const line = lastEvent(report, "worker.published");
  if (line === null) {
    return failedInternal(
      ctx,
      snapshot,
      new AppError("internal", "the index worker exited 0 without publishing a report", {
        hint: "run `cgraph index` and read its stderr: the worker's last line is the report",
      }),
    );
  }

  const scanned = numberIn(line, "files") ?? 0;
  const failed = countEvents(report, "worker.file_not_parsed");
  // Every offered path either produced a unit or logged a parse failure, so this subtraction is
  // exact over the set the child reported; discovery's excluded paths never reach the parser.
  const files: IndexFileCounts = {
    scanned,
    parsed: Math.max(0, scanned - failed),
    failed,
    skipped: 0,
  };
  const mode = asMode(stringIn(line, "mode")) ?? args.mode;
  const durationMs = numberIn(line, "durationMs") ?? 0;
  const edges = numberIn(line, "edges");
  const now = ctx.now();
  const generation = stringIn(line, "generation") ?? snapshot.generation ?? "";

  const payload: IndexPayload = {
    mode,
    duration_ms: durationMs,
    previous_generation: snapshot.generation,
    files,
  };
  const spec: EnvelopeSpec = {
    summary:
      `${mode === "full" ? "Full" : "Fast"} index published: ${files.parsed} file(s) parsed, ` +
      `${files.failed} failed${edges === null ? "" : `, ${edges} edge(s)`} ` +
      `in ${seconds(durationMs)}.`,
    // The child names the generation it published but not that generation's `published_at`, and a
    // success is fresh by definition: the run just finished, so this clock is the publish time to
    // within the run's own tail.
    read_status: {
      generation,
      indexed_at: now.toISOString(),
      stale: false,
      stale_checked_at: now.toISOString(),
    },
    coverage: coverageOf(files),
  };
  return sized(ctx, ok(payload, spec));
}

/** Exit 2: the tree already matches the graph, so nothing was published and nothing was parsed. */
function nothingToDo(
  ctx: ToolContext,
  report: WorkerReport,
  snapshot: LiveSnapshot,
  args: WorkerArgs,
  elapsedMs: number,
): Envelope<IndexPayload | null> {
  const scanned = numberIn(lastEvent(report, "worker.nothing_to_do"), "files") ?? 0;
  const files: IndexFileCounts = { scanned, parsed: 0, failed: 0, skipped: 0 };
  const payload: IndexPayload = {
    mode: args.mode,
    duration_ms: elapsedMs,
    previous_generation: snapshot.generation,
    files,
  };
  const spec: EnvelopeSpec = {
    summary: `Nothing to do: the graph already matches the working tree (${scanned} files scanned).`,
    read_status: readStatusOfLive(snapshot, ctx.now()),
    coverage: coverageOf(files),
  };
  return sized(ctx, ok(payload, spec));
}

/** Exit 75: another indexer holds the lock, which is a retry, not a failure of this call. */
function locked(
  ctx: ToolContext,
  report: WorkerReport,
  snapshot: LiveSnapshot,
): Envelope<IndexPayload | null> {
  const line = lastEvent(report, "worker.locked");
  const retryAfterMs = numberIn(line, "retryAfterMs");
  const holder = numberIn(line, "pid");
  const error = new AppError(
    "index_locked",
    holder === null
      ? "another indexer is writing this graph"
      : `another indexer (pid ${holder}) is writing this graph`,
    {
      hint: `wait for it to finish and retry; the lock file is ${lockPath(ctx.config.dataDir)}`,
      retryable: true,
      ...(retryAfterMs === null ? {} : { retryAfterMs }),
    },
  );
  const spec: EnvelopeSpec = {
    summary: error.message,
    read_status: readStatusOfUnpublished(snapshot, ctx.now()),
    coverage: coverageOf({ scanned: 0, parsed: 0, failed: 0, skipped: 0 }),
  };
  return sized(ctx, fail(error, spec));
}

/**
 * Exit 1, and any code this tool does not expect: the run failed. The message is the child's own
 * diagnosis, because a generic "indexing failed" would throw away the only information the call
 * produced.
 */
function failedInternal(
  ctx: ToolContext,
  snapshot: LiveSnapshot,
  error: AppError,
): Envelope<IndexPayload | null> {
  const spec: EnvelopeSpec = {
    summary: error.message,
    read_status: readStatusOfUnpublished(snapshot, ctx.now()),
    coverage: coverageOf({ scanned: 0, parsed: 0, failed: 0, skipped: 0 }),
  };
  return sized(ctx, fail(error, spec));
}

/**
 * Runs the worker and maps its status byte to an envelope: 0 published, 2 nothing to do, 75 locked,
 * 1 and anything else failed. Never throws — a rejected spawn becomes the same `internal` failure
 * an exit 1 does, because a caller of a repair tool needs a reason, not an exception.
 *
 * `run` is the spawn seam: production passes nothing and gets {@link streamWorker}; a test supplies
 * a runner returning canned events, so the four mappings are asserted without an index. The
 * invocation a runner receives is still built by {@link workerInvocation} from `ctx.config`, which
 * is what keeps "the child indexes the tree the server was bound to" testable in the same file.
 */
export async function handle(
  ctx: ToolContext,
  args: IndexArgs,
  run: WorkerRunner = streamWorker,
): Promise<Envelope<IndexPayload | null>> {
  const resolved = resolveArgs(args);
  const snapshot = liveSnapshot(ctx);
  const startedAt = ctx.now().getTime();

  let report: WorkerReport;
  try {
    report = await run(ctx, workerInvocation(ctx, resolved));
  } catch (error) {
    return failedInternal(
      ctx,
      snapshot,
      new AppError("internal", `could not start the index worker: ${messageOf(error)}`, {
        hint: `the worker is ${CLI_ENTRY} inside ${APP_ROOT}`,
      }),
    );
  }
  const elapsedMs = Math.max(0, ctx.now().getTime() - startedAt);

  switch (report.exitCode) {
    case 0:
      return published(ctx, report, resolved, snapshot);
    case 2:
      return nothingToDo(ctx, report, snapshot, resolved, elapsedMs);
    case 75:
      return locked(ctx, report, snapshot);
    default:
      return failedInternal(
        ctx,
        snapshot,
        new AppError(
          "internal",
          `the index worker failed (exit ${report.exitCode}): ${lastErrorLine(report)}`,
          { hint: "run `cgraph index` and read its stderr for the full run" },
        ),
      );
  }
}
