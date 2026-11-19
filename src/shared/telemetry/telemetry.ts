/**
 * The recorder: one per process, and the only owner of the identity, the `seq` counter, the buffer
 * and the sink.
 *
 * The shape is a consequence of two constraints. First, a tool call must never fail because
 * telemetry did: `recordCall` appends to an array and returns, and every sink call is wrapped, so a
 * full disk or a read-only data dir costs records, never an answer. Second, a flush must be one
 * append per daily file — the server and the index worker it spawns hold separate recorders over
 * the same `<dir>/<date>.jsonl`, so the buffer is grouped by date and each group is handed to the
 * sink in a single call, which makes interleaving line-atomic.
 *
 * There is no interval: the age trigger is evaluated on the next record and reads the injected
 * clock, so nothing here keeps the process alive and a test can prove the trigger without sleeping.
 */
import { existsSync, statSync } from "node:fs";
import { join } from "node:path";
import type { TelemetryConfig } from "#shared/config";
import { type Logger, log } from "#shared/log";
import { newSessionId, resolveInstallId, utcDateOf } from "./identity";
import {
  type CallSpec,
  callRecordOf,
  type IndexRunSpec,
  indexRunRecordOf,
  type RecordContext,
  type RepoSizeBucket,
  repoSizeBucketOf,
  rootKindOf,
  type SessionFacts,
  type SessionMode,
  sessionRecordOf,
  type TelemetryRecord,
} from "./record";
import { jsonlSink, type TelemetrySink } from "./sink";

/** Records buffered before a flush. Bounds a lost tail and caps one `O_APPEND` write. */
export const FLUSH_AT_RECORDS = 32;

/** Age of the oldest buffered record that forces a flush, in milliseconds. */
export const FLUSH_AT_MS = 5_000;

export interface TelemetryOptions {
  /** The telemetry directory, always `AppConfig.telemetry.dir`; the recorder never re-derives it. */
  readonly dir: TelemetryConfig["dir"];
  readonly repoRoot: string;
  readonly dataDir: string;
  readonly sessionMode: SessionMode;
  readonly serverVersion: string;
  readonly graphSchemaVersion: number;
  readonly sink?: TelemetrySink;
  readonly now?: () => Date;
  readonly log?: Logger;
}

export interface Telemetry {
  readonly installId: string;
  readonly sessionId: string;
  recordCall(spec: CallSpec): void;
  recordIndexRun(spec: IndexRunSpec): void;
  flush(): void;
  close(): void;
}

/** One serialised record, with the date its own `ts` falls on, so a flush can group without a clock. */
interface BufferedLine {
  readonly date: string;
  readonly line: string;
  /** The clock reading of the record itself; the age trigger never takes a second reading. */
  readonly at: number;
}

