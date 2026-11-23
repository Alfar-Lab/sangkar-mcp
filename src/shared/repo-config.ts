import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { type Logger, log } from "#shared/log";

/**
 * The per-repo settings file (`<dataDir>/config.json`): whether this tree is watched, how often,
 * and whether its usage may be recorded. The file is hand-editable and outlives any one process,
 * so every read is defensive — a broken file degrades to the defaults, never to a crash in the
 * daemon or a tool call.
 *
 * A repository that says nothing inherits the machine-level file (`<home>/config.json`, see
 * `./global-config.ts`): this file narrows those defaults, it does not restate them.
 */

export const WATCH_INTERVAL_DEFAULT_MS = 2_000;
/** Below this a poll costs more than the edit it notices: one save lands in every window. */
export const WATCH_INTERVAL_MIN_MS = 1_000;
/** Above this the graph is stale between polls for as long as a working day. */
export const WATCH_INTERVAL_MAX_MS = 300_000;

/**
 * Which file the values came from: this tree's own `<dataDir>/config.json`, or the machine default.
 * Reported as `status.payload.config.source`, so "the tree says nothing" is visible without a log line.
 */
export type RepoConfigSource = "project" | "global";

export interface RepoConfig {
  readonly repoRoot: string;
  readonly dataDir: string;
  readonly watch: boolean;
  readonly watchIntervalMs: number;
  readonly telemetry: boolean;
  readonly source: RepoConfigSource;
}

/** The three values a repository's file can override, and what a missing field falls back to. */
export interface RepoDefaults {
  readonly watch: boolean;
  readonly watchIntervalMs: number;
  readonly telemetry: boolean;
}

/**
 * The built-in fallback, used only when no machine-level file supplies one. Both switches are
 * `true` because this file only narrows what the machine allows; the machine-level switch is the
 * one an operator turns things off with, and `recordingEnabled` still requires consent and
 * registration.
 */
export const DEFAULT_REPO_SETTINGS: RepoDefaults = {
  watch: true,
  watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
  telemetry: true,
};

/** Mirrors the private `DATA_DIR_NAME` in `config.ts`, which is not exported for import. */
const DATA_DIR_NAME = ".sangkar";

export function repoConfigPath(dataDir: string): string {
  return join(dataDir, "config.json");
}

/** The file's own spellings. Snake case on disk, camel case in TypeScript, mapped only here. */
interface RepoConfigFile {
  readonly watch?: unknown;
  readonly watch_interval?: unknown;
  readonly telemetry?: unknown;
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * The interval in milliseconds, clamped into the supported band. A number outside it is a
 * mistake in the file, not a reason to stop watching, so both ends are clamped rather than
 * honoured; a non-integer, a zero, a negative or a wrong type is not a slow poll but nonsense,
 * and takes `fallback` — the machine default when one was supplied, else the built-in period.
 */
export function clampWatchInterval(
  raw: unknown,
  fallback: number = WATCH_INTERVAL_DEFAULT_MS,
): number {
  if (typeof raw !== "number" || !Number.isInteger(raw) || raw <= 0) {
    return fallback;
  }
  if (raw < WATCH_INTERVAL_MIN_MS) {
    return WATCH_INTERVAL_MIN_MS;
  }
  return Math.min(raw, WATCH_INTERVAL_MAX_MS);
}

/**
 * The settings for `repoRoot`, read from `<dataDir>/config.json`. A missing file is silent — it is
 * the normal state of a repo that was indexed but never configured, the values fall back to the
 * machine file, and `source: "global"` says so in `status`. Any other rejection keeps its warning:
 * an unreadable or malformed file is something an operator meant to have, and a file-level failure
 * falls back to every default while a bad field falls back alone, so one typo does not silently
 * discard the others. `defaults` is the machine-level fallback (`<home>/config.json`); the built-in
 * one is used when a caller has none.
 */
export function readRepoConfig(
  repoRoot: string,
  options: {
    readonly dataDir?: string;
    readonly defaults?: RepoDefaults;
    readonly log?: Logger;
  } = {},
): RepoConfig {
  const dataDir = options.dataDir ?? join(repoRoot, DATA_DIR_NAME);
  const path = repoConfigPath(dataDir);
  const logger = options.log ?? log;
  const machine = options.defaults ?? DEFAULT_REPO_SETTINGS;

  // `warn` is what separates "the tree says nothing, which is normal" from "the tree says something
  // this module cannot use": the first is reported by `source` alone, the second on stderr as well.
  const fallback = (reason: string, warn: boolean): RepoConfig => {
    if (warn) {
      logger.warn("repo config invalid, using the global config", { path, reason });
    }
    return { repoRoot, dataDir, ...machine, source: "global" };
  };

  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (e) {
    // A missing file is the normal state of a repo that was indexed but never configured, so it is
    // answered with the machine values and no log line; `error.code` is the only signal that tells
    // it apart from a file the operator wrote and cannot now be read.
    return fallback(`unreadable: ${messageOf(e)}`, (e as NodeJS.ErrnoException).code !== "ENOENT");
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    return fallback(`invalid JSON: ${messageOf(e)}`, true);
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return fallback("not a JSON object", true);
  }

  const file = parsed as RepoConfigFile;
  const flag = (name: string, value: unknown, absent: boolean): boolean => {
    if (typeof value === "boolean") {
      return value;
    }
    // JSON has no truthiness contract: `"yes"` or `1` here is a typo, and guessing would let a
    // quoted `"false"` read as consent.
    logger.warn("repo config field ignored, using the default", {
      path,
      field: name,
      reason: value === undefined ? "missing" : "not a JSON boolean",
    });
    return absent;
  };

  return {
    repoRoot,
    dataDir,
    watch: flag("watch", file.watch, machine.watch),
    // An absent or nonsensical interval inherits the machine default, so a repo file that only
    // turns watching off never silently retunes it to the built-in period.
    watchIntervalMs: clampWatchInterval(file.watch_interval, machine.watchIntervalMs),
    telemetry: flag("telemetry", file.telemetry, machine.telemetry),
    source: "project",
  };
}

/**
 * Writes the three settings in the on-disk shape, creating the data dir because a repo that was
 * never indexed has none yet. The caller owns the write lock elsewhere; this is one small file.
 */
export function writeRepoConfig(config: Omit<RepoConfig, "source">): void {
  mkdirSync(config.dataDir, { recursive: true });
  writeFileSync(
    repoConfigPath(config.dataDir),
    JSON.stringify({
      watch: config.watch,
      watch_interval: config.watchIntervalMs,
      telemetry: config.telemetry,
    }),
  );
}

/**
 * The recording precedence: machine consent, then registration, then the repo file. A repo cannot
 * opt itself in past an operator's `SANGKAR_TELEMETRY=off`, and an unregistered tree is not
 * recorded no matter what its own file says.
 */
export function recordingEnabled(input: {
  readonly consent: boolean;
  readonly repo: RepoConfig;
  readonly registered: boolean;
}): boolean {
  return input.consent && input.registered && input.repo.telemetry;
}
