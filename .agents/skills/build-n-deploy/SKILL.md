---
name: build-n-deploy
description: "Cut a sangkar-mcp release from version choice to deployment: pick the SemVer bump under the house standard, write the version into both files, sync README.md with what shipped, run the preflight, commit, tag v[MAJOR].[MINOR].[PATCH]-[TAG].[ITERATION], push and publish the GitHub Release, then hand the maintainer the npm publish command that needs their OTP. Use when asked to release, deploy, bump a version, cut a tag, sync or refresh the README before a release, write release notes, deploy to GitHub, or publish sangkar-mcp to npm."
license: MIT
compatibility: Bun 1.4.4+ at ~/.bun/bin/bun (not on PATH), git, a `gh` CLI logged in to Alfar-Lab/sangkar-mcp, npm logged in as the package owner with 2FA. Never runs `npm publish` itself — that step belongs to the maintainer.
---

# build-n-deploy

One release is six moves in this order: choose the number, write it into both files, sync `README.md`
with what shipped, prove the tree with the preflight, tag and publish on GitHub, then hand the
maintainer the npm command. Steps 1–8 are yours. The publish itself (step 9) is a command you print,
never a command you run: `npm publish` needs an OTP only the maintainer has.

This skill restates `docs/maintainers.md` §Publishing to npm and the Definition of done in `AGENTS.md`. When
either moves, this file moves in the same commit.

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
- `src/index.ts` → `const SERVER_VERSION = "…"` (≈ line 141)

`SERVER_VERSION` is what the MCP `initialize` answer reports and what telemetry records, so a release
that bumps only `package.json` ships a server that lies about itself. Bump both in the release commit,
then confirm there is no third copy:

```sh
grep -rn "0\.[0-9]*\.[0-9]*" --include='*.ts' --include='*.json' src package.json
```

## 4. Sync `README.md` with what shipped

`README.md` is the user contract this tag ships, so the tag must carry one that already describes it.
Correct the README *before* the release commit: a reader who clones the tag, or lands on the GitHub
Release, reads the README first.

**Find the drift.** What moved since the last tag decides which section can be stale:

```sh
LAST=$(git describe --tags --abbrev=0)
git log --oneline "$LAST"..HEAD
git diff --stat "$LAST"..HEAD -- bin src package.json
```

| If this moved | Re-read this README section |
|---|---|
| `bin/sangkar.ts`, `src/shared/cli/**`, any tool `manifest.ts` | `README.md` §CLI, §MCP & Skill — names, order, defaults, the `budget` sentence |
| `src/daemon/**`, `src/shared/registry.ts`, `src/shared/repo-config.ts` | `README.md` §MCP & Skill — the registry and daemon block |
| `src/shared/config.ts`, a `SANGKAR_*` variable, `--data-dir` | `README.md` §Environment, §Where the data lives |
| `src/shared/store/**`, `migrations/**`, a table or an index | `README.md` §Where the data lives; `docs/maintainers.md` §Distribution |
| `src/shared/telemetry/**` | `README.md` §MCP & Skill (the report verb), §Environment (`SANGKAR_TELEMETRY`) |
| `package.json` scripts, the `bun build` flags, `files` | `docs/maintainers.md` §Checks, §Distribution, §Publishing to npm |
| anything in this release at all | `README.md` §Install — the version it names |

**Re-check the pinned facts by running them, not by reading them:**

```sh
grep -n '0\.[0-9]\+\.[0-9]\+' README.md      # every version string must name an installable version
grep -n 'beta\|latest' README.md             # every dist-tag sentence
npm view sangkar-mcp dist-tags               # the truth those sentences must match
grep -n 'seven \|eleven \|58 \|903 ' README.md   # counts written in prose
bun run bin/sangkar.ts --help                # compare against the `## CLI usage` block
bun run bin/sangkar.ts daemon --help         # and each verb the README demonstrates
bun test 2>&1 | tail -3                      # the count the README quotes, if it quotes one
```

What those checks enforce:

- A version string in the README must be one a reader can install today. Before the npm publish that
  is the tag being cut (`bun install -g github:Alfar-Lab/sangkar-mcp#v$V`) or the previous `beta`;
  after the publish it is `beta` itself. The previous release's number left behind in prose is the
  defect this step exists for.
- A verb, tool name, payload key or default in prose is wrong the moment the code disagrees with it.
  Fix the text; do not soften it with "currently" or "approximately".
