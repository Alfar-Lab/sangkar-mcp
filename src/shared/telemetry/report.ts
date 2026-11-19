/**
 * Read one telemetry directory and fold it into a single data object. Both halves are pure in the
 * sense a report needs: `readRecords` touches the disk and nothing else, and `aggregate` takes
 * records that are already in memory, with no clock, no environment and no injection point. A
 * callback could only re-round what this file rounds itself, so keeping the arithmetic here is what
 * makes the determinism a property of the module rather than of its caller.
 *
 * Two failures are expected here and neither is fatal. A process killed between two lines of an
 * append leaves a torn final line, so an unparseable line must cost one counter and nothing else;
 * and `record.ts` keeps `v: 1` while a field is *added* and bumps it only when one is removed or
 * renamed, so a line stamped with another version is a record this reader must not guess at.
 *
 * The union is declared once, in `record.ts`, and is never re-declared here: this module reads it.
 */
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import {
  type CallRecord,
  type IndexRunRecord,
  RECORD_VERSION,
  type SessionRecord,
  type TelemetryRecord,
} from "./record";

export interface TelemetryReadResult {
  readonly files: readonly string[];
  readonly records: readonly TelemetryRecord[];
  readonly unparsed: number;
}

export interface ToolStats {
  readonly tool: string;
  readonly calls: number;
  readonly statuses: Readonly<Record<string, number>>;
  readonly error_codes: Readonly<Record<string, number>>;
  readonly duration_ms: {
    readonly p50: number;
    readonly p95: number;
    readonly mean: number;
    readonly max: number;
  };
  readonly truncated: number;
  readonly mean_budget_used_ratio: number | null;
  readonly eq: number;
  readonly gte: number;
}

export interface IndexRunRow {
  readonly ts: string;
  readonly mode: string;
  readonly exit_code: number;
  readonly duration_ms: number;
  readonly files_seen: number;
  readonly files_indexed: number;
  readonly files_skipped: number;
  readonly files_failed: number;
  readonly files_changed_in_run: number;
  readonly unresolved_ratio: number;
  readonly db_size_bytes: number;
  readonly generation_advanced: boolean;
}

export interface TrendPoint {
  readonly date: string;
  readonly ratio: number;
  readonly runs: number;
}

export interface ReportData {
  readonly totals: {
    readonly records: number;
    readonly calls: number;
    readonly index_runs: number;
    readonly sessions: number;
    readonly unparsed: number;
    readonly install_ids: number;
    readonly days: number;
  };
  readonly tools: readonly ToolStats[];
  readonly status_mix: Readonly<Record<string, number>>;
  readonly error_mix: Readonly<Record<string, number>>;
  readonly truncation: {
    readonly count: number;
    readonly rate: number;
    readonly reasons: Readonly<Record<string, number>>;
  };
  readonly relations: { readonly eq: number; readonly gte: number; readonly eq_ratio: number };
  readonly coverage: { readonly stale_calls: number; readonly mean_unresolved_edges: number };
  readonly arg_names: readonly { readonly name: string; readonly calls: number }[];
  readonly index_runs: readonly IndexRunRow[];
  readonly unresolved_trend: readonly TrendPoint[];
  readonly sessions: {
    readonly by_mode: Readonly<Record<string, number>>;
    readonly by_root_kind: Readonly<Record<string, number>>;
    readonly by_size_bucket: Readonly<Record<string, number>>;
  };
  readonly window: {
    readonly since: string | null;
    readonly first_ts: string | null;
    readonly last_ts: string | null;
  };
}

const DAY_MS = 86_400_000;
const DATE_LITERAL = /^\d{4}-\d{2}-\d{2}$/;
const DAYS_LITERAL = /^(\d+)d$/;

/**
 * The CLI needs to refuse a malformed `--since` before any IO, so the shape test is separate from
 * the parse below: `parseSince` answers `null` for a bad value and reports nothing, because a
 * reporter that throws on its own flag has no way left to print the usage line.
 */
export function isSinceLiteral(raw: string): boolean {
  if (DATE_LITERAL.test(raw)) {
    return utcMidnightOf(raw) !== null;
  }
  return daysOf(raw) !== null;
}

