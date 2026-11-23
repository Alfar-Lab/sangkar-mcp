# Project layout

The tree, the recipes and the data model. Entry point: [`AGENTS.md`](../../AGENTS.md). The module rules that
make this shape mandatory are in [`rules.md`](rules.md).

## Layout

```
bin/sangkar.ts              entry point: mcp (default) | worker | index | telemetry | daemon | repo,
                            then a tool name — the name is the first token that is not a global flag
src/index.ts                composition root: builds ToolContext, registers tools, serves the repository
                            each call selects from the reader pool, registers the repo a successful
                            `index` run touched, installs SIGINT/SIGTERM, maps handler throws to
                            envelopes
src/shared/                 kernel + engine — the only cross-module import target
  config.ts                 AppConfig, repo-root autodetection, the machine home (`SANGKAR_HOME`), the
                            registry, config and daemon paths, withRepo() for a second repository
  global-config.ts          `<home>/config.json`: the installation's defaults — watch, watch_interval,
                            telemetry, the report server's host and port, and the two budgets. Written
                            with its defaults by the first run, read once per process; a broken field
                            falls back to the built-in default with one warning
  log.ts                    stderr JSON lines
  errors.ts                 AppError + the closed error-code union
  envelope.ts               the response contract (nine metadata fields + payload; error only for
                            status error/bootstrapping)
  budget.ts                 emit(): the SINGLE truncation point, the only place that mints `next`
  cursor.ts                 signed stateless cursors, generation-bound
  paths.ts                  normalizeRel, qn, prefixBounds (half-open; excludes the `src-old` sibling)
  instructions.ts           INSTRUCTIONS_TEXT + nextToolsFor() for `next_tools`
  registry.ts               `sangkar.json`: read, atomic write, register/remove, and selectRepo()'s order —
                            the named repo, then the pin, then cwd, then the only registered one
  repo-config.ts            `<repo>/.sangkar/config.json`: watch, watch_interval (clamped 1000..300000) and
                            telemetry, each narrowing the machine default; a bad file or field falls
                            back to that default with one warning, a missing file falls back silently
                            and reports `source: "global"`, recordingEnabled()
  pidlock.ts                the daemon's pid file: acquire, heartbeat, release, stale reclaim
  cli/args.ts               the one parser, driven by the manifests: `splitGlobals` for the four global
                            flags, `resolveCommand` for the name with globals on either side of it,
                            `parseArgs`/`parseCliArgs` for the call itself
  cli/render.ts             the only place a rendering is chosen: `renderToon` by default, `renderJson`
                            under `--json`, `mcpText` for the protocol result, `exitCodeOf`
  cli/repo.ts               the `repo` verb: add, remove, list, config
  store/schema.ts           Drizzle table definitions (eleven tables)
  store/migrations/         0000_init.sql + meta/ (drizzle-kit) and raw/0001_fts.sql (FTS5)
  store/migrate.ts          one runner, one ledger, forward-only
  store/db.ts               openReader/openWriter, PRAGMAs, prepared-statement LRU (cap 200)
  store/pool.ts             one reader per repository root, for the process that serves several; idle
                            eviction, no timer of its own
  store/lock.ts             create-exclusive write lock (Bun has no flock), stale reclaim
  store/publish.ts          stage → beginWrite → seal → publish → discard; withStage()
  store/read/               the GraphReader boundary: nodes, edges, metrics, coverage, files,
                            reader.ts (aggregate interface), context.ts (ToolContext)
  domain/                   resolve (ambiguity), neighbors (grouped), coverage (absence), risk, diff,
                            freshness
  telemetry/                records, JSONL sink, aggregator, HTML report; imports #shared/* only,
                            never a tool or the indexer
  telemetry/for-repo.ts     one tree's recorder: machine consent, then registration, then the repo file
  telemetry/html-multi.ts   the report page for every registered repository
  telemetry/serve.ts        the loopback listener that re-renders that page per request
src/daemon/                 the background process: one pid lock, one watcher per registered repo, one
                            loopback report server. May import `#shared/*`, `src/indexer/**` and its own
                            siblings, never `src/tools/**`
  types.ts                  DaemonStatus / RepoRuntime: the data one status answer carries
  lifecycle.ts              runDaemon (pid lock, tick, signals) plus start, stop and status
  reconcile.ts              one pass over sangkar.json that makes the live watchers match it
  main.ts                   the composition order: lock, reconciler, report server, loop
  cli.ts                    the `daemon` verb: run | start | stop | status
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
src/prompts/                INSTRUCTIONS.md, the agent-facing guide `INSTRUCTIONS_TEXT` ships; the suite
                            pins the two byte for byte, so it is data, not a module — no .ts lives here
tests/                      59 files, see `.agents/prompts/test-map.md`
agent/                      the distributable Agent Skills skill (SKILL.md) + install.sh for
                            Claude Code, Codex, OpenCode, Pi; restates the manifests, so a tool's
                            arguments, defaults or budget ceilings move here in the same commit
docs/                       the record — read before changing behaviour
  my-product/               the product design record: what this product must do and the contracts it
                            keeps — DESIGN.md (objectives, absence semantics, acceptance),
                            TOOLS-SPEC.md, DATABASE.md, TECH-SPEC.md, TELEMETRY.md, INDEXING-PIPELINE.md
  research/                 the product-related research record these decisions rest on: the two
                            reference implementations read from their own source (TOOL-SURFACE.md,
                            TOOL-USAGE-ANALYSIS.md, GRAPH-MODEL.md, STORAGE-ARCHITECTURE.md,
                            INDEXING-PIPELINE.md) and tool-outputs/, the 49 real MCP payloads
                            TOOL-OUTPUTS.md documents and the parity tests quote as oracles
.ppm/                       two completed PPM plans (see `AGENTS.md` § "Docs and plans")
.sangkar/graph.db           this repo's graph (git-ignored); `.codegraph/` is the pre-rename store
```

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
