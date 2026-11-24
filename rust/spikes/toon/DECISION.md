# DECISION — the CLI renderer depends on `serde_toon_format` 0.2.0; nothing is vendored

**Verdict: `serde_toon_format` 0.2.0's default encoder renders every envelope the CLI renders today
— all fifteen contract cases decode back to the input value through `@toon-format/toon` 4.4.0, and
the ten real envelopes come out byte-identical to it — so TASK-026 should take the crate and must
not vendor or re-port the TypeScript encoder.**

No case failed, so there is no failing pointer to reduce and the vendoring fallback stays unbuilt.
The spike is kept as the evidence, per the task.

## What was built and run

| Path | What it is |
|---|---|
| `rust/spikes/toon/` | Standalone package (`[workspace]` in its `Cargo.toml`, own target dir, `serde_toon_format = "0.2"` + `serde_json = "1"`). `src/main.rs` reads one JSON value on stdin and writes TOON on stdout; empty stdin is answered with empty output and exit 0. |
| `scripts/parity/toon-roundtrip.ts` | The oracle: fifteen cases through the spike, decoded with the reference decoder, deep-compared to the JSON that went in. Prints one line per case and the JSON pointer of the first difference; a 9/9 self-check proves the comparator can fail. |
| `rust/spikes/toon/fixtures/cli/*.json` | The five CLI envelopes, verbatim: `status`, `summary`, `trace runTool --depth 2`, `trace runTool --depth 1`, `trace <miss>` — every one captured with `--json` and a throwaway `--data-dir`. |
| `rust/spikes/toon/probes/edge-cases.ts` | Ancillary caveat hunt, not part of the contract: 53 hand-written shapes plus the five largest `docs/research/tool-outputs/` envelopes. |

```sh
~/.bun/bin/bun run scripts/parity/toon-roundtrip.ts        # 15/15, exit 0
~/.bun/bin/bun run rust/spikes/toon/probes/edge-cases.ts   # 58/58, exit 0
cargo run --manifest-path rust/spikes/toon/Cargo.toml --quiet -- < /dev/null; echo $?   # 0
~/.bun/bin/bun run scripts/parity/toon-roundtrip.ts --capture   # re-mints the five CLI captures
```

`--capture` indexes into `mkdtemp` and passes `--data-dir` to every call, so the repository's own
`.sangkar/graph.db` and `~/.config/sangkar/` are never written. `~/.config/sangkar/sangkar.json` was
diffed before and after the captures and is unchanged — this repository was already registered
before this task, and nothing was added or removed. Two `trace` cases resolve `src/index.ts::runTool`
for their depth; if that symbol is ever renamed both captures degrade to near-copies of the miss
case while still round-tripping, so re-point `CLI_CASES` before trusting a refreshed set.

Two negative controls keep the green run honest. The comparator's 9/9 self-check (the `control:`
line, printed on every run) locates injected differences including `~0`/`~1` pointer escaping, and
the FAIL path was exercised end to end by temporarily making the binary rewrite `status: ok` to
`status: OK`: the run printed `differs at /status: got string("OK"), want string("ok")` for the first
case and exited 1. The tamper was reverted; `src/main.rs`'s checksum was restored byte for byte.

## Encoder options used

The published package is `serde_toon_format` **0.2.0** — the contract's pinned `"0.2"` exists, so
nothing was substituted. Two naming details matter downstream:

- the **library target is `serde_toon`**, not `serde_toon_format` (`Cargo.toml` `[lib] name`), so the
  import is `serde_toon::…`;
- there is no `serde_toon_format::encode` free function. The entry point is
  **`serde_toon::to_string(&value)`**, which is exactly
  `to_string_with_options(value, &EncodeOptions::default())` (`lib.rs:142-148`).

Options are the plain defaults, no builder: `EncodeOptions { indent: Indent::Spaces(2), delimiter:
Delimiter::Comma }`. `encode_canonical`/`CanonicalProfile` and the optional `parallel` feature are
not used — default features only. The crate documents itself as "TOON v4.1 encoder/decoder", the
same working draft `@toon-format/toon` 4.4.0 implements.

One feature of the dependency tree is load-bearing and worth knowing: `serde_toon_format` depends on
`serde_json` with `preserve_order` and `float_roundtrip`, so `serde_json::Value` keeps insertion
order. That is why the real envelopes match the TypeScript output byte for byte instead of coming
out with sorted keys. `rust/crates/sangkar-core/Cargo.toml:10` already asks for `preserve_order`, so
feature unification into the product workspace adds nothing new.

## Result: 15/15 contract cases

