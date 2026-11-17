import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { changedSince, GIT_TIMEOUT_MS } from "#shared/domain/diff";
import { type Conn, db, openReader, openWriter } from "#shared/store/db";
import { migrate } from "#shared/store/migrate";
import * as schema from "#shared/store/schema";

/**
 * The change set against two kinds of working tree: a real git fixture, and a plain temp directory
 * that git cannot read (which is what the integration fixture is, so `degraded` has to be a
 * result and not an exception).
 *
 * Every fixture is a `mkdtemp` directory, and the database sits where the product puts it —
 * `<root>/.codegraph/graph.db` — so the tracked files and the graph are exercised together. The
 * module writes nothing, which is asserted here rather than assumed: `git status --porcelain` and
 * the index mtime must be identical before and after a call.
 */

const TS_A = "export const a = 1;\n";
const TS_B = "export const b = 2;\n";
const TS_A_CHANGED = "export const a = 11;\nexport const extra = 3;\n";
const MD_DOC = "# fixture\n";

const dirs: string[] = [];
const readers: Conn[] = [];

function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

function write(root: string, path: string, bytes: string): void {
  const absolute = join(root, path);
  mkdirSync(dirname(absolute), { recursive: true });
  writeFileSync(absolute, bytes);
}

function remove(root: string, path: string): void {
  rmSync(join(root, path), { force: true });
}

function sha256(bytes: string): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

interface IndexedFile {
  readonly path: string;
  readonly language: string;
  /** The bytes the indexer saw; the working tree may hold anything, including nothing. */
  readonly bytes: string;
}

/** A migrated database recording `files` as indexed, opened read-only like every read path. */
function openGraph(root: string, files: readonly IndexedFile[]): Conn {
  const path = join(root, ".codegraph", "graph.db");
  mkdirSync(dirname(path), { recursive: true });
  const writer = openWriter(path);
  try {
    migrate(writer.sqlite);
    db(writer)
      .insert(schema.file)
      .values(
        files.map((file, index) => ({
          id: index + 1,
          path: file.path,
          language: file.language,
          sha256: sha256(file.bytes),
          size: file.bytes.length,
          lineCount: file.bytes.split("\n").length,
          parseStatus: "ok",
          parseErrorRanges: null,
          indexedGeneration: "g-diff",
        })),
      )
      .run();
  } finally {
    writer.close();
  }
  const reader = openReader(path);
  readers.push(reader);
  return reader;
}

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
    "user.name=diff fixture",
    "-c",
    "user.email=diff@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    message,
  );
}

/** A committed repository holding an indexable file, a second one to delete and a README. */
function makeGitRepo(): string {
  const root = tempDir("codegraph-diff-git-");
  write(root, "src/a.ts", TS_A);
  write(root, "src/b.ts", TS_B);
  write(root, "README.md", MD_DOC);
  git(root, "init", "-q", "-b", "main");
  git(root, "add", "-A");
  commit(root, "init");
  return root;
}

