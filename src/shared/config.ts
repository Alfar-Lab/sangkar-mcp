import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { globalConfigPath, readGlobalConfig } from "#shared/global-config";
import { type Logger, log } from "#shared/log";
import type { RepoDefaults } from "#shared/repo-config";

export interface Budgets {
  /**
   * Fallback token budget for a call that sends none. Per-tool defaults live in each tool's
   * manifest (TOOLS-SPEC.md §9); this is only the fallback for a handler without one.
   */
  defaultTokens: number;
  /** Ceiling above which `emit` refuses with `budget_too_large` (TOOLS-SPEC.md §9). */
  maxTokens: number;
}

export interface TelemetryConfig {
  /** Whether this installation may record usage events at all. */
  readonly enabled: boolean;
  /** Where events would be written; always computed so a caller can name the path without one. */
  readonly dir: string;
}

/** The two files and the one address a background daemon needs in the machine-level home. */
export interface DaemonPaths {
  readonly pidPath: string;
  readonly logPath: string;
  /** What `daemon run` binds when `--host` is absent; the loopback policy is `serve.ts`'s. */
  readonly host: string;
  /** What it binds when `--port` is absent; `0` asks the kernel for one. */
  readonly port: number;
}

/** The directory name under the config root, so `${XDG_CONFIG_HOME:-$HOME/.config}/sangkar`. */
export const HOME_DIR_NAME = "sangkar";

/**
 * The resolved process configuration. Resolved once at start; every module that needs a
 * setting receives it from here, because this file is the only reader of the environment.
 */
export interface AppConfig {
  /** Directory holding graph.db, write.lock and the data dir's own .gitignore. */
  dataDir: string;
  /** Absolute path of the indexed working tree. */
  repoRoot: string;
  /** Cursor HMAC key, passed to the cursor module so it never reads the environment. */
  cursorKey: string;
  budgets: Budgets;
  /** The consent switch as a value, so the recorder never reads the environment itself. */
  readonly telemetry: TelemetryConfig;
  /**
   * Machine-level home: `$SANGKAR_HOME`, else `<config root>/sangkar`. It belongs to the machine,
   * not to the tree, so `withRepo()` leaves it alone while swapping the repository fields.
   */
  readonly home: string;
  /** The daemon's repo registry, `<home>/sangkar.json`. */
  readonly registryPath: string;
  /** The machine-level settings file, `<home>/config.json`, created on the first run. */
  readonly configPath: string;
  /**
   * What a repository inherits before its own `<repo>/.sangkar/config.json` speaks: the machine
   * file's `watch`, `watch_interval` and `telemetry`.
   */
  readonly repoDefaults: RepoDefaults;
  /** Where the daemon writes its pid and its log, both under `<home>`, and what it binds. */
  readonly daemon: DaemonPaths;
}

export interface LoadConfigOptions {
  /** The flags to read, in production `process.argv.slice(2)` with the mode token still in it. */
  argv?: readonly string[];
  /** Injected for tests; production callers pass nothing. */
  env?: Record<string, string | undefined>;
  /** Injected for tests; defaults to the process working directory. */
  cwd?: string;
  /** Injected for tests; defaults to `#shared/log`. */
  log?: Logger;
}

export interface WithRepoOptions {
  /** Overrides the default `<repoRoot>/.sangkar`; resolved by the caller, not here. */
  readonly dataDir?: string;
}

/** The data dir's name inside a repository, per DATABASE.md §3.0. */
const DATA_DIR_NAME = ".sangkar";

/**
 * Value of a `--<name> value` or `--<name>=value` flag, or `undefined` when the flag is absent.
 *
 * The whole array is scanned in any position because the CLI hands over `process.argv.slice(2)`
 * with the mode token still in it, so a global flag may sit on either side of that token.
 */
export function flagValue(argv: readonly string[], name: string): string | undefined {
  const bare = `--${name}`;
  const inline = `${bare}=`;
  for (const [index, arg] of argv.entries()) {
    if (arg === bare) {
      return argv[index + 1];
    }
    if (arg.startsWith(inline)) {
      return arg.slice(inline.length);
    }
  }
  return undefined;
}

/**
 * The working tree that owns `cwd`: the nearest directory holding a `.sangkar` data dir, else
 * the nearest directory holding a `.git` (a directory, or a file in a linked worktree), else
 * `cwd` itself. Pure filesystem lookup — no `git` subprocess, so this works in a bare temp
 * directory and when git is not installed.
 */
export function detectRepoRoot(cwd: string): string {
  const start = resolve(cwd);
  let dir = start;
  for (;;) {
    if (existsSync(join(dir, DATA_DIR_NAME)) || existsSync(join(dir, ".git"))) {
      return dir;
    }
    const parent = dirname(dir);
    if (parent === dir) {
      // Reached the filesystem root without a marker: the caller's directory is the tree.
      return start;
    }
    dir = parent;
  }
}

/**
 * An empty variable is an absent one: `SANGKAR_HOME=` or `XDG_CONFIG_HOME=` in a shell profile must
 * not resolve to a working-directory-relative path, which would scatter the registry and the daemon
 * files through whatever tree the process happens to start in.
 */
