#!/usr/bin/env bun
/**
 * Ancillary evidence for TASK-010: the shapes the CLI renderer will meet that the ten real
 * envelopes do not happen to carry today.
 *
 * `scripts/parity/toon-roundtrip.ts` is the contract — fifteen cases, five of them named — and its
 * verdict is what `DECISION.md` rests on. This file is the caveat hunt beside it: magnitude and
 * precision edge cases for the number writer, the string values that look like something else,
 * quoting of keys, empty containers, and the tabular/list boundary. It only checks
 * `decode(spike(json))` against the input, and counts how often the bytes differ from
 * `@toon-format/toon`'s own `encode` — a difference is allowed, a different value is not.
 *
 * Usage, from the repository root:
 *
 *     ~/.bun/bin/bun run rust/spikes/toon/probes/edge-cases.ts
 *
 * Exits 0 when every case round-trips, 1 at the first case whose value changed, 2 when the spike
 * binary is missing (build it first with `cargo build --manifest-path rust/spikes/toon/Cargo.toml`).
 */

import { decode, encode } from "@toon-format/toon";
import { existsSync, readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "../../../..");
const BINARY = join(ROOT, "rust/spikes/toon/target/debug/sangkar-spike-toon");

if (!existsSync(BINARY)) {
  process.stderr.write(
    `edge-cases: ${BINARY} is missing; build it with \`cargo build --manifest-path rust/spikes/toon/Cargo.toml\`\n`,
  );
  process.exit(2);
}

const text = (value: string): Record<string, unknown> => ({ v: value });

/** A value whose encoding must not change its meaning. Labels are the finding when one breaks. */
const CASES: ReadonlyArray<readonly [string, unknown]> = [
  // Numbers the TS encoder writes in exponent form and Rust's f64 Display writes positionally.
  ["number/1e300", { v: 1e300 }],
  ["number/1e-300", { v: 1e-300 }],
  ["number/-1e-7", { v: -1e-7 }],
  ["number/float-precision", { a: 1.1, b: 2.2, c: 1.1 + 2.2 }],
  ["number/integers", { zero: 0, neg: -1, big: 9007199254740992, negBig: -9007199254740993 }],
  // Strings that could be mistaken for a literal if the writer forgot to quote them.
  ["string/numeric", text("42")],
  ["string/leading-zero", text("007")],
  ["string/exponent", text("1e5")],
  ["string/hex", text("0x10")],
  ["string/keyword", text("true")],
  ["string/null-word", text("null")],
  ["string/nan-word", text("NaN")],
  ["string/version", text("1.0.0")],
  ["string/date", text("2026-11-24T04:58:26.066Z")],
  ["string/dash", text("-")],
  ["string/dash-number", text("-0")],
  ["string/colon", text(":")],
  ["string/colon-tail", text("a:")],
  ["string/leading-bracket", text("[1]")],
  ["string/leading-brace", text("{a}")],
  ["string/hash", text("# x")],
  ["string/quote", text('x"y')],
  ["string/trailing-backslash", text("a\\")],
  ["string/tab-and-colon", text("\ta: b")],
  ["string/crlf", text("a\r\nb")],
  ["string/vtab", text("a\u000bb")],
  ["string/nul", text("a\u0000b")],
  ["string/line-separator", text("a\u2028b")],
  ["string/empty", text("")],
  ["string/newline-only", text("\n")],
  ["string/padding", { left: "  pad", right: "pad  ", mid: "a  b" }],
  ["string/unicode", { emoji: "🎯", combining: "e\u0301", cjk: "日本語" }],
  // Keys are quoted by a different rule than values.
  ["keys/needing-quotes", { "a b": 1, "a:b": 2, "-a": 3, "a,b": 4, "": 5, 'x"y': 6, "[a]": 7 }],
  ["keys/numeric", { "0": 1, "1": 2 }],
  ["keys/long", { ["k".repeat(500)]: 1 }],
  // Containers at and across the tabular/list boundary.
  ["containers/empty-object", {}],
  ["containers/empty-array", []],
  ["containers/root-scalars", ["hello", 42, null]],
  ["containers/nested-empty-object", { o: {}, p: { q: {} } }],
  ["containers/array-of-arrays", [[1, 2], [3, 4]]],
  ["containers/one-row-table", [{ a: 1, b: 2 }]],
  ["containers/ragged-rows", [{ a: 1, b: 2 }, { a: 3 }]],
  ["containers/null-cells", [{ a: 1, b: null }, { a: null, b: 2 }]],
  ["containers/nested-object-cell", [{ id: 1, deg: { in: 2, out: 3 } }, { id: 2, deg: { in: 0, out: 1 } }]],
  ["containers/nested-array-cell", [{ id: 1, lines: [1, 2] }, { id: 2, lines: [3, 4] }]],
  ["containers/empty-objects", [{}, {}]],
  ["containers/all-null", [null, null]],
  ["containers/mixed-null", [null, [], {}, "", 0, false]],
  ["containers/deep-20", (() => { let value: unknown = { leaf: 1 }; for (let i = 0; i < 20; i += 1) value = { n: value }; return value; })()],
  ["containers/40-tabular-rows", { rows: Array.from({ length: 40 }, (_, i) => ({ i, name: `n${i}`, flag: i % 2 === 0 })) }],
  ["containers/40-nested-arrays", { rows: Array.from({ length: 40 }, (_, i) => [i, i + 1, i + 2]) }],
  ["containers/1000-rows", Array.from({ length: 1000 }, (_, i) => ({ i, file: `src/unit-${i}.rs` }))],
  ["containers/long-string", { s: "x".repeat(2000) }],
];

