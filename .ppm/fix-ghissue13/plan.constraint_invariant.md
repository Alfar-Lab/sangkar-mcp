# Constraints and invariants

One bullet per rule every task must respect. A worker that cannot honour one of these stops and
reports instead of editing.

## Verification environment (measured at `dc5a548`, 2026-11-20)

- `bun` **is** on `PATH` on this machine: `bun` → `/home/ljosalfar-server/.local/bin/bun`, version
  `1.4.4`. Every command below runs from the repository root
  `/home/ljosalfar-server/Documents/researchs/codegraph-mcp`. (`~/.local/bin/bun` is the absolute
  form for a worker whose `PATH` differs.)
- The scripts are `bun test`, `bun run typecheck` (`tsc --noEmit`), `bun run lint`
  (`biome check src tests bin`), `bun run db:drift`
  (`drizzle-kit generate --name drift-check && git diff --exit-code src/shared/store/migrations`).
- **Baseline is green and must stay green.** `bun run typecheck` exit 0; `bun run lint` exit 0
  (134 files); `bun test` 903 pass / 0 fail in ~55 s; `bun test tests/tools-status.test.ts`
  25 pass; `bun test tests/store-refresh.test.ts` 7 pass. A task that finds a red baseline records
  it as pre-existing in `progress` and does not "fix" it.
- A scoped check for a task is `bun test tests/<file>.test.ts`. The phase gate adds `typecheck` and
  `lint`; the phase-1 gate adds the full `bun test` and `bun run db:drift`.
- Database and sandbox: the verify commands need **no** live database. Every test builds its own
  graph under `mkdtemp` (`tests/store-refresh.test.ts:83`) with `SANGKAR_HOME` pointed at the temp
  home (`:98`), so no migration, seed step or service is a prerequisite.
- What the executor must not do: no live-DB migration, no `--data-dir` pointing inside this repo, no
  write to shared infrastructure, no git write command (`add`, `commit`, `checkout`, `stash`,
  `push`, tag), no `npm publish`.

## Data and state

- **`.sangkar/graph.db` (4 677 632 bytes, mtime 2026-11-20 14:12) is not touched by any task.** No
  test, probe or command writes it; re-indexing this repository is the orchestrator's decision. The
  full suite is known not to modify it (mtime unchanged after a run) — a run that does is a defect
  to report, not to clean up by deletion.
- Fixtures live under `mkdtemp` and are removed in `afterAll`; a test never writes inside this
  repository. No network access in any test.

## Payload and store invariants

- **Payload keys are a public contract** (`docs/my-product/TOOLS-SPEC.md` §1.1, `tests/contract.test.ts`).
  `payload.graph` keeps exactly `nodes`, `edges`, `size_bytes`, `db_path`; `size_bytes` stays a
  `number` and stays **bytes** of the database file (not logical pages, not MiB). No payload field is
  renamed, removed or added by this plan, and the nine envelope fields are untouched.
- **No schema, migration, table or column change.** `src/shared/store/migrations/**` stays
  byte-identical; `bun run db:drift` must exit 0 with an empty
  `git status --porcelain src/shared/store/migrations`. If `db:drift` writes a file, delete that file
  and report it as a defect, never as a change.
- No dependency moves: the pins in `package.json` (`@modelcontextprotocol/sdk` 1.32.2,
  `drizzle-orm` 0.45.4, `web-tree-sitter` 0.27.2, `tree-sitter-wasms` 0.1.13) stay as they are.
- `ToolContext` stays a **type-only, connection-free seam**: a handler still never opens a database,
  never imports `drizzle-orm`, never reads `process.env`. `dbSizeBytes` is a value handed in by the
  composition root; a handler must not re-derive it with `statSync` when it is present.
- `src/shared/store/read/context.ts` keeps importing nothing from `src/tools/**` or `src/index.ts`
  (asserted by `tests/read-files.test.ts`).
- Module rules that `tests/boundary.test.ts` enforces stay satisfied: `tool-isolation`,
  `indexer-isolation`, `shared-isolation`, `drizzle-location`, `root-privileges`.
- The store's open-time policy is preserved: the file is fingerprinted **before** `openReader`
  (`src/index.ts:436-441`), `sameFile` keeps comparing `{ino, size, mtimeMs}`, and a refused refresh
  keeps the previous reader with `staleReason` (`src/index.ts:493-542`,
  `tests/store-refresh.test.ts:198-220`). Do not add a `Store` member and do not stat the file after
  opening it.
- No new behaviour in any tool other than `status`; the other six handlers keep their contracts.

## Documentation and tests

- Behaviour a doc specifies is changed in the same commit: the `ToolContext` field table
  (`docs/my-product/TECH-SPEC.md:358-366`) and the field list in `.agents/prompts/rules.md:24-25` are
  part of this change. `docs/my-product/TOOLS-SPEC.md` is **not** edited (its only `size_bytes`
  occurrence at `:257` is an example value, not a rule).
- **Commit scheduling, so the same-commit rule is satisfied across the phase split.** No task runs a
  git write command; the orchestrator commits once per phase. Phase 0 documents only the seam itself
  (the field and what it means), nothing in phase 0 asserts how `status` derives `size_bytes`, and no
  document specifies that derivation — verified: the only `size_bytes` occurrence in
  `docs/my-product/` is the §2 example value at `TOOLS-SPEC.md:257`. So `AGENTS.md`'s "documentation
  in the same commit" rule is not violated by documenting the field in phase 0 and the payload
  behaviour in phase 1.
- Never weaken an assertion to make a test pass. `tests/tools-status.test.ts:489`
  (`size_bytes: DB_BYTES`) stays and keeps proving the stat fallback; the new case proves the
  recorded value wins.
- Tests are hermetic: `mkdtemp`, `SANGKAR_HOME`/`SANGKAR_CURSOR_KEY` set explicitly, no network, no
  writes to this repository, cleanup in `afterAll`.
- No new test file is created and no existing test file is renamed or re-scoped, so
  `.agents/prompts/test-map.md` is **not** edited: the repo rule there fires on *add, rename, delete
  or re-scope*, and this plan only adds cases to two files named in its `files`. (That map has no row
  for the store-refresh decision today; closing that gap is out of scope and recorded as R6 in
  `plan.risk.md`.)

## Ownership

- A task modifies only the paths in its `files`. `src/index.ts`, `src/shared/store/read/context.ts`
  and `src/tools/status/handler.ts` are each owned by exactly one task in exactly one phase.
- A worker that needs another file stops and reports it; the orchestrator re-plans, it does not
  widen a task after dispatch.
