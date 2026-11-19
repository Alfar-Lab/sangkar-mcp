import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import { UNRESOLVED_BUCKETS } from "#shared/domain/unresolved";
import type { TestCensus } from "#shared/envelope";
import { log } from "#shared/log";
import { isUnderPrefix } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type {
  GraphReader,
  IndexedFileRow,
  NeighbourEdge,
  NodeRow,
  SkippedPathRow,
  TombstoneRow,
  UnresolvedRef,
} from "#shared/store/read/reader";
import {
  type AreaPayload,
  type ChangedFileRow,
  type ChangePayload,
  handle,
  type RepoPayload,
  type UnresolvedCensus,
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

const ROOT = mkdtempSync(join(tmpdir(), "sangkar-summary-"));
const FIXTURE_PATH = "src/a.ts";
const FIXTURE_BYTES = "export function alpha(): number {\n  return 1;\n}\n";
/** Where the git fixture moves {@link FIXTURE_PATH}, so the rename has a destination to report. */
const RENAMED_PATH = "src/c.ts";
/** An indexed path the fixture leaves no bytes for: the change set's `deleted` entry. */
const DELETED_PATH = "src/gone.ts";

/**
 * What the graph skipped, and why the two counts differ: one path sits under a top-level directory
 * the graph indexed, so a prefix-scoped read returns it, while the other sits under a directory no
 * indexed file lives in — `rowsForPaths` never returns it, and only the graph's own count holds it.
 * That is the pair `#shared/domain/coverage` keeps apart on purpose.
 */
const SKIPPED_ROWS: readonly SkippedPathRow[] = [
  { id: 1, path: "src/vendor.min.js", reason: "size_cap", detail: null, language: "javascript" },
  {
    id: 2,
    path: "docs/legacy.xslt",
    reason: "grammar_unavailable",
    detail: null,
    language: "xslt",
  },
];

/** The indexed test corpus the graph reports; `coverage.tests` carries it in every scope. */
const TESTS_CENSUS: TestCensus = { nodes: 3, languages: [{ language: "typescript", nodes: 3 }] };
const gitDirs: string[] = [];

function git(root: string, ...args: readonly string[]): string {
  const result = Bun.spawnSync({
    cmd: ["git", "--no-pager", "-C", root, ...args],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString()}`);
  }
  return result.stdout.toString();
}

mkdirSync(join(ROOT, "src"), { recursive: true });
writeFileSync(join(ROOT, FIXTURE_PATH), FIXTURE_BYTES, "utf8");

afterAll(() => {
  for (const dir of gitDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
  rmSync(ROOT, { recursive: true, force: true });
});

/**
 * A committed repository whose only indexed file has just been renamed. Rename detection is the one
 * fact only git can supply, so the pair `diff` builds from `changed ∪ removed` — a `renamed`
 * destination plus the hash comparison's `deleted` source — can only be produced against a real
 * repository. Returns the root and the commit the rename is measured against.
 */
function renamedRepo(): { root: string; base: string } {
  const root = mkdtempSync(join(tmpdir(), "sangkar-summary-git-"));
  gitDirs.push(root);
  mkdirSync(join(root, "src"), { recursive: true });
  writeFileSync(join(root, FIXTURE_PATH), FIXTURE_BYTES, "utf8");
  git(root, "init", "-q", "-b", "main");
  git(root, "add", "-A");
  git(
    root,
    "-c",
    "user.name=summary fixture",
    "-c",
    "user.email=summary@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    "init",
  );
  const base = git(root, "rev-parse", "HEAD").trim();
  git(root, "mv", FIXTURE_PATH, RENAMED_PATH);
  git(
    root,
    "-c",
    "user.name=summary fixture",
    "-c",
    "user.email=summary@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    "rename",
  );
  return { root, base };
}

const GENERATION = "g-2";
const OLD_GENERATION = "g-1";
const INDEXED_AT = "2026-01-03T00:00:00Z";
const CURSOR_KEY = "k".repeat(32);
const EMPTY_SHA = "0".repeat(64);

function sha256Of(bytes: string): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

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

/**
 * One row per rule, so a census assertion can name a bucket instead of a total: a stdlib call, a
 * missing package, a relative import that bound nothing, and two call sites the resolver could not
 * type. Only the last two could be user-defined code, which is what `material` counts.
 */
const UNRESOLVED_ROWS: readonly UnresolvedRef[] = [
  {
    id: 1,
    fileId: 1,
    path: FIXTURE_PATH,
    line: 1,
    rawTarget: "fs",
    evidenceKind: "dynamic",
    evidence: "import:fs",
  },
  {
    id: 2,
    fileId: 1,
    path: FIXTURE_PATH,
    line: 2,
    rawTarget: "lodash",
    evidenceKind: null,
    evidence: "import:lodash",
  },
  {
    id: 3,
    fileId: 1,
    path: FIXTURE_PATH,
    line: 3,
    rawTarget: "./missing.js",
    evidenceKind: null,
    evidence: "import:./missing.js",
  },
  {
    id: 4,
    fileId: 1,
    path: FIXTURE_PATH,
    line: 4,
    rawTarget: "obj[0]",
    evidenceKind: "dynamic",
    evidence: "call:obj[0]",
  },
  {
    id: 5,
    fileId: 1,
    path: FIXTURE_PATH,
    line: 5,
    rawTarget: "render",
    evidenceKind: "receiver_type_unknown",
    evidence: "call:render",
  },
];

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
  /** Store the working tree's real digest, so the degraded diff reports no changed file. */
  readonly cleanTree?: boolean;
  /** Report no test gap for the changed symbols, so the change set is fully covered. */
  readonly noTestGaps?: boolean;
  /** The unresolved rows `unresolvedForFiles` answers, scoped by file id like the reader's. */
  readonly unresolvedRows?: readonly UnresolvedRef[];
  /**
   * An indexed path whose bytes are gone from the working tree: the change set's `deleted` entry.
   * The graph still holds its row and its nodes, which is exactly what the payload must not count.
   */
  readonly deletedPath?: string;
  /** The tombstones `removedSymbolsFor` answers, which is what `removed_symbols` renders. */
  readonly removedSymbols?: readonly TombstoneRow[];
}

/** A hand-written reader: every method present, types only from `#shared/*`. */
function makeReader(options: ReaderOptions = {}): GraphReader {
  const file: IndexedFileRow = {
    id: 1,
    path: FIXTURE_PATH,
    language: "typescript",
    // Deliberately not the digest of the bytes on disk: the hash comparison reports it changed.
    sha256: options.cleanTree === true ? sha256Of(FIXTURE_BYTES) : EMPTY_SHA,
    size: Buffer.byteLength(FIXTURE_BYTES, "utf8"),
    lineCount: 3,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: options.indexedGeneration ?? GENERATION,
  };
  // A second indexed path whose bytes no longer exist on disk, so the hash comparison calls it
  // removed and the payload has to say what happened to it.
  const deleted: IndexedFileRow | null =
    options.deletedPath === undefined
      ? null
      : {
          ...file,
          id: 2,
          path: options.deletedPath,
          sha256: EMPTY_SHA,
          indexedGeneration: options.indexedGeneration ?? GENERATION,
        };
  const files = deleted === null ? [file] : [file, deleted];
  const goneNodes: readonly NodeRow[] =
    deleted === null
      ? []
      : [
          {
            ...node(90, "vanished"),
            fileId: deleted.id,
            path: deleted.path,
            qn: `${deleted.path}::vanished`,
            isTest: false,
          },
        ];

  return {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: (fileId) =>
      fileId === file.id ? [...NODES] : goneNodes.filter((row) => row.fileId === fileId),
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
    rowsForPaths: (prefixes) => ({
      indexed: files,
      // A prefix sees the skipped paths under it, never every skip in the graph: that second
      // question is `notIndexedCount`'s, and the two are only equal when the scope is the graph.
      notIndexed: SKIPPED_ROWS.filter((row) =>
        prefixes.some((prefix) => isUnderPrefix(row.path, prefix)),
      ),
    }),
    unresolvedForFiles: (fileIds) =>
      (options.unresolvedRows ?? []).filter((row) => fileIds.includes(row.fileId)),
    fileByPath: (path) => files.find((row) => row.path === path) ?? null,
    allFiles: () => [...files],
    notIndexedAll: () => [...SKIPPED_ROWS],
    notIndexedCount: () => SKIPPED_ROWS.length,
    testsCensus: () => TESTS_CENSUS,
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
      filesIndexed: files.length,
      nodes: NODES.length,
      edges: 5,
      durationMs: 12,
      mode: "full",
      status: "ok",
    }),
    removedSymbolsFor: () => [...(options.removedSymbols ?? [])],
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
      options.noTestGaps === true || !ids.includes(2)
        ? []
        : [
            {
              id: 2,
              qn: `${FIXTURE_PATH}::${NAMES[1] ?? ""}`,
              kind: "function",
              file: FIXTURE_PATH,
              lines: [2, 4] as const,
              testedDepth: null,
            },
          ],
    readNodeFts: () => [],
  };
}

function contextWith(reader: GraphReader): ToolContext {
  return contextWithRoot(reader, ROOT);
}

/** The repository root is the one filesystem fact a handler takes from `ctx.config`. */
function contextWithRoot(reader: GraphReader, root: string): ToolContext {
  return {
    reader,
    cursor: createCursors({ key: CURSOR_KEY }),
    log,
    now: () => new Date("2026-01-03T00:00:05Z"),
    config: loadConfig({
      argv: ["--repo-root", root],
      env: { SANGKAR_CURSOR_KEY: CURSOR_KEY },
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
      "unresolved",
    ]);
    // The diff degraded to the hash comparison, so the file with a wrong stored digest is the
    // change set — the same answer the integration fixture gets in a plain directory. A plain
    // modification is labelled `modified`; the counts are the file's own rows, not a second read.
    expect(payload.changed_files).toEqual([
      {
        path: FIXTURE_PATH,
        state: "modified",
        symbols: NODES.length,
        entry_point: true,
      } satisfies ChangedFileRow,
    ]);
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
    // This fixture carries no unresolved row: the census is present and zero, never absent.
    expect(payload.unresolved).toEqual(zeroCensus());
    expect(env.totals.unresolved_material).toBe(0);
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

  test("the census explains the unresolved total, even with no changed files", async () => {
    const reader = makeReader({ cleanTree: true, unresolvedRows: UNRESOLVED_ROWS });
    const env = await handle(contextWith(reader), {});
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    // Nothing changed, which is exactly when a flat unresolved count would go unexplained.
    expect(payload.changed_files).toEqual([]);

    expect(Object.keys(payload.unresolved.buckets).sort()).toEqual([...UNRESOLVED_BUCKETS].sort());
    expect(payload.unresolved.total).toBe(UNRESOLVED_ROWS.length);
    // One number, not two counts of the same rows: the payload and coverage cannot disagree.
    expect(payload.unresolved.total).toBe(env.coverage.unresolved_edges);
    expect(summed(payload.unresolved.buckets)).toBe(payload.unresolved.total);
    expect(payload.unresolved.buckets.builtin_stdlib).toBe(1);
    expect(payload.unresolved.buckets.external_package).toBe(1);
    expect(payload.unresolved.buckets.internal_module).toBe(1);
    expect(payload.unresolved.buckets.dynamic_property_call).toBe(1);
    expect(payload.unresolved.buckets.receiver_type_unknown).toBe(1);
    expect(payload.unresolved.buckets.ambiguous).toBe(0);
    // `obj[0]` is recorded exactly when no declared target exists, so only `render` is material.
    expect(payload.unresolved.material).toBe(1);
    expect(payload.unresolved.material).toBeLessThanOrEqual(payload.unresolved.total);
    expect(env.totals.unresolved_material).toBe(payload.unresolved.material);

    // `repo` reads the same rows through the same assembly, so the two scopes cannot disagree.
    const repo = await handle(contextWith(reader), { scope: "repo" });
    expect((repo.payload as RepoPayload).unresolved).toEqual(payload.unresolved);
    expect(repo.totals.unresolved_material).toBe(payload.unresolved.material);
  });

  /**
   * An empty change set has nothing to assess, so it may not claim test coverage of anything: the
   * driver list is the sentence an agent quotes, and "every changed symbol has a test" over zero
   * symbols is a claim about work nobody did (#10). `test_gaps_total` follows the same rule and
   * says not-applicable (`null`), never zero. Asserted through the fixture because the checkout
   * this runs in is dirty and cannot serve as a clean-tree probe.
   */
  test("an empty change set claims no test coverage", async () => {
    const env = await handle(contextWith(makeReader({ cleanTree: true })), {});
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    expect(payload.changed_files).toEqual([]);
    expect(payload.changed_symbols).toEqual([]);
    expect(payload.test_gaps).toEqual([]);
    expect(payload.risk.drivers.length).toBeGreaterThan(0);
    for (const driver of payload.risk.drivers) {
      expect(driver).not.toMatch(/test|coverage/i);
    }
    // A fact, not a coverage claim, and no untested-changes sentence either.
    expect(env.summary).toContain("no changed symbols to assess");
    expect(env.summary).not.toMatch(/test|coverage/i);
    // Not applicable is null; zero would read as "assessed, nothing missing".
    expect(env.totals.test_gaps_total).toBeNull();
  });

  test("one changed file keeps the gap driver and a numeric test-gap total", async () => {
    const env = await handle(contextWith(makeReader()), {});
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    expect(payload.changed_files).toEqual([
      { path: FIXTURE_PATH, state: "modified", symbols: NODES.length, entry_point: true },
    ]);
    expect(payload.risk.drivers).toContain(
      `${payload.test_gaps.length} changed symbols have no reachable test`,
    );
    expect(
      payload.risk.drivers.some((driver) => driver.includes("every changed symbol has a test")),
    ).toBe(false);
    expect(env.summary).toContain(`${payload.test_gaps.length} untested changes.`);
    expect(env.totals.test_gaps_total).toBe(payload.test_gaps.length);
  });

  test("a covered change set keeps the success sentence and a zero total", async () => {
    const env = await handle(contextWith(makeReader({ noTestGaps: true })), {});
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    expect(payload.changed_files).toEqual([
      { path: FIXTURE_PATH, state: "modified", symbols: NODES.length, entry_point: true },
    ]);
    expect(payload.test_gaps).toEqual([]);
    expect(payload.risk.drivers).toContain(
      "every changed symbol has a test within the tested-depth limit",
    );
    expect(env.summary).toContain("every changed symbol has a test within the tested-depth limit.");
    // Zero gaps over a non-empty change set is a measurement, so it stays a number.
    expect(env.totals.test_gaps_total).toBe(0);
    expect(env.totals.test_gaps_total).not.toBeNull();
  });

  /**
   * A deleted path used to vanish from the change scope: it was in `removed`, which the payload
   * never rendered, so a summary over a deletion described a tree nobody had. It is now an entry
   * with `state: "deleted"` and zero symbols — the graph's surviving rows for it are history, not a
   * change to review — while `removed_symbols` keeps counting what the path used to declare.
   */
  test("a deleted tracked file is an entry with state deleted and no symbols to rank", async () => {
    const env = await handle(
      contextWith(
        makeReader({
          deletedPath: DELETED_PATH,
          removedSymbols: [
            {
              qn: `${DELETED_PATH}::vanished`,
              kind: "function",
              fileId: 2,
              lastSeenGeneration: OLD_GENERATION,
              removedInGeneration: GENERATION,
              hadCallers: true,
            },
          ],
        }),
      ),
      {},
    );
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    expect(payload.changed_files).toEqual([
      { path: FIXTURE_PATH, state: "modified", symbols: NODES.length, entry_point: true },
      { path: DELETED_PATH, state: "deleted", symbols: 0, entry_point: false },
    ]);
    // The graph still holds a node for the deleted path, and the payload still counts none of it.
    expect(payload.changed_symbols.every((symbol) => symbol.qn.startsWith(FIXTURE_PATH))).toBe(
      true,
    );
    // The symbols are gone from the tree, not from the review: the tombstone is what counts them.
    expect(payload.removed_symbols).toEqual([
      { qn: `${DELETED_PATH}::vanished`, kind: "function", had_callers: true },
    ]);
    expect(env.totals.removed_symbols_total).toBe(1);
    // Both entries are files the change set named, so the total counts both of them.
    expect(env.totals.changed_files_total).toBe(2);
  });

  /**
   * A rename is one fact with two paths: the git half reports the destination with a `from`, and the
   * hash comparison reports the source as `deleted` because its bytes are gone. Rendering both would
   * count one move as two changed files, which is what a "rename empties the diff" answer looked
   * like from the outside.
   */
  test("a rename is one entry carrying from, and counts as one changed file", async () => {
    const { root, base } = renamedRepo();
    const env = await handle(contextWithRoot(makeReader(), root), { base });
    expect(env.status).toBe("ok");

    const payload = env.payload as ChangePayload;
    // The destination has no `file` row in this fixture, so it has no symbols yet — a count of 0
    // rather than a guess — but it is still one entry with the path it came from.
    expect(payload.changed_files).toEqual([
      { path: RENAMED_PATH, state: "renamed", from: FIXTURE_PATH, symbols: 0, entry_point: false },
    ]);
    // The `deleted` half of the pair is what would double-count the move.
    expect(payload.changed_files.some((row) => row.path === FIXTURE_PATH)).toBe(false);
    expect(env.totals.changed_files_total).toBe(1);
    expect(env.summary).toContain("1 files changed");
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
      "unresolved",
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
    expect(payload.not_indexed).toEqual([{ path: "src/vendor.min.js", reason: "size_cap" }]);
    // The payload lists what this scope skipped; the total is the graph's, so the two differ by the
    // skip under a top-level directory no indexed file lives in.
    expect(env.coverage.skipped).toBe(1);
    expect(env.totals.not_indexed_total).toBe(SKIPPED_ROWS.length);
    // The test census rides on `coverage` in every scope, this one included.
    expect(env.coverage.tests).toEqual(TESTS_CENSUS);
    expect(payload.unresolved).toEqual(zeroCensus());
    expect(env.totals.unresolved_material).toBe(0);
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

/**
 * Two counts travel together and neither implies the other: `coverage.skipped` says how much of
 * *this scope* the indexer did not read, `not_indexed_total` says how much of the *graph* it never
 * read. A scope that inferred the second from the first would report a smaller repository than
 * `status` reports for the same generation, which is what the two tests below pin.
 */
describe("summary: coverage counts", () => {
  test("coverage.skipped stays scoped while not_indexed_total is the graph's", async () => {
    const env = await handle(contextWith(makeReader()), { scope: "repo" });
    expect(env.status).toBe("ok");

    // Only `src` was indexed, so the skip under `docs/` is outside every prefix the scope queried.
    expect(env.coverage.skipped).toBe(1);
    expect(env.coverage.not_indexed.map((row) => row.path)).toEqual(["src/vendor.min.js"]);
    // The graph counted it anyway, and that is the number `status` publishes too.
    expect(env.totals.not_indexed_total).toBe(SKIPPED_ROWS.length);
    expect(env.totals.not_indexed_total).toBeGreaterThan(env.coverage.skipped);
  });

  test("every scope publishes the graph's skip total and the test census", async () => {
    const reader = makeReader();
    const args = [{}, { scope: "repo" as const }, { scope: "area" as const, target: "src" }];
    // One number per generation: the scope changes the list, never the repository's skip count.
    const published: (number | null | undefined)[] = [];
    for (const scopeArgs of args) {
      const env = await handle(contextWith(reader), scopeArgs);
      expect(env.status).toBe("ok");
      published.push(env.totals.not_indexed_total);
      // The test corpus is a coverage fact, so every scope carries it beside its own counts.
      expect(env.coverage.tests).toEqual(TESTS_CENSUS);
    }
    expect(published).toEqual([SKIPPED_ROWS.length, SKIPPED_ROWS.length, SKIPPED_ROWS.length]);
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
  /**
   * Small enough to cut the ranked list, large enough to keep a leading item of it: the envelope
   * carries the coverage census and the test corpus on every answer, so a page boundary below this
   * would drop `changed_symbols` whole and prove nothing about resuming a ranking.
   */
  const PAGE_BUDGET = 800;

  test("a truncated page is resumed without repeating an item", async () => {
    const ctx = contextWith(makeReader());
    const budget = PAGE_BUDGET;

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
    const budget = PAGE_BUDGET;
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
