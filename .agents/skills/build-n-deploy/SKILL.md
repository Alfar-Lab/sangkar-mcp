---
name: build-n-deploy
description: "Cut a sangkar-mcp release from version choice to deployment: pick the SemVer bump under the house standard, set the version in rust/Cargo.toml, refresh rust/Cargo.lock with cargo, sync README.md with what shipped, run the preflight, commit, tag v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION], push, and let the release workflow attach the tarball and SHA256SUMS to the GitHub Release. Use when asked to release, deploy, bump a version, cut a tag, sync or refresh the README before a release, write release notes, deploy to GitHub, or verify an installed sangkar-mcp release."
license: MIT
compatibility: cargo 1.100.0 (the workspace pins it in rust/rust-toolchain.toml), git, a `gh` CLI logged in to Alfar-Lab/sangkar-mcp, and the tools the release image carries — strip, objdump, tar, sha256sum. No registry login and no OTP: nothing here publishes.
---

# build-n-deploy

One release is seven moves in this order: choose the number, set the version in `rust/Cargo.toml` and
refresh `rust/Cargo.lock`, sync `README.md` with what shipped, prove the tree with the preflight, commit,
tag and push, then watch the workflow build and publish the GitHub Release. Every one of them runs here.
The build that ships is `.github/workflows/release.yml`'s; a `workflow_dispatch` run of that same workflow
is the rehearsal. Nothing is published to a package registry any more: a release is a Git tag, a GitHub
Release, and two assets — the tarball and `SHA256SUMS` — that `install.sh` downloads and verifies.

This skill restates the Definition of done in `.agents/prompts/run-build.md` and the distribution facts in
`docs/maintainers.md` §Distribution and §The installer. When either moves, this file moves in the same
commit.

## 1. The version standard

```
v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION]
```

| Component | Named | Bumped by | Resets |
|---|---|---|---|
| `MAJOR` | major release | a breaking change: removed endpoint, changed API or data shape, re-architected core | `MINOR` and `PATCH` → 0 |
| `MINOR` | new feature | a backward-compatible addition | `PATCH` → 0 |
| `PATCH` | fix | a bug, a performance or security fix, no new feature, no break | nothing |

Judge the bump by what breaks for a user, not by how much work it was.

**The `0.y.z` exception.** While `MAJOR` is 0 the product is pre-production, so breaking changes and
big features both bump `MINOR` (`v0.1.0` → `v0.2.0`). `MAJOR` reaches 1 only when the maintainer
declares the product production-stable — never infer that from a green suite.

**Pre-release labels.** `-[TAG].[ITERATION]` marks a version still under test, and the three labels
are a ladder:

- `alpha` — internal testing, features may still change shape.
- `beta` — features complete, limited staging/UAT testing for bugs.
- `rc` — release candidate, no new features, only critical-bug verification.

`ITERATION` starts at 1 on entering a label and is `+1` per fix *inside that same phase*
(`v0.2.0-beta.1` → `v0.2.0-beta.2`). A fix landed after the stable release is a new `PATCH`, not an
iteration. When testing ends, the whole suffix is dropped (`v0.2.0-rc.1` → `v0.2.0`).

**Immutability and case.** A published or tagged number is never deleted, moved or reused for
different code. A mistake ships as a new number. `v` and the label are always lowercase.

## 2. Choose the next number

```sh
git describe --tags --abbrev=0                        # the last released version
git log --oneline "$(git describe --tags --abbrev=0)"..HEAD
git tag --sort=-v:refname | head -5                   # the numbers already spent
```

The log decides the component. A commit marked `!` or a `BREAKING CHANGE:` footer is a `MINOR` bump while
0.x. A back-to-back fix inside the current test phase changes only the iteration. A tag is the only claim
a number has: nothing publishes, so no registry can hold the number first. If `v$V` already exists, that
number is spent — choose the next one, do not reuse it.

## 3. Set the version, then let cargo refresh the lockfile

The version lives once in the source and a second time in the committed lockfile:

- `rust/Cargo.toml` → `[workspace.package] version`, which every crate inherits through
  `version.workspace = true`
- `rust/Cargo.lock` → the `version` of each of the nine workspace members, carrying the same string nine
  times over

