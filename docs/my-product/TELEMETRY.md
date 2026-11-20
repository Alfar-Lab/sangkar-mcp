# Telemetry design

**Normative for** `src/shared/telemetry/**`, the `SANGKAR_TELEMETRY` switch in `src/shared/config.ts`,
the per-repo settings in `src/shared/repo-config.ts` and the `sangkar telemetry` verb. Companion to
`TECH-SPEC.md` §11 (stdout discipline) and to the environment table in `README.md`.

Every claim here is checkable against the code that ships today. Where the implementation and this
document disagree, one of them is a defect — there is no third story.

---

## 1. What it is

Local-only usage telemetry. Three JSON-Lines record kinds are appended to
`<dataDir>/telemetry/YYYY-MM-DD.jsonl`, one line per record, and `sangkar telemetry` folds the daily
files into a single self-contained HTML file — or, with `--serve`, answers with the same kind of page
for every registered repository (§6.1).

Nothing is uploaded, and nothing leaves the machine. The sink has no uploader: `sink.ts` imports
`node:fs` and `node:path` only, and `TelemetrySink` is the seam a future uploader would implement —
until one exists, "local only" is a property of the code rather than of a promise. One listener does
live in this directory, and it is the report server: `serve.ts` is `Bun.serve` on loopback and
nothing else (§6.1), serving a page built the way `--out` builds its file — `html-multi.ts` reuses
`html.ts`'s stylesheet and panels and adds no external font, script, image or stylesheet, so the
machine-wide report is exactly as self-contained as the single-repo one. Nothing here opens an
outbound connection: a report of every repository on the machine is a document that stays on the
machine, whether it is read from a file or from a socket.

| Piece | File | Owns |
|---|---|---|
| record schema and the three builders | `record.ts` | the shape, the field order, every derived field |
| identity | `identity.ts` | `install_id`, `session_id` |
| recorder | `telemetry.ts` | the clock, `seq`, the buffer, the flush |
| per-repo decision | `for-repo.ts` | whether a repository records, and into which directory |
| destination | `sink.ts` | one append per daily file |
| read and fold | `report.ts` | skip rules, aggregation, ratios |
| render | `html.ts` | the document, inline CSS/JS/JSON |
| render every repository | `html-multi.ts` | one section per source directory, plus the machine total |
| serve | `serve.ts` | the loopback-only listener, its three routes, the default port |
| verb | `cli.ts` | flags, exit codes, the one stdout line |

---

## 2. Consent

One environment variable, read once, in `src/shared/config.ts`, and handed around as the value
`AppConfig["telemetry"]` (`{ enabled, dir }`). No module below the root reads the variable itself.

| `SANGKAR_TELEMETRY` | Effect |
|---|---|
| unset | on |
| empty string | on |
| exactly `off` | off |
| anything else (`0`, `false`, `OFF`, `Off `) | on |

One spelling, case-sensitive, and no aliases. The reason is the failure direction: an operator's typo
must not be able to opt a machine in silently, so the only value that disables is the one that was
written down, and everything an operator might intend as "off" while spelling it otherwise is treated
as an unset variable — which is the documented default.

Off means no file and no directory is created. `jsonlSink` calls `mkdirSync` on its first non-empty
write, and `createTelemetry` is never constructed, so a disabled installation leaves no
`<dataDir>/telemetry/` behind. It does not mean "written and then ignored".

### 2.1 The recording precedence

Three inputs, one decision. The decision is `recordingEnabled` in `src/shared/repo-config.ts`, called
from exactly one place — `telemetryForRepo` in `src/shared/telemetry/for-repo.ts` — because a daemon
serving several trees must decide per tree, while consent is per machine:

| Input | Read from | What "no" means |
|---|---|---|
| `SANGKAR_TELEMETRY` | `src/shared/config.ts`, once per process, as `config.telemetry.enabled` | the process records nothing, for every repository it serves |
| registration | `<home>/sangkar.json` through `readRegistry` | a tree the registry does not list is not recorded, whatever its own file says |
| the repo's settings | `<repo>/.sangkar/config.json` through `readRepoConfig` | `"telemetry": false` disables that one repository |

