---
name: build-n-deploy
description: "Cut a sangkar-mcp release from version choice to deployment: pick the SemVer bump under the house standard, write the version into both files, sync README.md with what shipped, run the preflight, commit, tag v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION], push and publish the GitHub Release, then hand the maintainer the npm publish command that needs their OTP. Use when asked to release, deploy, bump a version, cut a tag, sync or refresh the README before a release, write release notes, deploy to GitHub, or publish sangkar-mcp to npm."
license: MIT
compatibility: cargo 1.100.0 (the workspace pins it in rust/rust-toolchain.toml), Node 18+ as `node`, git, a `gh` CLI logged in to Alfar-Lab/sangkar-mcp, npm logged in as the package owner with 2FA. Never runs `npm publish` itself — that step belongs to the maintainer.
---

# build-n-deploy

One release is six moves in this order: choose the number, write it into both files, sync `README.md`
with what shipped, prove the tree with the preflight, tag and publish on GitHub, then hand the
maintainer the npm command. Steps 1–8 are yours. The publish itself (step 9) is a command you print,
never a command you run: `npm publish` needs an OTP only the maintainer has.

This skill restates `docs/maintainers.md` §Publishing to npm and the Definition of done in
`.agents/prompts/run-build.md`. When either moves, this file moves in the same commit.

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
npm view sangkar-mcp versions --json                  # what the registry already holds
```

The log decides the component; the registry check proves the number is free. A commit marked `!` or a
`BREAKING CHANGE:` footer is a `MINOR` bump while 0.x. A back-to-back fix inside the current test
phase changes only the iteration. If the tag or the npm version already exists, that number is spent:
choose the next one, do not reuse it.

## 3. Write it into both files

The version lives twice and nothing binds the copies:

- `package.json` → `"version"`
- `rust/Cargo.toml` → `[workspace.package] version`, which every crate inherits through
  `version.workspace = true`

The binary carries no hand-written version constant: `sangkar-core`'s `server_version()` returns
`env!("CARGO_PKG_VERSION")`, which is what the MCP `initialize` answer reports and what telemetry
records. A release that bumps only `package.json` therefore ships a server that lies about itself. Bump
both in the release commit, then find every other copy of the number (substitute the one you are
cutting):

```sh
grep -n '"version"\|^version' package.json rust/Cargo.toml npm/linux-x64-gnu/package.json
grep -rn '0\.6\.0-beta\.1' --include='*.toml' --include='*.json' --include='*.rs' \
  package.json rust/Cargo.toml rust/crates npm/linux-x64-gnu/package.json
