import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readdirSync, statSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { openReader } from "#shared/store/db";
import { type MigrateResult, migrate } from "#shared/store/migrate";
import { beginWrite, STAGE_INFIX, withStage } from "#shared/store/publish";
import { fileByPath } from "#shared/store/read/files";
import { findByQn, nodesByFile } from "#shared/store/read/nodes";
import { makeReader } from "#shared/store/read/reader";
import * as schema from "#shared/store/schema";

/**
 * The phase 0 smoke gate: the whole store path, end to end, on a real temporary repository.
 *
 * One graph is built the way the indexer will build it — `migrate()` on a live file, then a
 * `withStage` run that writes a `generation`, a `file` and two `node` rows through
 * `WriteScope.transaction`, seals, and renames over the live file — and read back through the
 * reader primitives a tool will be handed. Nothing is written inside the repository: the data
 * directory is a `mkdtemp` created per run, so this test is the store's contract and not a
 * fixture of it.
 *
 * The assertions are organised by who owns the fact: `migrate()` owns the schema version, the
 * publish protocol owns what is on disk, and the read primitives own what a tool sees.
 */

const FILE_ID = 1;
const WIDGET_ID = 1;
const RENDER_ID = 2;
const GENERATION = "g-5f2c9a17";
const PUBLISHED_AT = "2026-11-17T13:50:15.000Z";
const PATH = "src/widget.ts";
const WIDGET_QN = "src/widget.ts::Widget";
const RENDER_QN = "src/widget.ts::Widget.render";
const SHA256 = "a".repeat(64);

const SIDECAR_SUFFIXES = ["-wal", "-shm", "-journal"] as const;

interface Counts {
  readonly files: number;
  readonly nodes: number;
}

interface Run {
  readonly dir: string;
  readonly livePath: string;
  readonly stagePath: string;
  readonly seededFrom: string;
  readonly returned: { generation: string; files: number; nodes: number };
  /** Row counts of the live file *while* the stage was being written, i.e. before the rename. */
  readonly liveDuringRun: Counts;
  /** Row counts of the stage at the same moment: the new rows exist there and nowhere else. */
  readonly stageDuringRun: Counts;
  readonly firstMigrate: MigrateResult;
  readonly secondMigrate: MigrateResult;
}

let run: Run;

/** The shipped migrations, applied by the one runner, on a caller-owned handle. */
function buildLive(livePath: string): { first: MigrateResult; second: MigrateResult } {
  const handle = new Database(livePath, { create: true });
  try {
    const first = migrate(handle);
    const second = migrate(handle);
    return { first, second };
  } finally {
    handle.close();
  }
}

function countsIn(path: string): Counts {
  const handle = new Database(path, { readonly: true });
  try {
    const row = handle
      .query<{ files: number; nodes: number }, []>(
        "SELECT (SELECT count(*) FROM file) AS files, (SELECT count(*) FROM node) AS nodes",
      )
      .get();
    if (row === null) {
      throw new Error(`no counts row from ${path}`);
    }
    return { files: row.files, nodes: row.nodes };
  } finally {
    handle.close();
  }
}

function dirNames(dir: string): string[] {
  return readdirSync(dir).sort();
}

/** A stage file or a database sidecar: what the publish protocol promises to leave behind never. */
function leftovers(dir: string): string[] {
  return dirNames(dir).filter(
    (name) =>
      name.includes(STAGE_INFIX) || SIDECAR_SUFFIXES.some((suffix) => name.endsWith(suffix)),
  );
}

/**
 * Builds the graph the rest of the file asserts on: live file → migrate → `withStage` write →
 * seal → publish. Everything the protocol is supposed to keep private is captured here
 * (`stagePath`, the two counts taken mid-run) so a later assertion can prove it rather than
 * re-derive it.
 */
