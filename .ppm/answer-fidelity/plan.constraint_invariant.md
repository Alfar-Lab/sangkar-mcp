## Constraints

- **Module isolation is a build failure, not a preference.** `tests/boundary.test.ts`: `src/tools/<tool>/**` may import only `#shared/*` and same-tool siblings; `src/indexer/**` only `#shared/*` and indexer siblings; `src/shared/**` never the indexer or a tool; `drizzle-orm` only under `src/shared/store/**`; only `src/index.ts` may import the indexer or a tool handler.
- **Nothing prints to stdout except the MCP protocol.** Every diagnostic goes through `src/shared/log.ts` to stderr. `console.log` is lint-banned.
- **Handlers never touch a connection and never build a query in a loop.** Reads go through `ToolContext.reader`; statements are prepared through the cache in `store/db.ts` (LRU cap 200). A new reader method that reuses `prepared()` is fine; a per-call query builder is not.
- **`total_relation: "eq"` stays a claim.** `emit()` is the only truncation point and downgrades it; nothing in this plan may bypass that. Any new list must be emitted through `emit()` with a budget.
- **Payload keys are a public contract.** A new or changed key requires the matching `docs/TOOLS-SPEC.md` edit in the same commit, and an assertion in that tool's own test.
- **No schema change.** `meta` is key/value; `unresolved` already stores `evidence_kind` and `evidence`. No new column, no `db:generate`, no migration file. `bun run db:drift` must stay green and its output must not move.
- **Never weaken a test to make it pass.** If a test encodes a contract this plan changes, change the test and say which invariant moved in the commit body.
- **Preserve unrelated work.** The tree may carry an untracked `.ppm/sangkar-telemetry/`; do not delete, commit, or reformat it.
- **Add no dependency.** The builtin allowlist, the bucket classifier, and the git ref read all use the standard library and existing modules.
- **Formatting is biome's job.** Do not hand-format; `bun run lint` must stay clean.
- **Dependency pins stay**: `@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4, `web-tree-sitter` 0.27.2, `tree-sitter-wasms` 0.1.13.

## Verification environment

- **`bun` is not on `PATH`.** Every verify command uses `~/.bun/bin/bun`. Do not assume a bare `bun`.
- **No service, no container, no shared database.** The store is a local SQLite file at `<repoRoot>/.sangkar/graph.db`, git-ignored. Unit tests build their own fixtures in temp dirs; that is the isolated environment for every fan-out task.
- **Runtime CLI probes need a current index.** The checked-in-store state is `g-d975f0`, which is stale (base `a4ccf52` ≠ HEAD). A task whose acceptance criteria name a live CLI output must first run `~/.bun/bin/bun run bin/sangkar.ts index` in this repository. That writes only to `.sangkar/`, which is ignored.
- **Never run two indexes at once.** Tasks in the same wave may share a repository, and a second concurrent `index` contends for `write.lock`. Before indexing, check `status.health.indexer`; if a run is in flight, wait for the watcher to publish and verify the payload afterwards instead of starting another run.
- **Executor must not**: modify or delete `.sangkar/graph.db` by hand, write `.ppm/*/tasks/*.json` directly, edit `migrations/**`, force-push, or touch the untracked telemetry directory.
- **Phase-scoped checks**: `~/.bun/bin/bun run typecheck` and the task's own test file. Whole-suite and `db:drift` belong to the closing task; `~/.bun/bin/bun run acceptance` runs once at close, because it asserts latency and the prepared-statement ratio, which this plan must not move.
