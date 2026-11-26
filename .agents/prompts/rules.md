# Rules

The hard constraints. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Non-negotiables

These six module rules are the crate DAG plus the manifest and source scans, and every one of them is a
`cargo test -p sangkar-boundary` failure rather than a review comment. `.github/workflows/ci.yml` runs the
gate as part of `cargo test --workspace`, so a violation is a red build.

| Rule | Meaning | Enforced by |
|---|---|---|
| `tool-isolation` | A tool module may use its own siblings and the crates the DAG allows, never another tool. **Tools never name each other.** | `sangkar-boundary`: the DAG row — `sangkar-tools` declares `core`, `store` and `domain`, no other `sangkar-*` crate — plus the source scan that refuses `crate::<another tool>` under `rust/crates/sangkar-tools/src/` and refuses `crate::tools` anywhere |
| `indexer-isolation` | The writer is one crate with the read path's dependencies and nothing else, and nothing but the CLI and the daemon may name it. | `sangkar-boundary`: `sangkar-index` declares `core`, `store` and `domain` only; the DAG has `sangkar-cli` and `sangkar-daemon` as its only dependents, and a missing edge fails as loudly as an extra one |
| `shared-isolation` | `core`, `store`, `domain` and `telemetry` never reach up: no tools, no indexer, no daemon, no CLI. | `sangkar-boundary`: their DAG rows name no such crate, and the both-directions check fails if one is added |
| `rusqlite-location` | SQLite lives behind one crate. `rusqlite` is a dependency of `sangkar-store` and of nothing else, and it is a runtime dependency there. | `sangkar-boundary::check_rusqlite`, which also refuses a missing declaration: the store must declare it in `[dependencies]` |
| `root-privileges` | One composition root. `sangkar-cli` is the only crate that names `tools`, `index` and `daemon` together, and the only crate with a binary. | `sangkar-boundary`: the DAG row for `sangkar-cli` (every crate but `boundary`) and the remaining rows, which deny `tools` to every crate but the CLI |
| `daemon-privileges` | The background process serves; it does not answer. `sangkar-daemon` declares `core`, `store`, `index` and `telemetry`, never `tools`. | `sangkar-boundary`: the DAG row for `sangkar-daemon`, checked in both directions |

The teeth are in `rust/crates/sangkar-boundary/src/lib.rs` and
`rust/crates/sangkar-boundary/tests/dag.rs`: the crate is test-only and dependency-free, it reads the nine
manifests and every Rust source and test file as text, and `cargo test -p sangkar-boundary` fails on

- a `sangkar-*` dependency that is not an edge of `DAG`, and an edge of `DAG` that a manifest does not declare;
- `rusqlite` in any other crate, or outside the store's `[dependencies]`;
- `println!`, `print!`, `eprintln!` or `std::io::stdout` in a non-test source file;
- a tool module that names another tool module;
- a `TODO(phase…` hand-off marker in any source or test file.

Each rule is proved twice: against the real tree, and against a synthetic input that makes it fire. Tests are
exempt from the stdout and tool-isolation scans by path — a test prints its own evidence — but not from the
marker scan. `build.rs` files sit at the crate root, outside the walk, because a build script writes to
cargo's pipes and never to a running process's stdout. Third-party dependencies are free: only the
`sangkar-*` prefix is the DAG's business.

Other invariants that break the product when violated:

- **stdout is the MCP protocol.** Every log line goes to stderr through `rust/crates/sangkar-core/src/log.rs`,
  the workspace's only stderr writer; the boundary scan bans the print macros outside tests, and
  `rust/crates/sangkar-cli/tests/mcp_surface.rs` asserts the protocol end-to-end. The `tools/call` result text
  is opaque inside the frame — TOON by default, JSON under `sangkar mcp --json` — so the invariant is about
  the bytes around it, never about its encoding.
- **Tools never touch a connection.** A handler receives `ToolContext` from
  `rust/crates/sangkar-store/src/read/context.rs` (`reader`, `cursor`, `log`, `now`, `config`, and the size
  recorded for the file the reader opened, `dbSizeBytes`). It does not open a database, does not import
  `rusqlite`, does not read `std::env`: the settings that do come from the environment are read once, in
  `rust/crates/sangkar-core/src/config.rs` (`SANGKAR_CURSOR_KEY`, `SANGKAR_TELEMETRY`, the machine-level
  `SANGKAR_HOME`, and `SANGKAR_WORKER_CMD` for the command an index run spawns).
  `rust/crates/sangkar-cli/src/dispatch.rs` starts no watcher — the daemon owns every poller, one per
  registered repository.
