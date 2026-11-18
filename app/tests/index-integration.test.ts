import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { detectLanguage } from "../src/indexer/lang/registry";
import { runWorker } from "../src/indexer/worker";

/**
 * The integration half of the phase 1 closing gate: two runs of the real worker over the reference
 * corpus, checked through the artifact it publishes instead of through its stderr log.
 *
 * What has to hold after a full run is what the read path depends on — one `generation` row, no
 * call site silently dropped (`unresolved` is populated, not empty), and a coverage table that
 * never calls a supported language unsupported. The second run is the idempotence claim: over an
 * unchanged tree the worker publishes nothing and returns 2, so a watcher's idle poll is free.
 *
 * The corpus is a nested git checkout that the *root* repository ignores, so a `git status` in the
 * app's own repository proves nothing about it; the assertion runs git inside the corpus, where an
 * untracked `.sangkar/` or a rewritten fixture would show up.
 */

const CORPUS = resolve(import.meta.dir, "..", "..", "code-review-graph");

/** Two full indexes of the corpus take ~20 s; the default 5 s per test would flake. */
const RUN_TIMEOUT_MS = 300_000;

const dirs: string[] = [];

interface Run {
  /** The first run: a forced full rebuild, because a fresh data dir has nothing to reuse. */
  readonly firstCode: number;
  /** The second run over the same unchanged tree. */
  readonly secondCode: number;
  readonly generationsAfterFirst: number;
  readonly generationsAfterSecond: number;
  readonly published: GenerationRow | null;
  readonly indexedFiles: number;
  readonly unresolved: number;
  readonly notIndexed: readonly { path: string; reason: string }[];
  readonly corpusStatus: readonly string[];
  readonly corpusDataDir: boolean;
}

interface GenerationRow {
  readonly id: string;
  readonly seq: number;
  readonly status: string;
  readonly mode: string;
  readonly filesIndexed: number;
  readonly nodes: number;
  readonly edges: number;
}

let run: Run;

afterAll(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

beforeAll(async () => {
  const dataDir = await mkdtemp(join(tmpdir(), "sangkar-integration-"));
  dirs.push(dataDir);
  const livePath = join(dataDir, "graph.db");

  const firstCode = await runWorker({ repoRoot: CORPUS, dataDir, force: true, mode: "full" });
  const generationsAfterFirst = count(livePath, "generation");
  const secondCode = await runWorker({ repoRoot: CORPUS, dataDir, force: false, mode: "fast" });
  const generationsAfterSecond = count(livePath, "generation");

  run = {
    firstCode,
    secondCode,
    generationsAfterFirst,
    generationsAfterSecond,
    published: generation(livePath),
    indexedFiles: count(livePath, "file"),
    unresolved: count(livePath, "unresolved"),
    notIndexed: query<{ path: string; reason: string }>(
      livePath,
      "SELECT path, reason FROM not_indexed ORDER BY path",
    ),
    corpusStatus: corpusStatus(),
    corpusDataDir: existsSync(join(CORPUS, ".sangkar")),
  };

  // One stderr JSON line, so the progress log quotes real output rather than a passing assertion.
  process.stderr.write(
    `${JSON.stringify({
      ts: new Date().toISOString(),
      level: "info",
      msg: "phase1.integration.measured",
      corpus: "code-review-graph",
      firstRunExit: run.firstCode,
      secondRunExit: run.secondCode,
      generationsAfterFirstRun: run.generationsAfterFirst,
      generationsAfterSecondRun: run.generationsAfterSecond,
      generation: run.published,
      fileRows: run.indexedFiles,
      unresolvedRows: run.unresolved,
      notIndexedRows: run.notIndexed.length,
      corpusStatusLines: run.corpusStatus.length,
      corpusDataDirCreated: run.corpusDataDir,
    })}\n`,
  );
}, RUN_TIMEOUT_MS);

describe("index integration over the reference corpus", () => {
  test("the first run publishes exactly one generation row", () => {
    expect({ firstCode: run.firstCode, generations: run.generationsAfterFirst }).toEqual({
      firstCode: 0,
      generations: 1,
    });
    expect({ status: run.published?.status, mode: run.published?.mode }).toEqual({
      status: "published",
      mode: "full",
    });
    // The generation row is the reader's summary of the graph, so its counts are the graph's.
    expect({
      filesIndexed: run.published?.filesIndexed,
      fileRows: run.indexedFiles,
      counted: run.published !== null && run.published.nodes > 0 && run.published.edges > 0,
    }).toEqual({
      filesIndexed: run.indexedFiles,
      fileRows: run.indexedFiles,
      counted: true,
    });
  });

  test("call sites that did not bind are recorded in unresolved, not dropped", () => {
    expect(run.unresolved).toBeGreaterThan(0);
  });

  test("no supported language is reported as unsupported_ext", () => {
    expect(
      run.notIndexed.filter(
        (row) => row.reason === "unsupported_ext" && detectLanguage(row.path) !== null,
      ),
    ).toEqual([]);
  });

  test("a second run over the unchanged corpus returns 2 and publishes nothing", () => {
    expect({ secondCode: run.secondCode, generations: run.generationsAfterSecond }).toEqual({
      secondCode: 2,
      generations: 1,
    });
  });

  test("the corpus is unmodified and no data dir was created inside it", () => {
    expect({ statusLines: run.corpusStatus, dataDirInCorpus: run.corpusDataDir }).toEqual({
      statusLines: [],
      dataDirInCorpus: false,
    });
  });
});

function count(livePath: string, table: "generation" | "file" | "unresolved"): number {
  return query<{ rows: number }>(livePath, `SELECT count(*) AS rows FROM ${table}`)[0]?.rows ?? 0;
}

/** The newest `generation` row: the one a reader is served. */
function generation(livePath: string): GenerationRow | null {
  const found = query<{
    id: string;
    seq: number;
    status: string;
    mode: string;
    files_indexed: number;
    nodes: number;
    edges: number;
  }>(
    livePath,
    "SELECT id, seq, status, mode, files_indexed, nodes, edges FROM generation ORDER BY seq DESC",
  );
  const row = found[0];
  if (row === undefined) {
    return null;
  }
  return {
    id: row.id,
    seq: row.seq,
    status: row.status,
    mode: row.mode,
    filesIndexed: row.files_indexed,
    nodes: row.nodes,
    edges: row.edges,
  };
}

/**
 * `git status --porcelain` *inside* the corpus, one line per pending path. Empty means the run
 * touched no tracked file and left no untracked one behind.
 */
function corpusStatus(): readonly string[] {
  const result = Bun.spawnSync({
    cmd: ["git", "-C", CORPUS, "status", "--porcelain"],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git status failed in ${CORPUS}: ${new TextDecoder().decode(result.stderr)}`);
  }
  return new TextDecoder()
    .decode(result.stdout)
    .split("\n")
    .map((line) => line.trimEnd())
    .filter((line) => line !== "");
}

/** Opens the published graph read-only, the mode every reader in this repository runs under. */
function query<T>(livePath: string, sql: string): T[] {
  const handle = new Database(livePath, { readonly: true });
  try {
    return handle.query<T, []>(sql).all();
  } finally {
    handle.close();
  }
}
