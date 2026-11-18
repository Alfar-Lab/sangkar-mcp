import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { detectRepoRoot, flagValue, loadConfig } from "#shared/config";
import { CURSOR_KEY_MIN_BYTES } from "#shared/cursor";

/**
 * Configuration is decided by the command line and the working directory, so every case here is a
 * `mkdtemp` tree with the markers the autodetect walk looks for. Nothing points at this
 * repository's own `.codegraph`, and the environment variables that used to carry the repo root,
 * the data dir and the profile are asserted to be inert.
 */

const dirs: string[] = [];

function tempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

/** A tree the autodetect walk stops at, marked the way the product marks it. */
function repoWithDataDir(prefix: string): string {
  const root = tempDir(prefix);
  mkdirSync(join(root, ".codegraph"), { recursive: true });
  return root;
}

function repoWithGitDir(prefix: string): string {
  const root = tempDir(prefix);
  mkdirSync(join(root, ".git"), { recursive: true });
  return root;
}

function nested(root: string, ...segments: string[]): string {
  const dir = join(root, ...segments);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/** Key strength in the unit the cursor guard measures, so the assertions match its check. */
function keyBytes(key: string): number {
  return Buffer.byteLength(key, "utf8");
}

afterAll(() => {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("flagValue", () => {
  test("reads --name value and --name=value", () => {
    expect(flagValue(["--repo-root", "/a"], "repo-root")).toBe("/a");
    expect(flagValue(["--repo-root=/a"], "repo-root")).toBe("/a");
  });

  test("returns undefined when the flag is absent, including a prefix lookalike", () => {
    expect(flagValue([], "repo-root")).toBeUndefined();
    expect(flagValue(["index", "--force"], "repo-root")).toBeUndefined();
    expect(flagValue(["--repo-roots", "/a"], "repo-root")).toBeUndefined();
  });

  test("scans the whole argv, so a flag after the mode token is still found", () => {
    expect(flagValue(["index", "extra-token", "--repo-root", "/a"], "repo-root")).toBe("/a");
    expect(flagValue(["index", "extra-token", "--repo-root=/a"], "repo-root")).toBe("/a");
  });
});

describe("detectRepoRoot", () => {
  test("finds the nearest .codegraph from a nested subdirectory", () => {
    const root = repoWithDataDir("cgraph-detect-data-");
    const deep = nested(root, "src", "shared", "store");
    expect(detectRepoRoot(deep)).toBe(root);
  });

  test("falls back to .git when no .codegraph exists, and prefers .codegraph over .git", () => {
    const gitRepo = repoWithGitDir("cgraph-detect-git-");
    expect(detectRepoRoot(nested(gitRepo, "src"))).toBe(gitRepo);

    const both = tempDir("cgraph-detect-both-");
    mkdirSync(join(both, ".git"), { recursive: true });
    mkdirSync(join(both, ".codegraph"), { recursive: true });
    expect(detectRepoRoot(nested(both, "src"))).toBe(both);
  });

  test("accepts .git as a file, as a linked worktree has", () => {
    const worktree = tempDir("cgraph-detect-worktree-");
    writeFileSync(join(worktree, ".git"), "gitdir: /elsewhere/.git/worktrees/wt\n");
    expect(detectRepoRoot(nested(worktree, "src"))).toBe(worktree);
  });

  test("returns the directory itself when nothing matches", () => {
    const bare = tempDir("cgraph-detect-bare-");
    expect(detectRepoRoot(bare)).toBe(resolve(bare));
  });
});

describe("loadConfig", () => {
  test("--repo-root wins over the autodetected tree", () => {
    const autodetected = repoWithDataDir("cgraph-config-auto-");
    const chosen = tempDir("cgraph-config-chosen-");
    const nestedDir = nested(autodetected, "src");

    // Sanity: without the flag the walk would land on `autodetected`.
    expect(detectRepoRoot(nestedDir)).toBe(autodetected);

    const config = loadConfig({ argv: ["status", "--repo-root", chosen], cwd: nestedDir });
    expect(config.repoRoot).toBe(chosen);
    expect(config.dataDir).toBe(join(chosen, ".codegraph"));
  });

  test("--repo-root=value works and a relative value resolves against cwd", () => {
    const parent = tempDir("cgraph-config-parent-");
    const child = nested(parent, "child");
    const config = loadConfig({ argv: [`--repo-root=${child}`], cwd: parent });
    expect(config.repoRoot).toBe(child);

    const relative = loadConfig({ argv: ["--repo-root=child"], cwd: parent });
    expect(relative.repoRoot).toBe(child);
  });

  test("a flag placed after another token is still applied", () => {
    const chosen = tempDir("cgraph-config-trailing-");
    const bare = loadConfig({
      argv: ["index", "some-token", "--repo-root", chosen],
      cwd: tmpdir(),
    });
    expect(bare.repoRoot).toBe(chosen);

    const inline = loadConfig({
      argv: ["index", "some-token", `--repo-root=${chosen}`],
      cwd: tmpdir(),
    });
    expect(inline.repoRoot).toBe(chosen);
  });

  test("autodetects the tree from a nested cwd and puts the data dir inside it", () => {
    const root = repoWithDataDir("cgraph-config-nested-");
    const config = loadConfig({ argv: [], cwd: nested(root, "app", "src") });
    expect(config.repoRoot).toBe(root);
    expect(config.dataDir).toBe(join(root, ".codegraph"));
  });

  test("falls back to cwd in a bare directory", () => {
    const bare = tempDir("cgraph-config-bare-");
    const config = loadConfig({ env: {}, cwd: bare });
    expect(config.repoRoot).toBe(resolve(bare));
    expect(config.dataDir).toBe(join(resolve(bare), ".codegraph"));
  });

  test("--data-dir resolves against the repo root", () => {
    const root = tempDir("cgraph-config-data-");
    const relative = loadConfig({
      argv: ["--repo-root", root, "--data-dir", "var/graph"],
      cwd: tmpdir(),
    });
    expect(relative.repoRoot).toBe(root);
    expect(relative.dataDir).toBe(join(root, "var", "graph"));

    const absolute = join(tempDir("cgraph-config-absdata-"), "elsewhere");
    const inline = loadConfig({
      argv: [`--repo-root=${root}`, `--data-dir=${absolute}`],
      cwd: tmpdir(),
    });
    expect(inline.dataDir).toBe(absolute);
  });

  test("ignores CODEGRAPH_REPO_ROOT, CODEGRAPH_DATA_DIR and CODEGRAPH_PROFILE", () => {
    const bare = tempDir("cgraph-config-envignored-");
    const env = {
      CODEGRAPH_REPO_ROOT: "/should/not/be/used",
      CODEGRAPH_DATA_DIR: "/should/not/be/used/either",
      CODEGRAPH_PROFILE: "admin",
      CODEGRAPH_CURSOR_KEY: "k".repeat(32),
    };

    const config = loadConfig({ argv: [], env, cwd: bare });
    expect(config.repoRoot).toBe(resolve(bare));
    expect(config.dataDir).toBe(join(resolve(bare), ".codegraph"));
    expect(config).not.toHaveProperty("profile");
    expect(config.cursorKey).toBe("k".repeat(32));

    // The tree comes from the flag even while the stale variables are set.
    const flagged = tempDir("cgraph-config-envignored-flag-");
    const overridden = loadConfig({ argv: ["--repo-root", flagged], env, cwd: bare });
    expect(overridden.repoRoot).toBe(flagged);
    expect(overridden.dataDir).toBe(join(flagged, ".codegraph"));
  });

  test("keeps the fallback budgets and supplies a key when unset", () => {
    const bare = tempDir("cgraph-config-budget-");
    const config = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(config.budgets.defaultTokens).toBe(1_200);
    expect(config.budgets.maxTokens).toBe(32_000);
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });
});

describe("loadConfig cursor key", () => {
  test("generates a key long enough for the cursor module when the variable is absent", () => {
    const bare = tempDir("cgraph-config-key-absent-");
    const config = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });

  test("generates a different key per call, so the fallback is random rather than constant", () => {
    const bare = tempDir("cgraph-config-key-random-");
    const first = loadConfig({ argv: [], env: {}, cwd: bare });
    const second = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(first.cursorKey).not.toBe(second.cursorKey);
  });

  test("returns an explicit variable verbatim, including one too short for cursors", () => {
    const bare = tempDir("cgraph-config-key-explicit-");
    const long = loadConfig({
      argv: [],
      env: { CODEGRAPH_CURSOR_KEY: "k".repeat(CURSOR_KEY_MIN_BYTES) },
      cwd: bare,
    });
    expect(long.cursorKey).toBe("k".repeat(CURSOR_KEY_MIN_BYTES));

    // The length check belongs to cursor.ts, which refuses a short key at startup; config only
    // reports what the environment supplied.
    const short = loadConfig({
      argv: [],
      env: { CODEGRAPH_CURSOR_KEY: "short" },
      cwd: bare,
    });
    expect(short.cursorKey).toBe("short");
  });

  test("treats an empty variable as absent and generates a key", () => {
    const bare = tempDir("cgraph-config-key-empty-");
    const config = loadConfig({ argv: [], env: { CODEGRAPH_CURSOR_KEY: "" }, cwd: bare });
    expect(config.cursorKey).not.toBe("");
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });
});
