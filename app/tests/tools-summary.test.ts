import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import { log } from "#shared/log";
import type { ToolContext } from "#shared/store/read/context";
import type {
  GraphReader,
  IndexedFileRow,
  NeighbourEdge,
  NodeRow,
} from "#shared/store/read/reader";
import {
  type AreaPayload,
  type ChangePayload,
  handle,
  type RepoPayload,
} from "../src/tools/summary/handler";

/**
 * `summary` against a fake reader (`TOOLS-SPEC.md` §4).
 *
 * The three scopes are asserted on their documented key sets, because "one shape, three scopes"
 * is the contract an agent parses: the envelope is identical and only `payload` varies.
 *
 * The change scope runs the real `#shared/domain/diff` over a temp directory that is not a git
 * repository, so it degrades to the hash comparison — the same path the integration fixture
 * takes. Nothing shells out to a working tree of ours and nothing touches the network: the
 * fixture file's stored digest is wrong on purpose, which is exactly how the indexer reports a
 * changed file.
 */

const ROOT = mkdtempSync(join(tmpdir(), "codegraph-summary-"));
const FIXTURE_PATH = "src/a.ts";
const FIXTURE_BYTES = "export function alpha(): number {\n  return 1;\n}\n";

mkdirSync(join(ROOT, "src"), { recursive: true });
writeFileSync(join(ROOT, FIXTURE_PATH), FIXTURE_BYTES, "utf8");

afterAll(() => {
  rmSync(ROOT, { recursive: true, force: true });
});

const GENERATION = "g-2";
const OLD_GENERATION = "g-1";
const INDEXED_AT = "2026-01-03T00:00:00Z";
const CURSOR_KEY = "k".repeat(32);
const EMPTY_SHA = "0".repeat(64);

/** Long enough that a 1200-token budget is plenty and a small one cuts mid-list. */
const NAMES = [
  "resolve_bare_call_targets_for_the_alpha_branch_01",
  "resolve_bare_call_targets_for_the_alpha_branch_02",
  "resolve_bare_call_targets_for_the_alpha_branch_03",
  "resolve_bare_call_targets_for_the_alpha_branch_04",
  "resolve_bare_call_targets_for_the_alpha_branch_05",
  "resolve_bare_call_targets_for_the_alpha_branch_06",
  "resolve_bare_call_targets_for_the_alpha_branch_07",
  "resolve_bare_call_targets_for_the_alpha_branch_08",
  "resolve_bare_call_targets_for_the_alpha_branch_09",
  "resolve_bare_call_targets_for_the_alpha_branch_10",
  "resolve_bare_call_targets_for_the_alpha_branch_11",
  "resolve_bare_call_targets_for_the_alpha_branch_12",
];

function node(id: number, name: string): NodeRow {
  return {
    id,
    fileId: 1,
    path: FIXTURE_PATH,
    parentId: null,
    kind: "function",
    subkind: null,
    name,
    qn: `${FIXTURE_PATH}::${name}`,
    startLine: id,
    endLine: id + 2,
    signature: null,
    docstring: null,
    isTest: false,
    props: {},
  };
}

const NODES: readonly NodeRow[] = NAMES.map((name, index) => node(index + 1, name));

interface ReaderOptions {
  /** The generation the file row was written by; the default is the served generation. */
  readonly indexedGeneration?: string;
  /** Reach the reader's entry-point statement bound, which is what `item_cap` reports. */
  readonly entryPointsAtLimit?: boolean;
}