async function publishGraph(): Promise<Run> {
  const dir = await mkdtemp(join(tmpdir(), "codegraph-smoke-"));
  const livePath = join(dir, "graph.db");
  const { first, second } = buildLive(livePath);

  let stagePath = "";
  let seededFrom = "";
  let liveDuringRun: Counts = { files: -1, nodes: -1 };
  let stageDuringRun: Counts = { files: -1, nodes: -1 };

  const returned = await withStage(livePath, (staged) => {
    stagePath = staged.path;
    seededFrom = staged.seededFrom;
    // A stage of an existing graph is a VACUUM INTO copy beside it, never the live file itself.
    expect(staged.livePath).toBe(livePath);
    expect(staged.path.startsWith(`${livePath}${STAGE_INFIX}`)).toBe(true);
    expect(existsSync(staged.path)).toBe(true);

    const scope = beginWrite(staged.path);
    try {
      scope.transaction((tx) => {
        tx.insert(schema.generation)
          .values({
            id: GENERATION,
            seq: 1,
            publishedAt: PUBLISHED_AT,
            worktreeDirty: 0,
            filesIndexed: 1,
            nodes: 2,
            edges: 0,
            durationMs: 4321,
            mode: "full",
            status: "ok",
          })
          .run();
        tx.insert(schema.meta).values({ key: "generation", value: GENERATION }).run();
        tx.insert(schema.file)
          .values({
            id: FILE_ID,
            path: PATH,
            language: "typescript",
            sha256: SHA256,
            size: 240,
            lineCount: 12,
            parseStatus: "ok",
            parseErrorRanges: null,
            indexedGeneration: GENERATION,
          })
          .run();
        // Parent before child: the self-referencing foreign key is checked per row.
        tx.insert(schema.node)
          .values({
            id: WIDGET_ID,
            fileId: FILE_ID,
            parentId: null,
            kind: "class",
            subkind: null,
            name: "Widget",
            qn: WIDGET_QN,
            startLine: 1,
            endLine: 12,
            signature: "class Widget",
            docstring: "A reusable widget.",
            isTest: 0,
          })
          .run();
        tx.insert(schema.node)
          .values({
            id: RENDER_ID,
            fileId: FILE_ID,
            parentId: WIDGET_ID,
            kind: "method",
            subkind: null,
            name: "render",
            qn: RENDER_QN,
            startLine: 3,
            endLine: 5,
            signature: "render(): string",
            docstring: null,
            isTest: 0,
          })
          .run();
      });
    } finally {
      // The scope must be closed before `seal`: the writer owns the WAL the seal needs gone.
      scope.close();
    }

    liveDuringRun = countsIn(livePath);
    stageDuringRun = countsIn(staged.path);

    return { generation: GENERATION, files: 1, nodes: 2 };
  });

  return {
    dir,
    livePath,
    stagePath,
    seededFrom,
    returned,
    liveDuringRun,
    stageDuringRun,
    firstMigrate: first,
    secondMigrate: second,
  };
}

beforeAll(async () => {
  run = await publishGraph();
});

afterAll(async () => {
  await rm(run.dir, { recursive: true, force: true });
});

describe("smoke: migrate", () => {
  test("the runner brings an empty file to this build's schema and is a no-op afterwards", () => {
    const first = run.firstMigrate;
    const second = run.secondMigrate;

    expect(run.dir.startsWith(tmpdir())).toBe(true);
    expect(first.fromVersion).toBe(0);
    expect(first.applied.length).toBeGreaterThan(0);
    expect(first.schemaVersion).toBe(first.applied.length);
    expect(first.applied[0]).toBe("0000_init");
    expect(first.applied).toContain("raw/0001_fts");
    expect(first.rebuildRequired).toBe(false);

    // One ledger: a second run applies nothing and reports the version it found.
    expect(second.applied).toEqual([]);
    expect(second.fromVersion).toBe(first.schemaVersion);
    expect(second.schemaVersion).toBe(first.schemaVersion);
    expect(second.rebuildRequired).toBe(false);
  });
});

