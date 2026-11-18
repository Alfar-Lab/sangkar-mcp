import { afterEach, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, rm, truncate, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { ALWAYS_EXCLUDED, discover, SIZE_CAP_BYTES } from "../src/indexer/discover";

/**
 * Discovery's contract is a claim about a tree, so every fixture is a real tree under `mkdtemp`
 * and the assertions are about what the walk reports for it: one accepted file with a hash the
 * test computes independently, and one entry per rejected candidate naming the reason.
 *
 * Nothing here writes inside the repository or the reference corpus, and nothing touches the
 * network.
 */

const dirs: string[] = [];

async function tempRepo(prefix = "sangkar-discover-"): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A `.ts` file whose bytes are a NUL probe hit, the shape of a compiled artefact left in place. */
const BINARY_BYTES = Buffer.from([0x63, 0x6f, 0x64, 0x65, 0x00, 0x01, 0x02, 0x00]);

async function seedFixture(repoRoot: string): Promise<void> {
  await writeFile(join(repoRoot, ".gitignore"), "ignored.ts\nbuild/\n");
  await writeFile(join(repoRoot, ".sangkarignore"), "generated/\n");

  await writeFile(join(repoRoot, "keep.ts"), "export const kept = 1;\n");
  await writeFile(join(repoRoot, "ignored.ts"), "export const hidden = 2;\n");
  await writeFile(join(repoRoot, "data.ts"), BINARY_BYTES);
  await writeFile(join(repoRoot, "mystery.weird"), "plain text, unknown extension\n");

  // Pruned directories: an ignored one from each control file, and every always-exclude name.
  const pruned = [
    "build/nested.ts",
    "generated/nested.ts",
    ...Array.from(ALWAYS_EXCLUDED, (name) => `${name}/hidden.ts`),
  ];
  for (const rel of pruned) {
    const abs = join(repoRoot, rel);
    await mkdir(dirname(abs), { recursive: true });
    await writeFile(abs, "export const nested = 3;\n");
  }
}

describe("discover", () => {
  test("sorts one accepted file and three rejections by path with their reasons", async () => {
    const repoRoot = await tempRepo();
    await seedFixture(repoRoot);

    const result = await discover(repoRoot);

    const keep = Buffer.from("export const kept = 1;\n");
    expect(result.files).toEqual([
      {
        path: "keep.ts",
        lang: "typescript",
        size: keep.byteLength,
        sha256: createHash("sha256").update(keep).digest("hex"),
      },
    ]);

    // Sorted by path, so `data.ts` precedes `ignored.ts` precedes `mystery.weird`; the reasons are
    // exactly the three the task names, and the pruned directories contribute no entries.
    expect(result.notIndexed).toEqual([
      { path: "data.ts", reason: "binary", language: "typescript" },
      { path: "ignored.ts", reason: "ignored", language: "typescript" },
      { path: "mystery.weird", reason: "unsupported_ext" },
    ]);
    expect(result.skipped).toBe(result.notIndexed.length);
  });

  test("is a pure function of the tree: two runs are identical", async () => {
    const repoRoot = await tempRepo();
    await seedFixture(repoRoot);

    const first = await discover(repoRoot);
    const second = await discover(repoRoot);

    expect(second).toEqual(first);
  });

  test("reports repo-relative forward-slash paths with no leading dot-slash", async () => {
    const repoRoot = await tempRepo();
    await mkdir(join(repoRoot, "src", "deep"), { recursive: true });
    await writeFile(join(repoRoot, "src", "deep", "leaf.ts"), "export const leaf = 1;\n");

    const result = await discover(repoRoot);

    const paths = [...result.files, ...result.notIndexed].map((entry) => entry.path);
    expect(paths).toEqual(["src/deep/leaf.ts"]);
    for (const path of paths) {
      expect(path.startsWith("/")).toBe(false);
      expect(path.startsWith("./")).toBe(false);
      expect(path.includes("\\")).toBe(false);
      expect(path.startsWith(repoRoot)).toBe(false);
    }
  });

  test("caps by stat before reading, so an oversized file is never opened", async () => {
    const repoRoot = await tempRepo();
    // Sparse, so the size is real to `stat` without 512 MiB of writes.
    const oversized = join(repoRoot, "huge.ts");
    await writeFile(oversized, "");
    await truncate(oversized, SIZE_CAP_BYTES + 1);

    const result = await discover(repoRoot);

    expect(result.files).toEqual([]);
    expect(result.notIndexed).toEqual([
      { path: "huge.ts", reason: "size_cap", language: "typescript" },
    ]);
  });

  test("honours negation and applies caller patterns after the control files", async () => {
    const repoRoot = await tempRepo();
    await writeFile(join(repoRoot, ".gitignore"), "keep.ts\n!notes.ts\n");
    await writeFile(join(repoRoot, "keep.ts"), "export const hidden = 1;\n");
    await writeFile(join(repoRoot, "notes.ts"), "export const kept = 2;\n");
    await writeFile(join(repoRoot, "extra.ts"), "export const extra = 3;\n");

    // `keep.ts` is dropped by the pattern, `notes.ts` is re-included by the later negation.
    const fromControlFile = await discover(repoRoot);
    expect(fromControlFile.files.map((file) => file.path)).toEqual(["extra.ts", "notes.ts"]);
    expect(fromControlFile.notIndexed).toEqual([
      { path: "keep.ts", reason: "ignored", language: "typescript" },
    ]);

    const withOption = await discover(repoRoot, { ignorePatterns: ["extra.ts"] });
    expect(withOption.files.map((file) => file.path)).toEqual(["notes.ts"]);
    expect(withOption.notIndexed).toEqual([
      { path: "extra.ts", reason: "ignored", language: "typescript" },
      { path: "keep.ts", reason: "ignored", language: "typescript" },
    ]);
  });
});
