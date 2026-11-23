import { afterAll, describe, expect, spyOn, test } from "bun:test";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig } from "#shared/config";
import { STALE_REASONS } from "#shared/domain/freshness";
import { type Coverage, type EnvelopeSpec, ok, type ReadStatus } from "#shared/envelope";
import type { Logger } from "#shared/log";
import { resolveInstallId } from "#shared/telemetry/identity";
import {
  type CallRecord,
  type CallSpec,
  callRecordOf,
  type IndexRunRecord,
  type IndexRunSpec,
  indexRunRecordOf,
  type RecordContext,
  type SessionFacts,
  type SessionRecord,
  sessionRecordOf,
  type TelemetryRecord,
} from "#shared/telemetry/record";
import { jsonlSink, nullSink, type TelemetrySink } from "#shared/telemetry/sink";
import {
  createTelemetry,
  FLUSH_AT_MS,
  FLUSH_AT_RECORDS,
  type Telemetry,
  type TelemetryOptions,
} from "#shared/telemetry/telemetry";

/**
 * The telemetry contract, executable. Each case is a promise the recorder made: an exact field
 * order per kind, an install id that covers one UTC day, one spelling for the off switch, a sink
 * failure that costs records rather than calls, the privacy invariant that no argument value,
 * symbol name or path reaches a line, and a flush that is bounded, dated and silent on stdout.
 *
 * Nothing here calls a tool handler or opens a database: a record is built from a hand-written
 * spec and a recorder from a `mkdtemp` directory with an injected clock and sink.
 *
 * The one case deliberately missing is "the off switch creates no `telemetry/` directory": off
 * yields `nullTelemetry()`, which takes no options and holds no path, so the assertion could not
 * fail. It belongs to the integration task that spawns the shipped CLI with `SANGKAR_TELEMETRY=off`.
 */

const FIXED_TS = "2026-11-18T10:00:00.000Z";
const FIXED_DATE = "2026-11-18";
const SERVER_VERSION = "0.1.0-beta.0";

/** plan.approach.md §3, in serialisation order; `v` sits second so a reader checks it first. */
const CALL_FIELDS = [
  "ts",
  "v",
  "kind",
  "install_id",
  "session_id",
  "seq",
  "tool",
  "status",
  "error_code",
  "duration_ms",
  "arg_names_set",
  "budget_tokens",
  "payload_tokens",
  "budget_used_ratio",
  "truncated",
  "truncation_reason",
  "total_relation",
  "coverage_complete",
  "coverage_indexed",
  "coverage_skipped",
  "coverage_failed",
  "unresolved_edges",
  "stale",
  "stale_reason",
  "generation_age_ms",
  "result_counts",
  "server_version",
  "bun_version",
  "os",
  "arch",
  "graph_schema_version",
];

const INDEX_RUN_FIELDS = [
  "ts",
  "v",
  "kind",
  "install_id",
  "session_id",
  "mode",
  "exit_code",
  "duration_ms",
  "was_forced",
  "generation_advanced",
  "files_seen",
  "files_indexed",
  "files_skipped",
  "files_failed",
  "nodes_by_kind",
  "edges_by_kind_tier",
  "unresolved_count",
  "unresolved_ratio",
  "languages",
  "db_size_bytes",
  "files_changed_in_run",
];

const SESSION_FIELDS = [
  "ts",
  "v",
  "kind",
  "install_id",
  "session_id",
  "session_mode",
  "server_version",
  "bun_version",
  "os",
  "arch",
  "is_first_run",
  "repo_size_bucket",
  "languages",
  "root_kind",
];

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

function contextOf(overrides: Partial<RecordContext> = {}): RecordContext {
  return {
    ts: FIXED_TS,
    installId: "0f".repeat(16),
    sessionId: "1a".repeat(16),
    seq: 1,
    serverVersion: SERVER_VERSION,
    bunVersion: "1.4.4",
    os: "linux",
    arch: "x64",
    graphSchemaVersion: 1,
    ...overrides,
  };
}

