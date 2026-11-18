# Tool specification

Companion to `DESIGN.md`. Six agent tools, one admin tool.

**On the examples:** these tools do not exist yet, so the payloads below are **constructed to spec**, not
captured. Where possible the values are lifted from the real capture in `TOOL-OUTPUTS.md` (the
`GraphStore.resolve_bare_call_targets` trace, the 530-file corpus, the 321 unresolved call sites) so the
shapes and magnitudes are plausible rather than invented from nothing. Two rules for reading them:

- every field shown is required by the envelope unless marked "when truncated" / "when ambiguous";
- no field is ever omitted silently — absence of a list means the list is empty, not unknown.

---

## 1. Conventions that apply to all seven tools

| Convention | Rule | Why |
|---|---|---|
| Naming | bare verbs, no `_tool` suffix | 174 invalid-name attempts / 166 errors in the measured runs, largely suffix and CLI-name confusion |
| Required args | **at most one** per tool | CRG's 2-arg `query_graph` carried an 11.2 % error rate; argument *count* is a bigger error source than argument *type* |
| Repo identity | **no `project` argument anywhere** — bound once at startup | CBM spent 12.4 % of its calls on `list_projects` re-discovering a path slug |
| Output | always the same JSON envelope; never a rendered table | the agent parses it; rendering is the client's job |
| Detail on demand | `cursor` on list tools, `expand(target)` for one item | replaces `detail_level`, `max_*`, `include_source`, `include_neighbors` — the flag explosion both parents grew |
| Budget | every tool takes `budget` (tokens), always optional, always small by default | CRG's hand-tuned per-tool ceilings were breached 3× on a 530-file repo |
| Ambiguity | never guess; return candidates | a confident answer about the wrong node is worse than "ambiguous" |
| Absence | never a bare `0`; always `total_relation` + `coverage` | the whole design exists to make "nothing calls X" trustworthy |

### 1.1 The envelope (exact fields)

```json
{
  "status": "ok",
  "summary": "one line, human-readable, says 'showing N of M' when truncated",
  "read_status": {
    "generation": "g-7f3a91",
    "indexed_at": "2026-11-17T13:50:15Z",
    "stale": true,
    "stale_reason": "3 files changed since index",
    "stale_checked_at": "2026-11-17T14:02:00Z"
  },
  "coverage": {
    "complete": false,
    "indexed": 530,
    "skipped": 12,
    "failed": 3,
    "unresolved_edges": 321,
    "not_indexed": [{"path": "docs/legacy.xslt", "reason": "grammar_unavailable"}]
  },
  "totals": { "callers_total": 2, "total_relation": "gte" },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["expand"],
  "payload": { }
}
```

`status` ∈ `ok | ambiguous | not_found | error | bootstrapping`.

### 1.2 Why every tool carries these six things

| Field | Why it must be in *every* response |
|---|---|
| `read_status.generation` / `stale` | an answer from a stale graph is a wrong answer delivered confidently. The agent must be able to see that the *freshness claim itself* has a timestamp. |
| `coverage.complete` / `unresolved_edges` | this is what converts "0 callers" into either a fact or a caveat. Without it the agent will assert absences the index cannot support. |
| `totals.total_relation` | `eq` vs `gte` is the single cheapest honest signal in the whole protocol. It costs ~20 bytes and prevents the worst class of error. |
| `truncated` + `next` | truncation without a continuation is data loss the agent cannot detect. |
| `next_tools` | with six tools and no prompt scaffolding, this is how the agent routes. CRG got 43 % of its calls from prompts that hard-coded exactly this chaining. |
| `summary` | the one field a model can quote directly into its answer without re-serialising anything. |

---

## 2. `status`

**Description (verbatim, as the model sees it):**

> Report whether the graph is current and what it covers. Call this before telling the user that
> something does not exist, and whenever a previous answer looked incomplete. Returns the index
> generation, how stale the working tree is, per-path coverage, and the language census. Cheap — call it
> freely.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `paths` | array\<string> | optional | `[]` = repo root | The trust question is usually *specific*: "is `src/auth/` indexed, and did it parse cleanly?" A repo-level-only answer forces a second call. This param is what replaces CBM's `check_index_coverage`, the #3 tool at 22 calls. |
| `budget` | integer | optional | `800` | Coverage of 40 paths must not blow up. 800 is the CRG `get_minimal_context` budget, the one tool in the measured set that stayed reliably small. |