/** `YYYY-MM-DD` is a calendar day, `Nd` is "N days before `now`", and both floor to UTC midnight. */
export function parseSince(raw: string | undefined, now: Date): Date | null {
  if (raw === undefined) {
    return null;
  }
  if (DATE_LITERAL.test(raw)) {
    return utcMidnightOf(raw);
  }
  const days = daysOf(raw);
  if (days === null) {
    return null;
  }
  // `now` is injected because this module owns no clock; the offset is applied to the epoch
  // reading and then floored to the UTC day, like every other date in telemetry.
  const shiftedMs = now.getTime() - days * DAY_MS;
  return startOfUtcDay(new Date(shiftedMs));
}

/** The turn of the day is UTC everywhere else in telemetry (`identity.utcDateOf`), so it is here. */
function utcMidnightOf(raw: string): Date | null {
  const year = Number(raw.slice(0, 4));
  const month = Number(raw.slice(5, 7));
  const day = Number(raw.slice(8, 10));
  const at = new Date(Date.UTC(year, month - 1, day));
  const valid =
    at.getUTCFullYear() === year && at.getUTCMonth() === month - 1 && at.getUTCDate() === day;
  return valid ? at : null;
}

function startOfUtcDay(at: Date): Date {
  return new Date(Date.UTC(at.getUTCFullYear(), at.getUTCMonth(), at.getUTCDate()));
}

/** `null` for anything that is not `Nd` with an integer N >= 1 (`0d` would mean "today"). */
function daysOf(raw: string): number | null {
  const digits = DAYS_LITERAL.exec(raw)?.[1];
  if (digits === undefined) {
    return null;
  }
  const days = Number(digits);
  return Number.isInteger(days) && days >= 1 ? days : null;
}

/**
 * The `<dir>/<date>.jsonl` layout `sink.ts` writes: only daily files are records, and the name is
 * the file's date rather than the line's, so the order below is a stable starting order only —
 * nothing aggregated depends on it.
 */
export function readRecords(
  dir: string,
  options: { readonly since: Date | null },
): TelemetryReadResult {
  const files = jsonlFilesOf(dir);
  const sinceMs = options.since === null ? null : options.since.getTime();
  const records: TelemetryRecord[] = [];
  let unparsed = 0;

  for (const file of files) {
    const text = textOf(join(dir, file));
    if (text === null) {
      // Deleted or unreadable between the listing and the read: a report must not fail because the
      // directory was rotated underneath it, and a file has no line to count.
      continue;
    }
    for (const line of linesOf(text)) {
      const record = recordOf(line);
      if (record === null) {
        unparsed += 1;
        continue;
      }
      if (sinceMs !== null && Date.parse(record.ts) < sinceMs) {
        continue;
      }
      records.push(record);
    }
  }

  return { files, records, unparsed };
}

/** A missing directory is "nothing recorded yet", not an error: the first run has no file. */
function jsonlFilesOf(dir: string): readonly string[] {
  if (!existsSync(dir)) {
    return [];
  }
  try {
    return readdirSync(dir)
      .filter((name) => name.endsWith(".jsonl"))
      .sort();
  } catch {
    return [];
  }
}

function textOf(file: string): string | null {
  try {
    return readFileSync(file, "utf8");
  } catch {
    return null;
  }
}

/**
 * The trailing empty element is the newline an append always ends with, and it is not a line. Every
 * other empty string is one — a zero-line append (or a killed process) leaves exactly that — and it
 * falls out as unparsed because `JSON.parse("")` throws.
 */
function linesOf(text: string): readonly string[] {
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") {
    lines.pop();
  }
  return lines;
}

/**
 * The five skip clauses. A line is dropped — counted in `unparsed` and folded into no ratio and no
 * count — when it is not JSON, not an object, at another `v`, of an unknown `kind`, or missing a
 * field this module reads. `JSON.stringify` writes `NaN` as `null` and a torn line is almost never
 * valid JSON, so one clause per kind is enough; the table below is that clause's whole content, and
 * it lives in one place so the reader and the schema cannot drift apart silently.
 */
