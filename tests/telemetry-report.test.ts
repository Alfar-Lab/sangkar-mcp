import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { escapeHtml, renderHtml } from "#shared/telemetry/html";
import {
  type CallRecord,
  type IndexRunRecord,
  RECORD_VERSION,
  type SessionRecord,
  type TelemetryRecord,
} from "#shared/telemetry/record";
import { aggregate, isSinceLiteral, parseSince, readRecords } from "#shared/telemetry/report";

/**
 * The report contract, executable. `report.ts` owns the arithmetic, so every number a reader
 * trusts — a count, a percentile, a ratio, a trend point, a table order — is pinned here against a
 * hand-written fixture instead of against a second implementation.
 *
 * Two properties are asserted on the rendered bytes on purpose: the file is self-contained (it
 * opens from a USB stick with the radio off) and every free string is escaped (a tool named
 * `</script>` must not close the tag that carries it). Neither can be seen from the aggregate.
 *
 * The union lives in `record.ts` and is imported, never re-declared: a fixture that drifts from
 * the recorder is a type error here rather than an empty report in production.
 */

const GENERATED_AT = "2026-11-20T09:30:00.000Z";
const META = { generated_at: GENERATED_AT, since: null } as const;
const INSTALL_ID = "0f".repeat(16);
const SESSION_ID = "1a".repeat(16);

const dirs: string[] = [];

function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

function callRecord(overrides: Partial<CallRecord> = {}): CallRecord {
  return {
    ts: "2026-11-18T10:00:00.000Z",
    v: RECORD_VERSION,
    kind: "call",
    install_id: INSTALL_ID,
    session_id: SESSION_ID,
    seq: 1,
    tool: "status",
    status: "ok",
    error_code: null,
    duration_ms: 10,
    arg_names_set: [],
    budget_tokens: 1_000,
    payload_tokens: 100,
    budget_used_ratio: 0.1,
    truncated: false,
    truncation_reason: null,
    total_relation: "gte",
    coverage_complete: true,
    coverage_indexed: 1,
    coverage_skipped: 0,
    coverage_failed: 0,
    unresolved_edges: 0,
    stale: false,
    stale_reason: null,
    generation_age_ms: null,
    result_counts: {},
    server_version: "0.1.0-beta.0",
    bun_version: "1.4.4",
    os: "linux",
    arch: "x64",
    graph_schema_version: 1,
    ...overrides,
  };
}

function indexRunRecord(overrides: Partial<IndexRunRecord> = {}): IndexRunRecord {
  return {
    ts: "2026-11-18T10:00:00.000Z",
    v: RECORD_VERSION,
    kind: "index_run",
    install_id: INSTALL_ID,
    session_id: SESSION_ID,
    mode: "full",
    exit_code: 0,
    duration_ms: 5_000,
    was_forced: false,
    generation_advanced: true,
    files_seen: 10,
    files_indexed: 9,
    files_skipped: 1,
    files_failed: 0,
    nodes_by_kind: {},
    edges_by_kind_tier: {},
    unresolved_count: 0,
    unresolved_ratio: 0,
    languages: {},
    db_size_bytes: 4_096,
    files_changed_in_run: 4,
    ...overrides,
  };
}

function sessionRecord(overrides: Partial<SessionRecord> = {}): SessionRecord {
  return {
    ts: "2026-11-18T10:00:00.000Z",
    v: RECORD_VERSION,
    kind: "session",
    install_id: INSTALL_ID,
    session_id: SESSION_ID,
    session_mode: "cli",
    server_version: "0.1.0-beta.0",
    bun_version: "1.4.4",
    os: "linux",
    arch: "x64",
    is_first_run: true,
    repo_size_bucket: "s",
    languages: ["typescript"],
    root_kind: "git",
    ...overrides,
  };
}

interface AggregateOverrides {
  readonly unparsed?: number;
  readonly files?: readonly string[];
  readonly since?: string | null;
}

function aggregateOf(
  records: readonly TelemetryRecord[],
  overrides: AggregateOverrides = {},
): ReturnType<typeof aggregate> {
  return aggregate({
    records,
    unparsed: overrides.unparsed ?? 0,
    files: overrides.files ?? [],
    since: overrides.since ?? null,
  });
}