- **Never build a query inside a handler or a loop.** Prepared statements only, through the bounded LRU in
  `rust/crates/sangkar-store/src/db.rs` (`STATEMENT_CACHE_CAPACITY`, 200) over the driver's own capped
  statement cache; `health.statement_cache_entries` publishes the size, so unbounded growth is observable
  instead of silent.
- **Never publish a partial graph.** Write to a stage, `seal()` (`wal_checkpoint(TRUNCATE)`,
  `journal_mode=DELETE`, `synchronous=FULL`, `quick_check`), then `rename()` over the live file;
  `rust/crates/sangkar-store/src/publish.rs` guarantees the discard path. Readers keep serving the previous
  generation.
- **`total_relation: "eq"` is a claim.** It is legal only when coverage is complete, no `unresolved` rows
  exist in scope, the generation is fresh, and the payload was not truncated. `emit()` in
  `rust/crates/sangkar-core/src/budget.rs` downgrades `eq` to `gte` itself when it truncates — do not "fix"
  that.
- **Eleven tables, one ledger.** `rust/crates/sangkar-store/migrations/0000_init.sql` is the drizzle-kit
  capture of the eleven tables and is never edited; FTS5 lives in
  `rust/crates/sangkar-store/migrations/raw/0001_fts.sql`. Both are embedded by `include_str!` and applied by
  one runner (`rust/crates/sangkar-store/src/migrate.rs`), forward-only, with `meta.schema_version` counting
  the files and a newer database refused rather than downgraded. `rust/crates/sangkar-store/src/schema.rs`
  names the tables and the `meta` keys for the tests to compare against `sqlite_master`.
- **One lockfile, committed.** `rust/Cargo.lock` is the only pin: every dependency version comes from it, and
  the release build (`.github/workflows/release.yml`'s `cargo build --release --locked …`) and the README's
  checkout command both pass `--locked`, so a stale lockfile fails the build instead of being rewritten
  behind a release's back. A dependency change is a lockfile diff reviewed like code — bump it with
  `cargo update -p <crate>` in its own commit, never by hand-editing a version. The toolchain is pinned once
  in `rust/rust-toolchain.toml` (1.100.0 with rustfmt and clippy) and named a second time in
  `.github/workflows/ci.yml`; the two must agree.

## Keeping this guidance true

The files this file points at are **part of the change, never a follow-up**. A commit that leaves one of them
wrong is incomplete in the same way as one that leaves `docs/my-product/TOOLS-SPEC.md` stale — so synchronize
the file in this directory a change touches, in the same commit:

| When you change… | Update, in the same commit |
|---|---|
| a crate, a module, a binary, a crate-boundary, or one of the recipes | [`project-layout.md`](project-layout.md) — its tree block, that recipe, and the data model if a table or column moved |
| a plan's or task's status, a measured graph number, or one of the known gaps | [`gaps-n-future-works.md`](gaps-n-future-works.md) — the numbers you just measured, never the ones already written |
| add, rename, delete or re-scope a test file | [`test-map.md`](test-map.md) — the "if you change… → these will tell you" row |

`gaps-n-future-works.md` carries counts taken from a real index of this tree, so an edit there needs the run
that produced them: `cargo build --release -p sangkar-cli`, then
`./rust/target/release/sangkar index --repo-root . --data-dir /tmp/… --mode full`, then the query the section
cites. Re-measure, never estimate. If a change invalidates guidance you cannot restate honestly, say so in
the commit body instead of leaving a confident stale line.

## Conventions

- Conventional commits, scoped, with `!` for behaviour/structure changes (`refactor!: delete the TypeScript
  tree`).
- `snake_case` in SQL, in Rust and on the wire. The payload keys are pinned by the goldens, so a name moves
  only in the commit that moves its golden.
- Comments explain **why** (a constraint, a measured number, a failure seen in production), never what the
  next line does. The existing code is a good model: `rust/crates/sangkar-index/src/lang/registry.rs` on why
  `impl_item` is not a class type, or the direction note on `tested_by` in
  `rust/crates/sangkar-index/src/persist/unit.rs`.
- `rustfmt` for formatting; `cargo fmt` writes, and `--check` is the gate. Do not hand-format.
- Never weaken an assertion to make a test pass. If a test encodes a contract and the contract changed, change
  both and say so in the commit body. The goldens under `rust/tests/golden/` are a contract in that sense.
