# Run and build

How to run and check the code on this machine. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Run it

`bun` **is on `PATH`** on this machine: 1.4.4, reached as `bun` through `~/.local/bin/bun` →
`~/.bun/bin/bun`, and `.profile` / `.bashrc` add `$BUN_INSTALL/bin` as well. `sangkar` is linked into
`~/.local/bin` too. A checkout whose link you have not made runs `bun run bin/sangkar.ts`.

A stripped shell has none of that — `env -i`, a cron job, a CI image without the profile — and there the
export is part of the command: `PATH="$HOME/.bun/bin:$PATH"`.

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

## Definition of done

```sh
bun run typecheck && bun run lint && bun run test && bun run db:drift
```

Plus, when behaviour changed: the tool's own test, `docs/TOOLS-SPEC.md` if a payload moved, and one line in
the commit body naming the invariant you touched. `bun run acceptance` when latency, budgets or the prepared
statement path could have moved.
