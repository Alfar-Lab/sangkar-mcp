# Risks and open questions

## Risks

| Risk | Mitigation |
|---|---|
| **Parser is the long pole.** `web-tree-sitter` + `tree-sitter-wasms` may not cover 12 languages well enough, or may be too slow for the p95 target. | Phase 1 leads with the language tables and the parser pool; **TASK-019** blocks the phase on the acceptance criteria (totals and by-kind counts against the recorded oracles, per-language file counts, at most 1 % of files with an ERROR node, p95 ≤ 25 ms/file, peak RSS ≤ 1 GB). Fallback ladder in TECH-SPEC §9: `@vscode/tree-sitter-wasm`, then a 6-language MVP. |
| **`shared` becomes a god module** because cross-tool reuse has nowhere else to go. | Promotion rule (TECH-SPEC §4.3): used by ≥2 modules, no tool policy, no tool-specific argument names, test imports no tool. `shared` may decide what is true, never what to show. |
| **Absence envelope turns out to be unimplementable as designed** (e.g. `unresolved` cannot be scoped to a query cheaply). | TASK-010 implements `coverage.assemble` against fixtures first; phase 1 produces real `unresolved` rows; phase 3 TASK-033 turns the invariant into an acceptance test. If scoping proves expensive, add an index on `unresolved(file_id)` before adding a cache. |
| **Payload budgets drift** as tools add fields — the failure CRG had 3 times. | One chokepoint (`emit`), a property test (TASK-011), and the acceptance test asserting every payload ≤ budget + 10 %. No per-tool ceilings anywhere. |
| **Drizzle overhead creeps back in** through a query built inside a handler. | Boundary test bans `drizzle-orm` outside `src/shared/store/**`; the prepared-statement cache is the only access path; TASK-033's acceptance run re-measures builder-vs-prepared (spike S6) and fails the gate if the prepared path is not at least 5× faster, so the rule stays load-bearing. |
| **Indexer memory blow-up** on a large repo (WASM trees retained per file). | Bounded LRU of parsers keyed by language, one file in flight per worker, per-file result released before the next; peak RSS ≤ 1 GB is asserted in **TASK-019** (criterion f). |
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
| Semantic search in the MVP | **No** — BM25 only; `node_vector` is created by TASK-005 and stays empty, and `search` reports `mode: "bm25"` | user, post-MVP |
| `.codegraph/languages.toml` (user-defined languages) | **No** — explicitly out of MVP; the language layer is data, so adding the TOML loader later is one new module plus a registry merge, not a refactor | user, post-MVP |

## Known debt accepted at planning time (non-blocking audit findings)

- TASK-012's node-type lists are validated by a **runtime probe** of each loaded grammar, not by a
  vendored `node-types.json` (that file ships with the WASM bundle, not beside it).
- The prefix-range rule lives in `#shared/paths.prefixBounds` and must reject `src-old` for prefix `src`;
  the naive `path >= p AND path < p||'0'` form is explicitly forbidden in TASK-002 and TASK-009.
- `TASK-018` degrades to hash-only change detection when the repository is not a git checkout, because the
  worker's integration fixture is a plain temp directory.
- `TASK-032`'s binary smoke performs a real `tools/call status` against an indexed fixture rather than an
  `initialize` ping, which would pass even if the runtime could not resolve grammar WASM files.
- Distribution order follows `TECH-SPEC.md` §15: npm package first, compiled binary once the grammar set is
  fixed.
- Packet cross-references ("cursor signing is the cursor task") were added by the audit fixes and violate the
  self-containment criterion in spirit. They are pointers to owners, not required context, so packets remain
  executable; if tasks are ever renumbered, re-check them.
- TASK-001 runs `bun install` in its verify, which is the single explicit exemption from the
  "no network in verify" rule: it populates `node_modules` once, including the offline grammar set every
  later task needs.
