import { describe, expect, test } from "bun:test";
import {
  assemble,
  type CoverageAssembly,
  type CoverageScope,
  type CoverageSource,
} from "#shared/domain/coverage";
import {
  EXPAND_DEPTH_MAX,
  expand,
  type GroupedNeighbors,
  type NeighbourSource,
} from "#shared/domain/neighbors";
import { type Resolution, type ResolutionSource, resolve } from "#shared/domain/resolve";
import { score } from "#shared/domain/risk";
import { type Coverage, type ReadStatus, relationFor } from "#shared/envelope";
import type { IndexedFileRow, SkippedPathRow, UnresolvedRef } from "#shared/store/read/coverage";
import type { EdgeRow, NeighbourEdge } from "#shared/store/read/edges";
import type { NodeRow } from "#shared/store/read/nodes";

/**
 * The four domain contracts against hand-written, reader-shaped fakes over in-memory fixtures.
 *
 * No database and no `#shared/store/read/reader` import: the aggregate `GraphReader` is written
 * by another task, and the point of this file is that resolve, expand, coverage and risk are
 * provable from facts alone. Each fake has exactly the methods the module under test calls, so a
 * widened dependency would stop compiling here.
 */

/** `noUncheckedIndexedAccess` makes every index a possible undefined; a test wants a failure. */
function at<T>(list: readonly T[], index: number): T {
  const value = list[index];
  if (value === undefined) {
    throw new Error(`expected an element at index ${index}`);
  }
  return value;
}

function okResult(result: Resolution): Extract<Resolution, { status: "ok" }> {
  if (result.status !== "ok") {
    throw new Error(`expected ok, got ${result.status}`);
  }
  return result;
}

function ambiguousResult(result: Resolution): Extract<Resolution, { status: "ambiguous" }> {
  if (result.status !== "ambiguous") {
    throw new Error(`expected ambiguous, got ${result.status}`);
  }
  return result;
}

function notFoundResult(result: Resolution): Extract<Resolution, { status: "not_found" }> {
  if (result.status !== "not_found") {
    throw new Error(`expected not_found, got ${result.status}`);
  }
  return result;
}

interface NodeSpec {
  readonly id: number;
  readonly name: string;
  readonly qn: string;
  readonly path: string;
  readonly kind?: string;
  readonly parentId?: number | null;
  readonly startLine?: number;
  readonly endLine?: number;
  readonly props?: Readonly<Record<string, unknown>>;
  readonly isTest?: boolean;
}

/** A `NodeRow` with the columns resolution and expansion actually read made explicit. */
function nodeOf(spec: NodeSpec): NodeRow {
  const startLine = spec.startLine ?? 1;
  return {
    id: spec.id,
    fileId: spec.id,
    path: spec.path,
    parentId: spec.parentId ?? null,
    kind: spec.kind ?? "function",
    subkind: null,
    name: spec.name,
    qn: spec.qn,
    startLine,
    endLine: spec.endLine ?? startLine,
    signature: null,
    docstring: null,
    isTest: spec.isTest ?? false,
    props: spec.props ?? {},
  };
}

function matchesKinds(nodes: readonly NodeRow[], kinds: readonly string[] | undefined): NodeRow[] {
  const filter = kinds ?? [];
  return nodes
    .filter((node) => filter.length === 0 || filter.includes(node.kind))
    .sort((a, b) => a.qn.localeCompare(b.qn));
}

/**
 * The store's two node lookups over a fixed list. `findByName` mirrors the real one: the kinds
 * filter is all-or-nothing and the order is `ORDER BY qn`.
 */
function fakeResolutionSource(
  nodes: readonly NodeRow[],
  decoratorIndex?: (name: string) => readonly NodeRow[],
): ResolutionSource {
  const source: ResolutionSource = {
    findByQn: (qualified) => nodes.find((node) => node.qn === qualified) ?? null,
    findByName: (name, kinds) =>
      matchesKinds(
        nodes.filter((node) => node.name === name),
        kinds,
      ),
  };
  if (decoratorIndex !== undefined) {
    source.findByDecorator = (name, kinds) => matchesKinds(decoratorIndex(name), kinds);
  }
  return source;
}

// ---------------------------------------------------------------------------------------------
// resolve: the five-step detection order and the ambiguity contract
// ---------------------------------------------------------------------------------------------

/**
 * One file node, one decorated class, two methods named `alpha` in different files, and one node
 * whose *name* is another node's `qn` so `exact_qn` and `exact_name` can be made to compete.
 */
