# MCP tool surface: `codebase-memory-mcp` vs `code-review-graph`

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python.
> [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — CBM `dev`, C/Go (164 grammars).

Verified counts: **CBM 17 tools** (the same 17 appear in its schemas, annotations and dispatch); **CRG 30 tools + 5 prompts**
(30 registered `@mcp.tool` handlers and 5 registered `@mcp.prompt` functions in the Python server).

---

## 1. The 17 CBM tools

| Tool | Job | Required | Key args (default) | Limit enforced |
|---|---|---|---|---|
| `index_repository` | build/refresh | `repo_path` | `mode`=full, `name`, `persistence`=false, `async`=false, `status`=false, `doc_candidates`, `index_worktree`=false, `target_projects` | discovery caps, worker quiet 900 s, memory budget |
| `search_graph` | symbol search (BM25 / regex / semantic) | `project` | `limit`=50 (≤500), `offset`, `semantic_query`, `max_output_tokens`=3200, `format`=tree, `fields[]`, `detail`=ids\|default | rows 500, bytes = 4×tokens |
| `query_graph` | read-only Cypher | `query`,`project` | `max_rows`=200 (≤99998), `graph`=code\|missed, `cursor`, `max_output_tokens` | 100 000 rows, 30 s |
| `trace_path` | callers/callees, data flow, topology | `function_name`,`project` | `direction`=both, `depth`=3 (≤15), `limit`=100 (≤5000), `mode`=calls, `include_overrides`, `parameter_name`, `edge_types[]` | depth clamp, budget 3200 |
| `get_code_snippet` | one symbol's source/outline | `qualified_name`,`project` | `include_neighbors`, `source_mode`=auto, `member_limit`=50 (≤500), `max_lines` (≤500) | 500 lines, 200-line outline |
| `get_file_outline` | declaration outline of a file | `project`,`file_path` | `labels[]` (≤16), `limit`=100 (≤200) | 2 MiB output |
| `get_graph_schema` | label/edge counts (+properties) | `project` | `diagnostics`=none\|full, `limit`=50 | — |
| `compare_graphs` | deterministic 2-snapshot diff | `base_project`,`target_project` | `limit`=200 (≤1000), `scan_limit`=2e6 | 512 KiB per set |
| `get_architecture` | bounded static architecture | `project` | `path`, `aspects[]`, `entry_node_id`, `include_behavior_evidence` | **no token budget** |
| `search_code` | graph-ranked text/grep | `pattern`,`project` | `mode`=compact, `context` (≤200), `regex`, `result_limit`=10, `raw_limit`=5 | 30 s scan, 64 MiB scan output |
| `list_projects` | enumerate projects | — | `detail`=identity, `limit`=50 | — |
| `delete_project` | **destructive** delete + ADR sidecar | `project` | — | refused while indexing |
| `index_status` | readiness/freshness/coverage | `project` | `verbose`, `diagnostics`=none\|summary\|full | 500 coverage samples |
| `check_index_coverage` | exact path/scope coverage | `project`* | `paths[]` (≤128), `scopes[]` (≤32), `path_limit`, `scope_limit` | 128 / 1000 |
| `detect_changes` | git diff → files/impact/tests | `project` | `scope`=impact, `direction`=inbound, `depth`=2 (≤15), `seed_limit`, `limit`=200, `base_branch`, `since`, `cursor` | `DETECT_MODCAP` 256, budget 3200 |
| `manage_adr` | **writes** ADR sidecar CRUD | `project` | `mode`=outline\|get\|update\|set_sections | `section_limit` 500 |
| `ingest_traces` | **writes** observed call paths | `traces[]`,`project` | `label`="unlabeled", per-trace `path[]` (2-256), `count` | 10 000 traces, 200 000 pairs |

Aliases and non-tool surfaces: `trace_call_path` is dispatchable but unadvertised.
Two **prompts** exist (`explore_codebase`, `review_change_impact`) — CBM's documentation does not
mention them. Tool-profile gating (`--tool-profile scout|analysis`) narrows the list to 8 or 13 tools.

## 2. The 30 CRG tools

### Build / update (3, all unbounded by timeout)
`build_or_update_graph_tool` (`full_rebuild`=false, `base`, `postprocess`=full) ·
`run_postprocess_tool` (`flows`,`communities`,`fts`) · `embed_graph_tool` (`model`, `provider`)

### Review & impact (5)
| Tool | What it adds | Caps |
|---|---|---|
| `get_minimal_context_tool` | ~100-token entry: stats + risk + top communities/flows + next-tool hints | budget 800 tokens |
| `detect_changes_tool` | risk-scored, priority-ordered review (`base`=HEAD~1, `max_depth`=2, `max_results`=25) | 100 funcs, 200 flows, 200 files, 600 source lines |
| `get_review_context_tool` | impact + source snippets + guidance (`detail_level`) | 100 nodes, 150 edges, 200 files, 800 source lines |
| `get_impact_radius_tool` | blast radius, `detail_level` incl. **`compact`** | 100 nodes, 150 edges, 200 files |
| `get_affected_flows_tool` | flows the change touches | 25 standard / 500 minimal, 400 flow steps |

### Query / search / traverse (3)
`query_graph_tool` (20 **fixed patterns** — `callers_of`, `tests_for`, `path_to`, …; `max_results`=100,
**no hard ceiling**) · `semantic_search_nodes_tool` (`limit`=20, `include_source`,
`expand_neighbors`) · `traverse_graph_tool` (`mode`=bfs, `depth` 1-6, `token_budget`=2000 ≤10 000)

### Analysis (8)
`get_hub_nodes_tool` · `get_bridge_nodes_tool` (betweenness) · `get_knowledge_gaps_tool`
(isolated nodes, thin communities, untested hotspots, import cycles) · `get_surprising_connections_tool`
· `get_suggested_questions_tool` · `find_large_functions_tool` · `refactor_tool`
(mode=rename|dead_code|suggest) · `apply_refactor_tool` (**rewrites source files**)

### Structure (6)
`list_flows_tool` · `get_flow_tool` · `list_communities_tool` · `get_community_tool` ·
`get_architecture_overview_tool` (defaults to `detail_level="minimal"`) · `list_graph_stats_tool`

### Docs (3) · Multi-repo (2)
`get_docs_section_tool`, `generate_wiki_tool` (writes `.code-review-graph/wiki/`), `get_wiki_page_tool`,
`list_repos_tool`, `cross_repo_search_tool`

## 3. The 5 CRG prompts

All prepend `_TOKEN_EFFICIENCY_PREAMBLE`: call `get_minimal_context_tool` first,
`detail_level="minimal"` unless insufficient, **≤3 calls per turn**.

| Prompt | Args | Workflow it dictates |
|---|---|---|
| `review_changes` | `base`=HEAD~1 | minimal_context → detect_changes (level by risk) → callers_of for high-risk fns → affected_flows only if >3 changed fns. Explicitly forbids `get_review_context_tool` |
| `architecture_map` | — | architecture_overview → list_flows → 1-2 `get_community` → Mermaid diagram |
| `debug_issue` | `description` | semantic_search(limit=5) → callers_of top 1-2 → get_flow → only then review_context |
| `onboard_developer` | — | stats → architecture_overview → list_communities → top-3 flows |
| `pre_merge_check` | `base`=HEAD~1 | detect_changes → flows if risk>0.4 → tests_for ≤3 fns → dead_code → large_functions if risk>0.7 → GO/NO-GO |

---

## 4. Bounds contract comparison

| | CBM | CRG |
|---|---|---|
| Truncation signal | `truncated` + `truncation_reason` + `total_relation: gte` + `next_cursor` | per-list `*_total` + `truncated`, plus `", showing N of M"` appended to `summary` |
| Token budget | `max_output_tokens` per tool, **4 bytes/token**; absent on `get_architecture`, `get_graph_schema`, `list_projects`, `index_status` | fixed per-tool budgets **pinned by tests** (a `BUDGETS` table, `ABSOLUTE_MAX_TOKENS = 50_000`) |
| Timeout | cypher 30 s; `search_code` scan 30 s; index worker quiet 900 s | `CRG_TOOL_TIMEOUT`, **default 0 = unbounded**; worker thread with `abandon_on_cancel`; 5 tools explicitly `bounded=False` |
| Validation | JSON-schema `minimum`/`maximum` per arg | `_validate_positive_int` rejects `<1` and booleans (20 tools); `detail_level` **never validated** — unknown value silently behaves as `standard` |
| Two tools outside the contract | `get_architecture` has no budget at all | `get_docs_section_tool`, `list_repos_tool` are sync `def`s → never offloaded, never timeout-bounded |

CRG's budget table is the more rigorous artifact: `test_budget_table_covers_every_registered_tool`
fails CI if a new `@mcp.tool` has no budget entry, and `test_hard_ceilings_bind` proves the caps are
applied. CBM instead encodes limits in the JSON schema and enforces them at the handler.

## 5. Read-only annotations

| | CBM | CRG |
|---|---|---|
| read-only | 13 of 17 | 25 of 30 |
| destructive | `delete_project`, `manage_adr`, `ingest_traces`, `index_repository` | build, postprocess, embed, wiki, apply_refactor |
| notes | `manage_adr` is `readOnlyHint:false, destructiveHint:true` even for `mode="get"` | `refactor_tool` claims `readOnlyHint:true` while mutating an in-memory preview registry (no disk write, so defensible) |

---

## 6. Overlaps — which tool does the agent actually need?

| Need | CBM | CRG |
|---|---|---|
| "where is X" | `search_graph` (symbol) → `search_code` (raw text) | `semantic_search_nodes_tool` |
| "who calls X" | `trace_path(direction=inbound)` or `query_graph` Cypher | `query_graph_tool(pattern="callers_of")` |
| "what breaks" | `detect_changes` | `detect_changes_tool` **or** `get_review_context_tool` **or** `get_impact_radius_tool` — three near-identical answers |
| "how is this system shaped" | `get_architecture(aspects[])` | `get_architecture_overview_tool` + flows + communities |
| "is this file indexed" | `index_status` / `check_index_coverage` / `query_graph(graph="missed")` | no per-path coverage tool at all |

**CBM has almost no redundancy** — one tool per question, with `search_graph`/`search_code`/
`query_graph` separated by *input shape* (symbol vs text vs query language).

**CRG carries deliberate duplication**: `detect_changes_tool`'s own docstring says it *"Replaces
get_review_context for change-aware reviews"*, yet both remain, and `get_impact_radius_tool`
re-implements the same radius with its own duplicated copy of the ceilings. Its 8 analysis tools are
four centrality rankings plus a composite — `get_suggested_questions_tool` is literally built from the
other four.

## 7. Documented vs code mismatches

| Claim | Reality |
|---|---|
| CBM's runtime-trace docs: `ingest_traces` "does not open or mutate a store", returns `status:"accepted"` + an unimplemented note | The handler opens a read-write store and returns `status:"stored"`, `pairs_stored`, `paths_stored`, `evicted_runs` |
| CBM's documentation: `get_architecture` covers "ADR" | `VALID_ASPECTS` has **no** `adr` aspect |
| CBM `check_index_coverage` schema marks only `project` required | the handler errors without `paths`/`scopes` |
| CBM schemas leave `max_output_tokens` without a `default` on `query_graph`/`search_code` | the handlers default 3200 / 12000-or-3200 |
| CRG `get_review_context_tool` docstring: "Default: 50" | the signature and CRG's own CLI docs say **100** — docstring is stale |
| CRG `get_affected_flows_tool` default `max_flows=50` | silently cut to **25** in standard mode |
| CRG docstrings: `max_members` "capped at 100" | actual cap **25** |
| CRG's feature docs: "every read-only MCP tool is bounded by a timeout" | two sync tools never reach `_offload` |
| CRG `MAX_IMPACT_DEPTH` / `MAX_BFS_DEPTH` constants | not enforced as ceilings; `max_depth` is only clamped `>=0` |

---

## 8. The design difference in one paragraph

CBM's surface is a **database client**: 17 verbs over one store, each with a schema-validated
argument set, a byte budget, and a truncation contract — it assumes the agent will compose queries
(`query_graph` is a real Cypher engine), so the convenience tools stay narrow. CRG's surface is a
**review workflow**: 30 verbs plus 5 prompts that prescribe the call sequence, a pinned token budget per
tool, and a strong bias toward the single question "did this change break something" — which is why
three of its tools answer that question redundantly and why its prompts spend most of their text
telling the model *not* to call the expensive ones.
