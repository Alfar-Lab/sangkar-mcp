# Maintaining sangkar

For whoever builds the artifact or cuts a release. `README.md` is for users and `AGENTS.md` is for
whoever edits the code. Nothing on this page ships in the npm tarball — `files` in `package.json` lists
`bin`, `agent` and `README.md`, and npm adds `package.json` and `LICENSE` itself.

## Cutting a release

The procedure is `.agents/skills/build-n-deploy/SKILL.md`: the SemVer standard, the two version
strings, the README sync, the tag, the GitHub Release, and the two `npm publish` commands the maintainer
runs by hand because they need an OTP. Follow that rather than re-deriving it here.

The tag is `v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION]` — `v0.7.0-beta.1`, or `v0.7.0` once the suffix
is dropped. `.github/workflows/release.yml` triggers on any `v*` tag: it builds the binary, asserts the
glibc floor, attaches the platform tarball and the bare binary to the GitHub Release, and publishes
nothing. A `workflow_dispatch` run builds and uploads the same artifacts without creating a Release, so
a release can be rehearsed.

## Distribution

Two npm packages, in the order `docs/my-product/TECH-SPEC.md` §15 states.

1. **`sangkar-mcp-linux-x64-gnu`** — the platform package, from `npm/linux-x64-gnu/`. Two files: the
   stripped 31,056,160-byte binary and a 276-byte `package.json` declaring `os: ["linux"]`,
   `cpu: ["x64"]` — 31.1 MB unpacked, 6.3 MB packed.
2. **`sangkar-mcp`** — the parent, from the repository root. It holds `bin/sangkar.js` (the launcher),
   `README.md`, `LICENSE` and `agent/`: 7 files, 46.8 kB unpacked. It names the platform package in
   `optionalDependencies`, so an install elsewhere skips it and the launcher refuses with one line and
   exit 70 instead of half-working.

**Platform first, parent second is not a preference.** The parent is only a launcher: it resolves
`sangkar-mcp-linux-x64-gnu/package.json` from the install and exits 70 when the lookup finds nothing.
Publishing the parent first advertises an install that cannot start a server for as long as the platform
package is missing.

**The binary.** `rust/scripts/package.sh` is the whole build:

```sh
bash rust/scripts/package.sh
```

It reads the version out of `package.json`, checks that `optionalDependencies` pins the platform package
at that same version, runs `cargo build --release --locked -p sangkar-cli --target
x86_64-unknown-linux-gnu` inside `rust/`, installs the binary into `npm/linux-x64-gnu/bin/sangkar`,
strips it, and rewrites that directory's `package.json` from the root one. Unstripped, the same build is
34,416,888 bytes in `rust/target/x86_64-unknown-linux-gnu/release/sangkar`; stripped it is 31,056,160.
`npm/linux-x64-gnu/bin/sangkar` is gitignored: the layout is committed, the binary is built.

**The glibc floor is 2.35.** `.github/workflows/release.yml` builds on `ubuntu-22.04`, and the linker
may not use a symbol version newer than that image's libc. The workflow asserts it: it reads the highest
`GLIBC_x.y` symbol version out of the binary and fails when it is above the 2.35 in its `GLIBC_FLOOR`.
A build on this machine gets this machine's libc instead — `GLIBC_2.39` today — so a locally built binary
is fine to test and must not be published as a release.

## Publishing to npm

Both commands are the maintainer's: `npm publish` needs a one-time password, or a granular token with
2FA bypass in `~/.npmrc`, and neither is in this repository. CI does not publish.

```sh
cd npm/linux-x64-gnu && npm publish --tag beta --otp=<six digits>
cd ../.. && npm publish --tag beta --otp=<six digits>
```

`--tag beta` while the version carries a pre-release suffix, `--tag latest` once it does not. The tag is
not optional: npm 11 refuses with `You must specify a tag using --tag when publishing a prerelease
version` when it is missing, and a publish that lands without one takes `latest`, which a `0.x`
pre-release must never hold.

Two facts worth knowing before the first run:

- **The two packages publish separately, so a failure between them is visible.** If the platform package
  lands and the parent fails, the registry serves a platform package nothing points at yet: re-run the
  parent. If the parent lands first, every install on Linux x64 refuses until the platform package
  lands. Publish in the order above.
- **Nothing is printed about `bin` any more.** The field is `"sangkar": "bin/sangkar.js"`, which npm
  accepts as written. `npm publish --tag beta --dry-run` on either package is how to see the file list
  and the packed size without publishing.

State on 2026-11-25, from `npm view sangkar-mcp dist-tags`: `beta` serves `0.6.0-beta.1`, `latest` serves
`0.1.0-beta.0` (the holding version that claimed the name), and `sangkar-mcp-linux-x64-gnu` is not on
the registry yet — `npm view sangkar-mcp-linux-x64-gnu` answers 404. Both version strings in the tree
read `0.7.0-beta.1`, the number the native release takes: `0.6.0-beta.1` is spent, because the registry
already holds it for the parent's TypeScript build, and a published number is never reused.

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

Package shapes, when a change touches packaging:

```sh
npm pack --dry-run                            # parent: 7 files, 46.8 kB unpacked
cd npm/linux-x64-gnu && npm pack --dry-run    # platform: 2 files, 31.1 MB unpacked
```

Neither command writes a tarball and neither publishes. The Bun toolchain and the TypeScript tree that
used to sit beside it are gone: `rust/Cargo.lock` and `rust/rust-toolchain.toml` pin the build.