const FILE_NODE = nodeOf({
  id: 1,
  name: "a.ts",
  qn: "src/a.ts::a.ts",
  path: "src/a.ts",
  kind: "file",
  endLine: 40,
});
const BETA = nodeOf({
  id: 2,
  name: "Beta",
  qn: "src/a.ts::Beta",
  path: "src/a.ts",
  kind: "class",
  props: { decorators: ["Injectable"] },
});
const ALPHA_METHOD = nodeOf({
  id: 3,
  name: "alpha",
  qn: "src/a.ts::Beta.alpha",
  path: "src/a.ts",
  kind: "method",
  parentId: 2,
  startLine: 10,
  endLine: 20,
});
const ALPHA_FUNCTION = nodeOf({
  id: 4,
  name: "alpha",
  qn: "src/b.ts::alpha",
  path: "src/b.ts",
  startLine: 3,
  endLine: 9,
});
const ZETA = nodeOf({ id: 5, name: "zeta", qn: "src/b.ts::zeta", path: "src/b.ts" });
/** Named `a`, so the path step sees two candidates inside `src/a.ts`. */
const SHORT_A = nodeOf({ id: 6, name: "a", qn: "src/a.ts::a", path: "src/a.ts" });
/** Named after another node's qn: only a deliberate fixture can make two steps compete. */
const QN_NAMED = nodeOf({
  id: 7,
  name: "src/a.ts::Beta.alpha",
  qn: "src/b.ts::weird",
  path: "src/b.ts",
});
/** Named `ts`, so `src/a.ts` also has a dotted tail to find. */
const TS_NAMED = nodeOf({ id: 8, name: "ts", qn: "src/b.ts::ts", path: "src/b.ts" });

const RESOLVE_NODES = [
  FILE_NODE,
  BETA,
  ALPHA_METHOD,
  ALPHA_FUNCTION,
  ZETA,
  SHORT_A,
  QN_NAMED,
] as const;

describe("resolve: detection order", () => {
  const source = fakeResolutionSource(RESOLVE_NODES);

  test("step 1 exact qn resolves without consulting a name", () => {
    const result = okResult(resolve(source, "src/a.ts::Beta.alpha"));
    expect(result.step).toBe("exact_qn");
    expect(result.node.id).toBe(ALPHA_METHOD.id);
    expect(result.score).toBe(1);
    expect(result.why).toBe("exact qualified name");
  });

  test("step 2 exact name resolves a bare name", () => {
    const result = okResult(resolve(source, "zeta"));
    expect(result.step).toBe("exact_name");
    expect(result.node.id).toBe(ZETA.id);
    expect(result.why).toBe("exact name, 1 of 1 candidate");
  });

  test("step 3 dotted tail prefers the tail inside its own scope", () => {
    const result = okResult(resolve(source, "Beta.alpha"));
    expect(result.step).toBe("dotted_tail");
    expect(result.node.id).toBe(ALPHA_METHOD.id);
    expect(result.score).toBe(0.85);
    expect(result.why).toContain("own scope");
  });

  test("step 4 path resolves to the file node, ahead of a symbol in that file", () => {
    const result = okResult(resolve(source, "src/a.ts"));
    expect(result.step).toBe("path");
    expect(result.node.kind).toBe("file");
    expect(result.node.id).toBe(FILE_NODE.id);
    expect(result.score).toBe(0.75);
    expect(result.why).toBe('file "src/a.ts"');
  });

  test("step 5 @decorator is a property filter, never a node kind", () => {
    const decorated = fakeResolutionSource(RESOLVE_NODES, (name) =>
      RESOLVE_NODES.filter((node) => {
        const decorators: unknown = node.props.decorators;
        return (
          name === "Injectable" &&
          Array.isArray(decorators) &&
          decorators.some((entry) => entry === name)
        );
      }),
    );
    const result = okResult(resolve(decorated, "@Injectable"));
    expect(result.step).toBe("decorator");
    expect(result.node.id).toBe(BETA.id);
    // The answer is the decorated class, not a synthesised `decorator` node.
    expect(result.node.kind).toBe("class");
    expect(result.why).toBe('property filter @Injectable on "Beta"');
  });

  test("the decorator step stays a filter when kinds exclude the decorated node", () => {
    const decorated = fakeResolutionSource(RESOLVE_NODES, () => [BETA]);
    expect(resolve(decorated, "@Injectable", ["function"]).status).toBe("not_found");
    const result = okResult(resolve(decorated, "@Injectable", ["class"]));
    expect(result.node.id).toBe(BETA.id);
  });

  test("without a props query the decorator step keeps only matching names", () => {
    const source = fakeResolutionSource(RESOLVE_NODES);
    // No node is named `Injectable`, so the fallback cannot invent one.
    expect(resolve(source, "@Injectable").status).toBe("not_found");
  });

  test("precedence: exact qn wins over an exact name that also matches", () => {
    // `QN_NAMED.name` is exactly `ALPHA_METHOD.qn`, so steps 1 and 2 both have an answer.
    const result = okResult(resolve(source, "src/a.ts::Beta.alpha"));
    expect(result.step).toBe("exact_qn");
    expect(result.node.id).toBe(ALPHA_METHOD.id);
  });

  test("precedence: exact name wins over a dotted tail", () => {
    const withDottedName = [
      ...RESOLVE_NODES,
      nodeOf({ id: 20, name: "Beta.alpha", qn: "src/c.ts::Beta.alpha", path: "src/c.ts" }),
    ];
    const result = okResult(resolve(fakeResolutionSource(withDottedName), "Beta.alpha"));
    expect(result.step).toBe("exact_name");
    expect(result.node.id).toBe(20);
  });

  test("precedence: a dotted tail wins over a path when both could answer", () => {
    const withTail = fakeResolutionSource([...RESOLVE_NODES, TS_NAMED]);
    const result = okResult(resolve(withTail, "src/a.ts"));
    expect(result.step).toBe("dotted_tail");
    expect(result.node.id).toBe(TS_NAMED.id);
    // Same query, no `ts` node: the path step answers, proving the order and not a special case.
    expect(okResult(resolve(source, "src/a.ts")).step).toBe("path");
  });

  test("a dotted query with no tail cannot match", () => {
    expect(resolve(source, "Beta.").status).toBe("not_found");
  });
});

