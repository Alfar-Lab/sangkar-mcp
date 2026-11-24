# Approach

## Current state

Verified at `3592da3` by reading the source; the full inventory is summarised here, per file, enough to
size the port.

**Size.** `src/` + `bin/` = 76 files / 25 773 lines; `tests/` = 60 files / 31 338 lines. Largest units:
`src/index.ts` 1456, `src/tools/trace/handler.ts` 1193, `expand/handler.ts` 1157, `indexer/worker.ts` 1054,
`indexer/extract.ts` 946, `tools/summary/handler.ts` 909, `indexer/persist.ts` 830, `indexer/resolve.ts` 757,
`tools/impact/handler.ts` 695, `indexer/watch.ts` 671, `tools/index/handler.ts` 648.

**Runtime couplings.** `bun:sqlite` in six files (`store/{db,migrate,publish}.ts`, `indexer/{persist,worker}.ts`,
`shared/runtime.ts`); `drizzle-orm` reaches into a private field at `indexer/persist.ts:234-245`;
`web-tree-sitter` + `tree-sitter-wasms` with a hand-rolled WASM `dylink`→`dylink.0` rewrite at
`indexer/parse.ts:227-307`; `Bun.spawn` for git (`domain/diff.ts:189`, `indexer/worker.ts:763`,
`indexer/watch.ts:195,418`, `tools/index/handler.ts:214`, `daemon/lifecycle.ts:284`); `Bun.serve` only for
the report server (`telemetry/serve.ts:147`); `import.meta.dir`/`import.meta.url` for the migrations directory
(`store/migrate.ts:24`), the grammar bundle (`indexer/parse.ts:66`), the worker's own path (`indexer/watch.ts:62`),
the app root (`daemon/lifecycle.ts:42`) and the CLI entry (`tools/index/handler.ts:47`).

**Storage.** Eleven tables (`store/schema.ts`), one ledger (`schema_migration`), FTS5 external-content
`node_fts` over `node_fts_state` with `tokenize = 'unicode61 remove_diacritics 2'`
(`migrations/raw/0001_fts.sql:12-20`). Reader PRAGMAs `foreign_keys=ON, busy_timeout=10000,
cache_size=-16384, mmap_size=268435456`; writer adds `journal_mode=WAL, synchronous=NORMAL,
cache_size=-65536, journal_size_limit=67108864` and fails with `unsupported_filesystem` when WAL does not
take (`store/db.ts:35-50,183-203`). Prepared-statement LRU cap 200 per handle (`store/db.ts:70-141`).
Publish = `VACUUM INTO` seed → one `BEGIN IMMEDIATE` → checkpoint → `journal_mode=DELETE` →
`synchronous=FULL` → `quick_check(1)==ok` → `rename()` (`store/publish.ts:141-401`).

**Read path.** `GraphReader` aggregate in `store/read/reader.ts` (544 lines) over `nodes`, `edges`,
`coverage`, `files`; seven pure domain modules (`resolve`, `neighbors`, `coverage`, `risk`, `diff`,
`freshness`, `unresolved`); `store/pool.ts` holds one reader per root.

**Surfaces.** `bin/sangkar.ts` dispatches six reserved verbs (`mcp|worker|index|telemetry|daemon|repo`) and
treats every other token as a tool name; `src/index.ts` registers 7 tools in a fixed order and serves MCP
over `StdioServerTransport` with two handlers only (`tools/list`, `tools/call`), results framed as
`{content:[{type:"text",text}], isError}` with **no** `structuredContent`; errors are in-band envelopes,
never JSON-RPC errors. CLI renders TOON by default, JSON under `--json`, and maps
`error|bootstrapping → 1`.

**Not portable as-is:** `budget.ts` measures `JSON.stringify` bytes (`BYTES_PER_TOKEN=4`); `cursor.ts`
signs `base64url(JSON)` with a 16-byte truncated HMAC; `telemetry/record.ts` writes `v:1` JSONL and names the
runtime field `bun_version` — a name it keeps, because the report never reads it (`report.ts:277-311`) and a
rename would change a record shape the parity bar forbids changing.

## Design

1. **Nine crates, one workspace; the DAG is the module rules.** Rationale: `cargo` already refuses a
   dependency that is not declared, so the six `tests/boundary.test.ts` rules become a manifest property
   plus one greppable module rule (tools may not name each other). Verified direction: `store` never imports
   `domain`, `domain` imports `store`, `telemetry` imports `core` only — the crate edges are the existing
   import edges, not a new architecture.
2. **`rusqlite` 0.43 `bundled` is the store.** `bundled` compiles SQLite with `SQLITE_ENABLE_FTS5`, so
   `node_fts` works unchanged; the synchronous API matches the existing synchronous reader; `prepare_cached`
   gives the prepared path with an LRU whose capacity we set to 200. The `drizzle` private-field hack in
   `persist.ts` disappears because the transaction handle *is* the connection.
3. **`history` is not rewritten, it is reproduced.** Every SQL string, PRAGMA sequence, path template and
   exit code is copied from the TypeScript source rather than re-derived. Where the source has an oddity
   (the `argsHash` formats, the `unicode61 remove_diacritics 2` tokenizer, `--no-optional-locks`), the
   oddity is ported and the comment explains why, per `.agents/prompts/rules.md`'s comment rule.
