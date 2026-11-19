import { randomBytes } from "node:crypto";
import { existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

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
}

export interface LoadConfigOptions {
  /** The flags to read, in production `process.argv.slice(2)` with the mode token still in it. */
  argv?: readonly string[];
  /** Injected for tests; production callers pass nothing. */
  env?: Record<string, string | undefined>;
  /** Injected for tests; defaults to the process working directory. */
  cwd?: string;
}

const DEFAULT_BUDGET_TOKENS = 1_200;
const MAX_BUDGET_TOKENS = 32_000;

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
 * Resolves the config from the command line and the working directory: `--repo-root` (default
 * autodetected from cwd), `--data-dir` relative to the repo root (default `<repoRoot>/.sangkar`),
 * then the two settings that come from the environment: `SANGKAR_CURSOR_KEY` and the telemetry
 * consent switch `SANGKAR_TELEMETRY` — unset or empty means enabled, and the one spelling `off`
 * disables it. `SANGKAR_NO_WATCH` is deliberately not read here; it is not a tool setting, it is
 * this process deciding whether it owns a poller (see `watcherDisabled` in `src/index.ts`).
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

  return {
    dataDir,
    repoRoot,
    // An absent or empty variable yields a per-process key, so a cursor minted by one invocation
    // cannot be verified by another; set SANGKAR_CURSOR_KEY to share cursors across processes.
    cursorKey:
      env.SANGKAR_CURSOR_KEY === undefined || env.SANGKAR_CURSOR_KEY === ""
        ? randomBytes(32).toString("hex")
        : env.SANGKAR_CURSOR_KEY,
    budgets: { defaultTokens: DEFAULT_BUDGET_TOKENS, maxTokens: MAX_BUDGET_TOKENS },
    // One spelling only: `off` disables, everything else (absent, empty, "0", "false", "OFF")
    // enables. Aliases would make an operator's typo silently opt a machine in.
    telemetry: {
      enabled: env.SANGKAR_TELEMETRY !== "off",
      dir: join(dataDir, "telemetry"),
    },
  };
}