A call is recorded only when all three say yes. The order between them is the point of the section:
`SANGKAR_TELEMETRY=off` **outranks the repo file**, so no repository can opt itself back in past the
operator, and the repo file's `false` **outranks the default of `true`**, so the file can only narrow
what the machine allows — never widen it. Registration sits between the two: a tree the operator never
registered is not a tree this installation agreed to describe. When the recorder is built, its
directory is the selected repository's own `<dataDir>/telemetry`, not the startup tree's, because a
daemon that swapped trees would otherwise write into the wrong one.

The settings file is one small JSON object, snake_case on disk and camel case in TypeScript, mapped in
that one module:

```json
{"watch":true,"watch_interval":2000,"telemetry":true}
```

A missing, unreadable or unparseable file means every default: `watch: true`, `watch_interval: 2000`,
`telemetry: true`. A bad field falls back alone, and a field that is not a JSON boolean is a typo that
takes its default rather than being interpreted — a quoted `"false"` must not read as consent.
`watch_interval` is milliseconds and is clamped into the band the daemon can honour: below `1000`
becomes `1000` (a poll faster than one save per window costs more than the edit it notices), above
`300000` becomes `300000`, and anything that is not a positive integer — `"2000"`, `2000.5`, `0`,
`-1` — falls back to `2000` instead of being guessed at. `sangkar repo config <path> --watch`,
`--watch-interval` and `--telemetry` write the file and then print the values a re-read returns, so a
clamped interval is reported as the daemon will actually use it.

---

## 3. Identity

Two random identifiers, and nothing about the repository, the machine or the user goes into either.

| Field | What it is | Lifetime |
|---|---|---|
| `install_id` | 16 random bytes (`randomBytes(16)`) as 32 hex characters, stored in `<dataDir>/telemetry/install_id.json` as `{ install_id, date }` | one UTC day |
| `session_id` | 16 random bytes as hex | one process, never persisted |

`install_id` is read at start-up and rotated whenever the stored `date` is not today's UTC date
(`utcDateOf`), so one id covers one UTC day and two days of records cannot be joined by it. The
rotation deletes the stale file first and recreates it with the `wx` flag, which is the race breaker:
of two processes starting on the same day exactly one creates the file, the loser reads `EEXIST` and
adopts the winner's id rather than overwriting it. A data dir that cannot be written costs the day's
grouping, never an answer — the in-memory id is used and nothing is thrown.

`session_id` is minted per process and never written anywhere, so it groups the records of one
`mcp` server, one CLI invocation or one index run and joins to nothing else.

No repo path, remote URL, hostname or user identity is read to build either id. There is no fallback
to a machine id, a MAC address or a hash of the repository path — a directory hash would make the id
stable across days and re-link the very records the rotation exists to separate.

---

## 4. The record schemas

Three kinds, one `JSON.stringify` per record, written in the field order of the interface in
`record.ts`. Every table below is in serialisation order and starts `ts, v`.

`v` is `RECORD_VERSION = 1`. **Adding** a field keeps it at `1` (a reader ignores the key it does not
need); **removing or renaming** one is a version bump, because `report.ts` skips a line whose `v` is
not `1` instead of guessing at it. A skip is counted in `unparsed` and folded into no ratio and no
count, so a wrong `v` costs records rather than corrupting an answer.

Every field is always present, and `null` marks "not known". A reader never has to distinguish an
absent key from an unknown value; `optionalRatio` in `html.ts` renders `null` as `—` rather than `0`
for exactly that reason.

### 4.1 `call` — one answer

