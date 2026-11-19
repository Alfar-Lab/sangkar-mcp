# Approach

## Current state (verified at ed16170 by reading the files)

- `src/index.ts` is 809 lines and is the single choke point. `const TOOLS` (147) holds seven
  `RegisteredTool` records. `callTool` (432) resolves a tool, refuses an unknown name, handles a
  `refused`/`absent` store, then `await tool.handle(...)`, `route(...)` and `answer(...)` — every non-handler
  answer is built with `fail()`/`bootstrapping()` off `boundarySpec()`. `runTool` (688) is the CLI twin and
  is "the only place in this file that writes to stdout". `shutdown` (517) is idempotent and already runs on
  SIGINT/SIGTERM (`onSignal`, 633) and on stdin EOF. `main` (784) resolves the config once and switches on
  the mode; `runWorker` is reached for `worker`/`index`.
- `Envelope` (`src/shared/envelope.ts`) carries `status`, `read_status{generation,indexed_at,stale,
  stale_reason}`, `coverage{complete,indexed,skipped,failed,unresolved_edges,not_indexed}`, `totals
  {total_relation,…}`, `truncated`, `truncation_reason`, `next`, `next_tools`, `payload`. Every field the
  `call` record needs is therefore already on the value the wrapper returns — no handler change is needed.
- `emit()` (`src/shared/budget.ts:335`) returns an `Envelope` and exports `BYTES_PER_TOKEN = 4`, but no
  token count, so `payload_tokens` is measured by the recorder as `ceil(byteLength(JSON.stringify(envelope))
  / 4)`.
- The budget argument is named `budget` in six of seven manifests (`expand`, `impact`, `search`, `summary`,
  `trace`, `status`); `src/tools/index/manifest.ts` declares none. `parseArgs` applies declared defaults in
  its one loop (`src/shared/cli/args.ts:200-211`), so `args.budget` is the effective budget after parsing.
  `parseCliArgs` (232) reads argv into a `raw` record that holds only what the caller wrote and ends by
  handing that record to `parseArgs` (314) — the raw record never carries a default, which is the hook the
  `call` wrapper uses to recover the supplied names (Design 10).
- `src/indexer/worker.ts` (845 lines): `WORKER_EXIT = {published: 0, failed: 1, nothingToDo: 2, locked: 75}`
  (112), `runWorker` (195) has exactly three returns plus one `catch`, `indexGraph` (311) computes
  `durationMs = Date.now() - ctx.startedAt` (332) and returns the exit code, `plan.nothingToDo` short-circuits
  at 315, `plan.changed` and `discovered` are in scope there, and `resolve.ts` returns
  `BindResult{edges, unresolved}` (91). `WorkUnit` nodes carry `kind` (`extract.ts:79`) and `ResolvedEdge`
  carries `kind`+`tier` (`resolve.ts:71`), so per-kind tallies are in-memory, no extra query.
- `src/shared/config.ts` (91) is the only reader of tool settings: `AppConfig` is a 4-field interface (19)
  built by `loadConfig` (91) from `--repo-root`, `--data-dir` and `SANGKAR_CURSOR_KEY`. `flagValue` (50) is
  exported and scans the whole argv. **Exception found:** `src/index.ts:596` reads `SANGKAR_NO_WATCH`
  directly, deliberately ("not a tool setting").
- `bin/sangkar.ts` (2.8 K): `RESERVED = ["mcp","worker","index"]` (11), `USAGE` (14) with `verbs`, `tools`
  and `global flags` blocks, `VERB_USAGE` (45) keyed by the reserved union, a `--help` guard before anything
  loads (57-73), then `await import("../src/index")` and `main(requested, argv)`.
- `tests/boundary.test.ts` `walkTree()` (288) recurses `src` and `tests` skipping `node_modules` and
  `fixtures`, so a new `src/shared/telemetry/**` tree is scanned with no test edit.
