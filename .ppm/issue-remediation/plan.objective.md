# Objective

Fix the seven open GitHub issues of `sangkar-mcp` so that an answer is always about the graph on
disk, every count is backed by the scope it names, a relative import binds to the file it points at,
a newly added file appears in the change set, and every published tool argument explains itself.
The issues were validated at `58a2103` (`/tmp/probe-graph`, fresh index, live stdio server); three of
the seven are wrong answers today, not polish.

## Scope

In (each item is the scope the issue already carries after its revision notes):

- **#1 (p0)** the MCP process serves its startup generation for its whole life, and a negative answer
  carries no staleness guard. Fix: re-open on generation change, plus one `read_status` field that
  makes a stale answer self-describing.
- **#4 (p1)** `status` returns a `skipped` count its own `not_indexed` array cannot back (107 vs `[]`
  on a fresh index) while `summary` reports 4. Fix: one scope-derived definition plus totals in both.
- **#2 (p1)** no test census, so "0 of N covered by tests" cannot be explained. Fix:
  `coverage.tests`.
- **#6 (p2)** relative import resolution is not importer-relative and `..` is never collapsed: 71 of
  117 relative imports in this repository never bind, although every target exists on disk. Fix:
  importer-first resolution with normalised `..`, then today's fallback.
- **#7 (p2)** untracked paths never enter the change set, so a new module looks like nothing changed.
  Fix: union `git status --porcelain` with the diff and label the new state `untracked`.
- **#8 (p2)** 30 of 32 published arguments carry no description and no tool publishes an example
  call; `expand.range` publishes `items: {"type": "string"}`. Fix the schema text and the item type.
- **#9 (p3)** only items 2 and 5 remain: no compact `read_status` form, and no two-worktree
  registration snippet. Item 2 is resolved as a recorded decision (see `plan.approach.md` D8);
  item 5 is a README change.

Out:

- The three gaps `AGENTS.md` lists as open work (438 `is_test` nodes but 7 `kind: "test"`; 344 class
  nodes producing 29 methods; binding recall for call sites). They are unfiled, and the last one is
  a receiver-typing problem that this plan's import fix does not address.
- Multi-root / a `project` argument (#9's rejected half), compact-envelope redesign, npm publishing,
  telemetry, and any schema or migration change.
- `docs/INSTRUCTIONS.md` and `INSTRUCTIONS_TEXT` (`tests/instructions.test.ts` pins them byte for
  byte; adding example calls there would be a third copy of the same text).

## Sources

- `.ppm/issue-remediation/plan.approach.md` (current state and decisions)
- `.ppm/issue-remediation/plan.constraint_invariant.md`
- `docs/my-product/TOOLS-SPEC.md` (payload contracts: §1.1, §1.2, §2, §4, §5, §6, §7, §10)
- `docs/my-product/DESIGN.md` (§5, §6, §8 — the freshness promise and the `read_status` shape)
- `docs/my-product/DATABASE.md` (§3.1, §5 — identity keys, one-reader-per-process)
- `AGENTS.md` (module rules, definition of done, conventions)
- GitHub issues #1, #2, #4, #6, #7, #8, #9 in `Alfar-Lab/sangkar-mcp`

## Done when

Every command below runs from the repository root; `bun` is not on `PATH`, so the binary is
`~/.bun/bin/bun`.

1. `~/.bun/bin/bun run typecheck && ~/.bun/bin/bun run lint && ~/.bun/bin/bun run db:drift` all exit 0.
2. `~/.bun/bin/bun run test` is green, with the new tests named in phase 1 and 2 present.
3. `~/.bun/bin/bun run acceptance` is green (latency, budgets, prepared-vs-builder ratio, and the
   `eq`-legitimacy criterion that import binding can move).
4. #1: with `mcp` running and a CLI `index` publishing a second generation, the server's next
   `status` reports the newer generation, and every answer carries `read_status.decisive`, which is
   `false` exactly when `read_status.stale` is `true` (never both `true`).
5. #4: `status --json` and `summary --json` on the same generation report an identical
   `totals.not_indexed_total`, and no answer contains a count with an empty list and no total.
6. #6: `SELECT count(*) FROM unresolved WHERE evidence LIKE 'import:%' AND raw_target LIKE '.%/'` on
   an index of this repository drops from 71 to 0, with the five regression cases green.
7. #7: one modified tracked file plus one new untracked file yields `changed_files_total: 2` with
   states `modified` and `untracked`.
8. #2: `status` and `summary` report `coverage.tests.nodes` and the languages behind it; `impact`
   says so when the corpus is empty instead of implying a coverage failure.
9. #8: a `tools/list` probe over stdio shows every property with an `enum` or a `description`, every
   tool with at least one example, and `expand.range` items typed `integer`.
10. #9: `README.md` shows a two-worktree `mcpServers` snippet, and `TOOLS-SPEC.md` §1.2 records the
    `read_status` compaction decision.