| Field | Type | Source |
|---|---|---|
| `ts` | string | the process clock, read once per record; ISO-8601 UTC |
| `v` | number | `RECORD_VERSION` = `1` |
| `kind` | `"call"` | literal |
| `install_id` | string | §3 |
| `session_id` | string | §3 |
| `seq` | number | per-process counter, incremented before the record; first record is `1` |
| `tool` | string | the registered tool name, or the literal `"unknown"` (§5) |
| `status` | `EnvelopeStatus \| "absent"` | `envelope.status`, or `"absent"` (§4.4) |
| `error_code` | string \| null | `envelope.error.code`, `null` when the envelope carries no error |
| `duration_ms` | number | the call site's clock around the call (`Date.now()` before and after in `src/index.ts`) |
| `arg_names_set` | string[] | the names the **request** supplied, deduplicated and sorted (§4.4) |
| `budget_tokens` | number | the **effective** budget after the manifest default; `0` when the tool declares no `budget` argument (§4.4) |
| `payload_tokens` | number | `ceil(byteLength(JSON.stringify(envelope)) / BYTES_PER_TOKEN)`, `BYTES_PER_TOKEN = 4`; `0` when there is no envelope |
| `budget_used_ratio` | number \| null | `payload_tokens / budget_tokens`, rounded to 4 decimals; `null` when `budget_tokens` is `0` |
| `truncated` | boolean | `envelope.truncated`; `false` when there is no envelope |
| `truncation_reason` | `TruncationReason \| null` | `envelope.truncation_reason` |
| `total_relation` | `"eq" \| "gte"` | `envelope.totals.total_relation`; `"gte"` when there is no envelope |
| `coverage_complete` | boolean | `envelope.coverage.complete`; `false` when there is no envelope |
| `coverage_indexed` | number | `envelope.coverage.indexed`; `0` when there is no envelope |
| `coverage_skipped` | number | `envelope.coverage.skipped`; `0` when there is no envelope |
| `coverage_failed` | number | `envelope.coverage.failed`; `0` when there is no envelope |
| `unresolved_edges` | number | `envelope.coverage.unresolved_edges`; `0` when there is no envelope |
| `stale` | boolean | `envelope.read_status.stale`; `true` when there is no envelope |
| `stale_reason` | `StaleReason \| "other"` \| null | `envelope.read_status.stale_reason` through a closed enum (§5) |
| `generation_age_ms` | number \| null | `ts` minus `envelope.read_status.indexed_at`; `null` when either does not parse |
| `result_counts` | map | top-level payload keys whose value is an array, mapped to its length; no value is copied |
| `server_version` | string | `SERVER_VERSION` in `src/index.ts` |
| `bun_version` | string | `Bun.version` |
| `os` | string | `process.platform` |
| `arch` | string | `process.arch` |
| `graph_schema_version` | number | `schemaVersion()`, the count of migration files the store was built from |

### 4.2 `index_run` — one index run

| Field | Type | Source |
|---|---|---|
| `ts` | string | the clock reading taken when the run's `finally` records it |
| `v` | number | `RECORD_VERSION` = `1` |
| `kind` | `"index_run"` | literal |
| `install_id` | string | §3 |
| `session_id` | string | §3 |
| `mode` | `"full" \| "fast"` | the worker's resolved mode |
| `exit_code` | number | the worker's exit code, decided before the record is written |
| `duration_ms` | number | the worker's own start-to-finish clock |
| `was_forced` | boolean | the run's `--force` flag |
| `generation_advanced` | boolean | `true` when `exit_code === 0`, i.e. the run published |
| `files_seen` | number | the run summary |
| `files_indexed` | number | the run summary |
| `files_skipped` | number | the run summary |
| `files_failed` | number | the run summary |
| `nodes_by_kind` | map | node kind → count, from the run's units |
| `edges_by_kind_tier` | map | `"<kind>:<tier>"` → count, tier `extracted` or `inferred` |
| `unresolved_count` | number | call sites that did not bind in this run |
| `unresolved_ratio` | number | `unresolved_count / (unresolved_count + Σ edges_by_kind_tier)`, 4 decimals, `0` when the denominator is `0` |
| `languages` | map | language id → file count; the run-level truth, because only the worker walked the tree |
| `db_size_bytes` | number | the published graph file's size after the run |
| `files_changed_in_run` | number | the run summary |

`generation_advanced` is `true` exactly when `exit_code === 0`, and the record is written in the
worker's `finally` after the lock is released — a non-zero exit means nothing was published.

### 4.3 `session` — one process