The binary carries no hand-written version constant: `sangkar-core`'s `server_version()` returns
`env!("CARGO_PKG_VERSION")`, which is what the MCP `initialize` answer reports and what telemetry
records. The CLI has no `--version` flag — it answers `unknown tool "--version"` and exits 2 — so a
build's version is read from the manifest that built it.

**Never hand-edit the lockfile copy.** Every release build passes `--locked`, which makes cargo refuse to
rewrite `rust/Cargo.lock`, so a lockfile still holding the previous version fails the build instead of
shipping a server that lies about itself. Let cargo rewrite the nine member entries after the bump:

```sh
(cd rust && cargo check)      # rewrites the member versions; `cargo metadata` does the same
```

Then find every other copy of the number (substitute the one you are cutting):

```sh
grep -n '^version' rust/Cargo.toml
grep -n '0\.7\.0-beta\.1' rust/Cargo.lock          # the version you are leaving, wherever it still sits
grep -rn '0\.6\.0-beta\.1' --include='*.toml' --include='*.rs' rust/   # fixtures that hardcode a version
```

The third grep prints the telemetry fixtures that hardcode a `server_version`, and the preflight in step 5
is what tells you one was missed.

## 4. Sync `README.md` with what shipped

`README.md` is the user contract this tag ships, so the tag must carry one that already describes it.
Correct the README *before* the release commit: a reader who clones the tag, or lands on the GitHub
Release, reads the README first.

**Find the drift.** What moved since the last tag decides which section can be stale:

```sh
LAST=$(git describe --tags --abbrev=0)
git log --oneline "$LAST"..HEAD
git diff --stat "$LAST"..HEAD -- rust install.sh .github/workflows/release.yml
```

| If this moved | Re-read this README section |
|---|---|
| `rust/crates/sangkar-cli/src/args.rs`, `dispatch.rs`, `render.rs`, a tool directory under `rust/crates/sangkar-tools/src/` | `README.md` §CLI, §MCP & Skill — names, order, defaults, the `budget` sentence |
| `rust/crates/sangkar-daemon/**`, `rust/crates/sangkar-cli/src/repo.rs` | `README.md` §MCP & Skill — the registry and daemon block |
| config resolution in `rust/crates/sangkar-core/**`, a `SANGKAR_*` variable, `--data-dir` | `README.md` §Environment, §Where the data lives |
| `rust/crates/sangkar-store/**`, its `migrations/`, a table or an index | `README.md` §Where the data lives; `docs/maintainers.md` §Distribution |
| `rust/crates/sangkar-telemetry/**` | `README.md` §MCP & Skill (the report verb), §Environment (`SANGKAR_TELEMETRY`) |
| `rust/Cargo.toml`'s version, `install.sh`, `.github/workflows/release.yml` | `README.md` §Install — the number it names, the flags, the refusal line; `docs/maintainers.md` §Distribution, §The installer |
| anything in this release at all | `README.md` §Install — the version it names |

**Re-check the pinned facts by running them, not by reading them:**

```sh
grep -n '0\.[0-9]\+\.[0-9]\+' README.md      # every version string must name an installable release
grep -n 'beta\|latest' README.md             # each is this release, an earlier one, or the legacy pointer
grep -n 'seven \|eleven \|58 \|903 ' README.md   # counts written in prose
(cd rust && cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu)
./rust/target/x86_64-unknown-linux-gnu/release/sangkar --help         # against the CLI blocks
./rust/target/x86_64-unknown-linux-gnu/release/sangkar daemon --help   # and each verb the README shows
(cd rust && cargo test -p sangkar-cli --test goldens -- --nocapture 2>&1 | grep compared)
```

What those checks enforce:

- A version string in the README must be one a reader can install today: this release's number, one an
  earlier Release still carries, or the `sangkar-mcp@0.6.0-beta.1` pointer that macOS and Windows install.
  The previous release's number left behind in prose is the defect this step exists for; so is a number
  that only exists in a checkout.
- A verb, tool name, payload key or default in prose is wrong the moment the code disagrees with it.
  Fix the text; do not soften it with "currently" or "approximately".