```
control: 9/9 injected differences located by pointer, reordered keys equal
15 cases: 5 cli captures + 5 docs/research/tool-outputs + 5 named shapes
ok   cli   cli/01-status                              rust/spikes/toon/fixtures/cli/01-status.json  2829 B toon
ok   cli   cli/02-summary                             rust/spikes/toon/fixtures/cli/02-summary.json  4033 B toon
ok   cli   cli/03-trace-chain                         rust/spikes/toon/fixtures/cli/03-trace-chain.json  9989 B toon
ok   cli   cli/04-trace-depth1                        rust/spikes/toon/fixtures/cli/04-trace-depth1.json  5276 B toon
ok   cli   cli/05-trace-not-found                     rust/spikes/toon/fixtures/cli/05-trace-not-found.json  455 B toon
ok   docs  docs/list_graph_stats_tool                 docs/research/tool-outputs/list_graph_stats_tool.json  2004 B toon
ok   docs  docs/get_minimal_context_tool              docs/research/tool-outputs/get_minimal_context_tool.json  711 B toon
ok   docs  docs/get_hub_nodes_tool                    docs/research/tool-outputs/get_hub_nodes_tool.json  1889 B toon
ok   docs  docs/detect_changes_tool                   docs/research/tool-outputs/detect_changes_tool.json  5637 B toon
ok   docs  docs/index_status                          docs/research/tool-outputs/index_status.json  1954 B toon
ok   shape shape/uniform-objects-tabular              inline  180 B toon
ok   shape shape/mixed-objects-list                   inline  88 B toon
ok   shape shape/nested-40-element-array              inline  1041 B toon
ok   shape shape/nulls-and-empty-arrays               inline  146 B toon
ok   shape shape/newline-colon-leading-dash-string    inline  101 B toon
15/15 round-tripped through @toon-format/toon 4.4.0 decode
```

The five named shapes, and the TOON the crate writes for them:

```
# array of uniform objects -> tabular, one shared header
[2]{name,kind,fan_in,fan_out}:
  runTool,function,2,19
  GraphStore,Class,0,42

# array of mixed objects -> list form
[3]:
  - id: 1
    note: first
  - id: 2
    extra: true
    tags[2]: a,b
  - note: null

# nested object with a 40-element array, and 40 tabular rows beside it (both abridged)
generation: g-b59b82
coverage:
  indexed: 274
  skipped: 5465
  complete: false
latencies[40]: 0,7,14,…,273
units[40]{file,nodes}:
  src/unit-0.rs,1
  …

# nulls and empty arrays (empty objects included)
status: ok
stale_reason: null
not_indexed: []
nested:
  inner: null
  list: []
pairs[2]: null,null
mixed[4]: null,1,"",false

# string carrying a newline, a colon and a leading dash -> quoted, `\n` escaped
summary: "line one\n- must read: this"
note: "- leading dash: colon"
```

Beyond the contract, `probes/edge-cases.ts` round-trips 58/58: 1e300/1e-300/-1e-7, 20-deep nesting,
1000 tabular rows, 40 nested arrays, 2000-character strings, `\u0000`/U+2028/CRLF/emoji, keys that
need quoting or are empty, and the five largest real envelopes (269 KB JSON → 147 KB TOON in ~26 ms
including process start).

## Findings TASK-026 must carry

1. **The renderer must append the trailing newline itself.** The crate writes none;
   `src/shared/cli/render.ts:34` writes `` `${encode(envelope)}\n` ``. Against the CLI's live default
   TOON output for a `status` envelope, the spike's output was exactly one byte shorter, and the
   only other lines that differed held per-run values (`stale_checked_at`, and the HMAC tag on the
   `next` cursor, minted per process) — so the newline is the whole formatting gap.
2. **Large- or small-magnitude floats are written positionally, not in exponent form.** Rust's f64
   `Display` gives `v: 1000…0` (301 digits) for `1e300`, `v: -0.0000001` for `-1e-7`, where the
   TypeScript encoder writes `1e+300` and `-1e-7`. `decode` returns the same f64, so this is token
   cost, not corruption. No sangkar envelope today contains such a number — scores, risk and
   `durationMs` all sit inside `1e-6 … 1e21` — but the renderer must not assume every field is
   short: a sub-microsecond duration or a very large counter would take the long form.
3. **Some numeric-looking strings are quoted that the reference encoder leaves bare.** `"1.0.0"`
   keeps its quotes; bare `1.0.0` is also decoded as that string by the reference decoder. Again
   larger, never wrong.
4. **An empty object renders as nothing.** At a nested position it is `key:` with no children; at the
   root it is the empty string, and `decode("")` is `{}`. So "root `{}`" and "empty stdout" are
   indistinguishable. No envelope is ever `{}` — the smallest one seen is a 470-byte trace miss — but
   a renderer that pipes into a decoder must not treat empty output as an error.
5. **Key order is preserved, so byte-level goldens are viable.** All ten real envelopes are
   byte-identical to `@toon-format/toon`'s `encode`, which makes a byte-for-byte TOON golden a
   legitimate check in TASK-036 rather than a whitelist of allowed formatting.

## The fallback, named but not needed

Had a shape failed, the plan was to vendor `@toon-format/toon` 4.4.0's encoder as a workspace crate
`sangkar-toon`, from `node_modules/@toon-format/toon/dist/index.mjs` (61.7 KB bundled, single file,
`sideEffects: false`), porting the encoder half only and keeping `decode` for the parity script. That
plan stays unexecuted: no pointer failed, and no value needed reducing. If a later envelope does fail
— a number inside finding 2's blind spot is the likeliest candidate — the reduced value belongs here,
next to the rule that the CLI's TOON must decode to the same value, not to the same bytes.
