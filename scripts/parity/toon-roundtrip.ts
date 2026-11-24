#!/usr/bin/env bun
/**
 * Asks whether `serde_toon_format` 0.2.0 (TOON's v4.1 working draft) can render the envelopes that
 * `@toon-format/toon` 4.4.0 renders today (plan `rust-migration` TASK-010).
 *
 * The arbiter is the reference decoder, not the reference encoder: TOON leaves indentation,
 * quoting, delimiter and table-vs-list choices to the writer, so bytes are allowed to differ. What
 * is not allowed is a value that decodes to something else. Each case is therefore
 * `decode(spike(toon))` deep-compared against the JSON that went in, and the first structural
 * difference is named as a JSON pointer.
 *
 * The fifteen cases are printed one line each, in three groups, so the accounting is visible:
 *
 * - five real CLI envelopes, captured with `status|summary|trace --json` and committed verbatim
 *   under `rust/spikes/toon/fixtures/cli/` (`--capture` refreshes them from a temp data dir);
 * - five real tool envelopes copied verbatim from `docs/research/tool-outputs/`;
 * - five named shapes the CLI renderer must survive: uniform-object array (tabular), mixed-object
 *   array (list form), a nested object holding a 40-element array, nulls with empty arrays, and a
 *   string carrying a newline, a colon and a leading dash.
 *
 * Usage, from the repository root:
 *
 *     ~/.bun/bin/bun run scripts/parity/toon-roundtrip.ts
 *     ~/.bun/bin/bun run scripts/parity/toon-roundtrip.ts --capture
 *
 * Exits 0 when every case round-trips; exits 1 at the first mismatch with the failing pointer;
 * exits 2 when the spike cannot be built or a fixture cannot be read. The verdict itself is in
 * `rust/spikes/toon/DECISION.md`.
 */

import { decode } from "@toon-format/toon";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, relative, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "../..");
const SPIKE_DIR = join(ROOT, "rust/spikes/toon");
const MANIFEST = join(SPIKE_DIR, "Cargo.toml");
const BINARY = join(SPIKE_DIR, "target/debug/sangkar-spike-toon");
const CLI_FIXTURES = join(SPIKE_DIR, "fixtures/cli");
const TOOL_OUTPUTS = join(ROOT, "docs/research/tool-outputs");

type Json = null | boolean | number | string | Json[] | { [key: string]: Json };

interface CliCase {
  /** Fixture file name, and the label printed on the line. */
  readonly file: string;
  /** Arguments that follow the binary name; the global flags are appended by the capturer. */
  readonly args: readonly string[];
}

/**
 * The five captures, chosen for shape rather than convenience: `status` and `summary` carry the
 * large `not_indexed` tabular array, `trace` resolves a real symbol into mixed caller/callee
 * objects, `--depth 1` bounds it differently, and the miss case is the `not_found` envelope with
 * its own empty arrays.
 */
const CLI_CASES: readonly CliCase[] = [
  { file: "01-status.json", args: ["status", "--json"] },
  { file: "02-summary.json", args: ["summary", "--json"] },
  { file: "03-trace-chain.json", args: ["trace", "runTool", "--depth", "2", "--json"] },
  { file: "04-trace-depth1.json", args: ["trace", "runTool", "--depth", "1", "--json"] },
  { file: "05-trace-not-found.json", args: ["trace", "no-such-symbol-anywhere", "--depth", "1", "--json"] },
];

/** Copied verbatim; `index_status` is one newline-laden `_text` string, the rest are containers. */
const TOOL_OUTPUT_FILES: readonly string[] = [
  "list_graph_stats_tool.json",
  "get_minimal_context_tool.json",
  "get_hub_nodes_tool.json",
  "detect_changes_tool.json",
  "index_status.json",
];

interface Diff {
  readonly pointer: string;
  readonly got: string;
  readonly want: string;
}

const pointerOf = (base: string, key: string | number): string =>
  `${base}/${String(key).replaceAll("~", "~0").replaceAll("/", "~1")}`;

const describe = (value: unknown): string => {
  if (value === null) return "null";
  if (Array.isArray(value)) return `array(${value.length})`;
  return typeof value === "object" ? "object" : `${typeof value}(${JSON.stringify(value)})`;
};