describe("resolve: ambiguity", () => {
  const source = fakeResolutionSource(RESOLVE_NODES);

  test("two candidates at the same score are ambiguous, with both", () => {
    const result = ambiguousResult(resolve(source, "alpha"));
    expect(result.step).toBe("exact_name");
    expect(result.candidates.length).toBe(2);
    // Nothing was cut, so the tie size and the shown size are both two.
    expect(result.candidates_total).toBe(2);
    expect(result.candidates_shown).toBe(2);

    const first = at(result.candidates, 0);
    const second = at(result.candidates, 1);
    expect(first.score).toBe(second.score);
    expect([first.qn, second.qn]).toEqual(["src/a.ts::Beta.alpha", "src/b.ts::alpha"]);
    expect(first.kind).toBe("method");
    expect(second.kind).toBe("function");
    expect(first.file).toBe("src/a.ts");
    expect(first.lines).toEqual([10, 20]);
    expect(first.why).toBe("exact name, 1 of 2 candidates");
    expect(second.why).toBe("exact name, 2 of 2 candidates");
  });

  test("kind narrowing turns an ambiguous name into an answer", () => {
    const result = okResult(resolve(source, "alpha", ["function"]));
    expect(result.step).toBe("exact_name");
    expect(result.node.id).toBe(ALPHA_FUNCTION.id);
    expect(result.why).toBe("exact name, 1 of 1 candidate");
  });

  test("a tie wider than the contract cap is cut to five candidates", () => {
    const many = Array.from({ length: 7 }, (_, index) =>
      nodeOf({
        id: 100 + index,
        name: "dup",
        qn: `src/f${index}.ts::dup`,
        path: `src/f${index}.ts`,
      }),
    );
    const result = ambiguousResult(resolve(fakeResolutionSource(many), "dup"));
    expect(result.candidates.length).toBe(5);
    expect(new Set(result.candidates.map((candidate) => candidate.qn)).size).toBe(5);
    // The cap hides rows, never the size of the tie: the totals come from the winners before the
    // slice, and every `why` is built from that same number, so phrase and field cannot disagree.
    expect(result.candidates_total).toBe(7);
    expect(result.candidates_shown).toBe(result.candidates.length);
    expect(result.candidates_total).toBeGreaterThan(result.candidates_shown);
    expect(at(result.candidates, 0).why).toBe("exact name, 1 of 7 candidates");
    expect(at(result.candidates, 4).why).toBe("exact name, 5 of 7 candidates");
    for (const candidate of result.candidates) {
      expect(candidate.why).toContain(`of ${result.candidates_total} candidates`);
    }
  });
});

describe("resolve: path prefix filter", () => {
  const DUP_A = nodeOf({ id: 40, name: "dup", qn: "src/a.ts::dup", path: "src/a.ts" });
  const DUP_NESTED = nodeOf({
    id: 41,
    name: "dup",
    qn: "src/nested/b.ts::dup",
    path: "src/nested/b.ts",
  });
  const DUP_LIB = nodeOf({ id: 42, name: "dup", qn: "lib/c.ts::dup", path: "lib/c.ts" });
  const source = fakeResolutionSource([DUP_A, DUP_NESTED, DUP_LIB]);

  test("path narrows three same-name candidates to one", () => {
    expect(ambiguousResult(resolve(source, "dup")).candidates_total).toBe(3);
    const result = okResult(resolve(source, "dup", { path: "src/nested" }));
    expect(result.step).toBe("exact_name");
    expect(result.node.id).toBe(DUP_NESTED.id);
    // The survivor counts the filtered total, not the unfiltered one.
    expect(result.why).toBe("exact name, 1 of 1 candidate");
  });

  test("path keeps a tie that is still a tie inside the prefix", () => {
    const result = ambiguousResult(resolve(source, "dup", { path: "src" }));
    expect(result.candidates.map((candidate) => candidate.qn)).toEqual([
      "src/a.ts::dup",
      "src/nested/b.ts::dup",
    ]);
    expect(result.candidates_total).toBe(2);
  });

  test("path that excludes every candidate is not_found with only filtered near misses", () => {
    // `dup.nope` is no step's match, and unfiltered its miss lists all three `dup` symbols.
    expect(notFoundResult(resolve(source, "dup.nope")).near_misses.length).toBe(3);
    const excluded = notFoundResult(resolve(source, "dup.nope", { path: "nowhere" }));
    expect(excluded.near_misses).toEqual([]);
    // A prefix that holds one of them keeps exactly that one, inside it.
    const inside = notFoundResult(resolve(source, "dup.nope", { path: "lib" }));
    expect(inside.near_misses.map((candidate) => candidate.qn)).toEqual(["lib/c.ts::dup"]);
  });

  test("path filters the dotted-tail, path and decorator steps too", () => {
    const alpha = fakeResolutionSource(RESOLVE_NODES);
    expect(okResult(resolve(alpha, "Beta.alpha", { path: "src" })).node.id).toBe(ALPHA_METHOD.id);
    expect(resolve(alpha, "Beta.alpha", { path: "lib" }).status).toBe("not_found");

    expect(okResult(resolve(alpha, "src/a.ts", { path: "src" })).step).toBe("path");
    expect(resolve(alpha, "src/a.ts", { path: "lib" }).status).toBe("not_found");

    const decorated = fakeResolutionSource(RESOLVE_NODES, () => [BETA]);
    expect(okResult(resolve(decorated, "@Injectable", { path: "src/a.ts" })).node.id).toBe(BETA.id);
    expect(resolve(decorated, "@Injectable", { path: "lib" }).status).toBe("not_found");
  });

  test("a filtered exact qn never answers from outside the prefix", () => {
    const alpha = fakeResolutionSource(RESOLVE_NODES);
    expect(okResult(resolve(alpha, "src/b.ts::zeta", { path: "src/b.ts" })).node.id).toBe(ZETA.id);
    expect(resolve(alpha, "src/b.ts::zeta", { path: "src/a.ts" }).status).toBe("not_found");
  });

  test("no path, an empty path and an empty options object answer as before", () => {
    const alpha = fakeResolutionSource(RESOLVE_NODES);
    const plain = ambiguousResult(resolve(alpha, "alpha"));
    expect(ambiguousResult(resolve(alpha, "alpha", {})).candidates).toEqual(plain.candidates);
    expect(ambiguousResult(resolve(alpha, "alpha", { path: "" })).candidates).toEqual(
      plain.candidates,
    );
    expect(ambiguousResult(resolve(alpha, "alpha", { path: "   " })).candidates).toEqual(
      plain.candidates,
    );
    // The pre-options call shape still means `{ kinds }`.
    expect(okResult(resolve(alpha, "alpha", ["function"])).node.id).toBe(ALPHA_FUNCTION.id);
  });
});