No `project`, no `verbose`, no `detail`. CBM's `verbose` + `diagnostics=none|summary|full` triple is
three ways to ask for the same report at three sizes; `budget` and `paths` cover it with fewer axes.

### Example output

```json
{
  "status": "ok",
  "summary": "530 files indexed, generation g-7f3a91, 3 files changed since index. 2 of 2 requested paths indexed.",
  "read_status": {
    "generation": "g-7f3a91",
    "indexed_at": "2026-11-17T13:50:15Z",
    "stale": true,
    "stale_reason": "3 files changed since index",
    "stale_checked_at": "2026-11-17T14:02:00Z"
  },
  "coverage": {
    "complete": true,
    "indexed": 530,
    "skipped": 12,
    "failed": 3,
    "unresolved_edges": 321,
    "not_indexed": []
  },
  "payload": {
    "project": { "name": "code-review-graph-7f3a", "root": "/work/code-review-graph", "commit": "bbdda4b9" },
    "graph": { "nodes": 13955, "edges": 130044, "size_bytes": 208666624, "db_path": "~/.cache/<n>/code-review-graph-7f3a/graph.db" },
    "languages": [
      { "language": "python", "files": 408, "parsed": 408, "failed": 0 },
      { "language": "xslt", "files": 4, "parsed": 0, "failed": 4, "note": "grammar_unavailable" }
    ],
    "paths": [
      { "path": "code_review_graph/graph.py", "indexed": true, "symbols": 214, "sha256": "9c1f…" },
      { "path": "docs/legacy.xslt", "indexed": false, "reason": "grammar_unavailable" }
    ],
    "health": { "indexer": "idle", "last_publish_ms": 41200, "consecutive_failures": 0 }
  }
}
```

### Why this output

- **`generation` at the top of `read_status`** — it is the join key for every cursor and every cached
  belief the agent holds. If it changes, prior conclusions are void.
- **`stale` with a reason and a timestamp** — CBM separated "when was the index built" from "when did we
  check", and that distinction is what lets the agent decide whether to re-ask. A bare `stale: true`
  invites it to re-index defensively (the 1067-call build problem).
- **per-language `{files, parsed, failed}`** — CRG lists a language census that still includes `xslt`
  and `liquid` even though both grammars failed to load. Reporting `parsed: 0, failed: 4` is the
  difference between a census and a claim.
- **`unresolved_edges` at top level, not buried** — it is the number that decides whether any later
  absence claim can be `eq`.
- **`health.indexer`** — without it, a stuck watcher looks exactly like an idle one.

---

## 3. `trace`

**Description:**

> Show what is connected to one symbol: callers, callees, implementers, importers and tests. Accepts a
> function or method name, a dotted name, a file path, or `@decorator`. Call this instead of reading
> files to discover relationships. When several symbols match it returns `status: "ambiguous"` with
> candidates instead of guessing. Does not return source code.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `query` | string | **required** | — | One free-form input is the whole point: the measured 884-vs-0 result says a constrained, single-entry API beats a structured one. Accepting name, dotted name, path and `@decorator` removes the "which tool do I use for this shape" decision entirely. |
| `direction` | enum `all`\|`in`\|`out` | optional | `all` | Callers and callees are different questions but overwhelmingly asked together; `in`/`out` exist for when the agent knows. Making it required would be a second required arg for no gain. |
| `kinds` | array\<enum> | optional | inferred | Narrowing is occasionally needed when a name collides across kinds (`File` vs `Class` vs `Function`). Left optional because inference handles ≥90 % of cases and a wrong `kinds` silently hides the answer. |
| `depth` | integer | optional | `2`, clamped `1–4` | Hop budget is what stops the 275 KB explosion. Clamped rather than required because "how deep" is the server's judgement, not the agent's. |
| `budget` | integer | optional | `3200` | The CRG `detect_changes` budget. Large enough for 2 hops of a dense symbol. |
| `cursor` | string | optional | `null` | Continuation of *this* query. Replaces `limit`/`offset`/`max_results`. |