function recordOf(line: string): TelemetryRecord | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(line);
  } catch {
    return null;
  }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    return null;
  }
  const value = parsed as Record<string, unknown>;
  if (value.v !== RECORD_VERSION) {
    return null;
  }
  if (value.kind === "call") {
    return hasFields(value, CALL_FIELDS) ? (value as unknown as CallRecord) : null;
  }
  if (value.kind === "index_run") {
    return hasFields(value, INDEX_RUN_FIELDS) ? (value as unknown as IndexRunRecord) : null;
  }
  if (value.kind === "session") {
    return hasFields(value, SESSION_FIELDS) ? (value as unknown as SessionRecord) : null;
  }
  return null;
}

/**
 * The required-field list per kind, derived from the interfaces in `record.ts`: the fields this
 * module reads, checked as a primitive. A nullable field is absent from the table because `null` is
 * a legal value there — the readers below guard those themselves — and a field no report touches is
 * absent because a reader ignores what it does not need. This is deliberately not a schema
 * validator: a line that satisfies it is aggregated, and neither the cast above nor the ones in
 * `aggregate` can carry a wrong type into a count, a percentile or a ratio, because every field
 * read is one of these.
 */
type FieldType = "string" | "number" | "boolean" | "list" | "map";
type FieldSpec = readonly (readonly [name: string, type: FieldType])[];

const CALL_FIELDS: FieldSpec = [
  ["ts", "string"],
  ["kind", "string"],
  ["install_id", "string"],
  ["tool", "string"],
  ["status", "string"],
  ["duration_ms", "number"],
  ["arg_names_set", "list"],
  ["budget_tokens", "number"],
  ["payload_tokens", "number"],
  ["truncated", "boolean"],
  ["total_relation", "string"],
  ["coverage_complete", "boolean"],
  ["coverage_indexed", "number"],
  ["coverage_skipped", "number"],
  ["coverage_failed", "number"],
  ["unresolved_edges", "number"],
  ["stale", "boolean"],
  ["result_counts", "map"],
];

const INDEX_RUN_FIELDS: FieldSpec = [
  ["ts", "string"],
  ["kind", "string"],
  ["install_id", "string"],
  ["mode", "string"],
  ["exit_code", "number"],
  ["duration_ms", "number"],
  ["generation_advanced", "boolean"],
  ["files_seen", "number"],
  ["files_indexed", "number"],
  ["files_skipped", "number"],
  ["files_failed", "number"],
  ["files_changed_in_run", "number"],
  ["unresolved_ratio", "number"],
  ["languages", "map"],
  ["db_size_bytes", "number"],
];

const SESSION_FIELDS: FieldSpec = [
  ["ts", "string"],
  ["kind", "string"],
  ["install_id", "string"],
  ["session_mode", "string"],
  ["is_first_run", "boolean"],
  ["repo_size_bucket", "string"],
  ["root_kind", "string"],
];

function hasFields(value: Record<string, unknown>, spec: FieldSpec): boolean {
  return spec.every(([name, type]) => hasType(value[name], type));
}

function hasType(value: unknown, type: FieldType): boolean {
  switch (type) {
    case "string":
      return typeof value === "string";
    case "number":
      return typeof value === "number";
    case "boolean":
      return typeof value === "boolean";
    case "list":
      return Array.isArray(value);
    case "map":
      return typeof value === "object" && value !== null && !Array.isArray(value);
  }
}

interface ToolAccumulator {
  readonly durations: number[];
  readonly ratios: number[];
  readonly statuses: Map<string, number>;
  readonly errorCodes: Map<string, number>;
  truncated: number;
  eq: number;
  gte: number;
}

interface TrendAccumulator {
  total: number;
  runs: number;
}

/**
 * The fold. Everything that leaves here is ordered by its own keys — `tools` by name, every mix by
 * key, `arg_names` by count then name, the runs and the trend chronologically — and every ratio
 * passes through {@link round4}, so two calls over the same records serialise to the same bytes.
 */
