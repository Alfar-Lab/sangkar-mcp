import { afterAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import type { Envelope } from "#shared/envelope";
import { log } from "#shared/log";
import { isUnderPrefix } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type { SkippedPathRow, UnresolvedRef } from "#shared/store/read/coverage";
import type {
  GenerationRow,
  GraphReader,
  IndexedFileRow,
  NodeRow,
} from "#shared/store/read/reader";
import { handle, type StatusEnvelope, type StatusPayload } from "../src/tools/status/handler";
import { input, manifest, STATUS_RETRY_AFTER_MS } from "../src/tools/status/manifest";

/**
 * `status` against a fake reader (TOOLS-SPEC.md §2).
 *
 * The tool's whole value is answering the trust question *including for paths that were never
 * indexed*, so the three required cases are the three ways a requested path can come back:
 * indexed, skipped with the indexer's reason, and unknown to the index entirely. A fourth case
 * covers the census rule the spec calls out — a language whose grammar never loaded must read as
 * `parsed: 0` plus a note, never as an empty language.
 *
 * Nothing here opens a database or a network socket. The reader is a hand-written object over plain
 * arrays, and the two directory paths are temp dirs: one holds a `graph.db` of known size (so
 * `graph.size_bytes` is asserted against real bytes rather than against a mock), the other is the
 * repo root the config reports.
 */

const DATA_DIR = mkdtempSync(join(tmpdir(), "codegraph-status-data-"));
const REPO_DIR = mkdtempSync(join(tmpdir(), "codegraph-status-repo-"));
const DB_PATH = join(DATA_DIR, "graph.db");
const DB_BYTES = 2048;

writeFileSync(DB_PATH, "x".repeat(DB_BYTES));

afterAll(() => {
  rmSync(DATA_DIR, { recursive: true, force: true });
  rmSync(REPO_DIR, { recursive: true, force: true });
});

const CURSOR_KEY = "status-test-cursor-key-0123456789ab";
const NOW = "2026-11-17T14:02:00Z";
const NOW_ISO = "2026-11-17T14:02:00.000Z";

const GENERATION = "g-7f3a91";
const OLDER_GENERATION = "g-700001";
const PUBLISHED_AT = "2026-11-17T13:50:15Z";
const BASE_COMMIT = "bbdda4b9";

const CONFIG = loadConfig({
  env: {
    CODEGRAPH_REPO_ROOT: REPO_DIR,
    CODEGRAPH_DATA_DIR: DATA_DIR,
    CODEGRAPH_CURSOR_KEY: CURSOR_KEY,
    CODEGRAPH_PROFILE: "agent",
  },
});

/** The nine fields every response carries, whatever its status (TOOLS-SPEC.md §1.1). */
const REQUIRED_FIELDS = [
  "status",
  "summary",
  "read_status",
  "coverage",
  "totals",
  "truncated",
  "truncation_reason",
  "next",
  "next_tools",
] as const;

/** One `file` row; `parsed` defaults to the current generation, as a clean publish leaves it. */
function fileRow(
  id: number,
  path: string,
  language: string,
  parseStatus = "ok",
  generation = GENERATION,
): IndexedFileRow {
  return {
    id,
    path,
    language,
    sha256: `sha-${id}`.padEnd(64, "0"),
    size: 100 * id,
    lineCount: 10 * id,
    parseStatus,
    parseErrorRanges: parseStatus === "ok" ? null : "12-12",
    indexedGeneration: generation,
  };
}

function skippedRow(
  id: number,
  path: string,
  reason: string,
  language: string | null,
): SkippedPathRow {
  return { id, path, reason, detail: null, language };
}

interface Fixture {
  readonly files: readonly IndexedFileRow[];
  readonly notIndexed?: readonly SkippedPathRow[];
  /** Symbols per file id; absent means the file defines none. */
  readonly symbols?: Readonly<Record<number, number>>;
  readonly unresolved?: readonly UnresolvedRef[];
  /** `null` models a database that has never published. */
  readonly generation?: string | null;
  readonly indexerState?: string | null;
  readonly consecutiveFailures?: number;
  /** What the fake handle's statement cache holds; absent models a reader without one. */
  readonly statementCacheEntries?: number;
}

/**
 * The census the store would return for these rows. `files.ts` folds `not_indexed` in — a skipped
 * path is one more file of its language that failed — and that identity is what the handler's
 * `skipped` (and its `complete: false`) arithmetic depends on, so the fake reproduces it rather
 * than handing the handler numbers that could not come out of the store.
 */
function censusFor(files: readonly IndexedFileRow[], skipped: readonly SkippedPathRow[]) {
  const buckets = new Map<string | null, { files: number; parsed: number; failed: number }>();
  const bucket = (language: string | null) => {
    const existing = buckets.get(language);
    if (existing !== undefined) {
      return existing;
    }
    const fresh = { files: 0, parsed: 0, failed: 0 };
    buckets.set(language, fresh);
    return fresh;
  };
  for (const file of files) {
    const counts = bucket(file.language);
    counts.files += 1;
    if (file.parseStatus === "failed") {
      counts.failed += 1;
    } else {
      counts.parsed += 1;
    }
  }
  for (const row of skipped) {
    const counts = bucket(row.language);
    counts.files += 1;
    counts.failed += 1;
  }
  return [...buckets.entries()]
    .sort(([a], [b]) => {
      if (a === b) {
        return 0;
      }
      if (a === null) {
        return 1;
      }
      if (b === null) {
        return -1;
      }
      return a < b ? -1 : 1;
    })
    .map(([language, counts]) => ({ language, ...counts }));
}

function nodeRow(id: number, file: IndexedFileRow): NodeRow {
  return {
    id,
    fileId: file.id,
    path: file.path,
    parentId: null,
    kind: "function",
    subkind: null,
    name: `symbol_${id}`,
    qn: `${file.path}::symbol_${id}`,
    startLine: 1,
    endLine: 2,
    signature: null,
    docstring: null,
    isTest: false,
    props: {},
  };
}

function generationRowOf(files: readonly IndexedFileRow[]): GenerationRow {
  return {
    id: GENERATION,
    seq: 7,
    publishedAt: PUBLISHED_AT,
    baseCommit: BASE_COMMIT,
    worktreeDirty: false,
    filesIndexed: files.length,
    nodes: 13_955,
    edges: 130_044,
    durationMs: 41_200,
    mode: "incremental",
    status: "published",
  };
}

/** A hand-written reader; every method present, and only `#shared/*` types in its signatures. */
function fakeReader(fixture: Fixture): GraphReader {
  const files = fixture.files;
  const notIndexed = fixture.notIndexed ?? [];
  const census = censusFor(files, notIndexed);
  const generation = fixture.generation === undefined ? GENERATION : fixture.generation;
  const generationRow = generationRowOf(files);

  return {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: (fileId) => {
      const file = files.find((row) => row.id === fileId);
      if (file === undefined) {
        return [];
      }
      const count = fixture.symbols?.[fileId] ?? 0;
      return Array.from({ length: count }, (_unused, index) => nodeRow(index + 1, file));
    },
    inbound: () => [],
    outbound: () => [],
    metricsFor: () => null,
    rowsForPaths: (prefixes) => ({
      indexed: files.filter((file) => prefixes.some((prefix) => isUnderPrefix(file.path, prefix))),
      notIndexed: notIndexed.filter((row) =>
        prefixes.some((prefix) => isUnderPrefix(row.path, prefix)),
      ),
    }),
    unresolvedForFiles: (fileIds) =>
      (fixture.unresolved ?? []).filter((row) => fileIds.includes(row.fileId)),
    fileByPath: (path) => files.find((file) => file.path === path) ?? null,
    allFiles: () => [...files],
    censusByLanguage: () => census,
    metaFor: () => ({
      projectName: "codegraph",
      projectRoot: REPO_DIR,
      generation,
      indexedAt: generation === null ? null : PUBLISHED_AT,
      indexerState: fixture.indexerState === undefined ? "idle" : fixture.indexerState,
      consecutiveFailures: fixture.consecutiveFailures ?? 0,
    }),
    latestGeneration: () => (generation === null ? null : generationRow),
    // A real reader reports its own `store/db.ts` cache; a hand-written fake reports what its
    // fixture says, so `health.statement_cache_entries` is asserted rather than assumed.
    statementCacheEntries: () => fixture.statementCacheEntries ?? 0,
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };
}

/** The seam every tool task shares: a reader, a cursor pair, a logger, a clock and a config. */
function contextFor(fixture: Fixture): ToolContext {
  return {
    reader: fakeReader(fixture),
    cursor: createCursors({ key: CURSOR_KEY }),
    log,
    now: () => new Date(NOW),
    config: CONFIG,
  };
}

function expectNineFields(env: Envelope<unknown>): void {
  for (const field of REQUIRED_FIELDS) {
    expect(Object.hasOwn(env, field)).toBe(true);
  }
}

/** Narrows to the payload, so a wrong status fails with the summary that explains it. */
function expectOk(env: StatusEnvelope): StatusPayload {
  if (env.status !== "ok" || env.payload === null) {
    throw new Error(`expected an ok envelope with a payload, got ${env.status}: ${env.summary}`);
  }
  return env.payload;
}

const CLEAN: Fixture = {
  files: [fileRow(1, "src/a.ts", "typescript"), fileRow(2, "src/b.py", "python")],
  symbols: { 1: 2, 2: 1 },
};

const MESSY: Fixture = {
  files: [
    fileRow(1, "src/a.ts", "typescript"),
    fileRow(2, "src/b.py", "python"),
    fileRow(3, "src/c.py", "python", "failed"),
  ],
  notIndexed: [
    skippedRow(10, "assets/logo.png", "binary", null),
    skippedRow(11, "docs/legacy.xslt", "grammar_unavailable", "xslt"),
    skippedRow(12, "docs/older.xslt", "grammar_unavailable", "xslt"),
  ],
  unresolved: [
    {
      id: 91,
      fileId: 1,
      path: "src/a.ts",
      line: 12,
      rawTarget: "mystery",
      evidenceKind: null,
      evidence: null,
    },
    {
      id: 92,
      fileId: 2,
      path: "src/b.py",
      line: 7,
      rawTarget: "other",
      evidenceKind: null,
      evidence: null,
    },
  ],
};

describe("status: complete coverage", () => {
  test("answers ok with all nine fields and the documented payload", () => {
    const env = handle(contextFor(CLEAN), { paths: ["src/a.ts"] });
    const payload = expectOk(env);

    expectNineFields(env);
    expect(env.status).toBe("ok");
    expect(Object.hasOwn(env, "error")).toBe(false);
    expect(env.summary).toBe(
      "2 files indexed, generation g-7f3a91. 1 of 1 requested paths indexed.",
    );
    expect(env.truncated).toBe(false);
    expect(env.truncation_reason).toBeNull();
    expect(env.next).toBeNull();

    expect(env.read_status).toEqual({
      generation: GENERATION,
      indexed_at: PUBLISHED_AT,
      stale: false,
      stale_checked_at: NOW_ISO,
    });
    // Complete coverage, no unresolved call sites and a fresh generation: the only state that may
    // claim an exact total.
    expect(env.coverage).toEqual({
      complete: true,
      indexed: 2,
      skipped: 0,
      failed: 0,
      unresolved_edges: 0,
      not_indexed: [],
    });
    expect(env.totals).toEqual({ total_relation: "eq", paths_total: 1 });

    expect(payload.project).toEqual({
      name: "codegraph",
      root: REPO_DIR,
      commit: BASE_COMMIT,
      // Derived from `ctx.config.dataDir`: the reader deliberately exposes no database path.
      db_path: DB_PATH,
    });
    expect(payload.graph).toEqual({
      nodes: 13_955,
      edges: 130_044,
      size_bytes: DB_BYTES,
      db_path: DB_PATH,
    });
    expect(payload.languages).toEqual([
      { language: "python", files: 1, parsed: 1, failed: 0 },
      { language: "typescript", files: 1, parsed: 1, failed: 0 },
    ]);
    expect(payload.paths).toEqual([
      { path: "src/a.ts", indexed: true, sha256: `sha-1`.padEnd(64, "0"), symbols: 2 },
    ]);
    expect(payload.health).toEqual({
      indexer: "idle",
      last_publish_ms: 41_200,
      consecutive_failures: 0,
      statement_cache_entries: 0,
    });
  });

  test("a request with no paths answers about the whole repository", () => {
    const env = handle(contextFor(CLEAN));
    const payload = expectOk(env);
    expect(env.summary).toBe("2 files indexed, generation g-7f3a91. Whole repository.");
    expect(payload.paths).toEqual([]);
    expect(env.totals).toEqual({ total_relation: "eq", paths_total: 0 });
  });

  test("a directory request expands to the files and skips under it", () => {
    const env = handle(contextFor(MESSY), { paths: ["docs"] });
    const payload = expectOk(env);

    // Every entry is a real row: a prefix has no digest and no symbol count of its own, so it is
    // answered at file granularity instead of pretending to be one file.
    expect(payload.paths).toEqual([
      { path: "docs/legacy.xslt", indexed: false, reason: "grammar_unavailable" },
      { path: "docs/older.xslt", indexed: false, reason: "grammar_unavailable" },
    ]);
    for (const entry of payload.paths) {
      expect(Object.hasOwn(entry, "symbols")).toBe(false);
    }
    expect(env.summary).toBe(
      "3 files indexed, generation g-7f3a91, 3 skipped, 1 failed to parse. 0 of 1 requested paths indexed.",
    );
  });
});

describe("status: paths that were never parsed", () => {
  test("a skipped path comes back with its reason, not as a zero", () => {
    const env = handle(contextFor(MESSY), { paths: ["docs/legacy.xslt"] });
    const payload = expectOk(env);

    expect(payload.paths).toEqual([
      { path: "docs/legacy.xslt", indexed: false, reason: "grammar_unavailable" },
    ]);
    // The skipped list is scope-limited to the requested paths while the counts are repo-wide, which
    // is why `skipped: 3` and a one-element `not_indexed` describe the same database.
    expect(env.coverage).toEqual({
      complete: false,
      indexed: 3,
      skipped: 3,
      failed: 1,
      unresolved_edges: 2,
      not_indexed: [{ path: "docs/legacy.xslt", reason: "grammar_unavailable" }],
    });
    expect(env.totals.total_relation).toBe("gte");
  });

  test("the census reports a language that parsed nothing with a note", () => {
    const env = handle(contextFor(MESSY), { paths: ["docs/legacy.xslt"] });
    const payload = expectOk(env);

    expect(payload.languages.find((row) => row.language === "xslt")).toEqual({
      language: "xslt",
      files: 2,
      parsed: 0,
      failed: 2,
      note: "grammar_unavailable",
    });
    // The unattributed bucket keeps `null` and gets no note: the note names a language, and a
    // skipped path whose grammar could not be identified has no name to attach it to.
    expect(payload.languages.find((row) => row.language === null)).toEqual({
      language: null,
      files: 1,
      parsed: 0,
      failed: 1,
    });
    expect(payload.languages.find((row) => row.language === "python")).toEqual({
      language: "python",
      files: 2,
      parsed: 1,
      failed: 1,
    });

    // The arithmetic the handler's coverage relies on, checked against the census it was given.
    const censusFiles = payload.languages.reduce((total, row) => total + row.files, 0);
    const censusFailed = payload.languages.reduce((total, row) => total + row.failed, 0);
    expect(censusFiles).toBe(env.coverage.indexed + env.coverage.skipped);
    expect(censusFailed).toBe(env.coverage.failed + env.coverage.skipped);
  });

  test("a path in neither file nor not_indexed is reported as not_indexed_unknown", () => {
    const env = handle(contextFor(MESSY), { paths: ["src/missing.ts"] });
    const payload = expectOk(env);

    expect(payload.paths).toEqual([
      { path: "src/missing.ts", indexed: false, reason: "not_indexed_unknown" },
    ]);
    expect(env.summary).toContain("0 of 1 requested paths indexed");
    expect(env.coverage.complete).toBe(false);
    expect(env.totals.total_relation).toBe("gte");
  });
});

describe("status: freshness and health", () => {
  test("a file row from an earlier generation makes the answer stale, dated", () => {
    const fixture: Fixture = {
      files: [
        fileRow(1, "src/a.ts", "typescript"),
        fileRow(2, "src/b.py", "python", "ok", OLDER_GENERATION),
      ],
    };
    const env = handle(contextFor(fixture), { paths: ["src/a.ts"] });

    expect(env.read_status.stale).toBe(true);
    expect(env.read_status.stale_reason).toBe(
      "1 of 2 indexed files were written by an earlier generation",
    );
    // A dated claim, not an absolute one: the agent can see when the freshness verdict ran.
    expect(env.read_status.stale_checked_at).toBe(NOW_ISO);
    // Stale is a warning, never a failure: the payload is still returned, flagged.
    expect(env.status).toBe("ok");
    expect(env.totals.total_relation).toBe("gte");
    expect(env.summary).toContain("1 written by an earlier generation");
  });

  test("health reports an unrecorded indexer state and a failure counter", () => {
    const env = handle(contextFor({ ...CLEAN, indexerState: null, consecutiveFailures: 3 }));
    const payload = expectOk(env);
    expect(payload.health).toEqual({
      indexer: null,
      last_publish_ms: 41_200,
      consecutive_failures: 3,
      statement_cache_entries: 0,
    });
  });

  test("health reports the reader handle's statement-cache size", () => {
    const env = handle(contextFor({ ...CLEAN, statementCacheEntries: 17 }));
    const payload = expectOk(env);

    // The count comes from the reader (`store/db.ts` `statementCacheStats().size`), not from the
    // handler: `shared` decides the number, `status` only carries it.
    expect(payload.health.statement_cache_entries).toBe(17);
  });
});

describe("status: no published generation", () => {
  test("answers bootstrapping with a retry interval and no payload", () => {
    const env = handle(contextFor({ files: [], generation: null }), { paths: ["src"] });

    expectNineFields(env);
    expect(env.status).toBe("bootstrapping");
    expect(env.payload).toBeNull();
    expect(env.error?.code).toBe("bootstrapping");
    expect(env.error?.retry_after_ms).toBe(STATUS_RETRY_AFTER_MS);
    // No generation id is invented: the empty one is the truthful value for "nothing published".
    expect(env.read_status.generation).toBe("");
    expect(env.coverage.indexed).toBe(0);
    expect(env.coverage.complete).toBe(false);
  });
});

describe("status: budget", () => {
  test("a cut payload keeps every count and mints a resumable next", () => {
    const files = Array.from({ length: 40 }, (_unused, index) =>
      fileRow(index + 1, `src/file_${String(index).padStart(2, "0")}.ts`, "typescript"),
    );
    const ctx = contextFor({ files });
    const paths = files.map((file) => file.path);
    const env = handle(ctx, { paths, budget: 200 });

    expect(env.truncated).toBe(true);
    expect(env.truncation_reason).toBe("budget");
    expect(typeof env.next).toBe("string");
    expect(env.totals.total_relation).toBe("gte");
    // The total survives the cut, so "showing N of M" stays computable.
    expect(env.totals.paths_total).toBe(40);
    const payload = expectOk(env);
    expect(payload.paths.length).toBeLessThan(40);

    // The token is signed over this call's canonical arguments, which is what lets `expand` refuse
    // it for a different generation or a different request.
    const argsHash = `sha256:${createHash("sha256")
      .update(JSON.stringify({ budget: 200, paths }))
      .digest("hex")}`;
    const resumed = ctx.cursor.verify(env.next ?? "", {
      generation: GENERATION,
      tool: "status",
      argsHash,
    });
    // `emit` drops from the largest list first: path entries, then the census line once it is the
    // biggest list left. The position is the count of everything it took out.
    const dropped = 40 - payload.paths.length + (payload.languages.length === 0 ? 1 : 0);
    expect(resumed).toEqual({ ok: true, position: dropped });
  });

  test("a budget above the ceiling is refused as an envelope, not as a throw", () => {
    const env = handle(contextFor(CLEAN), { paths: ["src/a.ts"], budget: 1_000_000 });

    expectNineFields(env);
    expect(env.status).toBe("error");
    expect(env.payload).toBeNull();
    expect(env.error).toEqual({
      code: "budget_too_large",
      message: "budget 1000000 exceeds the 32000 token ceiling",
      hint: "request at most 32000 tokens",
    });
    expect(env.summary).toBe("the requested budget could not be applied");
  });
});

describe("status: manifest", () => {
  test("describes the tool verbatim and declares exactly two optional arguments", () => {
    expect(manifest.name).toBe("status");
    expect(manifest.profile).toBe("agent");
    expect(manifest.description).toBe(
      "Report whether the graph is current and what it covers. Call this before telling the user " +
        "that something does not exist, and whenever a previous answer looked incomplete. Returns " +
        "the index generation, how stale the working tree is, per-path coverage, and the language " +
        "census. Cheap — call it freely.",
    );
    expect(Object.keys(input).sort()).toEqual(["budget", "paths"]);
    expect(input.budget.default).toBe(800);
    expect(input.paths.default).toEqual([]);
  });
});

describe("status: module surface", () => {
  test("the handler imports only #shared/* and its own manifest", async () => {
    const source = await Bun.file(
      join(import.meta.dir, "..", "src/tools/status/handler.ts"),
    ).text();
    const specifiers = new Bun.Transpiler({ loader: "ts" })
      .scan(source)
      .imports.map((entry) => entry.path);
    for (const specifier of specifiers) {
      const allowed =
        specifier.startsWith("node:") ||
        specifier.startsWith("#shared/") ||
        specifier === "./manifest";
      expect(`${specifier}:${allowed}`).toBe(`${specifier}:true`);
    }
    // A handler writes to `ctx.log`, never to stdout: stdout carries the MCP protocol.
    expect(source.includes("console.log")).toBe(false);
  });
});
