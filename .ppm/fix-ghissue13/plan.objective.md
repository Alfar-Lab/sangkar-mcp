# Objective

`status.payload.graph.size_bytes` must describe the database file the reader actually opened, not
whatever file sits at `graph.db` when the handler runs. Today `nodes`/`edges` come from the served
generation (`src/tools/status/handler.ts:161`, `:230-231`) while `size_bytes` is a live `statSync`
of the path (`:232`), so after a refused swap the payload publishes the served generation's counts
beside the size of a file that was never served — permanently, not for a microsecond.

## Scope

In:

- `ToolContext` (`src/shared/store/read/context.ts:49-55`) gains an optional sixth field
  `readonly dbSizeBytes?: number | null`, documented as the size of the file the reader opened,
  captured at open time.
- `buildContext` (`src/index.ts:407-417`) receives the value from the fingerprint the store already
  keeps (`FileFingerprint.size`, `src/index.ts:289-293`, `fingerprintOf` `:314-325`), at its three
  call sites: `:472` (`openStore`), `:521` (`refreshStore`), `:978` (`openFor`). The CLI path is
  `{ ...store.context, log }` (`src/index.ts:1198`) and inherits the field with no second edit.
- `status` handler (`src/tools/status/handler.ts:232`) prefers the recorded size when it is a
  number and keeps the existing stat (with its `0` plus `status.db_size_unavailable` warning) as the
  fallback for a hand-written context, so `tests/tools-status.test.ts:489` still means what it says.
- Header comment `src/tools/status/handler.ts:19-21` and the helper doc, which currently call the
  size "a `stat` of that file".
- The seam documentation that enumerates the field list: `docs/my-product/TECH-SPEC.md:358-366`
  (the three-column `ToolContext` table: field | type | why it is injected) and
  `.agents/prompts/rules.md:24-25` (the bullet at `:24`, its list on `:25`: `(reader, cursor, log,
  now, config)`).
- Tests: one unit case proving the recorded size wins over the file on disk, and one integration
  case set proving both the accepted swap and the refused swap.

Out:

- Any change to the payload's key set, to `payload.graph`'s four members, or to the envelope.
- Any schema, migration, table or column change (`db:drift` stays clean; no `migrations/**` edit).
- `docs/my-product/TOOLS-SPEC.md`: its only `size_bytes` occurrence (`:257`) is an example value and
  the field's meaning (bytes of the served graph) is unchanged, so no prose there defines behaviour
  to correct. See `plan.approach.md` D4.
- Opening the database per call, statting `/proc/self/fd/n`, or adding a `Store` field: the store
  already carries the fingerprint, which is the recording the issue asks for.
- Dropping `size_bytes` — considered and rejected by the owner (`plan.risk.md` R3).

## Sources

- GitHub issue `Alfar-Lab/sangkar-mcp#13` (open) — the defect, its scope and its two acceptance
  criteria.
- `.ppm/issue-remediation/plan.approach.md` D1 — the refresh-on-generation-change design this plan
  completes (issue #1 AC4: "both derive from the served generation, or drop `size_bytes`").
- `src/index.ts:289-336` (`FileFingerprint`, `fingerprintOf`, `sameFile`), `:436-441` (the
  fingerprint-before-open comment), `:493-560` (`refreshStore`, including the refusal path).
- `src/tools/status/handler.ts:128` (`size_bytes: number`), `:232`, `:486-494`.
- `docs/my-product/TECH-SPEC.md` §6 (`ToolContext`, the composition root), `AGENTS.md`,
  `.agents/prompts/rules.md`, `.agents/prompts/project-layout.md`.

## Done when

Every command runs from the repository root `/home/ljosalfar-server/Documents/researchs/codegraph-mcp`
with `bun` on `PATH` (verified: `bun` 1.4.4 at `~/.local/bin/bun`).

1. `bun run typecheck` exits 0 and `bun run lint` exits 0.
2. `bun test` is green: baseline 903 pass / 0 fail, plus the new cases (report the new total).
3. `bun test tests/store-refresh.test.ts` contains and passes a case where a second generation is
   published between two `status` calls and the reported `size_bytes` is the size the reader opened.
4. The refused-swap case holds: after a newer-schema file is published and the refresh refuses,
   `status` still reports the served file's size while `statSync(graph.db).size` is the newer,
   unserved file's — asserted as `size_bytes` `toBe(served)` and `statSync(dbPath).size`
   `toBeGreaterThan(served)`, so a live stat would fail the case.
5. `bun run db:drift` exits 0 with `git status --porcelain src/shared/store/migrations` empty.
6. `.sangkar/graph.db` is byte- and mtime-identical to the state before the work (no task writes it).
7. `git status --porcelain` shows **no** modified path outside the plan's own files: tracked edits
   must be exactly `src/shared/store/read/context.ts`, `src/index.ts`,
   `src/tools/status/handler.ts`, `tests/tools-status.test.ts`, `tests/store-refresh.test.ts`,
   `docs/my-product/TECH-SPEC.md`, `.agents/prompts/rules.md`, and the only `??` entries allowed are
   `?? .ppm/fix-ghissue13/` and `?? .ppm/fix-ghissue14/` — both untracked plan directories
   (`git status` reports an untracked directory once, not per file). The sibling
   `.ppm/fix-ghissue14/` is another planner's pre-existing directory, out of scope for this plan;
   any other path, or a modification inside either plan directory by this work, is a mismatch that is
   **reported, never repaired**.
8. `gh issue view 13 --repo Alfar-Lab/sangkar-mcp --json state,comments` shows one new comment
   carrying the commit, the changed files, both acceptance criteria with their evidence and the test
   counts, and the issue is `CLOSED` — or the comment names the criterion still unmet and the issue
   stays `OPEN`. `TASK-008` (the last task of the last phase) owns this; it writes no repository file.
