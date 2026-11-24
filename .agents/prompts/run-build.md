# Run and build

How to run and check the code on this machine. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Run it

`bun` **is on `PATH`** on this machine: 1.4.4, reached as `bun` through `~/.local/bin/bun` →
`~/.bun/bin/bun`, and `.profile` / `.bashrc` add `$BUN_INSTALL/bin` as well. `sangkar` is linked into
`~/.local/bin` too. A checkout whose link you have not made runs `bun run bin/sangkar.ts`.

A stripped shell has none of that — `env -i`, a cron job, a CI image without the profile — and there the
export is part of the command: `PATH="$HOME/.bun/bin:$PATH"`.

`sangkar index` asks before it uses a tree it found *above* the working directory, so a non-interactive
run needs `--yes` unless it starts in the tree itself or passes `--repo-root` (`INDEXING-PIPELINE.md`
§1.1). Every command below runs from the tree, which is the case that never asks.

```sh
bun install
bun run bin/sangkar.ts index          # first index; until then every tool answers bootstrapping
bun run bin/sangkar.ts status         # the envelope as TOON; add --json for one line of JSON
bun run bin/sangkar.ts trace 'handleRequest' --depth 1
bun run mcp                           # MCP server on stdio
```

A script that shells out to `bun` (`test`, `acceptance`, `db:parity`) inherits this `PATH`, so the call is
plain: `bun run test`. Verified the other way round too — `bun run db:parity` runs, and its script body is a
`bun test …` command that resolves the same binary.

| Script | Does |
|---|---|
| `bun run test` | whole suite (900 tests, 58 files) |
| `bun run typecheck` | `tsc --noEmit` |
| `bun run lint` | biome over `src tests bin` |
| `bun run acceptance` | the 9 acceptance assertions, incl. latency and the builder-vs-prepared ratio |
| `bun run db:generate` / `db:drift` / `db:parity` | Drizzle migration + drift + schema parity |
| `bun run build` | `bun build --compile` with `--asset src/shared/store/migrations` — migrations are embedded in the binary, so a new migration file must stay under that directory |

Data lives in `<repoRoot>/.sangkar/`; override with `--data-dir`. Delete the stale `.codegraph/` directory
after the first `sangkar index` (`.gitignore` says as much).

## The Rust tree

`rust/` is a second workspace under the toolchain `rust/rust-toolchain.toml` pins: 1.100.0 with rustfmt and
clippy. `rustup` holds it in `~/.cargo/bin`, and `.profile` / `.bashrc` source `~/.cargo/env` — a stripped
shell exports `PATH="$HOME/.cargo/bin:$PATH"`, which is the call every command below assumes. rustup fetches
the pinned channel on the first command inside `rust/`; `rustup toolchain install 1.100.0` does it up front.
`cc` has to exist: `rusqlite`'s `libsqlite3-sys` compiles the SQLite amalgamation and every `tree-sitter-*`
grammar compiles C or C++, so a box without a C toolchain fails before it links.

Run all of these from `rust/`; there is no root manifest, and `src/` is still the tree that answers.

| Command | Does |
|---|---|
| `cargo check --workspace` | type-check all nine crates (`--all-targets` adds the tests) |
| `cargo fmt --check` | rustfmt over the workspace; `cargo fmt` writes |
| `cargo clippy --workspace --all-targets -- -D warnings` | the lint gate: warnings denied, `[workspace.lints]` applied |
| `cargo test -p <crate>` | one crate — `-p sangkar-boundary` is the module-rule gate, `-p sangkar-core` the kernel |
| `cargo test --workspace` | the whole workspace: the gate a phase closes on, not something green while its modules are stubs |

`cargo check`, `cargo fmt --check` and `cargo clippy` pass over the workspace; while a phase is in flight the
loop is `cargo test -p <crate>`, and `cargo test --workspace` is what that phase's last task makes pass.
`cargo run -p sangkar-cli -- status` is the binary under construction — it answers nothing until the read path
lands.

## Definition of done

```sh
bun run typecheck && bun run lint && bun run test && bun run db:drift
```

Plus, when behaviour changed: the tool's own test, `docs/TOOLS-SPEC.md` if a payload moved, and one line in
the commit body naming the invariant you touched. `bun run acceptance` when latency, budgets or the prepared
statement path could have moved. For `rust/` the same gate is
`cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`, plus `cargo test -p <crate>` while
a phase is still filling stubs and `cargo test --workspace` once its last task lands.
