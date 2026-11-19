/**
 * The one place a telemetry line's shape is decided (plan.approach.md "The record schema"). Every
 * field is copied out of an explicit whitelist: spreading `spec.args`, `spec.suppliedNames` or
 * `spec.envelope` would let a payload value — a repo path, a symbol name — reach the line through
 * whatever a handler happened to leave on the object, and a later sink could then invent a field.
 *
 * No IO and no clock: `ts` is injected so a record can be placed exactly in a test, and this module
 * stays importable from the server, the worker and the CLI without opening anything.
 */
import { BYTES_PER_TOKEN } from "#shared/budget";
import type { ToolArgs } from "#shared/cli/args";
import { STALE_REASONS, type StaleReason } from "#shared/domain/freshness";
import type { Envelope, EnvelopeStatus, TotalRelation, TruncationReason } from "#shared/envelope";

/**
 * The only signal a reader has that a field was removed or renamed. Adding a field keeps it at `1`
 * (a reader ignores the key it does not need); dropping one is a version bump.
 */
export const RECORD_VERSION = 1;

export type TelemetryKind = "call" | "index_run" | "session";

/**
 * `absent` is the one status no envelope can carry: the CLI answered successfully without a graph,
 * so there is nothing to read a status from. Folding it into the envelope's closed set keeps a
 * report matching one union rather than two.
 */
export type RecordStatus = EnvelopeStatus | "absent";

/**
 * `other` is what the envelope's prose collapses to. The envelope types `stale_reason` as a plain
 * string (`src/shared/envelope.ts:51`) and `trace` writes a repo path into it, so a verbatim copy
 * would put a private path in every stale record. The envelope is untouched; this is a recording
 * rule, and the five {@link STALE_REASONS} values stay greppable in a report.
 */
export type RecordedStaleReason = StaleReason | "other";

export type SessionMode = "mcp" | "cli" | "worker";
export type RepoSizeBucket = "none" | "xs" | "s" | "m" | "l" | "xl";
export type RootKind = "git" | "plain";

/**
 * One answer, as recorded. Every field is always present so a reader never distinguishes "unknown"
 * from "omitted"; the order below is the serialisation order.
 */
export interface CallRecord {
  ts: string;
  v: typeof RECORD_VERSION;
  kind: "call";
  install_id: string;
  session_id: string;
  seq: number;
  tool: string;
  status: RecordStatus;
  error_code: string | null;
  duration_ms: number;
  /** The names the request supplied, before defaults; `[]` means "the caller sent no arguments". */
  arg_names_set: string[];
  /** The budget in force, i.e. after the manifest default, because that is the one measured. */
  budget_tokens: number;
  payload_tokens: number;
  budget_used_ratio: number | null;
  truncated: boolean;
  truncation_reason: TruncationReason | null;
  total_relation: TotalRelation;
  coverage_complete: boolean;
  coverage_indexed: number;
  coverage_skipped: number;
  coverage_failed: number;
  unresolved_edges: number;
  stale: boolean;
  stale_reason: RecordedStaleReason | null;
  generation_age_ms: number | null;
  /** Top-level payload keys whose value is an array, mapped to its length; no value is copied. */
  result_counts: Record<string, number>;
  server_version: string;
  bun_version: string;
  os: string;
  arch: string;
  graph_schema_version: number;
}

/** One index run, as recorded. The server's own identity fields are left to `call`/`session`. */
export interface IndexRunRecord {
  ts: string;
  v: typeof RECORD_VERSION;
  kind: "index_run";
  install_id: string;
  session_id: string;
  mode: "full" | "fast";
  exit_code: number;
  duration_ms: number;
  was_forced: boolean;
  /** True only for a run that published: `newGenerationId()` is minted on the write path alone. */
  generation_advanced: boolean;
  files_seen: number;
  files_indexed: number;
  files_skipped: number;
  files_failed: number;
  nodes_by_kind: Readonly<Record<string, number>>;
  edges_by_kind_tier: Readonly<Record<string, number>>;
  unresolved_count: number;
  unresolved_ratio: number;
  languages: Readonly<Record<string, number>>;
  db_size_bytes: number;
  files_changed_in_run: number;
}