```

`npm/linux-x64-gnu/package.json` is generated, not edited: `rust/scripts/package.sh` rewrites it from
the root one and refuses when `optionalDependencies` pins the platform package at another version. The
second grep also prints the telemetry fixtures that hardcode the current `server_version`, and the
preflight in step 5 is what tells you one was missed.

## 4. Sync `README.md` with what shipped

`README.md` is the user contract this tag ships, so the tag must carry one that already describes it.
Correct the README *before* the release commit: a reader who clones the tag, or lands on the GitHub
Release, reads the README first.

**Find the drift.** What moved since the last tag decides which section can be stale:

```sh
LAST=$(git describe --tags --abbrev=0)
git log --oneline "$LAST"..HEAD
git diff --stat "$LAST"..HEAD -- rust package.json bin npm
```

| If this moved | Re-read this README section |
|---|---|
| `rust/crates/sangkar-cli/src/args.rs`, `dispatch.rs`, `render.rs`, a tool directory under `rust/crates/sangkar-tools/src/` | `README.md` §CLI, §MCP & Skill — names, order, defaults, the `budget` sentence |
| `rust/crates/sangkar-daemon/**`, `rust/crates/sangkar-cli/src/repo.rs` | `README.md` §MCP & Skill — the registry and daemon block |
| config resolution in `rust/crates/sangkar-core/**`, a `SANGKAR_*` variable, `--data-dir` | `README.md` §Environment, §Where the data lives |
| `rust/crates/sangkar-store/**`, its `migrations/`, a table or an index | `README.md` §Where the data lives; `docs/maintainers.md` §Distribution |
| `rust/crates/sangkar-telemetry/**` | `README.md` §MCP & Skill (the report verb), §Environment (`SANGKAR_TELEMETRY`) |
| `package.json`, `rust/scripts/package.sh`, `rust/Cargo.toml`'s version | `docs/maintainers.md` §Checks, §Distribution, §Publishing to npm |
| anything in this release at all | `README.md` §Install — the version it names |

**Re-check the pinned facts by running them, not by reading them:**

```sh
grep -n '0\.[0-9]\+\.[0-9]\+' README.md      # every version string must name an installable version
grep -n 'beta\|latest' README.md             # every dist-tag sentence
npm view sangkar-mcp dist-tags               # the truth those sentences must match
npm view sangkar-mcp-linux-x64-gnu dist-tags # whether the platform package is on the registry yet
grep -n 'seven \|eleven \|58 \|903 ' README.md   # counts written in prose
(cd rust && cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu)
./rust/target/x86_64-unknown-linux-gnu/release/sangkar --help         # against the CLI blocks
./rust/target/x86_64-unknown-linux-gnu/release/sangkar daemon --help   # and each verb the README shows
(cd rust && cargo test -p sangkar-cli --test goldens -- --nocapture 2>&1 | grep compared)
```

What those checks enforce:

- A version string in the README must be one a reader can install today: the `beta` tag's version, or a
  number the registry already serves. The previous release's number left behind in prose is the defect
  this step exists for; so is a number that only exists in a checkout.
- A verb, tool name, payload key or default in prose is wrong the moment the code disagrees with it.
  Fix the text; do not soften it with "currently" or "approximately".
- The README's shape is a contract too: `## sangkar-mcp`, `## Installation & Requirements` (its `###`
  subsections), `## Usage` with `### MCP & Skill` and `### CLI`, `## License` — in that order, and
  nothing else. Maintainer material (distribution, publishing, checks) belongs in
  `docs/maintainers.md`, not as a new top-level section.
- `docs/maintainers.md` §Publishing to npm restates the traps in step 9 of this skill — when that flow
  moves, both files move in the same commit.

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

Add when the change touches packaging:

```sh
(cd rust && cargo build --release --locked -p sangkar-cli --target x86_64-unknown-linux-gnu)
npm pack --dry-run                            # parent: 7 files
cd npm/linux-x64-gnu && npm pack --dry-run    # platform: 2 files
```

A red preflight is not a release. Report the failure and stop; do not lower an assertion, skip a
file or tag anyway.

## 6. Commit, split by concern

Three commits, in this order, on `main`:

1. the change itself — `fix(scope): …`, `feat(scope): …`, `!` when behaviour or the repo structure
   breaks;
2. `docs(readme): <what was stale>` — step 4, when anything was;
3. `chore(release): prepare <version>` — the two version strings only.

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

Pushing the tag is the release: `.github/workflows/release.yml` builds the binary on `ubuntu-22.04`,
asserts that its highest `GLIBC_*` symbol version is at most the 2.35 in its `GLIBC_FLOOR`, attaches
`sangkar-$V-x86_64-unknown-linux-gnu.tar.gz` (the stripped binary) and
`sangkar-mcp-linux-x64-gnu-$V.tgz` (the platform package), and creates the Release with `prerelease`
set from the tag itself — a `-` in the name means prerelease. No `gh release create`, no asset uploaded
by hand.

```sh
gh run watch                                  # the run the tag in step 7 started
gh release view "v$V" --json tagName,isPrerelease,url,assets
```

These run after the tag exists; there is nothing to watch before it. The Release body lives in
`release.yml`'s `body:` — it names the glibc floor and the publish order, so it moves with this
procedure, not with one release. To correct a body after the fact, `gh release edit`; never delete the
tag.

A `workflow_dispatch` run on a branch builds and uploads the same artifacts without creating a Release,
which is how to rehearse the build.

## 9. npm — print these two, do not run them

Stop here and give the maintainer exactly these lines, in this order, `$V` expanded:

```sh
npm pack --dry-run                                              # parent: 7 files, 46.8 kB unpacked
(cd npm/linux-x64-gnu && npm pack --dry-run)                    # platform: 2 files, 30.9 MB unpacked
(cd npm/linux-x64-gnu && npm publish --tag beta --otp=<six digits>)   # 1. the platform package
npm publish --tag beta --otp=<six digits>                       # 2. the launcher
```

For a stable release both tags are `--tag latest`.

**The binary that publishes is the Release's, not a local build's.** Unpack
`sangkar-mcp-linux-x64-gnu-$V.tgz` over `npm/linux-x64-gnu/` first: `package.sh` on the maintainer's
machine leaves a `GLIBC_2.39` binary at `npm/linux-x64-gnu/bin/sangkar`, and the workflow's, built on
`ubuntu-22.04`, reports `GLIBC_2.34` against a 2.35 floor. Unpacking also rewrites that directory's
`package.json`, which is generated and identical either way.

**The platform package goes first.** The parent is only a launcher: `bin/sangkar.js` resolves
`sangkar-mcp-linux-x64-gnu/package.json` from the install and exits 70 when that lookup finds nothing.
Publishing the parent first advertises an install that cannot start a server for as long as the
platform package is missing. `docs/maintainers.md` §Distribution states the same order.

Why the rest is not optional:

- **`--tag` is mandatory.** npm 11 refuses outright without it — `You must specify a tag using --tag
  when publishing a prerelease version` — and a publish that lands without it takes `latest`, which a
  `0.x` pre-release must never hold. `latest` still sits on `0.1.0-beta.0`, left there when the name
  was first claimed, so `sangkar-mcp@beta` is the address to advertise.
- **Nothing is printed about `bin`.** The field is `"sangkar": "bin/sangkar.js"`, which npm accepts as
  written; `npm publish --tag beta --dry-run` in either directory prints the file list and stops
  without publishing.
- **Without an OTP nothing publishes:** `E403 … Two-factor authentication or granular access token
  with bypass 2fa enabled is required`.

Never put an OTP in a file, an env var you created, or a command you run yourself. The maintainer
runs it, or supplies a granular token with 2FA bypass in their own `~/.npmrc`.

## 10. Confirm the publish

After the maintainer reports success:

```sh
npm view sangkar-mcp dist-tags                    # beta moved; latest unchanged for a pre-release
npm view "sangkar-mcp@$V" version
npm view sangkar-mcp-linux-x64-gnu dist-tags      # the platform package is on the registry
npm i -g "sangkar-mcp@$V" && sangkar status       # from a shell with no checkout
```

If `dist-tags` shows a pre-release on `latest`, say so plainly and ask the maintainer to run
`npm dist-tag add "sangkar-mcp@<last stable>" latest` — the skill does not republish to fix a tag.

Then close the README loop, once: `## Install` said "install from the tag" in step 4, so point it at
the address that now exists and prove it with the same greps.

```sh
grep -n '0\.[0-9]\+\.[0-9]\+' README.md
npm view sangkar-mcp dist-tags                 # what the sentence must now say
```

Commit that as its own `docs(readme): point install at the published <tag>` after the tag. The tag
keeps the line that was true when it was cut; `main` carries the one that is true now.

## 11. Never

- Delete, move or reuse a tag, or republish an existing version. A defective release ships as the
  next number; a defective npm version gets `npm deprecate sangkar-mcp@$V "<reason>"` and then a
  new number — the next iteration while a pre-release, the next `PATCH` once stable. Editing a
  GitHub Release body with `gh release edit` is fine; deleting the tag is not.
- Tag a dirty tree, or tag before the preflight is green.
- Tag a release whose `## Install` still names the previous version, or whose verb and tool list
disagrees with `--help`. That is step 4, and it is not optional.
- Bump one version copy and not the other.
- Publish the parent before the platform package, or publish only one of them.
- Publish with `latest` while the version is a pre-release or `MAJOR` is 0.
- Force-push `main` or a tag without explicit approval from the maintainer.
- Touch `.ppm/*/tasks/*.json` while cutting a release.
