# Maintaining sangkar

For whoever builds the artifact or cuts a release. `README.md` is for users and `AGENTS.md` is for
whoever edits the code. This page covers how the release artifact is built, what a GitHub Release
carries, how the root `install.sh` turns those assets into an installed binary, and the checks to run
before a tag. None of it ships in the release, which holds one binary and a checksum.

Two files are named `install.sh`: the one at the repository root installs the `sangkar` binary from a
release tarball, and `agent/install.sh` installs the Agent Skills skill. This page is about the first.

## Cutting a release

The procedure is `.agents/skills/build-n-deploy/SKILL.md`: the SemVer standard, the version string in
`rust/Cargo.toml`, the README sync, the tag, and the GitHub Release. Follow that rather than
re-deriving it here.

The tag is `v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION]` — `v0.7.0-beta.2`, or `v0.7.0` once the suffix
is dropped. `.github/workflows/release.yml` triggers on any `v*` tag: it builds the binary, strips it,
asserts the glibc floor, and attaches exactly two assets to the GitHub Release —
`sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz` and `SHA256SUMS`. A `workflow_dispatch` run builds
and uploads the same two assets without creating a Release, so a release can be rehearsed.

## Distribution

One GitHub Release per tag, holding two assets:

1. **`sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz`** — one stripped `sangkar` binary at the
   archive root, and nothing else. No launcher, no `README.md`, no `agent/`.
2. **`SHA256SUMS`** — one line, `<sha256>  sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz`, written
   from inside the directory holding that tarball, so the name in the file matches the asset name.

**The workflow does the whole build.** `.github/workflows/release.yml` runs `cargo build --release
--locked -p sangkar-cli --target x86_64-unknown-linux-gnu` on `ubuntu-22.04`, strips the result,
asserts the glibc floor, archives it, and computes the checksum. There is no packaging script in the
repository.

**The glibc floor is 2.35.** The workflow builds on `ubuntu-22.04`, and the linker may not use a
symbol version newer than that image's libc. The workflow asserts it: it reads the highest
`GLIBC_x.y` symbol version out of the binary and fails when it is above the 2.35 in its `GLIBC_FLOOR`.
A build on this machine gets this machine's libc instead — `GLIBC_2.39` today — so a locally built
binary is fine to test and must not be published as a release. `ubuntu-22.04` is the floor because its
libc is the oldest glibc the project promises: a newer runner would silently raise what every user
must have.

**The installer URL is a branch URL.**
`https://raw.githubusercontent.com/Alfar-Lab/sangkar-mcp/main/install.sh` names `main`, not a tag, and
that is deliberate: the script resolves the version from the API at run time, so a versionless URL
keeps working after every release. Pointing users at a tagged copy would freeze the resolution logic.

## The installer

`install.sh` at the repository root is the only install path for the binary. It needs a POSIX shell,
`uname`, `getconf` and `tar`, a downloader (`curl` or `wget`) and a sha256 tool (`sha256sum` or
`shasum`), and it writes one file: the binary in `$HOME/.local/bin`, or in `--bin-dir DIR`.

**It resolves the newest release, pre-releases included.** The default version comes from
`https://api.github.com/repos/Alfar-Lab/sangkar-mcp/releases?per_page=1`, taking the first `tag_name`.
The endpoint is the list, not `releases/latest`, because `latest` answers with the newest release that
is not marked pre-release and every release so far carries a `-beta.N` suffix. `--version V` skips the
lookup and strips a leading `v`. The archive is unpacked for the single `sangkar` file at its root, so
an asset shaped differently fails before the destination is touched.

**It verifies before it writes.** The tarball's sha256 is compared against the `SHA256SUMS` asset of
the same release, and a mismatch or a missing entry stops the run before anything is unpacked or
written. There is no fallback source: nothing is fetched from a package registry, no mirror is
consulted, and the GitHub Release is the only place a binary comes from.

**It refuses everything but Linux x86_64 with glibc 2.35 or newer.** One line names what is supported,
and the refusal is a real state rather than a documentation shortcut: macOS, Windows, arm64, 32-bit and
musl all have no build to download. The platform check runs before any network call and before
anything is written, so the only side effect of a refusal is the exit status. The `export PATH=` line
is printed when the destination directory is not on `PATH`.

`sh -n install.sh` parses the script without running it, and CI gates on that same check: a shell
syntax error is caught before it can be the command a user pipes into `sh`.

## npm is retired

Nothing is published to npm any more. The registry still serves `sangkar-mcp@0.6.0-beta.1`, the last
TypeScript build, for macOS and Windows, and it is unmaintained: it runs on Bun, it gets no fixes, and
it is not a supported install path for the native release. `sangkar-mcp-linux-x64-gnu` was never
published — the native binary ships as a GitHub Release asset and nowhere else.

Registry state on 2026-11-25, from `npm view sangkar-mcp dist-tags`: `beta` serves `0.6.0-beta.1`,
`latest` serves `0.1.0-beta.0` (the `0.0.0-stage` holding version that took the tag when the name was
first claimed), and `sangkar-mcp-linux-x64-gnu` answers 404. Those tags are history, not a plan: `beta`
is the number the last TypeScript build already spent, so the native release took `0.7.0-beta.1` and
ships `0.7.0-beta.2` after the packaging change; a published number is never reused.

## Checks

Run from the repository root; `cargo` comes from rustup, and the workspace pins 1.100.0 in
`rust/rust-toolchain.toml`.

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cd rust
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                     # 978 passed, 0 failed, 1 ignored
cargo test -p sangkar-cli --test goldens   # 57 compared (57 identical)
```

The golden suite is the durable oracle: `rust/tests/golden/**` and `rust/tests/vectors/**` hold the
answers captured while the TypeScript implementation still answered them, and the test replays every one
against the binary cargo just built. A change that moves an answer moves the golden in the same commit,
with a reason — it is the only thing that still says the two implementations agreed.

Asset shape, when a change touches packaging — the installer and the workflow both depend on it:

```sh
sh -n install.sh                        # the script parses; CI runs this too
cd rust
cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu
mkdir -p ../dist && cp target/x86_64-unknown-linux-gnu/release/sangkar ../dist/sangkar
strip ../dist/sangkar
cd ../dist
tar -czf sangkar-0.7.0-beta.2-x86_64-unknown-linux-gnu.tar.gz sangkar
sha256sum sangkar-0.7.0-beta.2-x86_64-unknown-linux-gnu.tar.gz > SHA256SUMS
tar -tzf sangkar-0.7.0-beta.2-x86_64-unknown-linux-gnu.tar.gz   # exactly one line: sangkar
cat SHA256SUMS                                                 # <sha256>  sangkar-<version>-<target>.tar.gz
```

`tar -tzf` printing exactly `sangkar` is the assertion: the installer unpacks the archive root and
expects one file there. `dist/` holds the assembled assets and is gitignored; a locally built binary is
for this check only, since it carries this machine's glibc. The Bun toolchain and the TypeScript tree
that used to sit beside it are gone: `rust/Cargo.lock` and `rust/rust-toolchain.toml` pin the build.