/** What a process looks like at start-up; the denominator every later record is read against. */
export interface SessionRecord {
  ts: string;
  v: typeof RECORD_VERSION;
  kind: "session";
  install_id: string;
  session_id: string;
  session_mode: SessionMode;
  server_version: string;
  bun_version: string;
  os: string;
  arch: string;
  is_first_run: boolean;
  repo_size_bucket: RepoSizeBucket;
  languages: string[];
  root_kind: RootKind;
}

export type TelemetryRecord = CallRecord | IndexRunRecord | SessionRecord;

/**
 * The facts a caller has already resolved — clock, identity, versions, machine. Building them is
 * the recorder's job (it owns the clock and the process), so a builder never reads one.
 */
export interface RecordContext {
  readonly ts: string;
  readonly installId: string;
  readonly sessionId: string;
  readonly seq: number;
  readonly serverVersion: string;
  readonly bunVersion: string;
  readonly os: string;
  readonly arch: string;
  readonly graphSchemaVersion: number;
}

export interface CallSpec {
  readonly tool: string;
  readonly args: ToolArgs;
  /** The request's own argument names, kept beside the defaulted `args` for {@link CallRecord}. */
  readonly suppliedNames: readonly string[];
  readonly envelope: Envelope<unknown> | null;
  readonly status: RecordStatus;
  readonly errorCode: string | null;
  readonly durationMs: number;
}

export interface IndexRunSpec {
  readonly mode: "full" | "fast";
  readonly exitCode: number;
  readonly durationMs: number;
  readonly wasForced: boolean;
  readonly filesSeen: number;
  readonly filesIndexed: number;
  readonly filesSkipped: number;
  readonly filesFailed: number;
  readonly nodesByKind: Readonly<Record<string, number>>;
  readonly edgesByKindTier: Readonly<Record<string, number>>;
  readonly unresolvedCount: number;
  readonly languages: Readonly<Record<string, number>>;
  readonly dbSizeBytes: number;
  readonly filesChangedInRun: number;
}

export interface SessionFacts {
  readonly sessionMode: SessionMode;
  readonly isFirstRun: boolean;
  readonly repoSizeBucket: RepoSizeBucket;
  readonly languages: readonly string[];
  readonly rootKind: RootKind;
}

export function callRecordOf(ctx: RecordContext, spec: CallSpec): CallRecord {
  const env = spec.envelope;
  const payloadTokens =
    env === null ? 0 : Math.ceil(Buffer.byteLength(JSON.stringify(env), "utf8") / BYTES_PER_TOKEN);
  const budgetTokens = budgetTokensOf(spec.args);
  return {
    ts: ctx.ts,
    v: RECORD_VERSION,
    kind: "call",
    install_id: ctx.installId,
    session_id: ctx.sessionId,
    seq: ctx.seq,
    tool: spec.tool,
    status: spec.status,
    error_code: spec.errorCode,
    duration_ms: spec.durationMs,
    arg_names_set: suppliedNamesOf(spec.suppliedNames),
    budget_tokens: budgetTokens,
    payload_tokens: payloadTokens,
    budget_used_ratio: budgetTokens === 0 ? null : round4(payloadTokens / budgetTokens),
    truncated: env?.truncated ?? false,
    truncation_reason: env?.truncation_reason ?? null,
    total_relation: env?.totals.total_relation ?? "gte",
    coverage_complete: env?.coverage.complete ?? false,
    coverage_indexed: env?.coverage.indexed ?? 0,
    coverage_skipped: env?.coverage.skipped ?? 0,
    coverage_failed: env?.coverage.failed ?? 0,
    unresolved_edges: env?.coverage.unresolved_edges ?? 0,
    stale: env?.read_status.stale ?? true,
    stale_reason: recordedStaleReasonOf(env?.read_status.stale_reason),
    generation_age_ms: generationAgeMsOf(ctx.ts, env?.read_status.indexed_at),
    result_counts: resultCountsOf(env?.payload),
    server_version: ctx.serverVersion,
    bun_version: ctx.bunVersion,
    os: ctx.os,
    arch: ctx.arch,
    graph_schema_version: ctx.graphSchemaVersion,
  };
}