Deliberately absent: `include_tests`, `include_overrides`, `edge_types`, `parameter_name`,
`mode=calls|dataflow|deploy`, `include_evidence`. CBM's `trace_path` has all six and got 11 calls.
`tests` are a node kind in this design, so they are always included and always labelled; the other five
are separate products.

### Example output

```json
{
  "status": "ok",
  "summary": "resolve_bare_call_targets: 2 callers, 15 callees (4 shown). 1 call site unresolved.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "callers_total": 2, "callees_total": 15, "total_relation": "gte" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_8f21ad",
  "next_tools": ["expand"],
  "payload": {
    "resolution": {
      "query": "resolve_bare_call_targets",
      "matched": "code_review_graph/graph.py::GraphStore.resolve_bare_call_targets",
      "kind": "method",
      "how": "exact name, 1 of 1 candidate",
      "confidence": "extracted"
    },
    "node": {
      "id": 3211,
      "qn": "code_review_graph/graph.py::GraphStore.resolve_bare_call_targets",
      "kind": "method",
      "file": "code_review_graph/graph.py",
      "lines": [3201, 3216],
      "signature": "def resolve_bare_call_targets(self) -> int",
      "is_test": false
    },
    "in": [
      { "edge": "calls", "tier": "extracted", "items": [
        { "id": 3233, "qn": "code_review_graph/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "file": "code_review_graph/postprocessing.py", "line": 90, "hop": 1 },
        { "id": 6073, "qn": "tests/test_csharp_namespaces.py::_build", "kind": "function", "file": "tests/test_csharp_namespaces.py", "line": 812, "hop": 1, "is_test": true }
      ]},
      { "edge": "tested_by", "tier": "extracted", "items": [ { "id": 5997, "qn": "tests/test_cpp_overload_identity.py::test_cross_file_receiver_call_without_type_evidence_stays_unresolved", "kind": "test", "line": 821, "hop": 1 } ] }
    ],
    "out": [
      { "edge": "calls", "tier": "extracted", "items": [ { "id": 3341, "qn": "code_review_graph/graph.py::GraphStore._drop_bare_endpoint_context", "hop": 1 } ] },
      { "edge": "calls", "tier": "inferred", "items": [ { "id": 11002, "qn": "builtins.dict", "external": true, "hop": 1 } ] }
    ],
    "unresolved": [
      { "file": "code_review_graph/parser.py", "line": 8123, "name": "resolve_bare_call_targets", "evidence": "receiver type unknown", "tier": "unresolved" }
    ],
    "not_indexed": []
  }
}
```

### Why this output

- **`resolution` block before the answer.** The measured failure mode of free-text entry is a confident
  answer about the wrong node. `how` ("exact name, 1 of 1 candidate") and `confidence` let the agent
  judge the match without a second call. When it is not 1-of-1, `status` becomes `ambiguous` and this
  block is replaced by `candidates[≤5]`.
- **neighbours grouped by `edge` then `tier`, not one flat list.** Direction × relation *is* the
  semantic content of a trace. A flat list makes every row repeat `"edge": "calls"` (CBM does this and
  it is the single largest waste in its payload), and it hides the difference between a call and an
  implementation at a glance.
- **`tier` per group, not per row.** All edges in one group share a tier by construction, so hoisting it
  to the group costs one token instead of N.
- **`hop`** — required for the agent to know whether a 2-hop neighbour is a direct dependency or
  something it should verify. Without it, transitive and direct results look identical.
- **`external: true`** on `builtins.dict` — worth 12 bytes to stop the agent trying to resolve it.
- **`unresolved` as a first-class array, with `evidence`** — this is where this design diverges from
  CRG (which writes the edge and downgrades a tier) and from CBM (which deletes it and keeps a candidate
  table). Naming the *reason* the binding failed lets the agent decide whether to read the file.
- **`totals` with `gte`** because one call site is unresolved: 2 callers *as far as binding goes*.
  Reporting `eq: 2` here would be a lie the agent would repeat to the user.

---

## 4. `summary`

**Description:**

