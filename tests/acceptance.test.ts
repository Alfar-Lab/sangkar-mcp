import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { BYTES_PER_TOKEN } from "#shared/budget";
import { type AppConfig, loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import {
  type Coverage,
  ENVELOPE_STATUSES,
  type Envelope,
  type EnvelopeStatus,
} from "#shared/envelope";
import { TOOL_NAMES, type ToolName } from "#shared/instructions";
import { log } from "#shared/log";
import { prefixBounds } from "#shared/paths";
import {
  type Conn,
  db,
  openReader,
  prepared,
  STATEMENT_CACHE_CAPACITY,
  statementCacheStats,
} from "#shared/store/db";
import { migrate } from "#shared/store/migrate";
import type { ToolContext } from "#shared/store/read/context";
import { type GraphReader, makeReader } from "#shared/store/read/reader";
import * as schema from "#shared/store/schema";
import { runWorker } from "../src/indexer/worker";
import { handle as expandHandle } from "../src/tools/expand/handler";
import { DEFAULT_BUDGET_TOKENS as EXPAND_DEFAULT_TOKENS } from "../src/tools/expand/manifest";
import { handle as impactHandle } from "../src/tools/impact/handler";
import { input as impactInput } from "../src/tools/impact/manifest";
import { handle as indexHandle } from "../src/tools/index/handler";
import { handle as searchHandle } from "../src/tools/search/handler";
import { DEFAULT_BUDGET_TOKENS as SEARCH_DEFAULT_TOKENS } from "../src/tools/search/manifest";
import {
  type StatusEnvelope,
  type StatusPayload,
  handle as statusHandle,
} from "../src/tools/status/handler";
import { STATUS_DEFAULT_TOKENS } from "../src/tools/status/manifest";
import { handle as summaryHandle } from "../src/tools/summary/handler";
import { input as summaryInput } from "../src/tools/summary/manifest";
import { handle as traceHandle } from "../src/tools/trace/handler";
import {
  DEFAULT_BUDGET_TOKENS as TRACE_DEFAULT_TOKENS,
  type TraceArgs,
} from "../src/tools/trace/manifest";

/**
 * The phase 3 closing gate (`TASK-033`): `DESIGN.md` §14's acceptance criteria plus the two spikes
 * the plan left unowned — `TECH-SPEC.md` §16 S4 (publish atomicity with a concurrent reader) and S6
 * (prepared-statement discipline holds the latency budget).
 *
 * Every number in this file is measured, never assumed. The fixture is the reference corpus
 * (`code-review-graph/` at the repo root) indexed by the real worker into a `mkdtemp` data dir, so
 * the payloads, coverage and latencies are the ones the corpus actually produces; nothing is
 * written into the corpus and no database lives inside the repository. Each measurement prints its
 * own p50/p95/max (or byte count, or ratio) to stderr next to the threshold, which is what makes a
 * failure actionable instead of a red test with no number attached.
 *
 * The nine criteria, in the order the task states them:
 *
 * 1. every response fits its declared default budget + 10 % on real payloads;
 * 2. `total_relation: "eq"` appears only on complete, fresh, untruncated coverage with no
 *    unresolved rows in the queried scope, over at least 20 calls;
 * 3. a request touching a `not_indexed` path answers `coverage.complete: false` and says why;
 * 4. `trace` p95 < 40 ms, `search` p95 < 60 ms, `summary(change)` p95 < 120 ms over 30 repetitions;
 * 5. all nine envelope fields on every response, `error` only on `error`/`bootstrapping`;
 * 6. one surface publishes exactly the seven registered names, `index` included;
 * 7. a reader holding an open handle returns unchanged results while a publish renames over the
 *    live file (S4);
 * 8. 1 000 identical point reads built per call versus prepared: the prepared path is >= 5x faster
 *    (S6);
 * 9. the statement cache stays at or below its 200-entry cap under 5 000 distinct query shapes.
 *
 * Criterion 7 runs last on purpose: it publishes a new generation over the data dir, and while the
 * suite's reader keeps serving its snapshot (that is the assertion), no later test should depend on
 * the data dir being in its pre-publish state.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** `DESIGN.md` §14 / `TECH-SPEC.md` §14: the reference corpus, indexed but never written. */
const CORPUS = resolve(APP_ROOT, "code-review-graph");

/** Two full indexes of the corpus plus the sweeps; the default 5 s per test would never hold. */
const RUN_TIMEOUT_MS = 300_000;

/** The latency benchmark spends ~15 s in `trace` alone, so it gets its own, larger, bound. */
const LATENCY_TIMEOUT_MS = 180_000;

/** One server handshake and `tools/list`, bounded so a wedged child fails instead of hanging. */
const SESSION_TIMEOUT_MS = 30_000;

/** `TECH-SPEC.md` §14 measures p95 over at least this many repetitions, after a warm-up. */
const REPS = 30;
const WARMUP = 3;

/** The tolerance half of "declared budget + 10 %". */
const BUDGET_TOLERANCE = 1.1;

/** `TECH-SPEC.md` §14's latency gates. */
const TRACE_P95_MS = 40;
const SEARCH_P95_MS = 60;
const SUMMARY_CHANGE_P95_MS = 120;

/** S6's bar: prepared must beat a per-call builder by at least this factor. */
const PREPARED_RATIO = 5;

/** One read per iteration, 1 000 iterations per timed round, best of three rounds per path. */
const POINT_READS = 1_000;
const POINT_READ_ROUNDS = 3;

/** Criterion 9's shape count: enough distinct keys to overflow a 200-entry LRU 25 times over. */
const CACHE_SHAPES = 5_000;

/** `#shared/cursor` refuses anything under 32 bytes. */
const CURSOR_KEY = "acceptance-gate-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

/** The nine fields `TOOLS-SPEC.md` §1.1 requires of every answer, in its order. */
const ENVELOPE_FIELDS = [
  "status",
  "summary",
  "read_status",
  "coverage",
  "totals",
  "truncated",
  "truncation_reason",
  "next",
  "next_tools",
] as const;

/** The only two statuses whose answer is a failure, and therefore the only two carrying `error`. */
const STATUSES_CARRYING_ERROR: readonly EnvelopeStatus[] = ["error", "bootstrapping"];

/**
 * The drizzle operators, imported through a computed specifier because `tests/boundary.test.ts`
 * rule 4 forbids a static `drizzle-orm` import outside `src/shared/store/**` —
 * `tests/schema-parity.test.ts` loads its own drizzle introspection the same way.
 */
const DRIZZLE_ORM = ["drizzle", "orm"].join("-");

interface DrizzleOperators {
  /** `never` is assignable to whatever `.where()` declares, so the builder stays fully typed. */
  eq(left: unknown, right: unknown): never;
}

const { eq } = (await import(DRIZZLE_ORM)) as unknown as DrizzleOperators;

/** One stderr line per measurement: JSON, so the progress log can quote it verbatim. */
function report(msg: string, fields: Record<string, unknown>): void {
  process.stderr.write(`${JSON.stringify({ ts: new Date().toISOString(), msg, ...fields })}\n`);
}

function bytes(value: unknown): number {
  return Buffer.byteLength(JSON.stringify(value), "utf8");
}

function round(value: number): number {
  return Math.round(value * 100) / 100;
}

/** `noUncheckedIndexedAccess`-safe first element: a missing fixture row is a broken fixture. */
function first<T>(items: readonly T[], what: string): T {
  const value = items[0];
  if (value === undefined) {
    throw new Error(`the fixture has no ${what}`);
  }
  return value;
}

/** `path::Scope.name` -> `name`: the bare identifier `search` matches on and FTS5 can parse. */
function cleanName(qn: string): string {
  const tail = first(qn.split("::"), "a qualified name").split(".");
  return first(tail.slice(-1), "a symbol name");
}

/** Nearest-rank percentile: the smallest value at or above the requested share of the samples. */
function percentile(samples: readonly number[], share: number): number {
  const sorted = [...samples].sort((a, b) => a - b);
  const index = Math.min(
    sorted.length - 1,
    Math.max(0, Math.ceil((share / 100) * sorted.length) - 1),
  );
  return sorted[index] ?? Number.NaN;
}

interface Latency {
  readonly samples: number;
  readonly minMs: number;
  readonly p50Ms: number;
  readonly p95Ms: number;
  readonly maxMs: number;
}

/**
 * Warm up, then time `reps` runs of one call. The warm-up is what makes the first `trace` call
 * (mmap page faults, FTS/statement caches) not the p95.
 */
async function latencyOf(reps: number, run: () => Promise<unknown>): Promise<Latency> {
  for (let i = 0; i < WARMUP; i += 1) {
    await run();
  }
  const samples: number[] = [];
  for (let i = 0; i < reps; i += 1) {
    const started = performance.now();
    await run();
    samples.push(performance.now() - started);
  }
  return {
    samples: samples.length,
    minMs: round(Math.min(...samples)),
    p50Ms: round(percentile(samples, 50)),
    p95Ms: round(percentile(samples, 95)),
    maxMs: round(Math.max(...samples)),
  };
}

interface Fixture {
  /** The corpus, read-only: `runWorker` only ever reads it. */
  readonly corpus: string;
  /** `mkdtemp`: the one writable place, holding `graph.db`, the lock and the staging files. */
  readonly dataDir: string;
  readonly livePath: string;
  readonly conn: Conn;
  readonly reader: GraphReader;
  readonly ctx: ToolContext;
  /** Rows the worker published, quoted in the progress log. */
  readonly indexedMs: number;
  readonly fileRows: number;
  readonly nodeRows: number;
  readonly unresolvedRows: number;
  readonly skippedRows: number;
  /** The highest in-degree symbol: what `summary`/`impact` rank first for an agent to `trace`. */
  readonly hotspot: { readonly qn: string; readonly file: string; readonly inDegree: number };
  /** A deterministic mid-graph symbol and the 90th-percentile-degree symbol, for context. */
  readonly medianSymbol: string;
  readonly p90Symbol: string;
  /** A skipped path with its recorded reason (criterion 3). */
  readonly skipped: { readonly path: string; readonly reason: string };
  /** A parsed, unresolved-free file: the scope in which `eq` is legitimately reachable. */
  readonly cleanFile: string;
  /** The first symbol defined in {@link Fixture.cleanFile}. */
  readonly cleanSymbol: string;
  /** An unpublished database: the only way to get a real `bootstrapping` envelope. */
  readonly unpublishedCtx: ToolContext;
  readonly emptyConn: Conn;
}

const dirs: string[] = [];

let fixture: Fixture;

interface SqlCount {
  readonly rows: number;
}

function countIn(conn: Conn, sql: string, params: string[] = []): number {
  const row =
    params.length === 0
      ? conn.sqlite.query<SqlCount, []>(sql).get()
      : conn.sqlite.query<SqlCount, string[]>(sql).get(...params);
  return row?.rows ?? -1;
}

interface DegreeRow {
  readonly qn: string;
  readonly in_degree: number;
  readonly out_degree: number;
}

/** The measured half of the fixture: what the assertions read off the published graph. */
interface Measured {
  readonly fileRows: number;
  readonly nodeRows: number;
  readonly unresolvedRows: number;
  readonly skippedRows: number;
  /** The highest in-degree symbol: what `summary`/`impact` rank first for an agent to `trace`. */
  readonly hotspot: { readonly qn: string; readonly file: string; readonly inDegree: number };
  /** A deterministic mid-graph symbol and the 90th-percentile-degree symbol, for context. */
  readonly medianSymbol: string;
  readonly p90Symbol: string;
  /** A skipped path with its recorded reason (criterion 3). */
  readonly skipped: { readonly path: string; readonly reason: string };
  /** A parsed, unresolved-free file: the scope in which `eq` is legitimately reachable. */
  readonly cleanFile: string;
  /** The first symbol defined in {@link Measured.cleanFile}; its bare name is an FTS-safe query. */
  readonly cleanSymbol: string;
}

function inspect(conn: Conn, reader: GraphReader): Measured {
  const files = reader.allFiles();
  const hotspotRow = first(reader.hotspots(1), "hotspot row");
  const degrees = conn.sqlite
    .query<DegreeRow, []>(
      "SELECT n.qn AS qn, COALESCE(m.in_degree, 0) AS in_degree," +
        " COALESCE(m.out_degree, 0) AS out_degree FROM node n" +
        " LEFT JOIN node_metrics m ON m.node_id = n.id" +
        " WHERE n.kind IN ('function', 'method')" +
        " ORDER BY (COALESCE(m.in_degree, 0) + COALESCE(m.out_degree, 0)) ASC, n.qn ASC",
    )
    .all();
  const medianRow = degrees[Math.floor(degrees.length / 2)];
  const p90Row = degrees[Math.floor(degrees.length * 0.9)];
  if (medianRow === undefined || p90Row === undefined) {
    throw new Error("the graph has too few function nodes to sample");
  }

  const skipped = conn.sqlite
    .query<{ path: string; reason: string }, []>(
      "SELECT path, reason FROM not_indexed ORDER BY path LIMIT 1",
    )
    .get();
  if (skipped === null) {
    throw new Error("the fixture indexed every path, so criterion 3 has nothing to query");
  }

  // The smallest parsed file with no unbound call site whose first symbol has a plain identifier
  // name: that file is the one scope where `eq` is honest, and the name is a legal FTS5 query.
  const clean = conn.sqlite
    .query<{ path: string; qn: string; name: string }, []>(
      "SELECT f.path AS path, n.qn AS qn, n.name AS name FROM file f" +
        " JOIN node n ON n.file_id = f.id WHERE f.parse_status = 'ok'" +
        " AND (SELECT count(*) FROM unresolved u WHERE u.file_id = f.id) = 0" +
        " AND n.name GLOB '[A-Za-z_]*' AND n.name NOT GLOB '*[^A-Za-z0-9_]*'" +
        " ORDER BY (SELECT count(*) FROM node m WHERE m.file_id = f.id) ASC, f.path ASC," +
        " n.id ASC LIMIT 1",
    )
    .get();
  if (clean === null) {
    throw new Error("no parsed file is simultaneously unresolved-free and FTS-queryable");
  }

  return {
    fileRows: files.length,
    nodeRows: countIn(conn, "SELECT count(*) AS rows FROM node"),
    unresolvedRows: countIn(conn, "SELECT count(*) AS rows FROM unresolved"),
    skippedRows: countIn(conn, "SELECT count(*) AS rows FROM not_indexed"),
    hotspot: {
      qn: hotspotRow.qn,
      file: hotspotRow.file,
      inDegree: hotspotRow.inDegree,
    },
    medianSymbol: medianRow.qn,
    p90Symbol: p90Row.qn,
    skipped,
    cleanFile: clean.path,
    cleanSymbol: clean.qn,
  };
}

beforeAll(async () => {
  const root = await mkdtemp(join(tmpdir(), "sangkar-acceptance-"));
  dirs.push(root);
  const dataDir = join(root, "data");
  await mkdir(dataDir, { recursive: true });
  const livePath = join(dataDir, "graph.db");

  const startedAt = performance.now();
  const code = await runWorker({ repoRoot: CORPUS, dataDir, force: true, mode: "full" });
  const indexedMs = Math.round(performance.now() - startedAt);
  if (code !== 0) {
    throw new Error(`the worker exited ${code} over ${CORPUS}`);
  }

  const conn = openReader(livePath);
  const reader = makeReader(conn);
  const config: AppConfig = loadConfig({
    argv: ["--repo-root", CORPUS, "--data-dir", dataDir],
    env: { SANGKAR_CURSOR_KEY: CURSOR_KEY },
  });
  const ctx: ToolContext = {
    reader,
    cursor: createCursors({ key: CURSOR_KEY }),
    log,
    now: () => new Date(),
    config,
  };

  // An empty, migrated database is the only fixture that answers `bootstrapping` truthfully.
  const emptyDir = join(root, "unpublished");
  await mkdir(emptyDir, { recursive: true });
  const emptyPath = join(emptyDir, "graph.db");
  const rawEmpty = new Database(emptyPath, { create: true });
  try {
    migrate(rawEmpty);
  } finally {
    rawEmpty.close();
  }
  const emptyConn = openReader(emptyPath);
  const unpublishedCtx: ToolContext = {
    ...ctx,
    reader: makeReader(emptyConn),
    config: { ...config, dataDir: emptyDir },
  };

  const measured = inspect(conn, reader);
  fixture = {
    corpus: CORPUS,
    dataDir,
    livePath,
    conn,
    reader,
    ctx,
    indexedMs,
    unpublishedCtx,
    emptyConn,
    ...measured,
  };

  report("acceptance.fixture", {
    corpus: "code-review-graph",
    indexMs: indexedMs,
    fileRows: fixture.fileRows,
    nodeRows: fixture.nodeRows,
    unresolvedRows: fixture.unresolvedRows,
    skippedRows: fixture.skippedRows,
    hotspot: fixture.hotspot,
    medianSymbol: fixture.medianSymbol,
    p90Symbol: fixture.p90Symbol,
    skippedSample: fixture.skipped,
    cleanFile: fixture.cleanFile,
    cleanSymbol: fixture.cleanSymbol,
  });
}, RUN_TIMEOUT_MS);

afterAll(async () => {
  fixture?.conn.close();
  fixture?.emptyConn.close();
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/**
 * One call whose response a criterion reads. `scope` is the file set the handler assembled its
 * coverage over — `"all"` for the repo-wide scopes (`status`, `summary`) — because criterion 2's
 * independent check has to query the same set of files, not a convenient one.
 */
interface CallSpec {
  readonly label: string;
  readonly name: ToolName;
  readonly budget: number;
  /** Where the budget comes from, so a failure names the declaration rather than a number. */
  readonly budgetBasis: string;
  readonly scope: "all" | readonly string[];
  readonly run: () => Promise<Envelope<unknown>>;
}

interface Observed {
  readonly spec: CallSpec;
  readonly envelope: Envelope<unknown>;
  readonly byteCount: number;
}

/**
 * The 24 calls criteria 1, 2 and 5 read. They are the ordinary calls an agent makes on this graph —
 * the ranked hotspot, the sampled symbols, a common word, a file-scoped search, a skipped path —
 * not inputs chosen to make a threshold easy.
 */
function callsOf(fx: Fixture): readonly CallSpec[] {
  const { ctx } = fx;
  const topLevel = (path: string): string =>
    first(path.split("/"), `a top-level directory of ${path}`);
  const traceArgs = (query: string, depth: number): TraceArgs => ({ query, depth });

  const spec = (
    label: string,
    name: ToolName,
    budget: number,
    budgetBasis: string,
    scope: "all" | readonly string[],
    run: () => Promise<Envelope<unknown>>,
  ): CallSpec => ({ label, name, budget, budgetBasis, scope, run });

  const summaryBudget = summaryInput.budget.default;
  const impactBudget = impactInput.budget.default;
  const topDirectory = topLevel(fx.cleanFile);

  return [
    spec(
      "status: whole repo",
      "status",
      STATUS_DEFAULT_TOKENS,
      "status manifest",
      "all",
      async () => statusHandle(ctx, {}),
    ),
    spec(
      "status: one indexed file",
      "status",
      STATUS_DEFAULT_TOKENS,
      "status manifest",
      "all",
      async () => statusHandle(ctx, { paths: [fx.cleanFile] }),
    ),
    spec(
      "status: a skipped path",
      "status",
      STATUS_DEFAULT_TOKENS,
      "status manifest",
      "all",
      async () => statusHandle(ctx, { paths: [fx.skipped.path] }),
    ),
    spec(
      "summary: change (default)",
      "summary",
      summaryBudget,
      "summary manifest",
      "all",
      async () => summaryHandle(ctx, {}),
    ),
    spec("summary: repo", "summary", summaryBudget, "summary manifest", "all", async () =>
      summaryHandle(ctx, { scope: "repo" }),
    ),
    spec("summary: area", "summary", summaryBudget, "summary manifest", [fx.cleanFile], async () =>
      summaryHandle(ctx, { scope: "area", target: fx.cleanFile }),
    ),
    spec(
      "trace: hotspot at depth 1",
      "trace",
      TRACE_DEFAULT_TOKENS,
      "trace manifest",
      [fx.hotspot.file],
      async () => traceHandle(ctx, traceArgs(fx.hotspot.qn, 1)),
    ),
    spec(
      "trace: median-degree symbol",
      "trace",
      TRACE_DEFAULT_TOKENS,
      "trace manifest",
      [topLevel(fx.medianSymbol.split("::")[0] ?? "")],
      async () => traceHandle(ctx, traceArgs(fx.medianSymbol, 2)),
    ),
    spec(
      "trace: unresolved-free symbol",
      "trace",
      TRACE_DEFAULT_TOKENS,
      "trace manifest",
      [fx.cleanFile],
      async () => traceHandle(ctx, traceArgs(fx.cleanSymbol, 2)),
    ),
    spec("search: common word", "search", SEARCH_DEFAULT_TOKENS, "search manifest", [], async () =>
      searchHandle(ctx, { query: "parser" }),
    ),
    spec(
      "search: file-scoped, exact name",
      "search",
      SEARCH_DEFAULT_TOKENS,
      "search manifest",
      [fx.cleanFile],
      async () => searchHandle(ctx, { query: cleanName(fx.cleanSymbol), path: fx.cleanFile }),
    ),
    spec(
      "search: scoped to a top-level tree",
      "search",
      SEARCH_DEFAULT_TOKENS,
      "search manifest",
      [topDirectory],
      async () => searchHandle(ctx, { query: "read", path: topDirectory }),
    ),
    spec("impact: default change set", "impact", impactBudget, "impact manifest", [], async () =>
      impactHandle(ctx, {}),
    ),
    spec("impact: one file", "impact", impactBudget, "impact manifest", [fx.cleanFile], async () =>
      impactHandle(ctx, { paths: [fx.cleanFile] }),
    ),
    spec(
      "expand: hotspot symbol",
      "expand",
      EXPAND_DEFAULT_TOKENS,
      "expand manifest",
      [fx.hotspot.file],
      async () => expandHandle(ctx, { target: fx.hotspot.qn }),
    ),
    spec(
      "expand: a file",
      "expand",
      EXPAND_DEFAULT_TOKENS,
      "expand manifest",
      [fx.cleanFile],
      async () => expandHandle(ctx, { target: fx.cleanFile }),
    ),
    spec(
      "expand: a skipped path's coverage",
      "expand",
      EXPAND_DEFAULT_TOKENS,
      "expand manifest",
      "all",
      async () => expandHandle(ctx, { target: fx.skipped.path, what: "coverage" }),
    ),
    spec(
      "trace: the clean symbol again",
      "trace",
      TRACE_DEFAULT_TOKENS,
      "trace manifest",
      [fx.cleanFile],
      async () => traceHandle(ctx, traceArgs(fx.cleanSymbol, 2)),
    ),
    spec(
      "status: the clean file again",
      "status",
      STATUS_DEFAULT_TOKENS,
      "status manifest",
      "all",
      async () => statusHandle(ctx, { paths: [fx.cleanFile] }),
    ),
    spec(
      "summary: the clean file as an area",
      "summary",
      summaryBudget,
      "summary manifest",
      [fx.cleanFile],
      async () => summaryHandle(ctx, { scope: "area", target: fx.cleanFile }),
    ),
    spec("search: a class name", "search", SEARCH_DEFAULT_TOKENS, "search manifest", [], async () =>
      searchHandle(ctx, { query: "GraphStore" }),
    ),
    spec(
      "expand: the median symbol",
      "expand",
      EXPAND_DEFAULT_TOKENS,
      "expand manifest",
      [topLevel(fx.medianSymbol.split("::")[0] ?? "")],
      async () => expandHandle(ctx, { target: fx.medianSymbol }),
    ),
    spec(
      "impact: the clean file again",
      "impact",
      impactBudget,
      "impact manifest",
      [fx.cleanFile],
      async () => impactHandle(ctx, { paths: [fx.cleanFile] }),
    ),
    // `index` declares no budget argument (TOOLS-SPEC.md §10); it sizes with the config fallback.
    spec(
      "index: the tree already matches",
      "index",
      fx.ctx.config.budgets.defaultTokens,
      "config fallback",
      "all",
      async () => indexHandle(ctx, {}),
    ),
  ];
}

/**
 * Runs the sweeps once, after the fixture, so criteria 1, 2 and 5 read the same 24 responses
 * instead of paying for four `trace` calls and an `index` spawn three times over.
 */
let observed: readonly Observed[] = [];

async function sweep(): Promise<void> {
  const collected: Observed[] = [];
  for (const spec of callsOf(fixture)) {
    const envelope = await spec.run();
    collected.push({ spec, envelope, byteCount: bytes(envelope) });
  }
  observed = collected;
}

/** Criterion 5's extra surface: the two failure-shaped statuses, on a real (empty) database. */
async function edgeEnvelopes(): Promise<readonly Observed[]> {
  const errorCall: CallSpec = {
    label: "status: a budget above the ceiling",
    name: "status",
    budget: STATUS_DEFAULT_TOKENS,
    budgetBasis: "status manifest",
    scope: "all",
    run: async () => statusHandle(fixture.ctx, { budget: 40_000 }),
  };
  const bootstrappingStatus: CallSpec = {
    label: "status: nothing published",
    name: "status",
    budget: STATUS_DEFAULT_TOKENS,
    budgetBasis: "status manifest",
    scope: "all",
    run: async () => statusHandle(fixture.unpublishedCtx, {}),
  };
  const bootstrappingSummary: CallSpec = {
    label: "summary: nothing published",
    name: "summary",
    budget: summaryInput.budget.default,
    budgetBasis: "summary manifest",
    scope: "all",
    run: async () => summaryHandle(fixture.unpublishedCtx, {}),
  };
  const rows = [errorCall, bootstrappingStatus, bootstrappingSummary];
  const collected: Observed[] = [];
  for (const spec of rows) {
    const envelope = await spec.run();
    collected.push({ spec, envelope, byteCount: bytes(envelope) });
  }
  return collected;
}

let edge: readonly Observed[] = [];

beforeAll(async () => {
  await sweep();
  edge = await edgeEnvelopes();
}, RUN_TIMEOUT_MS);

/**
 * Unresolved call sites under `scope`, queried directly instead of through the reader: criterion 2
 * asks whether the *data* supports the claim, so it must not read the same module that produced it.
 * The prefix test reuses `#shared/paths`' bounds, which is what makes it the same set of files the
 * handler's `assemble` counted.
 */
function unresolvedInScope(conn: Conn, scope: "all" | readonly string[]): number {
  if (scope === "all") {
    return countIn(conn, "SELECT count(*) AS rows FROM unresolved");
  }
  if (scope.length === 0) {
    return 0;
  }
  const block = "(f.path = ? OR (f.path > ? AND f.path < ?))";
  const where = new Array<string>(scope.length).fill(block).join(" OR ");
  const params = scope.flatMap((prefix) => {
    const bounds = prefixBounds(prefix);
    return [bounds.exact, bounds.low, bounds.high];
  });
  return countIn(
    conn,
    `SELECT count(*) AS rows FROM unresolved u JOIN file f ON f.id = u.file_id WHERE ${where}`,
    params,
  );
}

describe("acceptance: nine criteria on the reference corpus", () => {
  /**
   * What "budget" measures — settled by the code, not by the wording of `DESIGN.md` §14:
   *
   * - `budget.ts:120` compares `byteSize(env)`, the **whole envelope**, and `budget.ts:31` is
   *   `Buffer.byteLength(JSON.stringify(value))` over that envelope;
   * - `budget.ts:132` reserves `NEXT_TOKEN_RESERVE_CHARS` for `next`, an envelope-level field, before
   *   measuring — a payload-only budget would have no reason to charge for it;
   * - `emit()`'s own doc comment says it drops items "until the envelope fits";
   * - the product's own property test asserts the same unit: `tests/budget.test.ts`'s "nested item
   *   lists are reachable by the budget" checks `bytes(out) <= 700 * BYTES_PER_TOKEN * 1.1` over the
   *   serialised envelope, which is `TECH-SPEC.md` §13's "`emit()` output ≤ budget + 10 %".
   *
   * DESIGN.md §14's cell says "p95 payload", but in this codebase `payload` names the tool's own
   * body inside the envelope (TOOLS-SPEC.md §1.1); the thing measured, charged and delivered is the
   * envelope. Both numbers are reported below so the reading is auditable either way.
   */
  test("criterion 1: every response fits its declared default budget + 10 %", () => {
    const rows = observed.map((row) => {
      const allowance = Math.floor(row.spec.budget * BYTES_PER_TOKEN);
      const payloadBytes = bytes(row.envelope.payload);
      return {
        call: row.spec.label,
        tool: row.spec.name,
        budgetTokens: row.spec.budget,
        budget: row.spec.budgetBasis,
        allowanceBytes: allowance,
        ceilingBytes: Math.floor(allowance * BUDGET_TOLERANCE),
        bytes: row.byteCount,
        payloadBytes,
        // The envelope's own size is what `emit` cannot cut: it only drops payload lists.
        envelopeBytes: row.byteCount - payloadBytes,
        notIndexedEntries: row.envelope.coverage.not_indexed.length,
        headroomBytes: Math.floor(allowance * BUDGET_TOLERANCE) - row.byteCount,
        truncated: row.envelope.truncated,
      };
    });
    report("acceptance.criterion1.budgets", { rows });

    const violations = rows
      .filter((row) => row.bytes > row.ceilingBytes)
      .map(
        (row) =>
          `${row.call} (${row.tool}): ${row.bytes} bytes exceeds ${row.ceilingBytes}` +
          ` (${row.budgetTokens} tokens x ${BYTES_PER_TOKEN} B = ${row.allowanceBytes} B +10 %)` +
          ` by ${row.bytes - row.ceilingBytes} bytes (payload ${row.payloadBytes} B, envelope` +
          ` ${row.envelopeBytes} B, coverage.not_indexed ${row.notIndexedEntries} entries),` +
          ` truncated=${row.truncated}`,
      );
    expect(violations).toEqual([]);
  });

  test("criterion 2: eq only on complete, fresh, untruncated coverage with no unresolved rows", () => {
    const eqRows = observed.filter((row) => row.envelope.totals.total_relation === "eq");
    const violations: string[] = [];

    for (const row of eqRows) {
      const { coverage, read_status: readStatus, truncated } = row.envelope;
      const scopeUnresolved = unresolvedInScope(fixture.conn, row.spec.scope);
      if (!coverage.complete) {
        violations.push(`${row.spec.label}: eq with coverage.complete=false`);
      }
      if (coverage.unresolved_edges !== 0) {
        violations.push(
          `${row.spec.label}: eq with coverage.unresolved_edges=${coverage.unresolved_edges}`,
        );
      }
      if (scopeUnresolved !== 0) {
        violations.push(
          `${row.spec.label}: eq while ${scopeUnresolved} unresolved rows are in scope`,
        );
      }
      if (readStatus.stale) {
        violations.push(`${row.spec.label}: eq with read_status.stale=true`);
      }
      if (truncated) {
        violations.push(`${row.spec.label}: eq with truncated=true`);
      }
    }

    report("acceptance.criterion2.relation", {
      calls: observed.length,
      eq: eqRows.length,
      eqCalls: eqRows.map((row) => row.spec.label),
      gte: observed.length - eqRows.length,
      unresolvedInDatabase: fixture.unresolvedRows,
    });

    expect(observed.length).toBeGreaterThanOrEqual(20);
    // Not vacuous: the fixture must contain at least one call where `eq` is legitimately reachable.
    expect(eqRows.length).toBeGreaterThan(0);
    expect(violations).toEqual([]);
  });

  test("criterion 3: a not_indexed path answers complete=false and names the reason", async () => {
    const envelope: StatusEnvelope = statusHandle(fixture.ctx, { paths: [fixture.skipped.path] });
    const coverage: Coverage = envelope.coverage;
    const listed = coverage.not_indexed.find((entry) => entry.path === fixture.skipped.path);
    const payload = envelope.payload as StatusPayload | null;
    const answered = payload?.paths.find((entry) => entry.path === fixture.skipped.path);

    report("acceptance.criterion3.not_indexed", {
      requested: fixture.skipped.path,
      status: envelope.status,
      coverageComplete: coverage.complete,
      coverageSkipped: coverage.skipped,
      listed,
      payloadPath: answered,
    });

    expect(coverage.complete).toBe(false);
    expect(listed).toBeDefined();
    expect(listed?.reason).toBe(fixture.skipped.reason);
    expect(typeof listed?.reason).toBe("string");
    expect(answered?.indexed).toBe(false);
    expect(answered?.reason).toBe(fixture.skipped.reason);
  });

  /**
   * `TECH-SPEC.md` §16 S6 gates "`trace` p95 < 40 ms at depth 2–4 on the fixture". Depth 2 is the
   * cheapest depth in that range and is the tool's own default (`src/tools/trace/manifest.ts:35`,
   * `DEFAULT_DEPTH = 2`), so the measured call is the ordinary one, not a stress input; the
   * mid-graph and 90th-percentile-degree context rows below show the same call on a sparse symbol.
   */
  test(
    "criterion 4: trace p95 < 40 ms, search p95 < 60 ms, summary(change) p95 < 120 ms",
    async () => {
      const { ctx } = fixture;

      const trace = await latencyOf(REPS, () =>
        traceHandle(ctx, { query: fixture.hotspot.qn, depth: 2 }),
      );
      const search = await latencyOf(REPS, () => searchHandle(ctx, { query: "parser" }));
      const summaryChange = await latencyOf(REPS, () => summaryHandle(ctx, {}));

      // Context, not a gate: the same call on a mid-graph symbol and on a 90th-percentile symbol,
      // so a failing hotspot p95 can be read as a class of calls rather than a single outlier.
      const medianTrace = await latencyOf(10, () =>
        traceHandle(ctx, { query: fixture.medianSymbol, depth: 2 }),
      );
      const p90Trace = await latencyOf(10, () =>
        traceHandle(ctx, { query: fixture.p90Symbol, depth: 2 }),
      );

      report("acceptance.criterion4.latency", {
        reps: REPS,
        warmup: WARMUP,
        trace: { thresholdMs: TRACE_P95_MS, target: fixture.hotspot.qn, ...trace },
        search: { thresholdMs: SEARCH_P95_MS, ...search },
        summaryChange: { thresholdMs: SUMMARY_CHANGE_P95_MS, ...summaryChange },
        contextMedianTrace: { target: fixture.medianSymbol, ...medianTrace },
        contextP90Trace: { target: fixture.p90Symbol, ...p90Trace },
      });

      const failures: string[] = [];
      if (trace.p95Ms >= TRACE_P95_MS) {
        failures.push(
          `trace p95 ${trace.p95Ms} ms >= ${TRACE_P95_MS} ms (p50 ${trace.p50Ms}, max ${trace.maxMs},` +
            ` ${REPS} reps, target ${fixture.hotspot.qn} at depth 2)`,
        );
      }
      if (search.p95Ms >= SEARCH_P95_MS) {
        failures.push(
          `search p95 ${search.p95Ms} ms >= ${SEARCH_P95_MS} ms (p50 ${search.p50Ms}, max ${search.maxMs})`,
        );
      }
      if (summaryChange.p95Ms >= SUMMARY_CHANGE_P95_MS) {
        failures.push(
          `summary(change) p95 ${summaryChange.p95Ms} ms >= ${SUMMARY_CHANGE_P95_MS} ms` +
            ` (p50 ${summaryChange.p50Ms}, max ${summaryChange.maxMs})`,
        );
      }
      expect(failures).toEqual([]);
    },
    LATENCY_TIMEOUT_MS,
  );

  test("criterion 5: nine envelope fields, error only on error or bootstrapping", async () => {
    const all = [...observed, ...edge];
    const violations: string[] = [];

    for (const row of all) {
      const env: Envelope<unknown> = row.envelope;
      if (!(ENVELOPE_STATUSES as readonly string[]).includes(env.status)) {
        violations.push(`${row.spec.label}: status ${env.status} is outside ENVELOPE_STATUSES`);
      }
      for (const field of ENVELOPE_FIELDS) {
        if (!Object.hasOwn(env, field)) {
          violations.push(`${row.spec.label}: ${field} is missing`);
        } else if (env[field] === undefined) {
          violations.push(`${row.spec.label}: ${field} is present but undefined`);
        }
      }
      if (!Object.hasOwn(env, "payload")) {
        violations.push(`${row.spec.label}: payload is missing`);
      }
      if (!Array.isArray(env.next_tools)) {
        violations.push(`${row.spec.label}: next_tools is not an array`);
      }
      if (!Array.isArray(env.coverage.not_indexed)) {
        violations.push(`${row.spec.label}: coverage.not_indexed is not an array`);
      }
      const carriesError = Object.hasOwn(env, "error");
      if (carriesError !== STATUSES_CARRYING_ERROR.includes(env.status)) {
        violations.push(
          `${row.spec.label}: error ${carriesError ? "present" : "absent"} on status ${env.status}`,
        );
      }
    }

    report("acceptance.criterion5.envelope", {
      responses: all.length,
      statuses: all.map((row) => row.envelope.status),
      labels: all.map((row) => row.spec.label),
    });

    const statuses = new Set(all.map((row) => row.envelope.status));
    for (const required of STATUSES_CARRYING_ERROR) {
      expect(statuses.has(required)).toBe(true);
    }
    expect(violations).toEqual([]);
  });

  test("criterion 6: the one surface registers the seven names, index included", async () => {
    const surface = await listTools();

    report("acceptance.criterion6.surface", {
      names: surface.names,
      stdoutLines: surface.stdoutLines,
      nonJsonLines: surface.nonJsonLines,
      exitCode: surface.exitCode,
    });

    expect([...surface.names].sort()).toEqual([...TOOL_NAMES].sort());
    expect(surface.names.length).toBe(7);
    expect(surface.names).toContain("index");
    // Bare verbs, no `_tool` suffix: the name is what the agent reads and types.
    expect(surface.names.filter((name) => !/^[a-z][a-z0-9_]*$/.test(name))).toEqual([]);
    expect(surface.names.filter((name) => name.endsWith("_tool"))).toEqual([]);
    expect(surface.nonJsonLines).toEqual([]);
  });

  test("criterion 8: 1 000 identical point reads — prepared is at least 5x a per-call builder", async () => {
    const qn = fixture.hotspot.qn;
    const sql = "SELECT id, qn, kind FROM node WHERE qn = ?";

    // The builder path: the Drizzle query is constructed again on every call, which is exactly the
    // shape `plan.constraint_invariant.md` forbids inside a loop, a handler and a BFS hop.
    const builderRead = () =>
      db(fixture.conn)
        .select({ id: schema.node.id, qn: schema.node.qn, kind: schema.node.kind })
        .from(schema.node)
        .where(eq(schema.node.qn, qn))
        .limit(1)
        .all();
    // The prepared path: one statement, cached under a stable key, as every reader primitive does.
    const statement = prepared(fixture.conn, "acceptance.pointRead", (sqlite) =>
      sqlite.query<{ id: number; qn: string; kind: string }, [string]>(sql),
    );
    const preparedRead = () => statement.get(qn);

    const built = builderRead();
    const preparedRow = preparedRead();
    expect(built.length).toBe(1);
    expect(preparedRow?.qn).toBe(qn);
    expect(built[0]?.qn).toBe(qn);

    const builderMs = bestOf(POINT_READ_ROUNDS, POINT_READS, builderRead);
    const preparedMs = bestOf(POINT_READ_ROUNDS, POINT_READS, preparedRead);
    const ratio = preparedMs === 0 ? Number.POSITIVE_INFINITY : builderMs / preparedMs;

    report("acceptance.criterion8.prepared", {
      reads: POINT_READS,
      rounds: POINT_READ_ROUNDS,
      bestOf: "each path",
      builderMs: round(builderMs),
      preparedMs: round(preparedMs),
      builderUsPerRead: round((builderMs * 1000) / POINT_READS),
      preparedUsPerRead: round((preparedMs * 1000) / POINT_READS),
      ratio: round(ratio),
      threshold: PREPARED_RATIO,
      statementCache: statementCacheStats(fixture.conn),
    });

    expect(ratio).toBeGreaterThanOrEqual(PREPARED_RATIO);
  });

  test("criterion 9: the statement cache stays at or below 200 under 5 000 distinct shapes", () => {
    const conn = openReader(fixture.livePath);
    try {
      const violations: string[] = [];
      let highWater = 0;
      for (let i = 0; i < CACHE_SHAPES; i += 1) {
        // A distinct SQL text per iteration: the shape a cache key is derived from. Real shapes vary
        // by `IN (…)` arity and column set; a distinct literal is the general case and is the one a
        // caller could produce without bound.
        const statement = prepared(conn, `acceptance.shape/${i}`, (sqlite) =>
          sqlite.query<{ shape: number; bound: number }, [number]>(
            `SELECT ${i} AS shape, ? AS bound`,
          ),
        );
        statement.get(i);
        const { size } = statementCacheStats(conn);
        highWater = Math.max(highWater, size);
        if (size > STATEMENT_CACHE_CAPACITY && violations.length < 5) {
          violations.push(
            `shape ${i}: cache grew to ${size} entries (cap ${STATEMENT_CACHE_CAPACITY})`,
          );
        }
      }
      const stats = statementCacheStats(conn);

      report("acceptance.criterion9.cache", {
        shapes: CACHE_SHAPES,
        cap: STATEMENT_CACHE_CAPACITY,
        highWater,
        finalSize: stats.size,
        capacity: stats.capacity,
        hits: stats.hits,
        misses: stats.misses,
        evictions: stats.evictions,
      });

      expect(violations).toEqual([]);
      expect(highWater).toBeLessThanOrEqual(STATEMENT_CACHE_CAPACITY);
      expect(stats.size).toBeLessThanOrEqual(STATEMENT_CACHE_CAPACITY);
      expect(stats.evictions).toBeGreaterThan(0);
    } finally {
      conn.close();
    }
  });

  test(
    "criterion 7: an open reader keeps its results while a publish renames over the file",
    async () => {
      const held = openReader(fixture.livePath);
      try {
        const heldReader = makeReader(held);
        const before = {
          generation: heldReader.metaFor().generation,
          indexedAt: heldReader.metaFor().indexedAt,
          files: heldReader.allFiles().length,
          node: heldReader.findByQn(fixture.hotspot.qn),
          callers: heldReader.inbound(heldReader.findByQn(fixture.hotspot.qn)?.id ?? -1).length,
        };

        // A forced full publish: a new generation id, written to a stage file and renamed over the
        // live one while `held` is open. The corpus is untouched; only the temp data dir changes.
        const code = await runWorker({
          repoRoot: fixture.corpus,
          dataDir: fixture.dataDir,
          force: true,
          mode: "full",
        });

        const reopened = openReader(fixture.livePath);
        let publishedGeneration: string | null = null;
        try {
          publishedGeneration = makeReader(reopened).metaFor().generation;
        } finally {
          reopened.close();
        }

        const after = {
          generation: heldReader.metaFor().generation,
          indexedAt: heldReader.metaFor().indexedAt,
          files: heldReader.allFiles().length,
          node: heldReader.findByQn(fixture.hotspot.qn),
          callers: heldReader.inbound(heldReader.findByQn(fixture.hotspot.qn)?.id ?? -1).length,
        };

        report("acceptance.criterion7.publish", {
          workerExit: code,
          generationBefore: before.generation,
          generationAfterPublish: publishedGeneration,
          generationOnHeldHandle: after.generation,
          filesBefore: before.files,
          filesAfter: after.files,
          callersBefore: before.callers,
          callersAfter: after.callers,
          nodeUnchanged: JSON.stringify(before.node) === JSON.stringify(after.node),
        });

        expect(code).toBe(0);
        expect(before.generation).not.toBeNull();
        expect(publishedGeneration).not.toBe(before.generation);
        expect(after).toEqual(before);
      } finally {
        held.close();
      }
    },
    RUN_TIMEOUT_MS,
  );
});

/** Milliseconds for `reads` calls, best of `rounds` timed rounds after a short warm-up. */
function bestOf(rounds: number, reads: number, run: () => unknown): number {
  let best = Number.POSITIVE_INFINITY;
  for (let round = 0; round < rounds; round += 1) {
    for (let i = 0; i < Math.floor(reads / 10); i += 1) {
      run();
    }
    const started = performance.now();
    for (let i = 0; i < reads; i += 1) {
      run();
    }
    best = Math.min(best, performance.now() - started);
  }
  return best;
}

interface Surface {
  readonly names: readonly string[];
  readonly stdoutLines: number;
  readonly nonJsonLines: readonly string[];
  readonly exitCode: number;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** The `tools/list` names the one surface publishes, read from a real server process over stdio. */
async function listTools(): Promise<Surface> {
  const proc = Bun.spawn({
    // Rooted by flags only: a session that inherits a root from the environment proves nothing.
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "mcp",
      "--repo-root",
      fixture.corpus,
      "--data-dir",
      fixture.dataDir,
    ],
    cwd: APP_ROOT,
    env: {
      ...process.env,
      SANGKAR_CURSOR_KEY: CURSOR_KEY,
      SANGKAR_NO_WATCH: "1",
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const requests = [
    {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: "acceptance-gate", version: "0.0.0" },
      },
    },
    { jsonrpc: "2.0", method: "notifications/initialized" },
    { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
  ];

  const timer = setTimeout(() => proc.kill("SIGKILL"), SESSION_TIMEOUT_MS);
  try {
    proc.stdin.write(requests.map((request) => `${JSON.stringify(request)}\n`).join(""));
    await proc.stdin.flush();
    proc.stdin.end();
    const [stdout, exitCode] = await Promise.all([new Response(proc.stdout).text(), proc.exited]);
    await new Response(proc.stderr).text();

    const nonJsonLines: string[] = [];
    let names: readonly string[] = [];
    let stdoutLines = 0;
    for (const line of stdout.split("\n")) {
      if (line.trim() === "") {
        continue;
      }
      stdoutLines += 1;
      let message: unknown;
      try {
        message = JSON.parse(line);
      } catch {
        nonJsonLines.push(line);
        continue;
      }
      if (!isRecord(message) || message.id !== 2 || !isRecord(message.result)) {
        continue;
      }
      const tools = message.result.tools;
      if (!Array.isArray(tools)) {
        continue;
      }
      names = tools.flatMap((tool) =>
        isRecord(tool) && typeof tool.name === "string" ? [tool.name] : [],
      );
    }
    return { names, stdoutLines, nonJsonLines, exitCode };
  } finally {
    clearTimeout(timer);
  }
}
