#!/usr/bin/env bun

import { guardRuntime, readRuntime } from "../src/shared/runtime";

/**
 * The runtime preflight, first statement of the process: nothing that opens a database has been
 * loaded yet, so a refusal costs no connection and no store. The one case it cannot catch is a
 * missing `bun` binary — the shebang fails before this file is parsed, so that error is the
 * shell's, not ours, and no line here can report it.
 */
const refusal = guardRuntime(readRuntime());

// Checked before `--help`: an interpreter that cannot run the product cannot print its help either.
if (refusal !== null) {
  process.exit(refusal);
}

/**
 * The verbs this process owns; every other first token is a tool name. `index` is both on purpose:
 * the verb is the bootstrap (a fresh tree has no store for a tool call to answer from) and the tool
 * is what a client calls, with identical arguments.
 */
const RESERVED = ["mcp", "worker", "index", "telemetry", "daemon", "repo"] as const;
type Reserved = (typeof RESERVED)[number];

const USAGE = `sangkar <name> [arguments] [global flags] — the name comes first.

verbs
  mcp     MCP server on stdio (default): the tool surface a client talks to
  worker  index worker process, spawned by the server
  index   index the repository; also a tool, with identical arguments
  telemetry  write a self-contained HTML report of the recorded usage (local only; recording is on by default)
  daemon  run, start, stop or inspect the daemon that watches the registered repositories
  repo    add, remove, list or configure the repositories this installation serves

tools
  status   report index status as a TOON envelope (JSON with --json)
  summary  summarize the current situation in a few hundred tokens
  trace    follow a call chain from one symbol
  search   find symbols or text in the indexed code
  impact   list what a change set can reach
  expand   fetch more detail for one item from a previous result
  index    index the repository (the verb above)

global flags (either side of the name: the name is the first token that is not one of them)
  --repo-root <path>  the tree to index and serve
  --data-dir <path>   where the graph is stored
  --json              print JSON instead of the default TOON envelope

examples
  sangkar trace 'handleRequest' --depth 1
  sangkar mcp --repo-root /path/to/repo
  sangkar telemetry --since 7d
  sangkar repo add .
  sangkar daemon status

sangkar index is how the first index is created; until then every tool answers bootstrapping.
`;

const WORKER_FLAGS = "[--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>]";

/**
 * `--yes` belongs to the verb, not to the worker: it answers the question the verb asks before it
 * spawns one, when the tree was autodetected above the working directory and stdin is a terminal.
 */
const INDEX_EXTRA =
  "[--yes]\n  --yes          index the autodetected tree without asking (the answer a script gives)";

/** One verb's own flags; `--help` on a verb prints this and exits without running anything. */
const VERB_USAGE: Readonly<Record<Reserved, string>> = {
  mcp: "usage: sangkar mcp [--repo-root <path>] [--data-dir <path>] [--json]",
  worker: `usage: sangkar worker ${WORKER_FLAGS}`,
  index: `usage: sangkar index ${WORKER_FLAGS} ${INDEX_EXTRA}`,
  // The three flags `--serve` added to the telemetry verb; the block a refusal prints lives in
  // `src/shared/telemetry/cli.ts`, and these lines are the answer `--help` gives from here.
  telemetry: `usage: sangkar telemetry [--out <path>] [--serve] [--port <n>] [--host <addr>] [--since <YYYY-MM-DD|Nd>] [--repo-root <path>] [--data-dir <path>]
  --serve        serve the report for every registered repository on loopback instead of writing one file
  --port <n>     the port to bind (default: config.json's daemon.port, else 7317; 0 picks a free port)
  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)`,
  // Held verbatim in `src/daemon/cli.ts` and `src/shared/cli/repo.ts`, which print it on a refusal;
  // a test compares the two so `--help` and a refusal cannot drift apart.
  daemon: `usage: sangkar daemon [run|start|stop|status] [--port <n>] [--host <addr>]
  run     run the daemon in the foreground until a signal stops it
  start   start it as a detached child and return once it holds the pid file
  stop    stop the daemon this home's pid file names
  status  one header line, then one line per registered repository
  --port <n>     the report server's port (default: config.json's daemon.port, else 7317; 0 picks a free port)
  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)
  --repo-root, --data-dir and --json are accepted by every subcommand and change nothing here
  --port and --host belong to "run"; start passes neither, so it binds the same defaults`,
  repo: `usage: sangkar repo [add|remove|list|config] <path>
  add <path>                register a tree and print its canonical path
  remove <path>             unregister a tree; the tree and its graph are left alone
  list                      one registered path per line (--json for a JSON document)
  config <path>             write <path>/.sangkar/config.json and print the effective settings
  --json                    the registry as {"version":1,"repos":[...]} (list only)
  --watch <true|false>      whether the daemon watches this tree (config only)
  --watch-interval <n>      poll period in milliseconds, 1000..300000 (config only)
  --telemetry <true|false>  whether this tree's usage may be recorded (config only)
  --repo-root, --data-dir and --json are accepted by every subcommand; only list changes with --json
  the <path> may follow the flags, and no token starting with -- is ever read as a path
  a relative <path> resolves against the current directory; a leading ~/ is expanded`,
};