const LARGE_ENVELOPES: readonly string[] = [
  "docs/research/tool-outputs/get_affected_flows_tool.json",
  "docs/research/tool-outputs/get_impact_radius_tool.json",
  "docs/research/tool-outputs/get_review_context_tool.json",
  "docs/research/tool-outputs/tools_list_crg.json",
  "docs/research/tool-outputs/tools_list_cbm.json",
];

/** Key order is rendering, not value, so it is sorted away before the equality test. */
function normalise(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(normalise);
  if (value && typeof value === "object") {
    const source = value as Record<string, unknown>;
    return Object.fromEntries(Object.keys(source).sort().map((key) => [key, normalise(source[key])]));
  }
  return value;
}

const sameValue = (got: unknown, want: unknown): boolean =>
  JSON.stringify(normalise(got)) === JSON.stringify(normalise(want));

function spike(jsonText: string): string {
  const run = Bun.spawnSync([BINARY], { stdin: new TextEncoder().encode(jsonText), stdout: "pipe", stderr: "pipe" });
  if (run.exitCode !== 0) {
    process.stderr.write(`edge-cases: spike exited ${run.exitCode}\n${run.stderr.toString()}\n`);
    process.exit(2);
  }
  return run.stdout.toString();
}

let failures = 0;
let byteDiffs = 0;
let total = 0;

for (const [label, want] of CASES) {
  total += 1;
  const toon = spike(JSON.stringify(want));
  let got: unknown;
  try {
    got = decode(toon);
  } catch (error) {
    failures += 1;
    process.stdout.write(`FAIL ${label}  decode rejected: ${String(error)}\n  toon: ${JSON.stringify(toon)}\n`);
    continue;
  }
  if (!sameValue(got, want)) {
    failures += 1;
    process.stdout.write(`FAIL ${label}\n  in  : ${JSON.stringify(want)}\n  toon: ${JSON.stringify(toon)}\n  back: ${JSON.stringify(got)}\n`);
    continue;
  }
  const identical = toon === encode(want);
  if (!identical) byteDiffs += 1;
  process.stdout.write(`ok   ${label}${identical ? "" : "  (bytes differ)"}\n`);
}

for (const path of LARGE_ENVELOPES) {
  total += 1;
  const jsonText = readFileSync(join(ROOT, path), "utf8");
  const started = performance.now();
  const toon = spike(jsonText);
  const elapsed = Math.round(performance.now() - started);
  let got: unknown;
  try {
    got = decode(toon);
  } catch (error) {
    failures += 1;
    process.stdout.write(`FAIL large/${path}  decode rejected: ${String(error)}\n`);
    continue;
  }
  if (!sameValue(got, JSON.parse(jsonText))) {
    failures += 1;
    process.stdout.write(`FAIL large/${path}  value changed\n`);
    continue;
  }
  process.stdout.write(
    `ok   large/${path.split("/").at(-1)}  ${(jsonText.length / 1024).toFixed(0)}K json -> ${(toon.length / 1024).toFixed(0)}K toon in ${elapsed}ms\n`,
  );
}

process.stdout.write(`${total - failures}/${total} edge cases round-tripped; ${byteDiffs}/${total} differ in bytes from @toon-format/toon encode\n`);
process.exit(failures === 0 ? 0 : 1);
