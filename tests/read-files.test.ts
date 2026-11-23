import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type AppConfig, loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import { log } from "#shared/log";
import { type Conn, db, openReader, openWriter } from "#shared/store/db";
import { migrate } from "#shared/store/migrate";
import type { CursorFacade, ToolContext } from "#shared/store/read/context";
import { allFiles, censusByLanguage, fileByPath, testsCensus } from "#shared/store/read/files";
import { type GraphReader, makeReader } from "#shared/store/read/reader";
import * as schema from "#shared/store/schema";

/**
 * The census, the aggregate reader and the `ToolContext` seam (`DATABASE.md` §3.2, §6, §8).
 *
 * Three properties are asserted against a real database, because all three are about agreement
 * between tables rather than about code shape:
 * - `not_indexed` and `parse_status` produce one census, so a language whose grammar fails is
 *   reported as failed files instead of disappearing;
 * - `readNodeFts` returns best-first on a stable triple `(bm25 asc, in_degree desc, id asc)`,
 *   which is what makes a cursor page boundary inside one generation safe;
 * - `meta`, `generation` and `tombstone` are readable through the reader without a connection;
 * - `testsCensus` counts `is_test = 1` and nothing else, so a graph with no test symbol answers
 *   `{nodes: 0, languages: []}` instead of omitting the field;
 *
 * One more property is a type: a `GraphReader` and a `ToolContext` built by hand from
 * `#shared/*` imports alone must typecheck, because that is the unit-test seam every tool task
 * depends on.
 */

const SRC_A_TYPESCRIPT = 1;
const SRC_B_PYTHON = 2;
const SRC_C_PYTHON = 3;
const SRC_D_PYTHON = 4;

/** Four nodes in one file, all matched by the same query, with degree and depth chosen below. */
const ALPHA = 1;
const BETA = 2;
const GAMMA = 3;
const DELTA = 4;
/** The one symbol in the fixture with `is_test = 1`: the whole `testsCensus` input. */
const TEST_BETA = 5;

const GENERATION_OLD = "g-1";
const GENERATION_NEW = "g-2";

const dirs: string[] = [];
let readerConn: Conn;
let reader: GraphReader;

/**
 * A throwaway graph, seeded and torn down inside one test.
 *
 * The census cases below are about graphs the shared fixture is not — one with no test node at
 * all, one whose counts span two languages — and seeding them here keeps the shared fixture
 * exactly as the FTS and ranked-query assertions expect it.
 */
function withTempGraph(
  seedRows: (handle: ReturnType<typeof db>) => void,
  run: (conn: Conn) => void,
): void {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-read-census-"));
  dirs.push(dir);
  const path = join(dir, "graph.db");
  const writer = openWriter(path);
  try {
    migrate(writer.sqlite);
    seedRows(db(writer));
  } finally {
    writer.close();
  }
  const conn = openReader(path);
  try {
    run(conn);
  } finally {
    conn.close();
  }
}

