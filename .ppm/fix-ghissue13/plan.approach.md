# Approach

## Current state (verified at `dc5a548`)

- `src/tools/status/handler.ts:161` and `:230-231` take `nodes`/`edges` from
  `ctx.reader.latestGeneration()` — the served generation. `:232` takes
  `size_bytes: dbSizeBytes(ctx, dbPath)` from a live `statSync(join(ctx.config.dataDir, "graph.db"))`
  (`:486-494`), so one payload describes two files.
- The disagreement is persistent, not a microsecond window. `refreshStore` (`src/index.ts:493-542`)
  keeps the previous reader when a newer file cannot be served (`staleReason: "a newer generation
  could not be opened"`, asserted at `tests/store-refresh.test.ts:198-220`), so every following
  `status` publishes the old generation's `nodes` beside the *unserved* file's size. Orchestrator
  reproduction: `nodes: 11` (served `g-aaaaaa`, 172 032 bytes at open) with `size_bytes: 720 896` (a
  newer-schema file on disk). `tests/store-refresh.test.ts` publishes files of exactly 172 032 bytes.
- The recording the issue asks for already exists: `fingerprintOf` (`src/index.ts:314-325`) returns
  `{ino, size, mtimeMs}`, `openStore` fingerprints **before** `openReader` (`src/index.ts:436-441`,
  with the comment explaining why the print must not be stamped after the open), and the `ready`
  variant of `Store` carries it (`:296-305`).
- `buildContext` (`src/index.ts:407-417`) is called three times: `:472` (`openStore`, where the local
  `fingerprint` is in scope), `:521` (`refreshStore`'s replacement, where `now` is in scope), `:978`
  (`openFor`, which re-builds the context around `store.context.reader`). The CLI spreads
  `store.context` (`:1198`), so one edit reaches both paths.
- The seam is documented as five fields in three places: `src/shared/store/read/context.ts:39` ("the
  five fields are the entire seam") plus the module-header sentence at `:4-6` (the clause naming
  "the reader, the cursor pair, the logger, the clock and the resolved config" is `:5`),
  `docs/my-product/TECH-SPEC.md:358-366` (the three-column `ToolContext` table: field | type | why it
  is injected rather than imported), and `.agents/prompts/rules.md:24-25` (the bullet at `:24`, its
  field list on `:25`: `(reader, cursor, log, now, config)`).
- Hand-written contexts live in `tests/tools-status.test.ts:299-308`, `tests/read-files.test.ts:714-720`
  and six other `tests/tools-*.test.ts` files; `tests/tools-status.test.ts:39-51` writes a real
  2 048-byte `graph.db` and `:489` asserts `size_bytes: DB_BYTES` against a context that will not
  carry the new field — which is exactly the fallback this plan keeps.
- Environment: `bun` 1.4.4 on `PATH`; baseline `typecheck`/`lint` clean, `bun test` 903 pass / 0
  fail (55 s), `tests/tools-status.test.ts` 25 pass, `tests/store-refresh.test.ts` 7 pass.

## Design

- **D1 — record at open, publish from the record.** `ToolContext.dbSizeBytes` is
  `FileFingerprint.size` as taken before `openReader`. Rationale: the fingerprint is the store's own
  statement about the file it opened, so no new stat, no `/proc/self/fd` portability problem, and no
  extra syscall on the hot path. A live `stat` in the handler is the defect.
- **D2 — optional sixth field, not a required one.** `readonly dbSizeBytes?: number | null`. Required
  would break eight hand-written `ToolContext` literals in `tests/` for no gain, and the field's
  absence has an honest meaning ("this context never opened a file"). `number | null` distinguishes
  "no file served" (`null`, the unreadable-reader context at `src/index.ts:977`) from "not recorded"
  (`undefined`, an old fake).
- **D3 — the stat survives as the fallback, in the handler only.** `sizeBytesOf(ctx, dbPath)` returns
  `ctx.dbSizeBytes` when `typeof recorded === "number"`, else today's `dbSizeBytes(ctx, dbPath)`
  including its `0` plus `status.db_size_unavailable` warn. Rationale: `tests/tools-status.test.ts:489`
  must keep proving the fallback, and a fake context built from `#shared/*` alone must still render a
  payload.
- **D4 — `docs/my-product/TOOLS-SPEC.md` is not edited.** Its single `size_bytes` occurrence (`:257`)
  is a value inside the §2 example payload; §2 has no row or sentence that defines how the number is
  obtained (checked: `grep -n "bytes" docs/my-product/TOOLS-SPEC.md` yields only the example and
  unrelated prose about message sizes). The field's key, type and unit do not change, so nothing the
  spec specifies becomes false. Editing it would create a contract sentence the owner did not ask
  for. Rejected alternative: adding a "captured at open" note to §2 (no consumer asked, and
  §1.1's payload contract is about keys).
- **D5 — one integration file, extended rather than duplicated.** The integration cases go into
  `tests/store-refresh.test.ts`, which already owns the fixture (`mkdtemp` + `SANGKAR_HOME` +
  `configFor` + `publish` + `withStage`/`migrate`/`beginWrite`) and already imports `openStore`,
  `refreshStore` and `fingerprintOf`. A new `tests/status-size.test.ts` would duplicate ~80 lines of
  fixture and force a `test-map.md` row for no coverage gain. The `publish` helper gains an options
  argument (padding bytes, `schema_version` offset) whose default reproduces today's behaviour, so
  the seven existing cases are untouched.
- **D6 — the assertion that makes the case real.** Each new case asserts a strict `not.toBe` between
  the recorded size and `statSync(dbPath).size`, so a regression to a live stat fails, and a fixture
  that failed to grow fails as a vacuous test instead of passing silently.

## Phases

### phase_0: Context seam and composition-root plumbing

Deliverable: `ToolContext.dbSizeBytes` exists and is filled with the store fingerprint's size on every
path that builds a context (`openStore`, `refreshStore`, `openFor`, and therefore the CLI's spread),
and the three places that document the seam name the sixth field. Sync barrier because phase 1's
handler reads the field and its tests type-check against it: without the seam and the plumbing, the
handler change cannot compile and the integration cases cannot observe a recorded size.

Why these are separate tasks: `src/shared/store/read/context.ts` (the type) must exist before
`src/index.ts` can pass the argument, and `src/index.ts` is the hotspot this phase serialises behind
the type; the documentation task depends only on the type, so it runs beside the plumbing.

### phase_1: The handler publishes the served file's size, and the tests prove it

Deliverable: `status.payload.graph.size_bytes` is the size the reader opened — for an accepted swap,
for a publish that lands after the open, and for a refused swap where the divergence used to be
permanent (issue #13's two acceptance criteria, plus the regression case the existing suite lacks).
Sync barrier because the phase gate runs the full suite, `db:drift` and the live-file check, which
are the evidence the plan's Done-when conditions rest on.

Why these are separate tasks: the handler plus its unit case is one concern in two files; the
integration cases need the handler's preference rule in place to pass, so they are `pre_request`-ed on
it rather than being written against a behaviour that does not exist yet.

The phase closes with two gates, not one: `TASK-007` proves the tree (full suite, `db:drift`, the
live-file check), and `TASK-008` — the last task of the last phase, `pre_request: [TASK-007]` —
publishes that evidence as one comment on GitHub issue #13 and closes it, or names the criterion still
unmet and leaves it open. Both write no repository file. The pattern is deliberate: an executed plan
that does not answer its issue leaves the tracker and the tree disagreeing about what shipped.
