# Migration: sangkar-mcp to Rust

**Status:** Complete. Phase 5 closed the cutover: one implementation, the Rust workspace under `rust/`,
released as one tarball on a GitHub Release — the Linux x64 binary and nothing else — and installed by
the root `install.sh` (§7).
**Decision date:** 2026-11-24 · **Base commit:** `3592da3` · **Version migrated:** `0.6.0-beta.1`.
**What moved:** 76 TypeScript files / 25 773 lines in `src/` and `bin/`, 60 test files / 31 338 lines in
`tests/`, 11 SQLite tables, 12 languages, 7 tools, 2 surfaces.
**What replaced it:** nine crates under `rust/` — 43 699 lines under their `src` directories (unit tests
included) and 44 915 under their `tests` directories — 978 tests in 71 suites, and one binary: 34 416 888
bytes as cargo builds it, 31 056 160 stripped.

**Packaging note (2026-11-26).** npm packaging is retired: `package.json`, `npm/`, `bin/sangkar.js` and
`rust/scripts/package.sh` are deleted, and nothing is published to a package registry. The release is a
GitHub Release tarball — `sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz` holding one binary, plus
`SHA256SUMS` — installed by the root `install.sh`. Launcher, platform-package and `npm pack` statements
below are the record of the era that ended there; they are kept as history, not as live statements.

The executable plan is `.ppm/rust-migration/`, 73 tasks over six phases; `ppm plan_status` is its state,
not this page. This page is the record of what was built, what each phase's gate observed, and every
place the result differs from the plan (§12).

---

## 1. Why

The product is right; the runtime it was written in was the cost. Three of the four arguments below were
already written down in [`TECH-SPEC.md`](TECH-SPEC.md) §15 as reasons the artifact could not ship well.

| Goal | What it fixed | Evidence at decision time |
|---|---|---|
| **A small artifact with no interpreter** | `npm i -g sangkar-mcp` then needed Bun ≥ 1.4.4 (102 MB) on the user's machine; `bun build --compile` measured **104–160 MB** because Bun embedded itself plus 12 WASM grammars | `package.json` `engines.bun`, `docs/maintainers.md` § Distribution then |
| **A grammar set that stops being a payload** | Native `tree-sitter` grammar crates are compiled into the binary, replacing `tree-sitter-wasms` (12 × 1–3 MB) plus a hand-rolled WASM dylink section rewrite | the rewrite existed only because of `web-tree-sitter` |
| **One index pass instead of a parser pool behind a FIFO queue** | All parse work was serialized on one queue, because the WASM parser was single-threaded and shared | the pool cached six languages, and "everything serialized on one FIFO queue" was its own comment |
| **Removing a whole layer of private-API coupling** | `drizzle-orm`'s `bun-sqlite` driver leaked into `persist.ts` through a private field, and `bun:sqlite`'s statement-close semantics dictated the entire `WriteScope.close()` ordering | `persist.ts` (`tx.session.client`), `publish.ts` |

All four were removed, and the result is measured rather than hoped for: the shipped artifact is one
binary with no interpreter in its path, eleven grammar crates compiled in and no WASM payload, one index
pass with no cross-process parse queue, and no ORM — `rusqlite` sits in `sangkar-store` alone. The promise
the plan deliberately did **not** make was speed of the seven query tools, and the record keeps that
promise: what the migration changed is the artifact and the index pass, and the goldens are what say the
answers did not move.

## 2. Non-goals

Every one of these was a decision, not an omission. Each would have broken a compatibility claim made in
this document, or is a feature the design record deliberately refuses.

- **No schema change.** Eleven tables, one ledger, `include_str!` embedding of the exact same `.sql`
  files. `schema_migration`, `meta`, and `node_fts`'s FTS5 tokenizer (`unicode61 remove_diacritics 2`)
  are byte-identical, so one `graph.db` stayed readable by both implementations through the whole
  migration.
- **No payload redesign.** Envelope keys, payload keys, the 12 error codes, the `eq`/`gte` rule,
  cursors, exit codes: unchanged. (§3 defines what "unchanged" means precisely.)
- **No new languages, tools, or arguments.** The 12 language rows moved as data, not as an opportunity.
- **No `notify`-based watching.** The watcher's signal is git state (`HEAD` + `status -z -uall`), not
  filesystem events; a polling loop is the correct design and stayed.
- **No async database layer.** `rusqlite`'s synchronous API matches the existing synchronous reader; the
  async surface stayed where it already is (MCP transport, daemon loop, report server).
- **No LSP, no embeddings, no analytics.** Already refused in [`DESIGN.md`](DESIGN.md) §3; a rewrite is
  the worst possible moment to reopen them.

All six held. The one product gap the port introduced is in discovery, not in the contract — a nested
`.gitignore` is not read (§12.8) — and it is recorded where it belongs, in
[`.agents/prompts/gaps-n-future-works.md`](../../.agents/prompts/gaps-n-future-works.md), instead of being
patched mid-port.

## 3. The parity bar: semantic, with three hard edges

Chosen over byte-identical output, because the TOON encoder (see §8 R1) would not be the same
implementation. What follows is the whole contract; anything not listed was free.