function renderedOf(
  records: readonly TelemetryRecord[],
  overrides: AggregateOverrides = {},
): string {
  return renderHtml(aggregateOf(records, overrides), META);
}

function writeJsonl(dir: string, name: string, lines: readonly string[]): void {
  writeFileSync(join(dir, name), `${lines.join("\n")}\n`, "utf8");
}

/** A line as the recorder would have written it, so the reader sees every field of the interface. */
function lineOf(record: TelemetryRecord): string {
  return JSON.stringify(record);
}

/** One field replaced: the reader's version and kind clauses are checked against a real record. */
function lineWith(record: TelemetryRecord, patch: Record<string, unknown>): string {
  const raw = JSON.parse(JSON.stringify(record)) as Record<string, unknown>;
  return JSON.stringify({ ...raw, ...patch });
}

/** One field dropped: a record missing a field the report reads is a line the reader must skip. */
function lineWithoutKey(record: TelemetryRecord, key: string): string {
  const raw = JSON.parse(JSON.stringify(record)) as Record<string, unknown>;
  const kept = Object.entries(raw).filter(([name]) => name !== key);
  return JSON.stringify(Object.fromEntries(kept));
}

describe("per-tool aggregation", () => {
  test("three tools with known statuses and error codes produce the exact counts", () => {
    const records: TelemetryRecord[] = [
      callRecord({ tool: "trace", status: "ok" }),
      callRecord({ tool: "trace", status: "ok" }),
      callRecord({ tool: "trace", status: "error", error_code: "invalid_args" }),
      callRecord({ tool: "search", status: "ok" }),
      callRecord({ tool: "search", status: "not_found" }),
      callRecord({ tool: "status", status: "absent", error_code: "bootstrapping" }),
    ];

    const data = aggregateOf(records);

    expect(data.totals).toEqual({
      records: 6,
      calls: 6,
      index_runs: 0,
      sessions: 0,
      unparsed: 0,
      install_ids: 1,
      days: 1,
    });

    // Whole objects, sorted by tool name, so a stray key or a dropped one fails the case.
    expect(data.tools).toEqual([
      {
        tool: "search",
        calls: 2,
        statuses: { not_found: 1, ok: 1 },
        error_codes: {},
        duration_ms: { p50: 10, p95: 10, mean: 10, max: 10 },
        truncated: 0,
        mean_budget_used_ratio: 0.1,
        eq: 0,
        gte: 2,
      },
      {
        tool: "status",
        calls: 1,
        statuses: { absent: 1 },
        error_codes: { bootstrapping: 1 },
        duration_ms: { p50: 10, p95: 10, mean: 10, max: 10 },
        truncated: 0,
        mean_budget_used_ratio: 0.1,
        eq: 0,
        gte: 1,
      },
      {
        tool: "trace",
        calls: 3,
        statuses: { error: 1, ok: 2 },
        error_codes: { invalid_args: 1 },
        duration_ms: { p50: 10, p95: 10, mean: 10, max: 10 },
        truncated: 0,
        mean_budget_used_ratio: 0.1,
        eq: 0,
        gte: 3,
      },
    ]);
    expect(data.status_mix).toEqual({ absent: 1, error: 1, not_found: 1, ok: 3 });
    expect(data.error_mix).toEqual({ bootstrapping: 1, invalid_args: 1 });
  });
});

describe("latency percentiles", () => {
  test("duration_ms 1..100 gives p50 50, p95 95, mean 50.5 and max 100", () => {
    const records = Array.from({ length: 100 }, (_unused, index) =>
      callRecord({ tool: "trace", duration_ms: index + 1 }),
    );

    const data = aggregateOf(records);
    expect(data.tools).toHaveLength(1);
    expect(data.tools[0]?.calls).toBe(100);
    expect(data.tools[0]?.duration_ms).toEqual({ p50: 50, p95: 95, mean: 50.5, max: 100 });
  });

  test("nearest-rank, not interpolated: the rank is the ceiling of q*n", () => {
    // 1,2,3,4: an interpolating percentile would answer 2.5 and 3.85 here.
    const data = aggregateOf([
      callRecord({ tool: "trace", duration_ms: 4 }),
      callRecord({ tool: "trace", duration_ms: 2 }),
      callRecord({ tool: "trace", duration_ms: 1 }),
      callRecord({ tool: "trace", duration_ms: 3 }),
    ]);

    expect(data.tools[0]?.duration_ms).toEqual({ p50: 2, p95: 4, mean: 2.5, max: 4 });
  });
});

