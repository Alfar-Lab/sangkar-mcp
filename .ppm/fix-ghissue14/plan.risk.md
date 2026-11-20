# Risks and open questions

## Risks

| Risk | Mitigation |
|---|---|
| **The fixture change silences the case it was meant to catch.** `tests/tools-expand.test.ts:616` asserts `not_indexed_total === 1` against a reader whose `notIndexedCount()` is `0` and `notIndexedAll()` is `[]` (`:301-302`), so today both derivations agree by accident. Flipping the assertion to `0` after the fix would leave the two meanings indistinguishable again. | The test task makes the fixture's graph count and prefix count differ — six literal out-of-prefix rows plus the folder's own, so the graph count is 7 and the prefix count 1 — and asserts all three: `totals.not_indexed_total === OUTSIDE_SKIPS.length + 1`, `coverage.skipped === 1`, `payload.not_indexed.length === 1`, plus the summary sentence naming 1 and a computed negative assertion on `${OUTSIDE_SKIPS.length + 1} not indexed`. Reverting the handler fix makes the cross-tool test fail on the totals, which is the point. |
| **The cross-tool test drags two more handlers into `tools-expand.test.ts`.** `status` and `summary` need `allFiles`, `censusByLanguage`, `notIndexedAll`, `latestGeneration` and a `dataDir` to `stat`. | The fixture is typed `GraphReader`, so it already satisfies the interface; `status`'s `dbSizeBytes` returns 0 when the file is absent instead of throwing (`src/tools/status/handler.ts:487-495`), and both handlers degrade on a non-git root. No module rule is involved: `tests/boundary.test.ts:324-331` walks `["src", "tests"]` and rule 5's `root-privileges` half exempts `tests/**` (`:202-207`), which is why `tests/acceptance.test.ts:30-49` already imports seven handlers. |
| **`status` and `summary` gain a dependency on fixture rows the expand suite never asserted.** A wrong `rowsForPaths` stub would make the summary scope count look arbitrary. | The cross-tool case asks `summary` for `scope: "area", target: "src/widgets"`, which resolves to exactly one prefix and one skip row — not the `repo` scope, whose `topLevelPrefixes` derivation is the separate defect recorded below. |
| **Moving `not_indexed_total` changes a client-visible number.** A consumer that read `expand`'s value as its scope's count now reads the graph's. | That is the issue's requirement and it is the number §1.1 always documented; the scope count does not disappear, it stays in `coverage.skipped`, which §1.1 already defines and which the same answer prints. Commit body uses `!`. |
| **The summary sentence and the trimmed list drift.** `emit` may trim `coverage.not_indexed` under a tight budget while `coverage.skipped` stays exact, so "5 not indexed" can sit beside 3 rows. | Pre-existing and documented (`TOOLS-SPEC` §1.1: `emit()` is the one thing that can leave the list shorter than the count). D2 changes which number is printed, not this asymmetry; adding a "showing N of M" wording would be a second behaviour change in a bug-fix plan. Real `expand` coverage answers on this repository are small enough that only `files` trims (measured: 38 of 74). |
| **`docs/` and `src/` could be committed apart.** `AGENTS.md:54` and `.agents/prompts/rules.md:52-53` are commit-granular: a commit that leaves `TOOLS-SPEC` describing the old carve-out is incomplete in the same way as one that leaves the code wrong. | The plan's structure carries that rule: the handler, the test and the `TOOLS-SPEC` edit are all phase 0, and phase 0 is one commit. Phase 1 is verification-only (whole-suite gate, live probe, `git status` review), so there is no second commit to desynchronise and no window in which the spec lies. |
| **The phase gate can pass on a tree where the spec is still wrong.** A docs edit is invisible to `biome check src tests bin` and to `bun test`. | The docs task's own verify is `grep`-based (the carve-out strings must be gone, both places must name `expand` and `coverage.skipped`), and phase 0's closing gate re-runs the five-file test set plus `typecheck`/`lint`; phase 1's `git status` review names every path the plan touched. |
| **A stale local graph makes a manual probe report pre-fix numbers.** `.sangkar/graph.db` is generation `g-a087a1` (134 indexed / 134 skipped); the issue quotes 115 on an unspecified tree and names no generation id. | Every probe reads a fresh `cp` of the file into a `/tmp` scratch and passes `--data-dir`; the plan records the generation it measured and states the issue's figure as the older tree's. Re-indexing the repository's own store stays the orchestrator's decision. |

## Open questions

None blocking. The design (no new key; `coverage.skipped` keeps the scope number; the sentence follows
the list) was ratified by the owner before this plan was written and is not reopened here.

## Recorded, not fixed: `summary`'s repo scope undercounts `coverage.skipped`

Separate candidate issue. On one generation (`g-a087a1`, 134 indexed files):
`status --json` → `coverage.skipped: 134` / `totals.not_indexed_total: 134`, while
`summary --json` → `coverage.skipped: 5` / `totals.not_indexed_total: 134`.

Evidence:

- `src/tools/summary/handler.ts:299-306` — `topLevelPrefixes(files)` builds the queried prefix list
  from `allFiles()`, so a top-level directory that holds no indexed file (here `.agents/`, `.ppm/`,
  `docs/`) is never queried and its skipped paths are unreachable: 5 of the 134.
- `src/tools/summary/handler.ts:812` — `scope === "area" && target !== "" ? [target] : topLevelPrefixes(ctx.reader.allFiles())`,
  the call site, for the `repo` and `change` scopes alike.
- The comment beside `topLevelPrefixes` claims the reader exposes no "every skipped path" query,
  although `GraphReader.notIndexedAll()` exists (`src/shared/store/read/reader.ts:208`) and `status`
  uses exactly that for the no-paths case (`src/tools/status/handler.ts:171-173`).
- The current behaviour was *requested*, not accidental:
  `.ppm/issue-remediation/tasks/phase_1.json` TASK-007 step "Add a case where a skipped path outside
  the top-level prefixes exists (`coverage.skipped` excludes it, `not_indexed_total` includes it)".

Why it is out of scope: it is a second, independent divergence between two tools' scope counts and
would move `summary`'s `coverage.skipped` and its printed sentence, i.e. a second behaviour change
with its own acceptance criteria. No task here touches `summary`.