function envValue(env: Record<string, string | undefined>, name: string): string | undefined {
  const value = env[name];
  return value === undefined || value === "" ? undefined : value;
}

/**
 * Expands a leading `~` against `env.HOME`, because the value comes from an env block or a config
 * file rather than a shell, so nothing else would have expanded it.
 */
function expandHome(path: string, env: Record<string, string | undefined>): string {
  if (!path.startsWith("~/")) {
    return path;
  }
  return join(envValue(env, "HOME") ?? homedir(), path.slice(2));
}

/**
 * The machine-level home: `SANGKAR_HOME` when set and non-empty, else the XDG config root with
 * `HOME_DIR_NAME` appended. Exported because the first run needs the home to create
 * `<home>/config.json` before a full `AppConfig` exists — and only this file may read the
 * environment, so `bin/sangkar.ts` derives it from here rather than from `process.env` itself.
 */
export function resolveHome(env: Record<string, string | undefined>): string {
  const explicit = envValue(env, "SANGKAR_HOME");
  if (explicit !== undefined) {
    return expandHome(explicit, env);
  }
  const configRoot =
    envValue(env, "XDG_CONFIG_HOME") ?? join(envValue(env, "HOME") ?? homedir(), ".config");
  return join(configRoot, HOME_DIR_NAME);
}

/**
 * Resolves the config from the command line and the working directory: `--repo-root` (default
 * autodetected from cwd), `--data-dir` relative to the repo root (default `<repoRoot>/.sangkar`),
 * then the machine-level file `<home>/config.json`, then the settings that come from the
 * environment: `SANGKAR_HOME` (the machine-level home, see `resolveHome`), `SANGKAR_CURSOR_KEY` and
 * the telemetry consent switch `SANGKAR_TELEMETRY` — the one spelling `off` disables it, any other
 * value enables it, and an unset variable defers to the file. The precedence is flag, then
 * environment, then file, then the built-in default; the old no-watch variable is gone, because the
 * daemon owns the watchers now.
 */
export function loadConfig(options: LoadConfigOptions = {}): AppConfig {
  const argv = options.argv ?? [];
  const env = options.env ?? process.env;
  const cwd = options.cwd ?? process.cwd();

  const rawRepoRoot = flagValue(argv, "repo-root");
  const repoRoot =
    rawRepoRoot === undefined || rawRepoRoot === ""
      ? detectRepoRoot(cwd)
      : resolve(cwd, rawRepoRoot);

  const rawDataDir = flagValue(argv, "data-dir");
  const dataDir =
    rawDataDir === undefined || rawDataDir === ""
      ? join(repoRoot, DATA_DIR_NAME)
      : resolve(repoRoot, rawDataDir);

  const home = resolveHome(env);
  // Read once, here: `#shared/config.ts` stays the only reader of the environment, and every
  // module that needs a machine default receives it on the resolved config instead of reopening
  // the file. A missing file is the fresh installation, so it is silent; a broken one is one
  // warning and the built-in defaults.
  const global = readGlobalConfig(home, { log: options.log ?? log });

  return {
    dataDir,
    repoRoot,
    home,
    registryPath: join(home, "sangkar.json"),
    configPath: globalConfigPath(home),
    repoDefaults: {
      watch: global.watch,
      watchIntervalMs: global.watchIntervalMs,
      telemetry: global.telemetry,
    },
    daemon: {
      pidPath: join(home, "daemon.pid"),
      logPath: join(home, "daemon.log"),
      host: global.daemon.host,
      port: global.daemon.port,
    },
    // An absent or empty variable yields a per-process key, so a cursor minted by one invocation
    // cannot be verified by another; set SANGKAR_CURSOR_KEY to share cursors across processes.
    cursorKey:
      env.SANGKAR_CURSOR_KEY === undefined || env.SANGKAR_CURSOR_KEY === ""
        ? randomBytes(32).toString("hex")
        : env.SANGKAR_CURSOR_KEY,
    budgets: global.budgets,
    // One spelling only: `off` disables, everything else (empty, "0", "false", "OFF") enables.
    // Aliases would make an operator's typo silently opt a machine in. An unset variable defers to
    // the machine-level file, so the file is the installation's default and the variable is the
    // launcher's override.
    telemetry: {
      enabled:
        env.SANGKAR_TELEMETRY === undefined ? global.telemetry : env.SANGKAR_TELEMETRY !== "off",
      dir: join(dataDir, "telemetry"),
    },
  };
}

/**
 * The same machine-level config rebound to another working tree: one process can serve N repos
 * without re-reading the environment, because the home, the registry, the daemon files, the key,
 * the budgets and the repository defaults are machine state rather than repo state.
 */
export function withRepo(
  config: AppConfig,
  repoRoot: string,
  options: WithRepoOptions = {},
): AppConfig {
  const root = resolve(repoRoot);
  const dataDir = options.dataDir ?? join(root, DATA_DIR_NAME);
  return {
    ...config,
    repoRoot: root,
    dataDir,
    telemetry: { ...config.telemetry, dir: join(dataDir, "telemetry") },
  };
}
