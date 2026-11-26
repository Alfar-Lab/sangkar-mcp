# Project layout

The tree, the recipes and the data model. Entry point: [`AGENTS.md`](../../AGENTS.md). The module rules that
make this shape mandatory are in [`rules.md`](rules.md).

## Layout

One implementation: the Cargo workspace under `rust/`. `sangkar` is one native binary — the crate
`rust/crates/sangkar-cli/`, entry point `rust/crates/sangkar-cli/src/main.rs` — and what ships it is the
release tarball `sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz`, which `install.sh` at the repository
root downloads and verifies against the release's `SHA256SUMS`.

The crate DAG, exactly as the gate asserts it: `core ← store ← domain ← tools`; `index` over
`core`+`store`+`domain`; `telemetry` over `core` only; `daemon` over `core`+`store`+`index`+`telemetry`,
never `tools`; `cli` the only crate that names `index`, `tools` and `daemon` together, and the only crate
with a binary.

```
rust/Cargo.toml                     the workspace: resolver 3, edition 2024, rust-version 1.100.0, one
                                    [workspace.package] version, `unused_dependencies = "allow"` so a
                                    crate may declare its DAG edge before the code that uses it
rust/Cargo.lock                     committed, lockfile v4 — the only pin file the build reads
rust/rust-toolchain.toml            channel 1.100.0, components rustfmt + clippy, profile minimal

rust/crates/sangkar-core/           the kernel: no workspace dependency, so every crate may use it
  rust/crates/sangkar-core/src/log.rs            the workspace's only stderr writer
  rust/crates/sangkar-core/src/envelope.rs       the response contract, built only through its constructors
  rust/crates/sangkar-core/src/budget.rs         emit(): the single truncation point, the only minter of `next`
  rust/crates/sangkar-core/src/cursor.rs         signed stateless cursors, generation-bound
  rust/crates/sangkar-core/src/errors.rs         the closed error-code union and the one error type
  rust/crates/sangkar-core/src/paths.rs          repo-relative paths, qn, half-open prefix bounds
  rust/crates/sangkar-core/src/instructions.rs   the one-screen guide and the `next_tools` table
  rust/crates/sangkar-core/src/config.rs         repo root, data dir, machine home; the environment, read once
  rust/crates/sangkar-core/src/global_config.rs  the installation's settings file and its defaults
  rust/crates/sangkar-core/src/repo_config.rs    one tree's settings: watch, watch_interval, telemetry
  rust/crates/sangkar-core/src/registry.rs       the machine registry and which tree a call selects
  rust/crates/sangkar-core/src/pidlock.rs        the daemon's pid file: acquire, heartbeat, release, reclaim
  rust/crates/sangkar-core/src/runtime.rs        the process facts telemetry records
  rust/crates/sangkar-core/assets/INSTRUCTIONS.md  the agent-facing text, embedded and pinned at 4087 bytes
  rust/crates/sangkar-core/build.rs              records RUSTC_VERSION for runtime_version()

rust/crates/sangkar-store/          the only crate that links rusqlite: SQLite, migrations, publish
  rust/crates/sangkar-store/src/schema.rs        the eleven table names and ten `meta` keys as constants
  rust/crates/sangkar-store/migrations/0000_init.sql     drizzle-kit output from the deleted schema.ts
  rust/crates/sangkar-store/migrations/raw/0001_fts.sql  the FTS5 tables Drizzle could not model
  rust/crates/sangkar-store/migrations/meta/    the drizzle-kit journal and snapshot behind 0000_init.sql
  rust/crates/sangkar-store/src/migrations.rs    both streams as `include_str!` + the ordered MIGRATIONS list
  rust/crates/sangkar-store/src/migrate.rs       one runner, one ledger, forward-only, schema_too_new
  rust/crates/sangkar-store/src/db.rs            openReader/openWriter, the PRAGMAs, the statement cache (cap 200)
  rust/crates/sangkar-store/src/pool.rs          one reader per repository root, idle eviction, no timer
  rust/crates/sangkar-store/src/lock.rs          create-exclusive write lock, stale reclaim
  rust/crates/sangkar-store/src/publish.rs       stage → seal → rename, and the discard path
  rust/crates/sangkar-store/src/read/            the GraphReader boundary: nodes, edges, metrics, coverage,
                                                 files, reader.rs (the aggregate), context.rs (ToolContext)

rust/crates/sangkar-domain/         the answers, over the reader
  rust/crates/sangkar-domain/src/resolve.rs      one free-text query to at most one node, or an honest ambiguity
  rust/crates/sangkar-domain/src/neighbors.rs    bounded-hop expansion, unbounded output
  rust/crates/sangkar-domain/src/coverage.rs     the absence contract's arithmetic
  rust/crates/sangkar-domain/src/risk.rs         one risk formula, so `summary` and `impact` cannot disagree
  rust/crates/sangkar-domain/src/diff.rs         the one change-set computation
  rust/crates/sangkar-domain/src/freshness.rs    is the generation a reader serves still true of the tree?
  rust/crates/sangkar-domain/src/unresolved.rs   the unresolved taxonomy, its census and the `material` predicate

rust/crates/sangkar-tools/          one module per tool: manifest (description + arguments) + handler
  rust/crates/sangkar-tools/src/registry.rs      the seven tools in tools/list order — the one table
  rust/crates/sangkar-tools/src/status/          is the graph current, and what does it cover
  rust/crates/sangkar-tools/src/summary/         what is going on here, in a few hundred tokens
  rust/crates/sangkar-tools/src/trace/           one symbol and its neighbours, grouped by relation
  rust/crates/sangkar-tools/src/search/          BM25 over the symbol index
  rust/crates/sangkar-tools/src/impact/          what a change set can reach, and whether it is tested
  rust/crates/sangkar-tools/src/expand/          more detail for one item from a previous result
  rust/crates/sangkar-tools/src/index/           spawn the worker and report what it published

rust/crates/sangkar-index/          the writer — runs in a spawned process, never in the server
  rust/crates/sangkar-index/src/lang/types.rs    the language layer as data: LangId, extensions, node-type lists
  rust/crates/sangkar-index/src/lang/registry.rs the twelve MVP rows and the two lookups built on them
  rust/crates/sangkar-index/src/parse.rs         one parser per language, a bounded pool
  rust/crates/sangkar-index/src/discover.rs      walk + exclusions; every skipped path with its reason
  rust/crates/sangkar-index/src/extract/         one cursor walk: declare.rs (nodes), sites.rs (call sites)
  rust/crates/sangkar-index/src/resolve.rs       binding: same file → imports → unique name → inherited method
  rust/crates/sangkar-index/src/persist/         unit.rs (one unit's rows), finalize.rs (the whole-graph derivations)
  rust/crates/sangkar-index/src/worker.rs        the index process: lock, routing, publish, exit codes
  rust/crates/sangkar-index/src/watch.rs         debounced polling that decides *when* a run is due

rust/crates/sangkar-telemetry/      usage records and the offline report; depends on core and nothing else
  rust/crates/sangkar-telemetry/src/record.rs    the one place a record line's shape is decided
  rust/crates/sangkar-telemetry/src/identity.rs  the install id and the daily session id
  rust/crates/sangkar-telemetry/src/sink.rs      where a serialised line goes (JSONL file, or nowhere)
  rust/crates/sangkar-telemetry/src/telemetry.rs the recorder: one per process, buffered, flushed on exit
  rust/crates/sangkar-telemetry/src/report.rs    reader + fold: a directory of JSONL into one aggregate
  rust/crates/sangkar-telemetry/src/html.rs      the report page, self-contained, no network
  rust/crates/sangkar-telemetry/src/html_multi.rs the machine-wide page, one section per repository
  rust/crates/sangkar-telemetry/src/serve.rs     the loopback listener that re-renders it per request
  rust/crates/sangkar-telemetry/src/for_repo.rs  one tree's recorder: consent, then registration, then the file

rust/crates/sangkar-daemon/         the background process; never depends on tools
  rust/crates/sangkar-daemon/src/types.rs        DaemonStatus / RepoRuntime: what one status answer carries
  rust/crates/sangkar-daemon/src/lifecycle.rs    pid lock, foreground loop, start/stop/status
  rust/crates/sangkar-daemon/src/reconcile.rs    one pass over the registry; live watchers match it afterwards
  rust/crates/sangkar-daemon/src/main.rs         the composition order: lock, reconciler, report server, loop
  rust/crates/sangkar-daemon/src/cli.rs          the `daemon` verb: run | start | stop | status

rust/crates/sangkar-cli/            the `sangkar` binary; the only crate that names tools + index + daemon
  rust/crates/sangkar-cli/src/dispatch.rs        the process entry point: six verbs, else a tool name
  rust/crates/sangkar-cli/src/args.rs            the parser, driven by the manifests' field descriptors
  rust/crates/sangkar-cli/src/render.rs          TOON by default, JSON under `--json`, and the exit codes
  rust/crates/sangkar-cli/src/route.rs           the post-conditions every envelope passes, and its spec
  rust/crates/sangkar-cli/src/mcp.rs             `rmcp` 3.11 over newline-delimited JSON-RPC on stdio
  rust/crates/sangkar-cli/src/repo.rs            the `repo` verb: add, remove, list, config
  rust/crates/sangkar-cli/src/index_gate.rs      whether an `index` run may start, and the question it asks
  rust/crates/sangkar-cli/src/store_lifecycle.rs what the handle opened, and when a newer generation landed
  rust/crates/sangkar-cli/src/telemetry_cmd.rs   `sangkar telemetry`: one file, or one listener

rust/crates/sangkar-boundary/       the gate: test-only, dependency-free, reads the tree as text
  rust/crates/sangkar-boundary/src/lib.rs        DAG, rusqlite location, print macros, tool isolation, markers
  rust/crates/sangkar-boundary/tests/dag.rs      every rule proved against the real tree and a synthetic break

rust/tests/parity/fixtures/         three checked-in repositories to index: polyglot, broken, empty
rust/tests/golden/                  the oracle: 19 envelopes per fixture (57), plus census/ and reader/ dumps
rust/tests/vectors/                 the cursor vectors the kernel is pinned to and the recorded MCP session
                                    the binary replays end to end
install.sh                          the installer the canonical URL serves: newest release (pre-releases
                                    included), sha256 verified, one binary into a `--bin-dir`
scripts/parity/                     the TypeScript harness that captured the goldens — it cannot run now
agent/                              the distributable Agent Skills skill + install.sh; restates the manifests,
                                    so a tool's arguments, defaults or budget ceilings move here in the same commit
.github/workflows/ci.yml            fmt, clippy and the whole workspace on 1.100.0, working-directory rust
docs/                               the record — read before changing behaviour
  docs/my-product/                  DESIGN.md (objectives, absence semantics, acceptance), TOOLS-SPEC.md,
                                    DATABASE.md, TECH-SPEC.md, TELEMETRY.md, INDEXING-PIPELINE.md, and
                                    RUST-MIGRATION.md (the decided port: parity bar, crate map, phases,
                                    and the gate each one proved)
  docs/research/                    the two reference implementations read from their own source, and the
                                    49 recorded MCP payloads TOOL-OUTPUTS.md quotes
.ppm/rust-migration/                the plan this tree was built under; `ppm plan_status` is the state
```

