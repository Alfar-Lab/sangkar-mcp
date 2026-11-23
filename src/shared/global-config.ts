import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { MAX_BUDGET_TOKENS } from "#shared/budget";
import { type Logger, log } from "#shared/log";
import {
  clampWatchInterval,
  DEFAULT_REPO_SETTINGS,
  type RepoDefaults,
  WATCH_INTERVAL_DEFAULT_MS,
} from "#shared/repo-config";

/**
 * The machine-level settings file (`<home>/config.json`), the sibling of `sangkar.json`: what this
 * installation's repositories inherit until their own `<repo>/.sangkar/config.json` says otherwise,
 * plus the two budgets and the report server's default address.
 *
 * Why a file next to the registry rather than more environment variables: a preference written to a
 * file outlives the shell that exported it, and an operator can see and edit every default in one
 * place. The variables that remain (`SANGKAR_HOME`, `SANGKAR_TELEMETRY`, `SANGKAR_CURSOR_KEY`) are
 * the ones a launcher sets, so they outrank this file rather than being replaced by it.
 *
 * The file is created with the defaults on the first real run (`bin/sangkar.ts`) and is
 * hand-editable afterwards, so every read is defensive: a broken file degrades to the defaults with
 * one warning, never to a crash in the server, the daemon or the CLI.
 */

/** The file's name inside the machine home; `sangkar.json` is the registry beside it. */
export const GLOBAL_CONFIG_NAME = "config.json";

/**
 * The report server's defaults. `serve.ts` owns the loopback policy and refuses anything else, but
 * these two values are what a machine that has not edited the file gets; a test pins them against
 * that module so the pair cannot drift.
 */
export const DEFAULT_REPORT_HOST = "127.0.0.1";
export const DEFAULT_REPORT_PORT = 7317;

/** The two ceilings `#shared/config.ts` hands every process, in the file's own vocabulary. */
export interface GlobalBudgets {
  readonly defaultTokens: number;
  readonly maxTokens: number;
}

export interface GlobalDaemon {
  readonly host: string;
  readonly port: number;
}

export interface GlobalConfig {
  readonly watch: boolean;
  readonly watchIntervalMs: number;
  readonly telemetry: boolean;
  readonly daemon: GlobalDaemon;
  readonly budgets: GlobalBudgets;
}

/**
 * What a missing file means, and what `ensureGlobalConfig` writes. The switches are `true` and the
 * interval is the built-in period because the machine file is the widest scope there is: a default
 * that silently narrowed what a repository may do would be a policy nobody asked for.
 */
export const DEFAULT_GLOBAL_SETTINGS: GlobalConfig = {
  watch: DEFAULT_REPO_SETTINGS.watch,
  watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
  telemetry: DEFAULT_REPO_SETTINGS.telemetry,
  daemon: { host: DEFAULT_REPORT_HOST, port: DEFAULT_REPORT_PORT },
  budgets: { defaultTokens: 1_200, maxTokens: MAX_BUDGET_TOKENS },
};

/** The file's own spellings. Snake case on disk, camel case in TypeScript, mapped only here. */
interface GlobalConfigFile {
  readonly watch?: unknown;
  readonly watch_interval?: unknown;
  readonly telemetry?: unknown;
  readonly daemon?: unknown;
  readonly budgets?: unknown;
}

export function globalConfigPath(home: string): string {
  return join(home, GLOBAL_CONFIG_NAME);
}

/**
 * The resolved settings in the file's own spelling. One mapping, used by the writer, so the file
 * this installation creates is one it also reads — camelCase keys would be a file that describes
 * itself differently from every hand-edited copy.
 */
function fileShape(config: GlobalConfig): Record<string, unknown> {
  return {
    watch: config.watch,
    watch_interval: config.watchIntervalMs,
    telemetry: config.telemetry,
    daemon: { host: config.daemon.host, port: config.daemon.port },
    budgets: {
      default_tokens: config.budgets.defaultTokens,
      max_tokens: config.budgets.maxTokens,
    },
  };
}