**Must be identical.**

1. **Tool surface.** The seven names, their descriptions' first sentence, every parameter's name, type,
   default and clamp, and the JSON schema shape tools/list returns (`{type:"object",properties,required?,examples?}`,
   `enum` → `{type:"string",enum}`, `min`/`max` → `minimum`/`maximum`). The manifest table in
   `rust/crates/sangkar-tools/src/registry.rs` is that spec, and `rust/crates/sangkar-cli/tests/mcp_surface.rs`
   holds it field for field against the recorded session.
2. **Envelope.** The nine metadata keys plus `payload`, always present; `error` if and only if `status` is
   `error` or `bootstrapping`; the five statuses; the four truncation reasons; the twelve error codes;
   `relationFor()`'s rule and `emit()`'s downgrade of `eq` to `gte`.
3. **Array order.** This is the edge people miss. Cursors are *positions into a generation's stable
   ordering*, and the two implementations had to mint the same token for the same payload — verifiable
   only when `SANGKAR_CURSOR_KEY` is set, because the key is otherwise 32 random bytes per process. A
   differently-sorted list is not a cosmetic difference; it silently serves the wrong page. Every ordering
   the code performs is part of the contract, including `SQLite`'s implicit `rowid` order where no
   `ORDER BY` is written. The vectors recorded to pin it are still the pin:
   `rust/crates/sangkar-core/tests/cursor_interop.rs` over `rust/tests/vectors/cursor_vectors.json`.
4. **JSON rendering.** `--json` output is one compact line plus `\n`. The text inside an MCP tools/call
   result is the same rendering with the trailing newline removed — TOON by default on both surfaces, and
   JSON under `sangkar mcp --json` — so the two surfaces cannot disagree about a payload. The goldens
   compare the JSON bytes, `next` included.
5. **Numbers and units.** `BYTES_PER_TOKEN=4`, `NEXT_TOKEN_RESERVE_CHARS=384`, per-tool default budgets,
   integer/`null` totals, and the `unresolved_material` / `candidates_shown` optional keys.