/**
 * First structural difference, as a JSON pointer. Object key order is ignored on purpose: the
 * envelopes are compared as values, and `serde_json::Value` sorts keys while the TypeScript
 * encoder preserves insertion order — a rendering choice the parity bar allows.
 */
function firstDifference(got: unknown, want: unknown, pointer = ""): Diff | null {
  if (got === want) return null;
  if (got === null || want === null || typeof got !== typeof want) {
    return { pointer, got: describe(got), want: describe(want) };
  }
  if (Array.isArray(got) || Array.isArray(want)) {
    if (!Array.isArray(got) || !Array.isArray(want)) {
      return { pointer, got: describe(got), want: describe(want) };
    }
    if (got.length !== want.length) {
      return { pointer, got: `array(${got.length})`, want: `array(${want.length})` };
    }
    for (let index = 0; index < got.length; index += 1) {
      const diff = firstDifference(got[index], want[index], pointerOf(pointer, index));
      if (diff) return diff;
    }
    return null;
  }
  if (typeof got === "object" && typeof want === "object") {
    const gotKeys = Object.keys(got as Record<string, unknown>);
    const wantKeys = Object.keys(want as Record<string, unknown>);
    for (const key of wantKeys) {
      if (!Object.hasOwn(got as Record<string, unknown>, key)) {
        return { pointer: pointerOf(pointer, key), got: "missing", want: describe((want as Json)[key]) };
      }
    }
    for (const key of gotKeys) {
      if (!Object.hasOwn(want as Record<string, unknown>, key)) {
        return { pointer: pointerOf(pointer, key), got: describe((got as Json)[key]), want: "missing" };
      }
      const diff = firstDifference(
        (got as Record<string, unknown>)[key],
        (want as Record<string, unknown>)[key],
        pointerOf(pointer, key),
      );
      if (diff) return diff;
    }
    return null;
  }
  return { pointer, got: describe(got), want: describe(want) };
}

const shapeCases: ReadonlyArray<{ readonly label: string; readonly value: Json }> = [
  {
    // Pure tabular: every row carries the same scalar fields, which is what `not_indexed` and
    // `hub_nodes` look like before their nested members are added.
    label: "shape/uniform-objects-tabular",
    value: [
      { name: "GraphStore", kind: "Class", file: "code_review_graph/graph.py", fan_in: 0, fan_out: 42 },
      { name: "runTool", kind: "function", file: "src/index.ts", fan_in: 2, fan_out: 19 },
      { name: "_resolve_bare_endpoints", kind: "function", file: "parser/resolve.py", fan_in: 3, fan_out: 7 },
    ],
  },
  {
    // Keys differ per row and one row carries an array and a null, so no header can be shared.
    label: "shape/mixed-objects-list",
    value: [
      { id: 1, note: "first" },
      { id: 2, extra: true, tags: ["a", "b"] },
      { note: null },
    ],
  },
  {
    // A 40-entry scalar array nested two objects deep, beside the same length as a tabular array.
    label: "shape/nested-40-element-array",
    value: {
      generation: "g-b59b82",
      coverage: { indexed: 274, skipped: 5465, complete: false },
      latencies: Array.from({ length: 40 }, (_, index) => index * 7),
      units: Array.from({ length: 40 }, (_, index) => ({ file: `src/unit-${index}.rs`, nodes: index + 1 })),
    },
  },
  {
    label: "shape/nulls-and-empty-arrays",
    value: {
      status: "ok",
      stale_reason: null,
      not_indexed: [],
      nested: { inner: null, list: [], deep: { empty: [] } },
      pairs: [null, null],
      mixed: [null, 1, "", false],
    },
  },
  {
    label: "shape/newline-colon-leading-dash-string",
    value: {
      summary: "line one\n- must read: this",
      note: "- leading dash: colon",
      blank: "",
      tabbed: "a\tb: c\n- d",
    },
  },
];

/**
 * A comparator that cannot fail would make a green run meaningless, so the injected differences
 * below must each be located at the named pointer — including the two that exercise `~0`/`~1`
 * escaping — and reordered keys must compare equal.
 */
