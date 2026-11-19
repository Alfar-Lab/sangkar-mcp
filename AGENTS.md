# AGENTS.md

Guidance for an agent working **in** this repository. `README.md` is for users; this file is for whoever
edits the code.

## What this is

`sangkar-mcp` indexes one repository into a SQLite call graph and serves **seven read-only tools** over two
surfaces with identical behaviour: an MCP server (`sangkar mcp`, stdio) and a shell command
(`sangkar trace <name>`). Four answers: *is the graph current and what does it cover*, *what is connected to
X*, *what is going on here*, *what can this change reach*.

The repository is bound at startup — autodetected from `cwd` — so **no tool takes a `project` argument**.

## Non-negotiables

These five module rules are enforced by `tests/boundary.test.ts` and are a build failure, not a preference.
The rule ids below are the exact strings in a violation message.

| Rule | Meaning |
|---|---|
| `tool-isolation` | `src/tools/<tool>/**` may import only `#shared/*` and `./` siblings inside the same tool. **Tools never import each other.** |
| `indexer-isolation` | `src/indexer/**` may import only `#shared/*` and siblings inside `src/indexer`. |
| `shared-isolation` | `src/shared/**` may never import `src/tools/**` or `src/indexer/**`. |
| `drizzle-location` | `drizzle-orm` may be imported only under `src/shared/store/**`. |
| `root-privileges` | `src/index.ts` is the only module allowed to import the indexer or a tool handler. |

Other invariants that break the product when violated:

- **stdout is the MCP protocol.** Every log line goes to stderr through `src/shared/log.ts`. `console.log` is
  banned by biome's `suspicious/noConsole` rule (only `error` and `warn` are allowed) and the protocol is
  asserted end-to-end by `tests/mcp-surface.test.ts`.
- **Tools never touch a connection.** A handler receives `ToolContext` from `#shared/store/read/context.ts`
  (`reader`, `cursor`, `log`, `now`, `config`). It does not open a database, does not import `drizzle-orm`,
  does not read `process.env`: the two settings that do come from the environment are read once, in
  `src/shared/config.ts` (`SANGKAR_CURSOR_KEY`, `SANGKAR_TELEMETRY`), and `src/index.ts` reads
  `SANGKAR_NO_WATCH` deliberately because it is not a tool setting — it is this process deciding whether
  it owns a poller.
- **Never build a Drizzle query inside a handler or a loop.** Prepared statements only, via the cache in
  `store/db.ts` (LRU, cap 200). Measured on `bun:sqlite`: a re-built builder costs 42.7 µs per read, a reused
  prepared query 1.7 µs. `tests/acceptance.test.ts` fails if the prepared path is not ≥5× faster than the
  per-call builder.
- **Never publish a partial graph.** Write to a stage, `seal()` (checkpoint, `journal_mode=DELETE`,
  `synchronous=FULL`, `quick_check`), then `rename()` over the live file. `withStage()` guarantees the
  discard path. Readers keep serving the previous generation.
- **`total_relation: "eq"` is a claim.** It is legal only when coverage is complete, no `unresolved` rows
  exist in scope, the generation is fresh, and the payload was not truncated. `emit()` downgrades `eq` to
  `gte` itself when it truncates — do not "fix" that.
- **No URI filenames.** `new Database("file:…?immutable=1")` fails in `bun:sqlite`; readers use
  `{ readonly: true }` only.
- **Eleven tables, one ledger.** `src/shared/store/schema.ts` (Drizzle) is the typed source of truth;
  `migrations/0000_init.sql` is generated from it; FTS5 lives in `migrations/raw/0001_fts.sql`. One runner
  (`migrate.ts`), forward-only, never `drizzle-orm/bun-sqlite/migrator` in a production path.
- Keep the dependency pins: `@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4, `web-tree-sitter`
  0.27.2, `tree-sitter-wasms` 0.1.13. Run on Bun (1.4.4+); Node cannot run this (`bun:sqlite`, `Bun.spawn`,
  `bun build`).

## Layout

```
bin/sangkar.ts              entry point: mcp (default) | worker | index | telemetry, then a tool name
src/index.ts                composition root: builds ToolContext, registers tools, starts the watcher,
                            installs SIGINT/SIGTERM, maps handler throws to envelopes