| Field | Type | Source |
|---|---|---|
| `ts` | string | the clock reading taken when the recorder is constructed |
| `v` | number | `RECORD_VERSION` = `1` |
| `kind` | `"session"` | literal |
| `install_id` | string | §3 |
| `session_id` | string | §3 |
| `session_mode` | `"mcp" \| "cli" \| "worker"` | the mode `main` resolved |
| `server_version` | string | `SERVER_VERSION` |
| `bun_version` | string | `Bun.version` |
| `os` | string | `process.platform` |
| `arch` | string | `process.arch` |
| `is_first_run` | boolean | `true` when this process created today's `install_id.json` |
| `repo_size_bucket` | `"none" \| "xs" \| "s" \| "m" \| "l" \| "xl"` | `graph.db`'s size, bucketed at 1 MiB, 10 MiB, 100 MiB and 1024 MiB, inclusive-low; `"none"` when the file is absent or empty |
| `languages` | string[] | always `[]` at session level: a process that did not walk the tree cannot name its languages, and `index_run.languages` is where the run-level answer lives |
| `root_kind` | `"git" \| "plain"` | `"git"` when `<repoRoot>/.git` exists, else `"plain"` |

The session record is buffered before any call can be recorded, carries no `seq`, and is the
denominator every later record is read against.

### 4.4 The three `call` rules that need their own note

**`arg_names_set` is the set the request supplied, before defaults.** `parseArgs` fills every declared
default (`src/shared/cli/args.ts:200-211`), so `Object.keys(args)` would report the schema rather than
the request: a bare `sangkar status` would record `["budget","paths"]` and the field would stop
answering the one question it exists for — which arguments a caller actually reaches for. A defaulted
name never appears, and an empty set means "the caller sent no arguments". On the MCP surface the
names are intersected with the tool's manifest, so a client-supplied key the schema does not declare
is not an argument and is not recorded. A positional CLI token is supplied:
`sangkar trace handleRequest` records `["query"]`, because `readArgv` binds the bare token to the
manifest's positional field.

**`budget_tokens` is the effective budget after the manifest default.** It measures the budget
actually in force, which is the only number a ratio can be taken against. The two fields point
different ways on purpose: `arg_names_set` describes the request, `budget_tokens` describes the answer.

**`status` is `envelope.status`, or `"absent"` for the CLI's no-graph `status` answer.** That answer
prints the reason and exits `0` without an envelope, so there is no status to read. The rule is that
a record's `status` and the process exit code never contradict each other: a process that exited `0`
is never recorded as `"error"`. Consequences worth recognising in a report: a `call` record with
`payload_tokens: 0` and `status: "absent"` is that no-graph `status` line, while `payload_tokens: 0`
with `status: "error"` is a refusal that never reached a handler.

---

## 5. The privacy prohibition

A record holds numbers, closed-union identifiers and timestamps. It never holds source text, symbol
names, qualified names, file paths, `search.query`, `impact.paths`, cursor strings, error message
text, a repository remote URL, git identity, a hostname or the value of an environment variable.
Records keep carrying none of those on the served page either: what `sangkar telemetry --serve` names
is the **directory each section's records were read from**, because the operator asked for that
attribution, and the server binds loopback only (§6.1).

The reason is not that the file is local. A local-only file is still a file that gets pasted into an
issue, attached to a bug report or synced with a dotfiles repository, and a symbol name or a path is
the *content* of the user's private repository — the very thing the index exists to describe without
publishing. A record must therefore be useless to anyone but its owner: worth reporting on, worthless
to read.

Two consequences are named because both are easy to lose later:

- **`tool` is clamped** to a registered tool name or the literal `"unknown"`. `params.name` is
  whatever the client sent, and on the unknown-tool branch there is no tool to take a name from, so
  the record cannot carry a client-chosen string.
- **`stale_reason` is a closed enum**: a member of `STALE_REASONS`
  (`src/shared/domain/freshness.ts:61-67`) or `"other"`. The envelope types `stale_reason` as a plain
  string, and `trace` writes a repository path into it (`src/tools/trace/handler.ts:680-683`) — so a
  verbatim copy would put a private path in every stale record. The envelope is untouched; this is a
  recording rule, and the five `STALE_REASONS` values stay greppable in a report.

Two enforcement points keep the rule from being a matter of discipline:

1. **The builders copy an explicit whitelist.** `record.ts` reads each field by name out of
   `spec.args`, `spec.suppliedNames` and `spec.envelope`; nothing is spread. A payload value can
   therefore reach a line only if a field is added to the schema for it.