/** A hand-written reader: every method present, types only from `#shared/*`. */
function makeReader(options: ReaderOptions = {}): GraphReader {
  const file: IndexedFileRow = {
    id: 1,
    path: FIXTURE_PATH,
    language: "typescript",
    // Deliberately not the digest of the bytes on disk: the hash comparison reports it changed.
    sha256: EMPTY_SHA,
    size: Buffer.byteLength(FIXTURE_BYTES, "utf8"),
    lineCount: 3,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: options.indexedGeneration ?? GENERATION,
  };

  return {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: (fileId) => (fileId === file.id ? [...NODES] : []),
    inbound: () => [],
    outbound: (): NeighbourEdge[] => [],
    metricsFor: (nodeId) => ({
      nodeId,
      inDegree: nodeId === 1 ? 9 : nodeId === 2 ? 4 : 0,
      outDegree: 1,
      isEntry: nodeId === 1,
      testedDepth: nodeId === 2 ? null : 1,
      reachableCount: nodeId === 1 ? 14 : null,
    }),
    rowsForPaths: () => ({
      indexed: [file],
      notIndexed: [
        {
          id: 1,
          path: "docs/legacy.xslt",
          reason: "grammar_unavailable",
          detail: null,
          language: "xslt",
        },
      ],
    }),
    unresolvedForFiles: () => [],
    fileByPath: (path) => (path === FIXTURE_PATH ? file : null),
    allFiles: () => [file],
    censusByLanguage: () => [{ language: "typescript", files: 1, parsed: 1, failed: 0 }],
    metaFor: () => ({
      projectName: "fake",
      projectRoot: ROOT,
      generation: GENERATION,
      indexedAt: INDEXED_AT,
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () => ({
      id: GENERATION,
      seq: 2,
      publishedAt: INDEXED_AT,
      baseCommit: null,
      worktreeDirty: false,
      filesIndexed: 1,
      nodes: NODES.length,
      edges: 5,
      durationMs: 12,
      mode: "full",
      status: "ok",
    }),
    removedSymbolsFor: () => [],
    entryPoints: (limit) => {
      const rows = (options.entryPointsAtLimit === true ? NAMES : [NAMES[0] ?? ""]).map(
        (name, index) => ({
          id: index + 1,
          qn: `${FIXTURE_PATH}::${name}`,
          kind: "function",
          file: FIXTURE_PATH,
          lines: [1, 3] as const,
          reachable: 14,
        }),
      );
      return rows.slice(0, limit);
    },
    hotspots: (limit) =>
      [
        {
          id: 1,
          qn: `${FIXTURE_PATH}::${NAMES[0] ?? ""}`,
          kind: "function",
          file: FIXTURE_PATH,
          lines: [1, 3] as const,
          inDegree: 9,
          outDegree: 1,
        },
      ].slice(0, limit),
    testGaps: (ids) =>
      ids.includes(2)
        ? [
            {
              id: 2,
              qn: `${FIXTURE_PATH}::${NAMES[1] ?? ""}`,
              kind: "function",
              file: FIXTURE_PATH,
              lines: [2, 4] as const,
              testedDepth: null,
            },
          ]
        : [],
    readNodeFts: () => [],
  };
}

function contextWith(reader: GraphReader): ToolContext {
  return {
    reader,
    cursor: createCursors({ key: CURSOR_KEY }),
    log,
    now: () => new Date("2026-01-03T00:00:05Z"),
    config: loadConfig({
      argv: ["--repo-root", ROOT],
      env: { CODEGRAPH_CURSOR_KEY: CURSOR_KEY },
    }),
  };
}

describe("summary: change", () => {
  test("returns exactly the documented keys, and drivers are sentences", async () => {
    const env = await handle(contextWith(makeReader()), {});
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    expect(Object.keys(payload).sort()).toEqual([
      "base",
      "changed_files",
      "changed_symbols",
      "removed_symbols",
      "risk",
      "suggested_next",
      "test_gaps",
    ]);
    // The diff degraded to the hash comparison, so the file with a wrong stored digest is the
    // change set — the same answer the integration fixture gets in a plain directory.
    expect(payload.changed_files).toEqual([FIXTURE_PATH]);
    expect(payload.changed_symbols).toHaveLength(NODES.length);
    expect(payload.changed_symbols[0]?.risk).toBeGreaterThanOrEqual(
      payload.changed_symbols[1]?.risk ?? 0,
    );
    expect(payload.changed_symbols[0]?.lines).toHaveLength(2);
    expect(payload.test_gaps).toEqual([
      { qn: `${FIXTURE_PATH}::${NAMES[1]}`, reason: "no tested_by edge within 2 hops" },
    ]);
    expect(payload.removed_symbols).toEqual([]);
    expect(payload.suggested_next.length).toBeGreaterThan(0);
    for (const suggestion of payload.suggested_next) {
      expect(suggestion.tool.length).toBeGreaterThan(0);
      expect(suggestion.why.length).toBeGreaterThan(0);
    }

    // A driver is quoteable text, never a bare float (`TOOLS-SPEC.md` §4).
    expect(payload.risk.drivers.length).toBeGreaterThan(0);
    for (const driver of payload.risk.drivers) {
      expect(driver).not.toMatch(/^-?\d+(\.\d+)?$/);
      expect(driver.length).toBeGreaterThan(10);
    }

    expect(env.totals.changed_files_total).toBe(1);
    expect(env.totals.changed_symbols_total).toBe(NODES.length);
    expect(env.totals.total_relation).toBe("gte");
    expect(env.next_tools).toEqual(["impact", "trace", "expand"]);
    // The default 1200-token budget carries this payload whole; only a smaller one pages it.
    expect(env.truncated).toBe(false);
    expect(env.truncation_reason).toBeNull();
  });

  test("a working-tree change is a stale answer, not a failure", async () => {
    const env = await handle(contextWith(makeReader()), {});
    expect(env.status).toBe("ok");
    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toBe("1 file changed since index");
    expect(env.read_status.indexed_at).toBe(INDEXED_AT);
  });
});

describe("summary: repo", () => {
  test("returns exactly the documented keys", async () => {
    const env = await handle(contextWith(makeReader()), { scope: "repo" });
    expect(env.status).toBe("ok");

    const payload = env.payload as RepoPayload;
    expect(Object.keys(payload).sort()).toEqual([
      "coupling_hotspots",
      "entry_points",
      "flows",
      "languages",
      "not_indexed",
      "size",
    ]);
    expect(payload.size).toEqual({ files: 1, nodes: NODES.length, edges: 5 });
    expect(payload.languages).toEqual([{ language: "typescript", files: 1 }]);
    expect(payload.entry_points).toEqual([
      { qn: `${FIXTURE_PATH}::${NAMES[0]}`, kind: "function", reachable: 14 },
    ]);
    expect(payload.coupling_hotspots).toEqual([
      { qn: `${FIXTURE_PATH}::${NAMES[0]}`, in: 9, out: 1 },
    ]);
    expect(payload.flows).toEqual([{ name: `${FIXTURE_PATH}::${NAMES[0]}`, steps: 14, files: 0 }]);
    expect(payload.not_indexed).toEqual([
      { path: "docs/legacy.xslt", reason: "grammar_unavailable" },
    ]);
    expect(env.totals.not_indexed_total).toBe(1);
  });

  test("a bounded entry-point list is reported as an item cap", async () => {
    const env = await handle(contextWith(makeReader({ entryPointsAtLimit: true })), {
      scope: "repo",
    });
    expect(env.status).toBe("ok");
    expect(env.truncated).toBe(true);
    expect(env.truncation_reason).toBe("item_cap");
    expect(env.totals.total_relation).toBe("gte");
    expect((env.payload as RepoPayload).entry_points).toHaveLength(10);
  });

  test("a file row from an older generation is stale without failing", async () => {
    const env = await handle(contextWith(makeReader({ indexedGeneration: OLD_GENERATION })), {
      scope: "repo",
    });
    expect(env.status).toBe("ok");
    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toBe("1 file in scope was indexed by an older generation");
  });
});

describe("summary: area", () => {
  test("returns exactly the documented keys for one path", async () => {
    const env = await handle(contextWith(makeReader()), { scope: "area", target: "src" });
    expect(env.status).toBe("ok");

    const payload = env.payload as AreaPayload;
    expect(Object.keys(payload).sort()).toEqual(["dependents_total", "risk", "surface", "target"]);
    expect(payload.target).toBe("src");
    expect(payload.surface).toHaveLength(NODES.length);
    expect(Object.keys(payload.surface[0] ?? {}).sort()).toEqual(["kind", "qn"]);
    // 9 from the entry symbol plus 4 from the untested one; the rest measure zero.
    expect(payload.dependents_total).toBe(13);
    expect(payload.risk.drivers.length).toBeGreaterThan(0);
    expect(env.totals.surface_total).toBe(NODES.length);
  });
});

describe("summary: cursors", () => {
  test("a truncated page is resumed without repeating an item", async () => {
    const ctx = contextWith(makeReader());
    const budget = 600;

    const first = await handle(ctx, { budget });
    expect(first.truncated).toBe(true);
    expect(first.next).not.toBeNull();
    const firstIds = (first.payload as ChangePayload).changed_symbols.map((symbol) => symbol.id);
    expect(firstIds.length).toBeGreaterThan(0);
    expect(firstIds.length).toBeLessThan(NODES.length);

    // Same budget, so the call identity the cursor pins is unchanged.
    const second = await handle(ctx, { budget, cursor: first.next ?? "" });
    expect(second.status).toBe("ok");
    const secondIds = (second.payload as ChangePayload).changed_symbols.map((symbol) => symbol.id);
    expect(secondIds.length).toBeGreaterThan(0);
    expect(secondIds.filter((id) => firstIds.includes(id))).toEqual([]);

    // Totals are the whole sequence's, not this page's.
    expect(second.totals.changed_symbols_total).toBe(NODES.length);
    expect(second.totals.total_relation).toBe("gte");
  });

  test("draining the pages yields every symbol once", async () => {
    const ctx = contextWith(makeReader());
    const budget = 600;
    const seen: number[] = [];
    let cursor: string | null = null;
    for (let page = 0; page < NODES.length + 2; page += 1) {
      const args = cursor === null ? { budget } : { budget, cursor };
      const env = await handle(ctx, args);
      for (const symbol of (env.payload as ChangePayload).changed_symbols) {
        seen.push(symbol.id);
      }
      cursor = env.next;
      if (cursor === null) {
        break;
      }
    }
    expect(new Set(seen).size).toBe(seen.length);
    expect([...seen].sort((a, b) => a - b)).toEqual(NODES.map((row) => row.id));
  });

  test("an unusable cursor is refused before any page is built", async () => {
    const env = await handle(contextWith(makeReader()), { cursor: "not-a-token" });
    expect(env.status).toBe("error");
    expect(env.payload).toBeNull();
    expect(env.error?.code).toBe("cursor_invalid");
    expect(env.error?.hint).toBe("re-run the original call to mint a fresh cursor");
  });

  test("a cursor from another generation is expired, and one from other args mismatches", async () => {
    const ctx = contextWith(makeReader());
    const expired = ctx.cursor.sign({
      generation: OLD_GENERATION,
      tool: "summary",
      argsHash: "sha256:abc",
      position: 1,
    });
    const expiredEnv = await handle(ctx, { cursor: expired });
    expect(expiredEnv.error?.code).toBe("cursor_expired");

    const mismatched = ctx.cursor.sign({
      generation: GENERATION,
      tool: "summary",
      argsHash: "sha256:abc",
      position: 1,
    });
    const mismatchEnv = await handle(ctx, { cursor: mismatched });
    expect(mismatchEnv.error?.code).toBe("cursor_mismatch");
  });
});

describe("summary: bootstrap", () => {
  test("an unpublished database answers bootstrapping instead of failing", async () => {
    const reader = makeReader();
    const empty: GraphReader = {
      ...reader,
      metaFor: () => ({
        projectName: null,
        projectRoot: ROOT,
        generation: null,
        indexedAt: null,
        indexerState: "running",
        consecutiveFailures: 0,
      }),
      latestGeneration: () => null,
    };
    const env = await handle(contextWith(empty), {});
    expect(env.status).toBe("bootstrapping");
    expect(env.error?.code).toBe("bootstrapping");
    expect(env.error?.retry_after_ms).toBeGreaterThan(0);
  });
});

/** Keeps the fixture honest: the file the reader reports must exist on disk to be hashed. */
void readFileSync(join(ROOT, FIXTURE_PATH), "utf8");
