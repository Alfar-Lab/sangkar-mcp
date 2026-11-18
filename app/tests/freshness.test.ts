import { afterAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  checkFreshness,
  type Freshness,
  type FreshnessOptions,
  headCommitOf,
  health,
  STALE_REASONS,
  type StaleReason,
  WATCHER_STALL_MS,
} from "#shared/domain/freshness";
import type {
  GenerationRow,
  GraphMeta,
  GraphReader,
  IndexedFileRow,
} from "#shared/store/read/reader";

/**
 * Freshness against a real temp directory and a fake reader (TOOLS-SPEC.md §1.2, §2).
 *
 * The two halves are deliberately separate: the files are real, so `files_changed` is a hash of
 * real bytes rather than a mocked comparison, and the reader is a hand-written object, so nothing
 * here opens a database. The clock and the `HEAD` probe are injected for the same reason the
 * handler's clock is: the ten-minute stall threshold and a commit comparison are facts a test must
 * be able to place exactly.
 *
 * Every value of {@link STALE_REASONS} is produced by one fixture below and the set is asserted at
 * the end, so a reason that stops being reachable fails a test instead of quietly disappearing.
 */

const ROOT = mkdtempSync(join(tmpdir(), "sangkar-freshness-"));

afterAll(() => {
  rmSync(ROOT, { recursive: true, force: true });
});

const NOW = new Date("2026-11-17T14:02:00Z");
const NOW_ISO = "2026-11-17T14:02:00.000Z";
/** 11m45s before `NOW`: past the ten-minute stall threshold. */
const STALE_PUBLISH = "2026-11-17T13:50:15Z";
/** 49s before `NOW`: a run that is busy but nowhere near stalled. */
const FRESH_PUBLISH = "2026-11-17T14:01:11Z";
const GENERATION = "g-7f3a91";
const SHORT_COMMIT = "bbdda4b9";
const FULL_COMMIT = "bbdda4b9c0ffee1234567890abcdef1234567890";
const OTHER_COMMIT = "1234567abcdef0123456789abcdef0123456789a";

/** A scratch directory that does not exist yet; `path.join` keeps every test's tree separate. */
function scratch(name: string): string {
  const path = join(ROOT, name);
  mkdirSync(path, { recursive: true });
  return path;
}

function digestOf(text: string): string {
  return createHash("sha256").update(text).digest("hex");
}

/** Writes `content` at `path` under `root` and returns the digest the indexer would have stored. */
function writeFile(root: string, path: string, content: string): string {
  const absolute = join(root, path);
  mkdirSync(dirname(absolute), { recursive: true });
  writeFileSync(absolute, content);
  return digestOf(content);
}

interface Fixture {
  readonly root: string;
  readonly files: readonly { readonly path: string; readonly sha256: string }[];
  /** Defaults to `root`; `null` models a database that never recorded one. */
  readonly projectRoot?: string | null;
  readonly generation?: string | null;
  readonly indexedAt?: string | null;
  readonly indexerState?: string | null;
  readonly consecutiveFailures?: number;
  readonly baseCommit?: string | null;
  readonly durationMs?: number;
  /** Present only when the fake models the handle's statement cache. */
  readonly statementCacheEntries?: number;
}

function fileRow(id: number, path: string, sha256: string): IndexedFileRow {
  return {
    id,
    path,
    language: path.endsWith(".ts") ? "typescript" : "python",
    sha256,
    size: 100,
    lineCount: 4,
    parseStatus: "ok",
    parseErrorRanges: null,
    indexedGeneration: GENERATION,
  };
}

function generationRowOf(fixture: Fixture): GenerationRow {
  return {
    id: GENERATION,
    seq: 7,
    publishedAt: fixture.indexedAt ?? STALE_PUBLISH,
    baseCommit: fixture.baseCommit === undefined ? null : fixture.baseCommit,
    worktreeDirty: false,
    filesIndexed: fixture.files.length,
    nodes: 13_955,
    edges: 130_044,
    durationMs: fixture.durationMs ?? 41_200,
    mode: "incremental",
    status: "published",
  };
}

/**
 * A hand-written reader: the four members freshness reads plus the rest of the interface, so a
 * change to `GraphReader` fails the type check here rather than at runtime in a tool.
 */
