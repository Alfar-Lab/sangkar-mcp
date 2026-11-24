# Migration: sangkar-mcp to Rust

**Status:** decided, not started. Executable plan: `.ppm/rust-migration/`.
**Decision date:** 2026-11-24 · **Base commit:** `3592da3` · **Version migrated:** `0.6.0-beta.1`.
**Size of the thing being moved:** 76 TypeScript files / 25 773 lines in `src/` + `bin/`, 60 test files /
31 338 lines in `tests/`, 11 SQLite tables, 12 languages, 7 tools, 2 surfaces.

---

## 1. Why

The product is right; the runtime it is written in is the cost. Three of the four arguments below are
already written down in [`TECH-SPEC.md`](TECH-SPEC.md) §15 as reasons the artifact cannot ship well today.

| Goal | What it fixes | Evidence in the tree today |
|---|---|---|
| **A small artifact with no interpreter** | `npm i -g sangkar-mcp` currently needs Bun ≥ 1.4.4 (102 MB) on the user's machine; `bun build --compile` measures **104–160 MB** because Bun embeds itself plus 12 WASM grammars | `package.json` `engines.bun`, `docs/maintainers.md` § Distribution |
| **A grammar set that stops being a payload** | Native `tree-sitter` grammar crates are compiled into the binary, replacing `tree-sitter-wasms` (12 × 1–3 MB) plus a hand-rolled WASM `dylink`→`dylink.0` section rewrite | `src/indexer/parse.ts:227-307` exists only because of `web-tree-sitter` |
| **One index pass instead of a parser pool behind a FIFO queue** | All parse work is serialized on one queue because the WASM parser is single-threaded and shared | `src/indexer/parse.ts:1-13` (`MAX_CACHED_LANGUAGES=6`, "everything serialized on one FIFO queue") |
| **Removing a whole layer of private-API coupling** | `drizzle-orm`'s `bun-sqlite` driver leaks into `persist.ts` through a private field, and `bun:sqlite` statement-close semantics dictate the entire `WriteScope.close()` ordering | `persist.ts:234-245` (`tx.session.client`), `publish.ts:20-30` |

Not a goal, and worth saying because it is the usual reason people rewrite: **speed of the seven query
tools.** They already answer in single-digit milliseconds through a prepared-statement cache; the Rust port
must not regress that, but the migration does not promise it gets faster. The index pass is where the time
is, and the parse queue above is the only measurable win available.

## 2. Non-goals

Every one of these is a decision, not an omission. Each would break a compatibility claim made in this
document, or is a feature the design record deliberately refuses.

- **No schema change.** Eleven tables, one ledger, `include_str!` embedding of the exact same `.sql`
  files. `schema_migration`, `meta`, and `node_fts`'s FTS5 tokenizer (`unicode61 remove_diacritics 2`)
  stay byte-identical, so one `graph.db` is readable by both implementations during the whole migration.
- **No payload redesign.** Envelope keys, payload keys, the 12 error codes, the `eq`/`gte` rule,
  cursors, exit codes: unchanged. (§3 defines what "unchanged" means precisely.)
- **No new languages, tools, or arguments.** The 12 language rows move as data, not as an opportunity.
- **No `notify`-based watching.** The watcher's signal is git state (`HEAD` + `status -z -uall`), not
  filesystem events; a polling loop is the correct design and stays.
- **No async database layer.** `rusqlite`'s synchronous API matches the existing synchronous reader; the
  async surface stays where it already is (MCP transport, daemon loop, report server).
- **No LSP, no embeddings, no analytics.** Already refused in [`DESIGN.md`](DESIGN.md) §3; a rewrite is
  the worst possible moment to reopen them.

## 3. The parity bar: semantic, with three hard edges

Chosen over byte-identical output, because the TOON encoder (see §8 R1) will not be the same
implementation. What follows is the whole contract; anything not listed is free.

**Must be identical.**

1. **Tool surface.** The seven names, their descriptions' first sentence, every parameter's name, type,
   default and clamp, and the JSON schema shape `tools/list` returns (`{type:"object",properties,required?,examples?}`,
   `enum` → `{type:"string",enum}`, `min`/`max` → `minimum`/`maximum`). `src/index.ts:227-275` is the spec.