> Summarize the current situation in a few hundred tokens. `scope="change"` (default) describes the
> working-tree diff with risk, the top blast radius and test gaps; `scope="repo"` describes the
> repository; `scope="area"` describes one path. Call this first in any task to decide what to do next.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `scope` | enum `change`\|`repo`\|`area` | optional | `change` | Zero required args, because "what did I just do" is the overwhelmingly common first question — CRG's `detect_changes` had no required args and took 1249 calls. `change` is the default because it is the cheapest to compute (a diff, not a whole-repo walk) and the most frequently wanted. |
| `target` | string | optional | repo root | Only meaningful for `area`. One param serving one scope is better than a separate `area` tool; the scope enum keeps them discoverable together. |
| `base` | string | optional | generation's commit | Diff base is a *policy* question (§15 Q4 of `DESIGN.md`). Defaulting to the indexed commit means no git call at query time and no discovery timeout (a real CRG failure mode). Pass an explicit base when you need a different diff. |
| `budget` | integer | optional | `1200` | This is the hot path at 43 % of measured traffic, so the default is small. `repo` scope is the cold one (3 calls in the measured runs) and must not raise it. |
| `cursor` | string | optional | `null` | Continuation, mostly for `repo` scope's long lists. |

No `detail_level`. Three scopes × two sizes is six behaviours from one enum; CRG's 13 tools each having
their own `minimal|standard` (plus one `compact`) is why its payloads could not be reasoned about
uniformly.

### Example output — `scope="change"` (default)

```json
{
  "status": "ok",
  "summary": "3 files changed, 5 symbols touched, risk 0.62 (medium). Top impact: GraphStore (41 dependents). 2 untested changes.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": true, "stale_reason": "3 files changed since index" },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "changed_files_total": 3, "changed_symbols_total": 5, "impacted_total": 41, "total_relation": "eq" },
  "truncated": false,
  "next": null,
  "next_tools": ["impact", "trace", "expand"],
  "payload": {
    "base": "bbdda4b9",
    "changed_files": ["code_review_graph/graph.py", "code_review_graph/postprocessing.py", "tests/test_graph.py"],
    "changed_symbols": [
      { "id": 3211, "qn": "code_review_graph/graph.py::GraphStore.resolve_bare_call_targets", "kind": "method", "change": "modified", "lines": [3201, 3216], "risk": 0.71 },
      { "id": 3233, "qn": "code_review_graph/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "change": "added", "risk": 0.34 }
    ],
    "risk": { "score": 0.62, "band": "medium", "drivers": ["41 dependents", "2 changed symbols without tests", "1 changed entry point"] },
    "test_gaps": [ { "qn": "code_review_graph/postprocessing.py::_resolve_bare_endpoints", "reason": "no tested_by edge within 2 hops" } ],
    "removed_symbols": [],
    "suggested_next": [ { "tool": "impact", "why": "41 dependents on GraphStore" }, { "tool": "trace", "query": "GraphStore.resolve_bare_call_targets", "why": "changed entry point" } ]
  }
}
```

### Example output — `scope="repo"` (spot the differences)

```json
{
  "status": "ok",
  "summary": "530 files, 13,955 nodes, 130,044 edges. Python-dominant. 3 entry points. Largest coupling: GraphStore (41 in, 88 out).",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "payload": {
    "size": { "files": 530, "nodes": 13955, "edges": 130044 },
    "languages": [ { "language": "python", "files": 408 }, { "language": "typescript", "files": 28 } ],
    "entry_points": [
      { "qn": "code_review_graph/cli.py::main", "kind": "function", "reachable": 214 }
    ],
    "coupling_hotspots": [
      { "qn": "code_review_graph/graph.py::GraphStore", "in": 41, "out": 88 }
    ],
    "flows": [ { "name": "build", "steps": 39, "files": 8 } ],
    "not_indexed": [ { "path": "docs/legacy.xslt", "reason": "grammar_unavailable" } ]
  }
}
```

### Why this output

- **One shape, three scopes.** The envelope and `read_status`/`coverage` are identical across scopes, so
  the agent parses the same things every time and only `payload` varies. This is the direct lesson from
  CRG's 30 tools with 30 bespoke shapes.
- **`risk.drivers` as sentences, not a bare number.** A score of 0.62 is unusable for deciding what to
  do; "41 dependents" and "2 changed symbols without tests" are actionable, and they are what the agent
  quotes to the user.
- **`test_gaps` carries `reason`** ("no `tested_by` edge within 2 hops"). Both parents report gaps as a
  flat list, which reads as "untested" when it may only mean "test found through a caller". The reason
  is the difference between a claim and a pointer.
