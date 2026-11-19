## Objective

Close the three issues that fully survived verification against `ed16170` — the ambiguity contract (#3), the unresolved-edge taxonomy (#5), and three sub-items of the provenance bundle (9.1 repository identity, 9.3 `expand` source range/outline, 9.4 runtime preflight) — plus the three issues split out of the review (#10 vacuous test-coverage driver, #11 renames and removed files in the change set, #12 import failures recorded as one kind). Each of these makes an answer state something it cannot currently back: two different candidate totals, an unexplained unresolved count, a null project identity, a source window no caller can narrow, a startup failure with no diagnostic, a coverage claim over zero symbols, a rename that empties the diff, and an import taxonomy that cannot tell ambiguity from dynamism.

## Scope

In:

- #3 `trace`/`expand` accept a `path` filter with `search`'s prefix semantics; ambiguity reports `candidates_total` (matched) beside `candidates_shown` (returned) and one consistent total in every `why`; each candidate carries a `next_call`.
- #5 every answering payload carries an unresolved census — `{total, material, buckets}` — beside the unchanged `coverage.unresolved_edges`; `trace` filters by bucket; a builtin allowlist collapses the receiver/member tail.
- 9.1 `meta` records `project_name`, `project_root`, `project_branch`, `project_remote` at publish time; `status.payload.project` reports them; the `store.identity_unrecorded` warning stops firing on a normally indexed store.
- 9.3 `expand what=source` accepts `range: [from, to]`; `expand what=outline` returns members with signatures and line numbers.
- 9.4 an unsupported or missing runtime prints one JSON diagnostic on stderr naming `Bun >= 1.4.4` and the resolved interpreter.
- #10 an empty change set emits no test-coverage driver and reports `test_gaps_total: null` rather than `0`.
- #11 the change set carries a per-entry `state` (`modified | added | deleted | renamed`) with a `from` path, survives a rename instead of degrading, and lists removed paths in `changed_files`.
- #12 import failures are recorded by cause — `ambiguous` with the competing paths, `missing_module` for a relative specifier that matches nothing, `dynamic` only for a specifier that cannot be static.

Out:

- #1 (reader never re-opens), #2 (test census), #4 (`skipped` scope disagreement), #7 (untracked files), #6 (importer-relative and `..` import resolution), #8 (per-argument descriptions). All are confirmed and their verdicts are posted on the issues; each needs its own plan. #12 makes #6's rows countable but does not fix #6.
- 9.2 (compact `read_status`) and 9.5 (worktree registration snippet).
- Any schema migration. `meta` is key/value, `unresolved.evidence_kind` is an unconstrained text column, and `ChangeSet` is an in-memory type, so this plan adds no column and no migration.

## Sources

- `docs/my-product/TOOLS-SPEC.md` — per-tool arguments, payload keys, budgets.
- `docs/my-product/DESIGN.md` §6, §8 — envelope contract, resolution contract.
- `docs/my-product/DATABASE.md` §3.1, §5, §7 — `meta` keys, publish path, absence semantics.
- `AGENTS.md` — module rules, definition of done, conventions.
- https://github.com/Alfar-Lab/sangkar-mcp/issues/3
- https://github.com/Alfar-Lab/sangkar-mcp/issues/5
- https://github.com/Alfar-Lab/sangkar-mcp/issues/9
- https://github.com/Alfar-Lab/sangkar-mcp/issues/10
- https://github.com/Alfar-Lab/sangkar-mcp/issues/11
- https://github.com/Alfar-Lab/sangkar-mcp/issues/12

## Done when

- `~/.bun/bin/bun run typecheck && ~/.bun/bin/bun run lint && ~/.bun/bin/bun run test && ~/.bun/bin/bun run db:drift` all exit 0.
- `trace 'messageOf' --json` on an ambiguous name reports one total: `candidates_total` equals the number named in every `why`, `candidates_shown` equals `candidates.length`, and each candidate has a `next_call`.
- `trace 'register' --path src/server/... / --path tests` narrows candidates exactly as `search --path` narrows hits.
- `status --json` reports non-null `project.root` and `project.branch`, and no `store.identity_unrecorded` line on stderr for a store indexed by this build.
- On this repository, `coverage.unresolved_edges` (9166 at `g-d975f0`) is accompanied by buckets summing to it and an `unresolved_material` value that is far smaller, with the per-file unresolved entries in `trace` each carrying a bucket.
- `expand <path> --what source --range 10:20` returns the clamped sub-span, and `expand <path> --what outline` lists members with signatures and line numbers.
- A CLI run with `BUN_VERSION` forced below the floor (or a non-Bun runtime) writes exactly one JSON line to stderr naming `Bun >= 1.4.4` and exits non-zero, with stdout untouched.
- On a clean tree, no `summary` risk driver mentions tests or coverage, and `totals.test_gaps_total` is `null`; with changes present it is a number again.
- A rename appears in `changed_files` as one entry with `state: "renamed"` and `from`, the rest of the change set survives it, and deleting a tracked file lists the path with `state: "deleted"` while `removed_symbols` still counts its symbols.
- After a re-index, `SELECT evidence_kind, count(*) FROM unresolved GROUP BY 1` contains an `ambiguous` population that includes import rows, and the rows that matched nothing relative are `missing_module` rather than `dynamic`.
