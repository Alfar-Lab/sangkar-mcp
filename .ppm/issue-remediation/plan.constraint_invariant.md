# Constraints and invariants

One bullet per rule every task must respect. A worker that cannot honour one of these stops and
reports instead of editing.

## Verification environment

- `bun` is **not** on `PATH` on this machine. Every command is `~/.bun/bin/bun run <script>` executed
  from the repository root `/home/ljosalfar-server/Documents/researchs/codegraph-mcp`.
- Baseline at `58a2103`: `typecheck`, `lint`, `test`, `db:drift` and `acceptance` are green. A task
  that finds a red baseline records it as pre-existing in `progress` and does not "fix" it.
- Scoped check for a task that changes one module: `~/.bun/bin/bun test tests/<file>.test.ts`
  (bunfig.toml drives discovery; `bun test <file>` is the whole-file form). The phase gate runs the
  full `typecheck`, `lint`, `test` and `db:drift`; `acceptance` runs in both phase gates.
- The reference corpus for the whole-suite integration tests is `code-review-graph/` at the
  repository root; two full indexes of it take ~20 s, so those tests are not per-task checks.
- No database may be migrated and no migration may be added: this plan changes no table and no
  column. `db:drift` (which runs `drizzle-kit generate --name drift-check`) must leave
  `src/shared/store/migrations` byte-identical. If it writes a file, delete that file and report it
  as a defect, not a change.
- Fixtures are created under `mkdtemp` and removed; a test never writes inside this repository.

## Data and state

- **`.sangkar/graph.db` is not touched by any task.** Every manual probe uses a scratch data dir
  (`--data-dir /tmp/<something>`); re-indexing the repository's own store is the orchestrator's
  decision, not a task's.
- No task runs a write command against this repository's git state (`add`, `commit`, `checkout`,
  `stash`). Tests create their own temporary repositories.
- No `--force` push, no tag, no `npm publish`.

## Module rules (enforced by `tests/boundary.test.ts`; a violation is a build failure)

- `tool-isolation`: `src/tools/<tool>/**` imports only `#shared/*` and its own siblings.
- `indexer-isolation`: `src/indexer/**` imports only `#shared/*` and its own siblings.
- `shared-isolation`: `src/shared/**` never imports `src/tools/**` or `src/indexer/**`.
- `drizzle-location`: `drizzle-orm` only under `src/shared/store/**`.
- `root-privileges`: `src/index.ts` is the only module that imports the indexer or a tool handler.

## Store and protocol invariants

- stdout is the MCP protocol. Every log line goes to stderr through `src/shared/log.ts`;
  `console.log` is banned by biome.
- A tool handler receives `ToolContext` and never opens a connection, never imports `drizzle-orm`,
  never reads `process.env`. `SANGKAR_CURSOR_KEY` and `SANGKAR_TELEMETRY` stay in
  `src/shared/config.ts`; `SANGKAR_NO_WATCH` stays in `src/index.ts`.
- No Drizzle query is built in a handler or a loop: prepared statements through the `store/db.ts` LRU
  (`cap 200`), keyed by a stable `sqlKey`. A new query in `read/files.ts` follows the existing
  `prepared(conn, "files.<name>", …)` shape.
- Never publish a partial graph; the stage → `seal()` → `rename()` protocol and `withStage()` are not
  modified by this plan.
- `emit()` remains the single truncation point and the only minter of `next`; `total_relation: "eq"`
  legality is unchanged (coverage complete, no `unresolved` in scope, fresh, not truncated).
- The nine envelope fields (`status`, `summary`, `read_status`, `coverage`, `totals`, `truncated`,
  `truncation_reason`, `next`, `next_tools`) stay intact. `decisive` goes **inside** `read_status`,
  never as a tenth top-level field.
- Cursors are generation-bound: a cursor minted under one generation must be refused after a newer
  generation is served, with the existing error code — re-opening the store must not make an old
  cursor valid again.

## Contracts and documentation

- Payload keys are a public contract. Any added field is documented in
  `docs/my-product/TOOLS-SPEC.md` in the same phase, and the tool's own test asserts it.
- `docs/INSTRUCTIONS.md` is pinned byte for byte to `INSTRUCTIONS_TEXT` by
  `tests/instructions.test.ts`. Neither is edited by this plan.
- Never weaken an assertion to make a test pass. Where a contract changed (`ChangeState` gains
  `untracked`, `read_status` gains `decisive`, schemas gain `description`/`examples`), change the
  assertion and the contract together and name the invariant in the commit body.
- Conventional commits, scoped, `!` for payload/behaviour changes. The orchestrator commits once per
  phase, after the phase gate is green.
- No new dependency. The porcelain parser and the path helper are hand-written: the pins in
  `package.json` (`@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4, `web-tree-sitter` 0.27.2,
  `tree-sitter-wasms` 0.1.13) do not move.

## Ownership

- A task modifies only the paths in its `files`. An overlap between two concurrently eligible tasks
  is a plan defect: report it rather than editing the other task's file.
- Hotspot files (`src/index.ts`, `src/shared/envelope.ts`, `docs/my-product/TOOLS-SPEC.md`) are owned
  by exactly one task per phase.