afterAll(() => {
  for (const reader of readers.splice(0)) {
    reader.close();
  }
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("changedSince against a git fixture", () => {
  test("a modified file is changed, a deleted file is removed, a modified .md is unmatched", async () => {
    const root = makeGitRepo();
    const reader = openGraph(root, [
      { path: "src/a.ts", language: "typescript", bytes: TS_A },
      { path: "src/b.ts", language: "typescript", bytes: TS_B },
    ]);

    write(root, "src/a.ts", TS_A_CHANGED);
    remove(root, "src/b.ts");
    write(root, "README.md", "# fixture, edited\n");

    const before = git(root, "status", "--porcelain");
    const indexMtime = statSync(join(root, ".git", "index")).mtimeMs;
    const result = await changedSince(root, "HEAD", reader);
    // Stat before any other git call: `git status` itself refreshes the index, which would hide
    // a write made by the change-set computation.
    const indexMtimeAfter = statSync(join(root, ".git", "index")).mtimeMs;

    expect(result.changed).toEqual(["src/a.ts"]);
    expect(result.removed).toEqual(["src/b.ts"]);
    expect(result.unmatched).toEqual([
      { path: "README.md", reason: "no_file_row_and_unparseable_language" },
    ]);
    expect(result.degraded).toBe(false);
    expect(result.degradedReason).toBeNull();

    // Nothing was written: the module only reads working-tree bytes and runs a lock-free diff.
    expect(git(root, "status", "--porcelain")).toBe(before);
    expect(indexMtimeAfter).toBe(indexMtime);
  });

  test("the hash comparison finds a changed indexed file that git ignores", async () => {
    const root = makeGitRepo();
    write(root, ".gitignore", "generated/\n");
    write(root, "generated/out.ts", TS_A);
    git(root, "add", "-A");
    commit(root, "ignore generated");
    const reader = openGraph(root, [
      { path: "src/a.ts", language: "typescript", bytes: TS_A },
      { path: "generated/out.ts", language: "typescript", bytes: TS_A },
    ]);

    // Git never reports an ignored path, so only `file.sha256` can notice this one.
    write(root, "generated/out.ts", TS_A_CHANGED);
    expect(git(root, "diff", "--name-status", "HEAD")).toBe("");

    const result = await changedSince(root, "HEAD", reader);

    expect(result.degraded).toBe(false);
    expect(result.changed).toEqual(["generated/out.ts"]);
    expect(result.removed).toEqual([]);
  });

  test("a path git calls deleted but the working tree still holds is changed, not removed", async () => {
    const root = makeGitRepo();
    const reader = openGraph(root, [{ path: "src/a.ts", language: "typescript", bytes: TS_A }]);

    // Stage the deletion, then put different bytes back: the path is present, so it is not gone.
    git(root, "rm", "-q", "--cached", "src/a.ts");
    write(root, "src/a.ts", TS_A_CHANGED);

    const result = await changedSince(root, "HEAD", reader);

    expect(result.removed).toEqual([]);
    expect(result.changed).toEqual(["src/a.ts"]);
  });

  test("a git invocation that exceeds the bound degrades instead of hanging", async () => {
    const root = tempDir("codegraph-diff-slow-");
    write(root, "src/a.ts", TS_A);
    // A clean filter that sleeps makes `git diff` read the working tree through a slow pipe.
    write(root, ".gitattributes", "*.ts filter=slow\n");
    git(root, "init", "-q", "-b", "main");
    git(root, "add", "-A");
    commit(root, "init");
    git(root, "config", "filter.slow.clean", "sleep 3");
    const reader = openGraph(root, [{ path: "src/a.ts", language: "typescript", bytes: TS_A }]);
    write(root, "src/a.ts", TS_A_CHANGED);

    const started = Bun.nanoseconds();
    const result = await changedSince(root, "HEAD", reader, { gitTimeoutMs: 100 });
    const elapsedMs = (Bun.nanoseconds() - started) / 1e6;

    expect(result.degraded).toBe(true);
    expect(result.degradedReason).toContain("100 ms");
    expect(elapsedMs).toBeLessThan(2000);
    // Degrading still answers: the hash comparison is the source that found the change.
    expect(result.changed).toEqual(["src/a.ts"]);
  });

  test("the default git bound is 30 s, so no invocation is unbounded", () => {
    expect(GIT_TIMEOUT_MS).toBe(30_000);
  });

  test("diff.ts never reads the environment: that access belongs to #shared/config.ts", () => {
    // plan.constraint_invariant.md: "the only `process.env` access is in `#shared/config.ts`". A
    // second one here would break `grep -rn process.env src`, so the source is the assertion.
    const source = readFileSync(
      join(import.meta.dir, "..", "src", "shared", "domain", "diff.ts"),
      "utf8",
    );
    expect(source.includes("process.env")).toBe(false);
  });
});

describe("changedSince outside git", () => {
  test("a plain temp directory degrades to the hash comparison without throwing", async () => {
    const root = tempDir("codegraph-diff-plain-");
    write(root, "a.ts", TS_A);
    write(root, "b.ts", TS_B);
    const reader = openGraph(root, [
      { path: "a.ts", language: "typescript", bytes: TS_A },
      { path: "b.ts", language: "typescript", bytes: TS_B },
    ]);

    write(root, "a.ts", TS_A_CHANGED);
    remove(root, "b.ts");

    const result = await changedSince(root, "HEAD", reader);

    expect(result.degraded).toBe(true);
    expect(result.degradedReason?.length).toBeGreaterThan(0);
    expect(result.changed).toEqual(["a.ts"]);
    expect(result.removed).toEqual(["b.ts"]);
    expect(result.unmatched).toEqual([]);
  });

  test("a file row that points outside the repo root is dropped, never reported", async () => {
    const parent = tempDir("codegraph-diff-outside-");
    const root = join(parent, "repo");
    mkdirSync(root, { recursive: true });
    write(parent, "outside.ts", TS_A_CHANGED);
    const reader = openGraph(root, [
      { path: "../outside.ts", language: "typescript", bytes: TS_A },
    ]);

    const result = await changedSince(root, "HEAD", reader);

    expect(result.changed).toEqual([]);
    expect(result.removed).toEqual([]);
    expect(result.unmatched).toEqual([]);
  });

  test("an empty base means the last commit, and a non-git root still returns a set", async () => {
    const root = makeGitRepo();
    const reader = openGraph(root, [{ path: "src/a.ts", language: "typescript", bytes: TS_A }]);
    write(root, "src/a.ts", TS_A_CHANGED);

    const withHead = await changedSince(root, "HEAD", reader);
    const withEmpty = await changedSince(root, "", reader);

    expect(withEmpty).toEqual(withHead);
  });
});