function selfCheck(): void {
  const pairs: ReadonlyArray<{ got: unknown; want: unknown; pointer: string }> = [
    { got: { a: 1 }, want: { a: 2 }, pointer: "/a" },
    { got: { a: { b: "x" } }, want: { a: { b: "y" } }, pointer: "/a/b" },
    { got: [1, 2], want: [1, 2, 3], pointer: "" },
    { got: [{ k: 1 }], want: [{ k: 2 }], pointer: "/0/k" },
    { got: { a: 1, b: 2 }, want: { a: 1 }, pointer: "/b" },
    { got: { a: 1 }, want: { a: 1, b: null }, pointer: "/b" },
    { got: { "a/b": 1 }, want: { "a/b": 2 }, pointer: "/a~1b" },
    { got: { "a~b": 1 }, want: { "a~b": 2 }, pointer: "/a~0b" },
    { got: null, want: [], pointer: "" },
  ];
  for (const pair of pairs) {
    const diff = firstDifference(pair.got, pair.want);
    if (!diff) fail(`toon-roundtrip: control missed a difference that should sit at ${pair.pointer || "<root>"}`);
    if (diff.pointer !== pair.pointer) {
      fail(`toon-roundtrip: control located a difference at ${diff.pointer || "<root>"}, expected ${pair.pointer || "<root>"}`);
    }
  }
  if (firstDifference({ b: 1, a: [2, { c: true }] }, { a: [2, { c: true }], b: 1 }) !== null) {
    fail("toon-roundtrip: control reported a difference between objects that differ only in key order");
  }
  process.stdout.write(
    `control: ${pairs.length}/${pairs.length} injected differences located by pointer, reordered keys equal\n`,
  );
}

const rel = (path: string): string => relative(ROOT, path);

function fail(message: string): never {
  process.stderr.write(`${message}\n`);
  process.exit(2);
}

function cargoCommand(): string {
  const onPath = Bun.which("cargo");
  if (onPath) return onPath;
  const rustupShim = join(homedir(), ".cargo/bin/cargo");
  if (existsSync(rustupShim)) return rustupShim;
  fail("toon-roundtrip: cargo not found on PATH or at ~/.cargo/bin/cargo");
}

/** The cargo/rustc shims live in one directory; `spawn` without it fails on `rustc` lookup. */
function toolchainEnv(): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (typeof value === "string") env[key] = value;
  }
  const cargoBin = dirname(cargoCommand());
  env.PATH = env.PATH ? `${cargoBin}:${env.PATH}` : cargoBin;
  return env;
}

function buildSpike(): void {
  const built = Bun.spawnSync([cargoCommand(), "build", "--quiet", "--manifest-path", MANIFEST], {
    cwd: ROOT,
    env: toolchainEnv(),
    stdout: "pipe",
    stderr: "pipe",
  });
  if (built.exitCode !== 0) {
    fail(`toon-roundtrip: cargo build failed\n${built.stderr.toString()}`);
  }
  if (!existsSync(BINARY)) fail(`toon-roundtrip: expected binary at ${rel(BINARY)} after build`);
}