function isReserved(value: string): value is Reserved {
  return (RESERVED as readonly string[]).includes(value);
}

const argv = process.argv.slice(2);
// The token that names the verb, found with the global flags already set aside: `sangkar --json
// status` and `sangkar status --json` must name the same command. Imported here rather than at the
// top so the runtime preflight above stays the first thing this process does.
const { resolveCommand } = await import("../src/shared/cli/args");
const { name, rest, help } = resolveCommand(argv);
const requested = name ?? "mcp";

// The whole line minus the command's own name. `rest` is always a suffix of `argv`, so the name sits
// at `argv.length - rest.length - 1`; handing the verbs this line rather than the tail is what makes
// a global's position irrelevant to every command, not only to the tools.
const verbArgv: readonly string[] =
  name === null ? rest : [...argv.slice(0, argv.length - rest.length - 1), ...rest];

// `sangkar --help` names no command, so the whole surface is the answer; `sangkar help` is the same
// request spelled with a word.
if ((help && name === null) || requested === "help") {
  process.stdout.write(USAGE);
  process.exit(0);
}

// Answered before anything runs: `sangkar index --help` must not start an index run.
if (isReserved(requested) && help) {
  process.stdout.write(`${VERB_USAGE[requested]}\n`);
  process.exit(0);
}

// The machine-level settings file is created before any verb reads it: a home without one is a
// fresh installation, not a broken one, and an operator who wants to change a default should find
// the file already there. The `--help` answers above exit first, so asking for usage never writes
// to the config directory, and a home this process may not write is one warning and the defaults.
// Just the home, not a whole `AppConfig`: loading the config here would read the file again and
// repeat its warnings once per verb, and the verb below resolves the config for its own work.
const { resolveHome } = await import("../src/shared/config");
const { ensureGlobalConfig } = await import("../src/shared/global-config");
ensureGlobalConfig(resolveHome(process.env));

// The report needs the telemetry directory and nothing else — no graph, no store, no kernel — so it
// dispatches here rather than through `main`, which is the only other place a verb could go. Each
// verb below receives the line without its own name, global flags still in it: `telemetry` reads
// `--repo-root`/`--data-dir` itself, and the daemon and repo verbs re-split the rest.
if (requested === "telemetry") {
  const { runTelemetry } = await import("../src/shared/telemetry/cli");
  process.exit(await runTelemetry(verbArgv));
}

// Same reason, one layer out: the registry is a JSON file and the daemon owns its own process, so
// neither verb has a graph to load. Both dispatch here, before the kernel below.
if (requested === "daemon") {
  const { runDaemonCli } = await import("../src/daemon/cli");
  process.exit(await runDaemonCli(verbArgv));
}

if (requested === "repo") {
  const { runRepoCli } = await import("../src/shared/cli/repo");
  process.exit(await runRepoCli(verbArgv));
}

// Imported lazily: `--help` and an unknown name must exit without loading the kernel.
const { main } = await import("../src/index");

const code = await main(requested, argv);

// 2 is the usage exit code for a tool name that was called wrongly, and `main` already wrote the
// reason. A reserved verb owns its own meaning of 2 — an index run with nothing to do reports
// `WORKER_EXIT.nothingToDo` — so printing the banner for it would tell an operator their command
// was malformed when the run actually succeeded.
if (code === 2 && !isReserved(requested)) {
  process.stderr.write(USAGE);
}

process.exit(code);
