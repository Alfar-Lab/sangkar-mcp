# Constraints and invariants

Rules every task must respect. Violating one is a defect, not a preference.

## Architecture (verified against `tests/boundary.test.ts` and `AGENTS.md`)

- Five module rules are a build failure; the ids are the exact strings in a violation message:
  `tool-isolation`, `indexer-isolation`, `shared-isolation`, `drizzle-location`, `root-privileges`
  (`tests/boundary.test.ts:17-31`, `RULES` at 220).
- Telemetry lives in `src/shared/telemetry/**`, so it may import `#shared/*` only. It must **never** import
  `src/tools/**` or `src/indexer/**` (`shared-isolation`), and it must never import `drizzle-orm`
  (`drizzle-location`).
- `src/index.ts` and `src/indexer/worker.ts` may import `#shared/telemetry/*`; a tool handler may not. No
  tool handler, manifest or payload is edited by any task in this plan — instrumentation happens at the
  composition root and in the worker, nowhere else.
- `tests/boundary.test.ts` walks every `.ts` file under `src` and `tests` (`walkTree`, line 288), so new
  files under those two trees are covered automatically. **Do not edit `boundary.test.ts`**, and do not add
  a fourth private alias. The walk does **not** cover `bin/`, so the new `bin/sangkar.ts` →
  `src/shared/telemetry/cli` edge is outside enforcement; it is legal today because `root-privileges`'
  else-branch only refuses `src/indexer/**` and a tool handler (`tests/boundary.test.ts:215`), and the edge
  points at `#shared`, which the bin may import anyway. That is acceptable while the bin's only new import
  is under `src/shared/**`; a bin import of the indexer or a tool handler would be legal to the checker and
  still wrong, so it is a review item, not an automated one.
- `bin/sangkar.ts` may import `#shared/*`: `tsconfig.json` maps `#shared/*` and `include` covers `bin`.

## Environment and consent

- `SANGKAR_TELEMETRY` is read in `src/shared/config.ts` **only** and exposed as
  `AppConfig["telemetry"]`. Semantics: unset or empty ⇒ enabled; the exact string `off` ⇒ disabled; any other
  value ⇒ enabled (matching `SANGKAR_NO_WATCH`'s "unset or empty means on" shape, line 595-597 of
  `src/index.ts`, without inventing a second vocabulary for "true").
- The one pre-existing exception stands: `src/index.ts:596` reads `SANGKAR_NO_WATCH`. No task adds a new
  `process.env` read anywhere else. The sweeps in TASK-007 and TASK-008 therefore compare against
  TASK-001's baseline instead of expecting two hits: `grep -rn "process.env" src/` already reports four files
  (`src/shared/config.ts:93`, `src/index.ts:642`, `src/indexer/watch.ts:342`,
  `src/tools/index/handler.ts:155`), and the invariant is "the same four, and `SANGKAR_TELEMETRY` only in
  `src/shared/config.ts`".
