import { Database } from "bun:sqlite";
import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { qn } from "#shared/paths";
import { migrate } from "#shared/store/migrate";
import { beginWrite, type WriteTx } from "#shared/store/publish";
import type { ExtractedNode, UnresolvedSite } from "../src/indexer/extract";
import {
  FTS_REBUILD_FRACTION,
  finalizeGeneration,
  type GenerationResult,
  type NotIndexedEntry,
  PENDING_GENERATION,
  type PersistUnit,
  persistUnit,
} from "../src/indexer/persist";
import type { BindResult, ResolvedEdge } from "../src/indexer/resolve";

/**
 * The persist pass against a real migrated database, built under `mkdtemp` exactly as the store
 * tests do, because everything under test is a fact about rows: a re-persist that leaves a
 * tombstone, a derived `tested_by` edge whose direction is the whole point, a `tested_depth`
 * column, and an FTS index that agrees with `node` after both a rowid delta and a rebuild.
 *
 * The fixture is three files: `src/widget.ts` declares `render` and `helper` and `render` calls
 * `helper`; `tests/widget.test.ts` declares a test node that calls `render`; `src/other.ts` is the
 * untouched control — nothing tests it and nothing calls it, so its `tested_depth` must stay NULL.
 *
 * The units are hand-built rather than extracted: grammar work is proved in `extract.test.ts` and
 * binding in `resolve.test.ts`, and what this file asserts is what the writer does with either.
 */

const WIDGET = "src/widget.ts";
const OTHER = "src/other.ts";
const TEST_FILE = "tests/widget.test.ts";

const WIDGET_FILE_QN = qn(WIDGET, "", "widget.ts");
const RENDER = qn(WIDGET, "", "render");
const HELPER = qn(WIDGET, "", "helper");
const OTHER_FILE_QN = qn(OTHER, "", "other.ts");
const LONELY = qn(OTHER, "", "lonely");
const TEST_FILE_QN = qn(TEST_FILE, "", "widget.test.ts");
const TEST_NODE = qn(TEST_FILE, "", "test_render");

type Binding = number | string | boolean | null;

const dirs: string[] = [];

