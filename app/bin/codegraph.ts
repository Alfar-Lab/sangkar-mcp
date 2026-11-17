#!/usr/bin/env bun

// Module scope: the dispatch below uses top-level await.
export {};

const MODES = ["mcp", "index", "status", "worker"] as const;
type Mode = (typeof MODES)[number];

const USAGE = `usage: codegraph [${MODES.join("|")}]

  mcp     MCP server on stdio (default)
  index   index the repository
  status  report index status
  worker  index worker process, spawned by the server

stdout carries the MCP protocol; diagnostics go to stderr.
`;

function isMode(value: string | undefined): value is Mode {
  return value !== undefined && (MODES as readonly string[]).includes(value);
}

const argv = process.argv.slice(2);
const requested = argv[0] ?? "mcp";

if (!isMode(requested)) {
  console.error(`codegraph: unknown mode "${requested}"`);
  console.error(USAGE);
  process.exit(2);
}

// Imported lazily: an unknown mode must exit 2 without loading the kernel.
const { main } = await import("../src/index");

process.exit(await main(requested, argv));