- `.gitignore:13` ignores `.sangkar/`, and `.sangkar/.gitignore` contains exactly `*`; `git check-ignore -v
  .sangkar/graph.db` reports `.gitignore:13`. No ignore entry is added, and `.sangkar/graph.db` is 2.8 MB
  (the repo's own graph), used as the first local report fixture by hand.
- **Typecheck break the brief did not name:** `tests/tools-index.test.ts:108` writes an `AppConfig` object
  literal. A new required field on `AppConfig` fails `tsc` there until that literal gains it.
- `tests/config.test.ts:170` already asserts `loadConfig` "ignores `SANGKAR_REPO_ROOT`, `SANGKAR_DATA_DIR`
  and `SANGKAR_PROFILE`", which is the file where the new variable's semantics belong.
- Compatibility of default-on: the spawn-based suites pass a temp `--data-dir`
  (`tests/mcp-surface.test.ts:105-141`), and the suites that assert an **exact** data-dir listing
  (`tests/publish.test.ts:127,155,200,291,342` and `tests/smoke.test.ts:260`, each `toEqual(["graph.db"])`)
  build the store through the publish primitives **in process**, never through `main`, so no recorder exists
  and a `telemetry/` subdirectory cannot appear under them. `tests/worker.test.ts` is not in that set: its
  `stageFiles` helper (`:141-144`) filters names containing `.stage.` and never asserts exactness.

## Design

1. **A module, not a hook.** `src/shared/telemetry/` holds `record.ts` (types), `sink.ts` (`jsonlSink`,
   `nullSink` behind one `TelemetrySink` interface), `identity.ts` (install id + session id), `telemetry.ts`
   (the recorder and the three record builders), `report.ts` (read + aggregate) and `html.ts` (render).
   Rationale: `shared-isolation` forbids it from reaching the indexer, so both the root and the worker can
   import it, and `root-privileges` stays intact because only `src/index.ts` imports the indexer.
2. **One creation per process, in `main`.** `main` calls `createTelemetry({config, sessionMode, …})` for
   `mcp`, for a tool call and for `worker`/`index` (passed into `runWorker` through a new optional
   `WorkerOptions.telemetry`). The session record is emitted at construction. `runWorker` called directly
   (as `tests/worker.test.ts` does) has no instance, so the same code path writes nothing — no existing
   worker test changes.
3. **The sink is 20 lines and two implementations.** `jsonlSink(dir)` implements
   `write(date, lines)` by appending `lines.join("\n") + "\n"` to `<dir>/<date>.jsonl`, the date supplied by
   the recorder from the record's own `ts`; `nullSink` discards. Adding an uploader later
   means adding one implementation of the same interface and one branch in `createTelemetry` — no call site
   moves. The server and the worker it spawns write the same daily file, so a flush is **one** append of the
   whole buffer joined with `"\n"` and capped at `FLUSH_AT_RECORDS` (32) records: `O_APPEND` makes each write
   land whole, so two processes interleave at line boundaries and never inside a line. A torn final line from
   a killed process is recovered by `readRecords`' skip rule (phase_2's `report.ts`), not by a lock.
4. **Daily-rotated install id, per-process session id.** `identity.ts` keeps `<dir>/install_id.json` holding
   `{install_id, date}`. On start: read it; if `date` is not today's UTC date, mint 16 random bytes as hex and
   overwrite (create with flag `wx`, fall back to read on `EEXIST`). `install_id` is stable inside one UTC
   day and unlinkable across days; `session_id` is 16 random bytes hex per process; `seq` counts calls from 1
   within the process. No repo path, no hostname, no user identity is read.
5. **`repo_size_bucket` is a stat, not a walk.** Bucketing the real file count would need a filesystem walk on
   every MCP start (`src/indexer/discover.ts` is the only walker and lives in the indexer, which shared may
   not import). The bucket is therefore derived from `statSync(join(dataDir,"graph.db")).size`:
   `none` (absent or 0), `xs` < 1 MiB, `s` < 10 MiB, `m` < 100 MiB, `l` < 1 GiB, `xl` ≥ 1 GiB. `root_kind` is
   `existsSync(join(repoRoot,".git"))` ? `"git"` : `"plain"`. `session.languages` is `[]` for a process that
   never walked the tree; the run-level truth is `index_run.languages`, which the worker fills from
   `discovered.files`.