describe("truncation and relation mix", () => {
  test("2 truncated of 4 calls is rate 0.5, and eq_ratio matches the fixture", () => {
    const data = aggregateOf([
      callRecord({
        tool: "trace",
        truncated: true,
        truncation_reason: "budget",
        total_relation: "gte",
      }),
      callRecord({
        tool: "trace",
        truncated: true,
        truncation_reason: "item_cap",
        total_relation: "gte",
      }),
      callRecord({ tool: "trace", total_relation: "eq" }),
      callRecord({ tool: "search", total_relation: "gte" }),
    ]);

    expect(data.truncation).toEqual({ count: 2, rate: 0.5, reasons: { budget: 1, item_cap: 1 } });
    expect(data.relations).toEqual({ eq: 1, gte: 3, eq_ratio: 0.25 });
    expect(data.tools.map((tool) => [tool.tool, tool.truncated, tool.eq, tool.gte])).toEqual([
      ["search", 0, 0, 1],
      ["trace", 2, 1, 2],
    ]);
  });
});

describe("the unresolved trend", () => {
  test("two UTC dates give two points, per-day mean, in chronological order", () => {
    const data = aggregateOf([
      indexRunRecord({ ts: "2026-11-18T12:00:00.000Z", unresolved_ratio: 0.5 }),
      indexRunRecord({ ts: "2026-11-17T09:00:00.000Z", unresolved_ratio: 0.25 }),
      indexRunRecord({ ts: "2026-11-18T18:00:00.000Z", unresolved_ratio: 0.75 }),
    ]);

    expect(data.unresolved_trend).toEqual([
      { date: "2026-11-17", ratio: 0.25, runs: 1 },
      { date: "2026-11-18", ratio: 0.625, runs: 2 },
    ]);
    expect(data.totals.index_runs).toBe(3);
    expect(data.totals.days).toBe(2);
  });
});

describe("the index-run table", () => {
  test("rows are chronological and carry exit code, mode and the file counters", () => {
    const earlier = indexRunRecord({
      ts: "2026-11-17T08:00:00.000Z",
      mode: "full",
      exit_code: 0,
      duration_ms: 5_000,
      files_seen: 12,
      files_indexed: 12,
      files_skipped: 0,
      files_failed: 0,
      files_changed_in_run: 12,
      unresolved_ratio: 0.2,
      db_size_bytes: 4_096,
      generation_advanced: true,
    });
    const later = indexRunRecord({
      ts: "2026-11-19T08:00:00.000Z",
      mode: "fast",
      exit_code: 3,
      duration_ms: 1_200,
      files_seen: 10,
      files_indexed: 7,
      files_skipped: 1,
      files_failed: 2,
      files_changed_in_run: 5,
      unresolved_ratio: 0.4,
      db_size_bytes: 8_192,
      generation_advanced: false,
    });

    // Read order is the reverse of recorded order: only `ts` may decide the table.
    const data = aggregateOf([later, earlier]);

    expect(data.index_runs).toEqual([
      {
        ts: "2026-11-17T08:00:00.000Z",
        mode: "full",
        exit_code: 0,
        duration_ms: 5_000,
        files_seen: 12,
        files_indexed: 12,
        files_skipped: 0,
        files_failed: 0,
        files_changed_in_run: 12,
        unresolved_ratio: 0.2,
        db_size_bytes: 4_096,
        generation_advanced: true,
      },
      {
        ts: "2026-11-19T08:00:00.000Z",
        mode: "fast",
        exit_code: 3,
        duration_ms: 1_200,
        files_seen: 10,
        files_indexed: 7,
        files_skipped: 1,
        files_failed: 2,
        files_changed_in_run: 5,
        unresolved_ratio: 0.4,
        db_size_bytes: 8_192,
        generation_advanced: false,
      },
    ]);

    const html = renderedOf([later, earlier]);
    expect(html.indexOf("2026-11-17T08:00:00.000Z")).toBeLessThan(
      html.indexOf("2026-11-19T08:00:00.000Z"),
    );
    expect(html).toContain("<td>full</td>");
    expect(html).toContain("<td>fast</td>");
    expect(html).toContain('<td class="num">3</td>');
    expect(html).toContain("<td>advanced</td>");
    expect(html).toContain("<td>unchanged</td>");
  });
});

