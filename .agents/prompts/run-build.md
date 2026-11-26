# Run and build

How to run and check the code on this machine. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Run it

`sangkar` is one native binary from the Cargo workspace under `rust/`. `cargo` 1.100.0 is on `PATH` through
`~/.cargo/bin` (`rustup` puts it there, and `.profile` / `.bashrc` source `~/.cargo/env`); a stripped shell
exports it explicitly, which is the form every command below assumes:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
```

`rustup` fetches the channel `rust/rust-toolchain.toml` pins on the first command that needs it;
`rustup toolchain install 1.100.0 --component rustfmt --component clippy` does it up front. **`cc` has to
exist**: `rusqlite`'s `libsqlite3-sys` compiles the SQLite amalgamation and every `tree-sitter-*` grammar
compiles C or C++, so a machine without a C toolchain fails before it links.

The workspace manifest is `rust/Cargo.toml` and the repository root has none, so every command either runs
inside `rust/` or passes `--manifest-path rust/Cargo.toml`. The forms below are spelled from the repository
root, so a copy-paste works from anywhere in the tree.

```sh
cargo build --release --locked --manifest-path rust/Cargo.toml -p sangkar-cli
./rust/target/release/sangkar index --repo-root .        # first index; until then every tool answers bootstrapping
./rust/target/release/sangkar status                     # the envelope as TOON; --json for one line of JSON
./rust/target/release/sangkar trace 'dispatch' --depth 1
./rust/target/release/sangkar mcp                        # MCP server on stdio
```

`sangkar index` asks before it uses a tree it found *above* the working directory, so a non-interactive run
needs `--yes` unless it starts in the tree itself or passes `--repo-root` (`INDEXING-PIPELINE.md` §1.1). Every
command above runs from the tree, which is the case that never asks.

Data lives in the repository's `.sangkar` directory and `--data-dir` overrides it. In this repository a daemon
is usually watching that store, so a run that only measures the tree — the numbers in
[`gaps-n-future-works.md`](gaps-n-future-works.md), for one — uses a temp `--data-dir` instead of competing
with it. Delete the stale `.codegraph/` directory after the first index (`.gitignore` says as much).

## Commands

| Command | Does |
|---|---|
| `cargo fmt --manifest-path rust/Cargo.toml --all --check` | rustfmt over the workspace; `cargo fmt --all` writes |
| `cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings` | the lint gate: warnings denied, `[workspace.lints]` applied |
| `cargo test --manifest-path rust/Cargo.toml --workspace` | the whole workspace: 978 tests, 71 suites, 1 ignored |
| `cargo test --manifest-path rust/Cargo.toml -p sangkar-boundary` | the module-rule gate: 8 tests over the nine manifests and every source file |
| `cargo test --manifest-path rust/Cargo.toml -p sangkar-cli --test goldens` | the parity bar: 57 goldens replayed, 57 identical |
| `cargo check --manifest-path rust/Cargo.toml --workspace --all-targets` | type-check every crate and its tests without linking |
| `cargo build --release --locked --manifest-path rust/Cargo.toml -p sangkar-cli --target x86_64-unknown-linux-gnu`, then `strip rust/target/x86_64-unknown-linux-gnu/release/sangkar` | the release build the workflow runs, in the same order: one target, `--locked`, then strip |
| local asset-shape check: `tar -czf`, `( cd dist && sha256sum … > SHA256SUMS )`, then `tar -tzf` | what the release tarball carries: one member, `sangkar`, at the archive root |
| `sh -n install.sh` | parse-check the installer the canonical URL serves; CI runs the same check |

**The workflow builds the artifact, not a script.** `.github/workflows/release.yml` runs the build above on
`ubuntu-22.04` for `x86_64-unknown-linux-gnu`, strips the output, asserts ELF, x86-64 and a highest
`GLIBC_*` symbol version at or below 2.35, and attaches exactly two assets: the tarball
`sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz` and a `SHA256SUMS` naming it by bare filename. The
version comes from `rust/Cargo.toml`, and a tag build asserts that `v<that version>` equals the pushed tag
name. Nothing is published to a package registry: the root `install.sh` fetches the tarball from the
Release and verifies it against `SHA256SUMS` before writing. The local asset-shape check above is the same
shape, proved without the runner.

## Definition of done

```sh
cargo fmt --manifest-path rust/Cargo.toml --all --check \
  && cargo clippy --manifest-path rust/Cargo.toml --workspace --all-targets -- -D warnings \
  && cargo test --manifest-path rust/Cargo.toml --workspace \
  && cargo test --manifest-path rust/Cargo.toml -p sangkar-cli --test goldens
sh -n install.sh
```

Plus, when behaviour changed: the tool's own test, `docs/my-product/TOOLS-SPEC.md` if a payload moved, the
golden that payload invalidates, and one line in the commit body naming the invariant you touched. The goldens
under `rust/tests/golden/` are the only oracle now: `scripts/parity/` captured them and cannot re-run (it
imports the deleted TypeScript tree), so a payload that moves on purpose moves its golden in the same commit
and the diff is reviewed.

For a release, add the local asset-shape check from the command table and `sh -n install.sh`; the build that
ships is the workflow's, the procedure is `.agents/skills/build-n-deploy/SKILL.md`, and
`docs/maintainers.md` holds the distribution facts.
