# Maintaining sangkar

For whoever builds the artifact or cuts a release. `README.md` is for users and `AGENTS.md` is for
whoever edits the code. Nothing on this page ships in the npm tarball — `files` in `package.json`
lists `bin`, `src`, `agent` and `README.md` only.

## Cutting a release

The procedure is `.agents/skills/build-n-deploy/SKILL.md`: the SemVer standard, the two version
strings, the README sync, the tag, the GitHub Release, and the `npm publish` command the maintainer
runs by hand because it needs an OTP. Follow that rather than re-deriving it here.

## Distribution

The order is `docs/my-product/TECH-SPEC.md` §15's, and it is deliberate:

1. **Ship the Bun package first.** A `#!/usr/bin/env bun` entry point is a small artifact, needs no
   rebuild per platform, and only requires Bun on the user's machine (102 MB itself). This is the
   route the seven tools are exercised through today.
2. **Add the compiled binary once the grammar set is fixed.** `bun build --compile` produces a single
   file with no Bun prerequisite, but it is large and must be rebuilt per platform: 104 MB measured
   for a hello-world on Bun 1.4.4, 105.1 MB for this app (393 modules), and roughly 1–3 MB per
   embedded WASM grammar — an estimated 130–160 MB for the 12-language v1 set. Compiling grammars
   in is what makes the artifact big, so it waits until the language list stops moving.

Two facts about the standalone artifact today:

- **Migrations must be embedded.** The migration runner reads `.sql` files from
  `import.meta.dir/migrations`; inside a compiled binary that directory is the virtual
  `/$bunfs/root`, which holds only what the build put there. Without `--asset` the binary ships zero
  migrations, computes a shipped schema version of 0, and refuses every existing index with
  `schema_too_new`. The working build command is:

  ```sh
  bun build --compile bin/sangkar.ts \
    --asset "$PWD/src/shared/store/migrations" \
    --outfile dist/sangkar
  ```

  `bun run build` passes exactly this flag, so `dist/sangkar` can read and serve an index.
  `dist/sangkar mcp --repo-root /path/to/repo` is the same command line the Bun install answers,
  with the binary in place of `sangkar`, and `dist/sangkar status --repo-root /path/to/repo`
  answers from it too. `tests/binary-smoke.test.ts` builds and drives that artifact, which is the
  evidence the flag is required rather than merely tidy.
- **Grammar WASM is not embedded.** Grammars load lazily from `web-tree-sitter` and
  `tree-sitter-wasms` in `node_modules` at index time, never at startup, so a compiled binary serves
  an index with no `node_modules` beside it but cannot build one. Until the grammars are compiled in,
  indexing stays with the Bun install and the binary serves the graph it produced.

## Publishing to npm

```sh
bun run release:beta          # npm publish --tag beta
```

Two traps, both hit on 2026-11-18 and both proven with `npm publish --dry-run`:

- **`--tag beta` is not optional.** `publishConfig.tag` is *ignored* by npm 11 (it only suppresses
  the "you must specify a tag" guard for prereleases): without the flag on the command line the
  publish goes to `latest` anyway, which is the one tag a `0.2.0-beta.2` must never take.
- **The `bin` warning is cosmetic.** npm prints
  `bin[sangkar] script name bin/sangkar.ts was invalid and removed` when it rewrites the field to
  strip a leading `./`; the entry survives as `bin/sangkar.ts`. `npm pkg fix` shows the same result,
  so the canonical form is written in `package.json` and the warning no longer appears.

Publishing then needs two-factor authentication: either `--otp <code>` from an authenticator app,
or a granular access token with 2FA bypass in `~/.npmrc`. Without one the registry answers
`E403 ... Two-factor authentication or granular access token with bypass 2fa enabled is required`,
and nothing is published.

State on 2026-11-23: `beta` serves `0.4.0-beta.1`, published that day. `v0.3.0-beta.1` was tagged
without the #13 fix and stays as published — a released number is never moved onto different code — so
the fix shipped as the next iteration, and `v0.3.0-beta.2` is the first tag carrying both payload fixes.

## Checks

```sh
export PATH="$HOME/.bun/bin:$PATH"   # only needed in a stripped shell; `bun` is on PATH here
bun run typecheck && bun run lint && bun run test && bun run db:drift
bun test tests/binary-smoke.test.ts  # builds the standalone artifact and drives one tools/call
bun run acceptance                   # budgets, latency, the prepared-statement ratio
```

`bun test` uses fixtures and temp directories only: no network, no writes into the repository. The
invariants each file guards are mapped in `.agents/prompts/test-map.md`.