describe("the report is self-contained", () => {
  test("no external URL, no img/link source, no @import and no url() in the CSS", () => {
    const html = renderedOf([callRecord(), indexRunRecord(), sessionRecord()]);

    expect(html).not.toContain("http://");
    expect(html).not.toContain("https://");
    expect(html).not.toContain("src=");
    expect(html).not.toContain("<link");
    expect(html).not.toContain("@import");
    expect(html).not.toContain("url(");

    // A stripped document would satisfy every `not.toContain` above, so the panels must be here.
    expect(html).toContain("<style>");
    expect(html).toContain('id="tools"');
    expect(html).toContain('id="index-runs"');
    expect(html).toContain('id="sessions"');
    expect(html.length).toBeGreaterThan(5_000);
  });
});

describe("escaping", () => {
  test("the five characters are escaped, in the order that does not double-escape", () => {
    expect(escapeHtml('<a href="x">&')).toBe("&lt;a href=&quot;x&quot;&gt;&amp;");
    expect(escapeHtml("it's")).toBe("it&#39;s");
    expect(escapeHtml("plain")).toBe("plain");
  });

  test("a hostile tool value renders escaped and leaves exactly one script element", () => {
    const hostile = "</script><img src=x>";
    const html = renderedOf([callRecord({ tool: hostile })]);

    expect(html).not.toContain(hostile);
    expect(html.match(/<script/gi)?.length).toBe(1);
    expect(html.match(/<\/script>/gi)?.length).toBe(1);
    // The cell is present, escaped: the value is shown, it just cannot be markup.
    expect(html).toContain("&lt;/script&gt;&lt;img src=x&gt;");
  });
});

describe("an empty window", () => {
  test("an empty directory renders the empty state and no data rows", () => {
    const dir = tempDir("sangkar-telemetry-report-empty-");
    const read = readRecords(dir, { since: null });
    expect(read).toEqual({ records: [], unparsed: 0, files: [] });

    const data = aggregate({
      records: read.records,
      unparsed: read.unparsed,
      files: read.files,
      since: null,
    });
    expect(data.totals.records).toBe(0);
    expect(data.tools).toEqual([]);
    expect(data.status_mix).toEqual({});

    const html = renderHtml(data, META);
    expect(html).toContain('id="empty"');
    expect(html).toContain("nothing recorded yet");
    // Every cell that carries a number only comes from a data row.
    expect(html).not.toContain('class="num"');
    expect(html).toContain('class="empty"');

    // A missing directory is "nothing recorded yet", never an error.
    const missing = join(dir, "rotated-away");
    expect(() => readRecords(missing, { since: null })).not.toThrow();
    expect(readRecords(missing, { since: null })).toEqual({ records: [], unparsed: 0, files: [] });
  });
});