**One tree, one gate.** The TypeScript implementation is gone — its source tree, its test tree, its CLI
entry point, and the packaging that published it — and nothing outside this workspace answers.
The schema did not move with it — `rust/crates/sangkar-store/migrations/` holds the drizzle-kit output and
the hand-written FTS5 file, embedded by `include_str!`, so the two streams and the ledger are the same bytes
the old implementation applied. The assets moved into the crates they belong to: the migrations into
`sangkar-store`, the instructions text into `sangkar-core`.

What replaces the deleted test suite as an oracle is `rust/tests/golden/`: the envelopes the TypeScript
implementation produced for the three fixtures. `scripts/parity/` is the harness that captured them; it
imports the deleted tree, so it cannot run any more, and the committed goldens are the only oracle. A golden
moves only when the payload deliberately moves — see the recipes below.

`cargo test -p sangkar-boundary` is the gate that keeps the shape: it reads the nine manifests and every Rust
source and test file in the workspace as text, and fails on an undeclared DAG edge, `rusqlite` outside the
store, a print macro in a non-test source file, a tool naming another tool, or a phase marker. The rules, and
what each one means, are in [`rules.md`](rules.md).

## Recipes

**Add a language.** One registry row in `rust/crates/sangkar-index/src/lang/registry.rs` — `LangId`,
extensions, the grammar crates' `LANGUAGE`, the five node-type lists and the test-file regex — plus a `LangId`
variant in `rust/crates/sangkar-index/src/lang/types.rs` and **one grammar crate** in
`rust/crates/sangkar-index/Cargo.toml`. Nothing else changes: the extractor is table-driven, and
`rust/crates/sangkar-index/tests/lang.rs` probes every declared node type against the loaded grammar, so a
wrong type name fails there rather than in production.