2. **`tests/telemetry.test.ts` deep-walks the serialised output.** The privacy tests serialise a
   record built from a payload full of a secret symbol name and a secret path, walk every scalar
   (`arg_names_set` excluded, because its members are names and that is its job), and assert that no
   scalar contains any of them — plus that the walk saw more than twenty scalars, so an empty walk
   cannot pass.

---

## 6. The verb and its flags

```
sangkar telemetry [--out <path>] [--serve] [--port <n>] [--host <addr>] [--since <YYYY-MM-DD|Nd>] [--repo-root <path>] [--data-dir <path>]
```

`--repo-root` and `--data-dir` are the global flags every verb takes; `--out`, `--serve`, `--port`,
`--host` and `--since` belong to this one.

| Behaviour | Rule |
|---|---|
| default output | `<dataDir>/telemetry/report.html` |
| relative `--out` | resolved against the repo root, not the process cwd — the same rule `--data-dir` follows, because a relative path is the caller's spelling of "inside this tree" |
| `--out` with no value, or `--out=` | usage error |
| `--since` default | all records |
| `--since YYYY-MM-DD` | one calendar day, floored to UTC midnight; a date that does not exist (`2026-02-31`) is malformed |
| `--since Nd` | N days before now, floored to UTC midnight; `N` an integer ≥ 1 (`0d` is refused rather than read as "today") |
| malformed `--since` | usage error: exit `2`, usage on stderr, no file written |
| zero records | the report is still written, with its empty-state panel, and the process exits `0` |
| a corrupt or partial line | skipped, counted as unparsed, and reported in both the report's `unparsed lines` total and the stdout line |
| `--help` / `-h` | prints the verb's usage and writes nothing — answered before any flag is validated |
| success | exactly one stdout line (`wrote <path> (<n> records, <m> unparsed lines)`) and exit `0` |
| write failure | the reason on stderr and exit `1` |

UTC is the calendar of the whole feature: the daily file name, the day boundary and `--since` all turn
at UTC midnight, so a host's timezone cannot split one day's records across two files.

The verb reads the telemetry directory and nothing else — no graph, no store, no kernel — so it
answers on a tree that was never indexed; `--serve` additionally reads the registry, which is a file.
Its dependencies are the config loader, `report.ts`, `html.ts`, `serve.ts` and the two exit-code
constants. It opens no outbound connection and imports no reader, and the only socket it ever owns is
the loopback listener below.

### 6.1 `sangkar telemetry --serve [--port] [--host]`

`--serve` answers over HTTP instead of writing a file. The two answers are exclusive: `--serve` with
`--out` is a usage error, because a caller that named both has not decided which one it wants.

| Rule | Value |
|---|---|
| default port | `7317` (`DEFAULT_REPORT_PORT`); `--port 0` asks the kernel for one, and the announcement prints the port actually bound |
| port shape | a base-10 integer in `0..65535`; `1e3`, `80.5`, `+80` and the empty string are refused as usage errors, not coerced |
| default host | `127.0.0.1` |
| allowed hosts | exactly `127.0.0.1`, `::1`, `localhost` — the literal allow-list `LOOPBACK_HOSTS`, matched as strings |
| any other host | refused **before** a socket exists: the reason and the usage block on stderr, exit `2` |
| routes | `GET /` renders the report, `HEAD /` answers status and headers only, `GET /healthz` answers `ok`, everything else is `404` (`HEAD /healthz` included: the contract names the method of every route) |
| `--since` | applies to the served page as it does to the file: one clock reading, one window, shared by every section and by the machine total |
| stdout | one line, `serving <url> (<n> repositories)`, written after the listener is bound and before the wait |
| stop | `SIGINT` or `SIGTERM` closes the listener and the verb exits `0` |

**Loopback only, and nothing else.** A report of every repository on the machine is not something the
network gets to read, so the host is checked against a literal allow-list before `Bun.serve` is
reached. `localhost` is in the list because it is the spelling a person types; a hostname that merely
*resolves* to loopback is not, because accepting one would need a DNS lookup whose answer can change
between the check and the bind.