/** The three settings a repository's own file narrows, taken from the machine-level values. */
export function repoDefaultsOf(config: GlobalConfig): RepoDefaults {
  return {
    watch: config.watch,
    watchIntervalMs: config.watchIntervalMs,
    telemetry: config.telemetry,
  };
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * Writes the defaults when the file is absent, and never touches one that exists — a hand-edited
 * file is the whole point of the file. A home the process may not write (a read-only container
 * image, a CI checkout) is one warning: the defaults still apply, so nothing that follows needs it.
 *
 * The temp file plus `rename` is the same publish the registry uses, so a reader never sees a
 * half-written file. The temp name carries the pid: two first runs racing on one `config.json.tmp`
 * could otherwise truncate each other's copy between the write and the rename. The loser's rename
 * fails and is recognised as "someone else created it", not as a failure.
 *
 * Creating it is silent: stderr is the diagnostic channel of a run, and a settings file that
 * appears with its documented defaults is not something the caller has to act on. A failure is a
 * warning, because the defaults still apply and the operator should know the file is not written.
 */
export function ensureGlobalConfig(home: string, options: { readonly log?: Logger } = {}): boolean {
  const logger = options.log ?? log;
  const path = globalConfigPath(home);
  if (existsSync(path)) {
    return false;
  }
  const temp = `${path}.${process.pid}.tmp`;
  try {
    mkdirSync(home, { recursive: true });
    writeFileSync(temp, `${JSON.stringify(fileShape(DEFAULT_GLOBAL_SETTINGS), null, 2)}\n`);
    renameSync(temp, path);
    return true;
  } catch (e) {
    try {
      rmSync(temp, { force: true });
    } catch {
      // A temp file that cannot be removed (an unwritable home) is not worth a second failure.
    }
    if (existsSync(path)) {
      // Another process won the race; the file it wrote is byte-identical to ours.
      return false;
    }
    logger.warn("global config not created, using the defaults", { path, reason: messageOf(e) });
    return false;
  }
}

/**
 * The machine settings, or `DEFAULT_GLOBAL_SETTINGS` when the file is absent, unreadable, not an
 * object, or carries a field of the wrong shape. An absent file is silent rather than a warning:
 * it is the normal state of a home that has not run the CLI yet, and the defaults are exactly what
 * a fresh installation means.
 */
export function readGlobalConfig(
  home: string,
  options: { readonly log?: Logger } = {},
): GlobalConfig {
  const logger = options.log ?? log;
  const path = globalConfigPath(home);
  const ignore = (reason: string): GlobalConfig => {
    logger.warn("global config ignored, using the defaults", { path, reason });
    return DEFAULT_GLOBAL_SETTINGS;
  };

  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (e) {
    const code = (e as { code?: unknown }).code;
    if (code === "ENOENT") {
      return DEFAULT_GLOBAL_SETTINGS;
    }
    return ignore(`unreadable: ${messageOf(e)}`);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    return ignore(`invalid JSON: ${messageOf(e)}`);
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return ignore("not a JSON object");
  }

  const file = parsed as GlobalConfigFile;
  const field = (name: string, value: unknown): unknown => {
    if (value === undefined) {
      return undefined;
    }
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      return value;
    }
    logger.warn("global config field ignored, using the default", {
      path,
      field: name,
      reason: "not a JSON object",
    });
    return undefined;
  };

  /**
   * A switch is a JSON boolean or the default. `"yes"` and `1` are typos, and guessing would let a
   * quoted `"false"` read as consent — the same rule the per-repo file follows. A key the operator
   * simply left out is not a typo: this file is written by hand, so an absent field is silent.
   */
  const flag = (name: string, value: unknown, absent: boolean): boolean => {
    if (typeof value === "boolean") {
      return value;
    }
    if (value !== undefined) {
      logger.warn("global config field ignored, using the default", {
        path,
        field: name,
        reason: "not a JSON boolean",
      });
    }
    return absent;
  };

  /** A positive integer, or the default; only a key that is present and wrong earns a warning. */
  const positiveInteger = (value: unknown, absent: number, name: string): number => {
    if (typeof value === "number" && Number.isInteger(value) && value > 0) {
      return value;
    }
    if (value !== undefined) {
      logger.warn("global config field ignored, using the default", {
        path,
        field: name,
        reason: "not a positive integer",
      });
    }
    return absent;
  };

  const defaults = DEFAULT_GLOBAL_SETTINGS;
  const daemon = field("daemon", file.daemon) as
    | { readonly host?: unknown; readonly port?: unknown }
    | undefined;
  const budgets = field("budgets", file.budgets) as
    | { readonly default_tokens?: unknown; readonly max_tokens?: unknown }
    | undefined;

  const port = daemon?.port;
  const daemonPort =
    typeof port === "number" && Number.isInteger(port) && port >= 0 && port <= 65_535
      ? port
      : defaults.daemon.port;
  if (port !== undefined && daemonPort !== port) {
    logger.warn("global config field ignored, using the default", {
      path,
      field: "daemon.port",
      reason: "not an integer in 0..65535",
    });
  }

  const host = daemon?.host;
  // The loopback policy belongs to the report server, which refuses a non-loopback address before
  // it binds; this file only insists on a string, so a typo costs one refused start that names the
  // allowed spellings rather than a config layer that quietly rewrote it.
  const daemonHost = typeof host === "string" && host.trim() !== "" ? host : defaults.daemon.host;
  if (host !== undefined && daemonHost !== host) {
    logger.warn("global config field ignored, using the default", {
      path,
      field: "daemon.host",
      reason: "not a non-empty string",
    });
  }

  // `budget.ts` refuses a call above `MAX_BUDGET_TOKENS` whatever this file says, so a ceiling
  // above it is unsatisfiable and is clamped rather than promised: the file can lower the hard
  // limit, never raise it.
  const maxTokens = Math.min(
    positiveInteger(budgets?.max_tokens, defaults.budgets.maxTokens, "budgets.max_tokens"),
    MAX_BUDGET_TOKENS,
  );
  // A per-call fallback above the ceiling is unsatisfiable, so it is clamped rather than honoured,
  // exactly as an out-of-band watch interval is.
  const defaultTokens = Math.min(
    positiveInteger(
      budgets?.default_tokens,
      defaults.budgets.defaultTokens,
      "budgets.default_tokens",
    ),
    maxTokens,
  );

  return {
    watch: flag("watch", file.watch, defaults.watch),
    watchIntervalMs: clampWatchInterval(file.watch_interval, defaults.watchIntervalMs),
    telemetry: flag("telemetry", file.telemetry, defaults.telemetry),
    daemon: { host: daemonHost, port: daemonPort },
    budgets: { defaultTokens, maxTokens },
  };
}
