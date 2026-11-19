import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { changedSince, changedSinceFrom, GIT_TIMEOUT_MS } from "#shared/domain/diff";
import { type Conn, db, openReader, openWriter } from "#shared/store/db";
import { migrate } from "#shared/store/migrate";
import { makeReader } from "#shared/store/read/reader";
import * as schema from "#shared/store/schema";

/**
 * The change set against two kinds of working tree: a real git fixture, and a plain temp directory
 * that git cannot read (which is what the integration fixture is, so `degraded` has to be a
 * result and not an exception).
 *
 * Every fixture is a `mkdtemp` directory, and the database sits where the product puts it —
 * `<root>/.sangkar/graph.db` — so the tracked files and the graph are exercised together. The
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

/** The commit a change set is measured against. */
function head(root: string): string {
  return git(root, "rev-parse", "HEAD").trim();
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
  const path = join(root, ".sangkar", "graph.db");
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
  const root = tempDir("sangkar-diff-git-");
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
    // One entry per attributed path with its state; `README.md` is not attributed, so its only
    // trace is `unmatched` above.
    expect(result.entries).toEqual([
      { state: "modified", path: "src/a.ts" },
      { state: "deleted", path: "src/b.ts" },
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

  test("a rename is one renamed entry with the source, and one changed path", async () => {
    const root = makeGitRepo();
    const base = head(root);
    // The old path is indexed, which is the real case: its row outlives the bytes a rename moved.
    const reader = openGraph(root, [{ path: "src/a.ts", language: "typescript", bytes: TS_A }]);
    git(root, "mv", "src/a.ts", "src/c.ts");
    commit(root, "rename a to c");

    const result = await changedSince(root, base, reader);

    // A rename no longer degrades the set, and it is not reported twice: `changed` holds only the
    // destination, so a summary cannot count one move as two files.
    expect(result.degraded).toBe(false);
    expect(result.degradedReason).toBeNull();
    expect(result.changed).toEqual(["src/c.ts"]);
    expect(result.entries.filter((entry) => entry.state === "renamed")).toEqual([
      { state: "renamed", path: "src/c.ts", from: "src/a.ts" },
    ]);
    // The stale row for the old path is the hash comparison's own finding, unchanged by this task:
    // the bytes it describes are gone, so `removed` still names it and its entry is `deleted`.
    expect(result.removed).toEqual(["src/a.ts"]);
    expect(result.entries).toEqual([
      { state: "deleted", path: "src/a.ts" },
      { state: "renamed", path: "src/c.ts", from: "src/a.ts" },
    ]);
  });

  test("a copied file is added, not a rename of its source", async () => {
    const root = makeGitRepo();
    const base = head(root);
    const reader = openGraph(root, [
      { path: "src/a.ts", language: "typescript", bytes: TS_A },
      { path: "src/copy.ts", language: "typescript", bytes: TS_A },
    ]);
    write(root, "src/copy.ts", TS_A);
    git(root, "add", "src/copy.ts");

    const result = await changedSince(root, base, reader);

    expect(result.degraded).toBe(false);
    expect(result.changed).toEqual(["src/copy.ts"]);
    expect(result.removed).toEqual([]);
    // This build asks git for renames only, so a copy arrives as `A`; either way the source still
    // exists and the destination is simply a new path, with no `from` to render.
    expect(result.entries).toEqual([{ state: "added", path: "src/copy.ts" }]);
  });

  test("a copy record is added, and a malformed record is skipped instead of nulling the parse", async () => {
    const root = tempDir("sangkar-diff-parse-");
    const shim = join(root, "shim");
    mkdirSync(shim, { recursive: true });
    // Only a crafted stream can carry a `C` pair (git needs `-C` plus a modified source to emit
    // one) or a truncated record without also making a real git fail: `C100\0from\0to\0M\0path\0M`
    // is one copy, one modification, and one status with no path after it.
    writeFileSync(
      join(shim, "git"),
      "#!/bin/sh\nprintf 'C100\\0src/from.ts\\0src/copy.ts\\0M\\0src/broken.ts\\0M'\n",
      { mode: 0o755 },
    );
    write(root, "src/keep.ts", TS_A);
    write(root, "src/copy.ts", TS_A);
    write(root, "src/broken.ts", TS_A_CHANGED);
    const reader = openGraph(root, [{ path: "src/keep.ts", language: "typescript", bytes: TS_A }]);

    const result = await changedSince(root, "HEAD", reader, { env: { PATH: shim } });

    // A `null` parse would have degraded the whole set to the hash comparison; the two readable
    // records survive, and the truncated one is dropped on its own.
    expect(result.degraded).toBe(false);
    expect(result.degradedReason).toBeNull();
    expect(result.changed).toEqual(["src/broken.ts", "src/copy.ts"]);
    expect(result.entries).toEqual([
      { state: "modified", path: "src/broken.ts" },
      { state: "added", path: "src/copy.ts" },
    ]);
  });

  test("a git invocation that exceeds the bound degrades instead of hanging", async () => {
    const root = tempDir("sangkar-diff-slow-");
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

  test("changedSinceFrom answers the same set from a reader, so a handler needs no connection", async () => {
    const root = makeGitRepo();
    const conn = openGraph(root, [
      { path: "src/a.ts", language: "typescript", bytes: TS_A },
      { path: "src/b.ts", language: "typescript", bytes: TS_B },
    ]);

    write(root, "src/a.ts", TS_A_CHANGED);
    remove(root, "src/b.ts");

    // The reader carries every `path`/`sha256` pair the hash comparison reads, which is the whole
    // reason `summary` and `impact` can be handlers and still answer "what changed".
    const fromReader = await changedSinceFrom(root, "HEAD", makeReader(conn));

    expect(fromReader).toEqual(await changedSince(root, "HEAD", conn));
    expect(fromReader.changed).toEqual(["src/a.ts"]);
    expect(fromReader.removed).toEqual(["src/b.ts"]);
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
    const root = tempDir("sangkar-diff-plain-");
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
    // Entries are still populated without git: the hash comparison knows a digest difference from
    // a missing path, which is all `modified` and `deleted` need.
    expect(result.entries).toEqual([
      { state: "modified", path: "a.ts" },
      { state: "deleted", path: "b.ts" },
    ]);
  });

  test("a file row that points outside the repo root is dropped, never reported", async () => {
    const parent = tempDir("sangkar-diff-outside-");
    const root = join(parent, "repo");
    mkdirSync(root, { recursive: true });
    write(parent, "outside.ts", TS_A_CHANGED);
    const reader = openGraph(root, [
      { path: "../outside.ts", language: "typescript", bytes: TS_A },
    ]);

    const result = await changedSince(root, "HEAD", reader);

    expect(result.changed).toEqual([]);
    expect(result.removed).toEqual([]);
    expect(result.entries).toEqual([]);
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
