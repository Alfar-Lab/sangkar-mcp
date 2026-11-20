# Constraints and invariants

One bullet per rule every task must respect. A worker that cannot honour one stops and reports instead
of editing.

## Verification environment

- Repository root `/home/ljosalfar-server/Documents/researchs/codegraph-mcp`, Bun 1.4.4 on `PATH`
  (`bun --version` → `1.4.4`). Commands are `bun <script>` / `bun test <file>` run from the root;
  `~/.bun/bin/bun` is the same binary if `PATH` lacks it.
- Baseline at `dc5a548`, measured for this plan: `bun test tests/tools-expand.test.ts
  tests/tools-summary.test.ts tests/tools-status.test.ts` → 78 pass, 0 fail; `bun run typecheck` → exit
  0. A red baseline is recorded in `progress` as pre-existing, never "fixed".
- No database, service or network is needed. Every test that needs bytes uses `mkdtemp` temp dirs and
  removes them; no test writes inside this repository. `status`/`summary` tolerate a non-git repo root
  (they log `summary.change.degraded` and answer), which is what the fake-reader fixtures rely on.
- `bun run lint` is `biome check src tests bin` — it does **not** read `docs/`. A docs-only change
  cannot be verified by lint; verify it with `grep` assertions.
- Two gates, both run from the repository root. **Phase 0 gate**: `bun test
  tests/tools-expand.test.ts tests/tools-summary.test.ts tests/tools-status.test.ts
  tests/boundary.test.ts tests/contract.test.ts` (baseline `dc5a548`: 78 + 13 + 12 pass, 0 fail),
  `bun run typecheck` and `bun run lint`. **Phase 1 gate**, the repository Definition of Done
  (`.agents/prompts/run-build.md:38-42`): `bun run typecheck && bun run lint && bun run test &&
  bun run db:drift`, the whole-suite run, plus the live read-only probe and the `git status` review.
- `bun run db:drift` runs `drizzle-kit generate --name drift-check` and then `git diff --exit-code
  src/shared/store/migrations`. It must exit 0 and write nothing; if it writes a file, delete it and
  report a defect.
- Fixture generation for the tests is hand-written (`GraphReader` over plain arrays). Nothing indexes
  a real tree except a manual probe, and a manual probe only ever reads a copy.

## Data and state

- **`.sangkar/graph.db` is not modified by any task.** A manual probe copies it first
  (`cp .sangkar/graph.db /tmp/<scratch>/graph.db`) and passes `--data-dir /tmp/<scratch>`. No task runs
  `bin/sangkar.ts index` against any tree.
- No task runs a write command against this repository's git state (`add`, `commit`, `checkout`,
  `stash`, `tag`). **Commit granularity is the commit, not the file**: `AGENTS.md:54` and
  `.agents/prompts/rules.md:52-53` require the documentation touched by a change to land in the same
  commit, so the handler, its test and the `TOOLS-SPEC` correction are one commit. The orchestrator
  makes that single commit after phase 0's gate is green; phase 1 is verification-only and has
  nothing to commit.

## Public contract

- Payload keys are a public contract (`docs/my-product/TOOLS-SPEC.md` §1.1). **No new `totals` key, no
  new payload field, no renamed field.** The scope number stays where it already is:
  `coverage.skipped`.
- `coverage.skipped === coverage.not_indexed.length` must keep holding in `expand` **for an
  untruncated coverage answer**: both come from one `assemble()` call over the requested prefix, and
  the fix must not derive `skipped` from the new repo-wide total. `emit` is allowed to trim the
  envelope's `coverage.not_indexed` under a tight budget while `coverage.skipped` stays the exact
  count — that asymmetry is the documented truncation contract (`TOOLS-SPEC.md:100-102`; the coverage
  pass is `trimOnce` in `src/shared/budget.ts:284-320`, list drop at `:312`) — and live evidence shows
  it fires: `expand src/ --what coverage` on this repository answers `truncated: true`,
  `truncation_reason: "budget"`.
- `totals.not_indexed_total` is the repo-wide count read from the graph, identical for one generation
  across `status`, `summary` and `expand`. It is never derived from a list this answer holds.
- No envelope shape change: the nine fields, `total_relation` legality, `emit()`'s truncation and
  cursor minting are untouched. `emit` still may trim `coverage.not_indexed` under a tight budget while
  `coverage.skipped` stays the exact count (`TOOLS-SPEC` §1.1) — that asymmetry is documented, not a
  defect to fix here.
- No schema change, no migration, no dependency change. `package.json` pins stay put.

## Module rules (enforced by `tests/boundary.test.ts`)

- `tool-isolation`: `src/tools/<tool>/**` imports only `#shared/*` and its own siblings — so `expand`
  may not import `status`'s or `summary`'s helpers. A cross-tool *test* is a different matter by
  construction: `tests/boundary.test.ts:324-331` walks `["src", "tests"]`, and rule 5's
  `root-privileges` half exempts `tests/**` (`:202-207`), because the harness has to exercise the
  handlers. `tests/acceptance.test.ts:30-49` already imports seven of them.
- `shared-isolation`: `src/shared/**` never imports `src/tools/**` or `src/indexer/**`.
- Tool handlers take `ToolContext` and never open a connection, import `drizzle-orm` or read
  `process.env`. No Drizzle query may be built in a handler: the `store/db.ts` prepared cache is the
  only path, and this plan adds no query at all.

## Documentation and assertions

- `docs/my-product/TOOLS-SPEC.md` is the payload contract: `expand`'s key and §1.1 must agree, in the
  same phase, with no sentence left describing the old carve-out.
- Never weaken an assertion to make a test pass. Where a contract changed, the assertion and the
  contract move together and the commit body names the invariant.
- Conventional commits, scoped, `!` for a payload/behaviour change.

## Ownership

- A task modifies only the paths in its `files`. An overlap between two concurrently eligible tasks is
  a plan defect: report it rather than editing the other task's file.
- `src/tools/expand/handler.ts` is owned by exactly one task (TASK-001); `docs/my-product/TOOLS-SPEC.md`
  by exactly one task (TASK-003); `tests/tools-expand.test.ts` by exactly one task (TASK-002).