- The README's shape is a contract too: `## sangkar-mcp`, `## Installation & Requirements` (its `###`
  subsections), `## Usage` with `### MCP & Skill` and `### CLI`, `## License` — in that order, and
  nothing else. Maintainer material (distribution, the installer, the checks) belongs in
  `docs/maintainers.md`, not as a new top-level section.
- `docs/maintainers.md` §Distribution and §The installer restate what steps 5, 8 and 9 of this skill
  check — when that flow moves, both files move in the same commit.

Commit the result as its own change, `docs(readme): <what was stale>`, before the release commit: a
README edit folded into `chore(release)` hides it from anyone reading the log. If the behaviour a
`docs/my-product/*.md` file specifies moved too, that doc moves in the change commit that moved it,
under the same `.agents/prompts/rules.md` rule.

## 5. Preflight

The workspace pins its toolchain in `rust/rust-toolchain.toml`, so `cargo` on `PATH` is enough; export
`$HOME/.cargo/bin` only in a shell that has no profile:

```sh
export PATH="$HOME/.cargo/bin:$PATH"
(cd rust && cargo fmt --all --check)
(cd rust && cargo clippy --workspace --all-targets -- -D warnings)
(cd rust && cargo test --workspace)                    # 978 passed, 0 failed, 1 ignored
(cd rust && cargo test -p sangkar-cli --test goldens)  # 57 compared (57 identical)
```

The golden suite is the release's real gate: `rust/tests/golden/**` and `rust/tests/vectors/**` hold the
answers captured while the TypeScript implementation still answered them, and the test replays every one
against the binary cargo just built. A count below 57, or one difference, is a red preflight.

Add when the change touches packaging — this is the asset shape step 8 attaches, proved on this machine:

```sh
(cd rust && cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu)
V=0.7.0-beta.2            # the version under test
dist="$(mktemp -d)"
tar -czf "$dist/sangkar-$V-x86_64-unknown-linux-gnu.tar.gz" \
  -C rust/target/x86_64-unknown-linux-gnu/release sangkar
( cd "$dist" && sha256sum "sangkar-$V-x86_64-unknown-linux-gnu.tar.gz" > SHA256SUMS )
tar -tzf "$dist/sangkar-$V-x86_64-unknown-linux-gnu.tar.gz"   # one line, exactly: sangkar
cat "$dist/SHA256SUMS"                                        # <sha256>  sangkar-$V-…tar.gz
sh -n install.sh                                              # the installer parses
```

The tarball carries one member, `sangkar`, at the archive root: no directory entry, no `README.md`, no
`agent/`. `SHA256SUMS` holds the bare filename and is written from inside the directory that holds the
tarball, so the name it records is the name the Release publishes and `install.sh` looks up. `sh -n` is
the syntax gate CI runs, and it carries more weight than it looks: `install.sh` is a file users pipe into
`sh`, so a parse error is a release defect rather than a maintainer inconvenience.

A red preflight is not a release. Report the failure and stop; do not lower an assertion, skip a
file or tag anyway.

## 6. Commit, split by concern

Three commits, in this order, on `main`:

1. the change itself — `fix(scope): …`, `feat(scope): …`, `!` when behaviour or the repo structure
   breaks;
2. `docs(readme): <what was stale>` — step 4, when anything was;
3. `chore(release): prepare <version>` — the manifest version and the lockfile cargo refreshed from it,
   nothing else.

The tree must be clean before tagging: the tag has to describe exactly the code the preflight proved.

## 7. Tag and push

```sh
V=0.2.0-beta.2
git tag -a "v$V" -m "sangkar-mcp $V" -m "<two to four sentences: what this release does, and why>"
git push origin main
git push origin "v$V"
```

Annotated tags, `v` lowercase, first line `sangkar-mcp <version>`, then a body a reader can act on.
Before creating one:

```sh
git rev-parse -q --verify "refs/tags/v$V" || git ls-remote --tags origin "refs/tags/v$V"
```

Non-empty output means the number is taken — stop and go back to step 2. Never `-f` a tag.

## 8. Publish the GitHub Release

Pushing the tag is the release: `.github/workflows/release.yml` builds
`cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu` inside `rust/` on
`ubuntu-22.04`, strips the output, asserts it is an ELF x86-64 file whose highest `GLIBC_*` symbol version
is at most the 2.35 in its `GLIBC_FLOOR`, and attaches exactly two assets:

