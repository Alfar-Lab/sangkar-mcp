# Risks and open questions

## Risks

| Risk | Mitigation |
|---|---|
| **Parser is the long pole.** `web-tree-sitter` + `tree-sitter-wasms` may not cover 12 languages well enough, or may be too slow for the p95 target. | Phase 1 leads with the language tables and the parser pool; TASK-020 blocks the phase on the four acceptance criteria (parse without ERROR on the fixture, p95 ≤ 25 ms/file, ≥95 % of CRG's node counts, peak RSS ≤ 1 GB). Fallback ladder in TECH-SPEC §9: `@vscode/tree-sitter-wasm`, then a 6-language MVP. |
| **`shared` becomes a god module** because cross-tool reuse has nowhere else to go. | Promotion rule (TECH-SPEC §4.3): used by ≥2 modules, no tool policy, no tool-specific argument names, test imports no tool. `shared` may decide what is true, never what to show. |
| **Absence envelope turns out to be unimplementable as designed** (e.g. `unresolved` cannot be scoped to a query cheaply). | TASK-019 (phase 0) implements `coverage.assemble` against fixtures first; phase 1 produces real `unresolved` rows; phase 3 TASK-033 turns the invariant into an acceptance test. If scoping proves expensive, add an index on `unresolved(file_id)` before adding a cache. |
| **Payload budgets drift** as tools add fields — the failure CRG had 3 times. | One chokepoint (`emit`), a property test (TASK-011), and the acceptance test asserting every payload ≤ budget + 10 %. No per-tool ceilings anywhere. |
| **Drizzle overhead creeps back in** through a query built inside a handler. | Boundary test bans `drizzle-orm` outside `store/store/**`; the prepared-statement cache is the only access path; the phase 3 acceptance run re-measures and fails the gate if p95 regresses past `DESIGN.md` §14. |
| **Indexer memory blow-up** on a large repo (WASM trees retained per file). | Bounded LRU of parsers keyed by language, one file in flight per worker, per-file result released before the next; measure peak RSS in TASK-020. |
| **Scope creep into the excluded feature set** (Cypher, communities, wiki, refactor). | `plan.objective.md` §Scope "Out" is explicit; a task proposing one of them is out of plan and must be raised as an open question first. |
| **Windows/macOS differences** in the publish `rename()` and lock semantics. | MVP targets Linux only (the machine in use). Documented as out of scope; no task should add platform branches. |

## Open questions

| Question | Blocking? | Who answers |
|---|---|---|
| Product name and npm package name (currently `<name>` / `codegraph`) | **No** — every path in this plan is `app/…`, and `package.json#name` is set by TASK-001 to `codegraph-mvp` as a placeholder | user, before publishing |
| Which 12 languages exactly — the list in TECH-SPEC §9 (python, typescript, tsx, javascript, go, java, rust, c, cpp, csharp, php, ruby) | **No** — committed as the MVP set; TASK-012 implements it as data, so changing it later is a table edit | user, before phase 1 starts |
| MCP SDK vs hand-rolled stdio | **No** — TASK-001 installs `@modelcontextprotocol/sdk@1.32.2`; registration is isolated in `app/src/index.ts` so it can be swapped | implementer at TASK-001 |
| Zod vs literal JSON Schema for tool inputs | **No** — JSON Schema literal + a ~40-line validator in `shared`, because the eight argument shapes are simple and the model sees the schema either way | implementer at TASK-002 |
| `base` for `summary(change)`: last indexed commit vs live `git merge-base` | **No** — default to the generation's `base_commit`, overridable per call; no git call on the hot path (CRG's discovery timeouts came from the other choice) | implementer at TASK-023 |
| Semantic search in the MVP | **No** — BM25 only; `node_vector` stays an empty table and `search` reports `mode: "bm25"` | user, post-MVP |