**One section per source directory, plus a machine total.** The page is rebuilt on every request
rather than at bind time: a daemon serves for days, and a page rendered once would stop mentioning the
records written after it started. The registry is read per request, and each registered root becomes a
section labelled `<basename> — <root>` whose `source directory` line names the telemetry directory that
section's numbers were read from — the attribution says where a number came from, not what it
contains. A root with no telemetry directory is still a section, because "this repository recorded
nothing" is an answer, and dropping it would hide a tree that was registered but never used. The
`machine total` section comes first and is the only one that always exists: every section's records in
one fold, with the summed unparsed count, so an empty registry still states the window, the clock and
the reason the page looks empty.

---

## 7. What the report shows

Panels in the order `html.ts` renders them:

1. **header** — the window (`all records` or the `--since` value), `generated_at`, and totals:
   records, calls, index runs, sessions, unparsed lines, install ids, days.
2. **per tool** — calls, status mix, error codes, p50/p95/mean/max duration, truncated count, mean
   budget used, `eq` and `gte` counts.
3. **truncation** — truncated calls, truncation rate, reasons.
4. **relations** — `eq` ("total is complete"), `gte` ("total is a lower bound"), and the `eq` ratio
   of calls. This is the honesty panel: it says how often the graph could claim an exact total.
5. **coverage** — stale calls and the mean unresolved edges per call.
6. **argument names** — name → calls, from `arg_names_set`; names only, never values.
7. **unresolved edges per day** — one row per day with its index-run count and mean ratio, plus a bar
   scaled to the highest ratio in the window. The ratio column is the same number with the script off.
8. **index runs** — one row per run: timestamp, mode, exit code, duration, files seen/indexed/
   skipped/failed, files changed, unresolved ratio, db bytes, whether the generation advanced.
9. **sessions** — by session mode, by root kind, by repo size bucket.
10. **empty state** — last, and only when the window holds no record: it says which attempt would
    fill the panels in, and the layout does not move.

The file is self-contained: inline CSS, inline JS, the aggregate embedded as JSON, no network
reference of any kind, no external font and no external script. It opens from a USB stick on a
machine with the radio off and still draws its chart.

Two properties the renderer keeps deliberately: it never reads a clock (`generated_at` is a
parameter) and never uses `localeCompare` or `toLocaleString`, so two renders of the same directory
differ only where the directory did — which is what lets a report be diffed against the last one.

---

## 8. What this plan deliberately does not do

- **No network sink.** `TelemetrySink` is the seam; `jsonlSink` is its only implementation. Adding an
  uploader is one more implementation and a flag, not a change at any call site.
- **No pre-bucketing.** Records carry raw numbers — durations, token counts, byte sizes — and the
  report does the arithmetic. A bucketed number cannot be re-aggregated or re-percentiled later.
- **No retention and no pruning.** Daily `.jsonl` files are appended to and never rewritten, rotated
  or deleted. Growth is the operator's to manage; a silent delete in a telemetry path is a surprise
  nobody asked for.
- **No auto-open of the browser.** The verb prints the path and exits, or with `--serve` announces the
  URL and waits to be stopped; opening a window is not a side effect a report generator should have.
- **No sampling.** Every call is recorded. A sampled denominator would make the `eq` ratio and every
  percentile a claim about a different population than the one in the file.

One concurrency fact belongs here because it is the reason a flush is shaped the way it is: the server
and the worker it spawns hold separate recorders over the same daily file, and each flush hands its
whole group of lines to one `appendFileSync`. `O_APPEND` lands that buffer entire, so interleaving
happens between lines and never inside one — the files stay line-atomic without a lock, and a torn
final line is the only corruption a reader has to survive. The buffer is capped at 32 records and
flushed at 5 seconds of age, which is what bounds that cap.

---

## 9. Adding a field later

A field is added in one commit, in three places:

1. `record.ts` — the interface in serialisation order, and the builder that fills it from the
   whitelist.
2. `TELEMETRY.md` §4 — the table for that kind, in the same position, with the field's source.
3. `html.ts` — the panel that shows it, if any.

`v` stays `1` for an addition, because a reader ignores a key it does not need. It becomes `2` only
for a removal or a rename, and that bump means `report.ts` skips every older line — so a bump is a
decision to lose the history, not a formality.

A field that exists in a record but not in this document's table is a defect, and so is the reverse.
The schema is stated twice on purpose: once in types, once in prose a future reader can check the code
against, and the two drifting apart is exactly the failure this file exists to make visible.