**Add or change a tool.** One module — a directory under `rust/crates/sangkar-tools/src/` named for the
tool, with `mod.rs`, `manifest.rs` (name, description, argument descriptors) and `handler.rs` — one manifest
row in
`rust/crates/sangkar-tools/src/registry.rs`, and one golden under `rust/tests/golden/`. The handler must:
verify an incoming `cursor` before doing work, read only through `ctx.reader`, build the envelope only through
`sangkar_core::envelope`, and hand the payload to `emit(env, budget, { mint })`. Registration is the explicit
list in `registry.rs`, never directory scanning. If the tool takes arguments, `agent/SKILL.md` and
`docs/my-product/TOOLS-SPEC.md` move with it.

**Change a payload field.** The golden, the handler and `docs/my-product/TOOLS-SPEC.md` change in one commit,
the nine envelope fields stay intact, and the tool's own test gains the assertion. Payload keys are a public
contract; `rust/crates/sangkar-cli/tests/goldens.rs` compares key sets, key order and array order exactly.

**Add a telemetry field.** `rust/crates/sangkar-telemetry/src/record.rs`, `docs/my-product/TELEMETRY.md` §4
and the report panel in `rust/crates/sangkar-telemetry/src/html.rs` change in the same commit; a record field
the doc does not name is a defect. The record shape is pinned byte for byte by
`rust/crates/sangkar-telemetry/tests/record_sink.rs`.

