import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { extname, join, resolve } from "node:path";
import type { DiscoveredFile } from "../src/indexer/discover";
import { discover } from "../src/indexer/discover";
import { detectLanguage } from "../src/indexer/lang/registry";
import type { ParseSuccess } from "../src/indexer/parse";
import { parseFile } from "../src/indexer/parse";
import { runWorker } from "../src/indexer/worker";

/**
 * Phase 1 closing gate: does this indexer reproduce the reference server's graph on the reference
 * corpus, and does it do so inside the parser budget?
 *
 * The oracle is `tool-outputs/list_graph_stats_tool.json`, which describes CRG's **40-language**
 * graph. This indexer covers 12 languages, excludes non-code files and publishes resolved edges
 * only, so most of CRG's totals are not the same quantity. Every assertion here therefore compares
 * only the parts that mean the same thing in both graphs, and each of those carries the mapping that
 * makes it comparable:
 *
 * - **`Function` is not a kind on both sides.** CRG has no `Method`, so CRG `Function` is compared
 *   against our `function UNION method`.
 * - **Our `test` kind is name-derived.** A declaration whose *name* matches `TEST_NAME` becomes
 *   `test` whatever its role, so the python `Test*` classes CRG files under `Class` are in our
 *   `test` count. That is why `Class 877` is recorded and not asserted.
 * - **CRG counts unresolved calls as edges.** We publish resolved edges only and keep the unbound
 *   call sites in `unresolved`, so the like-for-like pair is (our `calls`) against (CRG `CALLS`
 *   direct) *and* (our `unresolved`) against (CRG `CALLS` unresolved).
 * - **File counts are per language label.** CBM's census has one TypeScript label; `registry.ts`
 *   splits `.ts` from `.tsx` because each needs its own grammar, so the census number is the union.
 *
 * Four further edge kinds are counted under a definition this indexer does not share. They are
 * recorded as measured facts on the stderr line and asserted only to be non-zero, never compared
 * against CRG: see the block below the parity tests.
 *
 * Every measured number goes out on one stderr JSON line, because `bun test` shows nothing for a
 * passing assertion and the progress log has to be able to quote real output.
 */

const CORPUS = resolve(import.meta.dir, "..", "..", "code-review-graph");

/** One full index plus one serial parse pass over 458 files takes ~15 s; the default 5 s would flake. */
const RUN_TIMEOUT_MS = 300_000;

/** How often the RSS sampler reads `process.memoryUsage()`. */
const RSS_SAMPLE_MS = 20;

/** A megabyte, as `ps` reports one. */
const MEGABYTE = 1024 * 1024;

/** `TECH-SPEC.md` §9.1: p50 ≤ 4 ms, p95 ≤ 25 ms per file, over at least this many files. */
const PARSE_BUDGET = { p50Ms: 4, p95Ms: 25, minFiles: 200 } as const;

/** `TECH-SPEC.md` §9.1: peak RSS ≤ 1 GB. */
const RSS_CEILING_MB = 1024;

/** One file may differ on a census too small for a percentage to say anything. */
const SMALL_CENSUS = 10;

/**
 * `list_graph_stats_tool.json`, CRG 2.4.0 on `code-review-graph@bbdda4b`, plus CBM's
 * `get_architecture` census for the per-language file counts. `callsUnresolved` is CRG's 44 338
 * unresolved CALLS, which it counts as edges and we count as `unresolved` rows.
 */
const ORACLE = {
  nodes: 13_955,
  edges: 130_044,
  callsDirect: 27_782,
  callsUnresolved: 44_338,
  contains: 13_403,
  testNodes: 6_351,
  functionNodes: 5_197,
  /** Recorded, not asserted: CRG's 530 files span 40 languages and 1 000 non-code nodes. */
  files: 530,
  classNodes: 877,
  /** Recorded, not asserted: CRG counts each of these under a definition we do not share. */
  imports: 4_511,
  inherits: 127,
  implements: 1,
  references: 2_654,
  testedBy: 37_209,
  filesByLanguage: { python: 408, typescript: 28, java: 4, c: 4, cpp: 3, rust: 3, go: 2 },
} as const;

/**
 * Census label → our registry labels. CBM's census has one TypeScript label while `registry.ts`
 * splits `.ts/.mts/.cts` (`typescript`) from `.tsx` (`tsx`) because each needs its own grammar, so
 * the census number for TypeScript is the union of the two.
 *
 * The census is also one file short of ours for cpp: `.hh` maps to `cpp` in `registry.ts` and CBM
 * does not claim it. That is the whole reason a census under 10 files allows a one-file gap —
 * python (408) and typescript (28) are compared exactly.
 */