const CLEAN_COVERAGE: Coverage = {
  complete: true,
  indexed: 12,
  skipped: 1,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

function readStatusOf(overrides: Partial<ReadStatus> = {}): ReadStatus {
  return {
    generation: "g-1",
    indexed_at: "2026-11-18T09:00:00.000Z",
    stale: false,
    ...overrides,
  };
}

function specOf(overrides: Partial<EnvelopeSpec> = {}): EnvelopeSpec {
  return {
    summary: "trace handleRequest: 2 callers, 1 callee (2 shown)",
    read_status: readStatusOf(),
    coverage: { ...CLEAN_COVERAGE, not_indexed: [] },
    ...overrides,
  };
}

function callSpec(overrides: Partial<CallSpec> = {}): CallSpec {
  return {
    tool: "trace",
    args: { query: "handleRequest", budget: 3200 },
    suppliedNames: ["query"],
    envelope: null,
    status: "ok",
    errorCode: null,
    durationMs: 12,
    ...overrides,
  };
}

function indexRunSpec(overrides: Partial<IndexRunSpec> = {}): IndexRunSpec {
  return {
    mode: "full",
    exitCode: 0,
    durationMs: 5_432,
    wasForced: false,
    filesSeen: 10,
    filesIndexed: 9,
    filesSkipped: 1,
    filesFailed: 0,
    nodesByKind: { function: 20, class: 2 },
    edgesByKindTier: { "calls/extracted": 30 },
    unresolvedCount: 3,
    languages: { typescript: 10 },
    dbSizeBytes: 4_096,
    filesChangedInRun: 4,
    ...overrides,
  };
}

function sessionFacts(overrides: Partial<SessionFacts> = {}): SessionFacts {
  return {
    sessionMode: "mcp",
    isFirstRun: true,
    repoSizeBucket: "s",
    languages: ["typescript"],
    rootKind: "git",
    ...overrides,
  };
}

type RecorderOverrides = Partial<Pick<TelemetryOptions, "sink" | "now" | "log">>;

function openRecorder(dir: string, overrides: RecorderOverrides = {}): Telemetry {
  return createTelemetry({
    dir,
    repoRoot: dir,
    dataDir: dir,
    sessionMode: "cli",
    serverVersion: SERVER_VERSION,
    graphSchemaVersion: 1,
    ...overrides,
  });
}

/** A logger that keeps only the warnings, which is the one level telemetry is asserted on. */
function capturingLogger(warnings: string[]): Logger {
  const ignore = (): void => {
    // Only `warn` carries a telemetry fact; the other three levels are not part of the contract.
  };
  return {
    debug: ignore,
    info: ignore,
    warn: (event) => {
      warnings.push(event);
    },
    error: ignore,
  };
}

/**
 * Every scalar in a parsed line, with the object member `skip` left out. `arg_names_set` is the one
 * dynamic field the privacy rule keeps on purpose — its members are argument *names* — so a
 * substring check on the whole line could not tell a leak from that field doing its job.
 */
function scalarsOf(value: unknown, skip: string): string[] {
  const out: string[] = [];
  const walk = (node: unknown, key: string): void => {
    if (key === skip) {
      return;
    }
    if (Array.isArray(node)) {
      for (const item of node) {
        walk(item, key);
      }
      return;
    }
    if (typeof node === "object" && node !== null) {
      for (const [childKey, child] of Object.entries(node)) {
        walk(child, childKey);
      }
      return;
    }
    out.push(String(node));
  };
  walk(value, "");
  return out;
}

describe("record shape", () => {
  test("a call record carries the plan's fields in order, with v second, and round-trips", () => {
    const record: CallRecord = callRecordOf(
      contextOf(),
      callSpec({ envelope: ok({ calls: [{ id: 1 }, { id: 2 }] }, specOf()) }),
    );

    expect(Object.keys(record)).toEqual(CALL_FIELDS);
    expect(Object.keys(record)[1]).toBe("v");
    expect(record.v).toBe(1);
    expect(record.kind).toBe("call");

    const line = JSON.stringify(record);
    expect(JSON.parse(line)).toEqual(record);
    expect(JSON.stringify(JSON.parse(line))).toBe(line);
  });

  test("an index_run record carries the plan's fields in order, with v second, and round-trips", () => {
    const record: IndexRunRecord = indexRunRecordOf(contextOf(), indexRunSpec());

    expect(Object.keys(record)).toEqual(INDEX_RUN_FIELDS);
    expect(Object.keys(record)[1]).toBe("v");
    expect(record.v).toBe(1);
    expect(record.kind).toBe("index_run");
    // The four server-identity fields belong to `call`/`session`, not to a run.
    expect(Object.keys(record)).not.toContain("bun_version");
    expect(Object.keys(record)).not.toContain("graph_schema_version");

    const line = JSON.stringify(record);
    expect(JSON.parse(line)).toEqual(record);
    expect(JSON.stringify(JSON.parse(line))).toBe(line);
  });

  test("a session record carries the plan's fields in order, with v second, and round-trips", () => {
    const record: SessionRecord = sessionRecordOf(contextOf(), sessionFacts());

    expect(Object.keys(record)).toEqual(SESSION_FIELDS);
    expect(Object.keys(record)[1]).toBe("v");
    expect(record.v).toBe(1);
    expect(record.kind).toBe("session");

    const line = JSON.stringify(record);
    expect(JSON.parse(line)).toEqual(record);
    expect(JSON.stringify(JSON.parse(line))).toBe(line);
  });
});

describe("install identity", () => {
  test("one id per UTC day: stable within a day, rotated on the next, one file left behind", () => {
    const dir = tempDir("sangkar-telemetry-id-");
    const day1Early = new Date("2026-11-18T01:00:00.000Z");
    const day1Late = new Date("2026-11-18T23:00:00.000Z");
    const day2 = new Date("2026-11-19T01:00:00.000Z");

    const first = resolveInstallId(dir, day1Early);
    expect(first.isFirstRun).toBe(true);
    expect(first.installId).toMatch(/^[0-9a-f]{32}$/);

    const later = resolveInstallId(dir, day1Late);
    expect(later.installId).toBe(first.installId);
    expect(later.isFirstRun).toBe(false);

    const tomorrow = resolveInstallId(dir, day2);
    expect(tomorrow.installId).not.toBe(first.installId);
    expect(tomorrow.isFirstRun).toBe(true);
    expect(readdirSync(dir)).toEqual(["install_id.json"]);
  });
});

describe("the consent switch", () => {
  test("exactly `off` disables recording; unset, empty and OFF enable it", () => {
    const root = tempDir("sangkar-telemetry-switch-");
    // Every case gets the fixture's own home, so a machine file in the developer's `$HOME` cannot
    // decide what "unset" means here.
    const configOf = (env: Record<string, string | undefined>) =>
      loadConfig({ env: { SANGKAR_HOME: join(root, "home"), ...env }, cwd: root });

    const off = configOf({ SANGKAR_TELEMETRY: "off" });
    expect(off.telemetry.enabled).toBe(false);
    expect(off.telemetry.dir).toBe(join(off.dataDir, "telemetry"));

    // One spelling, case-sensitive: a typo must not silently opt a machine in or out.
    const enabledCases: Record<string, string | undefined>[] = [
      {},
      { SANGKAR_TELEMETRY: "" },
      { SANGKAR_TELEMETRY: "OFF" },
    ];
    for (const env of enabledCases) {
      const config = configOf(env);
      expect(config.telemetry.enabled).toBe(true);
      expect(config.telemetry.dir).toBe(join(config.dataDir, "telemetry"));
    }
  });
});

describe("a sink that throws", () => {
  test("costs records, not the call: nothing rejects and exactly one warn is logged", () => {
    const dir = tempDir("sangkar-telemetry-throwing-");
    const warnings: string[] = [];
    let writes = 0;
    const sink: TelemetrySink = {
      write() {
        writes += 1;
        throw Object.assign(new Error("no space left on device"), { code: "ENOSPC" });
      },
    };
    const recorder = openRecorder(dir, {
      sink,
      now: () => new Date(FIXED_TS),
      log: capturingLogger(warnings),
    });

    expect(() => recorder.recordCall(callSpec())).not.toThrow();
    expect(() => recorder.flush()).not.toThrow();
    expect(() => recorder.close()).not.toThrow();

    // The sink was reached once, so the "did not reject" cases are not vacuous; the degraded
    // recorder then stays silent rather than retrying or flooding the log.
    expect(writes).toBe(1);
    expect(warnings).toEqual(["telemetry.sink_failed"]);
  });
});

describe("the privacy invariant", () => {
  const SECRET_NAME = "handleRequest";
  const SECRET_PATH = "/home/x/secret-repo/src/core.ts";

  test("no argument value, payload value or summary reaches a call record", () => {
    const spec = callSpec({
      args: { query: SECRET_NAME, file: SECRET_PATH, budget: 3200 },
      suppliedNames: ["query", "file"],
      envelope: ok(
        {
          query: SECRET_NAME,
          node: { path: SECRET_PATH, qn: "src/core.ts::handleRequest" },
          calls: [{ path: SECRET_PATH, name: SECRET_NAME }],
        },
        specOf({ summary: `${SECRET_NAME} has no callers under ${SECRET_PATH}` }),
      ),
    });

    const line = JSON.stringify(callRecordOf(contextOf(), spec));
    const parsed = JSON.parse(line) as Record<string, unknown>;
    const scalars = scalarsOf(parsed, "arg_names_set");

    // A walk over an empty list could not fail, so it has to have seen the whole record.
    expect(scalars.length).toBeGreaterThan(20);
    for (const value of scalars) {
      for (const forbidden of [SECRET_NAME, SECRET_PATH, "secret-repo", "core.ts"]) {
        expect(value).not.toContain(forbidden);
      }
    }

    // The names survive, sorted and deduplicated; only the values are gone.
    expect(parsed.arg_names_set).toEqual(["file", "query"]);
  });

  test("`stale_reason` is a closed enum, so trace's path-bearing prose becomes `other`", () => {
    const envelope = ok(
      {},
      specOf({
        read_status: readStatusOf({
          stale: true,
          stale_reason: '"src/core.ts" was last written by generation 3',
        }),
      }),
    );
    const record = callRecordOf(contextOf(), callSpec({ envelope }));

    expect(record.stale).toBe(true);
    expect(record.stale_reason).toBe("other");

    const parsed = JSON.parse(JSON.stringify(record)) as Record<string, unknown>;
    const scalars = scalarsOf(parsed, "arg_names_set");
    expect(scalars.length).toBeGreaterThan(20);
    for (const value of scalars) {
      for (const forbidden of ["src/core.ts", "core.ts", "generation"]) {
        expect(value).not.toContain(forbidden);
      }
    }
  });

  test("a STALE_REASONS value is kept verbatim and a missing reason records null", () => {
    for (const reason of STALE_REASONS) {
      const envelope = ok(
        {},
        specOf({ read_status: readStatusOf({ stale: true, stale_reason: reason }) }),
      );
      expect(callRecordOf(contextOf(), callSpec({ envelope })).stale_reason).toBe(reason);
    }

    // Named explicitly because it is the wording the resolver writes.
    const rootMismatch = ok(
      {},
      specOf({ read_status: readStatusOf({ stale: true, stale_reason: "root_mismatch" }) }),
    );
    expect(callRecordOf(contextOf(), callSpec({ envelope: rootMismatch })).stale_reason).toBe(
      "root_mismatch",
    );

    // No envelope at all: the answer is "not known", never a copy of anything.
    expect(callRecordOf(contextOf(), callSpec()).stale_reason).toBeNull();
  });
});

describe("the bounded flush", () => {
  test("31 buffered records write nothing; the 32nd writes one line per record in one append", () => {
    const dir = tempDir("sangkar-telemetry-flush-");
    const writes: { date: string; lines: string[] }[] = [];
    const sink: TelemetrySink = {
      write: (date, lines) => {
        writes.push({ date, lines: [...lines] });
      },
    };
    const recorder = openRecorder(dir, { sink, now: () => new Date(FIXED_TS) });

    // The session record is buffered at construction, so 30 calls make 31 buffered records.
    for (let i = 0; i < FLUSH_AT_RECORDS - 2; i += 1) {
      recorder.recordCall(callSpec());
    }
    expect(writes).toHaveLength(0);

    recorder.recordCall(callSpec());

    expect(writes).toHaveLength(1);
    expect(writes[0]?.date).toBe(FIXED_DATE);
    const lines = writes[0]?.lines ?? [];
    expect(lines).toHaveLength(FLUSH_AT_RECORDS);

    const records = lines.map((entry) => JSON.parse(entry) as TelemetryRecord);
    expect(records.filter((record) => record.kind === "session")).toHaveLength(1);
    const calls = records.filter((record) => record.kind === "call");
    expect(calls).toHaveLength(FLUSH_AT_RECORDS - 1);
    expect(calls.map((record) => record.seq)).toEqual(
      Array.from({ length: FLUSH_AT_RECORDS - 1 }, (_unused, index) => index + 1),
    );
  });

  test("an answer older than FLUSH_AT_MS is written on the next record", () => {
    const dir = tempDir("sangkar-telemetry-age-");
    const writes: { date: string; lines: string[] }[] = [];
    let clockMs = Date.parse(FIXED_TS);
    const sink: TelemetrySink = {
      write: (date, lines) => {
        writes.push({ date, lines: [...lines] });
      },
    };
    const recorder = openRecorder(dir, { sink, now: () => new Date(clockMs) });

    recorder.recordCall(callSpec());
    expect(writes).toHaveLength(0);

    // One millisecond short of the threshold: still buffered.
    clockMs = Date.parse(FIXED_TS) + FLUSH_AT_MS - 1;
    recorder.recordCall(callSpec());
    expect(writes).toHaveLength(0);

    clockMs = Date.parse(FIXED_TS) + FLUSH_AT_MS;
    recorder.recordCall(callSpec());

    // The session record plus the three calls: the trigger flushes what is buffered.
    expect(writes).toHaveLength(1);
    expect(writes[0]?.lines).toHaveLength(4);
  });

  test("close() flushes a buffer below the threshold, and a second close is a no-op", () => {
    const dir = tempDir("sangkar-telemetry-close-");
    const writes: { date: string; lines: string[] }[] = [];
    const sink: TelemetrySink = {
      write: (date, lines) => {
        writes.push({ date, lines: [...lines] });
      },
    };
    const recorder = openRecorder(dir, { sink, now: () => new Date(FIXED_TS) });

    recorder.recordCall(callSpec());
    expect(writes).toHaveLength(0);

    recorder.close();
    expect(writes).toHaveLength(1);
    expect(writes[0]?.lines).toHaveLength(2);

    recorder.close();
    expect(writes).toHaveLength(1);
  });

  test("a flush that straddles UTC midnight writes one file per date", () => {
    const dir = tempDir("sangkar-telemetry-midnight-");
    let clockMs = Date.parse("2026-11-18T23:59:59.000Z");
    const recorder = openRecorder(dir, {
      sink: jsonlSink(dir),
      now: () => new Date(clockMs),
    });

    recorder.recordCall(callSpec());
    clockMs = Date.parse("2026-11-19T00:00:01.000Z");
    recorder.recordCall(callSpec());
    recorder.close();

    // `install_id.json` shares the directory, so only the daily files are listed.
    const files = readdirSync(dir)
      .filter((name) => name.endsWith(".jsonl"))
      .sort();
    expect(files).toEqual(["2026-11-18.jsonl", "2026-11-19.jsonl"]);

    const firstDay = readFileSync(join(dir, "2026-11-18.jsonl"), "utf8").trimEnd().split("\n");
    const secondDay = readFileSync(join(dir, "2026-11-19.jsonl"), "utf8").trimEnd().split("\n");
    expect(firstDay).toHaveLength(2);
    expect(secondDay).toHaveLength(1);
  });
});

describe("stdout is the protocol", () => {
  test("recording, flushing and closing write nothing to stdout", () => {
    const dir = tempDir("sangkar-telemetry-stdout-");
    const recorder = openRecorder(dir, {
      sink: nullSink(),
      now: () => new Date(FIXED_TS),
      log: capturingLogger([]),
    });

    // Scoped to the calls below and nothing else: bun's own reporter writes to stdout during the
    // run, so a suite-wide spy would capture the test output rather than the recorder's.
    const chunks: string[] = [];
    const spy = spyOn(process.stdout, "write").mockImplementation(
      (chunk: string | Uint8Array): boolean => {
        chunks.push(String(chunk));
        return true;
      },
    );
    try {
      recorder.recordCall(callSpec());
      recorder.flush();
      recorder.close();
    } finally {
      spy.mockRestore();
    }

    expect(chunks).toEqual([]);
    expect(spy.mock.calls).toHaveLength(0);
  });
});
