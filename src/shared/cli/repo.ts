/**
 * `sangkar repo`: the registry (`<home>/sangkar.json`) and one repo's settings
 * (`<repo>/.sangkar/config.json`) on the command line.
 *
 * Two answers live here, and the split is the design:
 *
 * - `add`, `remove` and `list` are the registry's writers and its reader, in the same canonical
 *   spelling `#shared/registry` uses — so the path a person types and the path the daemon polls on
 *   its next tick are one string. Every path resolves against the current directory, because a
 *   shell's relative path is the caller's intent rather than a string to guess at.
 * - `config` is the only place a settings file is written from the command line. It reads the
 *   current file first, overrides exactly the flags it was given, and prints what a re-read returns
 *   — the effective values the daemon will act on, not the text that was typed.
 *
 * A subcommand's path may sit anywhere among its own flags, so the path is the first token that is
 * not a flag. Reading `rest[0]` instead made `repo config --watch false .` write the settings of a
 * directory called `--watch`, answer the same three lines, and exit 0 — a silent wrong target, which
 * is the one failure a settings verb cannot afford.
 *
 * stdout carries the answer, one line per result, which is why this module writes there at all;
 * a refusal writes the usage block, and every diagnostic goes through `#shared/log` on stderr.
 *
 * The four global flags are accepted next to every subcommand. `--json` changes only `list`'s
 * answer; every one of them is otherwise inert here, because the registry lives in the machine home
 * rather than in a tree — a flag that would mean nothing is accepted and ignored, never refused.
 */
import { resolve } from "node:path";
import { CliUsageError, splitGlobals } from "#shared/cli/args";
import { EXIT_FAILURE, EXIT_USAGE } from "#shared/cli/render";
import { flagValue, loadConfig } from "#shared/config";
import { log } from "#shared/log";
import {
  canonicalRoot,
  REGISTRY_VERSION,
  readRegistry,
  registerRepo,
  removeRepo,
} from "#shared/registry";
import {
  readRepoConfig,
  WATCH_INTERVAL_MAX_MS,
  WATCH_INTERVAL_MIN_MS,
  writeRepoConfig,
} from "#shared/repo-config";

/**
 * Printed after every refusal. The same text is `VERB_USAGE.repo` in `bin/sangkar.ts`, which
 * answers `--help` before dispatch; a test pins the two together.
 */
const USAGE = `usage: sangkar repo [add|remove|list|config] <path>
  add <path>                register a tree and print its canonical path
  remove <path>             unregister a tree; the tree and its graph are left alone
  list                      one registered path per line (--json for a JSON document)
  config <path>             write <path>/.sangkar/config.json and print the effective settings
  --json                    the registry as {"version":1,"repos":[...]} (list only)
  --watch <true|false>      whether the daemon watches this tree (config only)
  --watch-interval <n>      poll period in milliseconds, ${WATCH_INTERVAL_MIN_MS}..${WATCH_INTERVAL_MAX_MS} (config only)
  --telemetry <true|false>  whether this tree's usage may be recorded (config only)
  --repo-root, --data-dir and --json are accepted by every subcommand; only list changes with --json
  the <path> may follow the flags, and no token starting with -- is ever read as a path
  a relative <path> resolves against the current directory; a leading ~/ is expanded`;

/** One flag's value: the boolean or interval it spells, `undefined` when absent, or the refusal. */
type FlagRead<T> =
  | { readonly ok: true; readonly value: T | undefined }
  | { readonly ok: false; readonly reason: string };