function seed(writer: Conn): void {
  const handle = db(writer);

  handle
    .insert(schema.file)
    .values([
      {
        id: SRC_A_TYPESCRIPT,
        path: "src/a.ts",
        language: "typescript",
        sha256: "a".repeat(64),
        size: 200,
        lineCount: 8,
        parseStatus: "ok",
        parseErrorRanges: null,
        indexedGeneration: GENERATION_NEW,
      },
      {
        id: SRC_B_PYTHON,
        path: "src/b.py",
        language: "python",
        sha256: "b".repeat(64),
        size: 120,
        lineCount: 12,
        parseStatus: "ok",
        parseErrorRanges: null,
        indexedGeneration: GENERATION_NEW,
      },
      {
        id: SRC_C_PYTHON,
        path: "src/c.py",
        language: "python",
        sha256: "c".repeat(64),
        size: 90,
        lineCount: 7,
        parseStatus: "partial",
        parseErrorRanges: "7-7",
        indexedGeneration: GENERATION_NEW,
      },
      {
        id: SRC_D_PYTHON,
        path: "src/d.py",
        language: "python",
        sha256: "d".repeat(64),
        size: 10,
        lineCount: 1,
        parseStatus: "failed",
        parseErrorRanges: "1-1",
        indexedGeneration: GENERATION_NEW,
      },
    ])
    .run();

  // `src/gen.ts` has a skipped sibling in a language that is otherwise parsed, `legacy.xslt` is a
  // language with no file row at all, and `blob.bin` is a skip whose grammar the indexer could
  // not name at all.
  handle
    .insert(schema.notIndexed)
    .values([
      {
        id: 1,
        path: "src/gen.ts",
        reason: "grammar_unavailable",
        detail: "no grammar for .cts",
        language: "typescript",
      },
      {
        id: 2,
        path: "docs/legacy.xslt",
        reason: "grammar_unavailable",
        detail: null,
        language: "xslt",
      },
      { id: 3, path: "vendor/blob.bin", reason: "binary", detail: null, language: null },
    ])
    .run();

  // Identical token counts in `name`/`qn`/`docstring` for the first three rows, so their bm25
  // scores tie exactly and the tie-break is what the ordering test observes. The fourth has the
  // same term once in a longer document, so its score is strictly worse (bm25 is negative).
  handle
    .insert(schema.node)
    .values([
      {
        id: ALPHA,
        fileId: SRC_A_TYPESCRIPT,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "alpha",
        qn: "src/a.ts::alpha",
        startLine: 1,
        endLine: 2,
        signature: null,
        docstring: "widget",
        isTest: 0,
        props: "{}",
      },
      {
        id: BETA,
        fileId: SRC_A_TYPESCRIPT,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "beta",
        qn: "src/a.ts::beta",
        startLine: 3,
        endLine: 4,
        signature: null,
        docstring: "widget",
        isTest: 0,
        props: "{}",
      },
      {
        id: GAMMA,
        fileId: SRC_A_TYPESCRIPT,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "gamma",
        qn: "src/a.ts::gamma",
        startLine: 5,
        endLine: 6,
        signature: null,
        docstring: "widget",
        isTest: 0,
        props: "{}",
      },
      {
        id: DELTA,
        fileId: SRC_A_TYPESCRIPT,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "delta",
        qn: "src/a.ts::delta",
        startLine: 7,
        endLine: 8,
        signature: null,
        docstring: "widget filler filler filler filler filler",
        isTest: 0,
        props: "{}",
      },
    ])
    .run();

  // Deliberately outside `node_fts` and `node_metrics`: the census reads `node` and `file` only,
  // and the FTS ordering and ranked queries pinned above must not move because of it.
  handle
    .insert(schema.node)
    .values([
      {
        id: TEST_BETA,
        fileId: SRC_B_PYTHON,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "beta",
        qn: "src/b.py::beta",
        startLine: 1,
        endLine: 2,
        signature: null,
        docstring: null,
        isTest: 1,
        props: "{}",
      },
    ])
    .run();

  handle
    .insert(schema.nodeMetrics)
    .values([
      // ALPHA and BETA tie on in_degree, so only `id` can separate them.
      {
        nodeId: ALPHA,
        inDegree: 7,
        outDegree: 2,
        isEntry: 0,
        testedDepth: 0,
        reachableCount: null,
      },
      { nodeId: BETA, inDegree: 7, outDegree: 2, isEntry: 0, testedDepth: 1, reachableCount: null },
      // GAMMA ties on score but has the higher degree, so it must come first.
      {
        nodeId: GAMMA,
        inDegree: 9,
        outDegree: 1,
        isEntry: 1,
        testedDepth: null,
        reachableCount: 12,
      },
      // DELTA has by far the highest degree and still sorts last: score is the primary key.
      {
        nodeId: DELTA,
        inDegree: 99,
        outDegree: 0,
        isEntry: 1,
        testedDepth: 5,
        reachableCount: null,
      },
    ])
    .run();

  handle
    .insert(schema.meta)
    .values([
      { key: "project_name", value: "fixture" },
      { key: "project_root", value: "/tmp/fixture" },
      { key: "generation", value: GENERATION_NEW },
      { key: "indexer_state", value: "idle" },
      { key: "consecutive_failures", value: "3" },
    ])
    .run();

  handle
    .insert(schema.generation)
    .values([
      {
        id: GENERATION_OLD,
        seq: 1,
        publishedAt: "2026-01-01T00:00:00Z",
        baseCommit: "a".repeat(40),
        worktreeDirty: 0,
        filesIndexed: 3,
        nodes: 3,
        edges: 2,
        durationMs: 900,
        mode: "full",
        status: "superseded",
      },
      {
        id: GENERATION_NEW,
        seq: 2,
        publishedAt: "2026-01-02T00:00:00Z",
        baseCommit: "b".repeat(40),
        worktreeDirty: 1,
        filesIndexed: 4,
        nodes: 4,
        edges: 3,
        durationMs: 1_200,
        mode: "full",
        status: "published",
      },
    ])
    .run();

  handle
    .insert(schema.tombstone)
    .values([
      {
        qn: "src/old.ts::gone",
        kind: "function",
        fileId: null,
        lastSeenGeneration: GENERATION_OLD,
        removedInGeneration: GENERATION_NEW,
        hadCallers: 1,
      },
      {
        qn: "src/older.ts::vanished",
        kind: "function",
        fileId: null,
        lastSeenGeneration: "g-0",
        removedInGeneration: GENERATION_OLD,
        hadCallers: 0,
      },
    ])
    .run();

  handle
    .insert(schema.nodeFtsState)
    .values([
      {
        rowid: ALPHA,
        name: "alpha",
        qn: "src/a.ts::alpha",
        signature: null,
        docstring: "widget",
        propsText: "{}",
      },
      {
        rowid: BETA,
        name: "beta",
        qn: "src/a.ts::beta",
        signature: null,
        docstring: "widget",
        propsText: "{}",
      },
      {
        rowid: GAMMA,
        name: "gamma",
        qn: "src/a.ts::gamma",
        signature: null,
        docstring: "widget",
        propsText: "{}",
      },
      {
        rowid: DELTA,
        name: "delta",
        qn: "src/a.ts::delta",
        signature: null,
        docstring: "widget filler filler filler filler filler",
        propsText: "{}",
      },
    ])
    .run();

  // External-content FTS5 keeps no text of its own, so the index is built from the mirror.
  writer.sqlite.exec("INSERT INTO node_fts(node_fts) VALUES('rebuild')");
}

