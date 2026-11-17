# Constraints and invariants

Rules every task must respect. Violating one is a defect, not a preference.

## Architecture (from TECH-SPEC.md §3–§4)

- Three layers only: `app/src/shared/**` (kernel + engine), `app/src/tools/<name>/**` (private, one per
  tool), `app/src/indexer/**` (writer). `app/src/index.ts` is the composition root and the only module that
  imports tools; it may import `#shared/*`, `./indexer/*` and `./tools/*/manifest`, and no other file may
  import a tool handler or the indexer. Both halves of that rule are enforced by `app/tests/boundary.test.ts`.
- Tools never see a connection. Every handler receives `ToolContext {reader, cursor, log, now, config}`
  from `#shared/store/read/context.ts`, where `reader: GraphReader` is the aggregate interface from
  `#shared/store/read/reader.ts`. A handler that needs meta, generation, census or FTS results asks the
  reader; it never opens a database or reads `process.env` (the only `process.env` access is in
  `#shared/config.ts`, which also supplies `cursorKey` to the cursor module).
- **A tool module may import `#shared/*` only** (subpaths such as `#shared/envelope`, `#shared/store/read/edges`).
  There is no `src/shared/index.ts` barrel, so the import surface is explicit and every file can be added by
  one task without editing a shared file. No cross-tool imports, ever. Enforced by the `#/imports` resolver
  map plus `app/tests/boundary.test.ts`.
- **`shared` must never import `tools/` or `indexer/`.**
- **`drizzle-orm` may be imported only under `app/src/shared/store/**`.** Drizzle row types (`$inferSelect`)
  stop at `store/read/**` and are mapped to domain types before returning.
- `shared` may decide *what is true*; a tool decides *what to show*. No `limit`/`maxResults`/`detail_level`
  knobs in `shared`; no ranking or formatting in `shared`.
- Tools receive a `ToolContext` (reader, cursor, log, now, config). A tool never opens a database, never
  reads `process.env`, never builds an envelope by hand, never size-checks a payload itself.

## Runtime (from TECH-SPEC.md §1, §8, §11)

- Bun 1.4.4, TypeScript `strict`. No Node-only APIs where a Bun built-in exists.
- **stdout is the MCP protocol.** Logs go to stderr as JSON lines. `console.log` is forbidden in `app/src/`.
- Readers use `new Database(path, { readonly: true })`. **URI filenames (`file:…?immutable=1`) do not
  work** in `bun:sqlite` — do not use them.
- Indexing runs in a spawned child process, never in the MCP server process.
- No `flock` (unavailable): the write lock is a create-exclusive (`wx`) file with a PID and an mtime
  heartbeat; a stale lock (dead PID or heartbeat > 60 s) is reclaimed and logged.

## Data (from DATABASE.md)

- One database per repo at `app/.codegraph/graph.db` when the repo is the app's own fixture; for a target
  repo it is `<repo>/.codegraph/graph.db`. `.codegraph/.gitignore` contains a single `*`; the target repo's
  tracked files are **never** modified.
- Publish protocol is fixed: write to a stage file, `seal` (checkpoint, `journal_mode=DELETE`,
  `synchronous=FULL`, `quick_check`), then `rename()` over the live file. A failure at any step discards the
  stage; **a partial graph is never published.**
- `edges` holds resolved edges only, id-keyed, `ON DELETE CASCADE`. Unbound call sites go to `unresolved`.
  Never write an edge whose `dst_id` has no node row.
- Migrations: `app/src/shared/store/schema.ts` (Drizzle) is the source of truth for modeled tables;
  `app/src/shared/store/migrations/*.sql` from `drizzle-kit generate`; FTS5 and other raw objects live in
  `app/src/shared/store/migrations/raw/*.sql`; **one runner, one ledger** (`schema_migration`), forward-only.
  Do not use `drizzle-orm/bun-sqlite/migrator` in production paths.
- Query performance: **never build a Drizzle query inside a loop, a handler, or a BFS hop.** Use
  `.prepare()` and the cache in `store/db.ts`. Measured: re-built builder 42.7 µs vs prepared 1.7 µs per read.

## Contracts that must not drift

- Every tool response carries all nine envelope fields (`status`, `summary`, `read_status`, `coverage`,
  `totals`, `truncated`, `truncation_reason`, `next`, `next_tools`) plus `payload`.
- `total_relation: "eq"` is permitted **only** when coverage is complete, no `unresolved` rows exist in the
  queried scope, the generation is fresh, and the payload is not truncated. Otherwise `"gte"`.
- Truncation always sets `next`; a silent cut is a defect.
- Cursors are stateless and signed; an expired or mismatched cursor returns `cursor_expired` /
  `cursor_mismatch`, never a wrong page.
- Tool names are bare verbs. No `_tool` suffix anywhere.

## Verification environment

- All `verify` steps run in `app/` against **fixtures only**: `app/tests/fixtures/` for unit tests and an
  ephemeral database in a temp directory (`mkdtemp`) for integration tests.
- The reference corpus is `code-review-graph/` at the repo root, read-only, used by integration and parity
  tests. Tests must never write into it.
- Node/npm are not used. `bun` is at `~/.bun/bin/bun` and is **not on `PATH`** — verify commands use
  `$HOME/.bun/bin/bun` or the `package.json` scripts run through it.
- No task may run a network call in `verify`. Grammar WASM files come from the `tree-sitter-wasms` package
  installed by TASK-001 into `app/node_modules`, so parse tests are offline by construction.
- No task may modify: `app/package.json` (owned by TASK-001), the migration ledger, or any file outside its
  declared `files`.