describe("smoke: publish protocol", () => {
  test("the run writes into a stage, seals it, and only the rename reaches the live file", () => {
    expect(run.seededFrom).toBe("live");
    expect(run.stagePath.startsWith(`${run.livePath}${STAGE_INFIX}`)).toBe(true);
    expect(run.returned).toEqual({ generation: GENERATION, files: 1, nodes: 2 });

    // Mid-run, the new graph exists in the stage and not in the live file: a reader of the live
    // path keeps answering the previous generation until the rename.
    expect(run.stageDuringRun).toEqual({ files: 1, nodes: 2 });
    expect(run.liveDuringRun).toEqual({ files: 0, nodes: 0 });
  });

  test("after the publish the live file is the whole graph and no stage or sidecar is left", () => {
    expect(existsSync(run.stagePath)).toBe(false);
    expect(leftovers(run.dir)).toEqual([]);
    expect(dirNames(run.dir)).toEqual(["graph.db"]);
    expect(existsSync(run.livePath)).toBe(true);
    expect(statSync(run.livePath).size).toBeGreaterThan(0);
    expect(countsIn(run.livePath)).toEqual({ files: 1, nodes: 2 });
  });

  test("the published file is a self-contained database that migrate() treats as current", () => {
    const handle = new Database(run.livePath, { create: false });
    try {
      const journal = handle.query<{ journal_mode: string }, []>("PRAGMA journal_mode").get();
      expect(journal?.journal_mode).toBe("delete");
      expect(handle.query<{ quick_check: string }, []>("PRAGMA quick_check(1)").all()).toEqual([
        { quick_check: "ok" },
      ]);
      // The seal set `synchronous = FULL` on the file; the publish renamed that same file.
      expect(handle.query<{ synchronous: number }, []>("PRAGMA synchronous").get()).toEqual({
        synchronous: 2,
      });
    } finally {
      handle.close();
    }

    const writer = new Database(run.livePath, { create: false });
    try {
      const again = migrate(writer);
      expect(again.applied).toEqual([]);
      expect(again.fromVersion).toBe(run.firstMigrate.schemaVersion);
      expect(again.schemaVersion).toBe(run.firstMigrate.schemaVersion);
    } finally {
      writer.close();
    }

    expect(leftovers(run.dir)).toEqual([]);
  });
});

describe("smoke: read back", () => {
  test("the reader sees the generation, the file and both nodes the stage wrote", () => {
    const conn = openReader(run.livePath);
    try {
      const reader = makeReader(conn);

      // Generation: `meta.generation` names it and the `generation` row carries the census.
      const meta = reader.metaFor();
      expect(meta.generation).toBe(GENERATION);
      expect(meta.indexedAt).toBe(PUBLISHED_AT);
      expect(meta.indexerState).toBeNull();
      expect(meta.consecutiveFailures).toBe(0);
      expect(reader.latestGeneration()).toEqual({
        id: GENERATION,
        seq: 1,
        publishedAt: PUBLISHED_AT,
        baseCommit: null,
        worktreeDirty: false,
        filesIndexed: 1,
        nodes: 2,
        edges: 0,
        durationMs: 4321,
        mode: "full",
        status: "ok",
      });

      // File: the row the indexer writes, read back through the census query.
      expect(fileByPath(conn, PATH)).toEqual({
        id: FILE_ID,
        path: PATH,
        language: "typescript",
        sha256: SHA256,
        size: 240,
        lineCount: 12,
        parseStatus: "ok",
        parseErrorRanges: null,
        indexedGeneration: GENERATION,
      });
      expect(fileByPath(conn, `./${PATH}`)?.id).toBe(FILE_ID);
      expect(reader.allFiles()).toHaveLength(1);

      // Nodes: exact resolution on `qn`, and both of them in source order under the file.
      const render = findByQn(conn, RENDER_QN);
      expect(render).toEqual({
        id: RENDER_ID,
        fileId: FILE_ID,
        path: PATH,
        parentId: WIDGET_ID,
        kind: "method",
        subkind: null,
        name: "render",
        qn: RENDER_QN,
        startLine: 3,
        endLine: 5,
        signature: "render(): string",
        docstring: null,
        isTest: false,
        props: {},
      });
      const widget = findByQn(conn, WIDGET_QN);
      expect(widget?.name).toBe("Widget");
      expect(widget?.parentId).toBeNull();
      expect(widget?.docstring).toBe("A reusable widget.");
      expect(findByQn(conn, "src/widget.ts::Missing")).toBeNull();

      const nodes = nodesByFile(conn, FILE_ID);
      expect(nodes.map((node) => node.qn)).toEqual([WIDGET_QN, RENDER_QN]);
      expect(reader.nodesByFile(FILE_ID).map((node) => node.qn)).toEqual([WIDGET_QN, RENDER_QN]);
      expect(reader.members(WIDGET_ID).map((node) => node.qn)).toEqual([RENDER_QN]);
    } finally {
      conn.close();
    }
  });

  test("the published graph opens read-only, so a tool cannot write through its reader", () => {
    const conn = openReader(run.livePath);
    try {
      expect(() =>
        conn.sqlite.exec("INSERT INTO meta (key, value) VALUES ('smoke', 'write')"),
      ).toThrow();
      expect(makeReader(conn).metaFor().generation).toBe(GENERATION);
    } finally {
      conn.close();
    }
    // A failed write leaves the live file exactly as it was, and no sidecar behind.
    expect(leftovers(run.dir)).toEqual([]);
  });
});