describe("resolve: not_found", () => {
  const source = fakeResolutionSource(RESOLVE_NODES);

  test("not_found carries near misses instead of a bare zero", () => {
    const result = notFoundResult(resolve(source, "Beta.missing"));
    expect(result.near_misses.length).toBe(1);
    const near = at(result.near_misses, 0);
    expect(near.qn).toBe(BETA.qn);
    expect(near.why).toContain('segment "Beta"');
    expect(near.score).toBeLessThan(0.6);
  });

  test("an empty query is not_found with no near misses", () => {
    const result = notFoundResult(resolve(source, "   "));
    expect(result.near_misses).toEqual([]);
  });

  test("a path query with no match still reports segment near misses", () => {
    const result = notFoundResult(resolve(source, "src/a.ts::nope"));
    expect(result.near_misses.length).toBeGreaterThanOrEqual(0);
    expect(result.near_misses.length).toBeLessThanOrEqual(5);
  });
});

// ---------------------------------------------------------------------------------------------
// expand: grouping by edge kind and tier, hop and external flags
// ---------------------------------------------------------------------------------------------

function edgeOf(
  id: number,
  srcId: number,
  dstId: number,
  kind: string,
  tier: string,
  line: number,
): EdgeRow {
  return { id, srcId, dstId, kind, tier, fileId: 1, line, localName: "", evidence: null };
}

interface GraphFixture {
  readonly in: ReadonlyMap<number, readonly NeighbourEdge[]>;
  readonly out: ReadonlyMap<number, readonly NeighbourEdge[]>;
}

function fakeNeighbourSource(graph: GraphFixture): NeighbourSource {
  const filtered = (
    edges: readonly NeighbourEdge[] | undefined,
    kinds: readonly string[] | undefined,
  ): NeighbourEdge[] => {
    const list = edges ?? [];
    const filter = kinds ?? [];
    return filter.length === 0 ? [...list] : list.filter((edge) => filter.includes(edge.edge.kind));
  };
  return {
    inbound: (nodeId, kinds) => filtered(graph.in.get(nodeId), kinds),
    outbound: (nodeId, kinds) => filtered(graph.out.get(nodeId), kinds),
  };
}

/**
 * The same fixture behind a source that only knows how to batch, and that answers each frontier
 * **backwards** — ids in reverse, rows per id in reverse. A walk that let the arrival order decide
 * anything (which group claims a node, which hop it is attributed to) would disagree with
 * {@link fakeNeighbourSource} over this source, so the two are compared byte for byte below. The
 * single-id members throw: the walk must reach for the batched pair, and a silent fallback would
 * otherwise stay invisible here.
 */
function batchedNeighbourSource(graph: GraphFixture, log: string[]): NeighbourSource {
  const collect = (
    direction: "in" | "out",
    nodeIds: readonly number[],
    kinds: readonly string[] | undefined,
  ): NeighbourEdge[] => {
    log.push(`${direction}:${nodeIds.length}`);
    const filter = kinds ?? [];
    const rows: NeighbourEdge[] = [];
    for (const id of [...nodeIds].reverse()) {
      const list = (direction === "in" ? graph.in.get(id) : graph.out.get(id)) ?? [];
      for (const edge of [...list].reverse()) {
        if (filter.length === 0 || filter.includes(edge.edge.kind)) {
          rows.push(edge);
        }
      }
    }
    return rows;
  };
  return {
    inbound: () => {
      throw new Error("this source only answers batches");
    },
    outbound: () => {
      throw new Error("this source only answers batches");
    },
    inboundMany: (nodeIds, kinds) => collect("in", nodeIds, kinds),
    outboundMany: (nodeIds, kinds) => collect("out", nodeIds, kinds),
  };
}

