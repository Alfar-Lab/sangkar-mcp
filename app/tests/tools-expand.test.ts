import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { BYTES_PER_TOKEN } from "#shared/budget";
import { type AppConfig, loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import type { Envelope } from "#shared/envelope";
import { log } from "#shared/log";
import { isUnderPrefix } from "#shared/paths";
import type { CursorFacade, ToolContext } from "#shared/store/read/context";
import type { IndexedFileRow, PathCoverage, SkippedPathRow } from "#shared/store/read/coverage";
import type { EdgeRow, NeighbourEdge } from "#shared/store/read/edges";
import type { NodeMetrics } from "#shared/store/read/metrics";
import type { NodeRow } from "#shared/store/read/nodes";
import type { GraphReader } from "#shared/store/read/reader";
import {
  type AmbiguousPayload,
  type CoveragePayload,
  type ExpandArgs,
  type ExpandPayload,
  handle,
  type MembersPayload,
  type MissingPayload,
  type SourcePayload,
  TOOL_NAME,
} from "../src/tools/expand/handler";
import { DEFAULT_BUDGET_TOKENS, input, manifest } from "../src/tools/expand/manifest";

/**
 * `expand` against a hand-written `GraphReader` and one real directory (`TOOLS-SPEC.md` §7).
 *
 * A fake reader is enough for everything except the one mode that must touch the working tree:
 * `what: "source"` verifies the file's `sha256` before returning text, so that expectation is
 * proved against bytes on disk in a temp dir rather than against a stubbed hash function. No
 * database, no network, no shared mutable state between the two cases.
 *
 * The fixture is one TypeScript file (a class with two methods, a free function, two same-named
 * symbols for the tie) plus a folder whose coverage is half indexed and half skipped, because
 * those are the four inference answers the task's acceptance criteria name.
 */

const ROOT_GENERATION = "g-2";
const INDEXED_AT = "2026-01-03T00:00:00Z";
const KEY = "expand-test-cursor-key-material-32";

const FILE_A = 1;
const FILE_FOLDER = 2;
const FILE_DRIFTED = 3;

const NODE_FILE_A = 1;
const NODE_WIDGET = 10;
const NODE_RENDER = 11;
const NODE_DISPOSE = 12;
const NODE_HELPER = 13;
const NODE_DUP_METHOD = 14;
const NODE_DUP_FUNCTION = 15;
const NODE_DRIFTED = 20;

const A_PATH = "src/a.ts";
const DRIFTED_PATH = "src/drifted.ts";
const FOLDER_PREFIX = "src/widgets";

/** `noUncheckedIndexedAccess` makes every index a possible undefined; a test wants a failure. */
function at<T>(list: readonly T[], index: number): T {
  const value = list[index];
  if (value === undefined) {
    throw new Error(`expected an element at index ${index}`);
  }
  return value;
}

function sha256Of(text: string): string {
  return new Bun.CryptoHasher("sha256").update(text).digest("hex");
}

function nodeOf(overrides: Partial<NodeRow> & Pick<NodeRow, "id" | "name" | "qn">): NodeRow {
  return {
    fileId: FILE_A,
    path: A_PATH,
    parentId: NODE_FILE_A,
    kind: "function",
    subkind: null,
    startLine: 1,
    endLine: 2,
    signature: null,
    docstring: null,
    isTest: false,
    props: {},
    ...overrides,
  };
}

function edgeOf(id: number, srcId: number, dstId: number, kind: string): EdgeRow {
  return {
    id,
    srcId,
    dstId,
    kind,
    tier: "static",
    fileId: FILE_A,
    line: 7,
    localName: "",
    evidence: null,
  };
}

function neighbourOf(edge: EdgeRow, node: NodeRow): NeighbourEdge {
  return { edge, node };
}

/** Forty lines, so a window in the middle of the file is unambiguous. */
const A_SOURCE = Array.from(
  { length: 40 },
  (_unused, index) => `const v${index + 1} = ${index + 1};`,
)
  .join("\n")
  .concat("\n");

const DRIFTED_NOW = "function drifted() {\n  return 2;\n}\n";

const dirs: string[] = [];
let repoRoot = "";
let aSha = "";
let driftedIndexedSha = "";

function fileRow(id: number, path: string, sha256: string, lineCount: number): IndexedFileRow {
  return {
    id,
    path,
    language: "typescript",
    sha256,
    size: 512,
    lineCount,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: ROOT_GENERATION,
  };
}

function makeFixture(): {
  reader: GraphReader;
  files: Map<string, IndexedFileRow>;
  rows: Map<string, PathCoverage>;
} {
  const fileA = fileRow(FILE_A, A_PATH, aSha, 40);
  const folderFile = fileRow(FILE_FOLDER, `${FOLDER_PREFIX}/x.ts`, "b".repeat(64), 12);
  const driftedFile = fileRow(FILE_DRIFTED, DRIFTED_PATH, driftedIndexedSha, 3);
  const skipped: SkippedPathRow = {
    id: 9,
    path: `${FOLDER_PREFIX}/skip.xslt`,
    reason: "grammar_unavailable",
    detail: null,
    language: null,
  };

  const nodes: NodeRow[] = [
    nodeOf({
      id: NODE_FILE_A,
      name: "a.ts",
      qn: `${A_PATH}::a.ts`,
      kind: "file",
      parentId: null,
      startLine: 1,
      endLine: 40,
    }),
    nodeOf({
      id: NODE_WIDGET,
      name: "Widget",
      qn: `${A_PATH}::Widget`,
      kind: "class",
      startLine: 3,
      endLine: 20,
    }),
    nodeOf({
      id: NODE_RENDER,
      name: "render",
      qn: `${A_PATH}::Widget.render`,
      kind: "method",
      parentId: NODE_WIDGET,
      startLine: 5,
      endLine: 8,
    }),
    nodeOf({
      id: NODE_DISPOSE,
      name: "dispose",
      qn: `${A_PATH}::Widget.dispose`,
      kind: "method",
      parentId: NODE_WIDGET,
      startLine: 10,
      endLine: 12,
    }),
    nodeOf({
      id: NODE_HELPER,
      name: "helper",
      qn: `${A_PATH}::helper`,
      kind: "function",
      startLine: 25,
      endLine: 30,
    }),
    nodeOf({
      id: NODE_DUP_METHOD,
      name: "dup",
      qn: `${A_PATH}::Widget.dup`,
      kind: "method",
      parentId: NODE_WIDGET,
      startLine: 14,
      endLine: 16,
    }),
    nodeOf({
      id: NODE_DUP_FUNCTION,
      name: "dup",
      qn: `${A_PATH}::dup`,
      kind: "function",
      startLine: 33,
      endLine: 35,
    }),
    nodeOf({
      id: NODE_DRIFTED,
      name: "drifted",
      qn: `${DRIFTED_PATH}::drifted`,
      kind: "function",
      fileId: FILE_DRIFTED,
      path: DRIFTED_PATH,
      parentId: null,
      startLine: 1,
      endLine: 3,
    }),
  ];

  const metrics = new Map<number, NodeMetrics>([
    [
      NODE_WIDGET,
      {
        nodeId: NODE_WIDGET,
        inDegree: 0,
        outDegree: 2,
        isEntry: false,
        testedDepth: 2,
        reachableCount: null,
      },
    ],
    [
      NODE_RENDER,
      {
        nodeId: NODE_RENDER,
        inDegree: 2,
        outDegree: 1,
        isEntry: false,
        testedDepth: 0,
        reachableCount: null,
      },
    ],
  ]);

  const edgesIn = new Map<number, NeighbourEdge[]>([
    [NODE_RENDER, [neighbourOf(edgeOf(1, NODE_HELPER, NODE_RENDER, "calls"), helperNode(nodes))]],
  ]);
  const edgesOut = new Map<number, NeighbourEdge[]>([
    [NODE_RENDER, [neighbourOf(edgeOf(2, NODE_RENDER, NODE_HELPER, "calls"), helperNode(nodes))]],
  ]);

  const files = new Map<string, IndexedFileRow>([
    [A_PATH, fileA],
    [folderFile.path, folderFile],
    [DRIFTED_PATH, driftedFile],
  ]);
  const rows = new Map<string, PathCoverage>([
    [A_PATH, { indexed: [fileA], notIndexed: [] }],
    [FOLDER_PREFIX, { indexed: [folderFile], notIndexed: [skipped] }],
    [DRIFTED_PATH, { indexed: [driftedFile], notIndexed: [] }],
  ]);

  const reader: GraphReader = {
    findByQn: (qn) => nodes.find((node) => node.qn === qn) ?? null,
    findByName: (name) => nodes.filter((node) => node.name === name).sort(byQn),
    members: (parentId) => nodes.filter((node) => node.parentId === parentId).sort(byStartLine),
    nodesByFile: (fileId) => nodes.filter((node) => node.fileId === fileId).sort(byStartLine),
    inbound: (nodeId) => edgesIn.get(nodeId) ?? [],
    outbound: (nodeId) => edgesOut.get(nodeId) ?? [],
    metricsFor: (nodeId) => metrics.get(nodeId) ?? null,
    rowsForPaths: (prefixes) => {
      const covered: PathCoverage = { indexed: [], notIndexed: [] };
      for (const [prefix, entry] of rows) {
        if (!prefixes.some((wanted) => isUnderPrefix(prefix, wanted))) {
          continue;
        }
        return entry;
      }
      return covered;
    },
    unresolvedForFiles: () => [],
    fileByPath: (path) => files.get(path) ?? null,
    allFiles: () => [...files.values()],
    censusByLanguage: () => [{ language: "typescript", files: 3, parsed: 3, failed: 0 }],
    metaFor: () => ({
      projectName: "fixture",
      projectRoot: repoRoot,
      generation: ROOT_GENERATION,
      indexedAt: INDEXED_AT,
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () => ({
      id: ROOT_GENERATION,
      seq: 2,
      publishedAt: INDEXED_AT,
      baseCommit: null,
      worktreeDirty: false,
      filesIndexed: 3,
      nodes: nodes.length,
      edges: 2,
      durationMs: 12,
      mode: "full",
      status: "ok",
    }),
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };

  return { reader, files, rows };
}

function byQn(a: NodeRow, b: NodeRow): number {
  return a.qn.localeCompare(b.qn);
}

function byStartLine(a: NodeRow, b: NodeRow): number {
  return a.startLine - b.startLine;
}

function helperNode(nodes: readonly NodeRow[]): NodeRow {
  const helper = nodes.find((node) => node.id === NODE_HELPER);
  if (helper === undefined) {
    throw new Error("fixture is missing the helper node");
  }
  return helper;
}

function makeContext(reader: GraphReader, repoRootPath: string): ToolContext {
  const config: AppConfig = loadConfig({
    env: { CODEGRAPH_REPO_ROOT: repoRootPath, CODEGRAPH_CURSOR_KEY: KEY },
  });
  const cursor: CursorFacade = createCursors({ key: KEY });
  return { reader, cursor, log, now: () => new Date("2026-01-03T00:05:00Z"), config };
}

function membersOf(env: Envelope<ExpandPayload | null>): MembersPayload {
  const payload = env.payload;
  if (payload === null || !("members" in payload)) {
    throw new Error(`expected a members payload, got ${env.status}`);
  }
  return payload;
}

function coverageOf(env: Envelope<ExpandPayload | null>): CoveragePayload {
  const payload = env.payload;
  if (payload === null || !("prefix" in payload)) {
    throw new Error(`expected a coverage payload, got ${env.status}`);
  }
  return payload;
}

function sourceOf(env: Envelope<ExpandPayload | null>): SourcePayload {
  const payload = env.payload;
  if (payload === null || !("source_drift" in payload)) {
    throw new Error(`expected a source payload, got ${env.status}`);
  }
  return payload;
}

function missingOf(env: Envelope<ExpandPayload | null>): MissingPayload {
  const payload = env.payload;
  // Every answer body carries `what`; a tie carries `candidates`. What is left is the refusal.
  if (payload === null || "what" in payload || "candidates" in payload) {
    throw new Error(`expected a not_found payload, got ${env.status}`);
  }
  return payload;
}

function ambiguousOf(env: Envelope<ExpandPayload | null>): AmbiguousPayload {
  const payload = env.payload;
  if (payload === null || !("candidates" in payload)) {
    throw new Error(`expected an ambiguous payload, got ${env.status}`);
  }
  return payload;
}

describe("expand: manifest", () => {
  test("declares the spec's name, verbatim description and agent profile", () => {
    expect(manifest.name).toBe("expand");
    expect(manifest.profile).toBe("agent");
    expect(manifest.description).toContain("Fetch more detail for one item from a previous result");
    expect(manifest.description).toContain("says which it chose");
  });

  test("declares exactly three arguments, one of them required, with the spec's budget default", () => {
    expect(Object.keys(input)).toEqual(["target", "what", "budget"]);
    expect(input.target.required).toBe(true);
    expect([...input.what.values]).toEqual(["members", "edges", "source", "coverage"]);
    expect(input.budget.default).toBe(2500);
    expect(DEFAULT_BUDGET_TOKENS).toBe(2500);
    expect(TOOL_NAME).toBe("expand");
  });
});

describe("expand: inference", () => {
  let ctx: ToolContext;

  beforeAll(() => {
    repoRoot = mkdtempSync(join(tmpdir(), "codegraph-expand-"));
    dirs.push(repoRoot);
    mkdirSync(join(repoRoot, "src"), { recursive: true });
    aSha = sha256Of(A_SOURCE);
    driftedIndexedSha = sha256Of("function drifted() {\n  return 1;\n}\n");
    writeFileSync(join(repoRoot, A_PATH), A_SOURCE, "utf8");
    writeFileSync(join(repoRoot, DRIFTED_PATH), DRIFTED_NOW, "utf8");
    ctx = makeContext(makeFixture().reader, repoRoot);
  });

  afterAll(() => {
    for (const dir of dirs) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("a class target infers members and says so", async () => {
    const env = await handle(ctx, { target: `${A_PATH}::Widget` });

    expect(env.status).toBe("ok");
    expect(env.payload).not.toBeNull();
    const payload = membersOf(env);
    expect(payload.what).toBe("members");
    expect(payload.why).toBe("inferred: class -> members");
    expect(payload.target).toEqual({
      id: NODE_WIDGET,
      qn: `${A_PATH}::Widget`,
      kind: "class",
      lines: [3, 20],
    });
    // Source order, one level only: the members are the class's own methods.
    expect(payload.members.map((member) => member.name)).toEqual(["render", "dispose", "dup"]);
    expect(at(payload.members, 0).lines).toEqual([5, 8]);
    // `metricsFor` never measured `dispose` or `dup`, so their degree is unknown, not zero.
    expect(at(payload.members, 0).degree).toEqual({ in: 2, out: 1 });
    expect(at(payload.members, 1).degree).toBeNull();
    expect(env.totals.members_total).toBe(3);
    expect(env.summary).toBe("members of src/a.ts::Widget: 3.");
    expect(env.read_status).toMatchObject({ generation: ROOT_GENERATION, stale: false });
  });

  test("a file target infers members of its top-level symbols", async () => {
    const env = await handle(ctx, { target: A_PATH });

    expect(env.status).toBe("ok");
    const payload = membersOf(env);
    expect(payload.why).toBe("inferred: file -> members");
    expect(payload.target.kind).toBe("file");
    expect(payload.members.map((member) => member.name).sort()).toEqual([
      "Widget",
      "dup",
      "helper",
    ]);
  });

  test("a method target infers edges in the grouped neighbour shape trace ships", async () => {
    const env = await handle(ctx, { target: "Widget.render" });

    expect(env.status).toBe("ok");
    const payload = env.payload;
    if (payload === null || !("direction" in payload)) {
      throw new Error("expected an edges payload");
    }
    expect(payload.what).toBe("edges");
    expect(payload.why).toBe("inferred: method -> edges");
    expect(payload.depth).toBe(1);
    expect(payload.direction).toBe("all");
    expect(payload.in.map((group) => group.edge)).toEqual(["calls"]);
    expect(payload.out.map((group) => group.edge)).toEqual(["calls"]);
    const inbound = at(payload.in, 0);
    expect(inbound.tier).toBe("static");
    expect(inbound.items.map((item) => item.qn)).toEqual([`${A_PATH}::helper`]);
    expect(at(inbound.items, 0).hop).toBe(1);
    expect(env.totals.edges_total).toBe(2);
  });

  test("a folder target infers coverage of the prefix, both halves included", async () => {
    const env = await handle(ctx, { target: FOLDER_PREFIX });

    expect(env.status).toBe("ok");
    const payload = coverageOf(env);
    expect(payload.what).toBe("coverage");
    expect(payload.why).toBe("inferred: folder -> coverage");
    // A folder is an address, not a node: the three node fields are null rather than invented.
    expect(payload.target).toEqual({ id: null, qn: null, kind: "folder", lines: null });
    expect(payload.prefix).toBe(FOLDER_PREFIX);
    expect(payload.files.map((file) => file.path)).toEqual([`${FOLDER_PREFIX}/x.ts`]);
    expect(payload.not_indexed).toEqual([
      { path: `${FOLDER_PREFIX}/skip.xslt`, reason: "grammar_unavailable", detail: null },
    ]);
    // The envelope's own coverage keeps the same skip, so the payload and the contract agree.
    expect(env.coverage.not_indexed).toEqual([
      { path: `${FOLDER_PREFIX}/skip.xslt`, reason: "grammar_unavailable" },
    ]);
    expect(env.coverage.complete).toBe(false);
    expect(env.totals.files_total).toBe(1);
    expect(env.totals.not_indexed_total).toBe(1);
  });

  test("an explicit what wins and is echoed as requested, not inferred", async () => {
    const env = await handle(ctx, { target: `${A_PATH}::Widget`, what: "edges" });

    expect(env.status).toBe("ok");
    const payload = env.payload;
    if (payload === null || !("direction" in payload)) {
      throw new Error("expected an edges payload");
    }
    expect(payload.why).toBe("requested: edges");
  });

  test("coverage on a node answers for that node's file", async () => {
    const env = await handle(ctx, { target: `${A_PATH}::Widget`, what: "coverage" });

    expect(env.status).toBe("ok");
    const payload = coverageOf(env);
    expect(payload.why).toBe("requested: coverage");
    expect(payload.prefix).toBe(A_PATH);
    expect(payload.target.kind).toBe("class");
    expect(payload.files.map((file) => file.path)).toEqual([A_PATH]);
  });
});

describe("expand: source and its drift check", () => {
  let ctx: ToolContext;

  beforeAll(() => {
    repoRoot = mkdtempSync(join(tmpdir(), "codegraph-expand-source-"));
    dirs.push(repoRoot);
    mkdirSync(join(repoRoot, "src"), { recursive: true });
    aSha = sha256Of(A_SOURCE);
    // The row records the sha of the version that was indexed; the bytes on disk say otherwise.
    driftedIndexedSha = sha256Of("function drifted() {\n  return 1;\n}\n");
    writeFileSync(join(repoRoot, A_PATH), A_SOURCE, "utf8");
    writeFileSync(join(repoRoot, DRIFTED_PATH), DRIFTED_NOW, "utf8");
    ctx = makeContext(makeFixture().reader, repoRoot);
  });

  afterAll(() => {
    for (const dir of dirs) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("a verified file returns its numbered line window", async () => {
    const env = await handle(ctx, { target: `${A_PATH}::Widget.render`, what: "source" });

    expect(env.status).toBe("ok");
    const payload = sourceOf(env);
    expect(payload.source_drift).toBe(false);
    expect(payload.path).toBe(A_PATH);
    expect(payload.window).toEqual([5, 8]);
    expect(payload.content?.map((line) => line.line)).toEqual([5, 6, 7, 8]);
    expect(at(payload.content ?? [], 0).text).toBe("const v5 = 5;");
    expect(env.read_status.stale).toBe(false);
    expect(env.summary).toBe(`lines 5-8 of ${A_PATH}.`);
  });

  test("a modified file reports source_drift and no content", async () => {
    const env = await handle(ctx, { target: `${DRIFTED_PATH}::drifted`, what: "source" });

    expect(env.status).toBe("ok");
    const payload = sourceOf(env);
    expect(payload.source_drift).toBe(true);
    expect(payload.content).toBeNull();
    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toBe(`"${DRIFTED_PATH}" changed since the index`);
    expect(env.read_status.stale_checked_at).toBe("2026-01-03T00:05:00.000Z");
    expect(env.summary).toBe(`${DRIFTED_PATH} changed since the index: no source returned.`);
    // A stale answer may not claim an exact total.
    expect(env.totals.total_relation).toBe("gte");
  });

  test("a file that vanished since the index is drift too, not an error", async () => {
    const fixture = makeFixture();
    const drifted = fixture.files.get(DRIFTED_PATH);
    if (drifted === undefined) {
      throw new Error("fixture is missing the drifted file row");
    }
    const reader: GraphReader = {
      ...fixture.reader,
      fileByPath: (path) =>
        path === "src/gone.ts"
          ? { ...drifted, path: "src/gone.ts" }
          : (fixture.files.get(path) ?? null),
      findByQn: (qn) =>
        qn === "src/gone.ts::gone"
          ? nodeOf({
              id: 21,
              name: "gone",
              qn,
              path: "src/gone.ts",
              fileId: FILE_DRIFTED,
              parentId: null,
              startLine: 1,
              endLine: 2,
            })
          : fixture.reader.findByQn(qn),
    };
    const env = await handle(makeContext(reader, repoRoot), {
      target: "src/gone.ts::gone",
      what: "source",
    });

    expect(env.status).toBe("ok");
    const payload = sourceOf(env);
    expect(payload.source_drift).toBe(true);
    expect(payload.content).toBeNull();
    expect(env.read_status.stale_reason).toBe(`"src/gone.ts" is no longer readable on disk`);
  });

  test("a stored path that escapes the repo root is refused instead of read", async () => {
    const fixture = makeFixture();
    const reader: GraphReader = {
      ...fixture.reader,
      findByQn: (qn) =>
        qn === "../secrets.ts::leak"
          ? nodeOf({
              id: 22,
              name: "leak",
              qn,
              path: "../secrets.ts",
              fileId: FILE_A,
              parentId: null,
              startLine: 1,
              endLine: 2,
            })
          : fixture.reader.findByQn(qn),
      fileByPath: (path) =>
        path === "../secrets.ts"
          ? fileRow(FILE_A, "../secrets.ts", "c".repeat(64), 2)
          : (fixture.files.get(path) ?? null),
    };
    const env = await handle(makeContext(reader, repoRoot), {
      target: "../secrets.ts::leak",
      what: "source",
    });

    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("internal");
    expect(env.error?.message).toContain("outside the repository root");
  });
});

describe("expand: refusals", () => {
  let ctx: ToolContext;

  beforeAll(() => {
    repoRoot = mkdtempSync(join(tmpdir(), "codegraph-expand-refusals-"));
    dirs.push(repoRoot);
    aSha = sha256Of(A_SOURCE);
    driftedIndexedSha = sha256Of("function drifted() {\n  return 1;\n}\n");
    ctx = makeContext(makeFixture().reader, repoRoot);
  });

  afterAll(() => {
    for (const dir of dirs) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("an unknown target is not_found with the accepted forms", async () => {
    const env = await handle(ctx, { target: "nowhere::nothing" });

    expect(env.status).toBe("not_found");
    const payload = missingOf(env);
    expect(payload.target).toBe("nowhere::nothing");
    expect(payload.hint).toContain("qualified name");
    expect(env.totals.total_relation).toBe("gte");
  });

  test("a bare numeric id is refused with a hint rather than guessed at", async () => {
    const env = await handle(ctx, { target: "1006" });

    expect(env.status).toBe("not_found");
    const payload = missingOf(env);
    expect(payload.why).toBe("a bare numeric id cannot be resolved");
    expect(payload.hint).toContain("numeric id");
    expect(payload.hint).toContain("qualified name");
  });

  test("a folder asked for source is not_found, naming coverage as the folder's mode", async () => {
    const env = await handle(ctx, { target: FOLDER_PREFIX, what: "source" });

    expect(env.status).toBe("not_found");
    expect(missingOf(env).hint).toBe("pass what=coverage for a folder");
  });

  test("a tie is ambiguous with candidates and no answer body", async () => {
    const env = await handle(ctx, { target: "dup" });

    expect(env.status).toBe("ambiguous");
    const payload = ambiguousOf(env);
    expect(payload.candidates.map((candidate) => candidate.qn).sort()).toEqual(
      [`${A_PATH}::Widget.dup`, `${A_PATH}::dup`].sort(),
    );
    expect(env.totals.candidates_total).toBe(2);
  });

  test("an unpublished graph is bootstrapping, not not_found", async () => {
    const fixture = makeFixture();
    const reader: GraphReader = {
      ...fixture.reader,
      metaFor: () => ({
        projectName: null,
        projectRoot: null,
        generation: null,
        indexedAt: null,
        indexerState: "running",
        consecutiveFailures: 0,
      }),
      latestGeneration: () => null,
    };
    const env = await handle(makeContext(reader, repoRoot), { target: A_PATH });

    expect(env.status).toBe("bootstrapping");
    expect(env.error?.code).toBe("bootstrapping");
    expect(env.error?.retry_after_ms).toBe(5000);
  });

  test("a budget above the protocol ceiling is an error, not a silently clamped page", async () => {
    const env = await handle(ctx, { target: `${A_PATH}::Widget`, budget: 50_000 });

    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("budget_too_large");
  });
});

describe("expand: budget", () => {
  let ctx: ToolContext;

  beforeAll(() => {
    repoRoot = mkdtempSync(join(tmpdir(), "codegraph-expand-budget-"));
    dirs.push(repoRoot);
    aSha = sha256Of(A_SOURCE);
    driftedIndexedSha = sha256Of("function drifted() {\n  return 1;\n}\n");
    ctx = makeContext(makeFixture().reader, repoRoot);
  });

  afterAll(() => {
    for (const dir of dirs) {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("a budget below the envelope's floor refuses with the floor rather than paging forever", async () => {
    const whole = await handle(ctx, { target: `${A_PATH}::Widget` });
    const env = await handle(ctx, { target: `${A_PATH}::Widget`, budget: 1 });

    // Below the floor no cut can reach the allowance, so `emit` returns the envelope untouched
    // rather than a drained copy labelled `truncated`. Serving that would hand the caller a payload
    // many times its allowance, so the answer is the refusal and the floor to raise to.
    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("budget_too_large");
    expect(env.error?.hint).toContain("tokens");
    expect(env.next).toBeNull();
    expect(env.truncated).toBe(false);
    expect(env.totals.members_total).toBe(whole.totals.members_total);
  });

  test("every budget is either served whole or refused: never a cut that misses its allowance", async () => {
    const whole = await handle(ctx, { target: `${A_PATH}::Widget` });
    const wholeTokens = Math.ceil(
      Buffer.byteLength(JSON.stringify(whole), "utf8") / BYTES_PER_TOKEN,
    );
    const served: number[] = [];
    const refused: number[] = [];

    for (let budget = 1; budget <= wholeTokens + 2; budget += 1) {
      const env = await handle(ctx, { target: `${A_PATH}::Widget`, budget });
      if (env.status === "error") {
        // Below the floor the answer is the refusal and the number to raise the budget to, not a
        // payload many times the allowance labelled as a budget cut.
        expect(env.error?.code).toBe("budget_too_large");
        expect(env.next).toBeNull();
        expect(env.truncated).toBe(false);
        refused.push(budget);
        continue;
      }
      // This fixture's answer is small enough that anything above its floor carries it whole, so a
      // served page is the whole payload with nothing claimed or minted. Truncation itself, and the
      // signed token `emit` mints, are exercised where a page can actually split: `tools-trace`
      // (dense depth-2 pagination) and `tools-search` (paged results).
      expect(env.truncated).toBe(false);
      expect(env.truncation_reason).toBeNull();
      expect(env.next).toBeNull();
      expect(env.payload).toEqual(whole.payload);
      served.push(budget);
    }

    // The regime flips exactly once, and both sides of the flip were observed.
    expect(refused.length).toBeGreaterThan(0);
    expect(served.length).toBeGreaterThan(0);
    expect(Math.max(...refused)).toBeLessThan(Math.min(...served));
  });
});

describe("expand: typed arguments", () => {
  test("ExpandArgs accepts the three declared fields and nothing more", () => {
    const args: ExpandArgs = { target: "src/a.ts::Widget", what: "members", budget: 100 };
    expect(Object.keys(args)).toEqual(["target", "what", "budget"]);
    const bare: ExpandArgs = { target: "src/a.ts" };
    expect(bare.what).toBeUndefined();
  });
});