export async function runRepoCli(argv: readonly string[]): Promise<number> {
  if (argv.includes("--help") || argv.includes("-h")) {
    process.stdout.write(`${USAGE}\n`);
    return 0;
  }

  // The global flags leave the argv before any subcommand reads it, which is what makes `repo add .
  // --json` an acceptance rather than a stray flag. A value flag with nothing to consume is refused
  // here with this verb's own block, not the tool one `splitGlobals` was written for; `loadConfig`
  // below still receives the raw argv, so the two value flags keep whatever effect they had.
  let globals: ReturnType<typeof splitGlobals>;
  try {
    globals = splitGlobals(argv);
  } catch (error) {
    if (error instanceof CliUsageError) {
      process.stderr.write(`${error.message}\n${USAGE}\n`);
      return EXIT_USAGE;
    }
    throw error;
  }

  // `home` and `registryPath` come from `#shared/config.ts`, the only reader of the environment, so
  // the machine home decides the file here exactly as it does for the daemon.
  const config = loadConfig({ argv });
  const [verb, ...rest] = globals.rest;

  switch (verb) {
    case "add": {
      const stray = unknownFlag(rest, []);
      if (stray !== null) {
        return refuse(`"repo add" takes no ${stray}`);
      }
      const raw = positional(rest, []);
      if (raw === undefined || raw === "") {
        return refuse("repo add needs a path");
      }
      const root = canonical(raw);
      const result = registerRepo(config.registryPath, root, { log });
      if (result.added) {
        process.stdout.write(`registered ${root}\n`);
        return 0;
      }
      // `registerRepo` swallows a failed write and answers `added: false` for it, so the registry
      // it hands back is what separates "was already there" from "could not be written".
      if (result.registry.repos.includes(root)) {
        process.stdout.write(`already registered ${root}\n`);
        return 0;
      }
      return fail(`could not write ${config.registryPath}`);
    }

    case "remove": {
      const stray = unknownFlag(rest, []);
      if (stray !== null) {
        return refuse(`"repo remove" takes no ${stray}`);
      }
      const raw = positional(rest, []);
      if (raw === undefined || raw === "") {
        return refuse("repo remove needs a path");
      }
      const root = canonical(raw);
      const result = removeRepo(config.registryPath, root, { log });
      if (result.removed) {
        process.stdout.write(`removed ${root}\n`);
        return 0;
      }
      // Same split as `add`: still listed means the write failed, absent means it was never there.
      if (result.registry.repos.includes(root)) {
        return fail(`could not write ${config.registryPath}`);
      }
      process.stdout.write(`not registered ${root}\n`);
      return EXIT_FAILURE;
    }

    case "list": {
      const stray = unknownFlag(rest, []);
      if (stray !== null) {
        return refuse(`"repo list" takes no ${stray}`);
      }
      const registry = readRegistry(config.registryPath, { log });
      // `--json` is stripped from `rest` by `splitGlobals`, so the switch is the only record of it.
      if (globals.json) {
        const document = { version: REGISTRY_VERSION, repos: registry.repos };
        process.stdout.write(`${JSON.stringify(document)}\n`);
        return 0;
      }
      // Nothing to say about an empty registry is the whole answer: one path per line, no header.
      process.stdout.write(registry.repos.map((repo) => `${repo}\n`).join(""));
      return 0;
    }

    case "config": {
      const stray = unknownFlag(rest, ["watch", "watch-interval", "telemetry"]);
      if (stray !== null) {
        return refuse(`"repo config" takes no ${stray}`);
      }
      // The flags are read before the path, so a malformed flag answers for itself rather than as a
      // missing path: `repo config --watch` is a broken flag, not a path someone forgot to type.
      const watch = boolFlag(rest, "watch");
      if (!watch.ok) {
        return refuse(watch.reason);
      }
      const telemetry = boolFlag(rest, "telemetry");
      if (!telemetry.ok) {
        return refuse(telemetry.reason);
      }
      const interval = intervalFlag(rest);
      if (!interval.ok) {
        return refuse(interval.reason);
      }
      const raw = positional(rest, ["watch", "watch-interval", "telemetry"]);
      if (raw === undefined || raw === "") {
        return refuse("repo config needs a path");
      }

      const root = canonical(raw);
      const current = readRepoConfig(root, { defaults: config.repoDefaults, log });
      try {
        writeRepoConfig({
          ...current,
          watch: watch.value ?? current.watch,
          watchIntervalMs: interval.value ?? current.watchIntervalMs,
          telemetry: telemetry.value ?? current.telemetry,
        });
      } catch (error) {
        return fail(`could not write the settings for ${root}: ${messageOf(error)}`);
      }

      // Re-read rather than echo the arguments: the file is the contract, and a value the reader
      // clamps is reported as the daemon will use it.
      const effective = readRepoConfig(root, { defaults: config.repoDefaults, log });
      const lines = [
        `watch=${onOff(effective.watch)}`,
        `interval=${effective.watchIntervalMs}ms`,
        `telemetry=${onOff(effective.telemetry)}`,
      ];
      process.stdout.write(`${lines.join("\n")}\n`);
      return 0;
    }

    case undefined:
      return refuse("repo needs a subcommand");

    default:
      return refuse(`unknown subcommand "${verb}"`);
  }
}