export function createTelemetry(options: TelemetryOptions): Telemetry {
  const clock = options.now ?? (() => new Date());
  const logger = options.log ?? log;
  const sink = options.sink ?? jsonlSink(options.dir);

  const identity = resolveInstallId(options.dir, clock());
  const sessionId = newSessionId();

  let seq = 0;
  let closed = false;
  let degraded = false;
  let closeReported = false;
  const buffer: BufferedLine[] = [];

  // The session record is the denominator every later record is read against, so it is buffered
  // before the caller can make a call. It carries no `seq` (the schema has none for it) and goes
  // straight to the buffer rather than through `recordCall`.
  const opened = clock();
  append(sessionRecordOf(contextOf(opened), sessionFacts()), opened);

  return {
    installId: identity.installId,
    sessionId,
    recordCall,
    recordIndexRun,
    flush,
    close,
  };

  function contextOf(at: Date): RecordContext {
    return {
      ts: at.toISOString(),
      installId: identity.installId,
      sessionId,
      seq,
      serverVersion: options.serverVersion,
      bunVersion: Bun.version,
      os: process.platform,
      arch: process.arch,
      graphSchemaVersion: options.graphSchemaVersion,
    };
  }

  function sessionFacts(): SessionFacts {
    return {
      sessionMode: options.sessionMode,
      isFirstRun: identity.isFirstRun,
      repoSizeBucket: repoSizeBucketFor(options.dataDir),
      // A process that never walked the tree cannot name its languages; `index_run.languages` is
      // the run-level truth (plan.approach.md Design 5).
      languages: [],
      rootKind: rootKindOf(existsSync(join(options.repoRoot, ".git"))),
    };
  }

  function recordCall(spec: CallSpec): void {
    if (!accepts()) {
      return;
    }
    const at = clock();
    seq += 1;
    append(callRecordOf(contextOf(at), spec), at);
  }

  function recordIndexRun(spec: IndexRunSpec): void {
    if (!accepts()) {
      return;
    }
    const at = clock();
    seq += 1;
    append(indexRunRecordOf(contextOf(at), spec), at);
  }

  function append(record: TelemetryRecord, at: Date): void {
    buffer.push({
      date: utcDateOf(new Date(record.ts)),
      line: JSON.stringify(record),
      at: at.getTime(),
    });
    maybeFlush(at.getTime());
  }

  function maybeFlush(nowMs: number): void {
    const oldest = buffer[0];
    const aged = oldest !== undefined && nowMs - oldest.at >= FLUSH_AT_MS;
    if (buffer.length >= FLUSH_AT_RECORDS || aged) {
      flush();
    }
  }

  function flush(): void {
    if (degraded || closed || buffer.length === 0) {
      return;
    }
    const groups = new Map<string, string[]>();
    for (const entry of buffer) {
      const lines = groups.get(entry.date);
      if (lines === undefined) {
        groups.set(entry.date, [entry.line]);
      } else {
        lines.push(entry.line);
      }
    }
    // Consumed before the writes, and never re-queued on failure: a flush that straddles UTC
    // midnight can have its first group already on disk, and a retry would duplicate those lines
    // in a file that is append-only.
    buffer.length = 0;
    try {
      for (const [date, lines] of groups) {
        sink.write(date, lines);
      }
    } catch (error) {
      degraded = true;
      logger.warn("telemetry.sink_failed", { reason: reasonOf(error) });
    }
  }

  function close(): void {
    if (closed) {
      return;
    }
    flush();
    closed = true;
  }

  /**
   * `false` once the recorder can no longer produce a line. A degraded recorder stays silent: the
   * one `warn` already named the failure, and one line per dropped record would turn a full disk
   * into a log flood. A closed one reports itself exactly once.
   */
  function accepts(): boolean {
    if (degraded) {
      return false;
    }
    if (!closed) {
      return true;
    }
    if (!closeReported) {
      closeReported = true;
      logger.debug("telemetry.record_after_close");
    }
    return false;
  }
}

/**
 * A stat, not a walk: bucketing the real file count would need the indexer's discovery, which
 * `shared-isolation` forbids, and it would run on every MCP start. An absent or unreadable graph is
 * `"none"` — the same bucket a zero-byte file gets — because "no graph yet" is what it means.
 */
function repoSizeBucketFor(dataDir: string): RepoSizeBucket {
  try {
    return repoSizeBucketOf(statSync(join(dataDir, "graph.db")).size);
  } catch {
    return repoSizeBucketOf(0);
  }
}

/** The errno code when there is one — `ENOSPC` is the fact; the message repeats a path. */
function reasonOf(error: unknown): string {
  const code = (error as { code?: unknown }).code;
  if (typeof code === "string" && code !== "") {
    return code;
  }
  return error instanceof Error ? error.message : String(error);
}

/**
 * What a caller holds when `config.telemetry.enabled` is false or no recorder was supplied: the
 * same interface with no destination, so the call sites carry no conditional.
 */
export function nullTelemetry(): Telemetry {
  return {
    installId: "",
    sessionId: "",
    recordCall: noop,
    recordIndexRun: noop,
    flush: noop,
    close: noop,
  };
}

function noop(): void {
  // Nothing is recorded and nothing is flushed; the seam is the interface, not the behaviour.
}