afterEach(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A migrated graph in a temp directory: the shipped migrations through the one runner. */
async function tempGraph(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-persist-"));
  dirs.push(dir);
  const livePath = join(dir, "graph.db");
  const handle = new Database(livePath, { create: true });
  try {
    migrate(handle);
  } finally {
    handle.close();
  }
  return livePath;
}

/** One generation's writes: the whole run is one `BEGIN IMMEDIATE`, as `DATABASE.md` §5 fixes it. */
function writeGeneration(livePath: string, run: (tx: WriteTx) => void): void {
  const scope = beginWrite(livePath);
  try {
    scope.transaction(run);
  } finally {
    scope.close();
  }
}

/** The worker's half of the contract: the `file` row and its staleness manifest. */
function addFile(livePath: string, path: string, parseStatus = "ok"): number {
  const handle = new Database(livePath);
  try {
    handle
      .query<unknown, [string, string, number, number, string, string]>(
        "INSERT INTO file (path, language, sha256, size, line_count, parse_status, indexed_generation)" +
          " VALUES (?, 'typescript', ?, ?, ?, ?, ?)" +
          " ON CONFLICT(path) DO UPDATE SET sha256 = excluded.sha256," +
          " size = excluded.size, line_count = excluded.line_count," +
          " parse_status = excluded.parse_status",
      )
      .run(path, "a".repeat(64), 120, 9, parseStatus, "g-0");
    const row = handle
      .query<{ id: number }, [string]>("SELECT id FROM file WHERE path = ?")
      .get(path);
    if (row === null) {
      throw new Error(`no file row for ${path}`);
    }
    return row.id;
  } finally {
    handle.close();
  }
}

function rows<T>(livePath: string, sql: string, ...params: Binding[]): T[] {
  const handle = new Database(livePath, { readonly: true });
  try {
    return handle.query<T, Binding[]>(sql).all(...params);
  } finally {
    handle.close();
  }
}

function row<T>(livePath: string, sql: string, ...params: Binding[]): T | null {
  const found = rows<T>(livePath, sql, ...params);
  return found[0] ?? null;
}

function idOf(livePath: string, qualifiedName: string): number {
  const found = row<{ id: number }>(livePath, "SELECT id FROM node WHERE qn = ?", qualifiedName);
  if (found === null) {
    throw new Error(`no node for ${qualifiedName}`);
  }
  return found.id;
}

function decl(
  kind: ExtractedNode["kind"],
  path: string,
  scope: string,
  name: string,
  options: { readonly startLine?: number; readonly isTest?: boolean } = {},
): ExtractedNode {
  const start = options.startLine ?? 1;
  return {
    kind,
    name,
    qn: qn(path, scope, name),
    startLine: start,
    endLine: start + 2,
    signature: null,
    isTest: options.isTest ?? false,
    props: {},
  };
}

function fileNode(path: string, isTest = false): ExtractedNode {
  const name = path.slice(path.lastIndexOf("/") + 1);
  return decl("file", path, "", name, { isTest });
}

function unit(
  relPath: string,
  nodes: ExtractedNode[],
  options: { readonly notIndexed?: NotIndexedEntry[]; readonly unresolved?: UnresolvedSite[] } = {},
): PersistUnit {
  const base: PersistUnit = {
    relPath,
    nodes,
    rawEdges: [],
    unresolved: options.unresolved ?? [],
  };
  return options.notIndexed === undefined ? base : { ...base, notIndexed: options.notIndexed };
}

function resolvedOf(edges: ResolvedEdge[], unresolved: BindResult["unresolved"] = []): BindResult {
  return { edges, unresolved };
}

function edge(
  file: string,
  kind: ResolvedEdge["kind"],
  src: string,
  dst: string,
  line = 1,
  tier: ResolvedEdge["tier"] = "extracted",
): ResolvedEdge {
  return { file, kind, src, dst, tier, line, localName: "", evidence: kind };
}

/** `src/widget.ts`: the file node, `render`, and — unless dropped — `helper` called by `render`. */
function widgetUnit(includeHelper: boolean): PersistUnit {
  const file = fileNode(WIDGET);
  const render = decl("function", WIDGET, "", "render", { startLine: 3 });
  const nodes = includeHelper
    ? [file, render, decl("function", WIDGET, "", "helper", { startLine: 8 })]
    : [file, render];
  const edges = [edge(WIDGET, "contains", file.qn, render.qn, 3)];
  if (includeHelper) {
    edges.push(
      edge(WIDGET, "contains", file.qn, HELPER, 8),
      edge(WIDGET, "calls", RENDER, HELPER, 4),
    );
  }
  return unit(WIDGET, nodes, {
    unresolved: [
      { line: 5, rawTarget: "mystery", evidenceKind: "dynamic", evidence: "call:mystery" },
    ],
  });
}

function widgetEdges(includeHelper: boolean): ResolvedEdge[] {
  const edges = [edge(WIDGET, "contains", WIDGET_FILE_QN, RENDER, 3)];
  if (includeHelper) {
    edges.push(
      edge(WIDGET, "contains", WIDGET_FILE_QN, HELPER, 8),
      edge(WIDGET, "calls", RENDER, HELPER, 4),
    );
  }
  return edges;
}

function otherUnit(): PersistUnit {
  return unit(OTHER, [fileNode(OTHER), decl("function", OTHER, "", "lonely", { startLine: 3 })]);
}

function testUnit(): PersistUnit {
  return unit(TEST_FILE, [
    fileNode(TEST_FILE, true),
    decl("test", TEST_FILE, "", "test_render", { startLine: 3, isTest: true }),
  ]);
}

function testEdges(): ResolvedEdge[] {
  return [
    edge(TEST_FILE, "contains", TEST_FILE_QN, TEST_NODE, 3),
    edge(TEST_FILE, "calls", TEST_NODE, RENDER, 4),
  ];
}

/** The three-file graph, published as generation `g1`. */
function publishFirst(livePath: string): GenerationResult {
  for (const path of [WIDGET, OTHER, TEST_FILE]) {
    addFile(livePath, path);
  }
  let result: GenerationResult | undefined;
  writeGeneration(livePath, (tx) => {
    persistUnit(
      tx,
      widgetUnit(true),
      resolvedOf(widgetEdges(true), [
        {
          file: WIDGET,
          line: 6,
          rawTarget: "missing",
          evidenceKind: "receiver_type_unknown",
          evidence: "call:missing",
        },
      ]),
    );
    persistUnit(tx, otherUnit(), resolvedOf([edge(OTHER, "contains", OTHER_FILE_QN, LONELY, 3)]));
    persistUnit(tx, testUnit(), resolvedOf(testEdges()));
    result = finalizeGeneration(tx, {
      generationId: "g1",
      baseCommit: "abc1234",
      mode: "full",
      publishedAt: "2026-01-01T00:00:00.000Z",
      durationMs: 12,
    });
  });
  if (result === undefined) {
    throw new Error("finalizeGeneration returned nothing");
  }
  return result;
}

describe("persistUnit", () => {
  test("writes one unit set and the rows a later generation keys on", async () => {
    const livePath = await tempGraph();
    const first = publishFirst(livePath);

    const qns = rows<{ qn: string }>(livePath, "SELECT qn FROM node ORDER BY qn").map((r) => r.qn);
    expect(qns).toEqual([
      LONELY,
      OTHER_FILE_QN,
      HELPER,
      RENDER,
      WIDGET_FILE_QN,
      TEST_NODE,
      TEST_FILE_QN,
    ]);
    expect(first.nodes).toBe(7);
    expect(first.edges).toBe(7);
    expect(first.tombstones).toEqual([]);

    // Containment is the parent link, so `render` hangs off the file node and the file node off
    // nothing.
    const render = row<{ id: number; parent_id: number | null }>(
      livePath,
      "SELECT id, parent_id FROM node WHERE qn = ?",
      RENDER,
    );
    expect(render?.parent_id).toBe(idOf(livePath, WIDGET_FILE_QN));
    expect(
      row<{ parent_id: number | null }>(
        livePath,
        "SELECT parent_id FROM node WHERE qn = ?",
        WIDGET_FILE_QN,
      )?.parent_id,
    ).toBeNull();

    const kinds = rows<{ kind: string; tier: string; rows: number }>(
      livePath,
      "SELECT kind, tier, count(*) AS rows FROM edge GROUP BY kind, tier ORDER BY kind",
    );
    expect(kinds).toEqual([
      { kind: "calls", tier: "extracted", rows: 2 },
      { kind: "contains", tier: "extracted", rows: 4 },
      { kind: "tested_by", tier: "inferred", rows: 1 },
    ]);

    // Both kinds of unbound site are kept: the extractor's dynamic call and the bind pass's
    // receiver-typed miss.
    const widgetId = idOf(livePath, WIDGET_FILE_QN);
    const unresolved = rows<{ raw_target: string; evidence_kind: string }>(
      livePath,
      "SELECT raw_target, evidence_kind FROM unresolved WHERE file_id = ? ORDER BY raw_target",
      widgetId,
    );
    expect(unresolved).toEqual([
      { raw_target: "missing", evidence_kind: "receiver_type_unknown" },
      { raw_target: "mystery", evidence_kind: "dynamic" },
    ]);

    // Every node has a metrics row, the generation row is the publish, and `meta.generation`
    // names it.
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM node_metrics")?.rows).toBe(
      7,
    );
    expect(
      rows<{ id: string; seq: number; status: string; nodes: number; edges: number; mode: string }>(
        livePath,
        "SELECT id, seq, status, nodes, edges, mode FROM generation",
      ),
    ).toEqual([{ id: "g1", seq: 1, status: "published", nodes: 7, edges: 7, mode: "full" }]);
    expect(
      row<{ value: string }>(livePath, "SELECT value FROM meta WHERE key = 'generation'")?.value,
    ).toBe("g1");

    // Re-persisting the same unit set replaces its rows instead of duplicating them.
    writeGeneration(livePath, (tx) => {
      persistUnit(
        tx,
        widgetUnit(true),
        resolvedOf(widgetEdges(true), [
          {
            file: WIDGET,
            line: 6,
            rawTarget: "missing",
            evidenceKind: "receiver_type_unknown",
            evidence: "call:missing",
          },
        ]),
      );
      finalizeGeneration(tx, { generationId: "g2", baseCommit: "abc1234", mode: "fast" });
    });
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM node")?.rows).toBe(7);
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM unresolved")?.rows).toBe(
      2,
    );
    expect(row<{ rows: number }>(livePath, TOMBSTONE_COUNT_SQL)?.rows).toBe(0);
  });

  test("refuses a unit whose file row is missing, rather than inventing an id", async () => {
    const livePath = await tempGraph();
    const scope = beginWrite(livePath);
    try {
      expect(() =>
        scope.transaction((tx) => {
          persistUnit(tx, unit("src/ghost.ts", [fileNode("src/ghost.ts")]), resolvedOf([]));
        }),
      ).toThrow(/no file row for src\/ghost\.ts/);
    } finally {
      scope.close();
    }
  });

  test("owns the coverage rows of its own path and refuses another path's", async () => {
    const livePath = await tempGraph();
    const broken = "src/broken.ts";
    addFile(livePath, broken, "failed");
    const entry: NotIndexedEntry = {
      path: broken,
      reason: "grammar_unavailable",
      detail: "no grammar for typescript",
      language: "typescript",
    };
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, unit(broken, [fileNode(broken)], { notIndexed: [entry] }), resolvedOf([]));
      finalizeGeneration(tx, { generationId: "g1", baseCommit: null, mode: "full" });
    });
    expect(
      rows<{ path: string; reason: string; detail: string | null; language: string | null }>(
        livePath,
        "SELECT path, reason, detail, language FROM not_indexed",
      ),
    ).toEqual([
      {
        path: broken,
        reason: "grammar_unavailable",
        detail: "no grammar for typescript",
        language: "typescript",
      },
    ]);
  });

  test("drops the coverage rows of a path it now indexes", async () => {
    const livePath = await tempGraph();
    const broken = "src/broken.ts";
    addFile(livePath, broken, "failed");
    writeGeneration(livePath, (tx) => {
      persistUnit(
        tx,
        unit(broken, [fileNode(broken)], {
          notIndexed: [{ path: broken, reason: "parse_failed", detail: null, language: null }],
        }),
        resolvedOf([]),
      );
      finalizeGeneration(tx, { generationId: "g1", baseCommit: null, mode: "full" });
    });
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM not_indexed")?.rows).toBe(
      1,
    );

    writeGeneration(livePath, (tx) => {
      persistUnit(tx, unit(broken, [fileNode(broken)]), resolvedOf([]));
      finalizeGeneration(tx, { generationId: "g2", baseCommit: null, mode: "fast" });
    });
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM not_indexed")?.rows).toBe(
      0,
    );

    const scope = beginWrite(livePath);
    try {
      expect(() =>
        scope.transaction((tx) => {
          persistUnit(
            tx,
            unit(broken, [fileNode(broken)], {
              notIndexed: [{ path: OTHER, reason: "binary" }],
            }),
            resolvedOf([]),
          );
        }),
      ).toThrow(/not_indexed entry for src\/other\.ts/);
    } finally {
      scope.close();
    }
  });
});