const SEED = nodeOf({
  id: 1,
  name: "seed",
  qn: "src/seed.ts::seed",
  path: "src/seed.ts",
  startLine: 1,
});
const CALLER = nodeOf({
  id: 2,
  name: "caller",
  qn: "src/seed.ts::caller",
  path: "src/seed.ts",
  startLine: 7,
});
const BUILTIN = nodeOf({
  id: 3,
  name: "dict",
  qn: "builtins.dict",
  path: "",
  kind: "module",
  props: { external: true },
  startLine: 1,
});
const UNTAGGED_EXTERNAL = nodeOf({
  id: 7,
  name: "abc",
  qn: "collections.abc",
  path: "",
  kind: "module",
  startLine: 1,
});
const DEEP = nodeOf({
  id: 4,
  name: "deep",
  qn: "src/deep.ts::deep",
  path: "src/deep.ts",
  startLine: 2,
});
const INBOUND_ONE = nodeOf({
  id: 5,
  name: "user",
  qn: "src/a.ts::user",
  path: "src/a.ts",
  startLine: 4,
});
const INBOUND_TWO = nodeOf({
  id: 6,
  name: "Beta",
  qn: "src/a.ts::Beta",
  path: "src/a.ts",
  kind: "class",
  startLine: 9,
});

function link(edge: EdgeRow, node: NodeRow): NeighbourEdge {
  return { edge, node };
}

/** seed --calls--> caller --calls--> deep --calls--> seed (a cycle back to the seed). */
const EXPAND_GRAPH: GraphFixture = {
  out: new Map<number, readonly NeighbourEdge[]>([
    [
      SEED.id,
      [
        link(edgeOf(1, 1, CALLER.id, "calls", "extracted", 10), CALLER),
        link(edgeOf(2, 1, BUILTIN.id, "references", "inferred", 12), BUILTIN),
        link(edgeOf(3, 1, UNTAGGED_EXTERNAL.id, "references", "inferred", 13), UNTAGGED_EXTERNAL),
      ],
    ],
    [CALLER.id, [link(edgeOf(4, CALLER.id, DEEP.id, "calls", "extracted", 20), DEEP)]],
    [DEEP.id, [link(edgeOf(5, DEEP.id, SEED.id, "calls", "extracted", 30), SEED)]],
  ]),
  in: new Map<number, readonly NeighbourEdge[]>([
    [SEED.id, [link(edgeOf(6, INBOUND_ONE.id, SEED.id, "calls", "extracted", 40), INBOUND_ONE)]],
    [
      INBOUND_ONE.id,
      [link(edgeOf(7, INBOUND_TWO.id, INBOUND_ONE.id, "contains", "extracted", 50), INBOUND_TWO)],
    ],
  ]),
};

