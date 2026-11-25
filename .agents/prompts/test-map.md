# Test map

Which tests guard which change. Entry point: [`AGENTS.md`](../../AGENTS.md).

One implementation, one gate: `cargo test --workspace` runs 71 suites and 978 tests, 1 ignored (the log
subprocess in `rust/crates/sangkar-core/tests/kernel.rs`). Integration tests live in `rust/crates/*/tests/`;
a unit test sits in the `#[cfg(test)]` module at the bottom of the module it covers, so a change to that
module moves its own test with it. The oracle is the committed goldens under `rust/tests/golden/`:
`scripts/parity/` captured them and cannot run now, because it imported the deleted TypeScript tree, and a
payload that moves on purpose moves its golden in the same commit.

| If you change… | These will tell you |
|---|---|
| module imports, the crate DAG, `rusqlite`'s location, a print macro outside a test, a tool naming another tool | `rust/crates/sangkar-boundary/tests/dag.rs`, with the rules in `rust/crates/sangkar-boundary/src/lib.rs` — each rule is proved against the real tree and against a synthetic break |
| the envelope, `emit()`, truncation, budgets, the `eq`→`gte` downgrade | `rust/crates/sangkar-core/tests/envelope_budget.rs`; the truncation point itself in `rust/crates/sangkar-core/src/budget.rs` |
| a cursor's bytes: key order, integer spelling, base64url padding, the page arithmetic, expiry | `rust/crates/sangkar-core/tests/cursor_interop.rs`, over the vectors in `rust/tests/vectors/cursor_vectors.json`; `rust/crates/sangkar-core/src/cursor.rs` |
| the error-code union, the one stderr line, the path and qn helpers | `rust/crates/sangkar-core/tests/kernel.rs`; `rust/crates/sangkar-core/src/log.rs` |
| config precedence, the tree `sangkar` autodetects, the machine registry, which tree a call selects | `rust/crates/sangkar-core/tests/config_registry.rs` |
| the instructions text shipped to an agent, and the `next_tools` table | `rust/crates/sangkar-core/tests/instructions.rs`, which pins `rust/crates/sangkar-core/assets/INSTRUCTIONS.md` byte for byte |
| the installation's settings file, one tree's settings file, its defaults and its clamp, the daemon's pid file | `rust/crates/sangkar-core/tests/settings_pidlock.rs`; `rust/crates/sangkar-core/src/pidlock.rs` |
| the process facts a telemetry record carries (`runtime_version()`, os, arch, git) | `rust/crates/sangkar-core/tests/runtime.rs` |
| schema, migrations, the ledger, `schema_too_new` | `rust/crates/sangkar-store/tests/migrate.rs`; `rust/crates/sangkar-store/src/migrations.rs`, `rust/crates/sangkar-store/src/schema.rs` |
| connection PRAGMAs, the prepared-statement cache, `write.lock` | `rust/crates/sangkar-store/tests/db_lock.rs`; `rust/crates/sangkar-store/src/db.rs`, `rust/crates/sangkar-store/src/lock.rs` |
| publish: the stage, the seal, the rename, the discard path | `rust/crates/sangkar-store/tests/publish.rs`; `rust/crates/sangkar-store/src/publish.rs` |
| the reader surface, method by method, against the TypeScript reader's own answers | `rust/crates/sangkar-store/tests/reader_golden.rs`, over `rust/tests/golden/reader/` (one dump per fixture) |
| the aggregate reader, the four analytics queries, the pool, the `ToolContext` a handler is handed | `rust/crates/sangkar-store/tests/read_reader.rs`; the context in `rust/crates/sangkar-store/src/read/context.rs` |
| the row shapes `rust/crates/sangkar-store/src/read/nodes.rs`, `rust/crates/sangkar-store/src/read/edges.rs` and `rust/crates/sangkar-store/src/read/metrics.rs` return | `rust/crates/sangkar-store/tests/read_nodes_edges.rs` |
| what was skipped and why, which files cover a prefix, the per-language census | `rust/crates/sangkar-store/tests/read_coverage_files.rs` |
| resolution, neighbours, expansion | `rust/crates/sangkar-domain/tests/resolve_neighbors.rs` |
| coverage arithmetic and the risk score | `rust/crates/sangkar-domain/tests/coverage_risk.rs` |
| the change set and the freshness verdict | `rust/crates/sangkar-domain/tests/diff_freshness.rs` |
| the unresolved taxonomy, its bucket order and the `material` predicate | `rust/crates/sangkar-domain/tests/unresolved.rs` |
| `status`'s trust answer: indexed, skipped with a reason, unknown, and the census | `rust/crates/sangkar-tools/tests/status.rs` |
| `summary`'s three scopes and the census that explains an unresolved total | `rust/crates/sangkar-tools/tests/summary.rs` |
| `trace`'s group order, ambiguity, near misses and page boundary | `rust/crates/sangkar-tools/tests/trace.rs` |
| `search`'s BM25 order and its slicing | `rust/crates/sangkar-tools/tests/search.rs` |
| `impact`'s digest, change set, risk and page | `rust/crates/sangkar-tools/tests/impact.rs` |
| `expand`'s three modes, including the SHA-256 check before `what: "source"` | `rust/crates/sangkar-tools/tests/expand.rs` |
| `index`: the child's exit status mapped onto an envelope, and the command coming from `AppConfig` | `rust/crates/sangkar-tools/tests/index.rs`; `rust/crates/sangkar-tools/src/index/manifest.rs` |
| the seven manifests as tools/list returns them — names, descriptions, argument schemas, order | `rust/crates/sangkar-cli/tests/mcp_surface.rs`, against the recorded session; the descriptor parser in `rust/crates/sangkar-cli/tests/args_render.rs` |
| the language table: every declared node type, probed against the grammar this binary loads | `rust/crates/sangkar-index/tests/lang.rs`; the rows in `rust/crates/sangkar-index/src/lang/registry.rs` |
| parsing, the stored error ranges, the parser pool's bound | `rust/crates/sangkar-index/tests/parse.rs`; `rust/crates/sangkar-index/src/parse.rs` |
| discovery: what is accepted, what is excluded, the reason for every skipped path | `rust/crates/sangkar-index/tests/discover.rs`; `rust/crates/sangkar-index/src/discover.rs` |
| declaration extraction: kinds, qns, `contains`, `is_test`, `signature`, `props`, `imports` | `rust/crates/sangkar-index/tests/extract_declare.rs` |
| site extraction: `calls` receivers, both inheritance kinds, `references`, sites with no name | `rust/crates/sangkar-index/tests/extract_sites.rs` |
| binding: the tier order, the edges it refuses, the ambiguity it keeps | `rust/crates/sangkar-index/tests/resolve.rs` |
| one file's rows, and every row a removal records | `rust/crates/sangkar-index/tests/persist_unit.rs` |
| the whole-graph derivations: degrees, `tested_by`, `tested_depth`, the FTS delta, the generation row | `rust/crates/sangkar-index/tests/persist_finalize.rs` |
| the worker's process contract: four exit codes, an untouched stdout, `meta` rows on failure | `rust/crates/sangkar-index/tests/worker.rs` |
| the watch loop's timing, debounce and single-spawn rule | `rust/crates/sangkar-index/tests/watch.rs` |
| the graph the writer produces, against the TypeScript indexer's census of the same corpus | `rust/crates/sangkar-index/tests/census.rs`, over `rust/tests/golden/census/polyglot.ts.json` and `rust/tests/golden/census/broken.ts.json` |
| the telemetry record's bytes, the install id's rotation, the sink's failure path | `rust/crates/sangkar-telemetry/tests/record_sink.rs`; `rust/crates/sangkar-telemetry/src/record.rs` |
| the recorder: its flush triggers, the per-session `seq`, the consent gate | `rust/crates/sangkar-telemetry/tests/recorder.rs` |
| the report's arithmetic: counts, percentiles, ratios, the torn-line skip rule | `rust/crates/sangkar-telemetry/tests/report.rs` |
| the report page — panel order, escaping, self-containment — and the machine-wide page | `rust/crates/sangkar-telemetry/tests/html.rs`; `rust/crates/sangkar-telemetry/src/html_multi.rs` |
| the report server: its three routes, the re-render per request, the `--since` refusal | `rust/crates/sangkar-telemetry/tests/serve.rs` |
| the `telemetry` verb's arguments and exits | `rust/crates/sangkar-cli/src/telemetry_cmd.rs` |
| the daemon's pid lock, start, stop, status against real processes | `rust/crates/sangkar-daemon/tests/lifecycle.rs` |
| one reconcile pass: missing, unwatched, retuned, forgotten | `rust/crates/sangkar-daemon/src/reconcile.rs`; the registry walk end to end in `rust/crates/sangkar-daemon/tests/integration.rs` |
| the daemon's composition order, and the four `daemon` CLI answers | `rust/crates/sangkar-daemon/tests/integration.rs` |
| the `status` object an operator reads, and its nullability | `rust/crates/sangkar-daemon/src/types.rs` |
| the argument parser, and both renderings of one envelope | `rust/crates/sangkar-cli/tests/args_render.rs` (TOON decoded back to the same value, one-line JSON under `--json`, `exit_code_of`) |
| the process entry point: six verbs, else a tool name; the streams and the exit code | `rust/crates/sangkar-cli/tests/dispatch.rs`; the post-conditions in `rust/crates/sangkar-cli/src/route.rs` |
| the `repo` verb: add, remove, list, config | `rust/crates/sangkar-cli/tests/repo_cmd.rs` |
| the index gate: its five facts, the question, the refusal, `--yes` | `rust/crates/sangkar-cli/tests/index_gate.rs` |
| the store lifecycle: what the handle opened, the reopen after a publish, `schema_too_new` | `rust/crates/sangkar-cli/tests/store_refresh.rs` |
| any tool's answer on the three fixtures — key order, values, array order, `next`, exit code | `rust/crates/sangkar-cli/tests/goldens.rs`, which replays all 57 goldens in `rust/tests/golden/` against the binary cargo just built |
| the MCP handshake, the framing, an in-band error, stdout discipline | `rust/crates/sangkar-cli/tests/mcp_surface.rs`, over `rust/tests/vectors/mcp_session.ndjson` and `rust/tests/vectors/mcp_session_ts_only.json`; the server itself in `rust/crates/sangkar-cli/src/mcp.rs` |
| the shipped artifact: a binary that answers, indexes and serves with no interpreter on `PATH` | `rust/crates/sangkar-cli/tests/binary_smoke.rs` |

The fixture repositories those parity rows replay are `rust/tests/parity/fixtures/polyglot`,
`rust/tests/parity/fixtures/broken` and `rust/tests/parity/fixtures/empty`; a new fixture is a capture, and
the capture harness is retired — see [`gaps-n-future-works.md`](gaps-n-future-works.md).

What this map does **not** cover — the four measured product gaps no suite catches — is in
[`gaps-n-future-works.md`](gaps-n-future-works.md).
