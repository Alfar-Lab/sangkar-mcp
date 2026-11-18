# What the agent actually calls: usage telemetry vs the 47 tools

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python.
> [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — CBM `dev`, C/Go (164 grammars).

Source: the two charts supplied from real sessions. CBM and CRG ran on **different machines**, so
volumes are not comparable. Distributions, error rates and zero-call sets are.

| | codebase-memory-mcp | code-review-graph |
|---|---|---|
| Sessions analysed | 1 034 (145 calls came from **40 sessions**) | all sessions |
| Sessions that used the graph | **40 / 1 034 = 3.9 %** (last 100 sessions: **2**) | unknown |
| Total calls | 145 | 5 898 |
| Tools registered | 17 | 30 |
| Tools ever called | **8 (47 %)** | **14 (47 %)** |
| Zero-call tools | **9** | **16** |
| Calls from top 5 tools | 78.6 % | 90.9 % (5 real + the invalid-name bucket = 98.7 %) |

Both servers have **roughly half their surface unused**, and in both cases the unused half contains the
features their documentation leads with.

---

## 1. CBM: 145 calls, 8 tools, one dominant job

| Tool | Calls | Share | Cumulative |
|---|---|---|---|
| `index_status` | 35 | 24.1 % | 24.1 % |
| `search_graph` | 25 | 17.2 % | 41.4 % |
| `check_index_coverage` | 22 | 15.2 % | 56.6 % |
| `list_projects` | 18 | 12.4 % | 69.0 % |
| `index_repository` | 14 | 9.7 % | 78.6 % |
| `search_code` | 12 | 8.3 % | 86.9 % |
| `trace_path` | 11 | 7.6 % | 94.5 % |
| `get_architecture` | 8 | 5.5 % | 100 % |

**Never called (9):** `query_graph`, `get_code_snippet`, `get_file_outline`, `get_graph_schema`,
`detect_changes`, `compare_graphs`, `manage_adr`, `ingest_traces`, `delete_project`.

Three observations:

1. **Over half the calls (75 / 145 = 51.7 %) are state, not answers.** `index_status` +
   `check_index_coverage` + `list_projects` is the agent asking *"is this graph current, does it cover
   the path I care about, and what is this project even called"*. The graph is being used as a
   **freshness oracle** more than as a knowledge graph.
2. **`check_index_coverage` is the #3 tool — and it is the one capability CRG does not have.** The agent
   does not trust the graph by default; it verifies coverage before relying on an absence claim. That
   is a product insight, not an accident.
3. **`list_projects` at 18 calls** is a smell: the project identity is a derived slug from the
   absolute path, so the agent must re-discover it instead of remembering it.

## 2. CRG: 5 898 calls, but 6 tools carry 95.7 %

| Tool | Calls | Share | Errors | Error rate |
|---|---|---|---|---|
| `get_minimal_context_tool` | 1 284 | 21.8 % | 77 | 6.0 % |
| `detect_changes_tool` | 1 249 | 21.2 % | 47 | 3.8 % |
| `build_or_update_graph_tool` | 1 067 | 18.1 % | 138 | **12.9 %** |
| `query_graph_tool` | 884 | 15.0 % | 99 | **11.2 %** |
| `semantic_search_nodes_tool` | 880 | 14.9 % | 12 | 1.4 % |
| `get_impact_radius_tool` | 281 | 4.8 % | 19 | 6.8 % |
| *(invalid tool name)* | 174 | 3.0 % | 166 | **95.4 %** |
| `get_affected_flows_tool` | 43 | 0.7 % | 3 | 7.0 % |
| `get_review_context_tool` | 23 | 0.4 % | 1 | 4.3 % |
| the other 6 tools | **13** | **0.22 %** | 0 | 0 % |

**Never called (16):** `run_postprocess_tool`, `embed_graph_tool`, `get_docs_section_tool`,
`find_large_functions_tool`, `list_communities_tool`, `get_community_tool`, `refactor_tool`,
`apply_refactor_tool`, `generate_wiki_tool`, `get_wiki_page_tool`, `get_hub_nodes_tool`,
`get_bridge_nodes_tool`, `get_knowledge_gaps_tool`, `get_surprising_connections_tool`,
`get_suggested_questions_tool`, `cross_repo_search_tool`.

CRG's zero list is almost exactly its **"insight" product line**: hubs, bridges, knowledge gaps,
surprising connections, suggested questions, communities, wiki. Nine tools, one job
("tell me something interesting"), zero calls. Plus the whole refactor pair and `run_postprocess`
(the agent always does a full build rather than repairing derived tables).

**Total error rate: 562 / 5 898 = 9.5 %.** Two of the top three error producers are *infrastructure*,
not queries.

---

## 3. The sharpest signal in the dataset

Same intent, two API designs, opposite outcomes:

| | free-form query language | constrained pattern API |
|---|---|---|
| CBM `query_graph` (read-only Cypher, 30 s, 100 k rows) | **0 calls** | |
| CRG `query_graph_tool` (20 fixed patterns: `callers_of`, `tests_for`, `path_to`, …) | | **884 calls (15 %)**, 11.2 % error |

A model will pick from an enum all day; it will not author Cypher it cannot validate. CBM's flagship
engine is unexercised while CRG's much less capable fixed-pattern tool is its #4 tool. The 11.2 % error
rate on CRG's version is the residual cost of the *arguments*, not of the query language.

## 4. Why CRG gets 40× more calls (and why that is not a quality signal)

CBM: 3.6 calls per graph-using session. CRG: thousands, driven by **5 prompts that mandate chains**
("always call `get_minimal_context_tool` first", "≤3 calls per turn"). `minimal_context` +
`detect_changes` = 2 533 calls = **43 %** of everything, and every prompt starts with exactly those two.

So CRG's volume is **manufactured by its prompt/instructions layer**, not evidence that agents
independently want 30 tools. Strikingly, CBM also has 2 prompts (`explore_codebase`,
`review_change_impact`) that its installer never surfaces — and CBM's usage is 3.9 % of sessions.

**Prompt design is the actual adoption lever, and it dwarfs tool count.**

## 5. The 174 wasted calls

`(invalid tool name)` = 174 attempts, 166 of them errors → the agent **hallucinated a tool name 174 times**.

Most likely causes, in order:
1. All 30 CRG tools carry a `_tool` suffix while CBM's are bare verbs (`search_graph`, `trace_path`).
   An agent that works across both will write `detect_changes` or `get_impact_radius`.
2. CRG's CLI names differ from its MCP names (`detect-changes`, `impact`, `search`, `query`) — exactly
   the strings a model has seen in CRG's own documented command set.

Either way it is a **naming tax**, worth ~3 % of all calls. Renaming the 30 tools to bare verbs and
matching the CLI would recover it for free.

---

## 6. What this says to prune

Both zero-call sets are dominated by features that are *cheap to keep and expensive to trust*.
Proposed target surface, covering **>99 %** of everything observed:

| # | Tool | Absorbs | Observed need it serves |
|---|---|---|---|
| 1 | `status` | `index_status`, `check_index_coverage`, `list_projects`, `get_graph_schema` | 75 CBM calls + `minimal_context`'s orientation job |
| 2 | `search` | `search_graph`, `search_code`, `semantic_search`, `get_code_snippet`, `get_file_outline` | 37 CBM calls + 880 CRG calls |
| 3 | `trace` | `trace_path` / `callers_of` / `path_to` / `traverse_graph` | 11 CBM + 884 CRG pattern calls |
| 4 | `impact` | `detect_changes`, `get_impact_radius`, `get_affected_flows`, `get_review_context` | 1 249 + 281 + 43 + 23 CRG calls |
| 5 | `structure` | `get_architecture`, `list_communities`, `list_flows`, `get_flow` | 8 CBM + 4 CRG calls |
| 6 | `build` | `build_or_update_graph`, `run_postprocess` | 1 067 + 14 calls — but see below |
| 7 | `query` (patterns, not Cypher) | `query_graph` | keep the *enum* form, drop the free-form engine |

Cut outright, or move to CLI-only: hubs, bridges, knowledge gaps, surprising connections, suggested
questions, wiki ×2, docs section, refactor ×2, embed, cross-repo search, find large functions,
compare graphs, manage ADR, ingest traces. That is **18 of 47 tools** with zero calls across the whole
dataset.

**One caveat before cutting:** zero calls ≠ useless for
`compare_graphs`/`manage_adr`/`ingest_traces`/`refactor` — they are low-frequency by nature (setup,
audit, repair). The genuinely damning zeros are the *everyday read tools*:
CBM's `query_graph`, `get_code_snippet`, `get_file_outline`, `get_graph_schema`, `detect_changes`,
and CRG's nine analysis/insight tools.

## 7. Two product bugs the numbers expose

**`build_or_update_graph_tool` should not be an agent tool.** 1 067 calls, 12.9 % error — the single
worst error rate among real tools, and it is *infrastructure*, not a query. Every agent session pays a
build call to discover whether the graph is fresh. This should be a daemon/watch responsibility with the
agent calling only `status`. The same is true of CBM's `index_repository` (14 calls): indexing is
background work dressed as a tool.

**`get_minimal_context_tool` at 21.8 % with a 6.0 % error rate** is the agent's real entry point, and it
fails 1 in 17 times. Whatever fails there fails *before* any work happens — that is the highest-leverage
bug in the whole surface.
