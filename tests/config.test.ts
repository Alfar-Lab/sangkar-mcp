import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { detectRepoRoot, flagValue, HOME_DIR_NAME, loadConfig, withRepo } from "#shared/config";
import { CURSOR_KEY_MIN_BYTES } from "#shared/cursor";

/**
 * Configuration is decided by the command line and the working directory, so every case here is a
 * `mkdtemp` tree with the markers the autodetect walk looks for. Nothing points at this
 * repository's own `.sangkar`, and the environment variables that used to carry the repo root,
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
  mkdirSync(join(root, ".sangkar"), { recursive: true });
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
  test("finds the nearest .sangkar from a nested subdirectory", () => {
    const root = repoWithDataDir("sangkar-detect-data-");
    const deep = nested(root, "src", "shared", "store");
    expect(detectRepoRoot(deep)).toBe(root);
  });

  test("falls back to .git when no .sangkar exists, and prefers .sangkar over .git", () => {
    const gitRepo = repoWithGitDir("sangkar-detect-git-");
    expect(detectRepoRoot(nested(gitRepo, "src"))).toBe(gitRepo);

    const both = tempDir("sangkar-detect-both-");
    mkdirSync(join(both, ".git"), { recursive: true });
    mkdirSync(join(both, ".sangkar"), { recursive: true });
    expect(detectRepoRoot(nested(both, "src"))).toBe(both);
  });

  test("accepts .git as a file, as a linked worktree has", () => {
    const worktree = tempDir("sangkar-detect-worktree-");
    writeFileSync(join(worktree, ".git"), "gitdir: /elsewhere/.git/worktrees/wt\n");
    expect(detectRepoRoot(nested(worktree, "src"))).toBe(worktree);
  });

  test("returns the directory itself when nothing matches", () => {
    const bare = tempDir("sangkar-detect-bare-");
    expect(detectRepoRoot(bare)).toBe(resolve(bare));
  });
});

describe("loadConfig", () => {
  test("--repo-root wins over the autodetected tree", () => {
    const autodetected = repoWithDataDir("sangkar-config-auto-");
    const chosen = tempDir("sangkar-config-chosen-");
    const nestedDir = nested(autodetected, "src");

    // Sanity: without the flag the walk would land on `autodetected`.
    expect(detectRepoRoot(nestedDir)).toBe(autodetected);

    const config = loadConfig({ argv: ["status", "--repo-root", chosen], cwd: nestedDir });
    expect(config.repoRoot).toBe(chosen);
    expect(config.dataDir).toBe(join(chosen, ".sangkar"));
  });

  test("--repo-root=value works and a relative value resolves against cwd", () => {
    const parent = tempDir("sangkar-config-parent-");
    const child = nested(parent, "child");
    const config = loadConfig({ argv: [`--repo-root=${child}`], cwd: parent });
    expect(config.repoRoot).toBe(child);

    const relative = loadConfig({ argv: ["--repo-root=child"], cwd: parent });
    expect(relative.repoRoot).toBe(child);
  });

  test("a flag placed after another token is still applied", () => {
    const chosen = tempDir("sangkar-config-trailing-");
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
    const root = repoWithDataDir("sangkar-config-nested-");
    const config = loadConfig({ argv: [], cwd: nested(root, "app", "src") });
    expect(config.repoRoot).toBe(root);
    expect(config.dataDir).toBe(join(root, ".sangkar"));
  });

  test("falls back to cwd in a bare directory", () => {
    const bare = tempDir("sangkar-config-bare-");
    const config = loadConfig({ env: {}, cwd: bare });
    expect(config.repoRoot).toBe(resolve(bare));
    expect(config.dataDir).toBe(join(resolve(bare), ".sangkar"));
  });

  test("--data-dir resolves against the repo root", () => {
    const root = tempDir("sangkar-config-data-");
    const relative = loadConfig({
      argv: ["--repo-root", root, "--data-dir", "var/graph"],
      cwd: tmpdir(),
    });
    expect(relative.repoRoot).toBe(root);
    expect(relative.dataDir).toBe(join(root, "var", "graph"));

    const absolute = join(tempDir("sangkar-config-absdata-"), "elsewhere");
    const inline = loadConfig({
      argv: [`--repo-root=${root}`, `--data-dir=${absolute}`],
      cwd: tmpdir(),
    });
    expect(inline.dataDir).toBe(absolute);
  });

  test("ignores SANGKAR_REPO_ROOT, SANGKAR_DATA_DIR and SANGKAR_PROFILE", () => {
    const bare = tempDir("sangkar-config-envignored-");
    const env = {
      SANGKAR_REPO_ROOT: "/should/not/be/used",
      SANGKAR_DATA_DIR: "/should/not/be/used/either",
      SANGKAR_PROFILE: "admin",
      SANGKAR_CURSOR_KEY: "k".repeat(32),
    };

    const config = loadConfig({ argv: [], env, cwd: bare });
    expect(config.repoRoot).toBe(resolve(bare));
    expect(config.dataDir).toBe(join(resolve(bare), ".sangkar"));
    expect(config).not.toHaveProperty("profile");
    expect(config.cursorKey).toBe("k".repeat(32));

    // The tree comes from the flag even while the stale variables are set.
    const flagged = tempDir("sangkar-config-envignored-flag-");
    const overridden = loadConfig({ argv: ["--repo-root", flagged], env, cwd: bare });
    expect(overridden.repoRoot).toBe(flagged);
    expect(overridden.dataDir).toBe(join(flagged, ".sangkar"));
  });

  test("keeps the fallback budgets and supplies a key when unset", () => {
    const bare = tempDir("sangkar-config-budget-");
    const config = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(config.budgets.defaultTokens).toBe(1_200);
    expect(config.budgets.maxTokens).toBe(32_000);
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });
});

describe("loadConfig home", () => {
  test("SANGKAR_HOME wins over the XDG config root and names the derived paths", () => {
    const bare = tempDir("sangkar-home-explicit-");
    const config = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: "/machine/home", XDG_CONFIG_HOME: "/xdg/config", HOME: "/home/tester" },
      cwd: bare,
    });
    expect(config.home).toBe("/machine/home");
    expect(config.registryPath).toBe(join("/machine/home", "sangkar.json"));
    expect(config.daemon.pidPath).toBe(join("/machine/home", "daemon.pid"));
    expect(config.daemon.logPath).toBe(join("/machine/home", "daemon.log"));
  });

  test("XDG_CONFIG_HOME is the parent of the sangkar dir when SANGKAR_HOME is unset", () => {
    const bare = tempDir("sangkar-home-xdg-");
    const config = loadConfig({
      argv: [],
      env: { XDG_CONFIG_HOME: "/xdg/config", HOME: "/home/tester" },
      cwd: bare,
    });
    expect(config.home).toBe(join("/xdg/config", HOME_DIR_NAME));
    expect(config.registryPath).toBe(join("/xdg/config", HOME_DIR_NAME, "sangkar.json"));
    expect(config.daemon.pidPath).toBe(join("/xdg/config", HOME_DIR_NAME, "daemon.pid"));
  });

  test("both unset falls back to $HOME/.config/sangkar", () => {
    const bare = tempDir("sangkar-home-fallback-");
    const config = loadConfig({ argv: [], env: { HOME: "/home/tester" }, cwd: bare });
    expect(config.home).toBe(join("/home/tester", ".config", HOME_DIR_NAME));
    expect(config.daemon.logPath).toBe(
      join("/home/tester", ".config", HOME_DIR_NAME, "daemon.log"),
    );
  });

  test("an empty SANGKAR_HOME counts as absent, so the fallbacks decide", () => {
    const bare = tempDir("sangkar-home-empty-");
    const withXdg = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: "", XDG_CONFIG_HOME: "/xdg/config" },
      cwd: bare,
    });
    expect(withXdg.home).toBe(join("/xdg/config", HOME_DIR_NAME));

    // A bare `SANGKAR_HOME=` in a shell profile must not put the registry in the working directory.
    const homeOnly = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: "", HOME: "/home/tester" },
      cwd: bare,
    });
    expect(homeOnly.home).toBe(join("/home/tester", ".config", HOME_DIR_NAME));
  });

  test("an empty XDG_CONFIG_HOME counts as absent too", () => {
    const bare = tempDir("sangkar-home-empty-xdg-");
    const config = loadConfig({
      argv: [],
      env: { XDG_CONFIG_HOME: "", HOME: "/home/tester" },
      cwd: bare,
    });
    // `join("", "sangkar")` would be the relative "sangkar": a cwd-relative home, not a machine one.
    expect(config.home).toBe(join("/home/tester", ".config", HOME_DIR_NAME));
  });

  test("a leading ~/ expands against HOME, because no shell expanded it", () => {
    const bare = tempDir("sangkar-home-tilde-");
    const config = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: "~/machine/sangkar", HOME: "/home/tester" },
      cwd: bare,
    });
    expect(config.home).toBe(join("/home/tester", "machine", "sangkar"));
  });

  test("is machine state: --repo-root and --data-dir do not move it", () => {
    const root = tempDir("sangkar-home-machine-");
    const config = loadConfig({
      argv: ["--repo-root", root, "--data-dir", "var/graph"],
      env: { SANGKAR_HOME: "/machine/home" },
      cwd: tmpdir(),
    });
    expect(config.dataDir).toBe(join(root, "var", "graph"));
    expect(config.home).toBe("/machine/home");
    expect(config.registryPath).toBe(join("/machine/home", "sangkar.json"));
  });
});

describe("withRepo", () => {
  test("swaps the repo fields and copies every machine field untouched", () => {
    const first = tempDir("sangkar-withrepo-first-");
    const second = tempDir("sangkar-withrepo-second-");
    const base = loadConfig({
      argv: ["--repo-root", first],
      env: { SANGKAR_HOME: "/machine/home", SANGKAR_CURSOR_KEY: "k".repeat(32) },
      cwd: tmpdir(),
    });
    expect(base.repoRoot).toBe(first);

    const rebound = withRepo(base, second);
    expect(rebound).not.toBe(base);
    expect(rebound.repoRoot).toBe(resolve(second));
    expect(rebound.dataDir).toBe(join(resolve(second), ".sangkar"));
    expect(rebound.telemetry.dir).toBe(join(rebound.dataDir, "telemetry"));

    expect(rebound.home).toBe(base.home);
    expect(rebound.registryPath).toBe(base.registryPath);
    expect(rebound.daemon).toEqual(base.daemon);
    expect(rebound.cursorKey).toBe(base.cursorKey);
    expect(rebound.budgets).toEqual(base.budgets);
    expect(rebound.telemetry.enabled).toBe(base.telemetry.enabled);
    // The argument config keeps the first tree, so one binding cannot silently move another.
    expect(base.repoRoot).toBe(first);
    expect(base.dataDir).toBe(join(first, ".sangkar"));
    expect(base.telemetry.dir).toBe(join(first, ".sangkar", "telemetry"));
  });

  test("resolves the root it is given and honours an explicit data dir", () => {
    const parent = tempDir("sangkar-withrepo-parent-");
    const base = loadConfig({ argv: [], env: { SANGKAR_HOME: "/machine/home" }, cwd: parent });

    // `resolve` is the process cwd's business, not the previous root's: the caller hands over a path.
    expect(withRepo(base, "child").repoRoot).toBe(resolve("child"));

    const explicit = withRepo(base, parent, { dataDir: join(parent, "var", "graph") });
    expect(explicit.repoRoot).toBe(parent);
    expect(explicit.dataDir).toBe(join(parent, "var", "graph"));
    expect(explicit.telemetry.dir).toBe(join(parent, "var", "graph", "telemetry"));
    // Consent is a machine-level switch, so the rebound config carries it, not re-derives it.
    expect(explicit.telemetry).not.toBe(base.telemetry);
    expect(explicit.telemetry.enabled).toBe(true);
  });

  test("carries a disabled consent switch across the rebind", () => {
    const bare = tempDir("sangkar-withrepo-off-");
    const other = tempDir("sangkar-withrepo-off-other-");
    const base = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: "/machine/home", SANGKAR_TELEMETRY: "off" },
      cwd: bare,
    });
    const rebound = withRepo(base, other);
    expect(rebound.telemetry.enabled).toBe(false);
    expect(rebound.telemetry.dir).toBe(join(resolve(other), ".sangkar", "telemetry"));
  });
});

describe("loadConfig telemetry", () => {
  test("an unset switch means enabled", () => {
    const bare = tempDir("sangkar-telemetry-unset-");
    const config = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(config.telemetry.enabled).toBe(true);
  });

  test("an empty switch means enabled", () => {
    const bare = tempDir("sangkar-telemetry-empty-");
    const config = loadConfig({ argv: [], env: { SANGKAR_TELEMETRY: "" }, cwd: bare });
    expect(config.telemetry.enabled).toBe(true);
  });

  test("the exact spelling off disables, with no aliases", () => {
    const bare = tempDir("sangkar-telemetry-off-");
    expect(
      loadConfig({ argv: [], env: { SANGKAR_TELEMETRY: "off" }, cwd: bare }).telemetry.enabled,
    ).toBe(false);

    // One spelling only: a value that reads as a negation elsewhere is still consent.
    for (const value of ["OFF", "Off", "0", "false"]) {
      const config = loadConfig({ argv: [], env: { SANGKAR_TELEMETRY: value }, cwd: bare });
      expect(config.telemetry.enabled).toBe(true);
    }
  });

  test("the dir is always computed inside the data dir, including under --data-dir", () => {
    const bare = tempDir("sangkar-telemetry-dir-");
    const autodetected = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(autodetected.telemetry.dir).toBe(join(autodetected.dataDir, "telemetry"));

    const relocated = loadConfig({
      argv: ["--repo-root", bare, "--data-dir", "var/graph"],
      env: { SANGKAR_TELEMETRY: "off" },
      cwd: tmpdir(),
    });
    expect(relocated.telemetry.dir).toBe(join(relocated.dataDir, "telemetry"));
    expect(relocated.telemetry.dir).toBe(join(bare, "var", "graph", "telemetry"));
  });
});

describe("loadConfig cursor key", () => {
  test("generates a key long enough for the cursor module when the variable is absent", () => {
    const bare = tempDir("sangkar-config-key-absent-");
    const config = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });

  test("generates a different key per call, so the fallback is random rather than constant", () => {
    const bare = tempDir("sangkar-config-key-random-");
    const first = loadConfig({ argv: [], env: {}, cwd: bare });
    const second = loadConfig({ argv: [], env: {}, cwd: bare });
    expect(first.cursorKey).not.toBe(second.cursorKey);
  });

  test("returns an explicit variable verbatim, including one too short for cursors", () => {
    const bare = tempDir("sangkar-config-key-explicit-");
    const long = loadConfig({
      argv: [],
      env: { SANGKAR_CURSOR_KEY: "k".repeat(CURSOR_KEY_MIN_BYTES) },
      cwd: bare,
    });
    expect(long.cursorKey).toBe("k".repeat(CURSOR_KEY_MIN_BYTES));

    // The length check belongs to cursor.ts, which refuses a short key at startup; config only
    // reports what the environment supplied.
    const short = loadConfig({
      argv: [],
      env: { SANGKAR_CURSOR_KEY: "short" },
      cwd: bare,
    });
    expect(short.cursorKey).toBe("short");
  });

  test("treats an empty variable as absent and generates a key", () => {
    const bare = tempDir("sangkar-config-key-empty-");
    const config = loadConfig({ argv: [], env: { SANGKAR_CURSOR_KEY: "" }, cwd: bare });
    expect(config.cursorKey).not.toBe("");
    expect(keyBytes(config.cursorKey)).toBeGreaterThanOrEqual(CURSOR_KEY_MIN_BYTES);
  });
});
