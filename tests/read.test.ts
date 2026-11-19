import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { isUnderPrefix } from "#shared/paths";
import { type Conn, db, openReader, openWriter, statementCacheStats } from "#shared/store/db";
import { migrate } from "#shared/store/migrate";
import {
  notIndexedAll,
  notIndexedCount,
  rowsForPaths,
  unresolvedForFiles,
} from "#shared/store/read/coverage";
import { inbound, inboundMany, outbound, outboundMany } from "#shared/store/read/edges";
import { metricsFor } from "#shared/store/read/metrics";
import { findByName, findByQn, members, nodesByFile } from "#shared/store/read/nodes";
import * as schema from "#shared/store/schema";

/**
 * The typed read primitives against a real three-node database: one class with a method in
 * `src/a.ts`, one function in `src/b.ts`, one edge of each kind worth distinguishing, and a
 * skipped file beside an indexed sibling.
 *
 * The fixture is deliberately small enough to assert exact rows, because the two properties
 * that matter are exact: the prefix rule must exclude `src-old` for the prefix `src`, and the
 * `not_indexed` half of a coverage query must be populated with the reason.
 */

const READ_DIR = join(import.meta.dir, "..", "src", "shared", "store", "read");
const READ_MODULES = ["nodes.ts", "edges.ts", "metrics.ts", "coverage.ts"] as const;

/** File ids, node ids and the generation string the fixture writes. */
const SRC_A = 1;
const SRC_B = 2;
const SRC_OLD = 3;
const BETA = 1;
const ALPHA = 2;
const GAMMA = 3;
const GENERATION = "g-1";

const dirs: string[] = [];
let reader: Conn;

/**
 * The modules name `$inferSelect` in prose explaining the rule they obey; the check is about
 * code, so comments come out first.
 */
function withoutComments(source: string): string {
  return source.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");
}