- **`suggested_next` as objects with a `why`**, not bare tool names — this is the routing mechanism that
  replaces a prompt that hard-codes a chain.
- **`removed_symbols` always present, even empty** — a deleted function that still has callers is the
  highest-value review signal, and "not present because empty" would make it unlearnable.
- **No source snippets.** Measured: `get_code_snippet` and `get_file_outline` were near-zero; the agent
  reads files with its own reader. Snippets in summary would be the most expensive thing in the payload
  for the least-used information.

---

## 5. `search`

**Description:**

> Find symbols or text in the indexed code. Hybrid keyword (BM25) with optional semantic rerank.
> Returns declarations with file:line and their in/out degree — not source. Use when you know roughly
> what it is called but not where it lives.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `query` | string | **required** | — | Same free-text rationale as `trace`. One entry point covers identifier, substring and natural language. |
| `kind` | enum | optional | any | Occasionally you want only `class` or only `test`. Optional because a wrong `kind` hides the right answer, which is worse than a noisy list. |
| `path` | string | optional | repo root | Prefix filter. Justified because "find this name under `tests/`" is a common real need and there is no other way to express it now that every tool lost its structured args. |
| `budget` | integer | optional | `2000` | Enough for ~20 results with degrees; the measured `semantic_search` equivalent was 880 calls at much larger defaults. |
| `cursor` | string | optional | `null` | Continuation. |

No `provider`, no `model`, no `regex`, no `context`/`source_max_lines`. CBM's `search_code` alone has
five source-shaping knobs; those are file-reader features, not graph features.

### Example output

```json
{
  "status": "ok",
  "summary": "Found 2 declarations for 'resolve_bare_call_targets', showing 2 of 2.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "results_total": 2, "total_relation": "eq" },
  "payload": {
    "mode": "bm25",
    "results": [
      { "id": 3211, "qn": "code_review_graph/graph.py::GraphStore.resolve_bare_call_targets", "kind": "method", "file": "code_review_graph/graph.py", "lines": [3201, 3216], "signature": "def resolve_bare_call_targets(self) -> int", "degree": { "in": 2, "out": 15 }, "score": 0.787 },
      { "id": 8842, "qn": "code_review_graph/parser.py::CodeParser._resolve_call_targets", "kind": "method", "file": "code_review_graph/parser.py", "lines": [8024, 8385], "degree": { "in": 3, "out": 27 }, "score": 0.191 }
    ]
  }
}
```

### Why this output

- **`mode` disclosed** — if the agent asked something semantic and got BM25 because no embedding index
  exists, that changes how much it should trust the ranking. Never silent.
- **`degree` per hit** — this is the cheapest possible disambiguator: the right answer is usually the one
  other code actually uses. It costs two integers and often saves a follow-up `trace`.
- **`score` present but not ordered by it alone** — ties are broken by degree, because BM25 alone ranks
  a huge rarely-used class above a small hot function.
- **No source.** Same rationale as `summary`.

---

## 6. `impact`

**Description:**

> Given a change set, list what it can reach: dependents, affected entry points, and the tests that
> cover it, with a risk score. Defaults to the working-tree diff. Call before editing, or before
> merging.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `paths` | array\<string> | optional | working-tree diff vs `base` | Explicit change sets are for "what would break if I touched these"; the default is the far more common "what am I about to break". |
| `base` | string | optional | generation commit | Same reasoning as `summary`. |
| `depth` | integer | optional | `2`, clamped `1–4` | Impact is a transitive question, so depth matters here more than in `trace`. Clamped because unbounded depth is the documented cause of blowups. |
| `budget` | integer | optional | `2500` | Between `summary` and `trace`. |
| `cursor` | string | optional | `null` | Continuation for the dependents list. |

No `direction`, no `edge_types`, no `fixpoint`, no `seeds`, no `module_limit`. CBM's `detect_changes` has
nine of these and produced a 190-byte answer on a clean tree; the knobs cost more than the payload.

### Example output