const CENSUS_LANGUAGES = {
  python: ["python"],
  typescript: ["typescript", "tsx"],
  java: ["java"],
  c: ["c"],
  cpp: ["cpp"],
  rust: ["rust"],
  go: ["go"],
} as const satisfies Record<string, readonly string[]>;

/** The MVP language set's extensions for the one kind of file the census has no entry for. */
const OUTSIDE_MVP = {
  yaml: [".yaml", ".yml"],
  bash: [".sh", ".bash"],
  r: [".r"],
} as const;

type CensusLabel = keyof typeof CENSUS_LANGUAGES;

interface Ledger {
  readonly exitCode: number;
  readonly nodes: number;
  readonly nodeKinds: Readonly<Record<string, number>>;
  readonly edges: number;
  readonly edgeKinds: Readonly<Record<string, number>>;
  readonly callsByTier: Readonly<Record<string, number>>;
  readonly unresolved: number;
  readonly unresolvedByEvidence: Readonly<Record<string, number>>;
  readonly files: number;
  readonly filesByLanguage: Readonly<Record<string, number>>;
  readonly unsupportedExt: readonly string[];
  readonly parsedFiles: number;
  readonly p50Ms: number;
  readonly p95Ms: number;
  readonly filesWithError: number;
  readonly peakRssMb: number;
}

const dirs: string[] = [];
let ledger: Ledger;

const NODE_KINDS = "SELECT kind AS key, count(*) AS rows FROM node GROUP BY kind";
const EDGE_KINDS = "SELECT kind AS key, count(*) AS rows FROM edge GROUP BY kind";
const CALLS_BY_TIER =
  "SELECT tier AS key, count(*) AS rows FROM edge WHERE kind = 'calls' GROUP BY tier";
const UNRESOLVED_BY_EVIDENCE =
  "SELECT coalesce(evidence_kind, 'none') AS key, count(*) AS rows FROM unresolved" +
  " GROUP BY evidence_kind";
const FILE_EXTENSIONS_UNSUPPORTED =
  "SELECT path FROM not_indexed WHERE reason = 'unsupported_ext' ORDER BY path";
const LANGUAGE_TOTALS = "SELECT language AS key, count(*) AS rows FROM file GROUP BY language";