function fakeReader(fixture: Fixture): GraphReader {
  const generation = fixture.generation === undefined ? GENERATION : fixture.generation;
  const rows = fixture.files.map((file, index) => fileRow(index + 1, file.path, file.sha256));
  const generationRow = generationRowOf(fixture);
  const reader: GraphReader = {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: () => [],
    inbound: () => [],
    outbound: () => [],
    metricsFor: () => null,
    rowsForPaths: () => ({ indexed: [], notIndexed: [] }),
    unresolvedForFiles: () => [],
    fileByPath: () => null,
    allFiles: () => [...rows],
    censusByLanguage: () => [],
    metaFor: (): GraphMeta => ({
      projectName: "sangkar",
      projectRoot: fixture.projectRoot === undefined ? fixture.root : fixture.projectRoot,
      generation,
      indexedAt: fixture.indexedAt === undefined ? STALE_PUBLISH : fixture.indexedAt,
      indexerState: fixture.indexerState === undefined ? "idle" : fixture.indexerState,
      consecutiveFailures: fixture.consecutiveFailures ?? 0,
    }),
    latestGeneration: () => (generation === null ? null : generationRow),
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };
  if (fixture.statementCacheEntries === undefined) {
    // No cache member at all: the shape a fake without a handle has, and the case `health` must
    // answer with `0` instead of a number it made up.
    return reader;
  }
  return { ...reader, statementCacheEntries: () => fixture.statementCacheEntries ?? 0 };
}

/** The default probe for the tests that do not inject one: this temp tree is not a checkout. */
function noHead(): null {
  return null;
}

function fresh(reader: GraphReader, root: string, options: FreshnessOptions = {}): Freshness {
  return checkFreshness(reader, root, { now: () => NOW, readHead: noHead, ...options });
}

describe("freshness: unchanged tree", () => {
  test("an untouched working tree is fresh and dated", () => {
    const root = scratch("unchanged");
    const digest = writeFile(root, "src/a.ts", "export const a = 1;\n");
    const reader = fakeReader({
      root,
      files: [
        { path: "src/a.ts", sha256: digest },
        { path: "src/b.py", sha256: writeFile(root, "src/b.py", "b = 2\n") },
      ],
    });

    expect(fresh(reader, root)).toEqual({
      stale: false,
      stale_reason: "none",
      files_changed_since: 0,
      checked_at: NOW_ISO,
    });
  });

  test("a digest recorded in another case still compares equal", () => {
    const root = scratch("case");
    const digest = writeFile(root, "src/a.ts", "export const a = 1;\n");
    const reader = fakeReader({
      root,
      files: [{ path: "src/a.ts", sha256: digest.toUpperCase() }],
    });

    expect(fresh(reader, root).stale).toBe(false);
  });

  test("a file that is gone is not counted: it has no bytes to differ from", () => {
    const root = scratch("gone");
    const reader = fakeReader({ root, files: [{ path: "src/deleted.ts", sha256: digestOf("x") }] });

    const result = fresh(reader, root);
    expect(result.stale).toBe(false);
    expect(result.files_changed_since).toBe(0);
  });

  test("a file row that escapes the repo root is never read", () => {
    const root = scratch("escape");
    const outside = scratch("escape-outside");
    writeFile(outside, "secret.ts", "export const secret = 1;\n");
    const reader = fakeReader({
      root,
      files: [{ path: "../escape-outside/secret.ts", sha256: digestOf("something else") }],
    });

    expect(fresh(reader, root).files_changed_since).toBe(0);
  });
});

describe("freshness: files_changed", () => {
  test("a rewritten file makes the generation stale and is counted", () => {
    const root = scratch("changed");
    const digest = writeFile(root, "src/a.ts", "export const a = 1;\n");
    writeFile(root, "src/b.py", "b = 2\n");
    writeFile(root, "src/a.ts", "export const a = 2;\n");
    const reader = fakeReader({
      root,
      files: [
        { path: "src/a.ts", sha256: digest },
        { path: "src/b.py", sha256: digestOf("b = 2\n") },
      ],
    });

    expect(fresh(reader, root)).toEqual({
      stale: true,
      stale_reason: "files_changed",
      files_changed_since: 1,
      checked_at: NOW_ISO,
    });
  });
});

