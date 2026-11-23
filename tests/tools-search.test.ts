import { describe, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BYTES_PER_TOKEN } from "#shared/budget";
import { type AppConfig, loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import type { Envelope } from "#shared/envelope";
import { log } from "#shared/log";
import { isUnderPrefix } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type { IndexedFileRow } from "#shared/store/read/coverage";
import type {
  FtsHit,
  GraphReader,
  LineRange,
  NodeMetrics,
  NodeRow,
} from "#shared/store/read/reader";
import { handle, type SearchArgs, type SearchPayload } from "../src/tools/search/handler";
import { input, manifest, SEARCH_KINDS } from "../src/tools/search/manifest";

/**
 * `search` against a hand-written `GraphReader` (TOOLS-SPEC.md §5, TECH-SPEC.md §6).
 *
 * The fake returns hits in the order the real reader guarantees — `bm25` ascending, then
 * `in_degree` descending, then `id` ascending — so the assertions can be about what the *tool*
 * does with that order: preserve it, slice it at a cursor position, and never re-sort it. Degree
 * ordering deliberately contradicts score order in the fixture (the highest-degree node is last),
 * because a tool that sorted by degree would still pass a fixture where the two agree.
 *
 * The cursor tests use the real `createCursors()` pair, not a stub: whether page two continues
 * without duplicates depends on the signed position surviving a round trip, which a stub would
 * assert about itself.
 */

const QUERY = "widget";
const GENERATION = "g-search-1";
const INDEXED_AT = "2026-01-05T00:00:00Z";
const KEY = "search-test-key-0123456789abcdef-0123456789";

/** One ranked row: the reader's hit plus the metrics and signature a lookup would return. */
interface Row {
  readonly hit: FtsHit;
  readonly metrics: NodeMetrics | null;
  readonly signature: string | null;
}

function row(
  id: number,
  qn: string,
  kind: string,
  file: string,
  lines: LineRange,
  score: number,
  inDegree: number | null,
  outDegree: number | null,
  signature: string | null,
): Row {
  return {
    hit: { id, qn, kind, file, lines, score },
    // `null` degrees mean "the publish never measured this node", which the tool renders as 0/0.
    metrics:
      inDegree === null || outDegree === null
        ? null
        : {
            nodeId: id,
            inDegree,
            outDegree,
            isEntry: false,
            testedDepth: null,
            reachableCount: null,
          },
    signature,
  };
}

/**
 * Five hits, best first. `src/core/widget.ts::Widget` is the exact name and the best score, which
 * is what "an exact name ranks first" means here: the reader put it first and the tool kept it.
 */
const ROWS: readonly Row[] = [
  row(
    11,
    "src/core/widget.ts::Widget",
    "class",
    "src/core/widget.ts",
    [10, 40],
    -8.21,
    2,
    3,
    "export class Widget",
  ),
  row(
    12,
    "src/core/widget.ts::Widget.size",
    "method",
    "src/core/widget.ts",
    [18, 20],
    -6.4,
    1,
    0,
    "size(): number",
  ),
  row(
    13,
    "tests/widget.test.ts::widget",
    "function",
    "tests/widget.test.ts",
    [1, 12],
    -5.1,
    0,
    0,
    null,
  ),
  row(
    14,
    "src/cli/widget.ts::runWidget",
    "function",
    "src/cli/widget.ts",
    [3, 9],
    -4.02,
    9,
    4,
    "export function runWidget()",
  ),
  row(
    15,
    "src-old/widget.ts::Widget",
    "class",
    "src-old/widget.ts",
    [1, 5],
    -3.3,
    null,
    null,
    "class Widget",
  ),
];

/**
 * One indexed file row. `generation` is a parameter so a fixture can hold a file written by an
 * earlier publish: that is the graph-side drift `read_status.stale` reports.
 */
function indexedFile(id: number, path: string, generation: string = GENERATION): IndexedFileRow {
  return {
    id,
    path,
    language: "typescript",
    sha256: `${id}`.repeat(64).slice(0, 64),
    size: 100 * id,
    lineCount: 40,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: generation,
  };
}

const INDEXED_FILES: readonly IndexedFileRow[] = [
  indexedFile(1, "src/core/widget.ts"),
  indexedFile(2, "src/cli/widget.ts"),
  indexedFile(3, "tests/widget.test.ts"),
  indexedFile(4, "src-old/widget.ts"),
];

function nodeRow(entry: Row | undefined): NodeRow | null {
  if (entry === undefined) {
    return null;
  }
  const { hit } = entry;
  return {
    id: hit.id,
    fileId: 1,
    path: hit.file,
    parentId: null,
    kind: hit.kind,
    subkind: null,
    name: hit.qn.slice(hit.qn.lastIndexOf(":") + 1),
    qn: hit.qn,
    startLine: hit.lines[0],
    endLine: hit.lines[1],
    signature: entry.signature,
    docstring: null,
    isTest: hit.file.startsWith("tests/"),
    props: {},
  };
}

interface FakeOptions {
  readonly rows?: readonly Row[];
  readonly generation?: string | null;
  readonly indexedAt?: string | null;
  /** The generation every indexed file row was written by; older than `generation` means stale. */
  readonly indexGeneration?: string;
}

/** Every `GraphReader` method is present, types only from `#shared/*` — the seam of read-files.test.ts. */
function fakeReader(options: FakeOptions = {}): GraphReader {
  const rows = options.rows ?? ROWS;
  const generation = options.generation === undefined ? GENERATION : options.generation;
  const indexedAt = options.indexedAt === undefined ? INDEXED_AT : options.indexedAt;
  const staleGeneration = options.indexGeneration;
  const files =
    staleGeneration === undefined
      ? INDEXED_FILES
      : INDEXED_FILES.map((file) => ({ ...file, indexedGeneration: staleGeneration }));
  return {
    findByQn: (qn) => nodeRow(rows.find((entry) => entry.hit.qn === qn)),
    findByName: () => [],
    members: () => [],
    nodesByFile: () => [],
    inbound: () => [],
    outbound: () => [],
    metricsFor: (nodeId) => rows.find((entry) => entry.hit.id === nodeId)?.metrics ?? null,
    // `rowsForPaths([])` is empty in the real reader too: a repo-wide scope is not a prefix.
    rowsForPaths: (prefixes) =>
      prefixes.length === 0
        ? { indexed: [], notIndexed: [] }
        : {
            indexed: files.filter((file) =>
              prefixes.some((prefix) => isUnderPrefix(file.path, prefix)),
            ),
            notIndexed: [],
          },
    unresolvedForFiles: () => [],
    fileByPath: () => null,
    allFiles: () => [...files],
    notIndexedAll: () => [],
    notIndexedCount: () => 0,
    testsCensus: () => ({ nodes: 0, languages: [] }),
    censusByLanguage: () => [],
    metaFor: () => ({
      projectName: "fake",
      projectRoot: "/tmp/fake",
      generation,
      indexedAt,
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () => null,
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: (matchQuery, limit) =>
      matchQuery === QUERY ? rows.slice(0, limit).map((entry) => entry.hit) : [],
  };
}

// A home of its own: the machine file would otherwise be whatever the developer's has.
const fakeHome = mkdtempSync(join(tmpdir(), "sangkar-tools-search-home-"));
const fakeConfig: AppConfig = loadConfig({
  argv: ["--repo-root", "/tmp/fake"],
  env: { SANGKAR_HOME: fakeHome, SANGKAR_CURSOR_KEY: "k".repeat(32) },
});

function fakeContext(options: FakeOptions = {}): ToolContext {
  return {
    reader: fakeReader(options),
    cursor: createCursors({ key: KEY }),
    log,
    now: () => new Date("2026-01-05T00:00:00Z"),
    config: fakeConfig,
  };
}

function search(ctx: ToolContext, args: SearchArgs): Promise<Envelope<SearchPayload | null>> {
  return handle(ctx, args);
}

function resultsOf(
  env: Envelope<SearchPayload | null>,
): readonly SearchPayload["results"][number][] {
  return env.payload?.results ?? [];
}

describe("tools/search manifest", () => {
  test("carries the spec description verbatim and no profile field", () => {
    expect(manifest.name).toBe("search");
    expect("profile" in manifest).toBe(false);
    expect(manifest.description).toBe(
      "Find symbols or text in the indexed code. Hybrid keyword (BM25) with optional semantic rerank. " +
        "Returns declarations with file:line and their in/out degree — not source. Use when you know " +
        "roughly what it is called but not where it lives.",
    );
  });

  test("declares six arguments with query as the only required one", () => {
    expect(Object.keys(input)).toEqual(["query", "kind", "path", "budget", "cursor", "repo"]);
    expect(input.query).toEqual({
      type: "string",
      required: true,
      description: "Free text to find: an identifier, a substring or a phrase.",
    });
    expect(input.kind).toEqual({
      type: "enum",
      values: SEARCH_KINDS,
      description: "Return only declarations of this kind, when you already know it.",
    });
    expect(SEARCH_KINDS).toEqual(["function", "method", "class", "file", "decorator"]);
    expect(input.budget).toEqual({
      type: "integer",
      default: 2000,
      description: "Token ceiling for this result; exceeding it truncates and sets `next`.",
    });
    expect(input.path).toEqual({
      type: "string",
      description: "Repo-relative prefix: the path itself and everything under it.",
    });
    expect(input.cursor).toEqual({
      type: "string",
      description: "Continue this same search from the `next` cursor of a previous page.",
    });
  });
});

describe("tools/search ranking", () => {
  test("keeps the reader's order, so an exact name ranks first and degree never re-sorts", async () => {
    const env = await search(fakeContext(), { query: QUERY });
    const results = resultsOf(env);

    expect(env.status).toBe("ok");
    expect(results.map((result) => result.id)).toEqual([11, 12, 13, 14, 15]);
    expect(results[0]?.qn).toBe("src/core/widget.ts::Widget");
    // The node with the highest degree (9/4) stays fourth because its score is worse: score first.
    expect(results[3]?.id).toBe(14);
    expect(results[3]?.degree).toEqual({ in: 9, out: 4 });
    expect(results[0]?.degree).toEqual({ in: 2, out: 3 });
    // An unmeasured node still reports the two documented integers.
    expect(results[4]?.degree).toEqual({ in: 0, out: 0 });
  });

  test("reports bm25 mode and exactly the documented payload keys, never source", async () => {
    const env = await search(fakeContext(), { query: QUERY });
    const payload = env.payload;
    expect(payload?.mode).toBe("bm25");
    expect(Object.keys(payload ?? {})).toEqual(["mode", "results"]);
    const first = resultsOf(env)[0];
    expect(first).toBeDefined();
    expect(Object.keys(first ?? {}).sort()).toEqual(
      ["degree", "file", "id", "kind", "lines", "qn", "score", "signature"].sort(),
    );
    expect(first?.file).toBe("src/core/widget.ts");
    expect(first?.lines).toEqual([10, 40]);
    // A row without a signature omits the key instead of writing `undefined`.
    const unsigned = resultsOf(env)[2];
    expect(unsigned?.id).toBe(13);
    expect(Object.hasOwn(unsigned ?? {}, "signature")).toBe(false);
  });

  test("two identical calls return an identical order and identical payloads", async () => {
    const ctx = fakeContext();
    const first = await search(ctx, { query: QUERY });
    const second = await search(ctx, { query: QUERY });
    expect(first.payload).toEqual(second.payload);
    expect(first.summary).toBe(second.summary);
    expect(resultsOf(first).map((result) => result.id)).toEqual(
      resultsOf(second).map((result) => result.id),
    );
  });

  test("the kind filter keeps the reader's order and drops every other kind", async () => {
    const results = resultsOf(await search(fakeContext(), { query: QUERY, kind: "class" }));
    expect(results.map((result) => result.id)).toEqual([11, 15]);
    expect(results.every((result) => result.kind === "class")).toBe(true);
  });
});

describe("tools/search path filter", () => {
  test("a path prefix excludes everything outside it, including a sibling that shares the text", async () => {
    const env = await search(fakeContext(), { query: QUERY, path: "src" });
    const results = resultsOf(env);

    expect(results.map((result) => result.id)).toEqual([11, 12, 14]);
    expect(results.every((result) => isUnderPrefix(result.file, "src"))).toBe(true);
    // `src-old/widget.ts` shares the first three characters and must still be excluded.
    expect(results.some((result) => result.qn.includes("src-old"))).toBe(false);
    expect(results.some((result) => result.file.startsWith("tests/"))).toBe(false);
    expect(env.totals.results_total).toBe(3);
  });

  test("a fully indexed path scope may claim eq, while a repo-wide scope may not", async () => {
    const scoped = await search(fakeContext(), { query: QUERY, path: "src" });
    expect(scoped.coverage.complete).toBe(true);
    expect(scoped.coverage.indexed).toBe(2);
    expect(scoped.totals.total_relation).toBe("eq");
    // The graph-side check ran over the two files in scope, and it says so.
    expect(scoped.read_status.stale).toBe(false);
    expect(scoped.read_status.stale_checked_at).toBe("2026-01-05T00:00:00.000Z");
    expect(Object.hasOwn(scoped.read_status, "stale_reason")).toBe(false);

    const repoWide = await search(fakeContext(), { query: QUERY });
    expect(repoWide.coverage.complete).toBe(false);
    expect(repoWide.totals.total_relation).toBe("gte");
    // No prefix means no file was compared, so no freshness check may be claimed either.
    expect(Object.hasOwn(repoWide.read_status, "stale_checked_at")).toBe(false);
  });

  test("a file written by an earlier generation makes the answer stale without failing", async () => {
    const env = await search(fakeContext({ indexGeneration: "g-previous" }), {
      query: QUERY,
      path: "src",
    });
    expect(env.status).toBe("ok");
    expect(env.read_status.generation).toBe(GENERATION);
    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toContain("2 of 2 file(s) in scope");
    expect(env.read_status.stale_checked_at).toBe("2026-01-05T00:00:00.000Z");
    // A stale answer may never claim an exact total, and the results are still served.
    expect(env.totals.total_relation).toBe("gte");
    expect(resultsOf(env).length).toBeGreaterThan(0);
  });

  test("a filter that empties a matching query says which one emptied it", async () => {
    const byKind = await search(fakeContext(), { query: QUERY, kind: "decorator" });
    expect(byKind.status).toBe("not_found");
    expect(resultsOf(byKind)).toEqual([]);
    expect(byKind.summary).toContain("though 5 matched the query alone");

    const byPath = await search(fakeContext(), { query: QUERY, path: "docs" });
    expect(byPath.status).toBe("not_found");
    expect(byPath.summary).toContain("No declarations matched 'widget'");
  });

  test("a query that matches nothing is not_found, never a bare zero", async () => {
    const env = await search(fakeContext(), { query: "nomatch" });
    expect(env.status).toBe("not_found");
    expect(resultsOf(env)).toEqual([]);
    expect(env.totals.results_total).toBe(0);
    expect(env.summary).toContain("nomatch");
  });

  test("an empty query is answered without asking the reader for a MATCH", async () => {
    const env = await search(fakeContext(), { query: "   " });
    expect(env.status).toBe("not_found");
    expect(resultsOf(env)).toEqual([]);
    expect(env.summary).toContain("empty query");
  });
});

describe("tools/search pagination", () => {
  /**
   * The tightest budget that still truncates this fixture's answer, found rather than guessed: the
   * whole answer first, then down one token at a time. The fixture's envelope floor sits just under
   * its payload, so a hard-coded 200 would now land *below* the floor — where `search` refuses
   * instead of paging, because a page that cannot be cut to fit is not a page.
   */
  async function tightestCuttingBudget(ctx: ToolContext): Promise<number> {
    const whole = await search(ctx, { query: QUERY });
    const wholeTokens = Math.ceil(
      Buffer.byteLength(JSON.stringify(whole), "utf8") / BYTES_PER_TOKEN,
    );
    for (let budget = wholeTokens - 1; budget > 1; budget -= 1) {
      const env = await search(ctx, { query: QUERY, budget });
      if (env.status === "ok" && env.next !== null) {
        return budget;
      }
    }
    throw new Error("no budget truncates this fixture's answer");
  }

  test("a truncated page's next cursor returns the following page with no duplicates", async () => {
    const ctx = fakeContext();
    const budget = await tightestCuttingBudget(ctx);
    const pages: SearchPayload["results"][number][] = [];
    let cursor: string | undefined;
    let calls = 0;

    for (;;) {
      const args: SearchArgs =
        cursor === undefined ? { query: QUERY, budget } : { query: QUERY, budget, cursor };
      const env = await search(ctx, args);
      calls += 1;
      expect(env.status).toBe("ok");
      pages.push(...resultsOf(env));

      if (env.next === null) {
        expect(env.truncated).toBe(false);
        break;
      }
      // A cut page always says why and always carries a continuation.
      expect(env.truncated).toBe(true);
      expect(env.truncation_reason).toBe("budget");
      expect(env.totals.total_relation).toBe("gte");
      cursor = env.next;
      expect(calls).toBeLessThan(ROWS.length + 1);
    }

    expect(calls).toBeGreaterThan(1);
    const ids = pages.map((page) => page.id);
    expect(ids).toEqual(ROWS.map((entry) => entry.hit.id));
    expect(new Set(ids).size).toBe(ids.length);
  });

  test("a resumed page may use a different budget: the list, not the page size, is the call", async () => {
    const ctx = fakeContext();
    const first = await search(ctx, { query: QUERY, budget: await tightestCuttingBudget(ctx) });
    const next = first.next;
    expect(next).not.toBeNull();
    const firstIds = resultsOf(first).map((result) => result.id);
    expect(firstIds.length).toBeGreaterThan(0);
    expect(firstIds).toEqual(ROWS.slice(0, firstIds.length).map((entry) => entry.hit.id));

    const resumed = await search(ctx, { query: QUERY, budget: 2000, cursor: next ?? "" });
    expect(resumed.status).toBe("ok");
    const resumedIds = resultsOf(resumed).map((result) => result.id);
    expect(resumedIds.length).toBeGreaterThan(0);
    // The resumed page starts exactly where the tight one stopped, whatever budget either used.
    expect(resumedIds[0]).toBe(ROWS[firstIds.length]?.hit.id);
    expect(resumedIds.some((id) => firstIds.includes(id))).toBe(false);
  });
});

describe("tools/search cursors", () => {
  /** A token for this call's shape, with the three parts a refusal is separated by. */
  function token(
    overrides: { generation?: string; tool?: string; argsHash?: string } = {},
  ): string {
    return createCursors({ key: KEY }).sign({
      generation: overrides.generation ?? GENERATION,
      tool: overrides.tool ?? "search",
      argsHash: overrides.argsHash ?? "not-the-hash-of-any-call",
      position: 1,
    });
  }

  test("a cursor for other arguments is refused as cursor_mismatch and returns no page", async () => {
    const env = await search(fakeContext(), { query: QUERY, cursor: token() });
    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("cursor_mismatch");
    expect(env.error?.hint).toBe("re-run the original call with the same tool and arguments");
    expect(env.payload).toBeNull();
    expect(env.next).toBeNull();
  });

  test("a cursor minted in another generation is refused as cursor_expired", async () => {
    const env = await search(fakeContext(), {
      query: QUERY,
      cursor: token({ generation: "g-old" }),
    });
    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("cursor_expired");
    expect(env.error?.hint).toBe("re-run the original call");
  });

  test("a cursor for another tool and a malformed cursor are both cursor_invalid/cursor_mismatch", async () => {
    const otherTool = await search(fakeContext(), {
      query: QUERY,
      cursor: token({ tool: "trace" }),
    });
    expect(otherTool.error?.code).toBe("cursor_mismatch");

    const malformed = await search(fakeContext(), { query: QUERY, cursor: "not-a-cursor" });
    expect(malformed.status).toBe("error");
    expect(malformed.error?.code).toBe("cursor_invalid");
    expect(malformed.error?.hint).toBe("re-run the original call to mint a fresh cursor");
  });

  test("the cursor is verified before any page is read", async () => {
    let reads = 0;
    const base = fakeContext();
    const ctx: ToolContext = {
      ...base,
      reader: {
        ...fakeReader(),
        readNodeFts: (matchQuery, limit) => {
          reads += 1;
          return fakeReader().readNodeFts(matchQuery, limit);
        },
      },
    };
    const env = await search(ctx, { query: QUERY, cursor: token() });
    expect(env.error?.code).toBe("cursor_mismatch");
    expect(reads).toBe(0);
  });
});

describe("tools/search refusals", () => {
  test("an unpublished index answers bootstrapping with a retry interval", async () => {
    const env = await search(fakeContext({ generation: null }), { query: QUERY });
    expect(env.status).toBe("bootstrapping");
    expect(env.payload).toBeNull();
    expect(env.error?.code).toBe("bootstrapping");
    expect(env.error?.retry_after_ms).toBe(1000);
  });

  test("a budget that fits no result refuses instead of paging forever", async () => {
    const env = await search(fakeContext(), { query: QUERY, budget: 1 });
    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("budget_too_large");
    expect(env.next).toBeNull();
  });

  test("a budget above the ceiling is reported, not thrown", async () => {
    const env = await search(fakeContext(), { query: QUERY, budget: 40_000 });
    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("budget_too_large");
    expect(env.error?.hint).toContain("32000");
  });
});
