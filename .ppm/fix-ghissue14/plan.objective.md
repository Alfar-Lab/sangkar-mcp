# Objective

Make `totals.not_indexed_total` mean one thing in every tool: the number of paths the *graph* never
read, for the generation being served. Today `status` and `summary` publish the graph's count while
`expand` publishes the count of the prefix it was asked about, so a client that caches the key across
tools gets 134 from one call and 5 from the next on the same generation — the class of defect issue #4
was about, one tool over.

## Scope

In:

- `expand`'s `what: "coverage"` answer publishes the graph's skipped-path count in
  `totals.not_indexed_total`, from the `assemble()` result its scope already computes.
- `expand`'s summary sentence for that answer reads the scope's count (`coverage.skipped`), so the
  number it prints still describes the list printed beside it.
- The tests that pin both halves: the existing `expand` folder-coverage case is re-pointed at a
  fixture whose graph count differs from its prefix count, and one cross-tool case proves `status`,
  `summary` and `expand` agree on the total for one fixture generation.
- `docs/my-product/TOOLS-SPEC.md` §1.1 and §7 say the same thing **in the same commit** as the
  handler and its test (`AGENTS.md:54`, `.agents/prompts/rules.md:52-53` are commit-granular), with
  no residual sentence describing the old carve-out.

Out (recorded, not fixed — see `plan.risk.md`):

- `summary`'s *repo* scope undercounting `coverage.skipped` (5 vs `status`'s 134 on the same
  generation), because `topLevelPrefixes` only queries prefixes that hold an indexed file.
- A new payload key (`not_indexed_in_scope_total` or similar). The owner-ratified design keeps the
  scope number in `coverage.skipped`.
- Any change to `status`, `summary`, `impact`, `trace`, `search`, the envelope contract, the schema,
  or the migrations.
- Any behaviour of `coverage.tests`, `total_relation`, truncation, or the `index` tool's run
  accounting.

## Sources

- GitHub issue #14, `Alfar-Lab/sangkar-mcp`: "totals.not_indexed_total means the graph in
  status/summary and a scope in expand" (acceptance criteria quoted in `plan.approach.md` D1).
- `docs/my-product/TOOLS-SPEC.md` §1.1 (the envelope's counts) and §7 (`expand`).
- `.ppm/issue-remediation/plan.approach.md` D3 and `.ppm/issue-remediation/tasks/phase_1.json:76` —
  the decision that made `totals.not_indexed_total` the graph's count in `status` and `summary`.
- `AGENTS.md`, `.agents/prompts/rules.md` (module rules, documentation stays true in the same commit).

## Done when

Every command runs from `/home/ljosalfar-server/Documents/researchs/codegraph-mcp`; `bun` is 1.4.4 and
on `PATH`.

1. Phase 0 gate: `bun test tests/tools-expand.test.ts tests/tools-summary.test.ts
   tests/tools-status.test.ts tests/boundary.test.ts tests/contract.test.ts` — all pass. Baseline at
   `dc5a548`: 78 pass in the three tool files, 13 in `tests/boundary.test.ts`, 12 in
   `tests/contract.test.ts`; `bun run typecheck` → exit 0; `bun run lint` → exit 0.
2. Phase 1 gate, the repository Definition of Done (`.agents/prompts/run-build.md:38-42`):
   `bun run typecheck && bun run lint && bun run test && bun run db:drift` — every command exits 0,
   the whole `bun run test` suite is green, and `db:drift` leaves
   `git status --porcelain src/shared/store/migrations` empty.
3. Criterion 1 of the issue: on one fixture generation with skipped rows outside the requested
   prefix, `status`, `summary` and `expand` publish an identical `totals.not_indexed_total`
   (`tests/tools-expand.test.ts`, the cross-tool case added by the test task).
4. Criterion 2: `expand`'s scope count is readable as `coverage.skipped`, equals
   `coverage.not_indexed.length` for an untruncated coverage answer, and is what its summary
   sentence prints — pinned by the same file.
5. End to end on a read-only copy of this repository's graph (`--data-dir /tmp/<scratch>`, never
   `.sangkar/graph.db`): `status --json`, `summary --json` and `expand src/ --what coverage --json`
   report the same `totals.not_indexed_total`, while that `expand` answer's `coverage.skipped` is the
   prefix's smaller count and its `summary` prints that smaller count.
6. `grep -rn "not_indexed_total" docs/ src/` shows no sentence claiming `expand`'s key is its scope's
   count.
7. `gh issue view 14 --repo Alfar-Lab/sangkar-mcp --json state,comments` shows one new comment
   carrying the commit, the changed files, both acceptance criteria with their evidence and the test
   counts, and the issue is `CLOSED` — or the comment names the criterion still unmet and the issue
   stays `OPEN`. `TASK-006` (the last task of the last phase) owns this; it writes no repository file.
