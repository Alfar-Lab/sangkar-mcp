# Graph data model: nodes, edges, and what is deliberately neither

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python.
> [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — CBM `dev`, C/Go (164 grammars).

Every count below is measured from each project's own source, never from its documentation.

| | `code-review-graph` | `codebase-memory-mcp` |
|---|---|---|
| Node kinds | **14** + 1 display-only | **28** labels |
| Edge kinds | **17** (16 emitted + `OVERRIDES` dead) | **43** relations |
| Storage | `nodes` / `edges` tables, text-keyed | `nodes` / `edges` tables, id-keyed |
| Node-vs-edge decision | 4 parallel dictionaries | 1 spec struct per language |

---

## 1. The rule both projects follow

| Bucket | Test | Examples |
|---|---|---|
| **Node** | has an identity you query by name, a source location, and can be a reference target | `Function`, `Class`, `File`, `Module` |
| **Edge** | a directed relation between two nodes, with no identity of its own | `CALLS`, `IMPORTS`, `INHERITS` |
| **Property** | survives neither test → column or JSON on the node | `params`, `return_type`, `signature`, import paths, config **values**, scalar YAML values |

The property bucket is the one that explains most surprises. A function's parameters, its return type
and its signature are columns on the `Function` row in both projects. There is no `Parameter` node.

## 2. The mechanical mapping

The decision is not code — it is a table lookup.

### code-review-graph — four parallel extraction dicts

```
_CLASS_TYPES     -> Class node          _CALL_TYPES   -> CALLS edge
_FUNCTION_TYPES  -> Function node       _IMPORT_TYPES -> IMPORTS_FROM edge
```

32 languages have a row in all four (`dart` has none in `_CALL_TYPES` and is handled by
`_extract_dart_calls_from_children`). Languages whose four tables are empty
(elixir, nix, zig, lua, luau, sql, hcl) dispatch entirely through constructs handlers.

### codebase-memory-mcp — one spec struct per language

```c
node-producing fields:   function_node_types, class_node_types, field_node_types,
                         module_node_types, variable_node_types, decorator_node_types
edge-producing fields:   call_node_types, import_node_types, import_from_types,
                         throw_node_types, branching_node_types
grammar binding:         ts_factory
re-parse specs:          embedded_imports   // Svelte/Vue/Astro script_element -> raw_text
```

One row per language — 165 rows — with a compile-time `_Static_assert` that the row count equals
`CBM_LANG_COUNT`. A single unified walker, `cbm_extract_unified`, matches node kinds against these
arrays.

---

## 3. Node catalog

### code-review-graph — 14 kinds

| Kind | Meaning | Key columns / `extra` |
|---|---|---|
| `File` | one per parsed file | `language`, `file_hash` (sha256), `line_end` = line count |
| `Class` | class, struct, interface, enum, module | `parent_name`, `modifiers` |
| `Function` | function, method, constructor, XSLT template | `params`, `return_type`, `signature` |
| `Test` | a `Function` with `is_test = 1` | same columns; separate kind for query convenience |
| `Type` | type alias / interface / enum where the grammar emits one | as `Class` |
| `Endpoint` | **synthesised** routed entry point | `http_method`, `route`, `handler` |
| `Scheduler` | **synthesised** from `@Scheduled` | linked out by `TRIGGERS` |
| `ConfigProperty` | Spring config **key** (value discarded) | linked in by `DEPENDS_ON_CONFIG` |
| `Event` | **synthesised** Spring application event | `file_path` = `event`, `extra.virtual = true` |
| `YamlPath` | aggregated YAML path (`$.services[*].metadata.tier`) | `occurrence_count`, `line_samples`, `value_type` |
| `DocSection` | HTML `div`/`section`/`article` with an `id` | `html_id`, `html_tag`; **body text not indexed** |
| `Variable` | Liquid `assign` | `extra.lines` (name reused = one node) |
| `Output` | Liquid `{{ ... }}` | named `output:<expr>` |
| `JsonPath` | JSON path | capped 1,000 paths / 32 levels / 256 KiB |
| `Signal` | display-only, derived from `extra.verilog_kind` | not a stored kind |

### codebase-memory-mcp — 28 labels

Structural spine: `Project`, `Branch`, `Folder`, `Package`, `File`, `Module`, `Section`.

Code symbols: `Class`, `Struct`, `Interface`, `Trait`, `Enum`, `Type`, `Function`, `Method`,
`Field`, `Variable`, `Constant`, `Macro`, `Decorator`.

Synthesised infrastructure, each with a producer verified in code: `Route` (a dedicated route pass),
`Channel` (the channel extractor), `Resource` + `Chart` (the Kubernetes and Terraform passes),
`Image` (the Docker pass), `EnvVar` (the environment-access extractor), `Table` (the definition
extractor).
`Process` exists in the label set but its producer was not verified here — treat as UNKNOWN.

Note what CBM models as a first-class node and CRG does not: `Struct`, `Trait`, `Enum`,
`Constant`, `Macro`, `Decorator`, and the whole deploy chain (`Resource`/`Chart`/`Image`).
`Macro` nodes are only emitted in `full` mode, gated by `cbm_set_macro_extraction`.

---

## 4. Edge catalog

### code-review-graph — 17 kinds (16 emitted)

| Kind | source → target | Impact weight |
|---|---|---|
| `CALLS` | caller → callee (may be a bare name until a resolver qualifies it) | 1.0 |
| `INHERITS` / `IMPLEMENTS` | child → parent / interface | 0.9 |
| `TESTED_BY` | function → test function | 0.7 |
| `REFERENCES` | node → symbol used as a value (callbacks, arrays) | 0.6 |
| `DEPENDS_ON` | general dependency (Ansible `meta`, Solidity `using`) | 0.6 |
| `IMPORTS_FROM` | importing file → module / file / **package directory** | 0.5 |
| `CONTAINS` | file → class/function; class → method; file → YAML path | 0.3 |
| `INJECTS` | Spring bean → injected field type | — |
| `CONSUMES` / `PRODUCES` | method → `kafka:<topic>` (a **string target**, not a node) | — |
| `TEMPORAL_STUB` | class → declared workflow/activity interface | — |
| `DEPENDS_ON_CONFIG` | `@ConfigurationProperties` class → `ConfigProperty` | — |
| `HANDLES` | handler → `Endpoint`; listener → event | — |
| `TRIGGERS` | `Scheduler` → `@Scheduled` method | — |
| `PUBLISHES` | method → Spring event | — |
| `OVERRIDES` | **declared in the edge-weight table with weight 0.9, emitted by no parser** | 0.9 |

Traversal direction per kind is a separate table: `CONTAINS` is `none`,
`TESTED_BY` is `outgoing`, everything else `incoming`.

### codebase-memory-mcp — 43 relations

Structure: `HAS_BRANCH`, `CONTAINS_FOLDER`, `CONTAINS_FILE`, `DEFINES`, `DEFINES_METHOD`.

Code: `CALLS`, `CALL_REFERENCE`, `IMPORTS`, `USAGE`, `INHERITS`, `IMPLEMENTS`, `OVERRIDE`,
`DECORATES`, `RAISES`, `THROWS`, `WRITES`, `DATA_FLOWS`, `REFERENCES_FILE`, `MENTIONS`
(doc-comment → definition, with `via`/`syntax`/`tier`).

Runtime/wiring: `HTTP_CALLS`, `GRPC_CALLS`, `ASYNC_CALLS`, `EMITS`, `LISTENS_ON`, `HANDLES`,
`SPAWNS`, `CONFIGURES`, `INFRA_MAPS`, `CONNECTS_TO`, `DEPLOYS`, `DEPENDS_ON`.

Test/derived: `TESTS`, `TESTS_FILE`, `SIMILAR_TO`, `SEMANTICALLY_RELATED`, `FILE_CHANGES_WITH`.

Cross-repo: `CROSS_HTTP_CALLS`, `CROSS_GRPC_CALLS`, `CROSS_GRAPHQL_CALLS`, `CROSS_TRPC_CALLS`,
`CROSS_ASYNC_CALLS`, `CROSS_CHANNEL`.

---

## 5. Synthesised nodes — where the real judgement lives

Both projects invent nodes purely so an edge has a target. Same concept, different model:

| Concept | code-review-graph | codebase-memory-mcp |
|---|---|---|
| HTTP route | `Endpoint` node + `HANDLES` | `Route` node + `HANDLES` |
| Message topic | **string** `kafka:<topic>` + `CONSUMES`/`PRODUCES` | **`Channel` node** + `EMITS`/`LISTENS_ON` |
| Scheduled job | `Scheduler` node + `TRIGGERS` | `Process` / `Route` |
| Config key | `ConfigProperty` node, **value discarded** | `EnvVar` node, value in `properties` |
| Deployable artifact | — | `Resource`, `Chart`, `Image` + `DEPLOYS` |
| Doc anchor | `DocSection` (id only) | `Section` |
| Config shape | `YamlPath` / `JsonPath`, **aggregated** | YAML keys as properties |

---

## 6. What is deliberately *not* a node

| Thing | Where it goes | Reason |
|---|---|---|
| parameters, return type | `nodes.params`, `nodes.return_type` columns | arity explosion, no independent identity |
| signature text | `nodes.signature`, ≤512 chars, indexed by FTS | search needs it; the graph does not |
| import path segments | folded into one `IMPORTS`/`IMPORTS_FROM` edge | one edge, not a tree |
| config **values** | discarded (CRG) / `properties` (CBM) | secrets, no query needs them |
| YAML / JSON scalar values | discarded — type only (`value_type`) | cardinality |
| direct self-recursion | CBM omits the self `CALLS` edge, exposing `self_call_policy: omitted` | would double every edge count |
| repeated YAML list items | one aggregated `YamlPath` node with `occurrence_count` | a node per item makes the graph useless |

---

## 7. Endpoint discipline and cardinality

**No edge keeps a dangling target.**

- CRG: the endpoint reverts to its bare name, the qualified form moves to `extra.claimed_endpoint`
  (so the edge re-binds if that node appears later), and the edge drops to `UNRESOLVED`.
  Tiers: `EXTRACTED` 1.0, `INFERRED` 0.8 or resolver score,
  `AMBIGUOUS` 0.4 (`extra.ambiguous_targets`), `UNRESOLVED` 0.2.
- CBM: unbound calls are recorded in `index_unresolved_candidates`, so an inbound trace can say
  candidates were truncated instead of claiming there are no callers.

| Guard | CRG | CBM |
|---|---|---|
| similarity edges | none | `SIMILAR_TO` Jaccard ≥ 0.95, **≤10/node**; `SEMANTICALLY_RELATED` ≥ 0.75, **≤10/node** |
| edge granularity | one row per **call site** (`file_path` + `line` are columns) | one row per call site, composite unique key |
| dedup identity | diverge in Python on (kind, source, target, file, line, `source_offset`) | `UNIQUE(source_id, target_id, type, local_name_gen)` |
| containment | `CONTAINS` for everything | split: `CONTAINS_FOLDER`, `CONTAINS_FILE`, `DEFINES`, `DEFINES_METHOD` |

`local_name_gen` is a generated column: one `import` statement can carry several local names, so the
same `(source_id, target_id, type)` triple is legitimately several edges. Non-IMPORTS edges get
`''` — a NULL would break the UNIQUE dedup, since NULLs never conflict.

---

## 8. Storage consequence

| | CRG | CBM |
|---|---|---|
| Edge endpoint | `source_qualified` / `target_qualified` **text** | `source_id` / `target_id` **integers**, FK `ON DELETE CASCADE` |
| Rename of a symbol | reconciled in Python before delete (`_plan_file_identities`), no cascade | node row replaced, ids remapped, edges cascade |
| Query shape | `WHERE target_qualified = ?` on `idx_edges_target_kind` | `WHERE target_id = ?` on `idx_edges_target_kind` |
| FK enforcement | **off** (`foreign_keys` never set) | **on** (`PRAGMA foreign_keys = ON` at open) |

So CRG's model is *textual and repairable* — an edge can point at a name that does not exist yet, and
the resolver machinery is what closes the gap. CBM's model is *referential* — an edge cannot exist
without a node, which is why it spends passes on registries and candidate tables instead of tiers.