describe("freshness: root_mismatch", () => {
  test("a graph built for another project_root outranks every other reason", () => {
    const root = scratch("root-own");
    const elsewhere = scratch("root-other");
    const digest = writeFile(root, "src/a.ts", "export const a = 1;\n");
    writeFile(root, "src/a.ts", "export const a = 2;\n");
    const reader = fakeReader({
      root,
      projectRoot: elsewhere,
      files: [{ path: "src/a.ts", sha256: digest }],
      // Both of the remaining reasons would also fire; the root mismatch explains them all.
      baseCommit: SHORT_COMMIT,
      indexerState: "running",
    });

    const result = fresh(reader, root, { readHead: () => OTHER_COMMIT });
    expect(result).toEqual({
      stale: true,
      stale_reason: "root_mismatch",
      // The digest comparison is not attempted against the wrong tree, so no count is invented.
      files_changed_since: 0,
      checked_at: NOW_ISO,
    });
  });

  test("an unrecorded project_root is not a mismatch", () => {
    const root = scratch("root-unrecorded");
    const reader = fakeReader({ root, projectRoot: null, files: [] });

    expect(fresh(reader, root).stale_reason).toBe("none");
  });
});

describe("freshness: base_commit_moved", () => {
  test("a HEAD the recorded base commit does not name makes the answer stale", () => {
    const root = scratch("commit-moved");
    const reader = fakeReader({ root, files: [], baseCommit: SHORT_COMMIT });

    expect(fresh(reader, root, { readHead: () => OTHER_COMMIT })).toEqual({
      stale: true,
      stale_reason: "base_commit_moved",
      files_changed_since: 0,
      checked_at: NOW_ISO,
    });
  });

  test("a short recorded commit matches the full HEAD it abbreviates", () => {
    const root = scratch("commit-prefix");
    const reader = fakeReader({ root, files: [], baseCommit: SHORT_COMMIT });

    expect(fresh(reader, root, { readHead: () => FULL_COMMIT }).stale).toBe(false);
  });

  test("an unreadable HEAD is unknown, never stale", () => {
    const root = scratch("commit-unknown");
    const reader = fakeReader({ root, files: [], baseCommit: SHORT_COMMIT });

    // `noHead` is the default probe here: a directory that is not a checkout has no verdict to give.
    expect(fresh(reader, root).stale_reason).toBe("none");
  });

  test("a generation that recorded no base commit is not compared", () => {
    const root = scratch("commit-none");
    const reader = fakeReader({ root, files: [], baseCommit: null });

    expect(fresh(reader, root, { readHead: () => OTHER_COMMIT }).stale).toBe(false);
  });
});

describe("freshness: watcher_stalled", () => {
  test("a busy indexer with nothing published for over ten minutes is stalled", () => {
    const root = scratch("stalled");
    const reader = fakeReader({
      root,
      files: [],
      indexerState: "running",
      indexedAt: STALE_PUBLISH,
    });

    expect(fresh(reader, root)).toEqual({
      stale: true,
      stale_reason: "watcher_stalled",
      files_changed_since: 0,
      checked_at: NOW_ISO,
    });
  });

  test("`busy` is a busy state too", () => {
    const root = scratch("stalled-busy");
    const reader = fakeReader({ root, files: [], indexerState: "busy", indexedAt: STALE_PUBLISH });

    expect(fresh(reader, root).stale_reason).toBe("watcher_stalled");
  });

  test("a run that published inside the threshold is not stalled", () => {
    const root = scratch("stalled-recent");
    const reader = fakeReader({
      root,
      files: [],
      indexerState: "running",
      indexedAt: FRESH_PUBLISH,
    });

    expect(fresh(reader, root).stale_reason).toBe("none");
  });

  test("exactly the threshold is not yet stalled, one millisecond past it is", () => {
    const root = scratch("stalled-boundary");
    // Anchored on the injected clock, not on the publish time: the threshold is measured from `now`.
    const atThreshold = fakeReader({
      root,
      files: [],
      indexerState: "running",
      indexedAt: new Date(NOW.getTime() - WATCHER_STALL_MS).toISOString(),
    });
    expect(fresh(atThreshold, root).stale_reason).toBe("none");

    const oneMsPast = fakeReader({
      root,
      files: [],
      indexerState: "running",
      indexedAt: new Date(NOW.getTime() - WATCHER_STALL_MS - 1).toISOString(),
    });
    expect(fresh(oneMsPast, root).stale_reason).toBe("watcher_stalled");

    const idle = fakeReader({ root, files: [], indexerState: "idle", indexedAt: STALE_PUBLISH });
    expect(fresh(idle, root).stale_reason).toBe("none");
  });
});