describe("re-persisting one file", () => {
  test("deletes the removed symbol, records exactly one tombstone and leaves no dangling edge", async () => {
    const livePath = await tempGraph();
    publishFirst(livePath);
    const helperId = idOf(livePath, HELPER);

    let second: GenerationResult | undefined;
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, widgetUnit(false), resolvedOf(widgetEdges(false)));
      // The test unit is handed over again: its `calls` edge targeted the old `render` id, which
      // the re-persist replaced, and a run that re-indexes a file has to re-index its dependents
      // too (the incremental planner's boundary, not this writer's).
      persistUnit(tx, testUnit(), resolvedOf(testEdges()));
      second = finalizeGeneration(tx, {
        generationId: "g2",
        baseCommit: "abc1234",
        mode: "fast",
        publishedAt: "2026-01-02T00:00:00.000Z",
        durationMs: 4,
      });
    });
    if (second === undefined) {
      throw new Error("finalizeGeneration returned nothing");
    }

    expect(rows<{ qn: string }>(livePath, "SELECT qn FROM node WHERE qn = ?", HELPER)).toEqual([]);
    expect(second.tombstones).toEqual([HELPER]);
    expect(second.nodes).toBe(6);
    expect(
      rows<{
        qn: string;
        kind: string;
        last_seen_generation: string;
        removed_in_generation: string;
        had_callers: number;
      }>(
        livePath,
        "SELECT qn, kind, last_seen_generation, removed_in_generation, had_callers FROM tombstone",
      ),
    ).toEqual([
      {
        qn: HELPER,
        kind: "function",
        last_seen_generation: "g1",
        removed_in_generation: "g2",
        had_callers: 1,
      },
    ]);
    // No pending marker survives a finalize, and nothing else disappeared.
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM tombstone WHERE removed_in_generation = ?",
        PENDING_GENERATION,
      )?.rows,
    ).toBe(0);

    // Deleting the node took its edges with it: nothing points at a row that is gone, and SQLite
    // agrees the graph is consistent.
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM edge e WHERE NOT EXISTS (SELECT 1 FROM node n WHERE n.id = e.src_id)" +
          " OR NOT EXISTS (SELECT 1 FROM node n WHERE n.id = e.dst_id)",
      )?.rows,
    ).toBe(0);
    expect(rows<Record<string, unknown>>(livePath, "PRAGMA foreign_key_check")).toEqual([]);

    // The FTS mirror and index agree with the node table.
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM node_fts_state WHERE rowid_ = ?",
        helperId,
      )?.rows,
    ).toBe(0);
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'helper'"),
    ).toEqual([]);
    // `test_render` tokenises to `test` + `render`, so the token finds the test as well as the
    // function it exercises.
    expect(
      rows<{ rowid: number }>(
        livePath,
        "SELECT rowid FROM node_fts WHERE node_fts MATCH 'render' ORDER BY rowid",
      ).map((found) => found.rowid),
    ).toEqual([idOf(livePath, RENDER), idOf(livePath, TEST_NODE)].sort((a, b) => a - b));

    // Three mirror rows went (the removed symbol and the two re-written ones) and two came back:
    // five of six rows is above the threshold, so this publish rebuilt the index — and the removed
    // symbol's token is gone from it, which is the half of a rebuild a delta test cannot show.
    expect(second.fts.mode).toBe("rebuild");

    // Degrees were recounted for the whole table because a symbol disappeared: `render` no longer
    // calls anything, and the test's `tested_by` edge still hangs off it.
    expect(second.metrics.mode).toBe("all");
    const render = row<{ in_degree: number; out_degree: number; tested_depth: number | null }>(
      livePath,
      "SELECT m.in_degree, m.out_degree, m.tested_depth FROM node_metrics m WHERE m.node_id = ?",
      idOf(livePath, RENDER),
    );
    expect(render).toEqual({ in_degree: 3, out_degree: 0, tested_depth: 1 });
  });
});