export function indexRunRecordOf(ctx: RecordContext, spec: IndexRunSpec): IndexRunRecord {
  const unresolvedTotal = spec.unresolvedCount + sumOf(spec.edgesByKindTier);
  return {
    ts: ctx.ts,
    v: RECORD_VERSION,
    kind: "index_run",
    install_id: ctx.installId,
    session_id: ctx.sessionId,
    mode: spec.mode,
    exit_code: spec.exitCode,
    duration_ms: spec.durationMs,
    was_forced: spec.wasForced,
    generation_advanced: spec.exitCode === 0,
    files_seen: spec.filesSeen,
    files_indexed: spec.filesIndexed,
    files_skipped: spec.filesSkipped,
    files_failed: spec.filesFailed,
    nodes_by_kind: { ...spec.nodesByKind },
    edges_by_kind_tier: { ...spec.edgesByKindTier },
    unresolved_count: spec.unresolvedCount,
    unresolved_ratio: unresolvedTotal === 0 ? 0 : round4(spec.unresolvedCount / unresolvedTotal),
    languages: { ...spec.languages },
    db_size_bytes: spec.dbSizeBytes,
    files_changed_in_run: spec.filesChangedInRun,
  };
}

export function sessionRecordOf(ctx: RecordContext, facts: SessionFacts): SessionRecord {
  return {
    ts: ctx.ts,
    v: RECORD_VERSION,
    kind: "session",
    install_id: ctx.installId,
    session_id: ctx.sessionId,
    session_mode: facts.sessionMode,
    server_version: ctx.serverVersion,
    bun_version: ctx.bunVersion,
    os: ctx.os,
    arch: ctx.arch,
    is_first_run: facts.isFirstRun,
    repo_size_bucket: facts.repoSizeBucket,
    languages: [...facts.languages],
    root_kind: facts.rootKind,
  };
}

const MIB = 1024 * 1024;

/**
 * The graph file's size, bucketed so a report compares repositories without carrying a byte count
 * that would identify one. Boundaries are inclusive-low: 1 MiB is `"s"`, not `"xs"`.
 */
export function repoSizeBucketOf(bytes: number): RepoSizeBucket {
  if (bytes <= 0) {
    return "none";
  }
  if (bytes < MIB) {
    return "xs";
  }
  if (bytes < 10 * MIB) {
    return "s";
  }
  if (bytes < 100 * MIB) {
    return "m";
  }
  if (bytes < 1024 * MIB) {
    return "l";
  }
  return "xl";
}

export function rootKindOf(hasGitMarker: boolean): RootKind {
  return hasGitMarker ? "git" : "plain";
}

/**
 * `parseArgs` fills every declared default (`src/shared/cli/args.ts:200-211`), so `Object.keys`
 * here would report the schema rather than the request — a bare `status` would record
 * `["budget","paths"]` and the field would stop answering the one question it exists for.
 */
function suppliedNamesOf(names: readonly string[]): string[] {
  return [...new Set(names)].sort();
}

/**
 * Deliberately the defaulted value, not the supplied one: the budget in force is what a ratio has
 * to be measured against, and a tool without a `budget` argument records `0`.
 */
function budgetTokensOf(args: ToolArgs): number {
  const budget = args.budget;
  return typeof budget === "number" ? Number(budget) : 0;
}

/** Four decimals is the report's precision; more would only describe float noise. */
function round4(value: number): number {
  return Math.round(value * 10_000) / 10_000;
}

/**
 * The envelope's string is not a value we may keep. A `STALE_REASONS` member passes through;
 * anything else — prose, and `trace` puts a path in it — becomes `"other"`.
 */
function recordedStaleReasonOf(reason: string | undefined): RecordedStaleReason | null {
  if (reason === undefined) {
    return null;
  }
  return (STALE_REASONS as readonly string[]).includes(reason) ? (reason as StaleReason) : "other";
}

function generationAgeMsOf(ts: string, indexedAt: string | undefined): number | null {
  if (indexedAt === undefined) {
    return null;
  }
  const recorded = Date.parse(ts);
  const indexed = Date.parse(indexedAt);
  return Number.isNaN(recorded) || Number.isNaN(indexed) ? null : recorded - indexed;
}

/** Lengths survive, values do not: a payload string never reaches the line from here. */
function resultCountsOf(payload: unknown): Record<string, number> {
  if (typeof payload !== "object" || payload === null) {
    return {};
  }
  const counts: Record<string, number> = {};
  for (const [key, value] of Object.entries(payload)) {
    if (Array.isArray(value)) {
      counts[key] = value.length;
    }
  }
  return counts;
}

function sumOf(counts: Readonly<Record<string, number>>): number {
  let total = 0;
  for (const value of Object.values(counts)) {
    total += value;
  }
  return total;
}