src/shared/                 kernel + engine — the only cross-module import target
  config.ts                 AppConfig, repo-root autodetection, the one env var
  log.ts                    stderr JSON lines
  errors.ts                 AppError + the closed error-code union
  envelope.ts               the response contract (nine metadata fields + payload; error only for
                            status error/bootstrapping)
  budget.ts                 emit(): the SINGLE truncation point, the only place that mints `next`
  cursor.ts                 signed stateless cursors, generation-bound
  paths.ts                  normalizeRel, qn, prefixBounds (half-open; excludes the `src-old` sibling)
  instructions.ts           INSTRUCTIONS_TEXT + nextToolsFor() for `next_tools`
  cli/args.ts, cli/render.ts  shared CLI parsing and the human-line renderer
  store/schema.ts           Drizzle table definitions (eleven tables)
  store/migrations/         0000_init.sql + meta/ (drizzle-kit) and raw/0001_fts.sql (FTS5)
  store/migrate.ts          one runner, one ledger, forward-only
  store/db.ts               openReader/openWriter, PRAGMAs, prepared-statement LRU (cap 200)
  store/lock.ts             create-exclusive write lock (Bun has no flock), stale reclaim
  store/publish.ts          stage → beginWrite → seal → publish → discard; withStage()
  store/read/               the GraphReader boundary: nodes, edges, metrics, coverage, files,
                            reader.ts (aggregate interface), context.ts (ToolContext)
  domain/                   resolve (ambiguity), neighbors (grouped), coverage (absence), risk, diff,
                            freshness
  telemetry/                records, JSONL sink, aggregator, HTML report; imports #shared/* only,
                            never a tool or the indexer
src/tools/<name>/           one private module per tool: manifest.ts (name, description, schema) +
                            handler.ts. Seven tools: status, summary, trace, search, impact, expand, index
src/indexer/                the writer — runs in a spawned process, never in the server
  lang/types.ts, registry.ts  the language layer as DATA: extension map + class/function/import/call/
                            inheritance node-type lists for 12 languages
  parse.ts                  web-tree-sitter pool with a bounded per-language LRU
  discover.ts               walk + exclusions; records what was skipped and why
  extract.ts                one cursor walk driven by the node-type tables
  resolve.ts                binding: same-file → imports → unique name → inherited method
  persist.ts                write a unit; metrics, tested_by, FTS delta, tombstones, generation
  worker.ts                 the index process: lock, incremental routing, publish, exit codes
  watch.ts                  debounced polling that spawns the worker