describe("a run written in several transactions", () => {
  test("keeps the disappeared qn until a later transaction names the generation", async () => {
    const livePath = await tempGraph();
    publishFirst(livePath);

    // One transaction per unit, the shape a run that does not want to hold one huge write lock
    // takes: nothing is carried in memory between the two calls.
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, widgetUnit(false), resolvedOf(widgetEdges(false)));
    });
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM tombstone WHERE removed_in_generation = ?",
        PENDING_GENERATION,
      )?.rows,
    ).toBe(1);

    let second: GenerationResult | undefined;
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, testUnit(), resolvedOf(testEdges()));
      second = finalizeGeneration(tx, { generationId: "g2", baseCommit: null, mode: "fast" });
    });
    if (second === undefined) {
      throw new Error("finalizeGeneration returned nothing");
    }
    expect(second.tombstones).toEqual([HELPER]);
    // The touched set is read from the rows themselves, so a node written two transactions ago is
    // still part of it.
    expect(second.metrics.mode).toBe("all");
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM tombstone WHERE removed_in_generation = 'g2'",
      )?.rows,
    ).toBe(1);
  });
});

describe("tested_by", () => {
  test("runs from the test node to the symbol it calls, with tested_depth 1 and 2 behind it", async () => {
    const livePath = await tempGraph();
    const first = publishFirst(livePath);

    const testedBy = rows<{ src: string; dst: string; tier: string }>(
      livePath,
      "SELECT s.qn AS src, d.qn AS dst, e.tier AS tier FROM edge e" +
        " JOIN node s ON s.id = e.src_id JOIN node d ON d.id = e.dst_id" +
        " WHERE e.kind = 'tested_by'",
    );
    expect(testedBy).toEqual([{ src: TEST_NODE, dst: RENDER, tier: "inferred" }]);
    // The inversion is deliberate and is what puts the edge in the queried symbol's inbound group:
    // no `tested_by` edge may start at a non-test node.
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM edge e JOIN node s ON s.id = e.src_id" +
          " WHERE e.kind = 'tested_by' AND s.is_test = 0",
      )?.rows,
    ).toBe(0);

    const depths = rows<{ qn: string; tested_depth: number | null }>(
      livePath,
      "SELECT n.qn AS qn, m.tested_depth AS tested_depth FROM node n" +
        " JOIN node_metrics m ON m.node_id = n.id ORDER BY n.qn",
    );
    expect(depths).toEqual([
      { qn: LONELY, tested_depth: null },
      { qn: OTHER_FILE_QN, tested_depth: null },
      { qn: HELPER, tested_depth: 2 },
      { qn: RENDER, tested_depth: 1 },
      { qn: WIDGET_FILE_QN, tested_depth: null },
      { qn: TEST_NODE, tested_depth: null },
      { qn: TEST_FILE_QN, tested_depth: null },
    ]);

    // Degrees count every edge, the derived one included: `render` is contained in its file, called
    // by the test and tested by it; the test node is contained, calls, and is the edge's source.
    expect(
      rows<{ qn: string; in_degree: number; out_degree: number }>(
        livePath,
        "SELECT n.qn AS qn, m.in_degree, m.out_degree FROM node n" +
          " JOIN node_metrics m ON m.node_id = n.id WHERE n.qn IN (?, ?) ORDER BY n.qn",
        RENDER,
        TEST_NODE,
      ),
    ).toEqual([
      { qn: RENDER, in_degree: 3, out_degree: 1 },
      { qn: TEST_NODE, in_degree: 1, out_degree: 2 },
    ]);
    expect(first.metrics.mode).toBe("neighbourhood");
  });
});