6. **Process contract.** Exit codes (`0`, `1`, `2`, and the worker's `0`, `1`, `2`, `75`), stdout carrying
   only the protocol or the rendered answer, and every diagnostic on stderr through one `log` module. No
   Rust path emits `70`: its only producer was the missing-interpreter preflight, which a static binary
   cannot fail, and the launcher that emitted it is deleted, so nothing under `rust/` emits `70` (§12.6).
7. **Files and locks.** `graph.db`, `.stage.<12 hex>`, `write.lock`, `daemon.pid`, `<home>/sangkar.json`,
   `<dataDir>/config.json`, `<dataDir>/telemetry/<YYYY-MM-DD>.jsonl`, `install_id.json`: same paths, same
   JSON shapes, same staleness rules. The two implementations took turns on one tree and neither had to
   migrate a file.

**May differ.**

- TOON whitespace, quoting, delimiter and table-vs-list choices — **provided** the reference decoder
  (`@toon-format/toon` 4.4.0) reads the Rust output back into a value structurally equal to the envelope.
  That round-trip is a test, not a hope: the spike measured it (§8 R1), and
  `rust/crates/sangkar-cli/tests/args_render.rs` keeps the in-product half by decoding the rendered TOON
  back to the same value.
- Log line ordering, stderr wording of refusals, `health.statement_cache_entries`, `duration_ms`,
  `indexed_at`/`stale_checked_at` values, and any measurement: same shape, different numbers.
- The `status` payload's `health.last_publish_ms` and DB size are read, not reproduced.
- Performance characteristics — and §9 records the one performance assertion this bar intended to keep,
  and did not (§12.11).

**Explicitly released.** The TypeScript suite was the *specification* during Phases 0–4 and was deleted at
cutover (Phase 5). Its durable replacement is the captured-golden harness in §9 — so "the tests still
pass" means "the goldens still pass", not "some TypeScript still runs".

## 4. Architecture as built

Nine crates in one workspace under `rust/`. The crate DAG *is* the module-rule table from
[`.agents/prompts/rules.md`](../../.agents/prompts/rules.md); `sangkar-boundary` (a test-only crate)
re-derives the DAG from `cargo metadata` and fails the build on any edge that is not listed here.

```
rust/
  Cargo.toml               workspace, one shared version, edition 2024, toolchain 1.100.0
  rust-toolchain.toml      pinned, components = ["rustfmt", "clippy"]
  crates/
    sangkar-core/          kernel: envelope, budget, cursor, errors, log, paths, instructions,
                           config, global-config, repo-config, registry, pidlock, runtime
    sangkar-store/         the only crate that links rusqlite: schema, migrations (include_str!),
                           migrate, db (PRAGMAs + prepared cache), publish, lock, pool, read/*
    sangkar-domain/        resolve, neighbors, coverage, risk, diff, freshness, unresolved
    sangkar-tools/         one module per tool: manifest.rs (name, description, input) + handler.rs
    sangkar-index/         lang/{types,registry}, discover, parse, extract, resolve (bind),
                           persist, worker, watch
    sangkar-telemetry/     record, telemetry, sink, identity, report, html, html-multi, serve
    sangkar-daemon/        types, lifecycle, reconcile, main, cli
    sangkar-cli/           the `sangkar` binary: verb dispatch, args, render, repo, index-gate
    sangkar-boundary/      test-only: dependency-DAG and stdout-discipline assertions
```

| Rule in `rules.md` | How Rust enforces it |
|---|---|
| `tool-isolation` | `sangkar-tools` depends on `core`+`store`+`domain`; the boundary scan refuses a tool module naming another (`crate::<another tool>` under `rust/crates/sangkar-tools/src/`) and refuses `crate::tools` anywhere |
| `indexer-isolation` | `sangkar-index` depends on `core`+`store`+`domain` only |
| `shared-isolation` | `core`, `store`, `domain`, `telemetry` never depend on `tools`, `index`, `daemon`, `cli` |
| `drizzle-location` | replaced by **`rusqlite`-location**: `rusqlite` appears in `sangkar-store`'s manifest only |
| `root-privileges` | `sangkar-cli` is the only crate that depends on `index`, `tools` and `daemon` together |
| `daemon-privileges` | `sangkar-daemon` depends on `core`+`store`+`index`+`telemetry`, never `tools` |
| stdout is the protocol | `println!`/`print!` banned outside tests; `rust/crates/sangkar-core/src/log.rs` writes stderr; the boundary scan greps, clippy denies, and `rust/crates/sangkar-cli/tests/mcp_surface.rs` asserts it end to end |

`ToolContext` keeps its TypeScript field names — `reader`, `cursor`, `log`, `now`, `config`,
`db_size_bytes` — in `rust/crates/sangkar-store/src/read/context.rs`, which is why `store` sits below
`domain`: the same direction the TypeScript imports already took (the domain modules imported the store,
never the reverse; verified by grep before this document was first written). Two of the types are shaped by
the language rather than by the port: `cursor` is an `Rc<dyn CursorFacade>`, because Rust needs the trait
object where TypeScript had structural typing, and `db_size_bytes` is a `u64`, because a context is only
built around an open handle.

## 5. Dependency replacements

Every pick was checked against the registry on 2026-11-24. The version in the "Shipped" column is what
`rust/Cargo.lock` holds.

| Replaced (pin at decision time) | Shipped (pin) | Why this one | Rejected |
|---|---|---|---|
| `@modelcontextprotocol/sdk` 1.32.2 | `rmcp` 3.11 (stdio) | Official Rust SDK; its stream transport frames on `\n` exactly like the TS SDK's read buffer, which is the assumption the recorded session encodes (`rust/tests/vectors/mcp_session.ndjson`) | Hand-rolled JSON-RPC — kept only as the fallback, and never built (§8 R2) |
| `drizzle-orm` 0.45.4 + `bun:sqlite` | `rusqlite` 0.43, feature `bundled` | `bundled` compiles SQLite with the FTS5 and JSON1 flags `node_fts` needs; synchronous API, `prepare_cached` semantics close to the existing LRU | `sqlx` (async, runtime-heavy), `turso` (immature FTS5 story) |
| `web-tree-sitter` 0.27.2 + `tree-sitter-wasms` 0.1.13 | `tree-sitter` 0.27 + eleven grammar crates for the twelve language rows | Grammars become compiled-in code, which removes the 12 MB WASM payload and the FIFO parse queue. Node-type names come from the **grammar** crates, whose releases are independent (e.g. `tree-sitter-java` 0.23, `tree-sitter-c-sharp` 0.24) — which is why §8 R3 probes every declared name instead of assuming it | Keeping WASM behind `wasmtime` — keeps the payload and the queue |
| `@toon-format/toon` 4.4.0 | `serde_toon_format` 0.2 | Serde-native, and measured sufficient: fifteen contract cases and ten real envelopes round-trip through the reference decoder, and the ten come out byte-identical (§8 R1) | Vendoring the 61.7 KB TS encoder — named, not needed |
| `Bun.spawn` (git, worker, daemon) | `std::process::Command` + `tokio::process` | Direct mapping; the git prologue (`--no-pager --no-optional-locks -c credential.helper= …`) is copied verbatim | `git2` (would change what `status --porcelain` means) |
| `Bun.serve` (report) | `tiny_http` 0.12 | One loopback listener, text/html only, no routing framework — and it serves the three literal routes the contract names | `hyper` (the other half of the plan's either/or), `axum` (a whole tower stack for one GET) |
| `node:crypto` HMAC/SHA-256 | `hmac` + `sha2` + `base64` + `constant_time_eq` | Byte level: 16-byte truncated tag, `base64url` unpadded, MAC over the encoded segment | — |
| `node:readline` (index gate prompt) | `std::io::stdin().read_line` | One y/N question | `dialoguer` |
| `Bun.CryptoHasher("sha256")` | `sha2` | Used for file digests and per-tool `argsHash` | — |
| Biome + `tsc` | `rustfmt` + `clippy -D warnings` | Same gate position in the loop | — |
| `bun test` | `cargo test` | Integration tests drive the built binary; unit tests live beside the module | — |

Pinned by `rust/Cargo.lock`, committed: every dependency version in the build comes from it, and the release
workflow builds with `--locked`.

## 6. Strangler mechanics

The database was the seam. Both binaries read and wrote the *same* `graph.db`, with the *same* schema,
under the *same* lock file, so either one could answer at any point in the migration. Seven mechanisms made
that true; the first six were Phase 0 tasks, because every later phase depended on them.

1. **One copy of the SQL.** While both existed, the Rust crate embedded the TypeScript directory by
   relative path; the files then moved into the crate, and
   `rust/crates/sangkar-store/src/migrations.rs` now embeds them by `include_str!` from
   `rust/crates/sangkar-store/migrations/`. One file, one reader, zero drift; the ledger and both streams
   are the bytes the old implementation applied.
2. **One lock, one stage, one publish.** `write.lock` (`{path,pid,startedAt}`), `.stage.<12 hex>`,
   `VACUUM INTO`, `wal_checkpoint(TRUNCATE)` → `journal_mode=DELETE` → `synchronous=FULL` →
   `quick_check(1)` → `rename()`. Copied, not improved, and pinned by
   `rust/crates/sangkar-store/tests/publish.rs`.
3. **Cursor interoperability, proven by test vector.** `base64url(JSON{generation,tool,argsHash,position,repo})`
   `.` `base64url(HMAC-SHA256(key, that segment)[0..16])`. A fixed key and payload produce the *identical*
   token string, which is what the six TypeScript-minted vectors are compared against byte for byte, and
   the refusal path is pinned by code and by order. This fixed JSON key order, integer formatting and
   base64url padding — the three things that would otherwise have drifted silently — and the vectors
   still do.
4. **One worker command, named by config.** `AppConfig` gained `worker_cmd` (default the current
   executable), overridable during the transition so the Rust `index` tool could spawn the *TypeScript*
   worker until Phase 3 landed. It did **not** go away at cutover, as this section once said it would: it
   is how `index` and the daemon name the child today, override `SANGKAR_WORKER_CMD`, and
   `rust/crates/sangkar-tools/tests/index.rs` is what holds it to `AppConfig` rather than to anything the
   handler can read.
5. **One daemon at a time, in a stated order.** `daemon.pid` content and staleness rules were unchanged,
   so the handover was: stop the TypeScript daemon, start the Rust one, and let the stale reclaim handle
   a crash. There was never a moment where both held the file.
6. **Goldens captured while the oracle still existed.** Every A/B comparison wrote the TypeScript answer to
   `rust/tests/golden/`; the Rust suite then compares against the file forever. This is what survived
   Phase 5's deletion of TypeScript (§9). The capture harness itself is retired with the tree it imported
   (§12.9).
7. **One platform at first, and the refusal is a feature.** The npm package declared exactly one platform
   package (`sangkar-mcp-linux-x64-gnu`). On macOS or Windows the launcher found nothing and said which
   platform it wanted, that only Linux x64 was published, and which version to pin to stay on the
   TypeScript build. Adding a platform meant one `optionalDependencies` entry, one
   `npm/linux-x64-gnu`-shaped directory and one row in the release matrix, never a rewrite of the
   launcher. That mechanism retired with npm packaging: the refusal is `install.sh`'s platform gate now,
   and adding a platform is the deferred work `TECH-SPEC.md` §15 lists.

The transition knob that did *not* exist: no `--engine=bun|rust` flag on the shipped binary. The two
binaries were separate artifacts (`bun run bin/sangkar.ts` versus `cargo run -p sangkar-cli`), and no
user-facing switch ever shipped.

## 7. Phases and gates

Six phases, each ending in a gate that had to be green before the next started. Phase boundaries were sync
barriers, not topics: the next phase's tasks consumed a contract the previous one made compile. The gate
column is the plan's, restated without the deleted paths it cited; the last column is what closed it and
what is still in the tree as proof.

| Phase | Deliverable | Gate (observable) | What closed it, and what still proves it |
|---|---|---|---|
| **0 Foundation** | Workspace, boundary crate, crate skeletons, cursor/envelope/budget ports, store + migrations + publish + lock, the two spikes, the parity harness and fixture repos, golden capture | `cargo test` green; a Rust process opens a TypeScript-built `graph.db` read-only and reports its `meta`; cursor vectors agree in both directions; TOON and MCP spikes have a written decision | The workspace built and `cargo test` was green; the vectors recorded then are still the pin, as far as the bar can be tested from one side — six TypeScript-minted tokens reproduced byte for byte and the refusals pinned by code and order (`rust/crates/sangkar-core/tests/cursor_interop.rs`, 13 tests); the reader's answers against the TypeScript reader's own dump from one database file are still compared (`rust/tests/golden/reader/`, replayed by `rust/crates/sangkar-store/tests/reader_golden.rs`); both spikes carry their verdict and its evidence in `rust/spikes/toon/DECISION.md` and `rust/spikes/mcp/DECISION.md` |
| **1 Read path** | `store::read/*` + `domain/*` complete | Every reader query answers the same rows as the TypeScript one on the same DB | `rust/crates/sangkar-store/tests/reader_golden.rs` compares every reader method against `rust/tests/golden/reader/`, and the row-level suites pin each shape over a hand-built database (`read_reader.rs`, `read_nodes_edges.rs`, `read_coverage_files.rs`) |
| **2 Tools + surfaces** | Seven handlers, seven manifests, `emit()` wiring, the store lifecycle (fingerprint, reopen after publish, `schema_too_new`, bootstrapping→ready), the `repo` verb, the index gate with `--yes`, the MCP server, CLI verbs and renderer | `sangkar <tool>` and `mcp` produce envelopes equal to the captured goldens for all seven tools on all fixtures; tools/list matches the TypeScript schema; a publish is picked up by a running server | `rust/crates/sangkar-cli/tests/goldens.rs` replays every golden and prints 57 compared (57 identical); `rust/crates/sangkar-cli/tests/mcp_surface.rs` matches tools/list field by field and answers a publish made between two calls with the new generation; `rust/crates/sangkar-cli/tests/store_refresh.rs` covers the reopen and `schema_too_new` |
| **3 Indexer** | `lang`, `discover`, `parse`, `extract`, `resolve`, `persist`, `worker`, `watch` | A repo indexed by Rust produces a `graph.db` the **TypeScript** reader serves, whose node/edge/unresolved census matches the TypeScript index of the same corpus per language; incremental routing publishes the same file set | `rust/crates/sangkar-index/tests/census.rs` reports polyglot 92 keys compared / 0 differ and broken 27 keys compared / 0 differ, with one allowed grammar substitution (§12.1), and it compares the six ordered row sequences, so ids and order must match. The TS-reader half survives as the goldens: the Rust `index` verb now builds the fixture databases and the same 57 comparisons pass |
| **4 Daemon + telemetry** | Recorder, sink, aggregator, HTML report, report server, daemon lifecycle/reconcile/CLI | `daemon run/start/stop/status` behaves as the TypeScript daemon tests demanded; the report page renders the same sections from the same JSONL | `rust/crates/sangkar-daemon/tests/lifecycle.rs` drives real processes and files, `rust/crates/sangkar-daemon/tests/integration.rs` the composition and the four CLI answers; the telemetry suites pin the record bytes, the fold, the page and the socket (`record_sink.rs`, `recorder.rs`, `report.rs`, `html.rs`, `serve.rs`) |
| **5 Cutover** | npm platform package + launcher shim (Linux x64 only), release workflow, binary smoke, docs rewrite, TypeScript deletion | `sangkar` from the published tarball on Linux x64 answers the goldens, indexes a repo, runs the daemon, and the TypeScript tree is gone | `rust/crates/sangkar-cli/tests/binary_smoke.rs` runs the binary cargo built as a real child process on a machine carrying only `PATH` (an empty directory) and `HOME`, and it answers, indexes and serves MCP — the same binary the release workflow packs into the tarball `install.sh` installs. The packed packages measured 7 files / 18.2 kB and 2 files / 6.3 MB at the time (`npm pack --dry-run`), a measurement no longer reproducible from the tree now that packaging is retired (2026-11-26). The TypeScript source, test tree, entry point and five pins are deleted. The *registry* step was never exercised — no package was ever published — and npm publishing has since been retired (packaging note)

Cut points, if the migration had been paused: after Phase 2 the Rust binary was a *useful* read-only
server for graphs the TypeScript indexer produced; after Phase 3 it was a complete implementation with a
TypeScript front end. Neither state was shippable — §6's no-engine-switch rule meant the npm package
flipped once, at Phase 5.

## 8. Risks

Ordered as the plan ordered them, most likely first. The last column is the risk's actual outcome.

| # | Risk | Why it was real | Actual outcome, and what still proves it |
|---|---|---|---|
| **R1** | TOON output is not the same format | `serde_toon_format` implements the **v4.1 working draft**; the pin was `@toon-format/toon` **4.4.0**, and its README says key-folding and flatten-depth options "were removed by spec 4" | **Did not materialise.** The Phase 0 spike ran fifteen contract cases and ten real envelopes through the reference decoder: all fifteen round-tripped, and the ten came out byte-identical. Two findings became code: the renderer appends the trailing newline itself, and key order is preserved. The fallback (vendoring the TypeScript encoder) was never built. In-product proof: `rust/crates/sangkar-cli/tests/args_render.rs` decodes the rendered TOON back to the value it was given. The one blind spot is recorded, not hidden: positional float spelling outside `1e-6 … 1e21` |
| **R2** | `rmcp` frames or handshakes differently | The TS SDK used newline-delimited JSON-RPC and no `Content-Length`; a header-framed Rust transport would have hung the first client | **Did not materialise.** `rmcp` 3.11 frames on `\n`, has no init gate, and carries in-band errors; the spike proved all four counts and found two differences that are not failures — responses arrive out of request order (legal JSON-RPC), and `inputSchema` key order is sorted unless `preserve_order` is on. `rust/crates/sangkar-cli/tests/mcp_surface.rs` therefore compares by request id, as parsed JSON. The hand-rolled fallback was not built |
| **R3** | Grammar node-type names drifted | The 12 rows were probed against `tree-sitter-wasms` 0.1.13; the native crates are independent releases, and a missing name is silent | **Materialised once, measured, and allowed.** Every declared node type is probed against the loaded grammar and a parsed snippet by `rust/crates/sangkar-index/tests/lang.rs`; the one behaviour difference is `base.Deposit` in the C# fixture (§12.1), and it is the only allowed delta in `rust/tests/golden/census/polyglot.ts.json` |
| **R4** | Cursor incompatibility | The MAC is over the JSON *text*; key order, integer formatting and base64url padding all matter, and each tool's `argsHash` format already differed | **Did not materialise.** `rust/crates/sangkar-core/tests/cursor_interop.rs` (13 tests) reproduces the six TypeScript-minted tokens in `rust/tests/vectors/cursor_vectors.json` byte for byte and pins the refusals by code and order; the goldens compare `next` byte for byte, which pins the per-tool digest format and the page arithmetic at once |
| **R5** | SQLite behaviour differs at the edges | FTS5 tokenizer options, `VACUUM INTO` on a read-only source, `journal_mode` while statements are cached, `RETURNING`, integer affinity | **Did not materialise.** `bundled` SQLite is the same engine; the PRAGMAs are read back after the handle opens (`rust/crates/sangkar-store/tests/db_lock.rs`), the publish protocol is driven over real files (`publish.rs`), and the reader goldens compare a Rust read against the answers the TypeScript read gave for the same fixture |
| **R6** | The `index` tool's worker hand-over is forgotten | Phase 2 shipped a read-only binary that still had to index, and Phase 3 changed who indexes | **Did not materialise.** The `worker_cmd` config knob survives as the way `index` and the daemon name their child, defaulting to the current executable and overridable with `SANGKAR_WORKER_CMD`; `rust/crates/sangkar-tools/tests/index.rs` asserts the command comes from `AppConfig`. The plan's sentence that the knob would "go away at cutover" was wrong, and §6.4 says so |
| **R7** | Packaging on Linux / old glibc | A binary built on a newer glibc will not run on a 2.35 userland, and a shim that silently installs with no binary is worse than one that refuses | **Handled, then superseded.** The launcher was Node and refused with one line and exit 70; that refusal is `install.sh`'s platform gate now. `.github/workflows/release.yml` builds on `ubuntu-22.04` and fails when the binary's highest `GLIBC_x.y` is above 2.35. A build on this machine reports `GLIBC_2.39` and must not be published. No package was ever on the registry — that publish needed the maintainer's OTP — and npm publishing has since been retired |
| **R8** | The rewrite drifts into a redesign | 25 k lines of "while I'm here" is the classic failure of a total migration | **Held.** Non-goals stayed constraints; the 57 goldens compare identical, so no payload key, value or order moved; the one new product gap the port introduced (a nested `.gitignore`) was recorded in `gaps-n-future-works.md` rather than fixed mid-port |
| **R9** | Effort is underestimated | 25 773 lines of source with 31 338 lines of contract tests, 12 grammars, and a daemon; the plan was 73 tasks, not 15 | **Not re-scorable from the tree, and not pretended otherwise.** No phase recorded wall-clock hours; what is measurable is the output, and §10 measures it — 978 tests, 71 suites, 43 699 source lines with unit tests, 44 915 lines of integration tests, one 31 056 160-byte binary |

## 9. Verification strategy

**Unit level.** Each port task carried the assertions of the TypeScript test file it replaced. The 60 files
were not ported 1:1 — the boundary test became `rust/crates/sangkar-boundary/tests/dag.rs`, the payload
shapes became the goldens, and the rest became 71 suites beside the crates they test. The mapping is in
the plan's phase entries and in each task's `verify`; the surviving map from a change to the tests that
catch it is [`.agents/prompts/test-map.md`](../../.agents/prompts/test-map.md).

**Parity level.** `rust/tests/parity/`:

- Three fixture repositories, checked in: `polyglot` (one small file per language, plus a call graph dense
  enough to exercise `trace`/`impact`), `broken` (parse errors, a binary file, an unreadable directory, a
  rename, a 600 KB file), `empty` (no files).
- For every tool × fixture × argument set, the Rust envelope is compared against the captured TypeScript
  golden in `rust/tests/golden/`: identical key sets at every depth, identical primitive values,
  identical array *order* and length, identical `status`/`error.code`/exit code. JSON is compared byte for
  byte, `next` included; TOON is compared structurally — rendered, decoded, and compared to the value it
  came from (`rust/crates/sangkar-cli/tests/args_render.rs`).
- Census comparison for Phase 3: per language, nodes, edges by kind and tier, unresolved by kind, files and
  skipped reasons, from both indexers on `polyglot` and `broken`
  (`rust/crates/sangkar-index/tests/census.rs`).

**The plan's two assertions, as they stand.**

- The prepared-statement path was meant to keep a ≥5× advantage over a per-call `prepare` as an
  assertion. It is **not** asserted: the tree ships no timing test (§12.11). What the store pins instead
  is the cache's behaviour — its size and hit accounting (`statement_cache_stats` in
  `rust/crates/sangkar-store/tests/db_lock.rs` and `rust/crates/sangkar-store/tests/read_nodes_edges.rs`)
  and that an empty prefix or file list prepares nothing
  (`rust/crates/sangkar-store/tests/read_coverage_files.rs`). The 42.7 µs → 1.7 µs measurement survives as
  the reason the cache exists, in the module comment at the top of `rust/crates/sangkar-store/src/db.rs` —
  a comment, not an assertion.
- `emit()` remains the only truncation point and the only minter of `next`; `total_relation: "eq"` still
  degrades to `gte` when coverage is incomplete, unresolved edges exist, the generation is stale, or the
  payload was truncated. `rust/crates/sangkar-core/tests/envelope_budget.rs` transcribes every case.

**Removed at cutover, with what replaces it:** the TypeScript MCP surface test →
`rust/crates/sangkar-cli/tests/mcp_surface.rs`, which replays the recorded session frame by frame; the
TypeScript binary smoke test → `rust/crates/sangkar-cli/tests/binary_smoke.rs`, the same assertions against
the artifact an install runs with no interpreter available; schema parity against Drizzle and the
`db:drift` check → `rust/crates/sangkar-store/tests/migrate.rs`, which compares the ledger's count with
`schema_version` on a real file (the `drizzle-kit generate` step disappeared, so there is no generated
schema left to drift).

## 10. Effort, measured

The plan is 73 tasks across six phases (`.ppm/rust-migration/`), with a peak width of 9 in the tool
fan-out. The estimate this section used to carry — "Phase 0 in days, each fan-out phase in weeks, cutover a
week" — cannot be re-scored from the tree: no phase recorded wall-clock hours. What is measurable is the
output, so that is what this section reports, with the command that produced each number.

| What | Number | Measured by |
|---|---|---|
| tasks | 73, in six phase files | the JSON under `.ppm/rust-migration/tasks/` |
| when | every commit in the plan falls on 2026-11-24 or 2026-11-25 | `git log --format=%ad` |
| Rust source | 43 699 lines under the nine crates' `src` trees, unit tests included | a line count over `rust/crates` |
| Rust integration tests | 44 915 lines under the nine `tests` trees | the same count |
| tests | 978 passed, 1 ignored, 71 suites | `cargo test --workspace` |
| goldens | 57 compared, 57 identical, 0 skipped | `cargo test -p sangkar-cli --test goldens -- --nocapture` |
| census | polyglot 92 keys compared, broken 27, 0 differ, 1 allowed delta | `cargo test -p sangkar-index --test census -- --nocapture` |
| binary | 34 416 888 bytes; 31 056 160 stripped | `ls -l rust/target/x86_64-unknown-linux-gnu/release/sangkar`, and the `sangkar` member of the release tarball |

The migration was worth doing at the point where the artifact mattered more than feature velocity. That
argument is now history; the artifact it bought is 31 056 160 bytes with no interpreter in the path, and
the four measured product gaps it did not buy are in
[`gaps-n-future-works.md`](../../.agents/prompts/gaps-n-future-works.md).

## 11. What this document obliged

All of it landed, and the three product documents that were expected to need nothing still need nothing.

- [`AGENTS.md`](../../AGENTS.md) and
  [`.agents/prompts/project-layout.md`](../../.agents/prompts/project-layout.md) gained the `rust/` tree in
  Phase 0's commit, not after.
- [`.agents/prompts/rules.md`](../../.agents/prompts/rules.md)'s module table is now the six Rust rules,
  each one a `cargo test -p sangkar-boundary` failure rather than a review comment.
- [`TECH-SPEC.md`](TECH-SPEC.md) §15 was rewritten at Phase 5, when distribution actually changed: the
  measured sizes, one platform, the glibc floor, and the launcher's refusal.
- [`docs/maintainers.md`](../../docs/maintainers.md) and the project skill
  [`.agents/skills/build-n-deploy/SKILL.md`](../../.agents/skills/build-n-deploy/SKILL.md) followed the
  two-package release as it stood in Phase 5; both describe the release tarball and `install.sh` since
  the packaging note above (2026-11-26).
- [`TOOLS-SPEC.md`](TOOLS-SPEC.md), [`DATABASE.md`](DATABASE.md) and [`TELEMETRY.md`](TELEMETRY.md) were
  not edited in Phase 5 — `git status` lists no change to them — which is the parity bar holding, not an
  omission.

## 12. What differs from this plan

Every place the result is not what the plan said, in the order the phase reports raised them. A deviation
missing from this list would be a defect in the document.

1. **A measured grammar substitution.** The C# fixture's `base.Deposit(amount)`
   (`rust/tests/parity/fixtures/polyglot/src/Account.cs`) parsed differently by grammar. The wasm
   `tree-sitter-c_sharp` bundle makes `base` the named node `base_expression`, so the TypeScript recorded
   the call site as `dynamic` with `raw_target: "base.Deposit"`; `tree-sitter-c-sharp` 0.24 makes `base`
   an anonymous token, so the Rust name chain sees one identifier and records `receiver_type_unknown`
   with `raw_target: "Deposit"`. One unresolved site moves bucket, and it is the only allowance in
   `rust/tests/golden/census/polyglot.ts.json`: `unresolved_by_kind.dynamic` 15 → 14,
   `unresolved_by_kind.receiver_type_unknown` 8 → 9, both allowed, with every node, edge and per-language
   count equal. Everything else in the census agrees: polyglot 92 keys, broken 27, 0 differ.
2. **`signature` reproduces a TypeScript quirk deliberately.** The deleted `signatureOf` sliced a
   declaration's first line at the *body's absolute column in the file line*, not at an offset from the
   node — so an indented K&R declaration keeps its trailing `{`, and an unindented single-line body is cut
   mid-word (`function area(): number { retur`). `signature_of` in
   `rust/crates/sangkar-index/src/extract/declare.rs` reproduces that arithmetic because the goldens
   captured its output; the Rust extraction expectations were re-pinned to the oracle afterwards, and
   `rust/tests/golden/` is what proves the agreement.
3. **`bun_version` keeps its v1 name and carries `runtime_version()`.** The field is spelled
   `bun_version` — `TELEMETRY.md` §4 still names it, because the name and the type really did stay, and its
   value column's `Bun.version` is the TypeScript spelling — but this binary fills it with the Rust
   compiler's version, spelled `rustc <version>`: `sangkar_core::runtime` documents the source and
   `rust/crates/sangkar-telemetry/src/record.rs` the field. No report aggregate reads it, so no v1 record
   shape changed.
4. **The telemetry reading API grew two signatures the plan's sketch did not have.** `emit`, cursors and
   `--json` parity held, but reading is not the sketch: `rust/crates/sangkar-telemetry/src/report.rs`
   returns `TelemetryReadResult.records` as `Vec<TelemetryRecord>` — with `data.totals` carrying the count
   — so the multi-repository report can fold real records, and
   `rust/crates/sangkar-telemetry/src/html_multi.rs` renders
   `render_multi_page(machine, generated_at, since, sections)` because the TypeScript signature takes four
   inputs, not the plan's one.
5. **The daemon's status shape follows this document's contract, not the TypeScript's.**
   `DaemonStatus` and `RepoRuntime` in `rust/crates/sangkar-daemon/src/types.rs` are
   `{pid, startedAt, uptimeMs, repositories[{root, watch, lastRun, lastExit, generation}], report}`; the
   TypeScript's `DaemonStatus` was `{pid, startedAt, home, registryPath, repos[{root, exists,
   watch, watchIntervalMs, telemetry}]}`. `daemon status --json` is therefore a Rust-only answer; the
   human lines of status, stop, start and run match the TypeScript's. Two facts follow from the
   composition: the daemon records `generation: null` because it does not open `graph.db` itself, and it
   reports dotted log event keys where the TypeScript wrote sentences.
6. **Exit 70 belonged only to the launcher shim.** Missing or unsupported platform, one stderr line, exit
   70; no native path emits it, exactly as §3.6 said. One detail the plan did not name: that launcher was
   Node rather than a shell script, and it used an async `createRequire` plus `spawn` rather than a static
   `require` or `spawnSync`, so it forwarded signals and kept working whether or not `"type": "module"`
   was in the root manifest. Both the launcher and the manifest are deleted; nothing emits 70 now.
7. **The report server is `tiny_http` 0.12.** The plan offered `tiny_http` or `hyper`; the first was taken.
   It serves `GET /`, `HEAD /` and `GET /healthz` matched literally, the way the TypeScript's server did,
   with `HEAD /healthz` a 404 because the contract names the method of every route; and it re-parses
   `--since` per request after refusing a bad value at bind time.
8. **A new product gap the port introduced: a nested `.gitignore` is not read.** The walker takes its
   ignore rules from the root `.gitignore` and `.sangkarignore` only, so a crate's own file is invisible
   and `rust/spikes/*/target` is walked. In this repository that is 9 build-script outputs indexed as real
   files (9 nodes, 14 unresolved rows) and 2 143 of the 2 406 `not_indexed` rows. It is not a parity
   regression — the TypeScript walked the same tree differently — but it is a product gap, measured, and
   recorded in `gaps-n-future-works.md` with the query that produced it.
9. **`scripts/parity/` is retired.** The capture harness was TypeScript and imported the tree this
   cutover deleted, so it cannot run and the goldens cannot be re-captured. The committed goldens and the
   two vector files are the oracle; a payload that moves on purpose moves its golden in the same commit,
   and the census test is what keeps row ids and their order honest. A replacement that writes a golden
   from a Rust run does not exist and is future work.
10. **Measured effort and sizes**, in place of the plan's estimate: §10 carries the table — 978 tests in 71
    suites, 43 699 source lines with unit tests, 44 915 lines of integration tests, a 31 056 160-byte
    stripped binary. The two npm package rows left that table with the packaging they measured
    (2026-11-26).
11. **The prepared-statement ratio is not asserted.** §9 said the migration would keep a ≥5×
    prepare-vs-`prepare_cached` comparison as an assertion; the tree has no timing test. The cache is
    pinned by its size, its hit accounting and the empty-input cases instead, and the 42.7 µs → 1.7 µs
    measurement is a module comment rather than a regression guard.

Nothing else differs. The envelope keys, the payload keys, the twelve error codes, the cursor format, the
exit codes and the seven tool surfaces are the TypeScript's, and the 57 goldens are the proof of that
rather than a promise about it.