tests/                      40 files, see the map below
docs/                       design record — read before changing behaviour
.ppm/                       two completed PPM plans (see "Docs and plans")
.sangkar/graph.db           this repo's graph (git-ignored); `.codegraph/` is the pre-rename store
```

## Run it

`bun` is **not on `PATH`** on this machine; the binary is `~/.bun/bin/bun`. `bun link` puts `sangkar` on
PATH, otherwise invoke `bun run bin/sangkar.ts`.

```sh
bun install
bun run bin/sangkar.ts index          # first index; until then every tool answers bootstrapping
bun run bin/sangkar.ts status         # five human lines; add --json for the envelope
bun run bin/sangkar.ts trace 'handleRequest' --depth 1
bun run mcp                           # MCP server on stdio
```

| Script | Does |
|---|---|
| `bun run test` | whole suite (≈47 s, 564 tests) |
| `bun run typecheck` | `tsc --noEmit` |
| `bun run lint` | biome over `src tests bin` |
| `bun run acceptance` | the 9 acceptance assertions, incl. latency and the builder-vs-prepared ratio |
| `bun run db:generate` / `db:drift` / `db:parity` | Drizzle migration + drift + schema parity |
| `bun run build` | `bun build --compile` with `--asset src/shared/store/migrations` — migrations are embedded in the binary, so a new migration file must stay under that directory |

Data lives in `<repoRoot>/.sangkar/`; override with `--data-dir`. Delete the stale `.codegraph/` directory
after the first `sangkar index` (`.gitignore` says as much).

## Test map — what guards what

| If you change… | These will tell you |
|---|---|
| module imports, layer rules | `boundary.test.ts` |
| the response shape, `emit()`, truncation, cursors | `envelope.test.ts`, `budget.test.ts`, `cursor.test.ts`, `contract.test.ts` |
| schema, migrations, indexes | `schema-parity.test.ts` (live DB vs `getTableConfig`), `db:drift` |
| connection, lock, publish | `lock.test.ts`, `publish.test.ts`, `smoke.test.ts` |
| the reader surface | `read.test.ts`, `read-files.test.ts` |
| resolution, neighbours, absence, risk, diff, freshness | `domain.test.ts`, `diff.test.ts`, `freshness.test.ts` |
| language tables or the parser pool | `lang.test.ts`, `parse.test.ts` |
| discovery, extraction, binding, persistence | `discover.test.ts`, `extract.test.ts`, `resolve.test.ts`, `persist.test.ts` |
| the index pipeline end to end | `worker.test.ts`, `index-integration.test.ts`, `watch.test.ts`, `parser-parity.test.ts` |
| one tool's payload or defaults | `tools-<name>.test.ts` |
| registration, profiles, stdio discipline | `mcp-surface.test.ts`, `tools-smoke.test.ts` |
| the CLI surface | `cli.test.ts`, `cli-args.test.ts`, `cli-render.test.ts` |
| the telemetry record shape, the consent switch, the daily id rotation and the privacy invariant | `telemetry.test.ts`, `telemetry-root.test.ts` |
| the report's aggregation, escaping and self-containment, and the verb | `telemetry-report.test.ts`, `cli.test.ts` |
| budgets and latency end to end | `acceptance.test.ts` |
| the shipped binary | `binary-smoke.test.ts` |

## Recipes

**Add a language.** `lang/registry.ts` + `lang/types.ts`: add the `LangId`, the extensions, the grammar WASM
name and the five node-type lists. Nothing else changes — the extractor is table-driven. `lang.test.ts`
probes every declared node type against the loaded grammar, so a wrong type name fails there rather than in
production.

**Add or change a tool.** Create `src/tools/<name>/{manifest,handler}.ts` (plus `tests/tools-<name>.test.ts`).
The handler must: verify an incoming `cursor` before doing work, read only through `ctx.reader`, build the
envelope only through `#shared/envelope`, and hand the payload to `emit(env, budget, { mint })`. Then register
it explicitly in `src/index.ts` — registration is an explicit import list, never directory scanning.

**Change a payload field.** Update `docs/TOOLS-SPEC.md` and the tool's `handler.ts` together, keep the nine
envelope fields intact, and add an assertion to that tool's test. Payload keys are a public contract.

**Add a telemetry field.** `src/shared/telemetry/record.ts`, `docs/my-product/TELEMETRY.md` §4 and the report
panel in `html.ts` change in the same commit; a record field the doc does not name is a defect.

**Add a table or column.** Edit `src/shared/store/schema.ts` → `bun run db:generate` → commit both
`0000_*.sql` and `migrations/meta/**` → `bun run db:parity`. If the object is not modelable (FTS5, a trigger,
a bulk statement), add a hand-written file under `migrations/raw/` instead and say why in its header.

## Data model in one screen

`file` (path, language, sha256 = the staleness manifest, parse_status) · `node` (kind, name, qn as
`path::Scope.name`, lines, is_test, props JSON) · `edge` (src/dst ids, kind, tier `extracted|inferred`,
file_id, line, local_name) · `node_metrics` (in/out degree, is_entry, tested_depth, reachable_count) ·
`unresolved` (call sites that did not bind — this is what makes absence honest) · `not_indexed` (path +
reason) · `tombstone` (removed symbols) · `generation` + `meta` · `node_fts` + `node_fts_state` ·
`node_vector` (created, intentionally empty in the MVP).

Reading a graph: `SELECT kind, tier, count(*) FROM edge GROUP BY 1,2` tells you the recall story in one
query; `unresolved` next to `edge.calls` is the binding failure rate.

## Current state and known gaps

All three plans are complete (`ppm plan_status`): `codegraph-mvp` 36/36, `cgraph-cli` 22/22,
`sangkar-telemetry` 22/22 — local usage records under `<dataDir>/telemetry/` (`SANGKAR_TELEMETRY=off`
disables them) and `sangkar telemetry`, the offline HTML report they are read with. The suite is green.

Three things the tests do **not** catch, re-measured on 2026-11-19 from a scratch index of this tree
taken after phase 1 of `issue-remediation` (`index --data-dir /tmp/…`): **1 640 nodes**, **9 347 edges**
(5 894 `calls`, 1 531 `contains`, 1 102 `references`, 536 `tested_by`, 275 `imports`, 9 `inherits`),
**11 519 unresolved** call sites and **115 `not_indexed`** paths — all `unsupported_ext`, no source file
among them. Relative imports bind now: **0** unresolved rows keep `import:` evidence against a `./` or
`../` target, against the pre-merge baseline of **71**. Treat the three below as the open work:

