# Gaps and future works

Plan state and the open work. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Current state and known gaps

All eight earlier plans are complete (`ppm plan_status` has no open task in any of them): `codegraph-mvp`
36/36, `cgraph-cli` 22/22, `sangkar-telemetry` 22/22, `sangkar-daemon` 27/27, `issue-remediation` 20/20,
`answer-fidelity` 19/19, `fix-ghissue13` 8/8, `fix-ghissue14` 6/6. What that leaves in the tree: the daemon
with its `daemon` and `repo` verbs over `sangkar.json`, the machine's `<home>/config.json` beside each
repository's `<repo>/.sangkar/config.json`, and usage records under `<dataDir>/telemetry/` — recording is on by
default, `SANGKAR_TELEMETRY=off` outranks both config files and disables it, and `sangkar telemetry` renders
the offline HTML report they are read with. The suite is green: 938 tests across 59 files.

A ninth plan is open: `rust-migration`, six phases and 73 tasks under `.ppm/rust-migration/`, in Phase 0. What
it has added is `rust/`, a nine-crate Cargo workspace whose modules are mostly 1-byte stubs: the kernel
modules that have landed, the store's `schema`/`migrations`/`migrate`, the `sangkar-boundary` DAG gate, the
parity harness under `scripts/parity/`, the three fixture repositories and the captured goldens exist; the
read path, the indexer, the tools and the daemon do not. `src/` is untouched and is still the published
artifact, and neither tree is deleted before Phase 5. `docs/my-product/RUST-MIGRATION.md` §7 states each
phase's gate; the three gaps below were measured on a graph the TypeScript indexer built, which is still the
only implementation that can build one.

Three things the tests do **not** catch, re-measured on 2026-11-23 from a fresh index of this tree
(`index --data-dir /tmp/…`, 136 files): **1 951 nodes**, **11 739 edges** (7 309 `calls`, 1 815 `contains`,
1 582 `references`, 690 `tested_by`, 334 `imports`, 9 `inherits`), **14 776 unresolved** call sites and
**148 `not_indexed`** paths — all `unsupported_ext` (78 `.json`, 64 `.md`, 2 `.sql`, 1 `.sh`, 1 `.toml`,
1 `.lock`, `LICENSE`), not one of them a file in any of the twelve indexed languages. Relative imports bind
now: **0** unresolved rows point at a `./` or `../` target, against the pre-merge baseline of **71** — the
only two `import:./…` strings left in the store are fixture literals inside `tests/unresolved.test.ts`.
Treat the three below as the open work:

1. **`kind: "test"` is not applied, and where it fires it is a name match.** This repo has **704** nodes
   with `is_test = 1` but only **12** with `kind = "test"`; **515** of the 704 are labelled `function` (118
   `class`, 59 `file`). `src/indexer/extract.ts:419` derives `is_test` as `this.#isTestFile || readsAsTest`,
   but line 420 hands `#kindOf` only `readsAsTest` — the `TEST_NAME` regex over the declaration's own name —
   so a test body with an ordinary name keeps `function`. The 12 that do come out as `test` are all regex
   hits, and 7 of them are `src/` declarations whose names merely *read* as test (`TestCensus`,
   `testsCensus`, `specFor`…), so the kind both under-fires under `tests/` and mis-fires under `src/`. Every
   payload that renders `kind` carries the mislabel; `search` cannot compensate, since its `kind` enum has
   five values and `test` is not one of them — the fact lives in `node.is_test`.
2. **`kind: "method"` is nearly unused.** **493** nodes are labelled `class` and only **31** are `method`,
   and all 31 are the members of five classes (`FileExtractor` 13, `SymbolIndex` 12, `StatementCache` 4,
   `CliUsageError` 1, `AppError` 1) — class members do come out as `method`. Of the 493, **399** are
   `interface` declarations and **89** `type` aliases; real `class` declarations number **5**, and this tree
   has no enum. `src/indexer/lang/registry.ts` lists all of those syntax types in `classTypes`, so the class
   census is a declaration census, not a class census. `parser-parity.test.ts` cannot see this because it
   asserts `function ∪ method`, and it records CRG's `Class 877` without asserting it.
3. **Binding recall is the product bottleneck.** **14 776** unresolved call sites against **7 309** `calls`
   edges — 66.9 % of all call sites, ≈2:1 — of which **5 915** are `material` (the rest collapse against the
   builtin allowlist); the store's split is 7 457 `dynamic`, 7 182 `receiver_type_unknown`, 137 `ambiguous`,
   0 `missing_module`. Only **418** of **1 951** nodes have a test within two hops (169 at depth 1, 249 at
   depth 2), so `total_relation` is `gte` almost everywhere, and the 1 533 nodes whose `tested_depth` is
   NULL include symbols a test does exercise but whose call never bound: `test_gaps` lists unproven coverage
   beside absent coverage and cannot tell them apart. The unresolved census the answering payloads now carry
   (`total`, `material`, `buckets`) at least counts the thinness instead of hiding it.

`parser-parity.test.ts` also deliberately does not gate total edge count against CRG (130 044 vs 62 536 on
the shared corpus) because the edge taxonomies differ. If you add edge kinds, decide whether to gate them.

## Future works

The three gaps above are the product work. One item sits outside them:

1. **Grammar WASM is not embedded in the compiled binary.** Grammars load from `node_modules` at index
   time, so `dist/sangkar` serves an index but cannot build one. Compiling the 12-language set in is what
   the distribution work waits for.

The packaging item this section used to carry closed on 2026-11-23: `0.4.0-beta.1` reached npm, so the
`beta` tag serves it.