- Telemetry is local only: no network call in any task, no new dependency, no change to the pins in
  `package.json` (`@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4, `web-tree-sitter` 0.27.2,
  `tree-sitter-wasms` 0.1.13).

## Privacy (the hard prohibition)

A record may contain **numbers, closed-union identifiers and timestamps**. It may never contain source text,
symbol names, qualified names, file paths, `search.query`, `impact.paths`, cursor strings, error message
text, a repo remote URL, git identity, a hostname, or an environment variable value.

- Records are built by **copying an explicit whitelist**, never by spreading an envelope, a payload or an
  `args` record. `arg_names_set` is the set of names the **request** supplied, before defaults — names only,
  no values, and never a name that only the manifest default produced, so an empty set means "the caller
  sent no arguments".
- `stale_reason` is a **closed enum**: one of `STALE_REASONS` (`src/shared/domain/freshness.ts:61-67`) or
  `"other"` when the envelope carried prose. The envelope's prose is never copied — `trace` builds
  `"${node.path}" was last written by generation N` (`src/tools/trace/handler.ts:680-683`) and `status` and
  `search` write counts and verdicts, so a verbatim copy leaks a repo path and a basename. The envelope
  itself is not changed; this is a recording rule.
- `error_code` is `AppError.code`, a closed union from `src/shared/errors.ts` — never `error.message`.
- `result_counts` holds `{key: arrayLength}` for top-level payload keys whose value is an array. No payload
  value is ever copied.
- The `index_run` record's maps (`nodes_by_kind`, `edges_by_kind_tier`, `languages`) are keyed by
  `NodeKind`, `RawEdgeKind`+`EdgeTier` and `LangId` — closed unions, not free text.
- `tool` is clamped: the recorded name is a registered tool name or the literal `"unknown"`, never the
  client's `params.name` string, which is free text the record must not echo.
- This invariant is asserted by a test that parses a record built from real values (a repo path, a symbol
  name and a `search.query`), deep-walks every value except `arg_names_set`, and asserts none appears — plus
  a case that builds a `call` record from an envelope carrying the `trace` prose and asserts the enum
  `"other"` with no path and no `generation`. Documenting it is not enough.

## Runtime and I/O

- **stdout is the MCP protocol.** Telemetry writes files; the only stdout write this plan adds is the one
  human line `sangkar telemetry` prints, exactly like `runTool`. Diagnostics go through `#shared/log`
  (stderr). `console.log` stays banned by `biome.json`'s `noConsole` (only `error`/`warn` allowed).
- Recording never throws and never blocks a tool call: every sink call is wrapped, and a sink that throws
  logs one `warn` and disables further writes for that process instead of throwing or repeating.
- Bounded buffer: flush when the buffer holds ≥ 32 records, or when the oldest buffered record is ≥ 5 000 ms
  old (evaluated on the next `record()`, so no unref'd interval and no timer keeping the process alive),
  plus an explicit `close()` on the exit path. A `SIGKILL` may lose at most the buffered tail — accepted and
  documented.
- Flush points: `shutdown()` in `src/index.ts` (the existing SIGINT/SIGTERM/EOF path, line 517), the
  `finally` of `runWorker`, and the end of a one-shot CLI tool call.
- Writes are appends: `appendFileSync` per flush, `mkdirSync(recursive)` on first use, one file per UTC date
  derived from the record's own `ts`. Existing lines are never rewritten. The server and the worker it spawns
  write the same file, so a flush is **one** append of the whole buffer joined with `"\n"` and capped at 32
  records: `O_APPEND` makes each write land whole, interleaving only at line boundaries. A torn final line
  from a killed process is recovered by the reader's skip rule, never by a lock.

## Surface

- Tool surface is unchanged: seven names, byte-identical manifests and descriptions, `tools/list` returns 7.
  `src/shared/instructions.ts` and `docs/INSTRUCTIONS.md` are **not** edited — `telemetry` is a verb, not a
  tool.
- The envelope, its nine fields, `emit()`'s budget behaviour, `total_relation` downgrading and cursors are
  untouched. Telemetry only *reads* what an envelope already says.
- Exit codes keep their meaning: 0 ok, 1 error, 2 usage/configuration refusal. `sangkar telemetry` follows
  it: 0 for a written report (including zero records), 2 for a bad flag value, 1 for a write failure.

## Verification environment

- Commands run from the repository root: `$HOME/.bun/bin/bun <script>`. **Bun is not on `PATH`**; a bare
  `bun` fails with `No such file or directory` and would read as a task failure.
- Checks are the package scripts: `run typecheck` (`tsc --noEmit`, includes `src`, `tests`, `bin`),
  `run lint` (biome over `src tests bin`), `test` (bun test, root `tests/`, ~564 tests, ≈47 s),
  `run db:drift` (drizzle-kit generate + `git diff --exit-code` on `src/shared/store/migrations`).
- `bun run acceptance` runs `tests/acceptance.test.ts` (9 assertions incl. latency and the
  builder-vs-prepared ratio). This plan adds no query to a hot path and touches no store module, so it is
  **not** part of the gate; the final verification task runs it once as a regression confirmation and
  reports the result rather than gating on it.
- Tests are offline and need no service. New fixtures are `mkdtemp` directories under the OS temp dir,
  removed in `afterAll`. No test may write into the repository's own `.sangkar/`, `code-review-graph/` or
  `codebase-memory-mcp/`.
- A test that asserts behaviour with telemetry off must delete `SANGKAR_TELEMETRY` from the child env and
  then set it explicitly; a child that inherits a setting proves nothing (`tests/cli.test.ts`'s `cleanEnv()`
  already deletes every `SANGKAR_*`, so a telemetry assertion there must set it back on purpose).
- Forbidden for the executor: editing `.ppm/*/tasks/*.json` by hand, editing `tests/boundary.test.ts`,
  weakening an assertion to make a check pass, adding a dependency, running `bun link`, or touching
  `docs/research/**`.
- Rollback: the change is additive code plus one new verb, no schema and no migration, so reverting the working-tree
  changes restores the previous behaviour completely (nothing is committed between tasks or phases, so
  there are no phase commits to revert). The only persisted artefact is
  `<dataDir>/telemetry/`, which `.sangkar/`'s own `.gitignore` (content: `*`) already excludes — **no
  `.gitignore` edit is needed** (verified: `.gitignore:13` ignores `.sangkar/`, and `.sangkar/.gitignore`
  contains a single `*`).
