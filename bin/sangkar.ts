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
const RESERVED = ["mcp", "worker", "index", "telemetry"] as const;
type Reserved = (typeof RESERVED)[number];

const USAGE = `sangkar <name> [arguments] [global flags] — the name comes first.

verbs
  mcp     MCP server on stdio (default): the tool surface a client talks to
  worker  index worker process, spawned by the server
  index   index the repository; also a tool, with identical arguments
  telemetry  write a self-contained HTML report of the recorded usage (local only; recording is on by default)

tools
  status   report index status: five human lines, or the envelope with --json
  summary  summarize the current situation in a few hundred tokens
  trace    follow a call chain from one symbol
  search   find symbols or text in the indexed code
  impact   list what a change set can reach
  expand   fetch more detail for one item from a previous result
  index    index the repository (the verb above)

global flags (on either side of the name)
  --repo-root <path>  the tree to index and serve
  --data-dir <path>   where the graph is stored
  --json              print the envelope instead of human lines

examples
  sangkar trace 'handleRequest' --depth 1
  sangkar mcp --repo-root /path/to/repo
  sangkar telemetry --since 7d

sangkar index is how the first index is created; until then every tool answers bootstrapping.
`;

const WORKER_FLAGS = "[--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>]";

/** One verb's own flags; `--help` on a verb prints this and exits without running anything. */
const VERB_USAGE: Readonly<Record<Reserved, string>> = {
  mcp: "usage: sangkar mcp [--repo-root <path>] [--data-dir <path>]",
  worker: `usage: sangkar worker ${WORKER_FLAGS}`,
  index: `usage: sangkar index ${WORKER_FLAGS}`,
  telemetry:
    "usage: sangkar telemetry [--out <path>] [--since <YYYY-MM-DD|Nd>] [--repo-root <path>] [--data-dir <path>]",
};

function isReserved(value: string): value is Reserved {
  return (RESERVED as readonly string[]).includes(value);
}

const argv = process.argv.slice(2);
const requested = argv[0] ?? "mcp";

if (requested === "--help" || requested === "-h" || requested === "help") {
  process.stdout.write(USAGE);
  process.exit(0);
}

// Answered before anything runs: `sangkar index --help` must not start an index run.
if (isReserved(requested) && (argv.includes("--help") || argv.includes("-h"))) {
  process.stdout.write(`${VERB_USAGE[requested]}\n`);
  process.exit(0);
}

// The report needs the telemetry directory and nothing else — no graph, no store, no kernel — so it
// dispatches here rather than through `main`, which is the only other place a verb could go.
if (requested === "telemetry") {
  const { runTelemetry } = await import("../src/shared/telemetry/cli");
  process.exit(await runTelemetry(argv.slice(1)));
}

// Imported lazily: `--help` and an unknown name must exit without loading the kernel.
const { main } = await import("../src/index");

const code = await main(requested, argv);

// 2 is the usage exit code: `main` already wrote the reason, and the banner belongs beside it.
if (code === 2) {
  process.stderr.write(USAGE);
}

process.exit(code);