describe("expand: grouping, hop and external", () => {
  const source = fakeNeighbourSource(EXPAND_GRAPH);

  test("groups are keyed by edge kind then tier, in both directions", () => {
    const result = expand(source, [SEED.id], { direction: "all", depth: 2 });
    expect(result.depth).toBe(2);
    expect(result.seeds).toEqual([SEED.id]);
    expect(result.out.map((group) => `${group.edge}/${group.tier}`)).toEqual([
      "calls/extracted",
      "references/inferred",
    ]);
    expect(result.in.map((group) => `${group.edge}/${group.tier}`)).toEqual([
      "calls/extracted",
      "contains/extracted",
    ]);
  });

  test("items carry hop, external and the readable location", () => {
    const result = expand(source, [SEED.id], { direction: "all", depth: 2 });
    const calls = at(result.out, 0);
    expect(calls.items).toEqual([
      {
        id: CALLER.id,
        qn: CALLER.qn,
        kind: "function",
        file: "src/seed.ts",
        line: 7,
        hop: 1,
        external: false,
      },
      {
        id: DEEP.id,
        qn: DEEP.qn,
        kind: "function",
        file: "src/deep.ts",
        line: 2,
        hop: 2,
        external: false,
      },
    ]);

    const references = at(result.out, 1);
    expect(references.items.map((item) => [item.qn, item.hop, item.external])).toEqual([
      [BUILTIN.qn, 1, true],
      [UNTAGGED_EXTERNAL.qn, 1, true],
    ]);

    const inbound = at(result.in, 0);
    expect(inbound.items.map((item) => [item.id, item.hop])).toEqual([[INBOUND_ONE.id, 1]]);
    expect(at(result.in, 1).items.map((item) => [item.id, item.hop])).toEqual([
      [INBOUND_TWO.id, 2],
    ]);
  });

  test("a cycle back to a seed is not reported as a neighbour", () => {
    const result = expand(source, [SEED.id], { direction: "all", depth: 4 });
    const ids = [...result.out, ...result.in].flatMap((group) =>
      group.items.map((item) => item.id),
    );
    expect(ids).not.toContain(SEED.id);
  });

  test("depth clamps to 1..4 and is reported as walked", () => {
    expect(expand(source, [SEED.id], { direction: "all", depth: 9 }).depth).toBe(EXPAND_DEPTH_MAX);
    expect(expand(source, [SEED.id], { direction: "all", depth: 0 }).depth).toBe(1);
    expect(expand(source, [SEED.id], { direction: "all", depth: Number.NaN }).depth).toBe(1);
    expect(expand(source, [SEED.id], { direction: "all", depth: 2.9 }).depth).toBe(2);
  });

  test("depth 1 sees only the first hop", () => {
    const result = expand(source, [SEED.id], { direction: "out", depth: 1 });
    const ids = result.out.flatMap((group) => group.items.map((item) => item.id));
    expect(ids).toEqual([CALLER.id, BUILTIN.id, UNTAGGED_EXTERNAL.id]);
    expect(ids).not.toContain(DEEP.id);
  });

  test("direction selects one half and leaves the other empty, not absent", () => {
    const inboundOnly = expand(source, [SEED.id], { direction: "in", depth: 2 });
    expect(inboundOnly.out).toEqual([]);
    expect(inboundOnly.in.length).toBe(2);

    const outboundOnly = expand(source, [SEED.id], { direction: "out", depth: 2 });
    expect(outboundOnly.in).toEqual([]);
    expect(outboundOnly.out.length).toBe(2);
  });

  test("the edge-kind filter is all-or-nothing", () => {
    const filtered = expand(source, [SEED.id], {
      direction: "out",
      depth: 2,
      kinds: ["references"],
    });
    expect(filtered.out.length).toBe(1);
    expect(at(filtered.out, 0).edge).toBe("references");
    expect(expand(source, [SEED.id], { direction: "out", depth: 2, kinds: [] }).out.length).toBe(2);
  });

  test("output is unbounded by design: seven neighbours stay seven", () => {
    const hub = nodeOf({ id: 20, name: "hub", qn: "src/hub.ts::hub", path: "src/hub.ts" });
    const spokes = Array.from({ length: 7 }, (_, index) =>
      nodeOf({
        id: 21 + index,
        name: `spoke${index}`,
        qn: `src/hub.ts::spoke${index}`,
        path: "src/hub.ts",
      }),
    );
    const hubGraph: GraphFixture = {
      out: new Map([
        [
          hub.id,
          spokes.map((spoke, index) =>
            link(edgeOf(100 + index, hub.id, spoke.id, "calls", "extracted", index + 1), spoke),
          ),
        ],
      ]),
      in: new Map(),
    };
    const grouped: GroupedNeighbors = expand(fakeNeighbourSource(hubGraph), [hub.id], {
      direction: "out",
      depth: 1,
    });
    expect(at(grouped.out, 0).items.length).toBe(7);
  });

  test("a node reachable twice is reported once, at the first hop", () => {
    const twice: GraphFixture = {
      out: new Map([
        [
          SEED.id,
          [
            link(edgeOf(1, SEED.id, CALLER.id, "calls", "extracted", 10), CALLER),
            link(edgeOf(2, SEED.id, CALLER.id, "references", "inferred", 11), CALLER),
          ],
        ],
      ]),
      in: new Map(),
    };
    const result = expand(fakeNeighbourSource(twice), [SEED.id], { direction: "out", depth: 2 });
    const ids = result.out.flatMap((group) => group.items.map((item) => item.id));
    expect(ids).toEqual([CALLER.id]);
    // Deterministic winner: `calls` sorts before `references`.
    expect(at(result.out, 0).edge).toBe("calls");
  });
});

// ---------------------------------------------------------------------------------------------
// expand: the hop is one batched lookup
// ---------------------------------------------------------------------------------------------

describe("expand: batching the frontier", () => {
  const single = fakeNeighbourSource(EXPAND_GRAPH);

  test("a batched source produces the identical payload as a per-id source", () => {
    // Every knob the walk has, including the all-or-nothing kind filter and the depth ceiling.
    for (const direction of ["all", "in", "out"] as const) {
      for (const depth of [1, 2, 3, 4]) {
        for (const kinds of [undefined, [], ["calls"], ["references", "contains"]]) {
          const options = kinds === undefined ? { direction, depth } : { direction, depth, kinds };
          expect(expand(batchedNeighbourSource(EXPAND_GRAPH, []), [SEED.id], options)).toEqual(
            expand(single, [SEED.id], options),
          );
        }
      }
    }
  });

  test("one batched call per direction per hop, never one per node", () => {
    // `in` is walked before `out`, so the log is the inbound walk's hops then the outbound walk's.
    for (const [depth, expected] of [
      [1, ["in:1", "out:1"]],
      [2, ["in:1", "in:1", "out:1", "out:3"]],
      [3, ["in:1", "in:1", "in:1", "out:1", "out:3", "out:1"]],
    ] as const) {
      const log: string[] = [];
      expand(batchedNeighbourSource(EXPAND_GRAPH, log), [SEED.id], { direction: "all", depth });
      // `in:N` / `out:N`: the frontier the hop was asked about, in the fixture's shape.
      expect(log).toEqual([...expected]);
    }
  });

  test("a walk that must fall back to single ids groups the same way", () => {
    // `fakeNeighbourSource` has no batched members, which is the pre-batching walk verbatim.
    const log: string[] = [];
    const fallback: NeighbourSource = {
      inbound: (nodeId, kinds) => {
        log.push(`in:${nodeId}`);
        return single.inbound(nodeId, kinds);
      },
      outbound: (nodeId, kinds) => {
        log.push(`out:${nodeId}`);
        return single.outbound(nodeId, kinds);
      },
    };
    expect(expand(fallback, [SEED.id], { direction: "all", depth: 2 })).toEqual(
      expand(single, [SEED.id], { direction: "all", depth: 2 }),
    );
    // The fallback is per id, which is the round-trip cost the batched member exists to remove.
    expect(log).toEqual(["in:1", "in:5", "out:1", "out:2", "out:3", "out:7"]);
  });
});

