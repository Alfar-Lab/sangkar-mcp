# Constraints and invariants

Every task must respect these. They are copied into each task's `details.contract`.

## The parity bar (semantic, from `docs/my-product/RUST-MIGRATION.md` §3)

- **Identical:** tool names, first sentence of every description, every parameter's name/type/default/clamp,
  the `tools/list` schema shape (`src/index.ts:227-275`); the nine envelope metadata keys + `payload` +
  `error`-iff-error/bootstrapping; the five statuses, four truncation reasons, twelve error codes; the
  `relationFor()` rule and `emit()`'s `eq`→`gte` downgrade; **array order** (cursors are positions into a
  generation's stable ordering, so a re-sorted list serves the wrong page); the `--json` bytes (one compact
  line + `\n`) and the MCP `tools/call` text (same JSON, no trailing newline); `BYTES_PER_TOKEN=4`,
  `NEXT_TOKEN_RESERVE_CHARS=384`, every default budget; exit codes `0/1/2` and the worker's `0/1/2/75`
  (`70` belongs only to the npm launcher shim's missing-platform-package case; no Rust path emits it);
  stdout carries only the protocol or the rendered answer.
- **Identical files and locks:** `graph.db`, `<live>.stage.<12 hex>`, `write.lock`, `daemon.pid`,
  `<home>/sangkar.json`, `<dataDir>/config.json`, `<dataDir>/telemetry/<YYYY-MM-DD>.jsonl`,
  `install_id.json` — same paths, same JSON shapes, same staleness rules.
- **May differ:** TOON whitespace/quoting/delimiter/table choices, *provided* the reference decoder
  (`@toon-format/toon` 4.4.0) round-trips the Rust output to a structurally equal value; log ordering;
  stderr wording of refusals; `health.statement_cache_entries`; all timestamps and durations.
- **Forbidden:** editing `docs/my-product/{TOOLS-SPEC,DATABASE,TELEMETRY}.md`. Those documents describe the
  contracts being preserved; a task that needs to change one has broken the bar — stop and report.

## Structure

- **The crate DAG is the module-rule table.** `sangkar-core` ← `sangkar-store` ← `sangkar-domain` ←
  `sangkar-tools`; `sangkar-index` may use core+store+domain; `sangkar-telemetry` only core/telemetry;
  `sangkar-daemon` may use core+store+index+telemetry, never tools; `sangkar-cli` is the only crate that may
  use index+tools+daemon together. `sangkar-boundary` fails the build on any other edge.
- **`rusqlite` appears in `sangkar-store`'s manifest only** (the replacement for `drizzle-location`). The
  `rusqlite` symbols may not appear in `sangkar-core`, `sangkar-domain` or `sangkar-tools`.
- **`println!`/`print!` are banned** outside `#[cfg(test)]`; `sangkar-core/src/log.rs` is the only stderr
  writer. The MCP surface test asserts stdout byte discipline end to end.
- **No new dependency** beyond `docs/my-product/RUST-MIGRATION.md` §5's table. A crate that is not listed
  there is a decision for the maintainer, not a task.

## Storage

- **One copy of the SQL during the migration.** `sangkar-store` embeds
  `src/shared/store/migrations/0000_init.sql` and `raw/0001_fts.sql` with relative `include_str!`. Those
  files are **never** edited by a task: they are generated from `src/shared/store/schema.ts` by
  `drizzle-kit`, and `bun run db:drift` must stay green for as long as they exist.
- **Never publish a partial graph.** Stage → `beginWrite` → one `BEGIN IMMEDIATE` → `seal()` (checkpoint,
  `journal_mode=DELETE`, `synchronous=FULL`, `quick_check(1)` → exactly `ok`) → `rename()` over the live
  file; `withStage()` guarantees the discard path. Readers keep serving the previous generation.
- **Never run two indexers on one tree at once.** The write lock is shared between the implementations, so a
  TS `bun run bin/sangkar.ts index` and a `cargo run -p sangkar-cli -- index` contend for
  `<dataDir>/write.lock`. Before any task that indexes, check `status.health.indexer`; if a run is in
  flight, wait rather than starting a second.
- **`total_relation: "eq"` stays a claim** (coverage complete, no unresolved rows in scope, fresh
  generation, payload not truncated) and `emit()` stays the only truncation point and the only minter of
  `next`.

## Process

- **Cursors are interoperable when the key is.**
  `base64url(JSON{generation,tool,argsHash,position,repo})` + `.` +
  `base64url(HMAC-SHA256(key, segment)[0..16])`, unpadded base64url, JSON keys in that order, integers
  unquoted. Both binaries must sign and verify the same bytes for the same key; without
  `SANGKAR_CURSOR_KEY` each process mints its own random key, exactly as today (`config.ts:290-294`), so
  cross-process resumption is a property of the configured key, not of the format. Per-tool `argsHash`
  formats are already inconsistent in the source (`sha256:<hex>` for trace/expand/status/summary, bare hex
  for search, 32-char truncation for impact) and are reproduced as they are.
- **Telemetry records keep `v: 1` and keep every field name, `bun_version` included.** The report never
  reads that field (`src/shared/telemetry/report.ts:277-311`); it carries the producing runtime's version
  string. Renaming it would be a record shape change, so `docs/my-product/TELEMETRY.md` needs no edit.
- **One daemon at a time, stated order:** stop the TypeScript daemon (`sangkar daemon stop`) before starting
  the Rust one. Never both.
- **Never weaken a golden to make a test pass.** If a captured golden is wrong, re-capture it from the
  TypeScript binary and say in the commit body why the earlier capture was wrong.
- **Preserve unrelated work.** The tree may carry untracked `.ppm/` content in other plans; do not delete,
  commit or reformat it. `.sangkar/graph.db` is ignored and may be rewritten by a run, never by hand.
- **Formatting and lint are tools' job:** `cargo fmt`, `cargo clippy -D warnings`. Do not hand-format.
- **Conventional commits**, scoped, `!` for the ones that change the repo's shape or the published artifact.

## Verification environment

- **Rust is not installed on this machine today.** `cargo` and `rustc` are absent; `cc`/`gcc` are present
  (needed by `libsqlite3-sys` `bundled` and the grammar crates). TASK-001 installs
  `rustup` with toolchain `1.100.0` (components `rustfmt`, `clippy`) and needs network once. Every later
  verify command may assume `cargo` on `PATH`.
- **Bun is at `~/.bun/bin/bun` (1.4.4)** and also on `PATH` as `~/.local/bin/bun`. The TypeScript oracle is
  run as `~/.bun/bin/bun run bin/sangkar.ts …`. Node is `v26.7.0`, npm `11.19.0`.
- **No service, no container, no shared database.** Every test builds its own SQLite file in a temp
  directory or uses a checked-in fixture under `rust/tests/parity/fixtures/`. The git-ignored
  `<repoRoot>/.sangkar/graph.db` is the only shared artifact, and only the closing task of a phase may read
  it.
- **Goldens are committed**, captured from the TypeScript binary while it still exists:
  `rust/tests/golden/<fixture>/<tool>.json` (envelopes) and `rust/tests/vectors/cursor_vectors.json`.
  After Phase 5 they are the only oracle, which is why they are committed files, not a script.
- **The fixture repositories are checked in**, three of them (`polyglot`, `broken`, `empty`), and indexing
  them is a normal `sangkar index --data-dir <tmp>` run. No test reaches the network.
- **Executor must not**: edit `src/shared/store/migrations/**`, edit the three contract docs named above,
  write `.ppm/*/tasks/*.json` by hand, force-push, delete `src/` before Phase 5, or publish to npm.
- **Phase-scoped checks**: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` are cheap and belong to every closing task; `~/.bun/bin/bun test` stays green
  until Phase 5's deletion and is run once per phase close, because the TypeScript suite is still the
  behavioural spec until then.