- The README's shape is a contract too: `## sangkar-mcp`, `## Installation & Requirements` (its `###`
  subsections), `## Usage` with `### MCP & Skill` and `### CLI`, `## License`, `## Future Works` — in
  that order, and nothing else. Maintainer material (distribution, publishing, checks) belongs in
  `docs/maintainers.md`, not as a new top-level section.
- `docs/maintainers.md` §Publishing to npm restates the traps in step 9 of this skill — when that flow
  moves, both files move in the same commit.

Commit the result as its own change, `docs(readme): <what was stale>`, before the release commit: a
README edit folded into `chore(release)` hides it from anyone reading the log. If the behaviour a
`docs/my-product/*.md` file specifies moved too, that doc moves in the change commit that moved it,
under the same AGENTS.md rule.

## 5. Preflight

`bun` is not on `PATH` on this machine, and the scripts shell out to `bun`, so the export is part of
the command:

```sh
export PATH="$HOME/.bun/bin:$PATH"
bun run typecheck && bun run lint && bun run test && bun run db:drift
```

Add when the change touches what they guard:

- `bun run acceptance` — budgets, latency, the prepared-statement path.
- `bun test tests/binary-smoke.test.ts` — `bin/`, packaging, migrations shipped in the binary. It
  builds `dist/sangkar` itself.

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

```sh
gh release create "v$V" --title "v$V — <one phrase naming what the release delivers>" \
  --prerelease --notes-file /tmp/notes.md
```

`--prerelease` for any `-alpha.N` / `-beta.N` / `-rc.N` tag; drop it only for a stable version.
Repository `Alfar-Lab/sangkar-mcp`, remote over ssh, `gh` account `alfahluzi`.

Notes follow the shape of `v0.2.0-beta.2`:

```markdown
## <the one sentence this release delivers>

<The failure or gap it closes, with the command output a user saw if there is one. Name the
previous version it regresses from.>

### What changed

- `path/to/file.ts`: what moved and by how much, and why that number.
- `tests/x.test.ts`: the assertion that now holds, and that it fails on the old code.

<A paragraph naming what did NOT move — the invariants: tables, ledger, publish protocol, cursor
wire format.>

### Notes

- npm state: which version the `beta` tag serves, and the install line to use until the publish
  lands (`bun install -g github:Alfar-Lab/sangkar-mcp#v$V`).
- Verified on this tag: the checks you ran, with their numbers.
```

Verify what the world now sees, then move on:

```sh
gh release view "v$V" --json tagName,isPrerelease,url
```

## 9. npm — print this, do not run it

Stop here and give the maintainer exactly these lines, `$V` expanded:

```sh
npm pack --dry-run                                   # tarball: bin, src, agent, README.md (+ LICENSE)
npm publish --tag beta --otp=<six digits>            # or: bun run release:beta -- --otp=<code>
```

For a stable release the tag is `--tag latest`.

Why each line is not optional:

- **`--tag beta` is mandatory.** npm 11 ignores `publishConfig.tag`; without the flag a prerelease
  takes `latest`, which is the one tag a `0.x` pre-release must never hold. `latest` already sits on
  a pre-release (`0.1.0-beta.0`, left there by the staged-publish flow that first claimed the name),
  so an untagged install is a moving target and `sangkar-mcp@beta` is the address to advertise.
- **If npm prints `bin[sangkar] script name bin/sangkar.ts was invalid and removed`, it is
  cosmetic.** npm rewrites the field to strip a leading `./`; the entry survives. With the canonical
  `bin/sangkar.ts` written in `package.json` the warning no longer appears, which
  `npm publish --dry-run` proves.
- **Without an OTP nothing publishes:** `E403 … Two-factor authentication or granular access token
  with bypass 2fa enabled is required`.

Never put an OTP in a file, an env var you created, or a command you run yourself. The maintainer
runs it, or supplies a granular token with 2FA bypass in their own `~/.npmrc`.

## 10. Confirm the publish

After the maintainer reports success:

```sh
npm view sangkar-mcp dist-tags                 # beta moved; latest unchanged for a pre-release
npm view "sangkar-mcp@$V" version
bun add -g "sangkar-mcp@$V" && sangkar status  # from a checkout-free shell
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
- Publish with `latest` while the version is a pre-release or `MAJOR` is 0.
- Force-push `main` or a tag without explicit approval from the maintainer.
- Touch `.ppm/*/tasks/*.json` while cutting a release.