```json
{
  "status": "ok",
  "summary": "3 changed files reach 41 nodes in 12 files. 2 entry points affected. 6 of 41 covered by tests.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": true },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "impacted_total": 41, "total_relation": "gte" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_31be07",
  "next_tools": ["trace", "expand"],
  "payload": {
    "changed": [
      { "path": "code_review_graph/graph.py", "symbols": 3, "entry_point": false }
    ],
    "impacted": [
      { "id": 3233, "qn": "code_review_graph/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "hop": 1, "via": "calls", "tier": "extracted", "tested": true },
      { "id": 1180, "qn": "code_review_graph/cli.py::_handle_build_update", "kind": "function", "hop": 2, "via": "calls", "tier": "inferred", "tested": false, "entry_point": true }
    ],
    "unmatched_files": [ { "path": "CHANGELOG.md", "reason": "no symbols" } ],
    "risk": { "score": 0.62, "band": "medium", "drivers": ["2 entry points affected", "6 of 41 impacted nodes tested"] }
  }
}
```

### Why this output

- **`hop` and `via` per impacted node** — "impacted" is meaningless without "how far" and "through what".
  Both are one short field each and together they let the agent group by distance in its own summary.
- **`entry_point: true` flagged inline** — an affected CLI command or HTTP handler is a different
  severity class from an affected helper, and computing that flag at the source is far cheaper than
  making the agent infer it.
- **`tested` per node rather than a separate gaps list** — impact and coverage are two views of the same
  rows; splitting them doubles the payload and forces the agent to join. `summary` keeps the aggregate
  `test_gaps` because that is the review-oriented view.
- **`unmatched_files`** — CRG has this and it matters: a changed file with no nodes (docs, config,
  generated) silently disappears from an impact analysis, and the agent will assume it was analysed.
- **`gte` on the total, not `eq`** — the list is truncated *and* 321 call sites are unresolved. Two
  independent reasons for the total to be a lower bound; the envelope says so once.

---

## 7. `expand`

**Description:**

> Fetch more detail for one item from a previous result. Pass the id, qualified name or path exactly as
> it appeared. Infers what to expand from the item's kind and says which it chose.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `target` | string | **required** | — | A single opaque handle copied verbatim from a prior payload. This is the whole reason the other tools can stay small: detail is addressable instead of pre-shipped. |
| `what` | enum `members`\|`edges`\|`source`\|`coverage` | optional | inferred from kind | Inference keeps the common case to one argument; explicit because sometimes you know. The chosen value is echoed in `payload.what` — inference is never silent. |
| `budget` | integer | optional | `2500` | Detail is by definition the expensive part, so a larger default than `summary`. |

Inference table (disclosed in the response, documented in the instructions):

| target kind | default `what` |
|---|---|
| `file`, `class` | `members` |
| `function`, `method` | `edges` |
| `folder` | `coverage` |

### Example output

```json
{
  "status": "ok",
  "summary": "members of GraphStore, showing 25 of 214.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": true, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "members_total": 214, "total_relation": "eq" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_9a02cc",
  "payload": {
    "target": { "id": 1006, "qn": "code_review_graph/graph.py::GraphStore", "kind": "class", "lines": [1006, 5859] },
    "what": "members",
    "why": "inferred: class -> members",
    "members": [
      { "id": 3211, "name": "resolve_bare_call_targets", "kind": "method", "lines": [3201, 3216], "degree": { "in": 2, "out": 15 } },
      { "id": 1108, "name": "_replace_file_data", "kind": "method", "lines": [1884, 1950], "degree": { "in": 1, "out": 9 } }
    ]
  }
}
```

### Why this output

- **`what` + `why` echoed** — the contract "inference is disclosed" only holds if the model can see what
  it got. This costs ~15 tokens and prevents an entire class of "I asked for members and got edges"
  confusion.
- **`target` block repeated in full** — the agent may be expanding an id it obtained several turns ago;
  restating `{qn, kind, lines}` removes the need for it to keep state and removes a whole class of
  mismatched-id bugs.
- **`degree` on members** — cheap, and it is how the agent picks which member to expand next instead of
  reading the file.
- **`source` exists but is not the default** — measured demand for source-in-payload was ~0. It stays
  available for the case where the agent genuinely wants 20 lines without a file read, but defaults
  point at structure.

---

## 8. `index` (admin profile only)

**Description:**

> Admin: force a full or fast index, or repair a broken one. Not for normal use — the server keeps the
> graph current automatically.