6. **Raw numbers only.** No bucketing, averaging or rounding happens in the sink; `unresolved_ratio` and
   `budget_used_ratio` are the two derived ratios the owner's schema asks for, rounded to 4 decimals so a
   line is stable. A future network sink owns any coarsening.
7. **The verb is a reserved verb, dispatched from `bin/sangkar.ts`.** `RESERVED` gains `"telemetry"`, `USAGE`
   gains a `verbs` line, `VERB_USAGE` gains its flags, and a narrow branch before the `../src/index` import
   calls `#shared/telemetry/cli`'s `runTelemetry(argv)`, which resolves its own config with `loadConfig`
   because the report needs only `dataDir` and never a graph. `src/index.ts` is not touched for the verb, so
   the two call-site edits it does need (phase_1) are the only ones it ever gets.
   *Rejected alternative:* routing through `main`'s switch. It would work, but it puts `src/index.ts` in two
   phases' ownership, and the bin already owns `RESERVED`/`USAGE`/`VERB_USAGE` — the verb's help, its
   dispatch and its flags end up in one file. `main`'s "resolve the config once" comment (784) still holds
   for every mode that serves a graph.
8. **The HTML is hand-built.** One template literal: `<style>` block inline, one `<script>` block inline, the
   aggregated data embedded as `JSON.stringify(data)` with `<` escaped to `\u003c` so a `</script>` in any
   string cannot close the tag, every interpolated string through `escapeHtml`. No `<link>`, no `src=`, no
   `@import`, no `url(`, no CDN — the file must open from a USB stick with no network.
9. **Determinism is a test property.** Maps are rendered sorted by key, numbers through one `fmt` helper with
   fixed decimals, and `generated_at` is injected by the caller, so `renderHtml(data, {generated_at})` twice
   with the same input is byte-identical.
10. **`arg_names_set` is explicit-only; `budget_tokens` is effective.** A defaulted argument is not a fact
    about the caller. `parseArgs` fills every declared default (`args.ts:200-211`), so `parseArgs`'s output
    already contains `budget` for a bare `sangkar status` (`{paths: [], budget: 800}`); recording that set
    would make the field a constant equal to the schema and destroy its only product value — seeing which
    arguments a caller actually reaches for. The wrapper therefore records the supplied names and keeps the
    defaulted record only for `budget_tokens`, which is meant to be the budget in force. CLI: `parseCliArgs`
    gains `parseCliArgsAndNames` (`{args, supplied}`) beside the unchanged `parseCliArgs`, because
    `tests/cli-args.test.ts:260-262` asserts the current return shape; `supplied` is the `raw` record's keys,
    which is exactly "declared names whose source value was not `undefined`". MCP: the raw argument keys,
    intersected with the manifest — `parseArgs` silently drops undeclared keys, so the intersection is what
    keeps a client-supplied key name out of the record.
11. **`stale_reason` is an enum, not a string.** The envelope types it as plain `string`
    (`src/shared/envelope.ts:51`) and six tools write prose there — `trace` interpolates a repo path
    (`src/tools/trace/handler.ts:682-683`), `status` composes verdict words
    (`src/tools/status/handler.ts:252,298,303`), `search` writes a sentence with counts
    (`src/tools/search/handler.ts:221`), and `expand`, `index` and `summary` write fixed sentences
    (`src/tools/expand/handler.ts:539`; `src/tools/index/handler.ts:282,291`;
    `src/tools/summary/handler.ts:658,685`) — so copying it verbatim would put a private path in every `call`
    record of a stale answer. The record stores one of the
    five `STALE_REASONS` values plus `"other"` for "the envelope carried prose". The envelope is untouched:
    this is a recording rule, enforced in `record.ts`'s whitelist, in `TELEMETRY.md` and in a test.

### The record schema (normative; every field always present, `null` where the answer is "not known")

`ts` is `new Date().toISOString()`; `install_id`, `session_id` per §4. Serialisation order is the order
below — the objects are built as literals in that order, which makes a line diffable. `v` is the record
format version and is `1` for every kind; it sits directly after `ts` so a reader checks the version before
it dispatches on `kind`. Adding a field keeps `v` at `1` (a reader ignores what it does not need); removing
or renaming a field is a version bump.

