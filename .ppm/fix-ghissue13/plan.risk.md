# Risks and open questions

## Risks

- **R1 — the fingerprint is taken before the handle is opened (residual race, accepted).**
  `openStore` and `refreshStore` call `fingerprintOf` and then `openReader` (`src/index.ts:436-441`,
  `:493-505`). A publish that renames a new file into `graph.db` between those two calls leaves the
  recorded `size` describing the *pre-rename* file while the handle reads the post-rename one.
  Mitigation: none available that keeps the field cheap and the refresh correct — the pre-open print
  is what makes a swapped-in file refresh on the next call (stamping after the open would leave the
  process serving an older graph until the following change); a post-open `stat` of the path is the
  bug being fixed; `statSync("/proc/self/fd/n")` is not portable (the issue says so itself).
  So the plan states the residual honestly in `ToolContext`'s doc comment and claims only what is
  true: **the number is the size recorded for the file this reader opened, not a live measurement of
  the path**. The payload must never be described as proving the exact inode.
- **R2 — an in-place rewrite of the live file can still disagree, in the other direction.**
  A publish always renames a new inode (`src/shared/store/publish.ts`), which an open handle cannot
  see; but a file updated *in place* (the trick `tests/store-refresh.test.ts:198-220` uses for the
  newer-schema case) is visible to the open handle, so `generation.nodes` could move while the
  open-time `size` stays behind. Mitigation: production never writes the live file
  (`publish.ts`'s stage → seal → rename is the only path); the schema-undercount reads as
  `read_status.stale` with `staleReason` set, which is the envelope's own mechanism for saying "this
  answer is about an older file". Not fixed here — fixing it would mean statting after open, which
  reintroduces #13.
- **R3 — dropping `size_bytes` was considered and rejected by the owner.** Issue #13 allowed either
  fix. Dropping it would break `src/shared/cli/render.ts:50` (prints `size_bytes` bytes),
  `tests/cli-render.test.ts:49` and `:143`, `tests/tools-status.test.ts:489`, and the §2 example in
  `docs/my-product/TOOLS-SPEC.md:257`, for a field the CLI and every operator already read. No task
  may remove, rename or re-type it.
- **R4 — `size_bytes` stays a number of bytes.** Not logical pages, not MiB, not a string. Changing
  the unit would falsify `render.ts:50`'s "… bytes" suffix and the spec example, and would be a
  silent break for any consumer doing arithmetic on it. The recorded value is
  `FileFingerprint.size`, which is `statSync().size`, so the unit is preserved by construction.
- **R5 — the CLI inherits the field through a spread, not a call site.** `src/index.ts:1198`
  (`{ ...store.context, log: scopedLog(name) }`) is the only CLI plumbing. If a later refactor stops
  spreading `store.context`, the CLI silently loses the value and falls back to a `stat` — for the
  CLI that is the *more* correct answer anyway (it opens the file per invocation and never holds a
  stale handle), so the failure mode is benign, but the plan records the path so a reviewer checks it.
  Same for `tests/` fakes: absence is legal by design (D2) and renders the fallback.
- **R6 — `tests/store-refresh.test.ts`'s new `publish` options could change the existing cases.**
  The helper is shared with seven passing tests. Mitigation: the options' defaults must reproduce
  today's SQL and row set exactly; the phase-1 gate re-runs the file's original seven cases, which
  must stay green and unmodified.
- **R7 — `.agents/prompts/test-map.md` has no row for the store-refresh decision.** That is a
  pre-existing gap, not one this change widens (no test file is added, renamed or re-scoped), so it
  is left alone; the same file's rows for "one tool's payload or defaults" already name
  `tools-status.test.ts`.
- **R8 — `bun run db:drift` is a generator.** It runs `drizzle-kit generate --name drift-check`, which
  writes a migration file when it detects drift. No schema file is touched by this plan, so it must
  be a no-op; a phase-1 gate that sees a new file under `src/shared/store/migrations/` deletes it and
  reports a defect instead of committing it.

## Open questions

- **Q1 (non-blocking, owner).** Should `docs/my-product/TOOLS-SPEC.md` §2 gain one sentence saying
  `graph.size_bytes` is the size captured when the reader opened its file? D4 says no: §2 only shows
  the value in its example, no prose there defines how it is obtained, and the key, type and unit are
  unchanged. The plan proceeds without the edit; adding the sentence later is a one-line change.
- **Q2 (non-blocking, owner).** Should `tests/read-files.test.ts` assert explicitly that a
  `ToolContext` literal without `dbSizeBytes` still type-checks? Its `:714-720` literal already
  exercises it (any required field would fail `bun run typecheck`), so the plan adds no assertion.
- **Q3 (blocking).** None. The owner's decision, the field's shape, the fallback and the test
  placement are all resolved above.