### Parameters

| name | type | required | default | why |
|---|---|---|---|---|
| `force` | boolean | optional | `false` | The only real need an agent has: "the graph looks wrong, rebuild it". |
| `mode` | enum `full`\|`fast` | optional | `full` | `fast` skips non-code files; useful for a first pass on a huge repo. |

### Example output

```json
{
  "status": "ok",
  "summary": "Full index published: 530 files, 13,955 nodes, 130,044 edges in 41.2s.",
  "read_status": { "generation": "g-9c02d1", "indexed_at": "2026-11-17T14:10:02Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 321, "not_indexed": [{ "path": "docs/legacy.xslt", "reason": "grammar_unavailable" }] },
  "payload": {
    "mode": "full",
    "duration_ms": 41203,
    "previous_generation": "g-7f3a91",
    "files": { "scanned": 542, "parsed": 530, "failed": 3, "skipped": 12 }
  }
}
```

### Why this tool exists at all, and why it is hidden

The measured runs show indexing called **1 081 times** and failing **12.9 %** of the time — the worst
error rate of any real tool, for what is pure infrastructure. It stays in the protocol because *repair*
is a real need, but it moves to the `admin` profile so the agent profile cannot see it. The instructions
say: if the graph looks wrong, call `status` and report — do not rebuild.

The output deliberately reports `previous_generation` and the full file accounting: after a rebuild the
only questions are "did it take" and "what did it still miss", and both are in the envelope already.

---

## 9. Budget defaults, and the argument for each

| Tool | Default budget | Rationale |
|---|---|---|
| `status` | 800 | fixed-size report; CRG's 800-token context tool stayed small |
| `summary(change)` | 1200 | hot path — 43 % of measured traffic |
| `summary(repo/area)` | 1200 (same) | cold scopes must not inflate the hot default |
| `trace` | 3200 | 2 hops of a dense symbol |
| `search` | 2000 | ~20 hits with degrees |
| `impact` | 2500 | capped dependents list |
| `expand` | 2500 | detail is where the bytes go, on purpose |

Ceiling: the server refuses `budget > 32000` with `error.code = "budget_too_large"` and a hint. An agent
that wants 100 KB should page.

Floor: every answer has a smallest size — the nine required envelope fields, the coverage counts and a
continuation token — and a budget below it cannot be cut to fit. The server then refuses with
`budget_too_large` and a hint naming the floor in tokens, rather than answering with a payload several
times the allowance: a response that claims `truncated` while still over budget is worse than a
refusal, because the caller cannot tell the two cases apart. `trace` is the one exception: a budget
below a single neighbour returns a terminal page (`truncated: true`, `next: null`) that names the
budget and points at `depth: 1`, since that is a smaller query rather than a smaller answer.

## 10. Total parameter surface

| Tool | required | optional | total |
|---|---|---|---|
| `status` | 0 | 2 | 2 |
| `trace` | 1 | 5 | 6 |
| `summary` | 0 | 4 | 5 |
| `search` | 1 | 4 | 5 |
| `impact` | 0 | 4 | 4 |
| `expand` | 1 | 2 | 3 |
| `index` | 0 | 2 | 2 |

**Seven tools, three required arguments in total.** For comparison: the two parents ship 47 tools and
CBM's `detect_changes` alone takes 9 optional arguments.

## 11. Anti-patterns this spec refuses, with the measurement

| Refused | Measured cost |
|---|---|
| A tool per output size (`detail_level`, `verbose`, `diagnostics`) | 13 of 30 CRG tools carry a `detail_level`; none is validated, so an unknown value silently means `standard` |
| `format: tree|json` | every CBM payload is a rendered table by default, so agents parse text that was meant for humans |
| `project` on every tool | 12.4 % of CBM calls were `list_projects` |
| Free-form query language | 0 calls vs 884 for a constrained enum |
| Source snippets in query payloads | `get_code_snippet` + `get_file_outline` ≈ 0 calls |
| `max_results` / `limit` / `offset` triples | replaced by `budget` + `cursor`; CRG's `max_results` has **no ceiling** on `query_graph_tool` |
| Silent grammar skips | CRG lists `xslt`/`liquid` as supported while both grammars fail to load |
| A bare `0` for "nothing found" | the reason the `total_relation` field exists |