afterAll(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

beforeAll(async () => {
  // The corpus is a read-only input: every byte the worker writes belongs in the temp data dir.
  const dataDir = await mkdtemp(join(tmpdir(), "sangkar-parity-"));
  dirs.push(dataDir);
  const livePath = join(dataDir, "graph.db");

  const discovered = await discover(CORPUS);
  const sampler = startRssSampler();

  const exitCode = await runWorker({ repoRoot: CORPUS, dataDir, force: true, mode: "full" });
  const parses = await measureParses(CORPUS, discovered.files);
  const peakRssMb = sampler.stop() / MEGABYTE;

  ledger = {
    exitCode,
    nodes: single(query<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM node")),
    nodeKinds: tally(query<{ key: string; rows: number }>(livePath, NODE_KINDS)),
    edges: single(query<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM edge")),
    edgeKinds: tally(query<{ key: string; rows: number }>(livePath, EDGE_KINDS)),
    callsByTier: tally(query<{ key: string; rows: number }>(livePath, CALLS_BY_TIER)),
    unresolved: single(
      query<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM unresolved"),
    ),
    unresolvedByEvidence: tally(
      query<{ key: string; rows: number }>(livePath, UNRESOLVED_BY_EVIDENCE),
    ),
    files: single(query<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM file")),
    filesByLanguage: tally(query<{ key: string; rows: number }>(livePath, LANGUAGE_TOTALS)),
    unsupportedExt: query<{ path: string }>(livePath, FILE_EXTENSIONS_UNSUPPORTED).map(
      (row) => row.path,
    ),
    parsedFiles: parses.files,
    p50Ms: parses.p50Ms,
    p95Ms: parses.p95Ms,
    filesWithError: parses.filesWithError,
    peakRssMb,
  };

  // The one line the progress log quotes: every number this file measured, on stderr, as JSON.
  process.stderr.write(
    `${JSON.stringify({
      ts: new Date().toISOString(),
      level: "info",
      msg: "phase1.parity.measured",
      exitCode: ledger.exitCode,
      corpus: "code-review-graph",
      oracle: ORACLE,
      measured: {
        nodes: ledger.nodes,
        nodeKinds: ledger.nodeKinds,
        edges: ledger.edges,
        edgeKinds: ledger.edgeKinds,
        callsByTier: ledger.callsByTier,
        unresolved: ledger.unresolved,
        unresolvedByEvidence: ledger.unresolvedByEvidence,
        files: ledger.files,
        filesByLanguage: ledger.filesByLanguage,
        censusFiles: censusFileCounts(ledger.filesByLanguage),
        unsupportedExtensions: ledger.unsupportedExt.length,
        parsedFiles: ledger.parsedFiles,
        p50Ms: round(ledger.p50Ms),
        p95Ms: round(ledger.p95Ms),
        filesWithError: ledger.filesWithError,
        peakRssMb: Math.round(ledger.peakRssMb),
      },
    })}\n`,
  );
}, RUN_TIMEOUT_MS);

describe("parser parity against code-review-graph", () => {
  test("the corpus indexes into a temp data dir and publishes a graph", () => {
    expect({
      exitCode: ledger.exitCode,
      empty: ledger.nodes === 0 || ledger.edges === 0 || ledger.files === 0,
    }).toEqual({ exitCode: 0, empty: false });
  });

  test("the node total is within 5 % of CRG's 13 955 nodes", () => {
    expectBands([band("nodes", ledger.nodes, ORACLE.nodes, 0.05)]);
  });

  test("calls edges are within 15 % of CRG's 27 782 direct CALLS", () => {
    // Extracted and inferred are both resolutions; `extracted_candidate` never reaches the edge
    // table. CRG's 44 338 unresolved calls are not edges on our side, and are asserted as
    // `unresolved` below.
    expectBands([band("calls", calls(ledger), ORACLE.callsDirect, 0.15)]);
  });

  test("contains edges are within 5 % of CRG's 13 403 CONTAINS", () => {
    expectBands([band("contains", ledger.edgeKinds.contains ?? 0, ORACLE.contains, 0.05)]);
  });

  test("unresolved rows are within 15 % of CRG's 44 338 unresolved CALLS", () => {
    // The like-for-like counterpart of CRG counting an unresolved call as an edge.
    expectBands([band("unresolved", ledger.unresolved, ORACLE.callsUnresolved, 0.15)]);
  });

  test("test and function UNION method are within 15 % of Test 6 351 and Function 5 197", () => {
    // CRG has no Method kind: its Function covers both of ours, which is the one kind mapping here.
    // Our `test` kind is name-derived (TEST_NAME on the declaration name), so the python `Test*`
    // classes CRG files under Class are inside this count — that is why Class 877 is not asserted.
    expectBands([
      band("kind test", ledger.nodeKinds.test ?? 0, ORACLE.testNodes, 0.15),
      band("kind functionUnionMethod", functionNodes(ledger), ORACLE.functionNodes, 0.15),
    ]);
  });

  test("per-language file counts are exact at 10 or more files and within one file below that", () => {
    const measured = censusFileCounts(ledger.filesByLanguage);
    expectBands(
      (Object.keys(ORACLE.filesByLanguage) as CensusLabel[]).map((language) => {
        const census = ORACLE.filesByLanguage[language];
        // Small bases cannot carry a percentage: one file is 33 % of three. `.hh` is the one-file
        // gap, because it maps to cpp here and CBM does not claim it. The `percent` argument is
        // unused once `files` is given.
        return census >= SMALL_CENSUS
          ? band(`files ${language}`, measured[language] ?? 0, census, 0)
          : band(`files ${language}`, measured[language] ?? 0, census, 0, 1);
      }),
    );
  });

  test("yaml, bash and r files are in not_indexed with reason unsupported_ext", () => {
    const extensions = ledger.unsupportedExt.map((path) => extname(path).toLowerCase());
    const present = Object.fromEntries(
      Object.entries(OUTSIDE_MVP).map(([label, wanted]) => [
        label,
        extensions.some((extension) => wanted.some((ext) => ext === extension)),
      ]),
    );
    const absent = { yaml: true, bash: true, r: true };
    expect(present).toEqual(absent);

    // A supported language reaching this reason would be a discovery defect, not a census fact.
    expect(ledger.unsupportedExt.filter((path) => detectLanguage(path) !== null)).toEqual([]);
  });

  test("at most 1 % of the indexed files contain an ERROR node", () => {
    const share = ledger.filesWithError / ledger.parsedFiles;
    expect({
      filesWithError: ledger.filesWithError,
      parsedFiles: ledger.parsedFiles,
      atMostOnePercent: share <= 0.01,
    }).toEqual({
      filesWithError: ledger.filesWithError,
      parsedFiles: ledger.parsedFiles,
      atMostOnePercent: true,
    });
  });

  test("p50 parse is at most 4 ms and p95 at most 25 ms per file over at least 200 files", () => {
    expect({
      files: ledger.parsedFiles,
      p50Ms: round(ledger.p50Ms),
      p95Ms: round(ledger.p95Ms),
      withinBudget:
        ledger.parsedFiles >= PARSE_BUDGET.minFiles &&
        ledger.p50Ms <= PARSE_BUDGET.p50Ms &&
        ledger.p95Ms <= PARSE_BUDGET.p95Ms,
    }).toEqual({
      files: ledger.parsedFiles,
      p50Ms: round(ledger.p50Ms),
      p95Ms: round(ledger.p95Ms),
      withinBudget: true,
    });
  });

  test("peak RSS stayed under 1 GB", () => {
    expect({
      peakRssMb: Math.round(ledger.peakRssMb),
      underCeiling: ledger.peakRssMb <= RSS_CEILING_MB,
    }).toEqual({ peakRssMb: Math.round(ledger.peakRssMb), underCeiling: true });
  });

  test("the four edge kinds CRG counts under a different definition are present but not compared", () => {
    // Asserted only to be non-zero: a definitional difference recorded as a fact is coverage, while
    // comparing them against CRG would be comparing two different quantities.
    //
    // `imports` — CRG emits one IMPORTS_FROM edge per imported symbol; we emit one per resolved
    // module, bound to the target file node (measured 2 311 against 4 511).
    // `inherits`+`implements` — CRG counts a base that resolves to no indexed node; we never
    // publish an edge whose target row does not exist (measured 42 + 6 against 127 + 1).
    // `references` — CRG's reference set is narrower than our six value-position sites
    // (measured 6 538 against 2 654).
    // `tested_by` — CRG emits one per test/code pair across 40 languages; we emit one per direct
    // test-to-symbol call, and the direction is deliberately inverted (measured 9 912 against
    // 37 209).
    const recorded: Readonly<Record<string, number>> = {
      imports: ledger.edgeKinds.imports ?? 0,
      inheritsAndImplements: (ledger.edgeKinds.inherits ?? 0) + (ledger.edgeKinds.implements ?? 0),
      references: ledger.edgeKinds.references ?? 0,
      testedBy: ledger.edgeKinds.tested_by ?? 0,
    };
    expect(
      Object.entries(recorded)
        .filter(([, count]) => count <= 0)
        .map(([kind]) => kind),
    ).toEqual([]);
  });
});

/**
 * One oracle comparison: a relative window, or an absolute count of files when the base is too
 * small for a percentage to mean anything. A zero `percent` is an exact match.
 */
interface Band {
  readonly criterion: string;
  readonly measured: number;
  readonly oracle: number;
  readonly percent: number;
  /** Absent for a relative band; a number of files is an absolute one, and `percent` is then 0. */
  readonly files?: number;
}

function band(
  criterion: string,
  measured: number,
  oracle: number,
  percent: number,
  files?: number,
): Band {
  return files === undefined
    ? { criterion, measured, oracle, percent }
    : { criterion, measured, oracle, percent, files };
}

/**
 * Asserts that no band was missed. The subject of the assertion is the violation list, so a failure
 * prints each criterion with the number measured, the oracle it missed and the window it had — a
 * bare `toBeLessThan` on a ratio would report the ratio alone and hide all three.
 */
function expectBands(bands: readonly Band[]): void {
  const violations = bands
    .filter((comparison) => !inside(comparison))
    .map((comparison) => ({
      criterion: comparison.criterion,
      measured: comparison.measured,
      oracle: comparison.oracle,
      deviation: round(Math.abs(comparison.measured - comparison.oracle) / comparison.oracle),
      window:
        comparison.files === undefined
          ? `${round(comparison.percent * 100)} %`
          : `+-${comparison.files} file(s)`,
    }));
  expect(violations).toEqual([]);
}

function inside(comparison: Band): boolean {
  const gap = Math.abs(comparison.measured - comparison.oracle);
  return comparison.files === undefined
    ? gap / comparison.oracle <= comparison.percent
    : gap <= comparison.files;
}

/** Resolved calls, i.e. every tier the edge table can hold; see the calls test. */
function calls(current: Ledger): number {
  return (current.callsByTier.extracted ?? 0) + (current.callsByTier.inferred ?? 0);
}

/** CRG `Function` carries methods too; this is the one kind mapping the parity basis allows. */
function functionNodes(current: Ledger): number {
  return (current.nodeKinds.function ?? 0) + (current.nodeKinds.method ?? 0);
}

/** Per-language file counts under the census's labels; see {@link CENSUS_LANGUAGES}. */
function censusFileCounts(
  filesByLanguage: Readonly<Record<string, number>>,
): Record<string, number> {
  const out: Record<string, number> = {};
  for (const [label, languages] of Object.entries(CENSUS_LANGUAGES)) {
    out[label] = languages.reduce((total, language) => total + (filesByLanguage[language] ?? 0), 0);
  }
  return out;
}

function round(value: number): number {
  return Number(value.toFixed(4));
}

/** One scalar `count(*)` read: an empty result set is a schema fact, so it reads as 0. */
function single(rows: readonly { rows: number }[]): number {
  return rows[0]?.rows ?? 0;
}

function tally(rows: readonly { key: string; rows: number }[]): Record<string, number> {
  const out: Record<string, number> = {};
  for (const row of rows) {
    out[row.key] = row.rows;
  }
  return out;
}

/**
 * Reads the published graph. `readonly: true` is the contract readers run under, so a query in this
 * file cannot become a write that changes what the following assertions see.
 */
function query<T>(livePath: string, sql: string): T[] {
  const handle = new Database(livePath, { readonly: true });
  try {
    return handle.query<T, []>(sql).all();
  } finally {
    handle.close();
  }
}

interface ParseMeasurement {
  readonly files: number;
  readonly p50Ms: number;
  readonly p95Ms: number;
  readonly filesWithError: number;
}

/**
 * Parses every discovered file once, timing only the parser: the source is read before the clock
 * starts, so file I/O never lands in the p50/p95 the budget names.
 */
async function measureParses(
  corpus: string,
  files: readonly DiscoveredFile[],
): Promise<ParseMeasurement> {
  const durations: number[] = [];
  let filesWithError = 0;

  for (const file of files) {
    const source = await readFile(join(corpus, file.path), "utf8");
    const started = performance.now();
    const result = await parseFile(file.path, source, file.lang);
    durations.push(performance.now() - started);

    if (!result.ok) {
      continue;
    }
    if (hasErrorNode(result.tree)) {
      filesWithError += 1;
    }
    // A tree is wasm memory; one alive at a time keeps the RSS ceiling a fact about the parser.
    result.tree.delete();
  }

  const sorted = [...durations].sort((left, right) => left - right);
  return {
    files: sorted.length,
    p50Ms: percentile(sorted, 0.5),
    p95Ms: percentile(sorted, 0.95),
    filesWithError,
  };
}

/**
 * A node the grammar could not reduce. `isMissing` is excluded on purpose: a missing token is a
 * recovery the extractor tolerates, while `ERROR` is a subtree it could not read at all.
 */
function hasErrorNode(tree: ParseSuccess["tree"]): boolean {
  const cursor = tree.walk();
  try {
    for (;;) {
      if (cursor.currentNode.isError) {
        return true;
      }
      if (cursor.gotoFirstChild()) {
        continue;
      }
      for (;;) {
        if (cursor.gotoNextSibling()) {
          break;
        }
        if (!cursor.gotoParent()) {
          return false;
        }
      }
    }
  } finally {
    cursor.delete();
  }
}

/** Nearest-rank percentile: no interpolation, so every reported millisecond is one measured parse. */
function percentile(sorted: readonly number[], p: number): number {
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(p * sorted.length) - 1));
  return sorted[index] ?? 0;
}

/**
 * Peak RSS, sampled every 20 ms across the whole subject: the index run and the parse pass, in this
 * process. It is therefore an upper bound that includes the test harness — 390 MB measured against a
 * 1 GB ceiling, which leaves the margin the ceiling is protecting.
 */
function startRssSampler(): { stop: () => number } {
  let peak = process.memoryUsage().rss;
  const timer = setInterval(() => {
    peak = Math.max(peak, process.memoryUsage().rss);
  }, RSS_SAMPLE_MS);
  timer.unref();
  return {
    stop: () => {
      clearInterval(timer);
      peak = Math.max(peak, process.memoryUsage().rss);
      return peak;
    },
  };
}
