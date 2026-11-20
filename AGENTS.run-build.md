# AGENTS.run-build.md

How to run and check the code on this machine. Entry point: [`AGENTS.md`](AGENTS.md).

## Run it

`bun` is **not on `PATH`** on this machine; the binary is `~/.bun/bin/bun`. `bun link` puts `sangkar` on
PATH, otherwise invoke `bun run bin/sangkar.ts`.

```sh
bun install
bun run bin/sangkar.ts index          # first index; until then every tool answers bootstrapping
bun run bin/sangkar.ts status         # five human lines; add --json for the envelope
bun run bin/sangkar.ts trace 'handleRequest' --depth 1
bun run mcp                           # MCP server on stdio
```

A script that shells out to `bun` (`test`, `acceptance`, `db:parity`) fails with
`bun: command not found` unless the binary is on `PATH`, because the script body is a `bun …`
command and not a re-entry through the same binary:
`PATH="$HOME/.bun/bin:$PATH" ~/.bun/bin/bun run test`.

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
