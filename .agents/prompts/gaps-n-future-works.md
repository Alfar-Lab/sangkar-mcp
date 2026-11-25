# Gaps and future works

Plan state and the open work. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Current state and known gaps

One implementation: the nine-crate Rust workspace under `rust/`, packaged as the npm launcher plus one
platform package. The `rust-migration` plan is in its last phase — `ppm plan_status` is the state, not this
file. What the tree carries: `cargo test --workspace` (978 passed, 1 ignored, 71 suites),
`cargo test -p sangkar-cli --test goldens` (57 goldens, 57 identical), `cargo test -p sangkar-boundary`
(8 tests), `npm pack --dry-run` (7 files).

Everything below was measured on this tree, not carried over from the TypeScript implementation. The run:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release --locked --manifest-path rust/Cargo.toml -p sangkar-cli --target x86_64-unknown-linux-gnu
./rust/target/x86_64-unknown-linux-gnu/release/sangkar index --repo-root . --data-dir /tmp/sangkar-gaps --mode full
```

That published generation `g-a8c053` from base commit `6835f17` with the worktree dirty (the phase-5 diff),
in 1 462 ms: **177 files** (168 rust, 7 typescript, 1 javascript, 1 python), **4 375 nodes**, **15 220 edges**
(7 568 `calls`, 4 198 `contains`, 2 055 `references`, 1 119 `tested_by`, 280 `imports`; 8 640 `extracted`,
6 580 `inferred`), **27 709 unresolved** call sites, **2 406 `not_indexed`** paths. Four things the suite does
**not** catch, each with the query that produced its numbers:

1. **`kind: "test"` is a name match, and only a name match.** `rust/crates/sangkar-index/src/extract/declare.rs:116-118`
   derives `is_test = is_test_file || reads_as_test` but hands `kind_of` only `reads_as_test`, so a test body
   with an ordinary name keeps its structural kind. This tree has **2 098** nodes with `is_test = 1` and only
   **68** with `kind = "test"`; **1 866** of the 2 098 are `function` (112 `class`, 52 `file`). Of the 68, 44 sit
   under a `tests/` directory and **24 do not** — `verb_spec`, `TestCensus`, `tests_census`, `spec_for`,
   `base_spec` — so the kind both under-fires under test paths and mis-fires outside them. Every payload that
   renders `kind` carries the mislabel; `search` cannot compensate, since its `kind` enum has five values and
   `test` is not one of them — the fact lives in `node.is_test`.
2. **`kind: "class"` counts declarations, and `kind: "method"` counts trait members.** **607** nodes are
   `class` — 588 Rust and 19 TypeScript. The Rust language row lists `struct_item`, `enum_item` and
   `trait_item` as its class types (`impl_item` deliberately is not one), so the 588 are structs, enums and
   traits — Rust has no `class` to count — and of the 19 TypeScript ones, 18 are interface/type aliases in
   `scripts/parity/` and one (`Session`) is the retired harness's own class. **45** nodes are `method`: the
   39 Rust rows are trait method signatures — a trait is a class type, an `impl` block is not, so impl members
   come out as `function` — and the 6 TypeScript rows are members of that harness class. Nothing asserts the
   split: the census goldens pin the labels as they are, so a wrong one would have to be caught by reading
   this paragraph, not by a test.
3. **Binding recall is still the product bottleneck.** **27 709** unresolved call sites against **7 568**
   `calls` edges — 78.5 % of all 35 277 call sites, ≈3.7:1 — of which **14 734** are `material` (the narrow
   population that could be user-defined code). The census the payloads carry splits them: 13 948
   `receiver_type_unknown`, 8 049 `dynamic_property_call`, 3 222 `external_package`, 1 854 `builtin_stdlib`,
   631 `ambiguous`, 5 `internal_module`. `total_relation` is therefore `gte` almost everywhere: only **419**
   of 4 375 nodes have a test within two hops (190 at depth 1, 229 at depth 2) and **3 956** have none, so
   `test_gaps` lists unproven coverage beside absent coverage and cannot tell them apart.
4. **Discovery walks a nested `target/` it cannot see the ignore rules for.** `ALWAYS_EXCLUDED` is
   `.git`, `node_modules`, `.sangkar`, `dist`, `vendor`, and the ignore rules come from the root `.gitignore`
   and `.sangkarignore` only — a crate's own `.gitignore` is not read. The two spike workspaces under
   `rust/spikes/` therefore have their 930 MB `target/` trees walked: **9** build-script outputs are indexed as
   real files (9 nodes, 14 unresolved rows) and **2 143** of the 2 406 `not_indexed` rows are under
   `rust/spikes/*/target/` (`.o`, `.rlib`, `.rmeta`, `.d`, `.json`, build-script stdout/stderr). The root
   `rust/target/` is pruned because the root `.gitignore` names it; a third-party crate's is not.

The queries behind 1–4, against the database the run above publishes:

```sh
./rust/target/x86_64-unknown-linux-gnu/release/sangkar status --repo-root . --data-dir /tmp/sangkar-gaps --json
python3 - /tmp/sangkar-gaps <<'PY'
import sqlite3, sys
con = sqlite3.connect(f"file:{sys.argv[1]}/graph.db?mode=ro", uri=True)
q = lambda s: con.execute(s).fetchall()
print(q("select kind, is_test, count(*) from node group by 1,2"))                 # gap 1 and 2
print(q("select count(*) from node n join file f on f.id=n.file_id "
        "where n.kind='test' and f.path not like '%/tests/%'"))                  # gap 1, the misfires
print(q("select tested_depth, count(*) from node_metrics group by 1"))           # gap 3
print(q("select reason, count(*) from not_indexed group by 1"))                  # gap 4
PY
```

`status --json` is where the unresolved census comes from (`payload.unresolved`: `total`, `material` and the
six buckets). `entryPoints` is also empty on this tree, for a reason the suite pins rather than catches:
`node_metrics.is_entry` is **0** for all 4 375 nodes, so `summary`'s flows are empty and the census goldens
record agreement with the TypeScript build, not an ordering.

One grammar substitution is **measured**, not assumed, and it is the only allowance the Rust census golden
carries: `base.Deposit(amount)` at `rust/tests/parity/fixtures/polyglot/src/Account.cs:22`. The wasm
`tree-sitter-c_sharp.wasm` bundle parses `base` as the named node `base_expression`, which is in neither
`NAME_TYPES` nor `CHAIN_TYPES`, so `calleeSite` returns null and the TypeScript build recorded the site as
`dynamic` with `raw_target: "base.Deposit"`; `tree-sitter-c-sharp` 0.24.0 — the crate this binary links —
makes `base` an anonymous token, so the Rust name chain sees one identifier and records
`receiver_type_unknown` with `raw_target: "Deposit"`. One unresolved site moves bucket, which is why
`rust/tests/golden/census/polyglot.ts.json` names that grammar pair in `allowed_delta`: the delta table
prints `unresolved_by_kind.dynamic` 15 → 14 and `unresolved_by_kind.receiver_type_unknown` 8 → 9, both
allowed, with the 26-row total and every node, edge and per-language count equal.

```sh
cargo test --manifest-path rust/Cargo.toml -p sangkar-index --test census -- --nocapture
# polyglot: 92 keys compared, 0 differ | broken: 27 keys compared, 0 differ
```

There is no second per-language delta: `broken` has no allowed row, and the census compares the six ordered
row sequences (node ids, edge ids, FTS state rowids, entry-point ids, hotspots, duplicate-name ids), so a
grammar change that reorders anything fails rather than shifting a total.

## Future works

The four gaps above are the product work. Three items sit outside them:

1. **Publishing the two packages needs the maintainer.** `rust/scripts/package.sh` lays out
   `sangkar-mcp-linux-x64-gnu` and the launcher ships it, but neither is on the registry; the publish needs an
   OTP and a fresh version number, because `0.6.0-beta.1` is spent on the TypeScript release that macOS,
   Windows and pre-2.35 glibc still install. `.agents/skills/build-n-deploy/SKILL.md` is the procedure.
2. **No second platform exists.** The launcher's table has one row (`linux:x64`); macOS and Windows get one
   refusal line naming the published platform and the version to pin. A second platform is a release target,
   a package directory and one table row — the release image decides the glibc floor, so a different libc is a
   different package, not a rebuild.
3. **The capture harness cannot run again.** `scripts/parity/` is TypeScript that imported the deleted tree;
   the committed goldens under `rust/tests/golden/` are the only oracle. A payload that moves on purpose now
   moves its golden in the same commit, and the census test is what keeps the row ids and their order honest.
   Writing the replacement (a Rust capture that reads a fixture and writes a golden) is worth a task the day
   someone needs a new fixture.
