# Approach

## Current state

Verified at `58a2103` with a fresh index in `/tmp/probe-graph` (`g-97e8be`, 108 files, 1594 nodes,
8633 edges) and a live stdio server. Line numbers are from that commit.

- **#1.** `src/index.ts:305-308` states the invariant ("the handle is opened once, here, and never
  re-opened"); `openStore` is called once in `runMcp` (`src/index.ts:656`). Probe: server answered
  `g-97e8be` on both sides of a CLI publish that produced `g-841614`. `ENVELOPE_STATUSES`
  (`src/shared/envelope.ts:15-21`) has no `decisive`; `checkFreshness` is called only from
  `src/tools/status/handler.ts:189`. `status.payload.graph.size_bytes` is a live `statSync`
  (`src/tools/status/handler.ts:216`) beside a pinned generation row. `meta.indexer_state` is only
  ever `idle` (`src/indexer/worker.ts:890`) or `failed` (`:1047`), while
  `src/shared/domain/freshness.ts:54` waits for `running|busy`, which nothing writes.
- **#4.** `src/tools/status/handler.ts:170-184` computes `skipped = censusFiles - files.length`
  (repo-wide) and fills `not_indexed` from `rowsForPaths(requested)`, which returns `{[], []}` when
  no paths are requested (`src/shared/store/read/coverage.ts:117-119`). Measured at a fresh index:
  `status` → `skipped: 107, not_indexed: []`; `summary` → `skipped: 4, not_indexed: [4 rows]`;
  `SELECT count(*) FROM not_indexed` → 107. `not_indexed_total` exists only in
  `src/tools/summary/handler.ts:645` and `src/tools/expand/handler.ts:1117`. The scoped definition
  already exists and is unused by `status`: `src/shared/domain/coverage.ts:99-116` (`assemble`).
- **#2.** `Coverage` (`src/shared/envelope.ts:54-62`) has no test dimension; `impact` prints
  `"… 0 of N covered by tests."` (`src/tools/impact/handler.ts:352`) with no way to say why.
  `tests/risk.ts` drivers are already guarded on an empty change set (`src/shared/domain/risk.ts:120`),
  so #10 does not resurface.
- **#6.** `symbolIndex.moduleCandidates` (`src/indexer/resolve.ts:229-264`) matches repo-wide stems
  and has no importer argument; `modulePath` (`:723-734`) keeps `..` because `normalizeRel`
  (`src/shared/paths.ts:5-19`) deliberately does. Measured on a fresh index of this repository: 71 of
  117 relative specifiers unresolved, 62 climbing with `..`, 9 defeated by a duplicate basename, 0
  unexplained, and **all 71 targets exist on disk**. The store already labels them (`missing_module`
  62, `ambiguous` 9 — `src/indexer/resolve.ts:636-664`). The importer path is available at every bind
  site: `BindContext.file` (`resolve.ts:306-307`, `:331-336`), `importHits(file, …)` (`:492-514`).
- **#7.** `src/shared/domain/diff.ts` runs exactly one git command (`gitNameStatus`, `:169-241`);
  `ChangeState` (`:47`) is `modified|added|deleted|renamed`. Probe: `src/__untracked_probe.ts`
  created and shown as `??` by git left `changed_files_total: 49` and no trace in either payload.
  `summary` has no `unmatched_files` field at all (`buildChange`, `src/tools/summary/handler.ts:804`).
  The precedent is `src/indexer/watch.ts:246` (`git status --porcelain`, no parser — the text is a
  signature key). Verified locally against git 2.53.0: `git status` **rejects** `--relative` and
  `--no-ext-diff`, accepts the rest of the prologue, and its `-z` v1 shape is `XY<SP><path>\0` with
  rename pairs reversed — `parseNameStatus` (`:259-284`) cannot read it.
- **#8.** Live `tools/list`: 7 tools, 32 properties, 2 with `description` (`index.force`,
  `index.mode`), 0 with examples. `propertySchemaOf` (`src/index.ts:173-192`) maps
  `enum/min/max/default/description`; `itemSchemaOf` (`:163-166`) hardcodes `items.type = "string"`,
  so `expand.range`'s integer items are published as strings. Four exact-shape `toEqual` assertions
  over a property in `tests/mcp-surface.test.ts` will break when descriptions land — by design:
  `:230` (`query`), `:232` (`depth`), `:238` (`direction`), `:245` (`paths`).