/**
 * `~/` is left to `canonicalRoot`, which is the one place the expansion rule lives; every other
 * spelling is the caller's and resolves against the working directory they ran the verb in.
 */
function canonical(path: string): string {
  return canonicalRoot(path.startsWith("~/") ? path : resolve(process.cwd(), path));
}

/**
 * The subcommand's path: the first token that is not a flag, with each flag's value skipped so
 * `--watch false` consumes `false` and not the path after it. A token beginning with `--` is never a
 * path — the `--` spelling belongs to flags, and accepting one as a directory is how the settings for
 * `--watch` were written into a directory named `--watch` while the answer looked right.
 *
 * Only the *first* occurrence of a value flag takes the next token: `flagValue` answers the first
 * one, so a repeated flag's value is an ordinary token and `repo config --watch false --watch .`
 * still names `.` instead of losing the path.
 */
function positional(argv: readonly string[], valueFlags: readonly string[]): string | undefined {
  const consumed = new Set<string>();
  for (let i = 0; i < argv.length; i += 1) {
    const token = argv[i] ?? "";
    if (!token.startsWith("--")) {
      return token;
    }
    // `--flag=value` carries its own value; the spaced form takes the next token with it.
    const name = token.slice(2).split("=")[0] ?? "";
    if (!valueFlags.includes(name) || consumed.has(name)) {
      continue;
    }
    consumed.add(name);
    if (token.includes("=")) {
      continue;
    }
    i += 1;
  }
  return undefined;
}

/**
 * The first flag a subcommand does not read, or `null`. A flag's value is a bare token and is
 * skipped, so `--watch false` is one flag and not an unknown one.
 */
function unknownFlag(argv: readonly string[], allowed: readonly string[]): string | null {
  for (const token of argv) {
    if (!token.startsWith("--")) {
      continue;
    }
    const name = token.slice(2).split("=")[0] ?? "";
    if (!allowed.includes(name)) {
      return token;
    }
  }
  return null;
}

/**
 * `--name true|false`, and the two spellings only: a shell has no boolean type, so `1`, `yes` and
 * `"false"` would each be a guess — and guessing wrong here writes a file that says the opposite.
 */
function boolFlag(argv: readonly string[], name: string): FlagRead<boolean> {
  const raw = flagValue(argv, name);
  if (raw === undefined) {
    return argv.includes(`--${name}`)
      ? { ok: false, reason: `--${name} needs true or false` }
      : { ok: true, value: undefined };
  }
  if (raw === "true" || raw === "false") {
    return { ok: true, value: raw === "true" };
  }
  return { ok: false, reason: `--${name} must be true or false, not "${raw}"` };
}

/**
 * A whole number of milliseconds inside the band `readRepoConfig` supports. An out-of-band value is
 * refused rather than written and clamped: the file said one thing and the daemon did another,
 * which is the kind of silent divergence a settings file exists to prevent.
 */
function intervalFlag(argv: readonly string[]): FlagRead<number> {
  const raw = flagValue(argv, "watch-interval");
  if (raw === undefined) {
    return argv.includes("--watch-interval")
      ? { ok: false, reason: "--watch-interval needs a value" }
      : { ok: true, value: undefined };
  }
  if (!/^[0-9]+$/.test(raw)) {
    return { ok: false, reason: `--watch-interval must be a whole number, not "${raw}"` };
  }
  const ms = Number(raw);
  if (ms < WATCH_INTERVAL_MIN_MS || ms > WATCH_INTERVAL_MAX_MS) {
    return {
      ok: false,
      reason: `--watch-interval must be ${WATCH_INTERVAL_MIN_MS}..${WATCH_INTERVAL_MAX_MS} ms, not ${ms}`,
    };
  }
  return { ok: true, value: ms };
}

function onOff(value: boolean): string {
  return value ? "on" : "off";
}

/** The reason first, then the block, so the last thing a caller sees is how to call it right. */
function refuse(reason: string): number {
  process.stderr.write(`${reason}\n${USAGE}\n`);
  return EXIT_USAGE;
}

/** A failure with no usage to offer: the verb was called correctly and the filesystem refused. */
function fail(reason: string): number {
  process.stderr.write(`sangkar repo: ${reason}\n`);
  return EXIT_FAILURE;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