describe("freshness: the reason set", () => {
  test("all five reasons are reachable from a fixture", () => {
    const root = scratch("all-reasons");
    const digest = writeFile(root, "src/a.ts", "export const a = 1;\n");
    writeFile(root, "src/a.ts", "export const a = 2;\n");
    const elsewhere = scratch("all-reasons-other");

    const reasons: StaleReason[] = [
      fresh(fakeReader({ root, files: [{ path: "src/a.ts", sha256: digest }] }), root).stale_reason,
      fresh(fakeReader({ root, files: [], projectRoot: elsewhere }), root).stale_reason,
      fresh(fakeReader({ root, files: [], baseCommit: SHORT_COMMIT }), root, {
        readHead: () => OTHER_COMMIT,
      }).stale_reason,
      fresh(fakeReader({ root, files: [], indexerState: "running" }), root).stale_reason,
      fresh(fakeReader({ root, files: [] }), root).stale_reason,
    ];

    expect([...reasons].sort()).toEqual([...STALE_REASONS].sort());
  });
});

describe("freshness: headCommitOf", () => {
  test("reads a loose ref", () => {
    const root = scratch("git-loose");
    writeFile(root, ".git/HEAD", "ref: refs/heads/main\n");
    writeFile(root, ".git/refs/heads/main", `${FULL_COMMIT}\n`);

    expect(headCommitOf(root)).toBe(FULL_COMMIT);
  });

  test("reads a ref that git has packed", () => {
    const root = scratch("git-packed");
    writeFile(root, ".git/HEAD", "ref: refs/heads/main\n");
    writeFile(
      root,
      ".git/packed-refs",
      `# pack-refs with: peeled fully-peeled sorted\n${FULL_COMMIT} refs/heads/main\n^${OTHER_COMMIT}\n`,
    );

    expect(headCommitOf(root)).toBe(FULL_COMMIT);
  });

  test("reads a detached HEAD", () => {
    const root = scratch("git-detached");
    writeFile(root, ".git/HEAD", `${FULL_COMMIT}\n`);

    expect(headCommitOf(root)).toBe(FULL_COMMIT);
  });

  test("follows a worktree's gitdir file", () => {
    const root = scratch("git-worktree");
    const gitDir = scratch("git-worktree-real");
    writeFile(gitDir, "HEAD", "ref: refs/heads/work\n");
    writeFile(gitDir, "refs/heads/work", `${OTHER_COMMIT}\n`);
    writeFile(root, ".git", `gitdir: ${gitDir}\n`);

    expect(headCommitOf(root)).toBe(OTHER_COMMIT);
  });

  test("a directory that is not a checkout is null", () => {
    expect(headCommitOf(scratch("git-none"))).toBeNull();
  });

  test("a ref that exists nowhere is null rather than a guess", () => {
    const root = scratch("git-unresolved");
    writeFile(root, ".git/HEAD", "ref: refs/heads/missing\n");

    expect(headCommitOf(root)).toBeNull();
  });
});

describe("freshness: health", () => {
  test("reports the indexer, the publish duration, the failure counter and the cache size", () => {
    const root = scratch("health");
    const reader = fakeReader({
      root,
      files: [],
      indexerState: "failed",
      consecutiveFailures: 3,
      durationMs: 41_200,
      statementCacheEntries: 17,
    });

    expect(health(reader)).toEqual({
      indexer: "failed",
      last_publish_ms: 41_200,
      consecutive_failures: 3,
      statement_cache_entries: 17,
    });
  });

  test("a reader with no statement cache reports zero entries and no publish", () => {
    const root = scratch("health-empty");
    const reader = fakeReader({ root, files: [], generation: null, indexedAt: null });

    expect(health(reader)).toEqual({
      indexer: "idle",
      last_publish_ms: 0,
      consecutive_failures: 0,
      statement_cache_entries: 0,
    });
  });
});
