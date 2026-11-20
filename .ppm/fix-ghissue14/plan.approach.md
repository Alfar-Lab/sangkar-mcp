# Approach

## Current state

Verified at `dc5a548`. Line numbers are from that commit. The graph probed below is this
repository's `.sangkar/graph.db`, generation `g-a087a1` (134 indexed files, published
2026-11-20T07:12:11Z). The issue itself names no generation id — its text says "Measured after
`59f…`: both report 115" — so the figure it quotes is 115 on an unspecified tree; this plan measured
134 on `g-a087a1`. The magnitude moved with the tree, the shape of the defect did not. Every probe
below read a **copy** of the store, `cp .sangkar/graph.db /tmp/probe14/graph.db`, with
`--data-dir /tmp/probe14`; the live store was never opened for writing.

- Probes on one generation: `status --json` → `totals.not_indexed_total: 134`,
  `coverage.skipped: 134`, `coverage.indexed: 134`; `summary --json` (default `change` scope) →
  `totals.not_indexed_total: 134`, `coverage.skipped: 5`; `expand src/ --what coverage --json` →
  `totals.not_indexed_total: 5`, `coverage.skipped: 5`, `payload.files: 38`,
  `summary: "coverage for src: showing 38 of 74 files, 5 not indexed."`. Two numbers, one key.
- `src/tools/expand/handler.ts:1117` — `not_indexed_total: payload.not_indexed.length`, inside
  `coverageAnswer` (declared `:1101`, returns at `:1134`). It is the only `expand` answer that
  publishes the key, and the value is the prefix's list length.
- `src/tools/expand/handler.ts:636` — `const skipped = totals.not_indexed_total ?? payload.not_indexed.length;`
  in `coverageSummary` (`:633-642`). Left alone, the sentence would read "134 not indexed" beside the
  5 rows the same answer printed.
- The graph's count is already computed and thrown away: `scopeOf` (`:409-423`) calls `assemble` and
  returns only `read_status`, `coverage`, `relation`; `assemble`
  (`src/shared/domain/coverage.ts:109-131`) returns `notIndexedTotal: source.notIndexedCount()` at
  `:131` in the same object. Its `Scoped` interface is `:361-365`. So the fix costs zero extra queries.
- `status` reads the same number once (`src/tools/status/handler.ts:184`, published at `:264`);
  `summary` reads it through `assemble` (`src/tools/summary/handler.ts:820-824`, published at `:847`).
  `GraphReader.notIndexedCount()` is declared at `src/shared/store/read/reader.ts:210` and bound at
  `:423`, and every hand-written fake reader in `tests/` already implements it.