function sangkar(args: readonly string[], dataDir: string): string {
  const run = Bun.spawnSync([process.execPath, "run", "bin/sangkar.ts", ...args, "--repo-root", ROOT, "--data-dir", dataDir], {
    cwd: ROOT,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (run.exitCode !== 0) {
    fail(`toon-roundtrip: \`sangkar ${args.join(" ")}\` exited ${run.exitCode}\n${run.stderr.toString()}`);
  }
  return run.stdout.toString().replace(/\n$/, "");
}

/**
 * Refreshes the five committed CLI envelopes. The index is built in a throwaway data dir, so this
 * never touches `~/.config/sangkar/` or the repository's own store; the repository is read only.
 */
function capture(): void {
  const dataDir = mkdtempSync(join(tmpdir(), "sangkar-toon-spike-"));
  process.stderr.write(`capturing into ${CLI_FIXTURES} (data dir ${dataDir})\n`);
  const index = Bun.spawnSync([process.execPath, "run", "bin/sangkar.ts", "index", "--repo-root", ROOT, "--data-dir", dataDir], {
    cwd: ROOT,
    stdout: "pipe",
    stderr: "pipe",
  });
  if (index.exitCode !== 0) {
    fail(`toon-roundtrip: \`sangkar index\` exited ${index.exitCode}\n${index.stderr.toString()}`);
  }
  mkdirSync(CLI_FIXTURES, { recursive: true });
  for (const testCase of CLI_CASES) {
    writeFileSync(join(CLI_FIXTURES, testCase.file), `${sangkar(testCase.args, dataDir)}\n`);
  }
}

/** Runs the spike over one JSON text and returns the TOON it wrote. */
function spike(jsonText: string, label: string): string {
  const run = Bun.spawnSync([BINARY], {
    cwd: ROOT,
    stdin: new TextEncoder().encode(jsonText),
    stdout: "pipe",
    stderr: "pipe",
  });
  if (run.exitCode !== 0) {
    fail(`toon-roundtrip: spike exited ${run.exitCode} on ${label}\n${run.stderr.toString()}`);
  }
  return run.stdout.toString();
}

interface Outcome {
  readonly status: "ok" | "fail";
  readonly detail: string;
}

function check(label: string, source: string, jsonText: string): Outcome {
  let input: Json;
  try {
    input = JSON.parse(jsonText) as Json;
  } catch (error) {
    fail(`toon-roundtrip: ${label} (${source}) is not valid JSON: ${String(error)}`);
  }
  const toon = spike(jsonText, label);
  let decoded: unknown;
  try {
    decoded = decode(toon);
  } catch (error) {
    return {
      status: "fail",
      detail: `${source}  decode() rejected the spike output: ${String(error)}\n  input:  ${JSON.stringify(input)}\n  toon:   ${JSON.stringify(toon)}`,
    };
  }
  const diff = firstDifference(decoded, input);
  if (diff) {
    return {
      status: "fail",
      detail: `${source}  differs at ${diff.pointer === "" ? "<root>" : diff.pointer}: got ${diff.got}, want ${diff.want}`,
    };
  }
  return { status: "ok", detail: `${source}  ${toon.length} B toon` };
}

function main(): void {
  selfCheck();
  if (process.argv.includes("--capture")) capture();
  buildSpike();

  const cases: Array<{ kind: string; label: string; source: string; jsonText: string }> = [];

  for (const testCase of CLI_CASES) {
    const path = join(CLI_FIXTURES, testCase.file);
    if (!existsSync(path)) fail(`toon-roundtrip: missing capture ${rel(path)}; run with --capture`);
    cases.push({
      kind: "cli",
      label: `cli/${testCase.file.replace(/\.json$/, "")}`,
      source: rel(path),
      jsonText: readFileSync(path, "utf8"),
    });
  }

  for (const file of TOOL_OUTPUT_FILES) {
    const path = join(TOOL_OUTPUTS, file);
    if (!existsSync(path)) fail(`toon-roundtrip: missing tool output ${rel(path)}`);
    cases.push({
      kind: "docs",
      label: `docs/${file.replace(/\.json$/, "")}`,
      source: rel(path),
      jsonText: readFileSync(path, "utf8"),
    });
  }

  for (const shape of shapeCases) {
    cases.push({
      kind: "shape",
      label: shape.label,
      source: "inline",
      jsonText: JSON.stringify(shape.value),
    });
  }

  process.stdout.write(
    `${cases.length} cases: ${CLI_CASES.length} cli captures + ${TOOL_OUTPUT_FILES.length} docs/research/tool-outputs + ${shapeCases.length} named shapes\n`,
  );

  for (const testCase of cases) {
    const outcome = check(testCase.label, testCase.source, testCase.jsonText);
    if (outcome.status === "fail") {
      process.stdout.write(`FAIL ${testCase.kind.padEnd(5)} ${testCase.label}\n     ${outcome.detail}\n`);
      process.exit(1);
    }
    process.stdout.write(`ok   ${testCase.kind.padEnd(5)} ${testCase.label.padEnd(42)} ${outcome.detail}\n`);
  }

  process.stdout.write(`${cases.length}/${cases.length} round-tripped through @toon-format/toon 4.4.0 decode; see rust/spikes/toon/DECISION.md\n`);
}

main();