describe("the skip rule", () => {
  test("torn, non-object and mistyped lines cost one unparsed count each and nothing else", () => {
    const dir = tempDir("sangkar-telemetry-report-skip-");
    const first = callRecord({ tool: "trace", seq: 1 });
    const second = callRecord({ tool: "trace", seq: 2 });

    writeJsonl(dir, "2026-11-18.jsonl", [
      lineOf(first),
      lineOf(second),
      '{"ts":"2026-11-18T11:00:00.000Z","v":1,"kind":"call","tool":"tor',
      "42",
    ]);

    const firstFile = readRecords(dir, { since: null });
    expect(firstFile.files).toEqual(["2026-11-18.jsonl"]);
    expect(firstFile.records).toHaveLength(2);
    expect(firstFile.unparsed).toBe(2);

    writeJsonl(dir, "2026-11-19.jsonl", [
      lineWith(callRecord({ tool: "ghost-tool", seq: 3 }), { kind: "mystery" }),
      lineWith(callRecord({ tool: "future-tool", seq: 4 }), { v: 2 }),
      lineWithoutKey(callRecord({ tool: "torn-tool", seq: 5 }), "arg_names_set"),
    ]);

    // The second file adds three unparsed lines and no record.
    const read = readRecords(dir, { since: null });
    expect(read.files).toEqual(["2026-11-18.jsonl", "2026-11-19.jsonl"]);
    expect(read.records).toHaveLength(2);
    expect(read.unparsed).toBe(5);

    const data = aggregate({
      records: read.records,
      unparsed: read.unparsed,
      files: read.files,
      since: null,
    });

    // The three lines of the second file raised `unparsed` and changed nothing else.
    expect(data.totals).toEqual({
      records: 2,
      calls: 2,
      index_runs: 0,
      sessions: 0,
      unparsed: 5,
      install_ids: 1,
      days: 1,
    });
    expect(data.tools.map((tool) => tool.tool)).toEqual(["trace"]);
    expect(data.status_mix).toEqual({ ok: 2 });
    expect(data.truncation).toEqual({ count: 0, rate: 0, reasons: {} });
    expect(data.relations).toEqual({ eq: 0, gte: 2, eq_ratio: 0 });
    expect(data.arg_names).toEqual([]);

    const html = renderHtml(data, META);
    expect(html).toContain("trace");
    expect(html).not.toContain("ghost-tool");
    expect(html).not.toContain("future-tool");
    expect(html).not.toContain("torn-tool");
  });
});

describe("determinism", () => {
  test("two renders of one report are byte-identical and two folds are deeply equal", () => {
    const records: TelemetryRecord[] = [
      callRecord({ tool: "trace", duration_ms: 12, arg_names_set: ["depth", "query"] }),
      callRecord({ tool: "search", duration_ms: 30, status: "not_found" }),
      indexRunRecord({ ts: "2026-11-18T10:00:00.000Z", unresolved_ratio: 0.25 }),
      sessionRecord(),
    ];

    const first = aggregateOf(records);
    const second = aggregateOf(records);

    expect(second).toEqual(first);
    expect(JSON.stringify(second)).toBe(JSON.stringify(first));

    const once = renderHtml(first, META);
    expect(renderHtml(second, META)).toBe(once);
    expect(renderHtml(first, META)).toBe(once);
  });
});

describe("--since", () => {
  test("only a calendar day or Nd with N >= 1 is a literal", () => {
    expect(isSinceLiteral("2026-11-01")).toBe(true);
    expect(isSinceLiteral("7d")).toBe(true);

    for (const rejected of ["2026-13-01", "0d", "7", "7days", ""]) {
      expect(isSinceLiteral(rejected)).toBe(false);
    }
  });

  test("7d is now minus seven days floored to the UTC day, and readRecords drops the rest", () => {
    const now = new Date("2026-11-18T13:45:23.000Z");
    const since = parseSince("7d", now);

    expect(since?.toISOString()).toBe("2026-11-11T00:00:00.000Z");
    // Seven days plus the part of the day the floor removed: the shift happens before the floor.
    expect(now.getTime() - (since?.getTime() ?? 0)).toBe(
      7 * 86_400_000 + (13 * 3_600 + 45 * 60 + 23) * 1_000,
    );
    expect(parseSince("2026-11-01", now)?.toISOString()).toBe("2026-11-01T00:00:00.000Z");
    expect(parseSince(undefined, now)).toBeNull();
    expect(parseSince("2026-13-01", now)).toBeNull();

    const dir = tempDir("sangkar-telemetry-report-since-");
    writeJsonl(dir, "2026-11-18.jsonl", [
      lineOf(callRecord({ ts: "2026-11-10T23:59:59.000Z" })),
      lineOf(callRecord({ ts: "2026-11-11T00:00:00.000Z" })),
      lineOf(callRecord({ ts: "2026-11-18T12:00:00.000Z" })),
    ]);

    const read = readRecords(dir, { since });
    expect(read.records.map((record) => record.ts)).toEqual([
      "2026-11-11T00:00:00.000Z",
      "2026-11-18T12:00:00.000Z",
    ]);
    expect(read.unparsed).toBe(0);
    expect(read.files).toEqual(["2026-11-18.jsonl"]);
  });
});