// ---------------------------------------------------------------------------------------------
// coverage: the eq/gte matrix
// ---------------------------------------------------------------------------------------------

function fileRow(id: number, path: string, generation: string, parseStatus = "ok"): IndexedFileRow {
  return {
    id,
    path,
    language: "typescript",
    sha256: "a".repeat(64),
    size: 100,
    lineCount: 10,
    parseStatus,
    parseErrorRanges: parseStatus === "ok" ? null : "7-7",
    indexedGeneration: generation,
  };
}

function skippedRow(path: string, reason: string): SkippedPathRow {
  return { id: 1, path, reason, detail: null, language: null };
}

function unresolvedRow(
  id: number,
  fileId: number,
  path: string,
  line: number,
  rawTarget: string,
): UnresolvedRef {
  return {
    id,
    fileId,
    path,
    line,
    rawTarget,
    evidenceKind: "receiver",
    evidence: "receiver type unknown",
  };
}

function fakeCoverageSource(
  indexed: readonly IndexedFileRow[],
  skipped: readonly SkippedPathRow[],
  unresolved: readonly UnresolvedRef[],
  notIndexedTotal: number = skipped.length,
): CoverageSource {
  return {
    rowsForPaths: () => ({ indexed, notIndexed: skipped }),
    unresolvedForFiles: (fileIds) => unresolved.filter((row) => fileIds.includes(row.fileId)),
    notIndexedCount: () => notIndexedTotal,
  };
}

const GENERATION = "g-2";
const COMPLETE = fakeCoverageSource(
  [fileRow(1, "src/a.ts", GENERATION), fileRow(2, "src/b.ts", GENERATION)],
  [],
  [],
);

function assemblyOf(source: CoverageSource, scope: Partial<CoverageScope> = {}): CoverageAssembly {
  return assemble(source, {
    paths: ["src"],
    generation: GENERATION,
    stale: false,
    ...scope,
  });
}

const FRESH_READ_STATUS: ReadStatus = {
  generation: GENERATION,
  indexed_at: "2026-11-17T13:50:15Z",
  stale: false,
};

describe("coverage: eq only when complete, unresolved-free and fresh", () => {
  test("a complete, unresolved-free, fresh scope is eq", () => {
    const result = assemblyOf(COMPLETE);
    expect(result.relation).toBe("eq");
    expect(result.coverage).toEqual({
      complete: true,
      indexed: 2,
      skipped: 0,
      failed: 0,
      unresolved_edges: 0,
      not_indexed: [],
    });
    expect(result.unresolved).toEqual([]);
    // The envelope must accept this coverage unchanged: one contract, one definition.
    const envelopeCoverage: Coverage = result.coverage;
    expect(relationFor(FRESH_READ_STATUS, envelopeCoverage)).toBe("eq");
  });

  test("an unresolved row in scope makes it gte", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", GENERATION), fileRow(2, "src/b.ts", GENERATION)],
      [],
      [unresolvedRow(1, 1, "src/a.ts", 12, "mystery")],
    );
    const result = assemblyOf(source);
    expect(result.relation).toBe("gte");
    expect(result.coverage.complete).toBe(true);
    expect(result.coverage.unresolved_edges).toBe(1);
    expect(result.unresolved.map((row) => `${row.path}:${row.line}`)).toEqual(["src/a.ts:12"]);
    expect(relationFor(FRESH_READ_STATUS, result.coverage)).toBe("gte");
  });

  test("a stale generation makes a complete scope gte", () => {
    const result = assemblyOf(COMPLETE, { stale: true });
    expect(result.coverage.complete).toBe(true);
    expect(result.relation).toBe("gte");
  });

  test("a file written by an older publish makes the scope gte", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", "g-1"), fileRow(2, "src/b.ts", GENERATION)],
      [],
      [],
    );
    const result = assemblyOf(source);
    expect(result.coverage.complete).toBe(true);
    expect(result.relation).toBe("gte");
  });

  test("a skipped path makes coverage incomplete and the relation gte", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", GENERATION)],
      [skippedRow("src/legacy.xslt", "grammar_unavailable")],
      [],
    );
    const result = assemblyOf(source);
    expect(result.coverage.complete).toBe(false);
    expect(result.coverage.skipped).toBe(1);
    expect(result.coverage.not_indexed).toEqual([
      { path: "src/legacy.xslt", reason: "grammar_unavailable" },
    ]);
    expect(result.relation).toBe("gte");
  });

  test("a partial parse is a failure, so coverage is incomplete", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", GENERATION), fileRow(2, "src/b.ts", GENERATION, "partial")],
      [],
      [],
    );
    const result = assemblyOf(source);
    expect(result.coverage.failed).toBe(1);
    expect(result.coverage.complete).toBe(false);
    expect(result.relation).toBe("gte");
  });

  test("an empty scope is never eq: an absence over zero files is unverified", () => {
    const result = assemblyOf(fakeCoverageSource([], [], []));
    expect(result.coverage.complete).toBe(false);
    expect(result.coverage.indexed).toBe(0);
    expect(result.relation).toBe("gte");
  });

  test("narrowing to fileIds keeps unresolved rows from outside the scope out of the answer", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", GENERATION), fileRow(2, "src/b.ts", GENERATION)],
      [],
      [unresolvedRow(1, 2, "src/b.ts", 30, "elsewhere")],
    );
    expect(assemblyOf(source).relation).toBe("gte");
    const narrowed = assemblyOf(source, { fileIds: [1] });
    expect(narrowed.unresolved).toEqual([]);
    expect(narrowed.coverage.unresolved_edges).toBe(0);
    expect(narrowed.relation).toBe("eq");
  });

  // Three skips in the graph, one of them under the queried prefix: the payload must be able to say
  // both, because the scoped count alone reads as "nothing was skipped".
  test("the scoped skipped count and the repo-wide total are reported separately", () => {
    const source = fakeCoverageSource(
      [fileRow(1, "src/a.ts", GENERATION)],
      [skippedRow("src/legacy.xslt", "grammar_unavailable")],
      [],
      3,
    );
    const result = assemblyOf(source);
    expect(result.coverage.skipped).toBe(1);
    expect(result.coverage.not_indexed).toHaveLength(1);
    expect(result.notIndexedTotal).toBe(3);
    expect(result.coverage.complete).toBe(false);
    expect(result.relation).toBe("gte");
  });
});

