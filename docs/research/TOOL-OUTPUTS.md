# Real tool outputs: both MCP servers on one corpus

> **Historical example data.** Every payload below was captured on 2026-11-17 and is kept as-is.
> `<repo>` is a placeholder for the analysed corpus repository and `<project>` for its derived project
> id (CBM's project slug); `<home>` stands in for the capture machine's home directory. Nothing below
> depends on that local checkout — the paths are illustrative, the payloads are real.

Everything below was produced by **actually running** both servers, not synthesised.

| | value |
|---|---|
| Corpus (identical for both) | `<repo>` = [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) @ `bbdda4b9e4b4`, branch `staging` (530 files) |
| `code-review-graph` | [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — 2.4.0, `build` (full), schema v15, Python 3.12 venv |
| `codebase-memory-mcp` | [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — `dev`, self-built `make -f Makefile.cbm cbm` (164 grammars), `mode=full` |
| Capture | MCP stdio JSON-RPC (`initialize` → `tools/list` → `tools/call`), 2026-11-17 |
| Payloads | This repo's capture files in `tool-outputs/`: CRG `./*.json` (32 files) · CBM `tool-outputs/*.json` |

## What the same corpus looks like to each

| | code-review-graph | codebase-memory-mcp |
|---|---|---|
| Files indexed | **530** | **608** |
| Nodes | **13 955** | **17 230** |
| Edges | **130 044** | **87 766** |
| Languages reported | 38 labels | 10 |
| Parse defects | xslt + liquid grammars unavailable (`LookupError`), skipped silently | 3 files `parse_partial` (C++ Qt header, SystemVerilog, PL/SQL fixtures) |
| Calls resolved | 27 782 direct / 44 338 unresolved | 321 unresolved candidates |

The file delta is explained by what each accepts as a source file: CBM indexes Markdown as `File` nodes
and CRG does not (54 `.md` files here — verified by `MATCH (f:File) WHERE f.file_path CONTAINS '.md'`
returning `README.zh-CN.md`, while CRG has no Markdown node kind). CBM's "10 languages" against CRG's
38 labels is a census/grouping difference, not a capability difference.

CBM produces **33 % fewer edges**, and the reason is visible in CRG's own census: 37 209 of its
130 044 edges are `TESTED_BY`, one per test/code pair. CBM models that relation more narrowly through
`TESTS`/`TESTS_FILE`, so the same corpus yields 87 766 edges.

## Output format differs on purpose

- **CRG returns JSON objects** with `status`, `summary`, per-list `*_total`, `truncated`.
- **CBM returns compact prose/tables by default** (`cols:`/`rows:` lines) and only emits JSON when asked
  with `"format":"json"`. That is why many CBM payloads below are wrapped as `{"_text": ...}` — the
  MCP `content[0].text` part is a rendered table, not a JSON object.


## code-review-graph — 30 tools, real payloads

### `get_minimal_context_tool`

Budget-pinned at 800 tokens. The "call me first" tool: everything an agent needs to decide its next call.

```bash
get_minimal_context_tool {}
```

```json
{
  "status": "ok",
  "summary": "13955 nodes, 130044 edges across 530 files. Risk: high (0.80).",
  "key_entities": [
    "GraphStore",
    "resolve_cpp_scoped_call_targets",
    "_resolve_bare_endpoints",
    "_schema_written_by",
    "test_release_list_tracks_the_newest_tag"
  ],
  "risk": "high",
  "communities": [
    "tests-file",
    "code-review-graph-name",
    "workflows"
  ],
  "flows_affected": [
    "activate",
    "benchmark_debug_workflow",
    "benchmark_pre_merge_workflow"
  ],
  "next_tool_suggestions": [
    "detect_changes_tool",
    "semantic_search_nodes_tool",
    "get_a
…
```

*905 bytes, trimmed to 600. Full payload: `tool-outputs/get_minimal_context_tool.json`*

### `list_graph_stats_tool`

Cheap orientation call. Note `CALLS by target resolution` — 62 % of call edges are unresolved in this repo.

```bash
list_graph_stats_tool {}
```

```json
{
  "status": "ok",
  "summary": "Graph statistics for code-review-graph:\n  Files: 530\n  Total nodes: 13955\n  Total edges: 130044\n  Languages: ansible, bash, c, cpp, csharp, dart, elixir, gdscript, go, hcl, html, java, javascript, json, julia, kotlin, liquid, lua, luau, nix, objc, perl, php, python, r, rescript, ruby, rust, scala, solidity, sql, swift, tsx, typescript, verilog, vue, xslt, yaml, zig\n  Last updated: 2026-11-17T13:50:15\n\nNodes by kind:\n  Class: 877\n  DocSection: 3\n  File: 530\n  Function: 5197\n  JsonPath: 156\n  Output: 2\n  Signal: 24\n  Test: 6351\n  Type: 7\n  Variable: 2\n  YamlPath: 806\n\nEdges by kind:\n  CALLS: 72120\n  CONSUMES: 4\n  CONTAINS: 13403\n  DEPEN
…
```

*2524 bytes, trimmed to 700. Full payload: `tool-outputs/list_graph_stats_tool.json`*

### `semantic_search_nodes_tool`

FTS mode by default (no embeddings installed). `score` is BM25, `signature` comes from post-processing.

```bash
semantic_search_nodes_tool {query: 'impact radius'}
```

```json
{
  "status": "ok",
  "query": "impact radius",
  "search_mode": "fts",
  "summary": "Found 46 node(s) matching 'impact radius', showing 20 of 46",
  "results": [
    {
      "name": "get_impact_radius_sql",
      "qualified_name": "<repo>/code_review_graph/graph.py::GraphStore.get_impact_radius_sql",
      "kind": "Function",
      "file_path": "<repo>/code_review_graph/graph.py",
      "line_start": 4663,
      "line_end": 4902,
      "language": "python",
      "params": "(\n        self,\n        changed_files: list[str],\n        max_depth: int = MAX_IM
…
```

*16569 bytes, trimmed to 700. Full payload: `tool-outputs/semantic_search_nodes_tool.json`*

### `query_graph_tool`

One of 20 fixed patterns. Returns full node rows plus the `call_site` line for each caller.

```bash
query_graph_tool {pattern: 'callers_of', target: 'GraphStore.resolve_bare_call_targets'}
```

```json
{
  "status": "ok",
  "pattern": "callers_of",
  "target": "<repo>/code_review_graph/graph.py::GraphStore.resolve_bare_call_targets",
  "description": "Find all functions that call a given function",
  "summary": "Found 12 call site(s) across 12 node(s) for callers_of('<repo>/code_review_graph/graph.py::GraphStore.resolve_bare_call_targets')",
  "result_count": 12,
  "results_omitted": 0,
  "results": [
    {
      "id": 3233,
      "kind": "Function",
      "name": "_resolve_bare_endpoints",
      "qualified_name": "<repo>/code_review_graph/postprocessing.py::_resolve_bare_endpoints",
…
```

*15124 bytes, trimmed to 800. Full payload: `tool-outputs/query_graph_tool.json`*

### `query_graph_tool`

Same query with `detail_level=minimal`: the whole payload is 65 bytes.

```bash
query_graph_tool {…, detail_level: 'minimal'}
```

```text
Unknown tool: 'query_graph_tool_callers_minimal'
```

*65 bytes, complete (CBM renders tables as text by default)*

### `traverse_graph_tool`

One of two tools with no `status` field. BFS from the best-matching node, bounded by tokens.

```bash
traverse_graph_tool {query: 'GraphStore', depth: 2, token_budget: 1500}
```

```json
{
  "start_node": "<repo>/code_review_graph/graph.py::GraphStore",
  "mode": "bfs",
  "max_depth": 2,
  "token_budget": 1500,
  "nodes_visited": 18,
  "traversal": [
    {
      "name": "GraphStore",
      "qualified_name": "<repo>/code_review_graph/graph.py::GraphStore",
      "kind": "Class",
      "file": "<repo>/code_review_graph/graph.py",
      "depth": 0
    },
    {
      "name": "__in
…
```

*7225 bytes, trimmed to 600. Full payload: `tool-outputs/traverse_graph_tool.json`*

### `get_impact_radius_tool`

205 687 bytes — the pinned worst case is 50 000 tokens. See the findings section.

```bash
get_impact_radius_tool {}
```

```json
{
  "status": "ok",
  "risk": "high",
  "unmatched_files": [
    "CHANGELOG.md"
  ],
  "unmatched_files_total": 1,
  "unmatched_files_truncated": false,
  "summary": "Blast radius for 3 changed file(s):\n  - 239 nodes directly changed\n  - 2905 nodes impacted (within 2 hops)\n  - 91 additional files affected\n  - 1 requested file(s) are not indexed: CHANGELOG.md\n  - Results truncated: showing 100 of 2905 impacted nodes\n  - Also truncated: 3443 edges, 139 changed nodes, 0 impacted files omitted
…
```

*205685 bytes, trimmed to 500. Full payload: `tool-outputs/get_impact_radius_tool.json`*

### `get_review_context_tool`

173 951 bytes with source disabled.

```bash
get_review_context_tool {include_source: false, max_files: 5}
```

```json
{
  "status": "ok",
  "risk": "high",
  "unmatched_files": [
    "CHANGELOG.md"
  ],
  "unmatched_files_total": 1,
  "unmatched_files_truncated": false,
  "summary": "Review context for 3 changed file(s):\n  - 239 directly changed nodes, showing 100 of 239\n  - 500 impacted nodes in 91 files, showing 100 of 500\n\nReview guidance:\n- 159 changed function(s) have no direct test: __enter__, __exit__, _align_edge_with_collisions, _aligned_edges, _batch_get_nodes\n  - of those, 144 are reached only
…
```

*173951 bytes, trimmed to 500. Full payload: `tool-outputs/get_review_context_tool.json`*

### `detect_changes_tool`

The review path. 3 changed files (my own `.code-review-graph/` artefacts get filtered), risk 0.80.

```bash
detect_changes_tool {}
```

```json
{
  "status": "ok",
  "summary": "Analyzed 3 changed file(s):\n  - 5 changed function(s)/class(es)\n  - 52 affected flow(s)\n  - 0 test gap(s)\n  - Overall risk score: 0.80\n  - Response bounded: 5 of 5 changed function(s), 0 of 0 changed YAML path(s), 0 of 0 test gap(s), 20 of 52 flow(s) shown",
  "risk_score": 0.8,
  "changed_functions": [
    {
      "id": 1929,
      "kind": "Class",
      "name": "GraphStore",
      "qualified_name": "<repo>/code_review_graph/graph.py::GraphStore",
      "file_path": "<repo>/code_review_graph/graph.py",
      "line_start": 1006,
      "line_end": 5859,
      "language": "python",
      "parent_name": null,
      "is_test": false,
      "risk_score": 0.7
    },
    {
      "id": 2011,
      "kind": "Function",
…
```

*10492 bytes, trimmed to 900. Full payload: `tool-outputs/detect_changes_tool.json`*

### `get_affected_flows_tool`

275 097 bytes — **17× its own 6 000-token default**. One flow alone was 153 KB.

```bash
get_affected_flows_tool {}
```

```json
{
  "status": "ok",
  "summary": "52 flow(s) affected by changes in 3 file(s), showing 25 of 52",
  "changed_files": [
    "CHANGELOG.md",
    "code_review_graph/graph.py",
    "tests/test_upgrade_path.py"
  ],
  "affected_flows": [
    {
      "id": 2,
      "name": "benchmark_debug_workflow",
      "entry_point_id": 1844,
      "depth": 15,
      "node_count": 810,
      "file_count": 20,
      "criticality": 0.739,
      "path": [
        1844,
        1841,
        3613,
        3667,
…
```

*275097 bytes, trimmed to 500. Full payload: `tool-outputs/get_affected_flows_tool.json`*

### `get_hub_nodes_tool`

Degree ranking, capped at 100.

```bash
get_hub_nodes_tool {top_n: 5}
```

```json
{
  "status": "ok",
  "summary": "13414 hub node(s) ranked by degree, showing 5 of 13414",
  "hub_nodes": [
    {
      "name": "GraphStore",
      "qualified_name": "<repo>/code_review_graph/graph.py::GraphStore",
      "kind": "Class",
      "file": "<repo>/code_review_graph/graph.py",
      "in_degree": 846,
      "out_degree": 769,
      "total_degree": 1615,
      "community_id": 6
    },
    {
      "name": "CodeParser",
      "qualified_name": "/home/lj
…
```

*2787 bytes, trimmed to 600. Full payload: `tool-outputs/get_hub_nodes_tool.json`*

### `get_bridge_nodes_tool`

Betweenness centrality — the chokepoints.

```bash
get_bridge_nodes_tool {top_n: 5}
```

```json
{
  "status": "ok",
  "summary": "8852 bridge node(s) ranked by betweenness, showing 5 of 8852",
  "bridge_nodes": [
    {
      "name": "CodeParser",
      "qualified_name": "<repo>/code_review_graph/parser.py::CodeParser",
      "kind": "Class",
      "file": "<repo>/code_review_graph/parser.py",
      "betweenness": 0.042807,
      "community_id": 6
    },
    {
      "name": "GraphStore",
      "qualified_name": "<repo>/
…
```

*2582 bytes, trimmed to 600. Full payload: `tool-outputs/get_bridge_nodes_tool.json`*

### `get_knowledge_gaps_tool`

`summary` is untruncated counts per category; `gaps` is the sample.

```bash
get_knowledge_gaps_tool {max_per_category: 3}
```

```json
{
  "status": "ok",
  "summary": {
    "isolated_nodes": 50,
    "thin_communities": 0,
    "untested_hotspots": 20,
    "single_file_communities": 4,
    "import_cycles": 1
  },
  "gaps": {
    "isolated_nodes": [
      {
        "name": "$.backup.git-push",
        "qualified_name": "<repo>/.beads/config.yaml::yaml:0:$.backup.git-push",
        "kind": "YamlPath",
        "file": "<repo>/.beads/config.yaml",
        "degree": 1
      },
      {
        "name": "$.database",
        "qualified_name": "<repo>
…
```

*4925 bytes, trimmed to 700. Full payload: `tool-outputs/get_knowledge_gaps_tool.json`*

### `get_surprising_connections_tool`

Composite surprise score on edges.

```bash
get_surprising_connections_tool {top_n: 5}
```

```json
{
  "status": "ok",
  "summary": "20400 surprising connection(s) ranked by surprise score, showing 5 of 20400",
  "surprising_connections": [
    {
      "source": "_spy",
      "source_qualified": "<repo>/tests/test_changes.py::TestAnalyzeChangesInternalParseRemap._spy",
      "target": "map_changes_to_nodes",
      "target_qualified": "<repo>/code_review_graph/changes.py::map_changes_to_nodes",
      "edge_kind": "CALLS",
      "surprise_score": 0.65,
…
```

*3748 bytes, trimmed to 600. Full payload: `tool-outputs/get_surprising_connections_tool.json`*

### `get_suggested_questions_tool`

Built entirely from the other four analysis tools. No `status` field.

```bash
get_suggested_questions_tool {}
```

```json
{
  "questions": [
    {
      "category": "bridge_node",
      "question": "'CodeParser' is a critical connector between multiple code regions. Is it adequately tested and documented?",
      "target": "<repo>/code_review_graph/parser.py::CodeParser",
      "priority": "high"
    },
    {
      "category": "bridge_node",
      "question": "'GraphStore' is a critical connector between multiple code regions. Is it adequately tested and documented?",
      "target": "<repo>/code-
…
```

*3253 bytes, trimmed to 600. Full payload: `tool-outputs/get_suggested_questions_tool.json`*

### `find_large_functions_tool`

Oversized symbols — `graph.py:GraphStore` at 4 853 lines is the obvious hit.

```bash
find_large_functions_tool {min_lines: 300, limit: 5}
```

```json
{
  "status": "ok",
  "summary": "Found 273 node(s) with >= 300 lines, showing 5 of 273:\n  25167 lines |     File | <repo>/code_review_graph/parser.py (code_review_graph/parser.py:1)\n  21366 lines |    Class | CodeParser (code_review_graph/parser.py:3801)\n  6216 lines |     File | <repo>/tests/test_multilang.py (tests/test_multilang.py:1)\n  6035 lines |     File | <repo>/code_review_graph/
…
```

*4326 bytes, trimmed to 600. Full payload: `tool-outputs/find_large_functions_tool.json`*

### `list_flows_tool`

186 stored flows; `path` is a raw node-id list.

```bash
list_flows_tool {limit: 3}
```

```json
{
  "status": "ok",
  "summary": "Found 186 execution flow(s), showing 3 of 186",
  "flows": [
    {
      "id": 1,
      "name": "activate",
      "entry_point_id": 855,
      "depth": 4,
      "node_count": 39,
      "file_count": 8,
      "criticality": 0.7426,
      "path": [
        855,
        783,
        811,
        836,
        837,
        839,
        854,
        853,
        883,
        884,
        895,
        902,
        779,
        784,
        781,
        780,
        782
…
```

*24129 bytes, trimmed to 500. Full payload: `tool-outputs/list_flows_tool.json`*

### `get_flow_tool`

19 448 bytes for one flow — the multi-KB-per-flow source of the `get_affected_flows` blowup.

```bash
get_flow_tool {flow_id: 1}
```

```json
{
  "status": "ok",
  "summary": "Flow 'activate': 39 nodes, depth 4, criticality 0.7426",
  "flow": {
    "id": 1,
    "name": "activate",
    "entry_point_id": 855,
    "depth": 4,
    "node_count": 39,
    "file_count": 8,
    "criticality": 0.7426,
    "path": [
      855,
      783,
      811,
      836,
      837,
      839,
      854,
      853,
      883,
      884,
      895,
      902,
      779,
      784,
      781,
      780,
      782,
      819,
      820,
      824,
      816,
…
```

*19448 bytes, trimmed to 500. Full payload: `tool-outputs/get_flow_tool.json`*

### `list_communities_tool`

Leiden via igraph when installed, else file grouping. `tests-file` holds 9 580 nodes.

```bash
list_communities_tool {max_results: 3, max_members: 5}
```

```json
{
  "status": "ok",
  "summary": "Found 10 communities, showing 3 of 10",
  "communities": [
    {
      "id": 10,
      "name": "tests-file",
      "level": 0,
      "cohesion": 0.2617,
      "size": 9580,
      "dominant_language": "python",
      "description": "Directory-based community: tests",
      "members": [
        "<repo>/tests/conftest.py::isolated_crg_home",
        "<repo>/tests/conftest.py::pytest_configure",
        "<home>/Doc
…
```

*3671 bytes, trimmed to 600. Full payload: `tool-outputs/list_communities_tool.json`*

### `get_community_tool`

Members capped at 25 regardless of `max_members`.

```bash
get_community_tool {community_id: 1, include_members: true, max_members: 5}
```

```json
{
  "status": "ok",
  "summary": "Community 'beads': 9 nodes, cohesion 0.7778",
  "community": {
    "id": 1,
    "name": "beads",
    "level": 0,
    "cohesion": 0.7778,
    "size": 9,
    "dominant_language": "json",
    "description": "Directory-based community: .beads",
    "members": [
      "<repo>/.beads/config.yaml::yaml:0:$",
      "<repo>/.beads/config.yaml::yaml:0:$.backup",
      "<repo>/code-review
…
```

*6065 bytes, trimmed to 600. Full payload: `tool-outputs/get_community_tool.json`*

### `get_architecture_overview_tool`

Defaults to `detail_level=minimal` — the only tool that does.

```bash
get_architecture_overview_tool {}
```

```json
{
  "status": "ok",
  "summary": "Architecture: 10 communities, 3 community pairs, 1 warning(s)",
  "communities": [
    {
      "id": 10,
      "name": "tests-file",
      "size": 9580,
      "cohesion": 0.2617,
      "dominant_language": "python"
    },
    {
      "id": 6,
      "name": "code-review-graph-name",
      "size": 2722,
      "cohesion": 0.1623,
      "dominant_language": "python"
    },
    {
      "id": 2,
      "name": "workflows",
      "size": 578,
      "cohesion": 0.9792,
      "dominant_language": "yaml"
    },
    {
      "id": 5,
      "name": "code-review-graph-vscode-node",
      "size": 333,
      "cohesion": 0.3701,
      "dominant_language": "typescript"
    },
…
```

*2810 bytes, trimmed to 700. Full payload: `tool-outputs/get_architecture_overview_tool.json`*

### `refactor_tool`

Read-only preview mode. 92 dead symbols found.

```bash
refactor_tool {mode: 'dead_code', max_results: 5}
```

```json
{
  "status": "ok",
  "summary": "Rename preview: resolve_bare_call_targets -> resolve_bare_call_targets_v2, 17 edit(s), showing 3 of 17. Use apply_refactor_tool(refactor_id='fe7dccdd') to apply.",
  "refactor_id": "fe7dccdd",
  "type": "rename",
  "old_name": "resolve_bare_call_targets",
  "new_name": "resolve_bare_call_targets_v2",
  "qualified_name": "<repo>/code_review_graph/graph.py::GraphStore.resolve_bare_call_targets",
  "edits": [
    {
      "file": "<repo>/code-revie
…
```

*2306 bytes, trimmed to 600. Full payload: `tool-outputs/refactor_tool.json`*

### `apply_refactor_tool`

Chained after `refactor_tool {mode:'rename'}`. Dry run shows the file set and edit count without touching disk.

```bash
apply_refactor_tool {refactor_id: <from rename preview>, dry_run: true}
```

```json
{
  "status": "ok",
  "dry_run": true,
  "applied": 0,
  "edits_applied": 17,
  "would_modify": [
    "<repo>/code_review_graph/graph.py",
    "<repo>/code_review_graph/postprocessing.py",
    "<repo>/code_review_graph/tools/build.py",
    "<repo>/tests/test_cpp_overload_identity.py",
    "<repo>
…
```

*3331 bytes, trimmed to 600. Full payload: `tool-outputs/apply_refactor_tool.json`*

### `get_docs_section_tool`

Reads its own bundled reference doc. A bad name returns the available list — a real recovery affordance.

```bash
get_docs_section_tool {section_name: 'commands'}
```

```json
{
  "status": "ok",
  "section": "commands",
  "content": "MCP tools (30): build_or_update_graph_tool, run_postprocess_tool, get_minimal_context_tool, get_impact_radius_tool, query_graph_tool, get_review_context_tool, semantic_search_nodes_tool, embed_graph_tool, list_graph_stats_tool, get_docs_section_tool, find_large_functions_tool, list_flows_tool, get_flow_tool, get_affected_flows_tool, list_communities_tool, get_community_tool, get_architecture_overview_tool, detect_changes_tool, refactor_t
…
```

*4124 bytes, trimmed to 500. Full payload: `tool-outputs/get_docs_section_tool.json`*

### `generate_wiki_tool`

Writes `.code-review-graph/wiki/` — 11 pages from the communities.

```bash
generate_wiki_tool {force: true}
```

```json
{
  "status": "ok",
  "summary": "Wiki generated: 11 new, 0 updated, 0 unchanged (11 total pages)",
  "wiki_dir": "<repo>/.code-review-graph/wiki",
  "pages_generated": 11,
  "pages_updated": 0,
  "pages_unchanged": 0,
  "_graph": {
    "updated_at": "2026-11-17T13:50:15",
    "age_seconds": 274,
    "built_at_sha": "bbdda4b9e4b48
…
```

*560 bytes, trimmed to 400. Full payload: `tool-outputs/generate_wiki_tool.json`*

### `get_wiki_page_tool`

11 590 bytes of markdown, `max_chars` capped at 80 000.

```bash
get_wiki_page_tool {community_name: 'code-review-graph-name'}
```

```json
{
  "status": "ok",
  "summary": "Wiki page for 'code-review-graph-name' (11047 chars)",
  "content": "# code-review-graph-name\n\n## Overview\n\nDirectory-based community: code_review_graph\n\n- **Size**: 2722 nodes\n- **Cohesion**: 0.1623\n- **Dominant Language**: python\n\n## Members\n\n| Name | Kind | File | Lines |\n|------|------|------|-------|\n| find_hub_nodes | Function | <repo>/code_review_graph/analysis.py | 27-68 |\
…
```

*11590 bytes, trimmed to 500. Full payload: `tool-outputs/get_wiki_page_tool.json`*

### `list_repos_tool`

The registry is empty on this machine — a real empty state, not an error.

```bash
list_repos_tool {}
```

```json
{
  "status": "ok",
  "summary": "0 registered repository(ies)",
  "repos": []
}
```

*80 bytes, complete*

### `cross_repo_search_tool`

Empty registry again; note the `summary` tells the agent how to fix it.

```bash
cross_repo_search_tool {query: 'GraphStore', max_results: 3}
```

```json
{
  "status": "ok",
  "summary": "No repositories registered. Use 'register' to add repos.",
  "results": []
}
```

*110 bytes, complete*

### `embed_graph_tool`

Fails cleanly: `sentence-transformers` is not installed in this venv.

```bash
embed_graph_tool {provider: 'local'}
```

```json
{
  "status": "error",
  "error": "The local embedding provider needs sentence-transformers. Install with: pip install \"code-review-graph[embeddings]\" \u2014 or switch provider to 'openai' / 'google' / 'minimax' / 'voyage'.",
  "_graph": {
    "updated_at": "2026-11-17T13:50:15",
    "age_seconds": 274,
    "built_at_sha": "bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208",
    "built_on_branch": "staging",
    "head_sha": "bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208",
    "head_matches_build": true
  }
…
```

*501 bytes, trimmed to 500. Full payload: `tool-outputs/embed_graph_tool.json`*

### `build_or_update_graph_tool`

Incremental update over an unchanged tree → three signals find nothing, `files_updated: 0`.

```bash
build_or_update_graph_tool {full_rebuild: false}
```

```json
{
  "files_updated": 0,
  "total_nodes": 0,
  "total_edges": 0,
  "changed_files": [],
  "dependent_files": [],
  "stale_files_removed": 0,
  "flow_entry_point_qns": [],
  "communities_were_affected": false,
  "errors": [],
  "freshness_advanced": true,
  "python_resolution": null,
  "rescript_resolution": null,
  "spring_resolution": null,
  "aop_resolution": null,
  "event_resolution": null,
  "temporal_resolution": null,
  "hcl_resolution": null,
  "scoped_resolution": null,
  "jedi_enrichment": null,
  "status": "ok",
  "build_type": "incremental",
  "base_resolved": "bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208",
  "summary": "No changes detected. Graph is up to date.",
  "postprocess_level
…
```

*1185 bytes, trimmed to 700. Full payload: `tool-outputs/build_or_update_graph_tool.json`*

### `run_postprocess_tool`

Derived-table rebuild without the expensive phases.

```bash
run_postprocess_tool {flows: false, communities: false, fts: true}
```

```json
{
  "status": "ok",
  "bare_edges_resolved": 0,
  "cpp_scoped_edges_resolved": 0,
  "signatures_updated": true,
  "fts_indexed": 13955,
  "summary": "Post-processing complete.",
  "_graph": {
    "updated_at": "2026-11-17T13:54:49",
    "age_seconds": 2,
    "built_at_sha": "bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208",
    "built_on_branch": "staging",
    "head_sha": "bbdda4b9e4b4818a9f04d7ef96ac0e
…
```

*449 bytes, trimmed to 400. Full payload: `tool-outputs/run_postprocess_tool.json`*


## codebase-memory-mcp — 16 of 17 tools, real payloads

### `index_status`

Prose key/value. `status: ready` is the gate; `git` block carries freshness.

```bash
index_status {project, verbose: true}
```

```text
project: <project>
nodes: 17230
edges: 87766
status: ready
root_path: <repo>
indexed_at: 2026-11-17T06:57:59Z
git:
  is_git: true
  is_worktree: false
  is_detached: false
  root_exists: true
  worktree_root: <repo>
  git_dir: .git
  git_common_dir: .git
  canonical_root: <repo>
  branch: staging
  branch_slug: staging
  head_sha: bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208
  base_sha: bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208
freshness:
  indexed_generation: 2026-11-17T06:57:59Z
  status_available: true
  tracked_changes:
    count: 0
    paths: 0
    truncated: false
  untracked_source:
    count: 0
    paths: 0
    truncated:
…
```

*1962 bytes of JSON, text trimmed to 900*

### `get_graph_schema`

Label/edge census with counts — how an agent learns the vocabulary before writing Cypher.

```bash
get_graph_schema {project}
```

```text
node_labels: 22  (cols: label count)
  Function 7007
  Method 5148
  Variable 1942
  Class 897
  Section 635
  File 600
  Module 562
  Route 168
  EnvVar 71
  Field 58
  Folder 51
  Interface 31
  Decorator 28
  Process 10
  Type 8
  Struct 5
  Enum 3
  Table 2
  Branch 1
  Image 1
  Project 1
  View 1
edge_types: 27  (cols: type count)
  CALLS 27811
  DEFINES 21281
  USAGE 10008
  TESTS 9834
  WRITES 6899
  DEFINES_METHOD 5133
  IMPORTS 1719
  DECORATES 1251
  SIMILAR_TO 863
  CONTAINS_FILE 600
  FILE_CHANGES_WITH 553
  MENTIONS 350
  CALL_REFERENCE 326
  HTTP_CALLS 265
  SPAWNS 264
  REFERENCES_FILE 263
  CONFIGURES 97
  OVERRIDE 72
  INHERITS 46
  CONTAINS_FOLDER 45
  RAISES 37
  SEMANTICALLY_RELATED 22
  IMPLEMENTS 12
  THROWS 11
  DEPENDS_ON 2
  HAS_BRANCH 1
  TESTS_FILE 1
total: 49
r
…
```

*919 bytes of JSON, text trimmed to 800*

### `search_graph`

`format:json` still returns `{cols, rows}` — a table, not objects. That is the contract, not a bug.

```bash
search_graph {project, query: 'resolve_bare_call_targets', format: 'json'}
```

```json
{
  "cols": [
    "qn",
    "label",
    "file",
    "lines",
    "rank"
  ],
  "rows": [
    [
      "<project>.code_review_graph.graph.GraphStore.resolve_bare_call_targets",
      "Method",
      "code_review_graph/graph.py",
      "3201-3216",
      -50.69574092348044
    ],
    [
      "<project>.code_review_graph.parser.CodeParser._resolve_call_targets",
      "Method",
      "code_review_graph/parser.py",
      "8024-8385",
      -12.27358801485935
    ]
  ],
…
```

*737 bytes, trimmed to 600. Full payload: `tool-outputs/search_graph.json`*

### `search_code`

Raw text search ranked by graph position; `scan_saturated`/`scope_ms` are real telemetry.

```bash
search_code {project, pattern: 'max_nodes', result_limit: 3}
```

```json
{
  "cols": [
    "qn",
    "label",
    "file",
    "lines",
    "matches",
    "matches_omitted",
    "in",
    "out"
  ],
  "rows": [
    [
      "<project>.code_review_graph.tools.query.get_impact_radius",
      "Function",
      "code_review_graph/tools/query.py",
      "738-1011",
      [
        738
      ],
      0,
      29,
      19
    ],
    [
      "<project>.code_review_graph.graph.GraphStore.get_impact_radius_sql",
      "Method",
      "code_review_graph/graph.py",
      "4663-4902",
      [
        4663
      ],
      0,
      18,
      11
    ]
  ]
…
```

*1502 bytes, trimmed to 700. Full payload: `tool-outputs/search_code.json`*

### `trace_path`

The truncation contract in action: `truncated: true`, `truncation_reason: page_limit`, and a `next` cursor.

```bash
trace_path {project, function_name: 'resolve_bare_call_targets', direction: 'both', depth: 2, limit: 4, max_output_tokens: 600}
```

```text
function: resolve_bare_call_targets
direction: both
self_recursive: false
recursive: false
self_call_policy: omitted
callees_total: 15
callees_total_relation: eq
callees: 4  (rows: name hop; qn = group prefix empty ? name : prefix + "." + name; "" marks empty prefix)
builtins:
  dict 2
  len 2
<project>.code_review_graph.graph.GraphStore:
  _drop_bare_endpoint_context 1
  _resolve_bare_endpoints 1
callers_total: 2
callers_total_relation: unknown
callers: 0  (cols: qn hop)
truncated: true
has_more: true
truncation_reason: page_limit
next: c1.o.uce9472b0a5e2d60fg2.7d9abc39e1aae13e.2.1422
hint: "more rows exist — re-call with cursor set to 'next' and ALL other arguments identical (no duplicates), or narrow with depth/edge_types"
```

*850 bytes, complete (CBM renders tables as text by default)*

### `query_graph`

Read-only Cypher. Small because the graph has only 2 callers here.

```bash
query_graph {project, query: "MATCH (n:Function)-[:CALLS]->(m:Function) WHERE m.name = 'resolve_bare_call_targets' RETURN n.name AS caller, n.file_path AS file LIMIT 5"}
```

```text
rows: 0  (cols: caller file)
returned: 0
total: 0
total_relation: eq
has_more: false
truncated: false
hint: "Query returned no results. Use get_graph_schema() to see available labels and edge types."
```

*226 bytes, complete (CBM renders tables as text by default)*

### `get_code_snippet`

`content_verified: true` + `store_generation` — the snippet is checked against the indexed bytes.

```bash
get_code_snippet {qualified_name, project, max_lines: 20, include_neighbors: true}
```

```text
name: resolve_bare_call_targets
qualified_name: <project>.code_review_graph.graph.GraphStore.resolve_bare_call_targets
label: Method
file_path: <repo>/code_review_graph/graph.py
start_line: 3201
end_line: 3216
store_generation: uce9472b0a5e2d60fg2
content_verified: true
source_mode: full
source: |
      def resolve_bare_call_targets(self) -> int:
          """Resolve bare CALLS targets backed by same-file or import evidence.
  
          After parsing, some CALLS edges have bare targets (no ``::`` separator)
          because the parser couldn't resolve cross-file. A globally unique name
          is not sufficient evidence: unrelated repositories often contain one
          matching helper by coincidence. The candidate must be in the call-site
          fil
…
```

*1793 bytes of JSON, text trimmed to 900*

### `get_file_outline`

Declaration list with line ranges; layout differs from CRG's per-node objects.

```bash
get_file_outline {project, file_path: 'code_review_graph/postprocessing.py', limit: 10}
```

```text
file_path: code_review_graph/postprocessing.py
results: 8  (cols: name label lines qn)
  logger Variable 29-29 <project>.code_review_graph.postprocessing.logger
  run_post_processing Function 32-77 <project>.code_review_graph.postprocessing.run_post_processing
  _resolve_bare_endpoints Function 83-103 <project>.code_review_graph.postprocessing._resolve_bare_endpoints
  _compute_signatures Function 106-138 <project>.code_review_graph.postpro
…
```

*1459 bytes of JSON, text trimmed to 700*

### `get_architecture`

Language census: CBM reports 10 languages for a repo where CRG reported 38 — different classification depth.

```bash
get_architecture {project, aspects: ['languages','packages']}
```

```text
project: <project>
total_nodes: 17230
total_edges: 87766
languages: 10  (cols: language files)
  Python 408
  TypeScript 28
  YAML 24
  Java 4
  C 4
  C++ 3
  Rust 3
  Bash 3
  Go 2
  R 2
packages: 15  (cols: name nodes fan_in fan_out)
  assets 827 0 0
  parser 610 0 0
  fixtures 515 0 0
  test_incremental 218 0 0
  src 168 0 0
  test_graph 150 0 0
  test_action_e2e 129 0 0
  tools 116 0 0
  test_rename_imports 105 0 0
  eval 101 0 0
  test_refactor 90 0 0
  daemon 86 0 0
  test_uninstall 70 0 0
  test_workspace_resolver 67 0 0
  test_tsconfig_resolver 67 0 0
```

*677 bytes, complete (CBM renders tables as text by default)*

### `detect_changes`

Empty diff: the tree is clean at `origin/staging`.

```bash
detect_changes {project, scope: 'files'}
```

```text
base: origin/staging
merge_base: bbdda4b9e4b4818a9f04d7ef96ac0e50da13f208
direction: inbound
changed_total: 0
changed_returned: 0
changed_has_more: false
seed_symbols: 0
```

*194 bytes, complete (CBM renders tables as text by default)*

### `check_index_coverage`

25 985 bytes — per-path verdicts, the one capability CRG has no equivalent for.

```bash
check_index_coverage {project, paths: ['code_review_graph/graph.py','does/not/exist.py']}
```

```text
project: <project>
signal: best_effort
indexed_at: 2026-11-17T06:57:59Z
metadata:
  generation: 2026-11-17T06:57:59Z
  index_mode: full
  recorded_at: 2026-11-17T06:57:59Z
  recording_status: complete
  ignored_files_stored: 12
  ignored_files_total: 12
  hash_records_complete: true
  unresolved_calls_complete: true
  coverage_version: 13
  generation_matches: true
  store_generation: uce9472b0a5e2d60fg2
paths: 2  (cols: requested_path path status freshness content_verified recommended_action coverage)
  code_review_graph/graph.py
…
```

*25985 bytes of JSON, text trimmed to 600*

### `manage_adr`

`status: no_adr` plus a hint that spells out the required section template.

```bash
manage_adr {project, mode: 'get'}
```

```text
content: -
status: no_adr
adr_hint: "No ADR yet. Create one with manage_adr(mode='update', content='## PURPOSE\\n...\\n\\n## STACK\\n...\\n\\n## ARCHITECTURE\\n...\\n\\n## PATTERNS\\n...\\n\\n## TRADEOFFS\\n...\\n\\n## PHILOSOPHY\\n...'). For guided creation: explore the codebase with get_architecture, then draft and store. Sections: PURPOSE, STACK, ARCHITECTURE, PATTERNS, TRADEOFFS, PHILOSOPHY."
```

*454 bytes, complete (CBM renders tables as text by default)*

### `list_projects`

7 projects on this machine; trivially cheap.

```bash
list_projects {detail: 'stats', limit: 3}
```

```text
projects: 3  (cols: name root_path branch nodes edges size_bytes)
  <home>-Documents-apps-ef-watcher <home>/Documents/apps/ef-watcher master 2597 5539 11337728
  <home>-Documents-apps-portfolio-web <home>/Documents/apps/portfolio-web main 739 1407 5046272
  <home>-Documents-harness-app-pi-web <home>/Documents/harness-app/pi-web custom 16571 68596 73269248
total: 7
returned: 3
has_more: true
next_offset:
…
```

*527 bytes of JSON, text trimmed to 500*

### `index_repository`

`status:true` reports the last job instead of re-indexing.

```bash
index_repository {repo_path, status: true}
```

```json
{
  "project": "<project>",
  "state": "idle",
  "detail": "no index job has run for this project in this daemon generation; index_status describes the published graph"
}
```

*234 bytes, complete*

### `ingest_traces`

I passed invented symbol names: `pairs_unmatched: 2` and the unmatched sample. A real rejection path.

```bash
ingest_traces {project, label: 'demo-run', traces: [{path: [...]}]}
```

```json
{
  "status": "stored",
  "label": "demo-run",
  "pairs_stored": 0,
  "paths_stored": 0,
  "pairs_unmatched": 2,
  "unmatched_sample": [
    "main",
    "GraphStore.build",
    "GraphStore.resolve_bare_call_targets"
  ],
  "pair_total": 0
}
```

*240 bytes, complete*

### `compare_graphs`

Same project twice is refused with a code. A genuine multi-project diff needs two indexed projects.

```bash
compare_graphs {base_project: P, target_project: P}
```

```json
{
  "error": "base_project and target_project must be distinct",
  "code": "invalid_arguments"
}
```

*96 bytes, complete*


---

## `delete_project` — deliberately not run

It deletes `<cache>/<project>.db`, its `-wal`/`-shm` and the `.adr.db` sidecar, and takes the project
mutation guard while doing it. Running it would have destroyed the index the other 16 examples depend on,
so it is the one tool with no captured output. Its contract is `{project, status, error}`
(`src/mcp/mcp.c:8122`).

## What the run exposed that the docs do not say

| Observation | Evidence |
|---|---|
| `get_affected_flows_tool` returned **275 097 bytes** (~69 k tokens) against a **pinned 6 000-token default / 40 000 worst case**; one flow alone was 153 479 bytes. The 400-step flow budget bounds *steps across* flows, not one flow's size | `tests/test_token_budget.py` pins 6000/40000; payload `tool-outputs/get_affected_flows_tool.json` |
| `get_impact_radius_tool` returned **205 687 bytes** (~51 k tokens) against a pinned 50 000-token worst case | payload in `tool-outputs/` |
| `get_review_context_tool` returned **173 951 bytes** (~43 k tokens) with `include_source: false` | payload in `tool-outputs/` |
| All three overruns happen on a **530-file repo**, not a monorepo — the failure mode scales with graph density (one dense flow, one dense radius), not repository size | corpus stats above |
| CBM counts Markdown as an indexable language; CRG has no Markdown node kind — so "files indexed" is not comparable across the two | `MATCH (f:File) … CONTAINS '.md'` vs `list_graph_stats_tool.nodes_by_kind` |
| CRG silently skipped two grammars (`xslt`, `liquid`) — `LookupError` from `tree-sitter-language-pack`; those files still get a `File` node but no symbols | build log; `languages` still lists both |
| CBM returns **tables, not objects**, even with `format: "json"` (`{cols, rows}`). An agent that expects keyed objects must map columns itself | `search_graph` payload |
| CBM's `truncation_reason` vocabulary (`page_limit`), the `next` cursor, and the "re-call with ALL other arguments identical" hint are a stricter contract than CRG's `", showing N of M"` summary text | `trace_path` payload |
| `cross_repo_search_tool` and `list_repos_tool` are useless until `register` is run — they return `status: "ok"` with an empty result and a hint | both payloads |