function seed(writer: Conn): void {
  const handle = db(writer);

  handle
    .insert(schema.file)
    .values([
      {
        id: SRC_A,
        path: "src/a.ts",
        language: "typescript",
        sha256: "a".repeat(64),
        size: 640,
        lineCount: 40,
        parseStatus: "ok",
        parseErrorRanges: null,
        indexedGeneration: GENERATION,
      },
      {
        id: SRC_B,
        path: "src/b.ts",
        language: "typescript",
        sha256: "b".repeat(64),
        size: 120,
        lineCount: 9,
        parseStatus: "partial",
        parseErrorRanges: "7-7",
        indexedGeneration: GENERATION,
      },
      {
        id: SRC_OLD,
        path: "src-old/a.ts",
        language: "typescript",
        sha256: "c".repeat(64),
        size: 90,
        lineCount: 5,
        parseStatus: "ok",
        parseErrorRanges: null,
        indexedGeneration: GENERATION,
      },
    ])
    .run();

  // Parents before children: the self-referencing foreign key is checked per row.
  handle
    .insert(schema.node)
    .values([
      {
        id: BETA,
        fileId: SRC_A,
        parentId: null,
        kind: "class",
        subkind: null,
        name: "Beta",
        qn: "src/a.ts::Beta",
        startLine: 1,
        endLine: 40,
        signature: "class Beta",
        docstring: "the enclosing class",
        isTest: 0,
        props: '{"decorators":["Injectable"]}',
      },
      {
        id: GAMMA,
        fileId: SRC_B,
        parentId: null,
        kind: "function",
        subkind: null,
        name: "gamma",
        qn: "src/b.ts::gamma",
        startLine: 3,
        endLine: 9,
        signature: "function gamma(): void",
        docstring: null,
        isTest: 0,
        props: "{}",
      },
    ])
    .run();
  handle
    .insert(schema.node)
    .values([
      {
        id: ALPHA,
        fileId: SRC_A,
        parentId: BETA,
        kind: "method",
        subkind: "method",
        name: "alpha",
        qn: "src/a.ts::Beta.alpha",
        startLine: 10,
        endLine: 20,
        signature: "alpha(): number",
        docstring: null,
        isTest: 0,
        props: "{}",
      },
    ])
    .run();

  handle
    .insert(schema.edge)
    .values([
      {
        id: 1,
        srcId: ALPHA,
        dstId: GAMMA,
        kind: "calls",
        tier: "extracted",
        fileId: SRC_A,
        line: 12,
        localName: "",
        evidence: "same_file",
      },
      {
        id: 2,
        srcId: BETA,
        dstId: ALPHA,
        kind: "defines",
        tier: "extracted",
        fileId: SRC_A,
        line: 10,
        localName: "",
        evidence: "same_file",
      },
      {
        id: 3,
        srcId: GAMMA,
        dstId: BETA,
        kind: "references",
        tier: "inferred",
        fileId: SRC_B,
        line: 5,
        localName: "Beta",
        evidence: "import:src/a",
      },
      {
        id: 4,
        srcId: GAMMA,
        dstId: ALPHA,
        kind: "calls",
        tier: "ambiguous",
        fileId: SRC_B,
        line: 6,
        localName: "",
        evidence: "receiver_type_unknown",
      },
    ])
    .run();

  handle
    .insert(schema.nodeMetrics)
    .values([
      { nodeId: BETA, inDegree: 1, outDegree: 1, isEntry: 1, testedDepth: 2, reachableCount: 12 },
      {
        nodeId: ALPHA,
        inDegree: 2,
        outDegree: 1,
        isEntry: 0,
        testedDepth: 0,
        reachableCount: null,
      },
      {
        nodeId: GAMMA,
        inDegree: 1,
        outDegree: 2,
        isEntry: 0,
        testedDepth: null,
        reachableCount: null,
      },
    ])
    .run();

  // Three skipped files: two under the queried prefix, one beside an indexed sibling, and one
  // under a subdirectory of the prefix so the repo-wide total differs from any prefix slice.
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
      { id: 2, path: "src-old/skip.ts", reason: "binary", detail: null, language: null },
      {
        id: 3,
        path: "src/vendor/bundle.min.js",
        reason: "size_cap",
        detail: "1 file over 2.0 MB",
        language: "javascript",
      },
    ])
    .run();

  handle
    .insert(schema.unresolved)
    .values([
      {
        id: 1,
        fileId: SRC_A,
        line: 12,
        rawTarget: "external.thing",
        evidenceKind: "dynamic",
        evidence: "no binding in scope",
      },
    ])
    .run();
}

beforeAll(() => {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-read-"));
  dirs.push(dir);
  const path = join(dir, "graph.db");
  const writer = openWriter(path);
  try {
    migrate(writer.sqlite);
    seed(writer);
  } finally {
    writer.close();
  }
  reader = openReader(path);
});