- `sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz` — one `sangkar` at the archive root, nothing else;
- `SHA256SUMS` — one line, `<sha256>  sangkar-<version>-x86_64-unknown-linux-gnu.tar.gz`, written from
  inside the directory holding the tarball so the bare filename is the asset name.

The version comes from `rust/Cargo.toml`, and a tag build additionally asserts that `v<that version>`
equals `$GITHUB_REF_NAME`: a tag naming one number while the manifest says another fails in the workflow
rather than shipping. `prerelease` is set from the tag itself — a `-` in the name means prerelease. No
`gh release create`, no asset uploaded by hand.

```sh
gh run watch                                  # the run the tag in step 7 started
gh release view "v$V" --json tagName,isPrerelease,url,assets
```

These run after the tag exists; there is nothing to watch before it. The Release body lives in
`release.yml`'s `body:` — it names the glibc floor and what the two assets are, so it moves with this
procedure, not with one release. To correct a body after the fact, `gh release edit`; never delete the
tag.

**The floor is the runner image's promise.** `ubuntu-22.04` links against glibc 2.35, so the binary runs
on 2.35 and newer and nowhere older — Debian 12, Ubuntu 22.04+, Fedora 36+. A build on this machine picks
up this machine's libc (`GLIBC_2.39` today) and is fine to test and wrong to ship. Moving the runner is a
release decision, not a maintenance one: it raises what every user must already have.

A `workflow_dispatch` run builds and uploads the same two assets without creating a Release. That is the
rehearsal: run it on a branch when the workflow itself changed, download the uploaded artifact, and check
the shape step 5 checks — the version it builds comes from the manifest, not from the ref.

## 9. Verify the Release from the outside

The Release is right when three things hold, and each one is a command:

```sh
V=0.7.0-beta.2
gh release view "v$V" --json assets --jq '.assets[].name'
# sangkar-0.7.0-beta.2-x86_64-unknown-linux-gnu.tar.gz
# SHA256SUMS

scratch="$(mktemp -d)"
gh release download "v$V" --dir "$scratch"
( cd "$scratch" && sha256sum -c SHA256SUMS )                     # sangkar-…tar.gz: OK
tar -tzf "$scratch/sangkar-$V-x86_64-unknown-linux-gnu.tar.gz"   # one line, exactly: sangkar

sh install.sh --version "$V" --bin-dir "$scratch/bin"
"$scratch/bin/sangkar" --help | head -3
```

Exactly two assets, `sha256sum -c` answers `OK` for the tarball the Release actually holds, and the root
`install.sh` installs that version into a scratch directory whose binary starts. Any of the three failing
is a broken release however green the workflow was: report it, and fix it with a new number rather than by
hand-editing an asset.

Then close the README loop, once: `## Install` advertised the branch URL
`https://raw.githubusercontent.com/Alfar-Lab/sangkar-mcp/main/install.sh` in step 4, and that URL resolves
the newest release at run time, so the command itself needs no edit per release — the version sentence
beside it does. Check it names the number the Release now serves:

```sh
grep -n '0\.[0-9]\+\.[0-9]\+' README.md
gh release view "v$V" --json tagName --jq .tagName   # what that sentence must now say
```

Commit a correction as its own `docs(readme): point install at <version>` after the tag. The tag keeps the
line that was true when it was cut; `main` carries the one that is true now.

## 10. Never

- Delete, move or reuse a tag, or republish a spent tag number. A defective release ships as the next
  number — the next iteration while a pre-release, the next `PATCH` once stable. Editing a GitHub
  Release body with `gh release edit` is fine; deleting the tag is not.
- Tag a dirty tree, or tag before the preflight is green.
- Tag a release whose `## Install` still names the previous version, or whose verb and tool list
  disagrees with `--help`. That is step 4, and it is not optional.
- Tag a version the lockfile disagrees with: `--locked` fails the build, and a hand-edited
  `rust/Cargo.lock` ships a server that lies about itself.
- Publish a tarball whose checksum does not verify, or a `SHA256SUMS` naming a file the Release does
  not hold.
- Force-push `main` or a tag without explicit approval from the maintainer.
- Touch `.ppm/*/tasks/*.json` while cutting a release.
