import { join, resolve } from "node:path";
import { AppError } from "#shared/errors";

/** Tool profiles are a hard allow-list at the protocol layer (DESIGN.md §4). */
export const PROFILES = ["agent", "admin"] as const;
export type Profile = (typeof PROFILES)[number];

export interface Budgets {
  /**
   * Fallback token budget for a call that sends none. Per-tool defaults live in each tool's
   * manifest (TOOLS-SPEC.md §9); this is only the fallback for a handler without one.
   */
  defaultTokens: number;
  /** Ceiling above which `emit` refuses with `budget_too_large` (TOOLS-SPEC.md §9). */
  maxTokens: number;
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
  profile: Profile;
  /** Cursor HMAC key, passed to the cursor module so it never reads the environment. */
  cursorKey: string;
  budgets: Budgets;
}

export interface LoadConfigOptions {
  /** Injected for tests; production callers pass nothing. */
  env?: Record<string, string | undefined>;
  /** Injected for tests; defaults to the process working directory. */
  cwd?: string;
}

const DEFAULT_BUDGET_TOKENS = 1_200;
const MAX_BUDGET_TOKENS = 32_000;

function isProfile(value: string): value is Profile {
  return (PROFILES as readonly string[]).includes(value);
}

/**
 * Resolves the config from the environment: `CODEGRAPH_REPO_ROOT` (default cwd),
 * `CODEGRAPH_DATA_DIR` (default `<repoRoot>/.codegraph`, per DATABASE.md §3.0),
 * `CODEGRAPH_PROFILE` (default `agent`) and `CODEGRAPH_CURSOR_KEY`.
 *
 * An unrecognised profile is fatal: a profile is an allow-list, so guessing one would
 * silently widen or narrow the tool surface.
 */
export function loadConfig(options: LoadConfigOptions = {}): AppConfig {
  const env = options.env ?? process.env;

  const repoRoot = resolve(env.CODEGRAPH_REPO_ROOT ?? options.cwd ?? process.cwd());

  const rawDataDir = env.CODEGRAPH_DATA_DIR;
  const dataDir =
    rawDataDir === undefined || rawDataDir === ""
      ? join(repoRoot, ".codegraph")
      : resolve(repoRoot, rawDataDir);

  const rawProfile = env.CODEGRAPH_PROFILE ?? "";
  const profileName = rawProfile === "" ? "agent" : rawProfile;
  if (!isProfile(profileName)) {
    throw new AppError("internal", `unknown profile "${profileName}"`, {
      hint: `set CODEGRAPH_PROFILE to one of: ${PROFILES.join(", ")}`,
    });
  }

  return {
    dataDir,
    repoRoot,
    profile: profileName,
    // Empty when unset; the cursor module owns the length check so the key stays in one place.
    cursorKey: env.CODEGRAPH_CURSOR_KEY ?? "",
    budgets: { defaultTokens: DEFAULT_BUDGET_TOKENS, maxTokens: MAX_BUDGET_TOKENS },
  };
}
