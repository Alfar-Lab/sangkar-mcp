import { describe, expect, test } from "bun:test";
import { loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import { log } from "#shared/log";
import type { ToolContext } from "#shared/store/read/context";
import type {
  GraphReader,
  IndexedFileRow,
  NeighbourEdge,
  NodeRow,
  UnresolvedRef,
} from "#shared/store/read/reader";
import {
  argsHashOf,
  handle,
  type TracePayload,
  type TraceResponse,
} from "../src/tools/trace/handler";
import { DEFAULT_BUDGET_TOKENS, input, manifest } from "../src/tools/trace/manifest";

/**
 * `trace` over a hand-written `GraphReader` (`tests/read-files.test.ts` is the seam this copies):
 * no database, no filesystem, no network. What is asserted here is the tool's own policy — the fixed
 * group order, `tier` hoisted onto the group, `hop` per item, ambiguity that never becomes an answer,
 * near misses instead of a bare zero, and a cursor page boundary that loses and repeats nothing.
 *
 * The fixture is deliberately dense enough to truncate at a small budget: one seed with two inbound
 * relations and four outbound ones, a second hop, an external node, a test node and one unbound call
 * site, so every claim the payload makes has a row behind it.
 */

const GENERATION = "g-1";
const KEY = "trace-test-cursor-key-0123456789abcdef";
const NOW = new Date("2026-01-04T00:00:00Z");

function makeNode(spec: {
  id: number;
  qn: string;
  kind: string;
  name: string;
  signature?: string | null;
  isTest?: boolean;
  startLine?: number;
  endLine?: number;
}): NodeRow {
  return {
    id: spec.id,
    fileId: spec.id,
    path: spec.qn.slice(0, spec.qn.indexOf("::")),
    parentId: null,
    kind: spec.kind,
    subkind: null,
    name: spec.name,
    qn: spec.qn,
    startLine: spec.startLine ?? 1,
    endLine: spec.endLine ?? 2,
    signature: spec.signature ?? null,
    docstring: null,
    isTest: spec.isTest ?? false,
    props: {},
  };
}

const ALPHA = makeNode({
  id: 1,
  qn: "src/a.ts::alpha",
  kind: "function",
  name: "alpha",
  signature: "function alpha(): void",
  startLine: 3,
  endLine: 9,
});

/** Two distinct nodes with the same bare name: the tie that must come back as `ambiguous`. */
const RUN_B = makeNode({ id: 2, qn: "src/b.ts::run", kind: "function", name: "run" });
const RUN_C = makeNode({ id: 3, qn: "src/c.ts::run", kind: "method", name: "run" });

/** A symbol reached only by a segment of the query, so `not_found` still has something to say. */
const GHOST = makeNode({ id: 4, qn: "src/ghost.ts::missing", kind: "function", name: "missing" });

const CALLER = makeNode({ id: 10, qn: "src/d.ts::caller", kind: "function", name: "caller" });
const TEST_CALLER = makeNode({
  id: 11,
  qn: "tests/a.test.ts::alpha_is_stable",
  kind: "test",
  name: "alpha_is_stable",
  isTest: true,
});

const CALLEE = makeNode({ id: 20, qn: "src/e.ts::callee", kind: "function", name: "callee" });
/** `qn` without a `::` is how `expand` recognises a node outside this repository. */
const BUILTIN = makeNode({ id: 21, qn: "builtins.dict", kind: "function", name: "dict" });
const DEEP = makeNode({ id: 22, qn: "src/h.ts::deep", kind: "function", name: "deep" });
const REFERENCED = makeNode({ id: 30, qn: "src/f.ts::shape", kind: "class", name: "shape" });
const IMPORTED = makeNode({ id: 40, qn: "src/g.ts::helpers", kind: "module", name: "helpers" });

/** Enough same-group neighbours that a small budget has to page instead of fitting everything. */
const HELPERS: readonly NodeRow[] = Array.from({ length: 8 }, (_, index) =>
  makeNode({
    id: 100 + index,
    qn: `src/helpers.ts::helper_${index}`,
    kind: "function",
    name: `helper_${index}`,
  }),
);

function edge(
  id: number,
  srcId: number,
  dstId: number,
  kind: string,
  tier = "extracted",
  line = 1,
): NeighbourEdge["edge"] {
  return { id, srcId, dstId, kind, tier, fileId: 1, line, localName: "", evidence: null };
}

function inboundOf(seed: number): NeighbourEdge[] {
  if (seed !== ALPHA.id) {
    return [];
  }
  return [
    { edge: edge(1, CALLER.id, ALPHA.id, "calls", "extracted", 12), node: CALLER },
    { edge: edge(2, TEST_CALLER.id, ALPHA.id, "tested_by", "extracted", 4), node: TEST_CALLER },
  ];
}

function outboundOf(seed: number): NeighbourEdge[] {
  if (seed === CALLEE.id) {
    return [{ edge: edge(3, CALLEE.id, DEEP.id, "calls", "extracted", 5), node: DEEP }];
  }
  if (seed !== ALPHA.id) {
    return [];
  }
  return [
    { edge: edge(4, ALPHA.id, CALLEE.id, "calls", "extracted", 20), node: CALLEE },
    { edge: edge(5, ALPHA.id, BUILTIN.id, "calls", "inferred", 21), node: BUILTIN },
    ...HELPERS.map((helper, index) => ({
      edge: edge(200 + index, ALPHA.id, helper.id, "calls", "extracted", 30 + index),
      node: helper,
    })),
    { edge: edge(50, ALPHA.id, REFERENCED.id, "references", "extracted", 40), node: REFERENCED },
    { edge: edge(51, ALPHA.id, IMPORTED.id, "imports", "extracted", 41), node: IMPORTED },
  ];
}

const FILE_A: IndexedFileRow = {
  id: ALPHA.fileId,
  path: "src/a.ts",
  language: "typescript",
  sha256: "a".repeat(64),
  size: 200,
  lineCount: 40,
  parseStatus: "ok",
  parseErrorRanges: null,
  indexedGeneration: GENERATION,
};

const UNBOUND: UnresolvedRef = {
  id: 1,
  fileId: FILE_A.id,
  path: "src/a.ts",
  line: 7,
  rawTarget: "mystery",
  evidenceKind: "receiver",
  evidence: "receiver type unknown",
};

interface FakeOptions {
  /** Omit the unbound call site, so the scope may claim an exact total. */
  readonly clean?: boolean;
  /** An indexed file written by another publish: the answer is about a stale file. */
  readonly indexedGeneration?: string;
  /** How many unbound call sites the file scope holds; enough of them out-bid a page. */
  readonly unresolvedRows?: number;
}

function makeReader(options: FakeOptions = {}): GraphReader {
  const file: IndexedFileRow = {
    ...FILE_A,
    indexedGeneration: options.indexedGeneration ?? GENERATION,
  };
  const unresolved: UnresolvedRef[] =
    options.clean === true
      ? []
      : options.unresolvedRows === undefined
        ? [UNBOUND]
        : Array.from({ length: options.unresolvedRows }, (_, index) => ({
            ...UNBOUND,
            id: index + 1,
            line: 7 + index,
          }));
  return {
    findByQn: (qn) => (qn === ALPHA.qn ? ALPHA : null),
    findByName: (name, kinds) => {
      const matches = (nodes: readonly NodeRow[]): NodeRow[] =>
        kinds === undefined || kinds.length === 0
          ? [...nodes]
          : nodes.filter((node) => kinds.includes(node.kind));
      switch (name) {
        case "alpha":
          return matches([ALPHA]);
        case "run":
          return matches([RUN_B, RUN_C]);
        case "missing":
          return matches([GHOST]);
        default:
          return [];
      }
    },
    members: () => [],
    nodesByFile: () => [],
    inbound: (nodeId) => inboundOf(nodeId),
    outbound: (nodeId) => outboundOf(nodeId),
    metricsFor: () => null,
    rowsForPaths: (prefixes) =>
      prefixes.includes("src/a.ts")
        ? { indexed: [file], notIndexed: [] }
        : { indexed: [], notIndexed: [] },
    unresolvedForFiles: (fileIds) => unresolved.filter((row) => fileIds.includes(row.fileId)),
    fileByPath: (path) => (path === file.path ? file : null),
    allFiles: () => [file],
    censusByLanguage: () => [{ language: "typescript", files: 1, parsed: 1, failed: 0 }],
    metaFor: () => ({
      projectName: "fixture",
      projectRoot: "/tmp/fixture",
      generation: GENERATION,
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
}

function makeContext(reader: GraphReader): ToolContext {
  return {
    reader,
    cursor: createCursors({ key: KEY }),
    log,
    now: () => NOW,
    config: loadConfig({
      env: { CODEGRAPH_REPO_ROOT: "/tmp/fixture", CODEGRAPH_CURSOR_KEY: KEY },
    }),
  };
}

const ctx = makeContext(makeReader());

function payloadOf(envelope: TraceResponse): TracePayload {
  // The ok branch is the only one that may carry a node and neighbour groups; asserting the status
  // first is what makes this cast safe and what makes a wrong status fail loudly.
  expect(envelope.status).toBe("ok");
  return envelope.payload as TracePayload;
}

function allItems(payload: TracePayload): { id: number; hop: number }[] {
  return [...payload.in, ...payload.out].flatMap((group) =>
    group.items.map((item) => ({ id: item.id, hop: item.hop })),
  );
}

describe("manifest", () => {
  test("name, profile and the verbatim description live in one place", () => {
    expect(manifest.name).toBe("trace");
    expect(manifest.profile).toBe("agent");
    expect(manifest.description).toContain("Show what is connected to one symbol");
    expect(manifest.description).toContain("Does not return source code.");
    expect(manifest.description).toContain('returns `status: "ambiguous"` with candidates');
  });

  test("the six-argument schema carries the documented defaults", () => {
    expect(input.query).toEqual({ type: "string", required: true });
    expect(input.direction.values).toEqual(["all", "in", "out"]);
    expect(input.direction.default).toBe("all");
    expect(input.depth).toMatchObject({ type: "integer", default: 2, min: 1, max: 4 });
    expect(input.budget).toEqual({ type: "integer", default: DEFAULT_BUDGET_TOKENS });
    expect(DEFAULT_BUDGET_TOKENS).toBe(3200);
    expect(input.cursor).toEqual({ type: "string" });
    expect("kinds" in input).toBe(true);
  });
});

describe("trace: a unique name resolves and its neighbours are grouped", () => {
  test("the resolution block and the node come from the resolved row", async () => {
    const envelope = await handle(ctx, { query: "alpha" });
    const payload = payloadOf(envelope);
    expect(payload.resolution).toEqual({
      query: "alpha",
      matched: "src/a.ts::alpha",
      kind: "function",
      how: "exact name, 1 of 1 candidate",
      confidence: "extracted",
    });
    expect(payload.node).toEqual({
      id: ALPHA.id,
      qn: "src/a.ts::alpha",
      kind: "function",
      file: "src/a.ts",
      lines: [3, 9],
      signature: "function alpha(): void",
      is_test: false,
    });
  });

  test("groups follow the fixed edge order and hoist tier onto the group", async () => {
    const payload = payloadOf(await handle(ctx, { query: "alpha" }));
    expect(payload.in.map((group) => [group.edge, group.tier])).toEqual([
      ["calls", "extracted"],
      ["tested_by", "extracted"],
    ]);
    // `calls` before `references` before `imports` is the contract's order, not alphabetical.
    expect(payload.out.map((group) => [group.edge, group.tier])).toEqual([
      ["calls", "extracted"],
      ["calls", "inferred"],
      ["references", "extracted"],
      ["imports", "extracted"],
    ]);
  });

  test("every item carries a hop, and only an external or test neighbour is labelled", async () => {
    const payload = payloadOf(await handle(ctx, { query: "alpha" }));
    const items = allItems(payload);
    expect(items.length).toBeGreaterThan(0);
    expect(items.every((item) => item.hop >= 1)).toBe(true);
    // The second hop is reached only through `callee`, and the group still keeps the hop per item.
    expect(items.find((item) => item.id === DEEP.id)?.hop).toBe(2);
    expect(items.find((item) => item.id === CALLEE.id)?.hop).toBe(1);
    // The builtin is an `inferred` call while the plain callee is an `extracted` one, so they sit in
    // two groups that share an edge kind: the tier, not the edge, is what separates them.
    const inferred = payload.out.find((group) => group.tier === "inferred");
    const builtin = inferred?.items.find((item) => item.id === BUILTIN.id);
    expect(builtin?.external).toBe(true);
    expect(builtin?.is_test).toBeUndefined();
    const testItem = payload.in.find((group) => group.edge === "tested_by")?.items[0];
    expect(testItem?.is_test).toBe(true);
    expect(testItem?.external).toBeUndefined();
    const plain = payload.in.find((group) => group.edge === "calls")?.items[0];
    expect(plain?.is_test).toBeUndefined();
    expect(plain?.external).toBeUndefined();
  });

  test("unresolved rows in scope are reported and made visible in coverage and totals", async () => {
    const envelope = await handle(ctx, { query: "alpha" });
    const payload = payloadOf(envelope);
    expect(payload.unresolved).toEqual([
      {
        file: "src/a.ts",
        line: 7,
        name: "mystery",
        evidence: "receiver type unknown",
        tier: "unresolved",
      },
    ]);
    expect(payload.not_indexed).toEqual([]);
    expect(envelope.coverage).toMatchObject({
      complete: true,
      indexed: 1,
      skipped: 0,
      failed: 0,
      unresolved_edges: 1,
    });
    // An unbound call site makes an exact caller count a lie, so the relation is a lower bound.
    expect(envelope.totals.total_relation).toBe("gte");
    expect(envelope.totals.callers_total).toBe(2);
    // 13 callees: the direct call, the builtin, eight same-group helpers, the second hop, a
    // referenced class and an imported module.
    expect(envelope.totals.callees_total).toBe(13);
  });

  test("the read status names the generation and records that freshness was checked", async () => {
    const envelope = await handle(ctx, { query: "alpha" });
    expect(envelope.read_status).toEqual({
      generation: GENERATION,
      indexed_at: "2026-01-03T00:00:00Z",
      stale: false,
      stale_checked_at: NOW.toISOString(),
    });
    expect(envelope.truncated).toBe(false);
    expect(envelope.next).toBeNull();
    expect(envelope.next_tools).toEqual(["expand"]);
    expect(envelope.summary).toContain("2 callers, 13 callees");
  });

  test("a clean, fresh scope is allowed to claim an exact total", async () => {
    const cleanCtx = makeContext(makeReader({ clean: true }));
    const envelope = await handle(cleanCtx, { query: "alpha" });
    expect(envelope.totals.total_relation).toBe("eq");
    expect(payloadOf(envelope).unresolved).toEqual([]);
  });

  test("a file written by another publish makes the answer stale", async () => {
    const oldCtx = makeContext(makeReader({ indexedGeneration: "g-0" }));
    const envelope = await handle(oldCtx, { query: "alpha" });
    expect(envelope.read_status.stale).toBe(true);
    expect(envelope.read_status.stale_reason).toContain("was last written by generation g-0");
    expect(envelope.totals.total_relation).toBe("gte");
  });
});

describe("trace: direction narrows the walk without inventing zeros", () => {
  test("direction in walks callers only and omits callees_total", async () => {
    const envelope = await handle(ctx, { query: "alpha", direction: "in" });
    const payload = payloadOf(envelope);
    expect(payload.out).toEqual([]);
    expect(payload.in.length).toBe(2);
    expect(envelope.totals.callers_total).toBe(2);
    expect(envelope.totals.callees_total).toBeUndefined();
    expect(envelope.summary).toContain("2 callers");
    expect(envelope.summary).not.toContain("callees");
  });

  test("direction out walks callees only and omits callers_total", async () => {
    const envelope = await handle(ctx, { query: "alpha", direction: "out" });
    const payload = payloadOf(envelope);
    expect(payload.in).toEqual([]);
    expect(envelope.totals.callees_total).toBe(13);
    expect(envelope.totals.callers_total).toBeUndefined();
  });

  test("depth 1 stops at the direct neighbours", async () => {
    const payload = payloadOf(await handle(ctx, { query: "alpha", depth: 1 }));
    expect(allItems(payload).find((item) => item.id === DEEP.id)).toBeUndefined();
    expect(allItems(payload).find((item) => item.id === CALLEE.id)?.hop).toBe(1);
  });
});

describe("trace: ambiguity and absence are answers, not guesses", () => {
  test("two tied candidates come back as ambiguous with a capped candidate list and no body", async () => {
    const envelope = await handle(ctx, { query: "run" });
    expect(envelope.status).toBe("ambiguous");
    const payload = envelope.payload as { candidates?: { qn: string; score: number }[] };
    expect(payload.candidates?.map((candidate) => candidate.qn)).toEqual([
      "src/b.ts::run",
      "src/c.ts::run",
    ]);
    expect(payload.candidates?.length).toBeLessThanOrEqual(5);
    // No answer body: an ambiguous envelope must not also carry a node or neighbours.
    expect("node" in payload).toBe(false);
    expect("in" in payload).toBe(false);
    expect("out" in payload).toBe(false);
    expect(envelope.summary).toContain("2 candidates");
  });

  test("kinds narrows resolution, so a tie across kinds can resolve", async () => {
    const tied = await handle(ctx, { query: "run" });
    expect(tied.status).toBe("ambiguous");
    const narrowed = await handle(ctx, { query: "run", kinds: ["method"] });
    expect(narrowed.status).toBe("ok");
    expect(payloadOf(narrowed).node.qn).toBe("src/c.ts::run");
  });

  test("no match returns near misses and never a bare zero", async () => {
    const envelope = await handle(ctx, { query: "missing.thing" });
    expect(envelope.status).toBe("not_found");
    const payload = envelope.payload as unknown as { near_misses: { qn: string; score: number }[] };
    expect(payload.near_misses.map((miss) => miss.qn)).toEqual(["src/ghost.ts::missing"]);
    expect(envelope.totals.total_relation).toBe("gte");
    expect(envelope.coverage.complete).toBe(false);
  });
});

/** 300 tokens is smaller than this fixture's whole payload; the first page cannot hold it all. */
const SMALL_BUDGET = 600;

describe("trace: a truncated page resumes on the flattened sequence", () => {
  async function pages(query: string, budget: number): Promise<TraceResponse[]> {
    const collected: TraceResponse[] = [];
    let cursor: string | null = null;
    // Bounded so a cursor that failed to advance fails the test instead of hanging it.
    for (let step = 0; step < 20; step += 1) {
      const args = cursor === null ? { query, budget } : { query, budget, cursor };
      const envelope = await handle(ctx, args);
      collected.push(envelope);
      cursor = envelope.next;
      if (cursor === null) {
        return collected;
      }
    }
    return collected;
  }

  test("the first page is cut, mints a next, and keeps the relation a lower bound", async () => {
    const first = await handle(ctx, { query: "alpha", budget: SMALL_BUDGET });
    expect(first.status).toBe("ok");
    expect(first.truncated).toBe(true);
    expect(first.truncation_reason).toBe("budget");
    expect(typeof first.next).toBe("string");
    expect(first.totals.total_relation).toBe("gte");
    // Totals are the relation's totals, not the page's: truncation never rewrites them.
    expect(first.totals.callers_total).toBe(2);
    expect(first.totals.callees_total).toBe(13);
    expect(first.summary).toContain("neighbours shown");
    expect(allItems(payloadOf(first)).length).toBeGreaterThan(0);
    expect(allItems(payloadOf(first)).length).toBeLessThan(15);
  });

  test("the resumed pages continue the sequence: no duplicates and every neighbour delivered once", async () => {
    const whole = allItems(payloadOf(await handle(ctx, { query: "alpha" }))).map((item) => item.id);
    const paged = await pages("alpha", SMALL_BUDGET);
    expect(paged.length).toBeGreaterThan(1);
    for (const envelope of paged) {
      expect(envelope.status).toBe("ok");
    }
    const delivered = paged.flatMap((envelope) =>
      allItems(payloadOf(envelope)).map((item) => item.id),
    );
    expect(new Set(delivered).size).toBe(delivered.length);
    // Every neighbour of the untruncated answer appears exactly once across the pages.
    expect([...delivered].sort((a, b) => a - b)).toEqual([...whole].sort((a, b) => a - b));
    // The last page is the only one without a continuation.
    expect(paged[paged.length - 1]?.next).toBeNull();
  });

  test("a resumed page keeps the group order and the per-item hop", async () => {
    const first = await handle(ctx, { query: "alpha", budget: SMALL_BUDGET });
    const next = first.next ?? "";
    const second = await handle(ctx, { query: "alpha", budget: SMALL_BUDGET, cursor: next });
    const payload = payloadOf(second);
    const groups = [...payload.in, ...payload.out].map((group) => group.edge);
    const rank = (edge: string): number =>
      ["calls", "inherits", "implements", "references", "tested_by", "imports"].indexOf(edge);
    expect(groups).toEqual([...groups].sort((a, b) => rank(a) - rank(b)));
    expect(allItems(payload).every((item) => item.hop >= 1)).toBe(true);
  });
});

describe("trace: a page always advances on a dense neighbour sequence", () => {
  test("the second page carries a different token and shares no row with the first", async () => {
    // The dense case in miniature: the evidence list is bigger than the page it competes with, which
    // is what let `emit` empty the neighbour groups and mint a token at the position it was given.
    const dense = makeContext(makeReader({ unresolvedRows: 160 }));
    const whole = allItems(payloadOf(await handle(dense, { query: "alpha" }))).map(
      (item) => item.id,
    );
    const first = await handle(dense, { query: "alpha", budget: SMALL_BUDGET });
    const firstIds = allItems(payloadOf(first)).map((item) => item.id);
    expect(first.truncated).toBe(true);
    expect(typeof first.next).toBe("string");
    expect(firstIds.length).toBeGreaterThan(0);

    const second = await handle(dense, {
      query: "alpha",
      budget: SMALL_BUDGET,
      cursor: first.next ?? "",
    });
    const secondIds = allItems(payloadOf(second)).map((item) => item.id);
    expect(secondIds.length).toBeGreaterThan(0);
    expect(second.truncated).toBe(true);

    // The defect: a page whose token repeats the position it was given re-serves itself forever.
    expect(typeof second.next).toBe("string");
    expect(second.next).not.toBe(first.next);
    expect(secondIds.filter((id) => firstIds.includes(id))).toEqual([]);

    // Contiguous, in sequence order: the two pages are the first and second slice of the whole.
    expect(firstIds).toEqual(whole.slice(0, firstIds.length));
    expect(secondIds).toEqual(whole.slice(firstIds.length, firstIds.length + secondIds.length));
    expect(first.summary).toContain("neighbours shown");
    expect(second.summary).toContain("neighbours shown");
    expect(second.totals.total_relation).toBe("gte");
  });

  test("unresolved evidence is capped so the neighbour rows still reach the page", async () => {
    // 160 unbound call sites in one file: uncapped, the evidence list alone is the whole budget,
    // which is how the neighbour groups were emptied while the payload still claimed to be a page.
    const noisy = makeContext(makeReader({ unresolvedRows: 160 }));
    const envelope = await handle(noisy, { query: "alpha", budget: SMALL_BUDGET });
    const payload = payloadOf(envelope);
    expect(envelope.coverage.unresolved_edges).toBe(160);
    expect(payload.unresolved.length).toBeLessThan(160);
    expect(payload.unresolved.length).toBeGreaterThan(0);
    expect(allItems(payload).length).toBeGreaterThan(0);
    expect(envelope.truncated).toBe(true);
    expect(envelope.totals.total_relation).toBe("gte");
  });

  test("a budget below one neighbour terminates honestly instead of minting a token", async () => {
    const envelope = await handle(ctx, { query: "alpha", budget: 120 });
    expect(envelope.status).toBe("ok");
    // A zero-row page is only honest as the last one: the cut is at the envelope's floor.
    expect(envelope.truncated).toBe(true);
    expect(envelope.truncation_reason).toBe("budget");
    expect(envelope.next).toBeNull();
    expect(allItems(payloadOf(envelope)).length).toBe(0);
    expect(envelope.summary).toContain("does not fit budget 120 tokens");
    // The cheaper query is the route it names; `expand` cannot stand in for a neighbour list.
    expect(envelope.next_tools).toContain("trace");
    expect(envelope.next_tools).not.toContain("expand");
    expect(envelope.totals.total_relation).toBe("gte");
  });
});

describe("trace: a cursor is refused rather than silently re-served", () => {
  test("a tampered token is cursor_invalid", async () => {
    const envelope = await handle(ctx, { query: "alpha", cursor: "not-a-cursor" });
    expect(envelope.status).toBe("error");
    expect(envelope.error?.code).toBe("cursor_invalid");
    expect(envelope.error?.hint).toContain("re-run the original call");
    expect(envelope.payload).toBeNull();
  });

  test("a token from another generation is cursor_expired", async () => {
    const stale = ctx.cursor.sign({
      generation: "g-0",
      tool: "trace",
      argsHash: argsHashOf({ query: "alpha" }),
      position: 0,
    });
    const envelope = await handle(ctx, { query: "alpha", cursor: stale });
    expect(envelope.error?.code).toBe("cursor_expired");
    expect(envelope.error?.hint).toBe("re-run the original call");
  });

  test("a token minted for another query is cursor_mismatch", async () => {
    const first = await handle(ctx, { query: "alpha", budget: SMALL_BUDGET });
    const envelope = await handle(ctx, { query: "alpha", budget: 200, cursor: first.next ?? "" });
    expect(envelope.error?.code).toBe("cursor_mismatch");
    expect(envelope.error?.hint).toContain("same tool and arguments");
  });

  test("a token from another tool is refused too", async () => {
    const foreign = ctx.cursor.sign({
      generation: GENERATION,
      tool: "search",
      argsHash: argsHashOf({ query: "alpha" }),
      position: 0,
    });
    const envelope = await handle(ctx, { query: "alpha", cursor: foreign });
    expect(envelope.error?.code).toBe("cursor_mismatch");
  });

  test("an unpublished graph says so instead of claiming the symbol does not exist", async () => {
    const reader: GraphReader = {
      ...makeReader(),
      metaFor: () => ({
        projectName: null,
        projectRoot: null,
        generation: null,
        indexedAt: null,
        indexerState: "idle",
        consecutiveFailures: 0,
      }),
    };
    const envelope = await handle(makeContext(reader), { query: "alpha" });
    expect(envelope.status).toBe("error");
    expect(envelope.error?.code).toBe("bootstrapping");
    expect(envelope.error?.retry_after_ms).toBeGreaterThan(0);
    expect(envelope.payload).toBeNull();
  });
});

describe("trace: the argsHash pins the call, not the cursor", () => {
  test("an implicit default hashes like an explicit one", () => {
    expect(argsHashOf({ query: "alpha" })).toBe(
      argsHashOf({ query: "alpha", direction: "all", depth: 2, budget: DEFAULT_BUDGET_TOKENS }),
    );
    expect(argsHashOf({ query: "alpha" })).not.toBe(
      argsHashOf({ query: "alpha", direction: "in" }),
    );
    expect(argsHashOf({ query: "alpha" })).toMatch(/^sha256:[0-9a-f]{64}$/);
  });
});