**`kind: "call"`** — `ts, v, kind, install_id, session_id, seq, tool, status, error_code, duration_ms,
arg_names_set, budget_tokens, payload_tokens, budget_used_ratio, truncated, truncation_reason,
total_relation, coverage_complete, coverage_indexed, coverage_skipped, coverage_failed, unresolved_edges,
stale, stale_reason, generation_age_ms, result_counts, server_version, bun_version, os, arch,
graph_schema_version`

| field | source |
|---|---|
| `v` | `RECORD_VERSION = 1` |
| `seq` | 1-based counter per process |
| `tool` | the registered tool name, or the literal `"unknown"` when `params.name` is not one of the seven |
| `status` | `envelope.status`; `"absent"` for the CLI's no-graph answer, which returns 0 without an envelope; never `"error"` for a process that exited 0 |
| `error_code` | `envelope.error.code`, or the refusal's `AppError.code`; never `message` |
| `duration_ms` | `Date.now()` at wrapper entry → `finally` |
| `arg_names_set` | the names the **request** supplied, before defaults — `[...supplied].sort()`; never a defaulted name, so `[]` means "the caller sent no arguments" |
| `budget_tokens` | `Number(args.budget)` when the manifest declares it, else `0` — the **effective** value, i.e. after the manifest default |
| `payload_tokens` | `ceil(JSON.stringify(envelope) bytes / BYTES_PER_TOKEN)`; `0` when no envelope |
| `budget_used_ratio` | `payload_tokens / budget_tokens` to 4 dp, `null` when `budget_tokens` is 0 |
| `truncated`, `truncation_reason` | from the envelope (`false`/`null` when none) |
| `total_relation` | `envelope.totals.total_relation` (`"gte"` when none) |
| `coverage_*` | `coverage.complete/indexed/skipped/failed` (`false`/0 when none) |
| `unresolved_edges` | `coverage.unresolved_edges` (0 when none) |
| `stale`, `stale_reason` | `read_status.stale` / the `stale_reason` **enum**: one of `STALE_REASONS` (`src/shared/domain/freshness.ts:61-67`) or `"other"` when the envelope carried prose; `null` when the envelope has no `stale_reason`. The prose is never copied — `trace` puts a repo path in it (`src/tools/trace/handler.ts:680-683`) |
| `generation_age_ms` | `Date.parse(ts) - Date.parse(read_status.indexed_at)`, `null` when unparseable |
| `result_counts` | top-level `payload` keys whose value is an array → `length` |
| `server_version` | the root's `SERVER_VERSION` (`src/index.ts:103`), injected at creation |
| `bun_version`, `os`, `arch` | `Bun.version`, `process.platform`, `process.arch` |
| `graph_schema_version` | `schemaVersion()` from `#shared/store/migrate`, injected by the root |

**`kind: "index_run"`** — `ts, v, kind, install_id, session_id, mode, exit_code, duration_ms, was_forced,
generation_advanced, files_seen, files_indexed, files_skipped, files_failed, nodes_by_kind,
edges_by_kind_tier, unresolved_count, unresolved_ratio, languages, db_size_bytes, files_changed_in_run`

| field | source (all inside `indexGraph`, `src/indexer/worker.ts:311`) |
|---|---|
| `mode` | `ctx.mode` (`"full"`/`"fast"`) |
| `exit_code` | `WORKER_EXIT` value the run returns |
| `duration_ms` | the existing `durationMs` (332) |
| `was_forced` | `ctx.force` |
| `generation_advanced` | `exit_code === WORKER_EXIT.published` — `newGenerationId()` is only minted on the write path |
| `files_seen` | `discovered.files.length` |
| `files_indexed` | `parsed` entries whose `unit !== null` |
| `files_skipped` | `discovered.skipped` |
| `files_failed` | `parsed` entries with `parseStatus === "failed"` |
| `nodes_by_kind` | `units.flatMap(u => u.nodes)` grouped by `node.kind` — nodes **written by this run** |
| `edges_by_kind_tier` | `resolved.edges` grouped by `` `${edge.kind}:${edge.tier}` `` |
| `unresolved_count` | `units.flatMap(u => u.unresolved).length + resolved.unresolved.length` (what `persistUnit` writes) |
| `unresolved_ratio` | `unresolved_count / (unresolved_count + edges.length)` to 4 dp, 0 when the denominator is 0 |
| `languages` | `discovered.files` grouped by `lang` (`LangId`, `src/indexer/discover.ts:43`) → file count |
| `db_size_bytes` | `statSync(livePath).size` in the exit path, `0` when absent |
| `files_changed_in_run` | `plan.changed.length` |

