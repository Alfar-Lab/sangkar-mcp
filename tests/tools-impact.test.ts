import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { BYTES_PER_TOKEN } from "#shared/budget";
import { loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import { UNRESOLVED_BUCKETS } from "#shared/domain/unresolved";
import type { Envelope } from "#shared/envelope";
import { log } from "#shared/log";
import { isUnderPrefix } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type {
  GraphReader,
  IndexedFileRow,
  NeighbourEdge,
  NodeMetrics,
  NodeRow,
  UnresolvedRef,
} from "#shared/store/read/reader";
import {
  handle,
  type ImpactPayload,
  type ImpactTotals,
  type UnresolvedCensus,
} from "../src/tools/impact/handler";
import { input, manifest } from "../src/tools/impact/manifest";

/**
 * `impact` as a handler: a fake reader for the graph and a hand-written `ToolContext`, which is
 * the seam `#shared/store/read/context.ts` exists for. Nothing here opens a database, starts a
 * server or touches the network.
 *
 * The diff half is exercised through `repoRoot`, the one filesystem fact a handler takes from
 * `ctx.config`:
 * - a plain `mkdtemp` directory is not a git repository, so `changedSinceFrom` degrades to the
 *   sha256 comparison of the `file` rows against the working tree (documented as `degraded` in
 *   `#shared/domain/diff`). That is the clean-tree and the modified-tree case without needing git.
 * - one fixture is a real local repository, because only `git diff` can report a changed path the
 *   indexer never parsed — the `README.md` that must appear in `unmatched_files` instead of
 *   vanishing.
 */

const GENERATION = "g-impact";
const INDEXED_AT = "2026-11-17T13:50:15Z";
const CHECKED_AT = new Date("2026-11-17T14:02:00Z");
const CURSOR_KEY = "impact-fixture-cursor-key-material-32b";

/** Bytes the indexer saw, and a second form the working tree may hold instead. */
const TS_A = "export const alpha = 1;\n";
const TS_A_CHANGED = "export const alpha = 2;\n";
const README = "# fixture\n";

const CURSORS = createCursors({ key: CURSOR_KEY });

/**
 * An empty scope measured nothing, and the answer says so bucket by bucket rather than omitting
 * the key: a missing `unresolved` would read as "nothing unresolved".
 */
function zeroCensus(): UnresolvedCensus {
  return {
    total: 0,
    material: 0,
    buckets: {
      builtin_stdlib: 0,
      external_package: 0,
      internal_module: 0,
      dynamic_property_call: 0,
      receiver_type_unknown: 0,
      ambiguous: 0,
    },
  };
}

/** The census must account for every row it counted, and for no row twice. */
function summed(buckets: Readonly<Record<string, number>>): number {
  return Object.values(buckets).reduce((total, count) => total + count, 0);
}

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

function write(root: string, path: string, bytes: string): void {
  const absolute = join(root, path);
  mkdirSync(dirname(absolute), { recursive: true });
  writeFileSync(absolute, bytes);
}

function sha256(bytes: string): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

/** `noUncheckedIndexedAccess` makes every index a possible undefined; a test wants a failure. */
function at<T>(list: readonly T[], index: number): T {
  const value = list[index];
  if (value === undefined) {
    throw new Error(`expected an element at index ${index}`);
  }
  return value;
}

function payloadOf(result: Envelope<ImpactPayload | null>): ImpactPayload {
  if (result.payload === null) {
    throw new Error(`expected a payload, got status ${result.status}`);
  }
  return result.payload;
}

function idsOf(result: Envelope<ImpactPayload | null>): number[] {
  return payloadOf(result).impacted.map((row) => row.id);
}

function fileRow(id: number, path: string, digest = "a".repeat(64)): IndexedFileRow {
  return {
    id,
    path,
    language: "typescript",
    sha256: digest,
    size: 24,
    lineCount: 2,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: GENERATION,
  };
}

interface NodeSpec {
  readonly id: number;
  readonly fileId: number;
  readonly path: string;
  readonly name: string;
  readonly kind?: string;
}

function nodeOf(spec: NodeSpec): NodeRow {
  return {
    id: spec.id,
    fileId: spec.fileId,
    path: spec.path,
    parentId: null,
    kind: spec.kind ?? "function",
    subkind: null,
    name: spec.name,
    qn: `${spec.path}::${spec.name}`,
    startLine: 1,
    endLine: 2,
    signature: null,
    docstring: null,
    isTest: false,
    props: {},
  };
}

function inboundEdge(spec: {
  readonly id: number;
  readonly from: NodeRow;
  readonly toId: number;
  readonly kind: string;
  readonly tier: string;
}): NeighbourEdge {
  return {
    edge: {
      id: spec.id,
      srcId: spec.from.id,
      dstId: spec.toId,
      kind: spec.kind,
      tier: spec.tier,
      fileId: spec.from.fileId,
      line: spec.from.startLine,
      localName: spec.from.name,
      evidence: null,
    },
    node: spec.from,
  };
}

function metricsOf(spec: Partial<NodeMetrics> & { readonly nodeId: number }): NodeMetrics {
  return {
    inDegree: 0,
    outDegree: 0,
    isEntry: false,
    testedDepth: null,
    reachableCount: null,
    ...spec,
  };
}

interface FakeGraph {
  readonly files?: readonly IndexedFileRow[];
  /** Nodes by `fileId`, which is how the handler gets from a changed path to its seeds. */
  readonly nodes?: ReadonlyMap<number, readonly NodeRow[]>;
  readonly inbound?: ReadonlyMap<number, readonly NeighbourEdge[]>;
  readonly metrics?: ReadonlyMap<number, NodeMetrics>;
  readonly unresolved?: readonly UnresolvedRef[];
  /** `false` models a database with no publish: `meta.generation` null and no generation row. */
  readonly published?: boolean;
  /** Counts the calls that mean "work started", so a cursor refusal can prove it did not. */
  readonly work?: { count: number };
}

function makeReader(graph: FakeGraph): GraphReader {
  const files = graph.files ?? [];
  const published = graph.published !== false;
  const work = graph.work;
  const count = (): void => {
    if (work !== undefined) {
      work.count += 1;
    }
  };
  return {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: (fileId) => {
      count();
      return [...(graph.nodes?.get(fileId) ?? [])];
    },
    inbound: (nodeId) => {
      count();
      return [...(graph.inbound?.get(nodeId) ?? [])];
    },
    outbound: () => [],
    metricsFor: (nodeId) => {
      count();
      return graph.metrics?.get(nodeId) ?? null;
    },
    rowsForPaths: (prefixes) => ({
      indexed: files.filter((file) => prefixes.some((prefix) => isUnderPrefix(file.path, prefix))),
      notIndexed: [],
    }),
    unresolvedForFiles: (fileIds) =>
      (graph.unresolved ?? []).filter((row) => fileIds.includes(row.fileId)),
    fileByPath: (path) => {
      count();
      return files.find((file) => file.path === path) ?? null;
    },
    allFiles: () => [...files],
    censusByLanguage: () => [],
    metaFor: () => ({
      projectName: "impact fixture",
      projectRoot: "/fixture",
      generation: published ? GENERATION : null,
      indexedAt: INDEXED_AT,
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () =>
      published
        ? {
            id: GENERATION,
            seq: 1,
            publishedAt: INDEXED_AT,
            baseCommit: null,
            worktreeDirty: false,
            filesIndexed: files.length,
            nodes: 3,
            edges: 2,
            durationMs: 12,
            mode: "full",
            status: "published",
          }
        : null,
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };
}

function contextOf(reader: GraphReader, repoRoot: string): ToolContext {
  return {
    reader,
    cursor: CURSORS,
    log,
    now: () => CHECKED_AT,
    config: loadConfig({
      argv: ["--repo-root", repoRoot],
      env: { SANGKAR_CURSOR_KEY: CURSOR_KEY },
    }),
  };
}

/**
 * `alpha` is called by `beta` and `beta` is referenced by `gamma`, so one seed gives a hop-1 row
 * reached through a `calls` edge and a hop-2 row reached through a `references` edge. `gamma` is an
 * entry point with no reachable test, which is the pair of flags the payload must distinguish.
 */
function baseGraph(): FakeGraph {
  const alpha = nodeOf({ id: 1, fileId: 1, path: "src/a.ts", name: "alpha" });
  const beta = nodeOf({ id: 2, fileId: 2, path: "src/b.ts", name: "beta" });
  const gamma = nodeOf({ id: 3, fileId: 3, path: "src/c.ts", name: "gamma" });
  return {
    files: [fileRow(1, "src/a.ts"), fileRow(2, "src/b.ts"), fileRow(3, "src/c.ts")],
    nodes: new Map([
      [1, [alpha]],
      [2, [beta]],
      [3, [gamma]],
    ]),
    inbound: new Map([
      [1, [inboundEdge({ id: 10, from: beta, toId: 1, kind: "calls", tier: "extracted" })]],
      [2, [inboundEdge({ id: 11, from: gamma, toId: 2, kind: "references", tier: "inferred" })]],
    ]),
    metrics: new Map([
      [1, metricsOf({ nodeId: 1, testedDepth: 0 })],
      [2, metricsOf({ nodeId: 2, inDegree: 1, testedDepth: 1 })],
      [3, metricsOf({ nodeId: 3, inDegree: 1, isEntry: true, testedDepth: null })],
    ]),
  };
}

/** One seed with `count` direct dependants, for the truncation and resume case. */
function wideGraph(count: number): FakeGraph {
  const alpha = nodeOf({ id: 1, fileId: 1, path: "src/a.ts", name: "alpha" });
  const nodes = new Map<number, readonly NodeRow[]>([[1, [alpha]]]);
  const metrics = new Map<number, NodeMetrics>([[1, metricsOf({ nodeId: 1, testedDepth: 0 })]]);
  const dependants: NeighbourEdge[] = [];
  for (let index = 0; index < count; index += 1) {
    const id = 100 + index;
    const fileId = 10 + index;
    const node = nodeOf({ id, fileId, path: `src/dep${index}.ts`, name: `dep${index}` });
    nodes.set(fileId, [node]);
    dependants.push(
      inboundEdge({ id: 1000 + index, from: node, toId: 1, kind: "calls", tier: "extracted" }),
    );
    metrics.set(
      id,
      metricsOf({ nodeId: id, inDegree: 1, testedDepth: index % 2 === 0 ? 1 : null }),
    );
  }
  return {
    files: [fileRow(1, "src/a.ts")],
    nodes,
    inbound: new Map([[1, dependants]]),
    metrics,
  };
}

function git(root: string, ...args: readonly string[]): void {
  const result = Bun.spawnSync({
    cmd: ["git", "--no-pager", "-C", root, ...args],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString()}`);
  }
}

function gitRepo(): string {
  const root = tempDir("sangkar-impact-git-");
  write(root, "src/a.ts", TS_A);
  write(root, "README.md", README);
  git(root, "init", "-q", "-b", "main");
  git(root, "add", "-A");
  git(
    root,
    "-c",
    "user.name=impact fixture",
    "-c",
    "user.email=impact@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    "init",
  );
  return root;
}

/** A commit id, so a rename can be measured against the tree that predates it. */
function rev(root: string, ref: string): string {
  const result = Bun.spawnSync({
    cmd: ["git", "--no-pager", "-C", root, "rev-parse", ref],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git rev-parse ${ref} failed: ${result.stderr.toString()}`);
  }
  return result.stdout.toString().trim();
}

/** `git mv` plus a commit: only git can report a move as one rename record. */
function renameInRepo(root: string, from: string, to: string): void {
  git(root, "mv", from, to);
  git(
    root,
    "-c",
    "user.name=impact fixture",
    "-c",
    "user.email=impact@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    "rename",
  );
}

/** The description the model sees, lifted from the spec so the two cannot drift silently. */
function specDescription(): string {
  const spec = readFileSync(
    join(import.meta.dir, "..", "docs", "my-product", "TOOLS-SPEC.md"),
    "utf8",
  );
  const start = spec.indexOf("## 6. `impact`");
  const end = spec.indexOf("### Parameters", start);
  const quoted = spec
    .slice(start, end)
    .split("\n")
    .filter((line) => line.startsWith(">"))
    .map((line) => line.replace(/^>\s?/, "").trim());
  return quoted.join(" ");
}

describe("impact manifest", () => {
  test("carries the spec's description verbatim and the five documented arguments", () => {
    expect(manifest.name).toBe("impact");
    expect("profile" in manifest).toBe(false);
    // Widened to `string` so `expect` compares text rather than the literal type.
    const description: string = manifest.description;
    expect(description).toBe(specDescription());
    expect(Object.keys(input)).toEqual(["paths", "base", "depth", "budget", "cursor"]);
    expect(input.paths).toMatchObject({ type: "array", items: { type: "string" } });
    expect(input.base).toMatchObject({ type: "string" });
    expect(input.depth).toMatchObject({ type: "integer", default: 2, min: 1, max: 4 });
    expect(input.budget).toMatchObject({ type: "integer", default: 2500 });
    expect(input.cursor).toMatchObject({ type: "string" });
  });
});

describe("impact handler: an explicit change set", () => {
  test("reports hop, via and tier per dependant and keeps an unmatched path visible", async () => {
    const result = await handle(contextOf(makeReader(baseGraph()), tempDir("cg-impact-")), {
      paths: ["src/a.ts", "docs/notes.md"],
      depth: 2,
    });

    expect(result.status).toBe("ok");
    const payload = payloadOf(result);
    expect(Object.keys(payload).sort()).toEqual([
      "changed",
      "impacted",
      "risk",
      "unmatched_files",
      "unresolved",
    ]);

    expect(payload.changed).toEqual([
      { path: "src/a.ts", state: "modified", symbols: 1, entry_point: false },
    ]);
    expect(payload.unmatched_files).toEqual([{ path: "docs/notes.md", reason: "not_indexed" }]);

    expect(payload.impacted).toEqual([
      {
        id: 2,
        qn: "src/b.ts::beta",
        kind: "function",
        hop: 1,
        via: "calls",
        tier: "extracted",
        tested: true,
      },
      {
        id: 3,
        qn: "src/c.ts::gamma",
        kind: "function",
        hop: 2,
        via: "references",
        tier: "inferred",
        tested: false,
        entry_point: true,
      },
    ]);
    // Absent, not false: the documented payload only carries the flag when it is true.
    expect(payload.impacted[0]).not.toHaveProperty("entry_point");

    expect(result.read_status.generation).toBe(GENERATION);
    expect(result.read_status.indexed_at).toBe(INDEXED_AT);
    expect(result.read_status.stale_checked_at).toBe(CHECKED_AT.toISOString());
    // An explicit change set was never compared to the working tree, so freshness is not claimed.
    expect(result.read_status.stale).toBe(true);
    expect(result.read_status.stale_reason).toContain("explicitly");
    expect(result.totals as ImpactTotals).toEqual({
      impacted_total: 2,
      unresolved_material: 0,
      total_relation: "gte",
    });
    expect(result.truncated).toBe(false);
    expect(result.next).toBeNull();
    expect(result.next_tools).toEqual(["trace", "expand"]);
    expect(result.summary).toContain("1 of 2 requested paths reach 2 nodes in 2 files");
    expect(result.summary).toContain("1 entry points affected");
    expect(result.summary).toContain("1 of 2 covered by tests");

    // The risk number is the shared formula's, not a second opinion: 2 dependants, 2 files, 1
    // changed symbol, none untested.
    expect(result.payload?.risk.band).toBe("low");
    expect(result.payload?.risk.score).toBe(0.05);
    expect(payload.risk.drivers[0]).toBe("2 dependents sit inside the expanded radius");
  });

  test("a changed path the graph cannot explain is reported, never dropped", async () => {
    const graph = baseGraph();
    // An indexed file whose publish wrote no nodes: it seeds nothing, so it cannot stay silent.
    const reader = makeReader({
      ...graph,
      files: [...(graph.files ?? []), fileRow(4, "src/empty.ts")],
    });
    const result = await handle(contextOf(reader, tempDir("cg-impact-md-")), {
      paths: ["CHANGELOG.md", "src/empty.ts"],
    });
    const payload = payloadOf(result);
    expect(payload.changed).toEqual([]);
    expect(payload.impacted).toEqual([]);
    expect(payload.unmatched_files).toEqual([
      { path: "CHANGELOG.md", reason: "not_indexed" },
      { path: "src/empty.ts", reason: "no_nodes_in_graph" },
    ]);
    expect(result.summary).toContain("2 changed paths could not be matched");
  });

  test("an unresolved call site in the change scope makes the total a lower bound", async () => {
    const graph = baseGraph();
    const reader = makeReader({
      ...graph,
      unresolved: [
        {
          id: 7,
          fileId: 1,
          path: "src/a.ts",
          line: 12,
          rawTarget: "mystery",
          evidenceKind: null,
          evidence: null,
        },
        {
          id: 8,
          fileId: 1,
          path: "src/a.ts",
          line: 13,
          rawTarget: "obj[0]",
          evidenceKind: "dynamic",
          evidence: "call:obj[0]",
        },
      ],
    });
    const result = await handle(contextOf(reader, tempDir("cg-impact-unresolved-")), {
      paths: ["src/a.ts"],
    });
    expect(result.coverage.unresolved_edges).toBe(2);
    expect(result.totals.total_relation).toBe("gte");

    // The flat count is explained: which rows exist, and which of them could still be user code.
    const payload = payloadOf(result);
    expect(Object.keys(payload.unresolved.buckets).sort()).toEqual([...UNRESOLVED_BUCKETS].sort());
    expect(payload.unresolved.buckets.receiver_type_unknown).toBe(1);
    expect(payload.unresolved.buckets.dynamic_property_call).toBe(1);
    expect(summed(payload.unresolved.buckets)).toBe(payload.unresolved.total);
    // One number, not two counts of the same rows: the payload and coverage cannot disagree.
    expect(payload.unresolved.total).toBe(result.coverage.unresolved_edges);
    // `obj[0]` is recorded exactly when no declared target exists, so only `mystery` is material.
    expect(payload.unresolved.material).toBe(1);
    expect(payload.unresolved.material).toBeLessThanOrEqual(payload.unresolved.total);
    expect((result.totals as ImpactTotals).unresolved_material).toBe(payload.unresolved.material);
  });
});

describe("impact handler: the working-tree diff", () => {
  test("a clean tree in a directory git cannot read reports nothing to impact", async () => {
    const root = tempDir("cg-impact-clean-");
    write(root, "src/a.ts", TS_A);
    // The stored digest is the working tree's, so the degraded hash-only diff finds no change.
    const graph = baseGraph();
    const reader = makeReader({
      ...graph,
      files: [fileRow(1, "src/a.ts", sha256(TS_A))],
      nodes: new Map([...(graph.nodes ?? [])].filter(([fileId]) => fileId === 1)),
    });

    const result = await handle(contextOf(reader, root), {});

    expect(result.status).toBe("ok");
    expect(result.payload).toEqual({
      changed: [],
      impacted: [],
      unmatched_files: [],
      risk: { score: 0, band: "low", drivers: expect.any(Array) as unknown as string[] },
      // An empty change set measured nothing, so the census is present and zero, never absent.
      unresolved: zeroCensus(),
    });
    expect(result.read_status.stale).toBe(false);
    expect(result.read_status.stale_reason).toBeUndefined();
    expect(result.summary).toBe("no changed files against HEAD: nothing to impact");
    expect(result.totals.impacted_total).toBe(0);
    expect((result.totals as ImpactTotals).unresolved_material).toBe(0);
    expect(result.truncated).toBe(false);
    expect(result.next).toBeNull();
  });

  test("a modified indexed file is found by the hash comparison", async () => {
    const root = tempDir("cg-impact-modified-");
    write(root, "src/a.ts", TS_A_CHANGED);
    const graph = baseGraph();
    const reader = makeReader({
      ...graph,
      files: [fileRow(1, "src/a.ts", sha256(TS_A))],
    });

    const result = await handle(contextOf(reader, root), { depth: 1 });

    expect(result.status).toBe("ok");
    expect(payloadOf(result).changed).toEqual([
      { path: "src/a.ts", state: "modified", symbols: 1, entry_point: false },
    ]);
    expect(idsOf(result)).toEqual([2]);
    expect(result.read_status.stale).toBe(true);
    expect(result.read_status.stale_reason).toBe("1 file changed since index");
    expect(result.summary).toContain("1 changed files reach 1 nodes in 1 files");
  });

  /**
   * A deleted file was counted only as a number before (`changeSet.removed.length`), so an impact
   * report over a deletion named a blast radius with the deletion itself invisible. The row is now
   * an entry — with zero symbols, because its bytes are gone — and it seeds nothing: the graph's
   * surviving rows for that path are history, and walking them would invent a blast radius.
   */
  test("a deleted tracked file is one entry with state deleted and no symbols to rank", async () => {
    const root = tempDir("cg-impact-deleted-");
    write(root, "src/a.ts", TS_A);
    const alpha = nodeOf({ id: 1, fileId: 1, path: "src/a.ts", name: "alpha" });
    const beta = nodeOf({ id: 2, fileId: 2, path: "src/b.ts", name: "beta" });
    const gamma = nodeOf({ id: 3, fileId: 3, path: "src/c.ts", name: "gamma" });
    const reader = makeReader({
      // `src/a.ts` matches its stored digest; `src/b.ts` has no bytes left to match.
      files: [fileRow(1, "src/a.ts", sha256(TS_A)), fileRow(2, "src/b.ts")],
      nodes: new Map([
        [1, [alpha]],
        [2, [beta]],
      ]),
      // A dependant behind the deleted file: nothing may be reached from a path that is gone.
      inbound: new Map([
        [2, [inboundEdge({ id: 20, from: gamma, toId: 2, kind: "calls", tier: "extracted" })]],
      ]),
      metrics: new Map([
        [1, metricsOf({ nodeId: 1, testedDepth: 0 })],
        [2, metricsOf({ nodeId: 2, inDegree: 1, testedDepth: 0 })],
      ]),
    });

    const result = await handle(contextOf(reader, root), {});

    const payload = payloadOf(result);
    expect(payload.changed).toEqual([
      { path: "src/b.ts", state: "deleted", symbols: 0, entry_point: false },
    ]);
    expect(payload.impacted).toEqual([]);
    // A deletion is a changed file the graph can explain, not an unmatched one.
    expect(payload.unmatched_files).toEqual([]);
    expect(result.read_status.stale_reason).toBe("1 path removed since index");
    expect(result.summary).toContain("1 changed files reach no dependents within 2 hops");
  });

  /**
   * A rename arrives as two entries from the diff — the git half's `renamed` destination and the
   * hash comparison's `deleted` source — and a payload that rendered both would count one move as
   * two changed files. The fixture holds a row for both paths because that is the state that
   * produces the pair; the assertion is that only the rename survives.
   */
  test("a rename is one entry carrying from, not a rename plus a deletion", async () => {
    const root = gitRepo();
    const base = rev(root, "HEAD");
    renameInRepo(root, "src/a.ts", "src/c.ts");
    const delta = nodeOf({ id: 2, fileId: 2, path: "src/c.ts", name: "alpha" });
    const reader = makeReader({
      files: [fileRow(1, "src/a.ts", sha256(TS_A)), fileRow(2, "src/c.ts", sha256(TS_A))],
      nodes: new Map([[2, [delta]]]),
      metrics: new Map([[2, metricsOf({ nodeId: 2, testedDepth: 0 })]]),
    });

    const result = await handle(contextOf(reader, root), { base });

    const payload = payloadOf(result);
    expect(payload.changed).toEqual([
      { path: "src/c.ts", state: "renamed", from: "src/a.ts", symbols: 1, entry_point: false },
    ]);
    // The `deleted` half of the pair is exactly what would double-count the move.
    expect(payload.changed.some((row) => row.path === "src/a.ts")).toBe(false);
    expect(payload.unmatched_files).toEqual([]);
    expect(result.summary).toContain("1 changed files reach no dependents within 2 hops");
  });

  test("a changed markdown file appears in unmatched_files with the diff's reason", async () => {
    const root = gitRepo();
    write(root, "README.md", "# changed\n");
    const graph = baseGraph();
    const reader = makeReader({
      ...graph,
      files: [fileRow(1, "src/a.ts", sha256(TS_A))],
    });

    const result = await handle(contextOf(reader, root), {});

    const payload = payloadOf(result);
    expect(payload.changed).toEqual([]);
    expect(payload.impacted).toEqual([]);
    // Git reported it; there is no file row and no MVP grammar covers `.md`, which is exactly the
    // path that used to disappear from an impact analysis.
    expect(payload.unmatched_files).toEqual([
      { path: "README.md", reason: "no_file_row_and_unparseable_language" },
    ]);
    expect(result.summary).toContain("1 changed path could not be matched");
  });
});

describe("impact handler: paging", () => {
  test("a truncated page sets gte and its cursor returns the following page with no duplicates", async () => {
    const ctx = contextOf(makeReader(wideGraph(80)), tempDir("cg-impact-page-"));

    const full = await handle(ctx, { paths: ["src/a.ts"], depth: 1, budget: 20_000 });
    const allIds = idsOf(full);
    expect(full.truncated).toBe(false);
    expect(allIds.length).toBe(80);

    // The impact envelope's floor is above 500 tokens for an 80-dependant answer, and below the
    // floor the handler now refuses instead of serving past the allowance. The tightest budget that
    // still cuts is found from the whole answer rather than guessed.
    const fullTokens = Math.ceil(Buffer.byteLength(JSON.stringify(full), "utf8") / BYTES_PER_TOKEN);
    let budget = fullTokens - 1;
    for (; budget > 1; budget -= 1) {
      const cut = await handle(ctx, { paths: ["src/a.ts"], depth: 1, budget });
      if (cut.status === "ok" && cut.truncated) {
        break;
      }
    }
    expect(budget).toBeGreaterThan(1);
    const first = await handle(ctx, { paths: ["src/a.ts"], depth: 1, budget });
    const firstIds = idsOf(first);
    expect(first.truncated).toBe(true);
    expect(first.truncation_reason).toBe("budget");
    expect(first.totals as ImpactTotals).toEqual({
      impacted_total: 80,
      unresolved_material: 0,
      total_relation: "gte",
    });
    // A partial page, not an empty one: the fixture is large enough that the budget cuts it.
    expect(firstIds.length).toBeGreaterThan(0);
    expect(firstIds.length).toBeLessThan(80);
    expect(firstIds).toEqual(allIds.slice(0, firstIds.length));
    expect(new Set(firstIds).size).toBe(firstIds.length);
    expect(first.next).not.toBeNull();
    expect(first.summary).toContain(`Showing ${firstIds.length} of 80`);

    const resumed = await handle(ctx, {
      paths: ["src/a.ts"],
      depth: 1,
      budget,
      cursor: first.next ?? "",
    });
    const resumedIds = idsOf(resumed);

    expect(resumedIds.length).toBeGreaterThan(0);
    expect(resumedIds.filter((id) => firstIds.includes(id))).toEqual([]);
    expect(new Set(resumedIds).size).toBe(resumedIds.length);
    // The next page continues exactly where the previous one stopped.
    expect(at(resumedIds, 0)).toBe(at(allIds, firstIds.length));
    expect(resumed.totals.impacted_total).toBe(80);
    expect(resumed.summary).toContain(`of 80`);
  });

  test("a cursor from another call is refused before any work is done", async () => {
    const work = { count: 0 };
    const ctx = contextOf(makeReader({ ...baseGraph(), work }), tempDir("cg-impact-cursor-"));

    const foreign = CURSORS.sign({
      generation: GENERATION,
      tool: "trace",
      argsHash: "0".repeat(32),
      position: 0,
    });
    const mismatch = await handle(ctx, { paths: ["src/a.ts"], cursor: foreign });

    expect(mismatch.status).toBe("error");
    expect(mismatch.payload).toBeNull();
    expect(mismatch.error?.code).toBe("cursor_mismatch");
    expect(mismatch.error?.hint).toBe("re-run the original call with the same tool and arguments");
    expect(mismatch.next).toBeNull();
    expect(work.count).toBe(0);

    const stale = CURSORS.sign({
      generation: "g-old",
      tool: "impact",
      argsHash: "0".repeat(32),
      position: 0,
    });
    const expired = await handle(ctx, { paths: ["src/a.ts"], cursor: stale });
    expect(expired.error?.code).toBe("cursor_expired");
    expect(expired.error?.hint).toBe("re-run the original call");
    expect(work.count).toBe(0);
  });
});

describe("impact handler: refusals that are not crashes", () => {
  test("a budget above the ceiling is refused with the contract error, not thrown", async () => {
    const ctx = contextOf(makeReader(baseGraph()), tempDir("cg-impact-budget-"));
    const result = await handle(ctx, { paths: ["src/a.ts"], budget: 40_000 });
    expect(result.status).toBe("error");
    expect(result.error?.code).toBe("budget_too_large");
    expect(result.error?.hint).toContain("32000");
  });

  test("an unpublished database answers bootstrapping instead of signing a cursor", async () => {
    const ctx = contextOf(
      makeReader({ ...baseGraph(), published: false }),
      tempDir("cg-impact-boot-"),
    );
    const result = await handle(ctx, {});
    expect(result.status).toBe("bootstrapping");
    expect(result.error?.code).toBe("bootstrapping");
    expect(result.error?.retry_after_ms).toBe(1000);
    expect(result.payload).toBeNull();
  });

  test("the handler names only #shared modules and never writes to stdout", () => {
    const source = readFileSync(
      join(import.meta.dir, "..", "src", "tools", "impact", "handler.ts"),
      "utf8",
    );
    const specifiers = [...source.matchAll(/from\s+"([^"]+)"/g)].map((match) => match[1] ?? "");
    expect(specifiers.length).toBeGreaterThan(0);
    for (const specifier of specifiers) {
      // A tool sees `#shared/*`, its own `./` siblings and node/bun builtins — nothing else.
      expect(
        specifier.startsWith(".") ||
          specifier.startsWith("#shared/") ||
          specifier.startsWith("node:"),
      ).toBe(true);
    }
    expect(source).not.toContain("drizzle-orm");
    expect(source).not.toContain("console.log");
    expect(source).not.toContain("process.env");
  });
});