**Add a table or column.** Write the file under `rust/crates/sangkar-store/migrations/raw/` — the 0000 stream
stays the drizzle-kit capture of the eleven tables and is never edited — add its constant and its
`MIGRATIONS` entry in `rust/crates/sangkar-store/src/migrations.rs`, raise the `schema_version()` assertion
pinned in that file, name the object in `rust/crates/sangkar-store/src/schema.rs` if it is a table, and update
`docs/my-product/DATABASE.md`. Say in the file's header why a hand-written statement was needed (FTS5, a
trigger, a bulk statement). A database written by a newer build is refused with `schema_too_new`, never
downgraded.

## Data model in one screen

`file` (path, language, sha256 = the staleness manifest, parse_status) · `node` (kind, name, qn as
`path::Scope.name`, lines, is_test, props JSON) · `edge` (`src`, `dst` node ids, kind, tier
`extracted|inferred`, file_id, line, local_name) · `node_metrics` (in/out degree, is_entry, tested_depth,
reachable_count) · `unresolved` (call sites that did not bind — this is what makes absence honest) ·
`not_indexed` (path + reason) · `tombstone` (removed symbols) · `generation` + `meta` · `node_fts` +
`node_fts_state` · `node_vector` (created, intentionally empty in the MVP).

Reading a graph: `SELECT kind, tier, count(*) FROM edge GROUP BY 1,2` tells you the recall story in one
query; `unresolved` next to `edge.calls` is the binding failure rate.