export function aggregate(input: {
  readonly records: readonly TelemetryRecord[];
  readonly unparsed: number;
  readonly files: readonly string[];
  readonly since: string | null;
}): ReportData {
  const calls = input.records.filter(isCallRecord);
  const runs = input.records.filter(isIndexRunRecord).sort(compareRuns);
  const sessions = input.records.filter(isSessionRecord);

  const byTool = new Map<string, ToolAccumulator>();
  const statusMix = new Map<string, number>();
  const errorMix = new Map<string, number>();
  const truncationReasons = new Map<string, number>();
  const argCounts = new Map<string, number>();
  let truncatedTotal = 0;
  let eqTotal = 0;
  let gteTotal = 0;
  let staleCalls = 0;
  let unresolvedTotal = 0;

  for (const call of calls) {
    const tool = toolAccumulatorOf(byTool, call.tool);
    tool.durations.push(call.duration_ms);
    bump(tool.statuses, call.status);
    bump(statusMix, call.status);
    if (typeof call.error_code === "string") {
      bump(tool.errorCodes, call.error_code);
      bump(errorMix, call.error_code);
    }
    if (typeof call.budget_used_ratio === "number") {
      tool.ratios.push(call.budget_used_ratio);
    }
    if (call.truncated) {
      truncatedTotal += 1;
      tool.truncated += 1;
      if (typeof call.truncation_reason === "string") {
        bump(truncationReasons, call.truncation_reason);
      }
    }
    if (call.total_relation === "eq") {
      eqTotal += 1;
      tool.eq += 1;
    } else if (call.total_relation === "gte") {
      gteTotal += 1;
      tool.gte += 1;
    }
    if (call.stale) {
      staleCalls += 1;
    }
    unresolvedTotal += call.unresolved_edges;
    for (const name of call.arg_names_set) {
      bump(argCounts, name);
    }
  }

  const tools = [...byTool.entries()]
    .sort(([left], [right]) => compareText(left, right))
    .map(([tool, accumulator]) => toolStatsOf(tool, accumulator));

  const trendByDay = new Map<string, TrendAccumulator>();
  for (const run of runs) {
    const day = dayOf(run.ts);
    const entry = trendByDay.get(day) ?? { total: 0, runs: 0 };
    entry.total += run.unresolved_ratio;
    entry.runs += 1;
    trendByDay.set(day, entry);
  }

  const byMode = new Map<string, number>();
  const byRootKind = new Map<string, number>();
  const bySizeBucket = new Map<string, number>();
  for (const session of sessions) {
    bump(byMode, session.session_mode);
    bump(byRootKind, session.root_kind);
    bump(bySizeBucket, session.repo_size_bucket);
  }

  // `install_id` groups records from one host-day without being kept: only the count leaves here.
  const installIds = new Set<string>();
  const recordDays = new Set<string>();
  let firstTs: string | null = null;
  let lastTs: string | null = null;
  for (const record of input.records) {
    installIds.add(record.install_id);
    recordDays.add(dayOf(record.ts));
    if (firstTs === null || record.ts < firstTs) {
      firstTs = record.ts;
    }
    if (lastTs === null || record.ts > lastTs) {
      lastTs = record.ts;
    }
  }

  const callsCount = calls.length;
  return {
    totals: {
      records: input.records.length,
      calls: callsCount,
      index_runs: runs.length,
      sessions: sessions.length,
      unparsed: input.unparsed,
      install_ids: installIds.size,
      days: recordDays.size,
    },
    tools,
    status_mix: sortedCounts(statusMix),
    error_mix: sortedCounts(errorMix),
    truncation: {
      count: truncatedTotal,
      rate: callsCount === 0 ? 0 : round4(truncatedTotal / callsCount),
      reasons: sortedCounts(truncationReasons),
    },
    relations: {
      eq: eqTotal,
      gte: gteTotal,
      eq_ratio: callsCount === 0 ? 0 : round4(eqTotal / callsCount),
    },
    coverage: {
      stale_calls: staleCalls,
      mean_unresolved_edges: callsCount === 0 ? 0 : round4(unresolvedTotal / callsCount),
    },
    arg_names: [...argCounts.entries()]
      .map(([name, count]) => ({ name, calls: count }))
      .sort((left, right) => right.calls - left.calls || compareText(left.name, right.name)),
    index_runs: runs.map(indexRunRowOf),
    unresolved_trend: [...trendByDay.entries()]
      .sort(([left], [right]) => compareText(left, right))
      .map(([date, entry]) => ({
        date,
        ratio: round4(entry.total / entry.runs),
        runs: entry.runs,
      })),
    sessions: {
      by_mode: sortedCounts(byMode),
      by_root_kind: sortedCounts(byRootKind),
      by_size_bucket: sortedCounts(bySizeBucket),
    },
    window: { since: input.since, first_ts: firstTs, last_ts: lastTs },
  };
}

