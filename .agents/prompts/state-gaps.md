# State and gaps

Plan state and the open work. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Current state and known gaps

All three plans are complete (`ppm plan_status`): `codegraph-mvp` 36/36, `cgraph-cli` 22/22,
`sangkar-telemetry` 22/22 — local usage records under `<dataDir>/telemetry/` (`SANGKAR_TELEMETRY=off`
disables them) and `sangkar telemetry`, the offline HTML report they are read with. `sangkar-daemon` is in
progress: the daemon, the machine registry and the per-repo config are in the tree, and
`ppm plan_status` has its task counts. The suite is green.

Three things the tests do **not** catch, re-measured on 2026-11-19 from a scratch index of this tree
taken after phase 1 of `issue-remediation` (`index --data-dir /tmp/…`): **1 640 nodes**, **9 347 edges**
(5 894 `calls`, 1 531 `contains`, 1 102 `references`, 536 `tested_by`, 275 `imports`, 9 `inherits`),
**11 519 unresolved** call sites and **115 `not_indexed`** paths — all `unsupported_ext`, no source file
among them. Relative imports bind now: **0** unresolved rows keep `import:` evidence against a `./` or
`../` target, against the pre-merge baseline of **71**. Treat the three below as the open work:

1. **`kind: "test"` is not applied.** This repo has **547** nodes with `is_test = 1` but only **12** with
   `kind = "test"`; **403** of the 547 are labelled `function`. `src/indexer/extract.ts` reaches `test` only
   through `#kindOf`'s `readsAsTest` — the `TEST_NAME` regex on the declaration's own name — never from
   `is_test`, so a test body with an ordinary name keeps `function`. Every payload that renders `kind`
   mislabels test functions, and `search(kind='test')` is nearly empty.
2. **`kind: "method"` is nearly unused.** **422** nodes are labelled `class` and only **31** are `method`;
   all 31 are the members of five classes (`FileExtractor` 13, `SymbolIndex` 12, `StatementCache` 4,
   `CliUsageError` 1, `AppError` 1), so class members do come out as `method` — most of the 422 are
   TypeScript interfaces, type aliases and enums, which `src/indexer/lang/registry.ts` lists in `classTypes`.
   `parser-parity.test.ts` cannot see this because it asserts `function ∪ method`.
3. **Binding recall is the product bottleneck.** This repo: **11 519 unresolved** call sites against
   **5 894** `calls` edges (≈66 %), and only **368** of **1 640** nodes have a test within two hops.
   `total_relation` is therefore `gte` almost everywhere, and `test_gaps` under-reports. Until recall
   improves, absence answers are honest but thin.

`parser-parity.test.ts` also deliberately does not gate total edge count against CRG (130 044 vs ~62 000 on
the shared corpus) because the edge taxonomies differ. If you add edge kinds, decide whether to gate them.