4. **Two spikes gate the migration, and both are Phase 0.** TOON (`serde_toon_format` 0.2.0 implements the
   v4.1 working draft; the pin is 4.4.0) and MCP transport (`rmcp` 3.11.0 frames on `\n`,
   `async_rw.rs:148`, matching the TS SDK; the handshake must still be compared byte for byte). Each spike
   writes a decision file and, if the default candidate fails, names the fallback.
5. **Goldens, not a live oracle, are the durable specification.** During Phases 0–4 the TypeScript binary is
   the oracle and every comparison re-captures if needed; Phase 5 deletes it, so each comparison also writes
   `rust/tests/golden/**`. A golden is a committed file; a comparison script is a development aid.
6. **One shared database is the seam.** Identical SQL (embedded by relative path), identical lock formats,
   identical cursor wire format, identical telemetry layout. That is what makes "Rust indexes, TypeScript
   serves" a testable state in Phase 3 rather than a hope.
7. **The write path ports last and in dependency order.** `lang` → `discover` → `parse` → `extract` →
   `resolve` → `persist` → `worker` → `watch`. `extract.ts` is the single largest behaviour under test and is
   split across two tasks (declarations/imports, then calls/inheritance/references) with a serialized file.
8. **The read path ports first because it is where a usable artifact appears.** After Phase 2 the Rust
   binary is a real read-only server for graphs the TypeScript indexer produces; that is the first point a
   reviewer can exercise the port on their own repository.
9. **Packaging flips once, at Phase 5**, as platform packages + a Node shim, because a `#!/bin/sh` bin file
   breaks npm's Windows `.cmd` wrapper. No engine switch ships.
10. **The record shape is copied, field names included.** `bun_version` keeps its name and carries whatever
    runtime string the binary knows; the report never reads it (`report.ts:277-311`), so no v1 file on disk
    changes meaning. A rename was considered and dropped: it would force a `TELEMETRY.md` edit, which the
    parity bar forbids.

## Phases

### phase_0: foundation

Deliverable: the workspace and its skeletons, the boundary gate, `core`'s contract types (errors, log,
paths, envelope, budget, cursor with cross-implementation vectors, config/registry/pidlock), `store`'s
migrations runner, connection/PRAGMA/lock/publish primitives, the TOON and MCP spikes with written
decisions, the parity harness with three fixture repositories, and the repo-guidance sync for the `rust/`
tree. Sync barrier because every later crate compiles against these types, and because the spikes decide two
dependencies that Phase 2 cannot be written without.

### phase_1: read path

Deliverable: `sangkar-store::read` (nodes, edges, metrics, coverage, files, the `GraphReader` aggregate,
`ToolContext`) and `sangkar-domain` (resolve, neighbors, coverage, risk, diff, freshness, unresolved) with
row-level goldens captured from the TypeScript reader on the same `graph.db`. Sync barrier because the seven
handlers in Phase 2 read exclusively through these two crates, and a wrong `GraphReader` signature would be
paid for seven times.

### phase_2: tools and surfaces

Deliverable: the seven manifests and handlers, `emit()` wiring with the per-tool `argsHash` and `mint`, the
CLI argument parser and renderer, the store lifecycle the composition root owns (fingerprint, reopen after a
publish renamed a new inode over `graph.db`, `schema_too_new` refusal, bootstrapping→ready), the `repo` verb
and the index gate with its `--yes`, the composition root with its throw→envelope mapping, the MCP server with
`tools/list`/`tools/call` and the instructions text, and the `index` tool spawning a worker named by config
(the TypeScript worker during the transition). Sync barrier because Phase 3's indexer must be exercised
*through* this surface, and because the goldens captured here are what every later phase compares against.
The store lifecycle is in this phase rather than a later one because `tests/store-refresh.test.ts` encodes it:
a server that pins the reader it opened never serves a freshly published generation, and every one of the
seven tools would then answer from a stale graph.

### phase_3: indexer

Deliverable: the twelve-language table with a per-name grammar probe, discovery with skip reasons, the
tree-sitter pool, the table-driven extractor, binding, persistence (unit write, tombstones, metrics,
`tested_by`, FTS delta, generation), the worker's lock and incremental routing, and the polling watcher.
Sync barrier because publication is the only writer of a `graph.db`, and Phase 4's daemon owns the watchers
that spawn it.

### phase_4: daemon and telemetry

Deliverable: the record/sink/identity layer, the buffered recorder, the aggregator, the HTML report and its
multi-repository variant, the loopback report server and the `telemetry` verb, plus the daemon's pidlock
lifecycle, reconciler of registered repositories, composition order and `daemon` verb. Sync barrier because
cutover needs a daemon that can hold the pid lock alone and a report that reads both record generations.

### phase_5: cutover

Deliverable: one npm platform package (`sangkar-mcp-linux-x64-gnu`) with a Node shim that refuses every other
platform by name, the build/release workflow for the single `x86_64-unknown-linux-gnu` target, the binary
smoke test, deletion of the TypeScript tree and its five dependency pins, and the documentation rewrite
(`AGENTS.md`, `.agents/prompts/*`, `TECH-SPEC.md` §15, `README.md`, `docs/maintainers.md`,
`.agents/skills/build-n-deploy`, `agent/`). Sync barrier because it is the gate: nothing ships, and nothing
TypeScript is deleted, until every earlier gate is green.