describe("FTS delta", () => {
  test("applies a rowid delta below the threshold and rebuilds above it", async () => {
    const livePath = await tempGraph();
    const ALPHA = "src/a.ts";
    const BEE = "src/b.ts";
    addFile(livePath, ALPHA);
    addFile(livePath, BEE);

    const alphaUnit = (name: string): PersistUnit => {
      const file = fileNode(ALPHA);
      return unit(ALPHA, [file, decl("function", ALPHA, "", name, { startLine: 3 })]);
    };
    const alphaEdges = (name: string): ResolvedEdge[] => [
      edge(ALPHA, "contains", qn(ALPHA, "", "a.ts"), qn(ALPHA, "", name), 3),
    ];
    const beeUnit = (): PersistUnit => {
      const file = fileNode(BEE);
      const nodes = [file];
      for (let index = 1; index <= 20; index += 1) {
        nodes.push(decl("function", BEE, "", `b${index}`, { startLine: index + 2 }));
      }
      return unit(BEE, nodes);
    };
    const beeEdges = (): ResolvedEdge[] => {
      const edges: ResolvedEdge[] = [];
      for (let index = 1; index <= 20; index += 1) {
        edges.push(edge(BEE, "contains", qn(BEE, "", "b.ts"), qn(BEE, "", `b${index}`), index + 2));
      }
      return edges;
    };

    expect(FTS_REBUILD_FRACTION).toBe(0.35);

    let first: GenerationResult | undefined;
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, alphaUnit("alpha"), resolvedOf(alphaEdges("alpha")));
      persistUnit(tx, beeUnit(), resolvedOf(beeEdges()));
      first = finalizeGeneration(tx, { generationId: "g1", baseCommit: null, mode: "full" });
    });
    if (first === undefined) {
      throw new Error("finalizeGeneration returned nothing");
    }
    // Everything is new, so the fraction is 1 and the first publish of a graph rebuilds.
    expect(first.fts).toEqual({ mode: "rebuild", inserted: 23, removed: 0, rows: 23, fraction: 1 });

    const alphaId = idOf(livePath, qn(ALPHA, "", "alpha"));
    let second: GenerationResult | undefined;
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, alphaUnit("beta"), resolvedOf(alphaEdges("beta")));
      second = finalizeGeneration(tx, { generationId: "g2", baseCommit: null, mode: "fast" });
    });
    if (second === undefined) {
      throw new Error("finalizeGeneration returned nothing");
    }
    // Two rows out of 23 is 0.087, well under the threshold: the delta path, not a rebuild.
    expect(second.fts.mode).toBe("delta");
    expect(second.fts.rows).toBe(23);
    expect(second.fts.fraction).toBeCloseTo(4 / 23, 10);
    expect(second.fts.removed).toBe(2);
    expect(second.fts.inserted).toBe(2);
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'alpha'"),
    ).toEqual([]);
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'beta'"),
    ).toEqual([{ rowid: idOf(livePath, qn(ALPHA, "", "beta")) }]);
    expect(
      row<{ rows: number }>(
        livePath,
        "SELECT count(*) AS rows FROM node_fts_state WHERE rowid_ = ?",
        alphaId,
      )?.rows,
    ).toBe(0);
    // The untouched file is still searchable: the delta removed only what it had to.
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'b7'"),
    ).toEqual([{ rowid: idOf(livePath, qn(BEE, "", "b7")) }]);

    let third: GenerationResult | undefined;
    writeGeneration(livePath, (tx) => {
      persistUnit(tx, beeUnit(), resolvedOf(beeEdges()));
      third = finalizeGeneration(tx, { generationId: "g3", baseCommit: null, mode: "fast" });
    });
    if (third === undefined) {
      throw new Error("finalizeGeneration returned nothing");
    }
    // Re-persisting the 21 nodes of `src/b.ts` deletes 21 mirror rows and adds 21: ids are not
    // reused, so a rewritten row costs one of each. 42 of 23 is above the threshold, so the index
    // is rebuilt from the mirror instead of being patched row by row.
    expect(third.fts.mode).toBe("rebuild");
    expect(third.fts.fraction).toBeCloseTo(42 / 23, 10);
    expect(third.fts.inserted).toBe(23);
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'b13'"),
    ).toEqual([{ rowid: idOf(livePath, qn(BEE, "", "b13")) }]);
    expect(
      rows<{ rowid: number }>(livePath, "SELECT rowid FROM node_fts WHERE node_fts MATCH 'beta'"),
    ).toEqual([{ rowid: idOf(livePath, qn(ALPHA, "", "beta")) }]);
    // The mirror and the node table are the same size after both paths, which is the invariant a
    // rebuild would otherwise be able to break silently.
    expect(
      row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM node_fts_state")?.rows,
    ).toBe(23);
    expect(row<{ rows: number }>(livePath, "SELECT count(*) AS rows FROM node")?.rows).toBe(23);
  });
});

/** Every tombstone, pending or published: the count a re-persist must leave at zero. */
const TOMBSTONE_COUNT_SQL = "SELECT count(*) AS rows FROM tombstone";