A run that exits `locked` or `failed` before `indexGraph` records zeros for every field it never computed and
the real `exit_code`; `nothingToDo` records `discovered`/`plan` facts with zero `nodes_by_kind`.

**`kind: "session"`** — `ts, v, kind, install_id, session_id, session_mode, server_version, bun_version, os,
arch, is_first_run, repo_size_bucket, languages, root_kind` (`session_mode`: `mcp` | `cli` | `worker`;
`is_first_run`: this process created today's `install_id.json`; `languages`: `[]` unless the process itself
walked the tree, see Design 5).

## Phases

### phase_0: shared telemetry module and config plumbing

Deliverable: `src/shared/telemetry/{record,sink,identity,telemetry}.ts` plus `AppConfig["telemetry"]`, each
proven by `tests/telemetry.test.ts` — including the redaction invariant, the daily rotation, the `off`
switch, a throwing sink and the bounded flush. No existing behaviour changes; nothing is wired yet. The
phase gate whitelists exactly those four module files, `src/shared/config.ts` and the three test files
(`tests/config.test.ts`, `tests/tools-index.test.ts`, `tests/telemetry.test.ts`), and it runs the full suite: `AppConfig` is `ToolContext.config` (`src/shared/store/read/context.ts:54`), so a new
required field on it reaches every suite, not only `tests/config.test.ts`.

Barrier because: phase_1's two call sites consume these exact signatures (`Telemetry`, `CallSpec`,
`IndexRunSpec`, `createTelemetry`) and the field lists above. Changing a record shape after the root and the
worker both write it means fixing two instruments and every report assertion at once.

### phase_1: instrumentation at the composition root and the indexer worker

Deliverable: a real `session` + `call` record from `src/index.ts` (both surfaces, all outcomes) and a real
`index_run` from `src/indexer/worker.ts`, plus the one additive export in `src/shared/cli/args.ts` that makes
the supplied argument names available, proven end to end by `tests/telemetry-root.test.ts` spawning the
shipped CLI and speaking JSON-RPC to the shipped server in a temp tree, plus a green full suite and boundary
run.

Barrier because: phase_2 aggregates the records this phase actually writes; a report written against an
imagined shape would need rewriting, and the record order/field coverage can only be checked once real files
exist.

### phase_2: the `telemetry` verb, aggregation and the HTML report

Deliverable: `report.ts` (read + aggregate), `html.ts` (self-contained render), `cli.ts` (`runTelemetry`
with `--out`/`--since`), `bin/sangkar.ts` (reserved verb, usage, lazy dispatch) and the two suites that pin
them, so `sangkar telemetry` writes one file that answers per-tool counts, status and error mixes, latency
percentiles, truncation rate, `eq`-vs-`gte`, the unresolved-ratio trend and the index-run table.

Barrier because: phase_3 documents a surface that must be final. The flag names, the default output path and
the zero-record/corrupt-line rules are what the doc records; writing them first would force a rewrite.

### phase_3: documentation and the closing gate

Deliverable: `docs/my-product/TELEMETRY.md`, a `TECH-SPEC.md` §11 cross-reference, the `AGENTS.md` edits, the
consent disclosure the user actually reads (`README.md`'s variable table and one clause in the `telemetry`
verb's `USAGE` line in `bin/sangkar.ts`), and the final verification run (typecheck, lint, full suite,
`db:drift`, one acceptance confirmation).

Barrier because: it consumes the finished verb and record set, and the closing gate must run last. The one
`USAGE` clause is the only phase_3 write outside `docs/`, and it is a serial edit to a file phase_2 already
closed — see plan.risk.md's ownership row.
