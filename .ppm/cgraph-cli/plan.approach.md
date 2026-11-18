# Approach

## Current state (verified at 4a42eb0)

- `app/bin/codegraph.ts` accepts only `MODES = ["mcp","index","status","worker"]` and exits 2 on
  anything else; it lazily imports `../src/index` and calls `main(requested, argv)`.
- `app/src/shared/config.ts` is the only environment reader for the roots: `repoRoot =
  resolve(env.CODEGRAPH_REPO_ROOT ?? cwd ?? process.cwd())`, `dataDir = env.CODEGRAPH_DATA_DIR ??
  join(repoRoot, ".codegraph")`, plus `CODEGRAPH_PROFILE` validated against `PROFILES`, plus
  `CODEGRAPH_CURSOR_KEY`. `AppConfig` carries `profile: Profile`.
- `app/src/index.ts` holds the registry (`TOOLS`, seven entries), the manifest-literal validator
  (`InputField`, `InputLiteral`, `parseArgs`, `matchesType`, `expectationOf`, lines ~99-320),
  `registerableTools(profile)`, `openStore(config)` (ready/absent/refused + `ToolContext`),
  `callTool`, `answer`, `runStatus` (four human lines) and `workerOptions` (`--mode`, `--force`).
- `--data-dir` is advertised in three error hints (`src/index.ts` guardRoot, `shared/store/db.ts`,
  `shared/store/migrate.ts`) but is parsed nowhere — a pre-existing gap this plan closes.
- Children are rooted through the environment, not argv: `src/indexer/watch.ts` `spawnCliWorker`
  writes `CODEGRAPH_REPO_ROOT`/`CODEGRAPH_DATA_DIR`, and `src/tools/index/handler.ts`
  `workerInvocation` does the same.
- `src/shared/instructions.ts` publishes `AGENT_TOOL_NAMES` (6), `ADMIN_TOOL_NAMES` (`index`),
  `TOOL_NAMES` (7), `NEXT_TOOLS`, `INSTRUCTIONS_TEXT`; `app/docs/INSTRUCTIONS.md` is a byte-identical
  copy that `tests/instructions.test.ts` compares against the constant.
- `tests/**`: 8 files call `loadConfig({env})` with `CODEGRAPH_REPO_ROOT`; `tests/acceptance.test.ts`,
  `tests/mcp-surface.test.ts`, `tests/tools-smoke.test.ts`, `tests/binary-smoke.test.ts` spawn the real
  entry with all five variables; `tests/tools-{expand,trace,index}.test.ts` assert `manifest.profile`.
- `docs/my-product/DATABASE.md:153` states the resolution order `root/.codegraph → --data-dir →
  CODEGRAPH_DATA_DIR`; `docs/my-product/{TECH-SPEC,DESIGN,TOOLS-SPEC}.md` describe tool profiles as a
  hard allow-list. (`docs/` was reorganised into `docs/my-product/` and `docs/research/` at
  2026-11-18; only `docs/my-product/` is normative.)
- Baseline: `$HOME/.bun/bin/bun test tests/boundary.test.ts` → 11 pass, 0 fail. Bun is installed at
  `$HOME/.bun/bin/bun` and is **not** on `PATH`.

## Design

1. **One validator, two callers.** `parseArgs` (behaviour unchanged) moves from the root into
   `#shared/cli/args.ts`; `parseCliArgs` builds the raw argument object from argv and then hands it to
   `parseArgs`, wrapping its `AppError` as `CliUsageError`. CLI and MCP cannot drift because they run
   the same check against the same manifest literal.
2. **argv is derived from the manifest, not written per tool.** Positional binds the single
   `required: true` field; every other field is `--name value`, with `--name=value`, repeated flags for
   arrays, `--flag` / `--no-flag` for booleans. `--help` prints `usageOf` generated from `input`.
3. **Repo root autodetect walks up from cwd**: nearest ancestor with `.codegraph/`, else nearest with
   `.git`, else cwd. Rationale: it fixes today's footgun where running from `app/src/` would index a
   subtree, and it keeps `.codegraph` beside the code as the project identity.
4. **Data dir is `<repo>/.codegraph` unless `--data-dir`.** The flag survives because read-only mounts
   and CI need it; the environment variable does not, because it is the thing that let a shell serve
   the wrong tree. A relative `--data-dir` resolves against the repo root (existing rule).
5. **Seven tools always.** The profile allow-list becomes an instruction-level discouragement instead
   of a protocol-level filter: `index` is registered everywhere and the guide says to call it only when
   `status` reports a broken graph. Rationale: a bash command that hides one of its own tools is worse
   than an agent that can see a repair tool the docs tell it not to use.
6. **Human output only for `status`.** `cgraph status` keeps today's four lines; `cgraph status --json`
   and every other tool print the envelope as one compact JSON document, so `| jq` works and no second
   formatter has to be maintained.
7. **Children get flags, never environment.** `worker` mode parses `--repo-root`/`--data-dir` like every
   other mode, so the server and its worker cannot disagree about which tree they are on.
8. **Exit codes are part of the CLI contract**: 0 for `ok`/`ambiguous`/`not_found`, 1 for
   `error`/`bootstrapping`, 2 for usage or configuration refusal.

## Phases

### phase_0: shared CLI primitives

Deliverable: `app/src/shared/cli/args.ts` and `app/src/shared/cli/render.ts`, each with its own test
file, plus the `bin.cgraph` entry in `app/package.json` — new files only, nothing existing changes.

Barrier because: phase_1 moves the root onto these signatures. Their contract (positional rules, flag
syntax, `CliUsageError`, exit-code mapping) must exist and be tested before the root, the bin entry and
fourteen test files are rewritten against it.

### phase_1: env and profile removal, CLI wiring

Deliverable: the breaking change, integrated — `AppConfig` without `profile`, config driven by
`--repo-root`/`--data-dir` with upward autodetect, seven manifests without `profile`,
`src/index.ts` dispatching tools from argv, `bin/codegraph.ts` accepting a tool name, children spawned
with flags, and every affected test migrated. The phase ends with the full suite green.

Barrier because: this phase is one atomic compile unit (config, manifests, root and tests must agree on
the new surface); phase_2 documents and exercises the surface it produces.

### phase_2: documentation and end-to-end proof

Deliverable: `app/README.md` and the four `docs/my-product/` files describing the real surface, a new
`app/tests/cli.test.ts` proving `cgraph <tool>` end-to-end from a nested directory, and the final
verification.

Barrier because: it consumes the finished CLI; documenting a surface that is still moving would have to
be written twice.
