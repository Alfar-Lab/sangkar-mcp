import { Database } from "bun:sqlite";
import { afterEach, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { acquireLock, lockPath, release } from "#shared/store/lock";
import { runWorker, WORKER_EXIT, type WorkerMode, type WorkerOptions } from "../src/indexer/worker";

/**
 * The worker's contract is a process contract: four exit codes, an untouched stdout, and `meta`
 * rows that report a failure without publishing a graph. So the tests drive {@link runWorker} the
 * way the server does — one call, one code — over a temp directory that is deliberately not a git
 * repository, because "a plain directory still indexes" is the degradation the design promises.
 *
 * The fixture is two Python files where the caller sorts *before* the file it calls
 * (`src/caller.py` calls `src/widget.py`): that is the order in which `persistUnit`'s per-file
 * `DELETE FROM node` would cascade the caller's edge away, so the cross-file edge assertion is the
 * evidence that the generation's edges survive a full re-persist.
 */

const CALLER = "src/caller.py";
const WIDGET = "src/widget.py";

const WIDGET_SOURCE = `def render(title):
    return title
`;

const CALLER_SOURCE = `from widget import render


def run():
    return render("x")
`;

interface TempRepo {
  readonly repoRoot: string;
  readonly dataDir: string;
  readonly livePath: string;
}

const dirs: string[] = [];

afterEach(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A temp working tree plus its data dir; nothing here is under version control. */
async function tempRepo(files: Record<string, string> = {}): Promise<TempRepo> {
  const repoRoot = await mkdtemp(join(tmpdir(), "sangkar-worker-"));
  dirs.push(repoRoot);
  const sources: Record<string, string> = {
    [CALLER]: CALLER_SOURCE,
    [WIDGET]: WIDGET_SOURCE,
    ...files,
  };
  for (const [path, content] of Object.entries(sources)) {
    const absolute = join(repoRoot, path);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, content, "utf8");
  }
  const dataDir = join(repoRoot, ".sangkar");
  return { repoRoot, dataDir, livePath: join(dataDir, "graph.db") };
}

function options(repo: TempRepo): WorkerOptions {
  return { repoRoot: repo.repoRoot, dataDir: repo.dataDir, force: false, mode: "fast" };
}

/**
 * Runs the worker with `process.stdout.write` captured. The capture asserts a *quantity*: the
 * worker is a child of an MCP server, so one byte on stdout is a protocol corruption, and the only
 * honest proof is that nothing wrote any.
 */
async function runCaptured(
  workerOptions: WorkerOptions,
): Promise<{ code: number; stdout: string }> {
  const original = process.stdout.write;
  const written: string[] = [];
  process.stdout.write = ((chunk: string | Uint8Array) => {
    written.push(typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8"));
    return true;
  }) as typeof process.stdout.write;
  try {
    const code = await runWorker(workerOptions);
    return { code, stdout: written.join("") };
  } finally {
    process.stdout.write = original;
  }
}

function rows<T>(livePath: string, sql: string): T[] {
  const handle = new Database(livePath, { readonly: true });
  try {
    return handle.query<T, []>(sql).all();
  } finally {
    handle.close();
  }
}

function count(livePath: string, table: "generation" | "node" | "edge" | "file"): number {
  return rows<{ rows: number }>(livePath, `SELECT count(*) AS rows FROM ${table}`)[0]?.rows ?? 0;
}

function meta(livePath: string, key: string): string | null {
  const found = rows<{ value: string }>(livePath, `SELECT value FROM meta WHERE key = '${key}'`);
  return found[0]?.value ?? null;
}

/** The newest `generation` row, or `null` before the first publish. */
function generation(livePath: string): Record<string, unknown> | null {
  const found = rows<Record<string, unknown>>(
    livePath,
    "SELECT id, seq, mode, status, base_commit, files_indexed, nodes, edges, worktree_dirty" +
      " FROM generation ORDER BY seq DESC",
  );
  return found[0] ?? null;
}

/** The id of the oldest generation: the one before this run published anything. */
function oldestGenerationId(livePath: string): string | null {
  const found = rows<{ id: string }>(livePath, "SELECT id FROM generation ORDER BY seq LIMIT 1");
  return found[0]?.id ?? null;
}

/** The id of the newest generation: the one a reader is served. */
function latestGenerationId(livePath: string): string | null {
  const found = rows<{ id: string }>(
    livePath,
    "SELECT id FROM generation ORDER BY seq DESC LIMIT 1",
  );
  return found[0]?.id ?? null;
}

function generationCount(livePath: string): number {
  return count(livePath, "generation");
}

/** Stage leftovers: a discarded stage must leave nothing behind, published or not. */
async function stageFiles(dataDir: string): Promise<string[]> {
  const names = await readdir(dataDir);
  return names.filter((name) => name.includes(".stage."));
}

async function exists(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

/** A git invocation that fails loudly; the git tests are about what git reports, not about git. */
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

function commit(root: string, message: string): void {
  git(
    root,
    "-c",
    "user.name=worker fixture",
    "-c",
    "user.email=worker@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    message,
  );
}

describe("runWorker", () => {
  test("publishes a plain directory once, then reports nothing to do", async () => {
    const repo = await tempRepo();

    const first = await runCaptured(options(repo));
    expect(first.code).toBe(WORKER_EXIT.published);
    // The four codes are the whole message; stdout stays empty for the protocol above us.
    expect(first.stdout).toBe("");

    // One generation, and the state a reader's `status.health` reports.
    expect(generationCount(repo.livePath)).toBe(1);
    expect(meta(repo.livePath, "indexer_state")).toBe("idle");
    expect(meta(repo.livePath, "consecutive_failures")).toBe("0");
    const published = generation(repo.livePath);
    expect(published?.mode).toBe("fast");
    expect(published?.status).toBe("published");
    // Not a git repository: no commit is claimed, and the run still published.
    expect(published?.base_commit).toBeNull();
    expect(published?.worktree_dirty).toBe(0);
    expect(published?.files_indexed).toBe(2);

    // Both files are in the graph, with the manifest facts the incremental planner keys on.
    const files = rows<{ path: string; line_count: number; parse_status: string }>(
      repo.livePath,
      "SELECT path, line_count, parse_status FROM file ORDER BY path",
    );
    expect(files.map((file) => file.path)).toEqual([CALLER, WIDGET]);
    expect(files[0]?.line_count).toBe(5);
    expect(files[0]?.parse_status).toBe("ok");
    expect(files[1]?.line_count).toBe(2);

    // The cross-file edge: caller sorts first, so its edge into `widget.py` is the one a per-file
    // delete would have cascaded away before the callee's nodes were re-inserted.
    const calls = rows<{ rows: number }>(
      repo.livePath,
      "SELECT count(*) AS rows FROM edge e" +
        " JOIN node s ON s.id = e.src_id JOIN node d ON d.id = e.dst_id" +
        " WHERE e.kind = 'calls' AND s.name = 'run' AND d.name = 'render'",
    );
    expect(calls[0]?.rows).toBe(1);

    // The data dir ignores itself with exactly one star, and the lock is gone after the run.
    expect(await readFile(join(repo.dataDir, ".gitignore"), "utf8")).toBe("*\n");
    expect(await exists(lockPath(repo.dataDir))).toBe(false);
    expect(await stageFiles(repo.dataDir)).toEqual([]);

    // Nothing changed and the run was not forced: 2, no second generation, still clean stdout.
    const second = await runCaptured(options(repo));
    expect(second.code).toBe(WORKER_EXIT.nothingToDo);
    expect(second.stdout).toBe("");
    expect(generationCount(repo.livePath)).toBe(1);
  });

  test("a modified file publishes again, then the tree is idle again", async () => {
    const repo = await tempRepo();
    expect((await runCaptured(options(repo))).code).toBe(WORKER_EXIT.published);
    const before = count(repo.livePath, "node");

    await writeFile(
      join(repo.repoRoot, WIDGET),
      `${WIDGET_SOURCE}\n\ndef helper(value):\n    return value\n`,
      "utf8",
    );

    const changed = await runCaptured(options(repo));
    expect(changed.code).toBe(WORKER_EXIT.published);
    expect(changed.stdout).toBe("");
    expect(generationCount(repo.livePath)).toBe(2);
    expect(count(repo.livePath, "node")).toBeGreaterThan(before);

    const idle = await runCaptured(options(repo));
    expect(idle.code).toBe(WORKER_EXIT.nothingToDo);
    expect(generationCount(repo.livePath)).toBe(2);
  });

  test("force publishes a fresh generation from an empty stage", async () => {
    const repo = await tempRepo();
    expect((await runCaptured(options(repo))).code).toBe(WORKER_EXIT.published);
    const nodes = count(repo.livePath, "node");

    // No `mode` override: a forced run defaults to `full`, per `GenerationInput`.
    const forced = await runCaptured({
      repoRoot: repo.repoRoot,
      dataDir: repo.dataDir,
      force: true,
    });
    expect(forced.code).toBe(WORKER_EXIT.published);
    expect(forced.stdout).toBe("");
    // A forced run starts from an empty file, so the graph's history starts over with it: one row,
    // `full`, and the same graph content the incremental run had published.
    expect(generationCount(repo.livePath)).toBe(1);
    expect(count(repo.livePath, "node")).toBe(nodes);
    expect(generation(repo.livePath)?.mode).toBe("full");
    expect(meta(repo.livePath, "indexer_state")).toBe("idle");
    expect(await stageFiles(repo.dataDir)).toEqual([]);
  });

  test("a deleted file leaves a tombstone and a coverage row is rewritten", async () => {
    const repo = await tempRepo();
    expect((await runCaptured(options(repo))).code).toBe(WORKER_EXIT.published);
    const firstGeneration = oldestGenerationId(repo.livePath) ?? "";
    const callerNodes = rows<{ rows: number }>(
      repo.livePath,
      `SELECT count(*) AS rows FROM file WHERE path = '${CALLER}'`,
    )[0]?.rows;
    expect(callerNodes).toBe(1);

    // The caller's bytes leave the tree and a file no grammar covers appears: the first must stop
    // being in the graph, the second must be in coverage with the reason discovery gave it.
    await rm(join(repo.repoRoot, CALLER));
    await writeFile(join(repo.repoRoot, "notes.txt"), "not source\n", "utf8");

    const published = await runCaptured(options(repo));
    expect(published.code).toBe(WORKER_EXIT.published);
    expect(published.stdout).toBe("");
    expect(
      rows<{ path: string }>(repo.livePath, "SELECT path FROM file ORDER BY path").map(
        (row) => row.path,
      ),
    ).toEqual([WIDGET]);

    // The removed symbols are tombstones named by the generation that removed them, so
    // `summary(change)` can report them after the rows are gone.
    const removedBy = latestGenerationId(repo.livePath) ?? "";
    expect(removedBy).not.toBe(firstGeneration);
    const tombstones = rows<{ removed_in_generation: string; last_seen_generation: string }>(
      repo.livePath,
      "SELECT removed_in_generation, last_seen_generation FROM tombstone ORDER BY qn",
    );
    expect(tombstones.length).toBeGreaterThan(0);
    for (const tombstone of tombstones) {
      expect(tombstone.removed_in_generation).toBe(removedBy);
      expect(tombstone.last_seen_generation).toBe(firstGeneration);
    }

    expect(
      rows<{ path: string; reason: string }>(
        repo.livePath,
        "SELECT path, reason FROM not_indexed ORDER BY path",
      ),
    ).toEqual([{ path: "notes.txt", reason: "unsupported_ext" }]);
  });

  test("a git repository records HEAD and publishes a committed change", async () => {
    const repo = await tempRepo();
    git(repo.repoRoot, "init", "-q", "-b", "main");
    // `-A` is safe: the data dir's own `.gitignore` holds one star, so no graph is ever staged.
    git(repo.repoRoot, "add", "-A");
    commit(repo.repoRoot, "init");
    const head = git(repo.repoRoot, "rev-parse", "HEAD").trim();

    const first = await runCaptured(options(repo));
    expect(first.code).toBe(WORKER_EXIT.published);
    expect(first.stdout).toBe("");
    // `generation.base_commit` is the commit the graph was built from, and the data dir the worker
    // just created does not make the tree dirty.
    expect(generation(repo.livePath)?.base_commit).toBe(head);
    expect(generation(repo.livePath)?.worktree_dirty).toBe(0);
    const firstId = latestGenerationId(repo.livePath);

    await writeFile(
      join(repo.repoRoot, WIDGET),
      `${WIDGET_SOURCE}\n\ndef helper(value):\n    return value\n`,
      "utf8",
    );
    git(repo.repoRoot, "add", "-A");
    commit(repo.repoRoot, "change");

    // Hash comparison and `git diff` agree here; either source alone would publish.
    const second = await runCaptured(options(repo));
    expect(second.code).toBe(WORKER_EXIT.published);
    expect(second.stdout).toBe("");
    expect(generationCount(repo.livePath)).toBe(2);
    expect(latestGenerationId(repo.livePath)).not.toBe(firstId);
    expect(generation(repo.livePath)?.base_commit).not.toBe(head);
  });

  test("a held lock is exit 75 and writes nothing", async () => {
    const repo = await tempRepo();
    await mkdir(repo.dataDir, { recursive: true });
    // The server holds the lock while a worker runs; a second worker must not queue or block.
    const held = await acquireLock(repo.dataDir);
    try {
      const { code, stdout } = await runCaptured(options(repo));
      expect(code).toBe(WORKER_EXIT.locked);
      expect(stdout).toBe("");
      expect(await exists(repo.livePath)).toBe(false);
    } finally {
      await release(held);
    }
  });

  test("a throw inside the stage discards it and records the failure in meta", async () => {
    const repo = await tempRepo();
    expect((await runCaptured(options(repo))).code).toBe(WORKER_EXIT.published);
    const generations = generationCount(repo.livePath);
    const nodes = count(repo.livePath, "node");
    const edges = count(repo.livePath, "edge");

    // The tree has to differ for the run to reach the stage at all; this change is never published.
    await writeFile(
      join(repo.repoRoot, WIDGET),
      `${WIDGET_SOURCE}\n\ndef helper(value):\n    return value\n`,
      "utf8",
    );

    // `finalizeGeneration` rejects a mode outside `full | fast`. That is an injected throw *inside*
    // the stage transaction, which is exactly the failure this test is about: the stage must be
    // discarded and the failure recorded, with the live graph left as it was.
    const broken = await runCaptured({
      ...options(repo),
      mode: "partial" as unknown as WorkerMode,
    });
    expect(broken.code).toBe(WORKER_EXIT.failed);
    expect(broken.stdout).toBe("");

    expect(generationCount(repo.livePath)).toBe(generations);
    expect(count(repo.livePath, "node")).toBe(nodes);
    expect(count(repo.livePath, "edge")).toBe(edges);
    expect(meta(repo.livePath, "consecutive_failures")).toBe("1");
    expect(meta(repo.livePath, "indexer_state")).toBe("failed");
    expect(await stageFiles(repo.dataDir)).toEqual([]);

    // A second failure counts as a second consecutive one, not a reset.
    expect(
      (
        await runCaptured({
          ...options(repo),
          mode: "partial" as unknown as WorkerMode,
        })
      ).code,
    ).toBe(WORKER_EXIT.failed);
    expect(meta(repo.livePath, "consecutive_failures")).toBe("2");

    // And the next run that publishes clears both: one new generation, idle, zero failures.
    const recovered = await runCaptured(options(repo));
    expect(recovered.code).toBe(WORKER_EXIT.published);
    expect(generationCount(repo.livePath)).toBe(generations + 1);
    expect(meta(repo.livePath, "indexer_state")).toBe("idle");
    expect(meta(repo.livePath, "consecutive_failures")).toBe("0");
  });
});