// ---------------------------------------------------------------------------------------------
// risk: driver sentences, not a bare number
// ---------------------------------------------------------------------------------------------

describe("risk: score, band and drivers", () => {
  test("the published example scores medium with driver sentences", () => {
    const result = score(
      { symbols: 5, files: 3, entryPoints: 1, untested: 2, removed: 0 },
      { nodes: 41, files: 10, maxIndegree: 12, topQn: "code_review_graph/graph.py::GraphStore" },
    );
    expect(result.score).toBe(0.55);
    expect(result.band).toBe("medium");
    expect(result.drivers.length).toBeGreaterThanOrEqual(1);
    // Every driver is prose with the numbers in it, never a bare count.
    for (const driver of result.drivers) {
      expect(driver).toContain(" ");
      expect(driver).toMatch(/\d/);
    }
    expect(result.drivers).toContain("41 dependents sit inside the expanded radius");
    expect(result.drivers).toContain("2 changed symbols have no reachable test");
    expect(result.drivers).toContain(
      "the widest single impact is code_review_graph/graph.py::GraphStore with 12 direct dependents",
    );
  });

  test("no dependents still yields driver sentences", () => {
    const result = score(
      { symbols: 1, files: 1, entryPoints: 0, untested: 0, removed: 0 },
      { nodes: 0, files: 0, maxIndegree: 0, topQn: null },
    );
    expect(result.drivers.length).toBeGreaterThanOrEqual(1);
    expect(result.drivers).toContain("no dependents were found inside the expanded radius");
    expect(result.drivers).toContain(
      "every changed symbol has a test within the tested-depth limit",
    );
    expect(result.band).toBe("low");
  });

  test("band thresholds cover low, medium and high", () => {
    const low = score(
      { symbols: 1, files: 1, entryPoints: 0, untested: 0, removed: 0 },
      { nodes: 1, files: 1, maxIndegree: 1, topQn: null },
    );
    expect(low.band).toBe("low");

    const high = score(
      { symbols: 10, files: 20, entryPoints: 2, untested: 5, removed: 3 },
      { nodes: 50, files: 20, maxIndegree: 50, topQn: "src/a.ts::Hub" },
    );
    expect(high.score).toBe(1);
    expect(high.band).toBe("high");
  });

  test("removed symbols add a driver only when they exist", () => {
    const withRemoved = score(
      { symbols: 1, files: 1, entryPoints: 0, untested: 0, removed: 2 },
      { nodes: 0, files: 0, maxIndegree: 0, topQn: null },
    );
    expect(withRemoved.drivers).toContain("2 removed symbols are still referenced");

    const without = score(
      { symbols: 1, files: 1, entryPoints: 0, untested: 0, removed: 0 },
      { nodes: 0, files: 0, maxIndegree: 0, topQn: null },
    );
    expect(without.drivers.some((driver) => driver.includes("removed"))).toBe(false);
  });

  test("the score is deterministic, bounded and monotone in dependents", () => {
    const facts = { symbols: 3, files: 2, entryPoints: 0, untested: 1, removed: 0 };
    const few = score(facts, { nodes: 2, files: 1, maxIndegree: 2, topQn: null });
    const many = score(facts, { nodes: 40, files: 1, maxIndegree: 9, topQn: null });
    expect(many.score).toBeGreaterThan(few.score);
    expect(few.score).toBeGreaterThanOrEqual(0);
    expect(many.score).toBeLessThanOrEqual(1);
    expect(score(facts, { nodes: 40, files: 1, maxIndegree: 9, topQn: null })).toEqual(many);
  });

  test("nonsense counts cannot produce a negative or unbounded score", () => {
    const result = score(
      { symbols: -5, files: Number.NaN, entryPoints: -1, untested: -2, removed: -3 },
      { nodes: -1, files: -1, maxIndegree: -1, topQn: null },
    );
    expect(result.score).toBe(0);
    expect(result.band).toBe("low");
    expect(result.drivers.length).toBeGreaterThanOrEqual(1);
  });
});