afterAll(() => {
  reader?.close();
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("read/nodes", () => {
  test("findByQn resolves the exact qn and maps the row off the SQL shape", () => {
    const beta = findByQn(reader, "src/a.ts::Beta");
    expect(beta).not.toBeNull();
    expect(beta?.id).toBe(BETA);
    expect(beta?.fileId).toBe(SRC_A);
    expect(beta?.path).toBe("src/a.ts");
    expect(beta?.kind).toBe("class");
    expect(beta?.isTest).toBe(false);
    expect(beta?.props).toEqual({ decorators: ["Injectable"] });
    expect(beta?.startLine).toBe(1);
  });

  test("findByQn returns null rather than throwing for an unknown qn", () => {
    expect(findByQn(reader, "src/a.ts::nope")).toBeNull();
  });

  test("findByName is exact, not fuzzy, and widens by kind only when asked", () => {
    expect(findByName(reader, "alpha").map((row) => row.qn)).toEqual(["src/a.ts::Beta.alpha"]);
    expect(findByName(reader, "alpha", ["method"]).map((row) => row.id)).toEqual([ALPHA]);
    expect(findByName(reader, "alpha", ["class"])).toEqual([]);
    expect(findByName(reader, "alp").map((row) => row.qn)).toEqual([]);
  });

  test("findByName orders the candidates by qn so a caller's cursor is stable", () => {
    const names = findByName(reader, "gamma").map((row) => row.qn);
    expect(names).toEqual(["src/b.ts::gamma"]);
  });

  test("members returns the direct children only", () => {
    expect(members(reader, BETA).map((row) => row.qn)).toEqual(["src/a.ts::Beta.alpha"]);
    expect(members(reader, ALPHA)).toEqual([]);
  });

  test("nodesByFile returns one file's nodes in source order", () => {
    expect(nodesByFile(reader, SRC_A).map((row) => row.qn)).toEqual([
      "src/a.ts::Beta",
      "src/a.ts::Beta.alpha",
    ]);
    expect(nodesByFile(reader, SRC_B).map((row) => row.name)).toEqual(["gamma"]);
  });
});

describe("read/edges", () => {
  test("outbound attaches the destination node", () => {
    const out = outbound(reader, BETA);
    expect(out).toHaveLength(1);
    expect(out[0]?.edge.kind).toBe("defines");
    expect(out[0]?.edge.srcId).toBe(BETA);
    expect(out[0]?.node.id).toBe(ALPHA);
    expect(out[0]?.node.qn).toBe("src/a.ts::Beta.alpha");
  });

  test("inbound attaches the source node", () => {
    const inb = inbound(reader, BETA);
    expect(inb).toHaveLength(1);
    expect(inb[0]?.edge.kind).toBe("references");
    expect(inb[0]?.edge.tier).toBe("inferred");
    expect(inb[0]?.edge.evidence).toBe("import:src/a");
    expect(inb[0]?.node.id).toBe(GAMMA);
    expect(inb[0]?.node.path).toBe("src/b.ts");
  });

  test("both directions group by kind in a stable order", () => {
    expect(outbound(reader, GAMMA).map((row) => row.edge.kind)).toEqual(["calls", "references"]);
    // Kind first, then tier: `calls` sorts before `defines`, so edge 4 precedes edge 2.
    expect(inbound(reader, ALPHA).map((row) => row.edge.id)).toEqual([4, 2]);
  });

  test("the kind filter is all-or-nothing and uses the composite index shape", () => {
    expect(outbound(reader, BETA, ["calls"])).toEqual([]);
    expect(outbound(reader, BETA, ["defines"]).map((row) => row.edge.id)).toEqual([2]);
    expect(inbound(reader, GAMMA, ["calls"]).map((row) => row.edge.id)).toEqual([1]);
    expect(inbound(reader, GAMMA, ["contains"])).toEqual([]);
  });

  test("no edge in either direction for a leaf on that side", () => {
    expect(outbound(reader, ALPHA).map((row) => row.edge.id)).toEqual([1]);
    expect(inbound(reader, ALPHA, ["defines"]).map((row) => row.node.name)).toEqual(["Beta"]);
  });
});

describe("read/edges: batched lookups", () => {
  /** Batches are unordered by contract, so a comparison sorts on the edge id first. */
  const byEdgeId = (rows: readonly { edge: { id: number } }[]) =>
    [...rows].sort((a, b) => a.edge.id - b.edge.id);

  test("a batch returns exactly the rows the same ids return one at a time", () => {
    expect(byEdgeId(inboundMany(reader, [ALPHA, BETA, GAMMA]))).toEqual(
      byEdgeId([ALPHA, BETA, GAMMA].flatMap((id) => inbound(reader, id))),
    );
    expect(byEdgeId(outboundMany(reader, [ALPHA, BETA, GAMMA]))).toEqual(
      byEdgeId([ALPHA, BETA, GAMMA].flatMap((id) => outbound(reader, id))),
    );
  });

  test("the kind filter and both endpoints behave as they do per id", () => {
    expect(inboundMany(reader, [ALPHA], ["calls"]).map((row) => row.edge.id)).toEqual([4]);
    expect(inboundMany(reader, [GAMMA], ["calls"]).map((row) => row.edge.id)).toEqual([1]);
    expect(inboundMany(reader, [GAMMA], ["contains"])).toEqual([]);
    expect(outboundMany(reader, [BETA], ["defines"]).map((row) => row.node.qn)).toEqual([
      "src/a.ts::Beta.alpha",
    ]);
  });

  test("an empty batch is an empty answer, not a query", () => {
    expect(inboundMany(reader, [])).toEqual([]);
    expect(outboundMany(reader, [], ["calls"])).toEqual([]);
  });

  test("a repeated id is a membership test, so its rows are not duplicated", () => {
    // The tail chunk is padded to the batch arity by repeating an id (`edges.ts` `toBatch`);
    // this is the SQLite behaviour that padding relies on.
    expect(byEdgeId(inboundMany(reader, [BETA, BETA, BETA]))).toEqual(
      byEdgeId(inbound(reader, BETA)),
    );
  });

  test("a frontier wider than one chunk is answered in full", () => {
    // 201 ids: the real ones sit in different chunks, the rest match nothing. A chunking bug would
    // either drop the second chunk or bind the wrong arity and raise.
    const ids = Array.from({ length: 201 }, (_, index) => 1000 + index);
    ids[0] = BETA;
    ids[200] = GAMMA;
    expect(byEdgeId(inboundMany(reader, ids))).toEqual(
      byEdgeId([...inbound(reader, BETA), ...inbound(reader, GAMMA)]),
    );
  });

  test("the statement cache stays bounded however many frontier lengths a caller asks for", () => {
    // Every batch binds the same arity, so the `IN (?, …)` shape cannot flood the 200-entry cache
    // the way one shape per frontier length would: 40 different sizes build exactly one statement.
    const kinds = ["calls", "defines"];
    const missesBefore = reader.statementCacheStats().misses;
    for (let size = 1; size <= 40; size += 1) {
      inboundMany(
        reader,
        Array.from({ length: size }, (_, index) => 1 + (index % 3)),
        kinds,
      );
    }
    const after = reader.statementCacheStats();
    expect(after.misses - missesBefore).toBe(1);
    expect(after.size).toBeLessThanOrEqual(after.capacity);
  });
});

describe("read/metrics", () => {
  test("metricsFor reports both degrees, entry and tested_depth", () => {
    const beta = metricsFor(reader, BETA);
    expect(beta).toEqual({
      nodeId: BETA,
      inDegree: 1,
      outDegree: 1,
      isEntry: true,
      testedDepth: 2,
      reachableCount: 12,
    });
  });

  test("tested_depth 0 and NULL stay distinguishable", () => {
    expect(metricsFor(reader, ALPHA)?.testedDepth).toBe(0);
    expect(metricsFor(reader, GAMMA)?.testedDepth).toBeNull();
    expect(metricsFor(reader, GAMMA)?.isEntry).toBe(false);
  });

  test("an unmeasured node is null, not a row of zeroes", () => {
    expect(metricsFor(reader, 999)).toBeNull();
  });
});

describe("read/coverage", () => {
  test("rowsForPaths(src) includes src/a.ts and excludes src-old/a.ts", () => {
    const coverage = rowsForPaths(reader, ["src"]);
    expect(coverage.indexed.map((row) => row.path)).toEqual(["src/a.ts", "src/b.ts"]);
    for (const row of coverage.indexed) {
      expect(isUnderPrefix(row.path, "src")).toBe(true);
    }
    expect(isUnderPrefix("src-old/a.ts", "src")).toBe(false);
  });

  test("rowsForPaths returns the not_indexed half with its reason", () => {
    const coverage = rowsForPaths(reader, ["src"]);
    expect(coverage.notIndexed.map((row) => row.path)).toEqual([
      "src/gen.ts",
      "src/vendor/bundle.min.js",
    ]);
    expect(coverage.notIndexed[0]?.reason).toBe("grammar_unavailable");
    expect(coverage.notIndexed[0]?.detail).toBe("no grammar for .cts");
    expect(coverage.notIndexed[0]?.language).toBe("typescript");
  });

  test("a skipped path alone is not an unseen path", () => {
    const coverage = rowsForPaths(reader, ["src/gen.ts"]);
    expect(coverage.indexed).toEqual([]);
    expect(coverage.notIndexed.map((row) => row.path)).toEqual(["src/gen.ts"]);
  });

  test("the prefix itself is matched by equality, its children by a strict range", () => {
    const coverage = rowsForPaths(reader, ["src/a.ts"]);
    expect(coverage.indexed.map((row) => row.path)).toEqual(["src/a.ts"]);
    expect(coverage.notIndexed).toEqual([]);
  });

  test("a sibling prefix is still its own prefix, not an excluded one", () => {
    const coverage = rowsForPaths(reader, ["src-old"]);
    expect(coverage.indexed.map((row) => row.path)).toEqual(["src-old/a.ts"]);
    expect(coverage.notIndexed.map((row) => row.path)).toEqual(["src-old/skip.ts"]);
  });

  test("several prefixes in one call stay one statement per arity", () => {
    const coverage = rowsForPaths(reader, ["src/a.ts", "src-old"]);
    expect(coverage.indexed.map((row) => row.path)).toEqual(["src-old/a.ts", "src/a.ts"]);
    expect(coverage.notIndexed.map((row) => row.path)).toEqual(["src-old/skip.ts"]);
  });

  test("the indexed file row keeps the census fields status needs", () => {
    const partial = rowsForPaths(reader, ["src/b.ts"]).indexed[0];
    expect(partial?.parseStatus).toBe("partial");
    expect(partial?.parseErrorRanges).toBe("7-7");
    expect(partial?.lineCount).toBe(9);
    expect(partial?.indexedGeneration).toBe(GENERATION);
  });

  test("no prefixes is an empty answer with no query", () => {
    expect(rowsForPaths(reader, [])).toEqual({ indexed: [], notIndexed: [] });
  });

  test("notIndexedAll is the repo-wide list a prefix can only be a slice of", () => {
    // `src-old` is deliberately outside the prefix: a total summed from prefix slices would
    // report 2 here, and the point of the repo-wide pair is that it reports 3.
    expect(notIndexedAll(reader).map((row) => row.path)).toEqual([
      "src-old/skip.ts",
      "src/gen.ts",
      "src/vendor/bundle.min.js",
    ]);
    expect(rowsForPaths(reader, ["src"]).notIndexed).toHaveLength(2);
    expect(notIndexedCount(reader)).toBe(3);
    expect(notIndexedCount(reader)).toBe(notIndexedAll(reader).length);
  });

  test("notIndexedAll keeps the row shape and the unattributed language", () => {
    const skipped = notIndexedAll(reader).find((row) => row.path === "src-old/skip.ts");
    expect(skipped?.id).toBe(2);
    expect(skipped?.reason).toBe("binary");
    expect(skipped?.detail).toBeNull();
    expect(skipped?.language).toBeNull();
  });

  test("unresolvedForFiles returns the unbound call sites of the given files", () => {
    expect(unresolvedForFiles(reader, [SRC_A]).map((row) => row.rawTarget)).toEqual([
      "external.thing",
    ]);
    expect(unresolvedForFiles(reader, [SRC_A])[0]?.path).toBe("src/a.ts");
    expect(unresolvedForFiles(reader, [SRC_A])[0]?.evidenceKind).toBe("dynamic");
    expect(unresolvedForFiles(reader, [SRC_B])).toEqual([]);
    expect(unresolvedForFiles(reader, [])).toEqual([]);
  });
});

describe("read boundary", () => {
  test("no Drizzle row type is named in these modules", () => {
    for (const name of READ_MODULES) {
      const source = readFileSync(join(READ_DIR, name), "utf8");
      expect(withoutComments(source).includes("$inferSelect")).toBe(false);
    }
  });

  test("a warm primitive builds no statement and misses no cache entry", () => {
    // Warm every shape this test repeats, so the second pass must be pure cache hits.
    findByQn(reader, "src/a.ts::Beta");
    findByName(reader, "alpha", ["method"]);
    metricsFor(reader, BETA);
    rowsForPaths(reader, ["src"]);
    const warm = statementCacheStats(reader);

    findByQn(reader, "src/a.ts::Beta");
    findByName(reader, "alpha", ["method"]);
    metricsFor(reader, BETA);
    rowsForPaths(reader, ["src"]);
    const after = statementCacheStats(reader);

    expect(after.misses).toBe(warm.misses);
    expect(after.hits).toBeGreaterThan(warm.hits);
    expect(after.size).toBeLessThanOrEqual(after.capacity);
  });
});
