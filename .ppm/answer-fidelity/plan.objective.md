## Objective

Close the three issues that fully survived verification against `ed16170` — the ambiguity contract (#3), the unresolved-edge taxonomy (#5), and three sub-items of the provenance bundle (9.1 repository identity, 9.3 `expand` source range/outline, 9.4 runtime preflight). Each of these makes an answer state something it cannot currently back: two different candidate totals, an unexplained unresolved count, a null project identity, a source window no caller can narrow, and a startup failure with no diagnostic.

## Scope

In:

- #3 `trace`/`expand` accept a `path` filter with `search`'s prefix semantics; ambiguity reports `candidates_total` (matched) beside `candidates_shown` (returned) and one consistent total in every `why`; each candidate carries a `next_call`.
- #5 every answering payload carries an unresolved census — `{total, material, buckets}` — beside the unchanged `coverage.unresolved_edges`; `trace` filters by bucket; a builtin allowlist collapses the receiver/member tail.
- 9.1 `meta` records `project_name`, `project_root`, `project_branch`, `project_remote` at publish time; `status.payload.project` reports them; the `store.identity_unrecorded` warning stops firing on a normally indexed store.
- 9.3 `expand what=source` accepts `range: [from, to]`; `expand what=outline` returns members with signatures and line numbers.
- 9.4 an unsupported or missing runtime prints one JSON diagnostic on stderr naming `Bun >= 1.4.4` and the resolved interpreter.

Out:

- The defects that are real but were diagnosed wrongly in their issues: #1 (reader never re-opens), #2 (vacuous driver on an empty change set), #4 (`skipped` scope disagreement), #7 (untracked files). Verdicts are posted on the issues; they need their own plans.
- #6 (importer-relative and `..` import resolution) and #8 (per-argument descriptions), both re-scoped on their issues.
- 9.2 (compact `read_status`) and 9.5 (worktree registration snippet).
- Any schema migration. `meta` is key/value and `unresolved` already carries `evidence_kind` and `evidence`, so this plan adds no column and no migration.

## Sources

- `docs/my-product/TOOLS-SPEC.md` — per-tool arguments, payload keys, budgets.
- `docs/my-product/DESIGN.md` §6, §8 — envelope contract, resolution contract.
- `docs/my-product/DATABASE.md` §3.1, §5, §7 — `meta` keys, publish path, absence semantics.
- `AGENTS.md` — module rules, definition of done, conventions.
- https://github.com/Alfar-Lab/sangkar-mcp/issues/3
- https://github.com/Alfar-Lab/sangkar-mcp/issues/5
- https://github.com/Alfar-Lab/sangkar-mcp/issues/9

## Done when

- `~/.bun/bin/bun run typecheck && ~/.bun/bin/bun run lint && ~/.bun/bin/bun run test && ~/.bun/bin/bun run db:drift` all exit 0.
- `trace 'messageOf' --json` on an ambiguous name reports one total: `candidates_total` equals the number named in every `why`, `candidates_shown` equals `candidates.length`, and each candidate has a `next_call`.
- `trace 'register' --path src/server/... / --path tests` narrows candidates exactly as `search --path` narrows hits.
- `status --json` reports non-null `project.root` and `project.branch`, and no `store.identity_unrecorded` line on stderr for a store indexed by this build.
- On this repository, `coverage.unresolved_edges` (9166 at `g-d975f0`) is accompanied by buckets summing to it and an `unresolved_material` value that is far smaller, with the per-file unresolved entries in `trace` each carrying a bucket.
- `expand <path> --what source --range 10:20` returns the clamped sub-span, and `expand <path> --what outline` lists members with signatures and line numbers.
- A CLI run with `BUN_VERSION` forced below the floor (or a non-Bun runtime) writes exactly one JSON line to stderr naming `Bun >= 1.4.4` and exits non-zero, with stdout untouched.