function isCallRecord(record: TelemetryRecord): record is CallRecord {
  return record.kind === "call";
}

function isIndexRunRecord(record: TelemetryRecord): record is IndexRunRecord {
  return record.kind === "index_run";
}

function isSessionRecord(record: TelemetryRecord): record is SessionRecord {
  return record.kind === "session";
}

/**
 * Chronological, with a tie-break that never leans on the read order: two runs recorded inside the
 * same millisecond (a `nothingToDo` run is cheap) would otherwise swap places between two reads.
 */
function compareRuns(left: IndexRunRecord, right: IndexRunRecord): number {
  return (
    compareText(left.ts, right.ts) ||
    compareText(left.session_id, right.session_id) ||
    left.exit_code - right.exit_code
  );
}

function indexRunRowOf(run: IndexRunRecord): IndexRunRow {
  return {
    ts: run.ts,
    mode: run.mode,
    exit_code: run.exit_code,
    duration_ms: run.duration_ms,
    files_seen: run.files_seen,
    files_indexed: run.files_indexed,
    files_skipped: run.files_skipped,
    files_failed: run.files_failed,
    files_changed_in_run: run.files_changed_in_run,
    unresolved_ratio: run.unresolved_ratio,
    db_size_bytes: run.db_size_bytes,
    generation_advanced: run.generation_advanced,
  };
}

function toolAccumulatorOf(byTool: Map<string, ToolAccumulator>, tool: string): ToolAccumulator {
  const found = byTool.get(tool);
  if (found !== undefined) {
    return found;
  }
  const created: ToolAccumulator = {
    durations: [],
    ratios: [],
    statuses: new Map(),
    errorCodes: new Map(),
    truncated: 0,
    eq: 0,
    gte: 0,
  };
  byTool.set(tool, created);
  return created;
}

function toolStatsOf(tool: string, accumulator: ToolAccumulator): ToolStats {
  const durations = [...accumulator.durations].sort((left, right) => left - right);
  return {
    tool,
    calls: durations.length,
    statuses: sortedCounts(accumulator.statuses),
    error_codes: sortedCounts(accumulator.errorCodes),
    duration_ms: {
      p50: percentile(durations, 0.5),
      p95: percentile(durations, 0.95),
      mean: round4(meanOf(durations)),
      max: maxOf(durations),
    },
    truncated: accumulator.truncated,
    mean_budget_used_ratio:
      accumulator.ratios.length === 0 ? null : round4(meanOf(accumulator.ratios)),
    eq: accumulator.eq,
    gte: accumulator.gte,
  };
}

/** Nearest-rank over an ascending array: no interpolation, so a report reproduces by hand. */
function percentile(sorted: readonly number[], quantile: number): number {
  if (sorted.length === 0) {
    return 0;
  }
  const rank = Math.min(sorted.length - 1, Math.ceil(quantile * sorted.length) - 1);
  return sorted[rank] ?? 0;
}

function meanOf(values: readonly number[]): number {
  if (values.length === 0) {
    return 0;
  }
  let total = 0;
  for (const value of values) {
    total += value;
  }
  return total / values.length;
}

function maxOf(values: readonly number[]): number {
  return values.length === 0 ? 0 : (values[values.length - 1] ?? 0);
}

function bump(counts: Map<string, number>, key: string): void {
  counts.set(key, (counts.get(key) ?? 0) + 1);
}

/** Built key by key in sorted order: `JSON.stringify` then follows insertion, not hash order. */
function sortedCounts(counts: Map<string, number>): Readonly<Record<string, number>> {
  const out: Record<string, number> = {};
  for (const key of [...counts.keys()].sort()) {
    out[key] = counts.get(key) ?? 0;
  }
  return out;
}

/** `localeCompare` answers differently per host locale; a report must not. */
function compareText(left: string, right: string): number {
  if (left === right) {
    return 0;
  }
  return left < right ? -1 : 1;
}

/** ISO-8601 in UTC puts the UTC date in the first ten characters, which is the whole group key. */
function dayOf(ts: string): string {
  return ts.slice(0, 10);
}

/** Four decimals is the report's precision; more would only describe float noise. */
function round4(value: number): number {
  return Math.round(value * 10_000) / 10_000;
}