2. **Envelope.** The nine metadata keys plus `payload`, always present; `error` if and only if `status` is
   `error` or `bootstrapping`; the five statuses; the four truncation reasons; the twelve error codes;
   `relationFor()`'s rule and `emit()`'s downgrade of `eq` to `gte`.
3. **Array order.** This is the edge people miss. Cursors are *positions into a generation's stable
   ordering* (`src/shared/cursor.ts:52-58`), and while both implementations exist they sign and verify
   each other's tokens — but only when `SANGKAR_CURSOR_KEY` is set, because the key is otherwise 32 random
   bytes per process (`config.ts:290-294`). A differently-sorted list is not a cosmetic difference; it
   silently serves the wrong page. Every ordering the code performs today is part of the contract,
   including `SQLite`'s implicit `rowid` order where no `ORDER BY` is written.
4. **JSON rendering.** `--json` output stays one compact line plus `\n`; the MCP `tools/call` text stays
   the same JSON without the trailing newline. Byte-identical, trivially.
5. **Numbers and units.** `BYTES_PER_TOKEN=4`, `NEXT_TOKEN_RESERVE_CHARS=384`, per-tool default budgets,
   integer/`null` totals, and the `unresolved_material` / `candidates_shown` optional keys.
6. **Process contract.** Exit codes (`0`, `1`, `2`, and the worker's `0/1/2/75`), stdout carrying only
   the protocol or the rendered answer, and every diagnostic on stderr through one `log` module. No Rust
   path emits `70`: its only producer was the missing-interpreter preflight
   (`src/shared/runtime.ts:69-79`), which a static binary cannot fail. The npm launcher shim keeps `70` for
   one thing — a platform package that is not installed.
7. **Files and locks.** `graph.db`, `.stage.<12 hex>`, `write.lock`, `daemon.pid`, `<home>/sangkar.json`,
   `<dataDir>/config.json`, `<dataDir>/telemetry/<YYYY-MM-DD>.jsonl`, `install_id.json`: same paths, same
   JSON shapes, same staleness rules. The two implementations must be able to take turns on one tree.

**May differ.**

- TOON whitespace, quoting, delimiter and table-vs-list choices — **provided** the reference decoder
  (`@toon-format/toon` 4.4.0) reads the Rust output back into a value that is structurally equal to the
  envelope. That round-trip is a test, not a hope (§8 R1).
- Log line ordering, stderr wording of refusals, `health.statement_cache_entries`, `duration_ms`,
  `indexed_at`/`stale_checked_at` values, and any measurement: same shape, different numbers.
- The `status` payload's `health.last_publish_ms` and DB size are read, not reproduced.
- Performance characteristics, apart from the two assertions §9 keeps.

**Explicitly released.** The TypeScript suite is the *specification* during the migration (Phase 0–4) and
is deleted at cutover (Phase 5). Its durable replacement is the captured-golden harness in §9 — so "the
tests still pass" means "the goldens still pass", not "some TypeScript still runs".

## 4. Target architecture

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
| `tool-isolation` | `sangkar-tools` depends on `core`+`store`+`domain`; a `tools::<a>` module may not name `tools::<b>` (boundary test greps for `crate::tools::` inside `src/tools/`) |
| `indexer-isolation` | `sangkar-index` depends on `core`+`store`+`domain` only |
| `shared-isolation` | `core`, `store`, `domain`, `telemetry` never depend on `tools`, `index`, `daemon`, `cli` |
| `drizzle-location` | replaced by **`rusqlite`-location**: `rusqlite` appears in `sangkar-store`'s manifest only |
| `root-privileges` | `sangkar-cli` is the only crate that depends on `index`, `tools` and `daemon` together |
| `daemon-privileges` | `sangkar-daemon` depends on `core`+`store`+`index`+`telemetry`, never `tools` |
| stdout is the protocol | `println!`/`print!` banned outside tests; `sangkar-core/src/log.rs` writes stderr; boundary test greps, clippy denies, and the MCP surface test asserts it end to end |

`ToolContext` keeps its TS shape (`ctx.reader`, `ctx.cursor`, `ctx.log`, `ctx.now`, `ctx.config`,
`ctx.db_size_bytes`) in `sangkar-store::read::context`, which is why `store` sits below `domain` — the same
direction the TS imports already take (`src/shared/domain/*` imports `#shared/store/*`, never the reverse;
verified by grep before this document was written).

## 5. Dependency replacements

Every pick was checked against the registry on 2026-11-24; the version is the pin the plan will use.

| Today (pin) | Rust (pin) | Why this one | Rejected |
|---|---|---|---|
| `@modelcontextprotocol/sdk` 1.32.2 | `rmcp` 3.11.0 (stdio) | Official Rust SDK; its stream transport frames on `\n` (`async_rw.rs:148`) exactly like the TS SDK's `ReadBuffer`, which is the assumption `tests/mcp-surface.test.ts` encodes | Hand-rolled JSON-RPC — kept only as the fallback if the handshake probe fails (§8 R2) |
| `drizzle-orm` 0.45.4 + `bun:sqlite` | `rusqlite` 0.43.0, feature `bundled` | `bundled` compiles SQLite with `SQLITE_ENABLE_FTS5` and `SQLITE_ENABLE_JSON1` (`libsqlite3-sys/build.rs:177-178`), which the `node_fts` virtual table needs; synchronous API, `prepare_cached` semantics close to the existing LRU | `sqlx` (async, runtime-heavy), `turso` (immature FTS5 story) |
| `web-tree-sitter` 0.27.2 + `tree-sitter-wasms` 0.1.13 | `tree-sitter` 0.27.2 + eleven grammar crates for the twelve language rows | Grammars become compiled-in code, which removes the 12 MB WASM payload and the FIFO parse queue. Node-type names come from the **grammar** crates, whose releases are independent (e.g. `tree-sitter-java` 0.23.5) — which is why §8 R3 probes every declared name instead of assuming it | Keeping WASM behind `wasmtime` — keeps the payload and the queue |
| `@toon-format/toon` 4.4.0 | `serde_toon_format` 0.2.0 | Serde-native, conformance-tested against the spec fixture suite; the fallback is vendoring the 61.7 KB TS encoder | Writing an encoder from scratch first |
| `Bun.spawn` (git, worker, daemon) | `std::process::Command` + `tokio::process` | Direct mapping; the git prologue (`--no-pager --no-optional-locks -c credential.helper= …`) is copied verbatim | `git2` (would change what `status --porcelain` means) |
| `Bun.serve` (report) | `tiny_http` or `hyper` | One loopback listener, `text/html` only, no routing framework | `axum` (a whole tower stack for one GET) |
| `node:crypto` HMAC/SHA-256 | `hmac` + `sha2` + `base64` + `constant_time_eq` | Byte level: 16-byte truncated tag, `base64url` unpadded, MAC over the encoded segment | — |
| `node:readline` (index gate prompt) | `std::io::stdin().read_line` | One `y/N` question | `dialoguer` |
| `Bun.CryptoHasher("sha256")` | `sha2` | Used for file digests and per-tool `argsHash` | — |
| Biome + `tsc` | `rustfmt` + `clippy -D warnings` | Same gate position in the loop | — |
| `bun test` | `cargo test` (+ `insta` optional) | Integration tests drive the built binary; unit tests live beside the module | — |

Pinned by `Cargo.lock` in the repo, exactly as `package.json` pins the five TS dependencies today.

## 6. Strangler mechanics

The database is the seam. Both binaries read and write the *same* `graph.db`, with the *same* schema,
under the *same* lock file, so either one can answer at any point in the migration. Six mechanisms make
that true; each is a Phase 0 task because every later phase depends on it.

1. **One copy of the SQL.** The Rust crate embeds the TypeScript directory by relative path
   (`include_str!("../../../../src/shared/store/migrations/0000_init.sql")` and `raw/0001_fts.sql`). One
   file, two readers, zero drift. Phase 5 moves the files into the crate and deletes the TS reader.
2. **One lock, one stage, one publish.** `write.lock` (`{path,pid,startedAt}`), `.stage.<12 hex>`,
   `VACUUM INTO`, `wal_checkpoint(TRUNCATE)` → `journal_mode=DELETE` → `synchronous=FULL` →
   `quick_check(1)` → `rename()`. Copied, not improved.
3. **Cursor interoperability, proven by test vector.** `base64url(JSON{generation,tool,argsHash,position,repo})`
   `.` `base64url(HMAC-SHA256(key, that segment)[0..16])`. A fixed key and payload must produce the
   *identical* token string in both implementations, and each must verify the other's. This pins JSON key
   order, integer formatting and base64url padding — the three things that would otherwise drift silently.
4. **One worker command, named by config.** `AppConfig` gains `worker_cmd` (default
   `[current_exe]`), overridable during the transition so the Rust `index` tool can spawn the *TypeScript*
   worker until Phase 3 lands. It goes away at cutover; `src/index.ts` still reads no environment variable.
5. **One daemon at a time, in a stated order.** `daemon.pid` content and staleness rules are unchanged, so
   the handover is: stop the TS daemon (`sangkar daemon stop`), start the Rust one, and let the stale
   reclaim handle a crash. There is never a moment where both hold the file.
6. **Goldens captured while the oracle still exists.** Every A/B comparison writes the TS answer to
   `rust/tests/golden/<fixture>/<tool>.json`; the Rust test suite then compares against the file forever.
   This is what survives Phase 5's deletion of TypeScript (§9).
7. **One platform at first, and the refusal is a feature.** The npm package declares exactly one platform
   package (`sangkar-mcp-linux-x64-gnu`). On macOS or Windows the shim finds nothing and says which platform
   it wanted, that only Linux x64 is published, and which version to pin to stay on the TypeScript build.
   Adding a platform later means adding one `optionalDependencies` entry, one `npm/<key>/` directory and one
   row in the release matrix — never rewriting the shim.

The transition knob that does *not* exist: no `--engine=bun|rust` flag on the shipped binary. The two
binaries are separate artifacts (`bun run bin/sangkar.ts` vs `cargo run -p sangkar-cli`), and no user-facing
switch ever lands in the npm package.

## 7. Phases and gates

Six phases, each ending in a gate that must be green before the next starts. Phase boundaries are sync
barriers, not topics: the next phase's tasks consume a contract the previous one made compile.

| Phase | Deliverable | Gate (observable) |
|---|---|---|
| **0 Foundation** | Workspace, boundary crate, crate skeletons, cursor/envelope/budget ports, store + migrations + publish + lock, the two spikes, the parity harness and fixture repos, golden capture | `cargo test` green; a Rust process opens a TypeScript-built `graph.db` read-only and reports its `meta`; cursor vectors agree in both directions; TOON and MCP spikes have a written decision |
| **1 Read path** | `store::read/*` + `domain/*` complete | Every reader query answers the same rows as TS on the same DB (`cargo test` over captured row goldens) |
| **2 Tools + surfaces** | Seven handlers, seven manifests, `emit()` wiring, the store lifecycle (fingerprint, reopen after publish, `schema_too_new`, bootstrapping→ready), the `repo` verb, the index gate with `--yes`, the MCP server, CLI verbs and renderer | `sangkar <tool>` and `mcp` produce envelopes equal to the captured goldens for all seven tools on all fixtures; `tools/list` matches the TS schema; a publish is picked up by a running server |
| **3 Indexer** | `lang`, `discover`, `parse`, `extract`, `resolve`, `persist`, `worker`, `watch` | A repo indexed by Rust produces a `graph.db` the **TypeScript** reader serves, whose node/edge/unresolved census matches the TS index of the same corpus per language; incremental routing publishes the same file set |
| **4 Daemon + telemetry** | Recorder, sink, aggregator, HTML report, report server, daemon lifecycle/reconcile/CLI | `daemon run/start/stop/status` behaves as in `tests/daemon-*.test.ts`; the report page renders the same sections from the same JSONL |
| **5 Cutover** | npm platform package + launcher shim (Linux x64 only), release workflow, binary smoke, docs rewrite, TypeScript deletion | `sangkar` from the published tarball on Linux x64 answers the goldens, indexes a repo, runs the daemon, and the TS tree is gone |

Cut points, if the migration is ever paused: after Phase 2 the Rust binary is a *useful* read-only server
for graphs the TypeScript indexer produces; after Phase 3 it is a complete implementation with a
TypeScript front end. Neither state is shippable — §6's no-engine-switch rule means the npm package flips
once, at Phase 5.

## 8. Risks

Ordered by how likely they are to actually happen, not by how bad they would be.

| # | Risk | Why it is real | Gate / mitigation | Fallback |
|---|---|---|---|---|
| **R1** | TOON output is not the same format | `serde_toon_format` implements the **v4.1 working draft**; the pin is `@toon-format/toon` **4.4.0**, and its README says key-folding and flatten-depth options "were removed by spec 4" | Phase 0 spike: encode ten real envelopes, decode with the TS `decode()`, assert deep equality; the MCP `tools/call` text is part of the gate | Port `@toon-format/toon`'s encoder (61.7 KB of bundled output, ~1–2 k lines) as a `sangkar-toon` crate |
| **R2** | `rmcp` frames or handshakes differently | The TS SDK uses newline-delimited JSON-RPC and no `Content-Length`; a header-framed Rust transport would hang the first client | Phase 0 spike: record a `initialize` → `tools/list` → `tools/call` session from the TS server, replay it against the Rust one, diff the bytes on the wire | Hand-rolled JSON-RPC over `tokio` stdin/stdout lines (≈400 lines, full control) |
| **R3** | Grammar node-type names drifted | The 12 rows were probed against `tree-sitter-wasms` 0.1.13; the native crates are independent releases (e.g. `tree-sitter-java` 0.23.5, `tree-sitter-c-sharp` 0.24.0), and `tests/lang.test.ts` exists precisely because a missing name is silent | Phase 3 port of `lang.test.ts` first: every declared type probed against the loaded grammar and a parsed snippet, per language. Then a per-language census A/B on the fixture corpus | Record the substitution per row, and the resulting extraction delta in `AGENTS.md`'s gaps file — a *measured* gap, not a silent one |
| **R4** | Cursor incompatibility | MAC is over the JSON *text*; key order, integer formatting and base64url padding all matter, and `argsHash` already differs per tool (`sha256:<hex>` for `trace`/`expand`/`status`/`summary`, bare hex for `search`, 32-char truncation for `impact`) | Phase 0 fixed vectors, both directions, plus a test that reads each tool's `argsHash` builder | None needed: the format is fully specified; this is work, not risk |
| **R5** | SQLite behaviour differs at the edges | FTS5 tokenizer options, `VACUUM INTO` on a read-only source, `journal_mode` switch while statements are cached, `RETURNING` on `INSERT`, integer affinity | Phase 0 store tasks run against real TS-built DBs, including a seeded stage and a sealed publish | None: `bundled` SQLite is the same engine, only the binding changes |
| **R6** | The `index` tool's worker hand-over is forgotten | Phase 2 ships a read-only binary that must still index, and Phase 3 changes who indexes | §6.4's `worker_cmd` config knob, asserted by an `index` integration test in both phases | Ship Phase 2 without `index` registered — refused by design, not by accident |
| **R7** | Packaging on Linux / old glibc | A binary built on glibc 2.43 (this machine) will not run on a 2.35 userland, and a shim that silently installs with no binary is worse than one that refuses | Phase 5: build on the oldest supported image, a `#!/usr/bin/env node` shim that names the platform it wanted and the version to pin, and a README that says Linux x64 | Publish the platform package without a shim and document the glibc floor. macOS/Windows are out of scope for the first release (§1 decision 2), so the `.cmd`-wrapper problem is deferred, not solved |
| **R8** | The rewrite drifts into a redesign | 25 k lines of "while I'm here" is the classic failure of a total migration | Non-goals (§2) are constraints in `plan.constraint_invariant.md`; a parity task closes each phase, and any payload change is a plan violation, not a judgement call | — |
| **R9** | Effort is underestimated | 25 773 lines of source with 31 338 lines of contract tests, 12 grammars, and a daemon; the plan is 73 tasks, not 15 | Phase 0 delivers a *usable* artifact early (the parity harness plus a store that reads); each phase is independently verifiable | Cut at Phase 2 or 3 (§7), keeping the TypeScript indexer |

## 9. Verification strategy

**Unit level.** Each port task carries the assertions of the TypeScript test file it replaces. The 60 files
are not ported 1:1 — `tests/boundary.test.ts` becomes `sangkar-boundary`, `tests/acceptance.test.ts` becomes
a `criterion`-free bench assertion, and the payload-shape tests become goldens. The mapping is in
`plan.approach.md`'s phase entries and restated in each task's `verify`.

**Parity level.** `rust/tests/parity/`:

- Three fixture repositories, checked in: `polyglot` (one small file per language, plus a call graph dense
  enough to exercise `trace`/`impact`), `broken` (parse errors, a binary file, an unreadable directory, a
  rename, a 600 KB file), `empty` (no files).
- For every tool × fixture × argument set, compare the Rust envelope against the captured TS golden:
  identical key sets at every depth, identical primitive values, identical array *order* and length,
  identical `status`/`error.code`/exit code. TOON is compared structurally (decode, then deep-equal), JSON
  byte for byte.
- Census comparison for Phase 3: per language, `{nodes, edges by kind and tier, unresolved by kind, files,
  skipped reasons}` from both indexers on `polyglot` and `broken`.

**Two assertions the migration keeps**, adapted where the mechanism changed:

- The prepared-statement path must still be ≥5× faster than a per-call `prepare` (the TS test measured
  42.7 µs vs 1.7 µs on `bun:sqlite`; `rusqlite` has different constants, so the ratio — not the
  microseconds — is the assertion).
- `emit()` remains the only truncation point and the only minter of `next`; `total_relation: "eq"` still
  degrades to `gte` when coverage is incomplete, unresolved edges exist, the generation is stale, or the
  payload was truncated.

**Removed at cutover, with what replaces it:** `tests/mcp-surface.test.ts` → wire-level replay test;
`tests/binary-smoke.test.ts` → the same test against the real shipped binary per platform;
`tests/schema-parity.test.ts` and `db:drift` → a Rust test asserting `schema_version` equals the embedded
migration count (the `drizzle-kit generate` step disappears because the schema is no longer generated).

## 10. Effort and what it means in practice

The plan is 73 tasks across six phases (`.ppm/rust-migration/`), with a peak width of 9 in the tool
fan-out. Sized honestly, on one experienced pair of hands: **Phase 0 in days, each fan-out phase in weeks,
cutover a week** — with the indexer (Phase 3) the largest single phase because `extract.ts` (946 lines) is a
table-driven walk over 12 grammars whose behaviour is not obvious from the tables alone, and it is the one
lane the plan cannot parallelize (each stage feeds the next).

The migration is worth doing at the point where the artifact matters more than the feature velocity: as
long as the product still changes shape, TypeScript is the cheaper place to change it. This document
therefore does not argue for a start date; it records what the migration *is*, so that when it starts, the
first commit is Phase 0's workspace and not a debate.

## 11. What this document obliges

- [`AGENTS.md`](../../AGENTS.md) and [`.agents/prompts/project-layout.md`](../../.agents/prompts/project-layout.md)
  gain the `rust/` tree in the same commit that creates it (Phase 0), not after.
- `rules.md`'s module table gains the Rust enforcement column in Phase 0; its six rules stop being
  TypeScript-only.
- [`TECH-SPEC.md`](TECH-SPEC.md) §15 is rewritten at Phase 5, when distribution actually changes — not
  before, because until then the shipped artifact is still the Bun package.
- `docs/maintainers.md` and `.agents/skills/build-n-deploy/SKILL.md` follow the release pipeline change in
  Phase 5.
- [`TOOLS-SPEC.md`](TOOLS-SPEC.md), [`DATABASE.md`](DATABASE.md) and [`TELEMETRY.md`](TELEMETRY.md) should
  need **no** change. If a task finds itself editing one of them, the parity bar has been broken and the
  task stops.