beforeAll(() => {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-read-files-"));
  dirs.push(dir);
  const path = join(dir, "graph.db");
  const writer = openWriter(path);
  try {
    migrate(writer.sqlite);
    seed(writer);
  } finally {
    writer.close();
  }
  readerConn = openReader(path);
  reader = makeReader(readerConn);
});

afterAll(() => {
  readerConn?.close();
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("read/files", () => {
  test("censusByLanguage merges not_indexed into files and failed", () => {
    expect(censusByLanguage(readerConn)).toEqual([
      { language: "python", files: 3, parsed: 2, failed: 1 },
      // `src/a.ts` parsed, `src/gen.ts` was skipped: one language, two paths, one failure.
      { language: "typescript", files: 2, parsed: 1, failed: 1 },
      // No `file` row at all: the census reports the grammar that failed, not an absent language.
      { language: "xslt", files: 1, parsed: 0, failed: 1 },
      // `not_indexed.language` is nullable; an unattributable skip is reported, never dropped.
      { language: null, files: 1, parsed: 0, failed: 1 },
    ]);
  });

  test("files is always parsed + failed, so the arithmetic is checkable", () => {
    for (const row of censusByLanguage(readerConn)) {
      expect(row.files).toBe(row.parsed + row.failed);
    }
  });

  test("a partial parse counts as parsed and keeps its error ranges", () => {
    const row = fileByPath(readerConn, "src/c.py");
    expect(row?.parseStatus).toBe("partial");
    expect(row?.parseErrorRanges).toBe("7-7");
    expect(censusByLanguage(readerConn)[0]).toMatchObject({ parsed: 2, failed: 1 });
  });

  test("fileByPath normalizes the path and returns null for a skipped one", () => {
    expect(fileByPath(readerConn, "./src/a.ts")?.id).toBe(SRC_A_TYPESCRIPT);
    expect(fileByPath(readerConn, "src/gen.ts")).toBeNull();
    expect(fileByPath(readerConn, "src/nowhere.ts")).toBeNull();
  });

  test("allFiles returns the whole manifest in path order", () => {
    expect(allFiles(readerConn).map((row) => row.path)).toEqual([
      "src/a.ts",
      "src/b.py",
      "src/c.py",
      "src/d.py",
    ]);
    expect(allFiles(readerConn).every((row) => row.sha256.length === 64)).toBe(true);
  });

  test("testsCensus counts the test node under its file's language", () => {
    // The fixture's four `src/a.ts` nodes are not tests, so only `src/b.py::beta` is counted and
    // `src/a.ts` contributes no entry at all rather than a zero row.
    expect(testsCensus(readerConn)).toEqual({
      nodes: 1,
      languages: [{ language: "python", nodes: 1 }],
    });
  });

  test("a graph with no test node is an empty census, not a missing one", () => {
    withTempGraph(
      () => {},
      (conn) => {
        expect(testsCensus(conn)).toEqual({ nodes: 0, languages: [] });
      },
    );
  });

  test("testsCensus sums the per-language counts it returns", () => {
    withTempGraph(
      (handle) => {
        handle
          .insert(schema.file)
          .values([
            {
              id: 1,
              path: "src/a.ts",
              language: "typescript",
              sha256: "a".repeat(64),
              size: 10,
              lineCount: 1,
              parseStatus: "ok",
              parseErrorRanges: null,
              indexedGeneration: GENERATION_NEW,
            },
            {
              id: 2,
              path: "src/b.py",
              language: "python",
              sha256: "b".repeat(64),
              size: 10,
              lineCount: 1,
              parseStatus: "ok",
              parseErrorRanges: null,
              indexedGeneration: GENERATION_NEW,
            },
          ])
          .run();
        // Two test nodes in one language and a non-test node beside them: `nodes` must sum the
        // entries (so it cannot disagree with `languages`) and the non-test row must not count.
        const rows = [
          { id: 1, fileId: 1, qn: "src/a.ts::t1", isTest: 1 },
          { id: 2, fileId: 1, qn: "src/a.ts::t2", isTest: 1 },
          { id: 3, fileId: 2, qn: "src/b.py::test_a", isTest: 1 },
          { id: 4, fileId: 2, qn: "src/b.py::helper", isTest: 0 },
        ];
        for (const row of rows) {
          handle
            .insert(schema.node)
            .values({
              id: row.id,
              fileId: row.fileId,
              parentId: null,
              kind: "function",
              subkind: null,
              name: row.qn.split("::")[1] ?? "",
              qn: row.qn,
              startLine: 1,
              endLine: 1,
              signature: null,
              docstring: null,
              isTest: row.isTest,
              props: "{}",
            })
            .run();
        }
      },
      (conn) => {
        const census = testsCensus(conn);
        expect(census.languages).toEqual([
          { language: "typescript", nodes: 2 },
          { language: "python", nodes: 1 },
        ]);
        expect(census.nodes).toBe(3);
        expect(census.nodes).toBe(census.languages.reduce((sum, row) => sum + row.nodes, 0));
      },
    );
  });
});

describe("read/reader", () => {
  test("makeReader binds the connection and exposes the primitives", () => {
    expect(reader.findByQn("src/a.ts::alpha")?.id).toBe(ALPHA);
    expect(reader.findByName("gamma").map((row) => row.id)).toEqual([GAMMA]);
    expect(reader.nodesByFile(SRC_A_TYPESCRIPT).map((row) => row.id)).toEqual([
      ALPHA,
      BETA,
      GAMMA,
      DELTA,
    ]);
    expect(reader.metricsFor(ALPHA)?.inDegree).toBe(7);
    expect(reader.fileByPath("src/b.py")?.language).toBe("python");
    expect(reader.testsCensus()).toEqual({
      nodes: 1,
      languages: [{ language: "python", nodes: 1 }],
    });
    expect(reader.allFiles().map((row) => row.id)).toEqual([
      SRC_A_TYPESCRIPT,
      SRC_B_PYTHON,
      SRC_C_PYTHON,
      SRC_D_PYTHON,
    ]);
  });

  test("metaFor reports identity, generation, publish time and health", () => {
    expect(reader.metaFor()).toEqual({
      projectName: "fixture",
      projectRoot: "/tmp/fixture",
      // The fixture recorded name and root only; an unrecorded branch or remote is null, never "".
      projectBranch: null,
      projectRemote: null,
      generation: GENERATION_NEW,
      indexedAt: "2026-01-02T00:00:00Z",
      indexerState: "idle",
      consecutiveFailures: 3,
    });
  });

  test("latestGeneration is the newest seq, not the newest id", () => {
    const latest = reader.latestGeneration();
    expect(latest?.id).toBe(GENERATION_NEW);
    expect(latest?.seq).toBe(2);
    expect(latest?.worktreeDirty).toBe(true);
    expect(latest?.filesIndexed).toBe(4);
  });

  test("removedSymbolsFor selects tombstones by the generation that removed them", () => {
    const current = reader.removedSymbolsFor(GENERATION_NEW);
    expect(current.map((row) => row.qn)).toEqual(["src/old.ts::gone"]);
    expect(current[0]?.hadCallers).toBe(true);
    expect(reader.removedSymbolsFor(GENERATION_OLD).map((row) => row.qn)).toEqual([
      "src/older.ts::vanished",
    ]);
    expect(reader.removedSymbolsFor("g-404")).toEqual([]);
  });

  test("entryPoints orders reachable descending with the unmeasured last", () => {
    const points = reader.entryPoints(10);
    expect(points.map((row) => row.id)).toEqual([GAMMA, DELTA]);
    expect(points[0]?.reachable).toBe(12);
    expect(points[1]?.reachable).toBeNull();
    expect(points[0]?.lines).toEqual([5, 6]);
  });

  test("hotspots orders by total degree and breaks the tie by id", () => {
    expect(reader.hotspots(4).map((row) => row.id)).toEqual([DELTA, GAMMA, ALPHA, BETA]);
    expect(reader.hotspots(2).map((row) => row.id)).toEqual([DELTA, GAMMA]);
    expect(reader.hotspots(4)[2]).toMatchObject({ inDegree: 7, outDegree: 2 });
  });

  test("testGaps reads tested_depth and keeps NULL distinguishable from a deep depth", () => {
    const gaps = reader.testGaps([ALPHA, BETA, GAMMA, DELTA]);
    expect(gaps.map((row) => row.id)).toEqual([GAMMA, DELTA]);
    // `null` is "no test reachable"; 5 is "reachable, but not within two hops".
    expect(gaps[0]?.testedDepth).toBeNull();
    expect(gaps[1]?.testedDepth).toBe(5);
    expect(reader.testGaps([ALPHA, BETA])).toEqual([]);
    expect(reader.testGaps([])).toEqual([]);
  });

  test("readNodeFts orders by bm25 ascending, then in_degree descending, then id", () => {
    const hits = reader.readNodeFts("widget", 10);
    expect(hits.map((row) => row.id)).toEqual([GAMMA, ALPHA, BETA, DELTA]);

    const scores = hits.map((row) => row.score);
    // The tie-break is the assertion, not the sort: ALPHA and BETA share a score and a degree and
    // are separated only by id, while GAMMA shares their score and sorts ahead of them on degree.
    expect(scores[0]).toBe(scores[1]);
    expect(scores[1]).toBe(scores[2]);
    // DELTA has the same term once in a longer document and by far the highest degree, yet its
    // worse score puts it last: score outranks degree.
    expect(scores[3]).toBeGreaterThan(scores[0] ?? 0);
  });

  test("readNodeFts honours the statement bound and still starts at the best row", () => {
    expect(reader.readNodeFts("widget", 2).map((row) => row.id)).toEqual([GAMMA, ALPHA]);
    expect(reader.readNodeFts("nomatchword", 5)).toEqual([]);
  });

  test("readNodeFts returns the payload fields search reports", () => {
    const first = reader.readNodeFts("widget", 1)[0];
    expect(first?.qn).toBe("src/a.ts::gamma");
    expect(first?.kind).toBe("function");
    expect(first?.file).toBe("src/a.ts");
    expect(first?.lines).toEqual([5, 6]);
  });
});

describe("read/context", () => {
  /** A hand-written reader: every method present, types only from `#shared/*`. */
  const fakeReader: GraphReader = {
    findByQn: (qn) => (qn === "src/a.ts::alpha" ? fakeNode() : null),
    findByName: () => [],
    members: () => [],
    nodesByFile: () => [],
    inbound: () => [],
    outbound: () => [],
    metricsFor: () => null,
    rowsForPaths: () => ({ indexed: [], notIndexed: [] }),
    notIndexedAll: () => [],
    notIndexedCount: () => 0,
    unresolvedForFiles: () => [],
    fileByPath: () => null,
    allFiles: () => [],
    censusByLanguage: () => [{ language: "python", files: 3, parsed: 2, failed: 1 }],
    testsCensus: () => ({ nodes: 0, languages: [] }),
    metaFor: () => ({
      projectName: "fake",
      projectRoot: "/tmp/fake",
      generation: "g-9",
      indexedAt: "2026-01-03T00:00:00Z",
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () => null,
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };

  function fakeNode(): ReturnType<GraphReader["findByQn"]> {
    return {
      id: 1,
      fileId: 1,
      path: "src/a.ts",
      parentId: null,
      kind: "function",
      subkind: null,
      name: "alpha",
      qn: "src/a.ts::alpha",
      startLine: 1,
      endLine: 2,
      signature: null,
      docstring: null,
      isTest: false,
      props: {},
    };
  }

  const cursors = createCursors({ key: "fake-cursor-key-material-32-bytes" });

  // A home of its own: the machine file would otherwise be whatever the developer's has, and a
  // there-read budget or consent switch would change what this fake context answers.
  const fakeHome = mkdtempSync(join(tmpdir(), "sangkar-read-files-home-"));
  const fakeConfig: AppConfig = loadConfig({
    argv: ["--repo-root", "/tmp/fake"],
    env: { SANGKAR_HOME: fakeHome, SANGKAR_CURSOR_KEY: "k".repeat(32) },
  });

  const fakeContext: ToolContext = {
    reader: fakeReader,
    cursor: cursors,
    log,
    now: () => new Date("2026-01-03T00:00:00Z"),
    config: fakeConfig,
  };

  test("a hand-written fake typechecks as GraphReader and answers through it", () => {
    expect(fakeReader.findByQn("src/a.ts::alpha")?.qn).toBe("src/a.ts::alpha");
    expect(fakeReader.findByQn("src/a.ts::nope")).toBeNull();
    expect(fakeReader.metaFor().generation).toBe("g-9");
    expect(fakeReader.testGaps([1, 2])).toEqual([]);
  });

  test("a fake ToolContext typechecks without importing the root", () => {
    const handle = (ctx: ToolContext, args: { qn: string }): string | null =>
      ctx.reader.findByQn(args.qn)?.qn ?? null;

    expect(handle(fakeContext, { qn: "src/a.ts::alpha" })).toBe("src/a.ts::alpha");
    expect(fakeContext.now().toISOString()).toBe("2026-01-03T00:00:00.000Z");
    expect(fakeContext.config.repoRoot).toBe("/tmp/fake");
    expect("profile" in fakeContext.config).toBe(false);
  });

  test("the cursor pair from #shared/cursor satisfies CursorFacade", () => {
    const facade: CursorFacade = cursors;
    const payload = {
      generation: "g-9",
      tool: "search",
      argsHash: "abc",
      position: 20,
    };
    const token = facade.sign(payload);
    const expected = { generation: "g-9", tool: "search", argsHash: "abc" };
    expect(facade.verify(token, expected)).toEqual({ ok: true, position: 20 });
    expect(facade.verify(token, { ...expected, tool: "trace" })).toMatchObject({
      ok: false,
      code: "cursor_mismatch",
    });
    expect(facade.verify(token, { ...expected, generation: "g-10" })).toMatchObject({
      ok: false,
      code: "cursor_expired",
    });
  });

  test("context and reader import only #shared subpaths, never the tool tree or the root", () => {
    const readDir = join(import.meta.dir, "..", "src", "shared", "store", "read");
    for (const name of ["context.ts", "reader.ts", "files.ts"] as const) {
      const source = readFileSync(join(readDir, name), "utf8");
      const specifiers = [...source.matchAll(/from\s+"([^"]+)"/g)].map((match) => match[1] ?? "");
      expect(specifiers.length).toBeGreaterThan(0);
      for (const specifier of specifiers) {
        expect(specifier.startsWith("#shared/")).toBe(true);
      }
      expect(source).not.toContain("drizzle-orm");
      expect(source).not.toContain("console.log");
    }
  });
});