- Comments already state the fixed semantics, so no `src/` comment needs an edit: `status:262-264`
  ("the same number `summary` and `expand` publish"), `src/shared/domain/coverage.ts:17-21`
  ("`notIndexedTotal` is the whole graph's count that a scoped answer (`summary`, `expand`) publishes
  as `not_indexed_total`"). They are false today and become true.
- Tests: `tests/tools-expand.test.ts:616` asserts `totals.not_indexed_total === 1` against a fixture
  whose `notIndexedAll: () => []` (`:301`) and `notIndexedCount: () => 0` (`:302`), so the case never
  had to distinguish the two counts. `tests/tools-summary.test.ts:730-757` pins the `summary` side
  already (`coverage.skipped: 1`, `not_indexed_total: SKIPPED_ROWS.length`, all three scopes publish
  it) — this plan cites it and does not duplicate it. `tests/tools-status.test.ts:735,805` pin the
  `status` side.
- Docs: `docs/my-product/TOOLS-SPEC.md:97` (§1.1) defines the key as the repo-wide count "read from
  the graph", naming only `status` and `summary`; `:794-797` (§7) says `expand`'s value is
  `payload.not_indexed.length`, "not the repo-wide … that `status` and `summary` publish". The file
  mentions the key at nine lines — `:54, :90, :95, :97, :205, :250, :493, :795, :796` — and only the
  §7 pair (`:795-796`) states the carve-out. §7's four example envelopes (`:800-916`) are `members`,
  `outline`, `source` and `ambiguous`; none is a coverage answer and none carries the key (0 matches
  in that range), so only the prose contradicts.

## Design

- **D1 — publish `assemble`'s number, not a fresh call.** `Scoped` gains
  `notIndexedTotal: number`, filled from `assembly.notIndexedTotal` in `scopeOf`, and
  `coverageAnswer` publishes it. Rationale: the value is already computed for this answer (one
  `COUNT(*)` through the prepared cache), and reading it from the same assembly the `coverage` object
  came from makes "same generation, same number" structural rather than a coincidence of two reads.
  Rejected: `ctx.reader.notIndexedCount()` at the publish site (a second query per coverage answer),
  a new payload key `not_indexed_in_scope_total` (owner-ratified: no new key), and reusing
  `totals.files_total` arithmetic (different population).
- **D2 — the summary sentence reads `coverage.skipped`.** `coverageSummary` takes the scope count as
  a parameter instead of `totals.not_indexed_total ?? payload.not_indexed.length`, and the post-`emit`
  re-render passes `env.coverage.skipped`. Rationale: the sentence prints beside the list this answer
  holds, so it must name that list's population; the repo-wide total is one tool call away and is not
  what "N not indexed" next to a 5-row list means. Post-`emit` behaviour is unchanged: a trimmed list
  beside the exact scope count is the case `TOOLS-SPEC` §1.1 already documents for `coverage`.
- **D3 — one cross-tool test, inside `tests/tools-expand.test.ts`.** The fixture there is the only
  fake `GraphReader` that already carries a folder prefix with a skipped row, and it is typed
  `GraphReader`, so the same `ToolContext` can be handed to `status`'s and `summary`'s `handle`.
  Rationale: the issue's criterion 1 is a cross-tool claim, and a test that calls all three handlers
  on one fixture generation proves it; a new `tests/not-indexed-total.test.ts` would duplicate ~250
  lines of fake reader and need a `.agents/prompts/test-map.md` row. Importing handlers into a test is
  established practice, not a boundary hole: `tests/boundary.test.ts:324-331` walks `["src", "tests"]`
  and rule 5's `root-privileges` half exempts `tests/**` (`:202-207`), which is why
  `tests/acceptance.test.ts:30-49` already imports seven handlers. `status`/`summary` degrade cleanly
  on the fixture's non-git temp root (they log `summary.change.degraded` and answer), which
  `tests/tools-summary.test.ts` already exercises.
- **D4 — the fixture's two counts are made unequal on purpose.** `notIndexedCount()` returns the
  graph's total and `notIndexedAll()` returns that many rows, of which exactly one is under the
  folder prefix. Rationale: a fixture where the two counts coincide cannot fail before the fix, which
  is why `:616` passes today.

## Phases

### phase_0: `expand` publishes the graph's skip total, spec in the same commit

Deliverable, shipped together: `src/tools/expand/handler.ts` publishes `totals.not_indexed_total`
from `assemble` and words its coverage sentence from `coverage.skipped`; `tests/tools-expand.test.ts`
re-points the folder-coverage assertion at a fixture whose graph count exceeds its prefix count and
adds the cross-tool agreement case; `docs/my-product/TOOLS-SPEC.md` §1.1 and §7 are corrected. The
commit exists because `AGENTS.md:54` and `.agents/prompts/rules.md:52-53` are commit-granular: a
commit that leaves `TOOLS-SPEC` describing the old carve-out is incomplete, so the spec cannot be a
later phase. The phase gate is the three tool test files plus `tests/boundary.test.ts` and
`tests/contract.test.ts`, `bun run typecheck` and `bun run lint`.

Sync barrier because phase 1 runs the repository Definition of Done over the final tree, and because
`tests/tools-expand.test.ts` cannot assert the contract before the handler implements it and before
the spec states it.

### phase_1: the repository Definition of Done, on the final tree

Deliverable: one read-only gate task that runs `.agents/prompts/run-build.md:38-42`
(`bun run typecheck && bun run lint && bun run test && bun run db:drift`), the whole suite rather than
the five phase-0 files, then the live read-only probe on a copy of this repository's graph and the
`git status` review. It is a phase of its own because a whole-suite run plus the acceptance-heavy
files costs minutes and belongs at the end, not inside the change loop; nothing is committed here
because phase 0's single commit already carries every file the plan touches. The phase closes with
`TASK-006` — the last task of the last phase, `pre_request: [TASK-005]` — which publishes that
evidence as one comment on GitHub issue #14 and closes it, or names the criterion still unmet and
leaves it open. It writes no repository file. The pattern is deliberate: an executed plan that does
not answer its issue leaves the tracker and the tree disagreeing about what shipped.
