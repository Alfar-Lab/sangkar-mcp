# Rules

The hard constraints. Entry point: [`AGENTS.md`](../../AGENTS.md).

## Non-negotiables

These six module rules are enforced by `tests/boundary.test.ts` and are a build failure, not a preference.
The rule ids below are the exact strings in a violation message.

| Rule | Meaning |
|---|---|
| `tool-isolation` | `src/tools/<tool>/**` may import only `#shared/*` and `./` siblings inside the same tool. **Tools never import each other.** |
| `indexer-isolation` | `src/indexer/**` may import only `#shared/*` and siblings inside `src/indexer`. |
| `shared-isolation` | `src/shared/**` may never import `src/tools/**` or `src/indexer/**`. |
| `drizzle-location` | `drizzle-orm` may be imported only under `src/shared/store/**`. |
| `root-privileges` | `src/index.ts` is the only module allowed to import the indexer or a tool handler. |
| `daemon-privileges` | `src/daemon/** may import only #shared/*, src/indexer/** and ./ siblings inside src/daemon, never src/tools/**` |

Other invariants that break the product when violated:

- **stdout is the MCP protocol.** Every log line goes to stderr through `src/shared/log.ts`. `console.log` is
  banned by biome's `suspicious/noConsole` rule (only `error` and `warn` are allowed) and the protocol is
  asserted end-to-end by `tests/mcp-surface.test.ts`.
- **Tools never touch a connection.** A handler receives `ToolContext` from `#shared/store/read/context.ts`
  (`reader`, `cursor`, `log`, `now`, `config`). It does not open a database, does not import `drizzle-orm`,
  does not read `process.env`: the settings that do come from the environment are read once, in
  `src/shared/config.ts` (`SANGKAR_CURSOR_KEY`, `SANGKAR_TELEMETRY`, and the machine-level
  `SANGKAR_HOME`). `src/index.ts` reads no environment variable and starts no watcher — the old
  no-watch variable is gone — because the daemon owns every poller, one per registered repository.
- **Never build a Drizzle query inside a handler or a loop.** Prepared statements only, via the cache in
  `store/db.ts` (LRU, cap 200). Measured on `bun:sqlite`: a re-built builder costs 42.7 µs per read, a reused
  prepared query 1.7 µs. `tests/acceptance.test.ts` fails if the prepared path is not ≥5× faster than the
  per-call builder.
- **Never publish a partial graph.** Write to a stage, `seal()` (checkpoint, `journal_mode=DELETE`,
  `synchronous=FULL`, `quick_check`), then `rename()` over the live file. `withStage()` guarantees the
  discard path. Readers keep serving the previous generation.
- **`total_relation: "eq"` is a claim.** It is legal only when coverage is complete, no `unresolved` rows
  exist in scope, the generation is fresh, and the payload was not truncated. `emit()` downgrades `eq` to
  `gte` itself when it truncates — do not "fix" that.
- **No URI filenames.** `new Database("file:…?immutable=1")` fails in `bun:sqlite`; readers use
  `{ readonly: true }` only.
- **Eleven tables, one ledger.** `src/shared/store/schema.ts` (Drizzle) is the typed source of truth;
  `migrations/0000_init.sql` is generated from it; FTS5 lives in `migrations/raw/0001_fts.sql`. One runner
  (`migrate.ts`), forward-only, never `drizzle-orm/bun-sqlite/migrator` in a production path.
- Keep the dependency pins: `@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4, `web-tree-sitter`
  0.27.2, `tree-sitter-wasms` 0.1.13. Run on Bun (1.4.4+); Node cannot run this (`bun:sqlite`, `Bun.spawn`,
  `bun build`).

## Keeping this guidance true

The files this file points at are **part of the change, never a follow-up**. A commit that leaves one of them
wrong is incomplete in the same way as one that leaves `docs/TOOLS-SPEC.md` stale — so synchronize the
file in this directory a change touches, in the same commit:

| When you change… | Update, in the same commit |
|---|---|
| a file, directory, entry point, module boundary, or one of the recipes | [`project-layout.md`](project-layout.md) — its tree block, that recipe, and the data model if a table or column moved |
| a plan's or task's status, a measured graph number, or one of the three known gaps | [`state-gaps.md`](state-gaps.md) — the numbers you just measured, never the ones already written |
| add, rename, delete or re-scope a test file | [`test-map.md`](test-map.md) — the "if you change… → these will tell you" row |

`state-gaps.md` carries counts taken from a real index of this tree, so an edit there needs the run that
produced them: `bun run bin/sangkar.ts index --data-dir /tmp/…`, then the query the section cites. Re-measure,
never estimate. If a change invalidates guidance you cannot restate honestly, say so in the commit body instead
of leaving a confident stale line.

## Conventions

- Conventional commits, scoped, with `!` for behaviour/repo-structure changes (`refactor!: move the app tree
  to the repository root`).
- `snake_case` in SQL, `camelCase` in TypeScript, mapped explicitly in `store/schema.ts`.
- Comments explain **why** (a constraint, a measured number, a failure seen in production), never what the
  next line does. The existing code is a good model: see `config.ts` on repo detection, `bunfig.toml` on test
  discovery, or the direction note on `tested_by` in `persist.ts`.
- Biome for formatting; do not hand-format.
- Never weaken an assertion to make a test pass. If a test encodes a contract and the contract changed, change
  both and say so in the commit body.