1. **`kind: "test"` is not applied.** This repo has **547** nodes with `is_test = 1` but only **12** with
   `kind = "test"`; **403** of the 547 are labelled `function`. `src/indexer/extract.ts` reaches `test` only
   through `#kindOf`'s `readsAsTest` — the `TEST_NAME` regex on the declaration's own name — never from
   `is_test`, so a test body with an ordinary name keeps `function`. Every payload that renders `kind`
   mislabels test functions, and `search(kind='test')` is nearly empty.
2. **`kind: "method"` is nearly unused.** **422** nodes are labelled `class` and only **31** are `method`;
   all 31 are the members of five classes (`FileExtractor` 13, `SymbolIndex` 12, `StatementCache` 4,
   `CliUsageError` 1, `AppError` 1), so class members do come out as `method` — most of the 422 are
   TypeScript interfaces, type aliases and enums, which `src/indexer/lang/registry.ts` lists in `classTypes`.
   `parser-parity.test.ts` cannot see this because it asserts `function ∪ method`.
3. **Binding recall is the product bottleneck.** This repo: **11 519 unresolved** call sites against
   **5 894** `calls` edges (≈66 %), and only **368** of **1 640** nodes have a test within two hops.
   `total_relation` is therefore `gte` almost everywhere, and `test_gaps` under-reports. Until recall
   improves, absence answers are honest but thin.

`parser-parity.test.ts` also deliberately does not gate total edge count against CRG (130 044 vs ~62 000 on
the shared corpus) because the edge taxonomies differ. If you add edge kinds, decide whether to gate them.

## Docs and plans

- `docs/my-product/` — the design record: `DESIGN.md` (objectives, contracts, absence semantics, acceptance),
  `TELEMETRY.md` (record schemas, the privacy prohibition, the verb, the report),
  `TOOLS-SPEC.md` (per-tool params, payload keys, budgets), `DATABASE.md` (tables, index plan, publish
  protocol), `TECH-SPEC.md` (runtime facts, module isolation, Drizzle, parser strategy), `GRAPH-MODEL.md`,
  `INDEXING-PIPELINE.md`, `STORAGE-ARCHITECTURE.md`.
- `docs/research/` — the two reference implementations' analysis and the real MCP payloads captured from them
  (`tool-outputs/`). Those numbers are the oracles the parity tests use.
- `docs/INSTRUCTIONS.md` — the agent-facing text shipped as `INSTRUCTIONS_TEXT`.
- `.ppm/codegraph-mvp/`, `.ppm/cgraph-cli/` — completed plans; `.ppm/sangkar-telemetry/` — in progress; task
  JSON is the execution record for each. Do not edit
  `.ppm/*/tasks/*.json` by hand.
- `.ppm/issue-remediation/` — the plan that closed issues #1, #2, #4, #6, #7, #8 and the remainder of #9:
  relative imports bind, every read answers from a refreshed store, `coverage.tests` and the repo-wide
  skipped total are published, and the two-worktree registration in `README.md` is documented.

Changing behaviour that a doc specifies means changing the doc in the same commit.

## Conventions

- Conventional commits, scoped, with `!` for behaviour/repo-structure changes (`refactor!: move the app tree
  to the repository root`).
- `snake_case` in SQL, `camelCase` in TypeScript, mapped explicitly in `store/schema.ts`.
- Comments explain **why** (a constraint, a measured number, a failure seen in production), never what the
  next line does. The existing code is a good model: see `config.ts` on repo detection, `bunfig.toml` on test
  discovery, or the direction note on `tested_by` in `persist.ts`.
- Biome for formatting; do not hand-format.
- Never weaken an assertion to make a test pass. If a test encodes a contract and the contract changed, change
  both and say so in the commit body.

## Definition of done

```sh
bun run typecheck && bun run lint && bun run test && bun run db:drift
```

Plus, when behaviour changed: the tool's own test, `docs/TOOLS-SPEC.md` if a payload moved, and one line in
the commit body naming the invariant you touched. `bun run acceptance` when latency, budgets or the prepared
statement path could have moved.
