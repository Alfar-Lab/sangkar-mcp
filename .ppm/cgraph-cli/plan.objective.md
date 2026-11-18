# Objective

Make `cgraph` a first-class bash command: `cgraph <tool> [flags]` runs any of the seven tools in the
current working tree, resolving the repository by walking up from the current directory instead of a
`CODEGRAPH_REPO_ROOT` environment variable. The three settings the CLI now decides itself
(`CODEGRAPH_REPO_ROOT`, `CODEGRAPH_DATA_DIR`, `CODEGRAPH_PROFILE`) are removed, and with the profile
concept gone the seven tools are always registered — one surface for MCP and for bash.

Why: the environment variables are the only thing standing between the current binary and
`cgraph trace foo` working from any subdirectory. They also duplicate a decision the process can make
from its own working directory, and they are the reason a mis-pointed shell can serve another project's
graph.

## Scope

In scope:

- `app/bin/codegraph.ts`: `cgraph <tool|mcp|worker> [args]`, help and usage text.
- `app/src/shared/cli/`: new `args.ts` (argv → arguments, usage generator) and `render.ts`
  (envelope → stdout, exit codes).
- `app/src/shared/config.ts`: `--repo-root` / `--data-dir` flags, upward autodetect, env removal,
  `Profile` deletion.
- `app/src/index.ts`: CLI tool dispatch, seven-tool registration, child flags instead of env.
- `app/src/indexer/watch.ts`, `app/src/tools/index/handler.ts`: worker spawned with flags.
- Seven tool manifests: `profile` field removed.
- `app/src/shared/instructions.ts` and its `app/docs/INSTRUCTIONS.md` copy: seven tools, one surface.
- `app/tests/**`: migrate fixtures off the removed variables; new CLI unit and end-to-end tests.
- `app/package.json`: `bin.cgraph` so `bun link` puts `cgraph` on `PATH`.
- `app/README.md`, `docs/my-product/{DATABASE,TECH-SPEC,DESIGN,TOOLS-SPEC}.md`: match the new surface.

Out of scope: any schema, migration or indexer-algorithm change; new dependencies; pagination-key
policy; `cgraph instructions` mode; global `bun link` execution (a user step, documented not run).

## Sources

- [../../app/README.md](../../app/README.md) — current install, env table, tool table.
- [../../docs/my-product/TECH-SPEC.md](../../docs/my-product/TECH-SPEC.md) — CLI entry, layout, composition-root rules.
- [../../docs/my-product/DATABASE.md](../../docs/my-product/DATABASE.md) — data-dir resolution order.
- [../../docs/my-product/DESIGN.md](../../docs/my-product/DESIGN.md) — tool profiles (§4) being removed.
- [../../docs/my-product/TOOLS-SPEC.md](../../docs/my-product/TOOLS-SPEC.md) — `index` as the repair tool (§8).

Paths under `docs/` were reorganised into `docs/my-product/` and `docs/research/` while this plan was
written. Only `docs/my-product/` is normative and therefore in scope; `docs/research/**` records
measurements and is not edited.

## Done when

1. `cd app && $HOME/.bun/bin/bun run typecheck` and `... run lint` pass.
2. `cd app && $HOME/.bun/bin/bun test` passes, including `tests/cli.test.ts` and
   `tests/binary-smoke.test.ts`.
3. From an indexed tree, `cgraph status`, `cgraph search <symbol>`, `cgraph trace <symbol> --depth 1`
   and `cgraph impact` answer without any `CODEGRAPH_*` variable set, and from a nested subdirectory
   they answer about the same database.
4. `cgraph nonsense` and `cgraph trace` (no positional) exit 2 with usage on stderr; `cgraph --help`
   lists the seven tools and the two modes.
5. `grep -rn "CODEGRAPH_REPO_ROOT\|CODEGRAPH_DATA_DIR\|CODEGRAPH_PROFILE" app/src app/bin app/tests
   docs/my-product app/README.md` returns nothing, and `grep -rn "profile" app/src` returns nothing.
6. `cgraph mcp --repo-root /path --data-dir /path` serves the same graph the old env contract served.