- **#9.** Items 1, 3, 4 are shipped (`meta.project_*` written at `src/indexer/persist.ts:179-182`,
  read at `src/shared/store/read/reader.ts:419-422`; `expand` `what: "outline"` and `range` at
  `src/tools/expand/manifest.ts:45-47`; `guardRuntime` at `bin/sangkar.ts:11`). Missing: a compact
  `read_status` (item 2) and any `mcpServers` snippet (item 5 — zero hits for `mcpServers`,
  `mcp add`, `claude_desktop` in `README.md` and `docs/`).

## Design

- **D1 (#1) — re-open on generation change, not on every call.** The store holder keeps a fingerprint
  of the file it opened (`statSync(dbPath)` → `ino`, `size`, `mtimeMs`; publish renames a new inode
  over the live path, so `ino` changes). `callTool` refreshes before the absent/refused
  short-circuits, so the decision is made from the file, not from the store variant: an `absent`
  store becomes `ready` on the first call after a publish, and an unchanged fingerprint costs one
  `stat`. On change: `openReader` → `readSchemaVersion` → `guardRoot` → `makeReader` → swap, and
  close the previous handle. On failure the previous reader keeps serving with `stale_reason` naming
  the newer file. Rejected: opening per call (the prepared-statement cache and the pragmas are per
  handle; `acceptance` measures the prepared path against a per-call builder), and a refuse-only
  design (issue AC2 requires the server to *report* the newer generation).
- **D2 (#1 AC3) — `read_status.decisive`, filled in one place.** `decisive = !stale`, written by
  `route()` and `uncheckedReadStatus()` in `src/index.ts` after the handler answered; no handler sets
  it, so `stale && decisive` is structurally impossible. Rejected: a tenth envelope field (breaks the
  nine-field contract and `tests/contract.test.ts`), and a per-tool computation (seven copies of one
  truth).
- **D3 (#4) — one definition of the two numbers, scoped like the list.** `coverage.skipped` is the
  count of `not_indexed` rows in the scope the tool assembled (`assemble` already does exactly that
  for `summary`; `status` derives the same number from the same reader methods, because its scope is
  "the requested paths, or the whole graph" and `assemble` has no vocabulary for that);
  `totals.not_indexed_total` is the one repo-wide figure, read from a single
  `SELECT count(*) FROM not_indexed`, so both tools report the same total for the same generation and
  `coverage.not_indexed.length` is the shown page. `status` with no paths scopes
  to the repository (`reader.notIndexedAll()`), so its list is populated and budget-trimmed with
  `truncated: true` — the same shape `summary` already produces. No `not_indexed_shown` field: the
  array length already is that number, and a second copy could disagree after `emit` trims.
  Rejected: renaming `skipped` (7 payloads, telemetry `coverage_skipped`,
  `tests/budget.test.ts:387`), a repo-wide `skipped` (that is the bug), and `rowsForPaths([])`
  meaning "everything" (an empty prefix does not describe a subtree, and `read-files.test.ts` pins
  the current empty answer).
- **D4 (#2) — `coverage.tests`.** `{nodes, languages: [{language, nodes}]}` from one prepared
  `SELECT f.language, COUNT(*) FROM node n JOIN file f ON f.id = n.file_id WHERE n.is_test = 1 GROUP BY
  f.language`, exposed as `GraphReader.testsCensus()` and emitted by `status` and `summary` only
  (the other five tools would pay a query for a field they do not render). `impact`'s coverage
  sentence gains a clause when `nodes === 0`. `test_gaps_total` stays a number.
- **D5 (#6) — importer first, then today's fallback.** New `#shared/paths` helper
  `resolveFrom(importer, specifier)` → a normalised repo-relative stem with `.`/`..` collapsed, or
  `null` when it escapes the root. `moduleCandidates(module, fromFile?)` tries the exact stem from
  `resolveFrom` first and falls back to the current repo-wide match; `moduleFile` forwards the
  parameter; `importHits` passes its `file`; `bindRaw` passes `context.file`. `normalizeRel` is
  untouched — stored paths keep `..` so an escaping path never looks contained. Rejected: changing
  `normalizeRel` (it would silently contain escaping paths and alter `node.qn`).
- **D6 (#7) — a second git read, in the same shape.** `gitStatusPorcelain` beside `gitNameStatus` with
  the accepted prologue, `status --porcelain -z --untracked-files=all`, its own parser, and a
  `rev-parse --show-prefix` step for a sub-directory root (porcelain paths are toplevel-relative).
  Only `??` records become entries, state `untracked`. The union drops an untracked path already
  reported `added`/`modified`/`renamed` by the diff, keeps `--no-ext-diff` unavailable rather
  than pretending otherwise, and counts an untracked path as freshness drift so `stale: false`
  cannot coexist with a change set that grew. `summary`'s payload does **not** gain
  `unmatched_files`: `TOOLS-SPEC` documents that field in §6 (`impact`) only, so an untracked path
  with no indexable language stays visible in `impact` and is a follow-up issue for `summary`, not a
  rider here.
- **D7 (#8) — descriptions per argument, one example per tool.** `description` on all 30 arguments
  that lack one; a module-level `example` export on each manifest (beside `input`, not inside
  `manifest` — that object is pinned by `as const satisfies { name, description }` in
  `src/tools/index/manifest.ts:20`), imported by the composition root and published as
  `inputSchema.examples = [example]`; `itemSchemaOf` honours `items.type` and `items.values`
  instead of hardcoding `string`. The manifests land **before** the plumbing task, because
  `src/index.ts:77-94` imports each manifest's exports explicitly and cannot import a name that does
  not exist yet. `docs/INSTRUCTIONS.md` is not touched (byte-pinned).
- **D8 (#9.2) — record the decision, do not add a compact form.** A fresh graph already emits three
  of the five `read_status` fields (`{"generation","indexed_at","stale":false}` in the probe); the
  ceiling is a few dozen tokens per answer while `next_tools` costs more per answer, and `read_status`
  is a public contract whose field set `DESIGN.md:166` already fixes at four. Deliverable: one
  paragraph in `TOOLS-SPEC.md` §1.2 and a closing note on the issue. If a compact form is wanted, it
  is a new issue with its own measurement, not a rider here.

## Phases

### phase_0: shared contract and primitives

The three changes every later task compiles against: `ReadStatus.decisive` and `Coverage.tests` in
`src/shared/envelope.ts`, the three new read primitives (`notIndexedAll`, `notIndexedCount`,
`testsCensus`), and the coverage assembly that publishes the repo-wide total beside a scoped count.
Serial for the type change, then two parallel slices on disjoint files. Nothing user-visible changes
in this phase, which is why it is a barrier: phase 1's five handler tasks each consume one of these
signatures.

### phase_1: the wrong-answer fixes

Dispatch one task per issue slice: the store re-open and `decisive` wiring in the composition root
(#1), `status` and `summary` adopting the shared coverage assembly plus the test census (#4, #2),
importer-relative import binding (#6), and untracked paths in the change set (#7). Disjoint file
ownership by construction; the only shared files (`src/index.ts`, the two handlers) belong to exactly
one task each. Closes with the `TOOLS-SPEC` sections these changes move (§1, §2, §4, the change-set
state vocabulary) and a full phase gate, because `acceptance`'s `eq`-legitimacy and unresolved
counts can move when binding improves.

### phase_2: tool schema text, examples, and the remaining docs

Phase 1 changed behaviour; this phase changes only what a client reads. The schema plumbing
(`itemSchemaOf`, `examples`) lands first because the manifest tasks write against it, then three
manifest slices (trace+search, summary+impact+status, expand+index), then the `tools/list` invariant
test and the docs: `TOOLS-SPEC` §3/§5/§6/§7/§8/§10 argument rows and example calls, `DESIGN` §5/§6,
`AGENTS.md`'s current-state numbers, and the README worktree registration snippet (#9.5). Barrier
because the closing test asserts over all seven manifests at once, which is only true after every
slice lands.
