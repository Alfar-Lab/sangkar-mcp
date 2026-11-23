# Tool specification

> **Source:** [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — CRG 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`), Python.
> [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — CBM `dev`, C/Go (164 grammars).

Companion to `DESIGN.md`. Seven tools on one surface.

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
| Repo identity | **no `project` argument anywhere**; the only repository argument is `repo` (§1.3), which *selects* among trees the operator already registered | CBM spent 12.4 % of its calls on `list_projects` re-discovering a path slug |
| Output | always the same envelope, and the same field names; **encoded as TOON by default on both surfaces**, one line of JSON under `--json` (CLI) or `sangkar mcp --json` (protocol) | the agent parses it; rendering is the client's job — and a TOON tabular row (`key[N]{f1,f2}:`) is still structure the agent parses, not a table rendered for a human |
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
    "stale_checked_at": "2026-11-17T14:02:00Z",
    "decisive": false
  },
  "coverage": {
    "complete": false,
    "indexed": 530,
    "skipped": 1,
    "failed": 3,
    "unresolved_edges": 321,
    "not_indexed": [{"path": "docs/legacy.xslt", "reason": "grammar_unavailable"}],
    "tests": { "nodes": 842, "languages": [{"language": "python", "nodes": 812}, {"language": "typescript", "nodes": 30}] }
  },
  "totals": { "callers_total": 2, "not_indexed_total": 12, "total_relation": "gte" },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["expand"],
  "payload": { }
}
```

`status` ∈ `ok | ambiguous | not_found | error | bootstrapping`.

The block above is the *shape*, written as JSON because that is what a reader can check field by
field. What is delivered is TOON — the same keys, the same values, the same nesting, spelled in fewer
tokens (`encode`/`decode` in `@toon-format/toon` round-trip it losslessly) — unless the caller asked
for JSON with `--json`, or with `sangkar mcp --json` on the protocol surface. Neither rendering
alters a field name or a value: a TOON `not_indexed[N]{path,reason}:` block is a header plus N pairs,
not a table rendered for a human.

**`totals`.** Every list a payload carries gets one `<list>_total` key, and a total is
`number | null`:

| Value | Means |
|---|---|
| a number | the list applies to this scope, and this many items matched |
| `null` | the list does not apply to this scope — an empty change set has no test gaps to count, which is a different answer from "zero gaps" |

Every total stays exactly what the handler counted: `emit()` copies `totals` through unchanged,
which is why no key here may be inferred from a payload length. Two further keys are optional
because only some answers can honestly populate them: `unresolved_material` (the part of the
unresolved census that could still be user-defined code — §2) and `candidates_shown` (how much
of an ambiguity this answer served, beside `candidates_total`, the size of the tie — §3, §7).
`emit()` remains the single truncation point: it preserves each `<list>_total` untouched, and
when it cuts it downgrades `total_relation` from `eq` to `gte` itself. A handler neither restates
a total nor widens a relation.

**`read_status.decisive`.** `false` exactly when `stale` is `true`; filled by the composition root,
never by a handler; a stale answer is self-describing, not silently wrong. A handler knows its own
verdict and not the freshness it answered under, so the one place that sees both — `route()` in
`src/index.ts` — writes this field, and it writes it after the `root_mismatch` downgrade, which is
the last word on staleness: `stale: true` with `decisive: true` is unreachable. An answer that
skipped a handler entirely (`bootstrapping`, `error`) is `decisive: false` by construction, because
nothing that bypassed the tool can be the answer that ends a search.

**`coverage.skipped`, `coverage.tests`, `totals.not_indexed_total`.** Two counts and one census,
each answering a different question:

| key | type | what it is |
|---|---|---|
| `coverage.skipped` | integer | the count of `not_indexed` rows in the scope this answer assembled — the rows the list beside it holds, so `coverage.skipped` is `coverage.not_indexed.length` in every answer that assembles coverage from the graph (`status`, `summary`, `impact`, `trace`, `expand`); what the *repository* never read is `totals.not_indexed_total`'s job. `index` is the one answer this does not describe: it reports a run's accounting, so its `skipped` is the run's count of non-indexable paths and its list is empty by construction (§8) |
| `coverage.tests` | object | `{ nodes, languages: [{ language, nodes }] }` — the test corpus this graph indexed. `nodes: 0` means no test symbols were indexed, a different fact from "nothing covers this change"; `status` and `summary` always carry it, and the other tools do not pay for the query |
| `totals.not_indexed_total` | integer | the repo-wide count of paths the index never read, read from the graph rather than from this answer's list; `status`, `summary` and `expand` report the same number for one generation, so a narrow scope cannot understate what the repository never read |

`emit()` is the one thing that can leave the list shorter than the count beside it: a tight budget
trims `coverage.not_indexed` from the low-path end (`DESIGN.md` §7.1), `coverage.skipped` stays
exact, and the envelope says `truncated: true` with a `next` cursor. An entry for a path the payload
itself names is pinned to the last pass, so the reason for a path in the answer survives the cut
unless nothing else can pay for it.

### 1.2 Why every tool carries these six things

| Field | Why it must be in *every* response |
|---|---|
| `read_status.generation` / `stale` | an answer from a stale graph is a wrong answer delivered confidently. The agent must be able to see that the *freshness claim itself* has a timestamp. |
| `coverage.complete` / `unresolved_edges` | this is what converts "0 callers" into either a fact or a caveat. Without it the agent will assert absences the index cannot support. |
| `totals.total_relation` | `eq` vs `gte` is the single cheapest honest signal in the whole protocol. It costs ~20 bytes and prevents the worst class of error. |
| `truncated` + `next` | truncation without a continuation is data loss the agent cannot detect. |
| `next_tools` | with six tools and no prompt scaffolding, this is how the agent routes. CRG got 43 % of its calls from prompts that hard-coded exactly this chaining. |
| `summary` | the one field a model can quote directly into its answer without re-serialising anything. |

**Decision: `read_status` keeps its field set.** Issue #9's item 2 proposed a compact form for the
common fresh answer, and this is the recorded answer: no. A fresh graph fills the first three members
(`generation`, `indexed_at`, `stale`) and the rest by whichever layer knows them — `stale_reason` only
when `stale` is true, `stale_checked_at` by the handler that ran the check, `decisive` by the
composition root (§1.1) — so the whole set costs a measured few dozen tokens per answer, and
`next_tools` on the same answer is worth more than a compact form would save. `read_status` is a public
contract rather than an internal detail: `DESIGN.md` §5 fixes its members, and a second shape would
have to say twice which of `stale` and `decisive` is authoritative.

### 1.3 The `repo` argument (all seven tools)

Every tool takes one optional argument naming the repository to answer from: type `string`, optional,
**no default**, "a registered repository's path or basename". The published property schema is
`{"type": "string"}` plus its description and — deliberately — no `default`, which the manifest
carries verbatim:

> Registered repository to answer from — its path or basename. Omit it to use the working
> directory's repository, or the only registered one.

The registry is `<home>/sangkar.json`, the list `sangkar repo add|remove|list` maintains; every entry
is stored canonicalised, so `~` is expanded, a relative spelling is resolved and symlinks are
followed before any comparison.

**Resolution order** — `selectRepo` in `src/shared/registry.ts` is the whole contract:

1. **A supplied `repo` is honoured first.** It is matched against the registry as a path, then as a
   basename. A name that matches no entry is `not_found`; a basename two entries share is
   `ambiguous`, with the tie in the message and the registry in the hint. This holds *whatever the
   process was pinned to*: an explicit request that silently fell back to `--repo-root` would be a
   confident answer about the wrong tree. On the CLI the same argument is `--repo <path|name>`.
2. **Otherwise an explicit `--repo-root` supplies the default tree**, honoured without consulting
   the registry — which keeps the ad-hoc invocation on an unregistered tree working.
3. **Otherwise the working directory.** The registry entry whose canonical root equals the
   canonicalised cwd answers; failing that, the single registered repo answers a process started
   anywhere; failing that the call is refused — `not_found` when nothing is registered,
   `ambiguous` when several are and none matches the cwd — each with the registry as its hint.

Both refusals reuse codes that already existed. `not_found` and `ambiguous` are members of the closed
union in `src/shared/errors.ts`, and **no new error code was introduced** for any of this: a refusal
is an `error` envelope whose `error.code` is one of those two on the MCP surface, and the usage exit
`2` with the message and the registry on stderr on the CLI. **No payload key changed either.**
`repo` chooses which graph answers and is consumed by the composition root before the handler is
reached, so it appears in no payload, no `totals` key and no envelope: every payload table below
(§2–§8) is unchanged, and the only number that moves is the argument count (§10).

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
| `repo` | string | optional | — | Which registered tree to report on; §1.3 orders the fallbacks when it is omitted. |

No `project`, no `verbose`, no `detail`. CBM's `verbose` + `diagnostics=none|summary|full` triple is
three ways to ask for the same report at three sizes; `budget` and `paths` cover it with fewer axes.

### `project`, `config` and the unresolved census

`payload.project` carries `branch` and `remote` beside `name`, `root`, `commit` and `db_path`. Both
are `null` when the publish could not record them — a graph written before these keys existed, or a
working tree where git could not answer — and `null` is disclosed rather than guessed: `status`
still answers.

`payload.config.source` is `"project"` when the tree's own `<dataDir>/config.json` was read, and
`"global"` when it was absent or unusable and the machine file supplied the three repository
settings. The fallback is normal and silent — a tree that was indexed but never configured has no
file — so this one field is where it becomes visible instead of a warning on every process start. A
file that is present and unparseable is a different case: it falls back the same way and *also*
writes one `repo config invalid, using the global config` line to stderr.

`payload.unresolved` explains the flat `coverage.unresolved_edges` count instead of restating it:

| key | type | what it is |
|---|---|---|
| `total` | integer | the same measurement as `coverage.unresolved_edges`, read once from the row set that produced it, so the two can never disagree |
| `material` | integer | the part of `total` that could still be user-defined code — the only part an absence claim has to answer for |
| `buckets` | object | the closed six-bucket taxonomy the `trace` filter names (§3), every bucket present in its declared order, zeros included; `sum(buckets)` is `total` |

The census is **repository-wide at every scope**: `coverage.unresolved_edges` in this tool is the
repo-wide count, so a per-path census would make two numbers inside one answer describe two
populations. `paths` scopes `payload.paths` and `coverage.not_indexed`; it does not scope
`payload.unresolved`.

`paths` scopes the skip count too, and only by scoping the list beneath it: `coverage.skipped` is
the number of `not_indexed` rows under the requested paths, so it is always the length of the list
printed beside it, and `totals.not_indexed_total` carries the graph's own count for the same
generation. That is what lets one line read "1 skipped of 12 repo-wide" without claiming the other
eleven were read. `coverage.tests` is not scoped at all — the test corpus is a property of the
generation, so every answer reports the same census, and `nodes: 0` says the graph indexed no test
symbols rather than that this scope has none.

An example of the census on its own:

```json
"unresolved": {
  "total": 321,
  "material": 188,
  "buckets": {
    "builtin_stdlib": 74,
    "external_package": 31,
    "internal_module": 18,
    "dynamic_property_call": 40,
    "receiver_type_unknown": 140,
    "ambiguous": 18
  }
}
```

### Example output

```json
{
  "status": "ok",
  "summary": "530 files indexed, generation g-7f3a91, 1 skipped of 12 repo-wide, 3 failed to parse, 3 changed on disk since the index. 1 of 2 requested paths indexed.",
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
    "skipped": 1,
    "failed": 3,
    "unresolved_edges": 321,
    "not_indexed": [{"path": "docs/legacy.xslt", "reason": "grammar_unavailable"}],
    "tests": { "nodes": 842, "languages": [{"language": "python", "nodes": 812}, {"language": "typescript", "nodes": 30}] }
  },
  "totals": { "paths_total": 2, "not_indexed_total": 12, "total_relation": "gte" },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["expand"],
  "payload": {
    "project": { "name": "acme-graph-7f3a", "root": "/work/acme-graph", "branch": "staging", "remote": "origin", "commit": "bbdda4b9", "db_path": "~/.cache/<n>/acme-graph-7f3a/graph.db" },
    "config": { "source": "project" },
    "graph": { "nodes": 13955, "edges": 130044, "size_bytes": 208666624, "db_path": "~/.cache/<n>/acme-graph-7f3a/graph.db" },
    "unresolved": { "total": 321, "material": 188, "buckets": { "builtin_stdlib": 74, "external_package": 31, "internal_module": 18, "dynamic_property_call": 40, "receiver_type_unknown": 140, "ambiguous": 18 } },
    "languages": [
      { "language": "python", "files": 408, "parsed": 408, "failed": 0 },
      { "language": "xslt", "files": 4, "parsed": 0, "failed": 4, "note": "grammar_unavailable" }
    ],
    "paths": [
      { "path": "src/graph.py", "indexed": true, "symbols": 214, "sha256": "9c1f…" },
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
- **`payload.unresolved` beside it, not instead of it** — the count and its breakdown are one
  measurement, so the bucket that makes the number actionable is a census over the same rows. A
  repo-wide total with a scope-limited breakdown would be worse than a bare number, because the two
  could not be added up.
- **`health.indexer`** — without it, a stuck watcher looks exactly like an idle one.

---

## 3. `trace`

**Description:**

> Show what is connected to one symbol: callers, callees, implementers, importers and tests. Accepts a
> function or method name, a dotted name, a file path, or `@decorator`. Call this instead of reading
> files to discover relationships. When several symbols match it returns `status: "ambiguous"` with
> candidates instead of guessing. Does not return source code.

### Parameters

**Example call** (`inputSchema.examples`): `{"query": "handleRequest", "depth": 1}`

| name | type | required | default | why |
|---|---|---|---|---|
| `query` | string | **required** | — | One free-form input is the whole point: the measured 884-vs-0 result says a constrained, single-entry API beats a structured one. Accepting name, dotted name, path and `@decorator` removes the "which tool do I use for this shape" decision entirely. |
| `direction` | enum `all`\|`in`\|`out` | optional | `all` | Callers and callees are different questions but overwhelmingly asked together; `in`/`out` exist for when the agent knows. Making it required would be a second required arg for no gain. |
| `kinds` | array\<enum> | optional | inferred | Narrowing is occasionally needed when a name collides across kinds (`File` vs `Class` vs `Function`). Left optional because inference handles ≥90 % of cases and a wrong `kinds` silently hides the answer. |
| `path` | string | optional | repo root | Repo-relative prefix with `search`'s rule (§5): the path itself and everything under it. It narrows resolution and the near misses, so a tie entirely inside the prefix can resolve and a miss inside it never answers from outside. Optional because the prefix is a narrowing, not a question. |
| `depth` | integer | optional | `2`, clamped `1–4` | Hop budget is what stops the 275 KB explosion. Clamped rather than required because "how deep" is the server's judgement, not the agent's. |
| `budget` | integer | optional | `3200` | The CRG `detect_changes` budget. Large enough for 2 hops of a dense symbol. |
| `unresolved` | enum `builtin_stdlib`\|`external_package`\|`internal_module`\|`dynamic_property_call`\|`receiver_type_unknown`\|`ambiguous` | optional | all buckets | Narrows the `payload.unresolved` evidence list to one bucket of the closed taxonomy the store already records. Optional because the taxonomy's whole job is explaining a total, and asking for one reason is a drill-down, not the question. |
| `cursor` | string | optional | `null` | Continuation of *this* query. Replaces `limit`/`offset`/`max_results`. |
| `repo` | string | optional | — | Selects the tree the query resolves in: one call resolves in one graph (§1.3). |

The `unresolved` value set is the taxonomy itself (`#shared/domain/unresolved.ts`), spread into the
schema rather than restated, so a bucket added to the taxonomy is filterable in the same commit. It
narrows **what is listed only**: `payload.unresolved_census` and `coverage` always describe the
unfiltered scope, because a filter chooses what to read, not which call sites failed to bind.

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
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "callers_total": 2, "callees_total": 15, "total_relation": "gte" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_8f21ad",
  "next_tools": ["expand"],
  "payload": {
    "resolution": {
      "query": "resolve_bare_call_targets",
      "matched": "src/graph.py::GraphStore.resolve_bare_call_targets",
      "kind": "method",
      "how": "exact name, 1 of 1 candidate",
      "confidence": "extracted"
    },
    "node": {
      "id": 3211,
      "qn": "src/graph.py::GraphStore.resolve_bare_call_targets",
      "kind": "method",
      "file": "src/graph.py",
      "lines": [3201, 3216],
      "signature": "def resolve_bare_call_targets(self) -> int",
      "is_test": false
    },
    "in": [
      { "edge": "calls", "tier": "extracted", "items": [
        { "id": 3233, "qn": "src/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "file": "src/postprocessing.py", "line": 90, "hop": 1 },
        { "id": 6073, "qn": "tests/test_csharp_namespaces.py::_build", "kind": "function", "file": "tests/test_csharp_namespaces.py", "line": 812, "hop": 1, "is_test": true }
      ]},
      { "edge": "tested_by", "tier": "extracted", "items": [ { "id": 5997, "qn": "tests/test_cpp_overload_identity.py::test_cross_file_receiver_call_without_type_evidence_stays_unresolved", "kind": "test", "line": 821, "hop": 1 } ] }
    ],
    "out": [
      { "edge": "calls", "tier": "extracted", "items": [ { "id": 3341, "qn": "src/graph.py::GraphStore._drop_bare_endpoint_context", "hop": 1 } ] },
      { "edge": "calls", "tier": "inferred", "items": [ { "id": 11002, "qn": "builtins.dict", "external": true, "hop": 1 } ] }
    ],
    "unresolved": [
      { "file": "src/parser.py", "line": 8123, "name": "resolve_bare_call_targets", "bucket": "receiver_type_unknown", "evidence": "receiver type unknown", "tier": "unresolved" }
    ],
    "unresolved_census": {
      "total": 321,
      "material": 188,
      "buckets": { "builtin_stdlib": 74, "external_package": 31, "internal_module": 18, "dynamic_property_call": 40, "receiver_type_unknown": 140, "ambiguous": 18 }
    },
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
- **`unresolved_census` beside the evidence list** — `total` is copied from the scope's own
  `coverage.unresolved_edges` over the same rows, so the count and its breakdown cannot disagree.
  `material` is the narrower population that could still be user-defined code, which is the part an
  absence claim actually has to answer for.
- **`bucket` on every unresolved row** — `evidence` is the store's prose and may be reworded; the
  bucket is the closed value the filter accepts, so the agent can widen or narrow the list on the
  next call without parsing a sentence.
- **The `unresolved` filter never narrows the census.** A filtered answer's summary says so:
  `; 12 unresolved call sites in src/parser.py, 3 filtered out (unresolved: receiver_type_unknown).`
  An evidence list of three under a scope total of twelve is otherwise exactly the kind of partial
  number this protocol exists to prevent.

### Example output — `status: "ambiguous"`

```json
{
  "status": "ambiguous",
  "summary": "build: 7 candidates matched, none preferred",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 0, "skipped": 0, "failed": 0, "unresolved_edges": 0, "not_indexed": [] },
  "totals": { "total_relation": "gte", "candidates_total": 7, "candidates_shown": 5 },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["search", "status"],
  "payload": {
    "candidates": [
      {
        "id": 8842,
        "qn": "src/parser.py::CodeParser.build",
        "kind": "method",
        "file": "src/parser.py",
        "lines": [8024, 8385],
        "score": 0.61,
        "why": "exact name, 7 of 7 candidates",
        "next_call": { "tool": "trace", "query": "src/parser.py::CodeParser.build", "direction": "all" }
      }
    ],
    "candidates_total": 7,
    "candidates_shown": 5
  }
}
```

A near miss (`status: "not_found"`) carries the same `next_call` on each entry of `near_misses`,
because a miss and a tie differ in whether an answer was preferred, not in whether an address was
produced.

**How a tie is counted.** `candidates_total` is the size of the tie *before* the five-candidate cap,
and `candidates_shown` is the width the cap allowed; both are copied from the resolution, so the
summary line and the payload cannot disagree. They appear in `totals` and in the payload because
`candidates_shown` counts what was served rather than what matched — folding it into a
`<list>_total` name would make that name lie. A miss reports `near_misses_total` instead.

**`next_call`** is the call that turns one candidate into an answer: `query` is the candidate's own
`qn` and `direction` echoes the caller's choice, so the agent issues it unchanged instead of
reassembling a query from `file` and `lines` — which is how a confident answer about the wrong
node happens.

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
| `repo` | string | optional | — | The tree whose change set to describe; one answer describes one graph, never a union (§1.3). |

No `detail_level`. Three scopes × two sizes is six behaviours from one enum; CRG's 13 tools each having
their own `minimal|standard` (plus one `compact`) is why its payloads could not be reasoned about
uniformly.

### Example output — `scope="change"` (default)

```json
{
  "status": "ok",
  "summary": "3 files changed, 5 symbols touched, risk 0.62 (medium). Top impact: GraphStore (41 dependents). 2 untested changes.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": true, "stale_reason": "3 files changed since index" },
  "coverage": { "complete": false, "indexed": 530, "skipped": 1, "failed": 3, "unresolved_edges": 321, "not_indexed": [{ "path": "docs/legacy.xslt", "reason": "grammar_unavailable" }], "tests": { "nodes": 842, "languages": [ { "language": "python", "nodes": 812 }, { "language": "typescript", "nodes": 30 } ] } },
  "totals": { "changed_files_total": 3, "changed_symbols_total": 5, "test_gaps_total": 2, "removed_symbols_total": 0, "impacted_total": 41, "unresolved_material": 188, "not_indexed_total": 12, "total_relation": "gte" },
  "truncated": false,
  "next": null,
  "next_tools": ["impact", "trace", "expand"],
  "payload": {
    "base": "bbdda4b9",
    "changed_files": [
      { "path": "src/graph.py", "state": "modified", "symbols": 3, "entry_point": true },
      { "path": "src/postprocessing.py", "state": "added", "symbols": 2, "entry_point": false },
      { "path": "src/legacy.py", "state": "deleted", "symbols": 0, "entry_point": false }
    ],
    "changed_symbols": [
      { "id": 3211, "qn": "src/graph.py::GraphStore.resolve_bare_call_targets", "kind": "method", "change": "modified", "lines": [3201, 3216], "risk": 0.71 },
      { "id": 3233, "qn": "src/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "change": "added", "risk": 0.34 }
    ],
    "risk": { "score": 0.62, "band": "medium", "drivers": ["41 dependents", "2 changed symbols without tests", "1 changed entry point"] },
    "test_gaps": [ { "qn": "src/postprocessing.py::_resolve_bare_endpoints", "reason": "no tested_by edge within 2 hops" } ],
    "removed_symbols": [],
    "suggested_next": [ { "tool": "impact", "why": "41 dependents on GraphStore" }, { "tool": "trace", "query": "GraphStore.resolve_bare_call_targets", "why": "changed entry point" } ],
    "unresolved": { "total": 321, "material": 188, "buckets": { "builtin_stdlib": 74, "external_package": 31, "internal_module": 18, "dynamic_property_call": 40, "receiver_type_unknown": 140, "ambiguous": 18 } }
  }
}
```

### Example output — `scope="repo"` (spot the differences)

```json
{
  "status": "ok",
  "summary": "530 files, 13,955 nodes, 130,044 edges. Python-dominant. 3 entry points. Largest coupling: GraphStore (41 in, 88 out).",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 1, "failed": 3, "unresolved_edges": 321, "not_indexed": [{ "path": "docs/legacy.xslt", "reason": "grammar_unavailable" }], "tests": { "nodes": 842, "languages": [ { "language": "python", "nodes": 812 }, { "language": "typescript", "nodes": 30 } ] } },
  "payload": {
    "size": { "files": 530, "nodes": 13955, "edges": 130044 },
    "languages": [ { "language": "python", "files": 408 }, { "language": "typescript", "files": 28 } ],
    "entry_points": [
      { "qn": "src/cli.py::main", "kind": "function", "reachable": 214 }
    ],
    "coupling_hotspots": [
      { "qn": "src/graph.py::GraphStore", "in": 41, "out": 88 }
    ],
    "flows": [ { "name": "build", "steps": 39, "files": 8 } ],
    "not_indexed": [ { "path": "docs/legacy.xslt", "reason": "grammar_unavailable" } ],
    "unresolved": { "total": 321, "material": 188, "buckets": { "builtin_stdlib": 74, "external_package": 31, "internal_module": 18, "dynamic_property_call": 40, "receiver_type_unknown": 140, "ambiguous": 18 } }
  }
}
```

### `changed_files` rows: `state`, `from` and `symbols`

Every row is `{ path, state, from?, symbols, entry_point }`, and `state` is one of `modified`,
`added`, `deleted`, `renamed`, `untracked`:

| state | what it says | `from` | `symbols` |
|---|---|---|---|
| `modified` | different bytes at a path the graph already holds | absent | declarations the graph has for that path |
| `added` | a path the graph does not hold, or did not parse; a git copy reports `added` too | absent | `0` when nothing indexed it |
| `deleted` | the path was indexed and its bytes are gone | absent | `0` — its symbols are history, not part of the change to review |
| `renamed` | one move, reported once | the path it came from | declarations the graph has for the new path |
| `untracked` | present on disk with nothing in git's index: git has never tracked the path | absent | `0` when the graph holds no `file` row for it, its declarations when it does |

`untracked` is the one state `git diff` alone cannot produce. The diff needs a committed side to
compare against and a path git has never tracked has none, so it comes from a second read —
`git status --porcelain -z --untracked-files=all`, where `--untracked-files=all` is what names a new
file inside a new directory instead of that directory. An untracked path passes the same gate as
every other: a path the graph already holds, or one whose extension a grammar covers, takes a row
like any other change and counts in `changed_files_total`; a path with neither is named in
`unmatched_files` with its reason instead (§6). A path the diff, a rename or the digest comparison
already spoke about keeps that more specific state — `untracked` only describes a path no other
source claimed, which is why one file never appears twice.

A rename is one row, not two: `{ "path": "src/graph.py", "state": "renamed", "from": "src/old_graph.py", "symbols": 3, "entry_point": true }`.
`changed_files_total` counts rows, so a rename contributes one. A deleted row keeps its place in
the list with `symbols: 0` — a reviewer who cannot see the deletion cannot see the change — while
`removed_symbols_total` still counts the symbols that went with it.

### `test_gaps_total`: `null` is not `0`

An empty change set has no symbols to assess, so it has no test gaps to count. `test_gaps_total` is
`null` there and a number otherwise: `0` would be the claim "every changed symbol has a test", which
the answer never measured. The same holds in the risk drivers — the test-coverage sentence is
emitted only when there are changed symbols, so no "every changed symbol has a test" line appears
for an empty set. `null` means *not applicable to this scope*; unknown never appears as a total.

### Why this output

- **One shape, three scopes.** The envelope and `read_status`/`coverage` are identical across scopes, so
  the agent parses the same things every time and only `payload` varies. This is the direct lesson from
  CRG's 30 tools with 30 bespoke shapes.
- **`risk.drivers` as sentences, not a bare number.** A score of 0.62 is unusable for deciding what to
  do; "41 dependents" and "2 changed symbols without tests" are actionable, and they are what the agent
  quotes to the user.
- **`changed_files` rows carry `state` and `from`** — a list of bare paths makes a rename look like a
  deletion plus an addition, and hides that a deleted path contributed no symbols. The label is the
  difference between "three files touched" and "one modified, one added, one removed".
- **`payload.unresolved` and `totals.unresolved_material`** — the same census as `status` (§2): the
  count beside its breakdown, and the material part in `totals` so a caller reading only the counts
  still knows how much of the absence claim is user code.
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

**Example call** (`inputSchema.examples`): `{"query": "publish", "kind": "function"}`

| name | type | required | default | why |
|---|---|---|---|---|
| `query` | string | **required** | — | Same free-text rationale as `trace`. One entry point covers identifier, substring and natural language. |
| `kind` | enum `function`\|`method`\|`class`\|`file`\|`decorator` | optional | any | Narrowing for when the caller already knows the shape. `test` is deliberately not a value: the index records tests as `node.is_test`, a flag rather than a kind, so these five are the whole enum. Optional because a wrong `kind` hides the right answer, which is worse than a noisy list. |
| `path` | string | optional | repo root | Prefix filter. Justified because "find this name under `tests/`" is a common real need and there is no other way to express it now that every tool lost its structured args. |
| `budget` | integer | optional | `2000` | Enough for ~20 results with degrees; the measured `semantic_search` equivalent was 880 calls at much larger defaults. |
| `cursor` | string | optional | `null` | Continuation. |
| `repo` | string | optional | — | The tree to search; a page never mixes results from two graphs (§1.3). |

No `provider`, no `model`, no `regex`, no `context`/`source_max_lines`. CBM's `search_code` alone has
five source-shaping knobs; those are file-reader features, not graph features.

**The `path` rule, shared by the other tools.** `path` is a repo-relative prefix: it matches the path
itself and every path under it, so `src` matches `src/a.ts` and `src/graph.py` but not `src-old/a.ts`
(the subtree range is `path === prefix || (path > prefix + "/" && path < prefix + "0")`, because `-`
sorts below `/`). Paths are normalised to forward slashes and empty or `.` segments are dropped
before comparing. An absent `path` means the whole graph. `trace` (§3) and `expand` (§7) take the
identical argument with the identical rule — `trace` applies it to resolution, `expand` to the
target it resolves — so one sentence describes all three tools.

### Example output

```json
{
  "status": "ok",
  "summary": "Found 2 declarations for 'resolve_bare_call_targets', showing 2 of 2.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "results_total": 2, "total_relation": "gte" },
  "payload": {
    "mode": "bm25",
    "results": [
      { "id": 3211, "qn": "src/graph.py::GraphStore.resolve_bare_call_targets", "kind": "method", "file": "src/graph.py", "lines": [3201, 3216], "signature": "def resolve_bare_call_targets(self) -> int", "degree": { "in": 2, "out": 15 }, "score": 0.787 },
      { "id": 8842, "qn": "src/parser.py::CodeParser._resolve_call_targets", "kind": "method", "file": "src/parser.py", "lines": [8024, 8385], "degree": { "in": 3, "out": 27 }, "score": 0.191 }
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
| `repo` | string | optional | — | The tree whose dependents are counted; the seeds and the answer come from one graph (§1.3). |

No `direction`, no `edge_types`, no `fixpoint`, no `seeds`, no `module_limit`. CBM's `detect_changes` has
nine of these and produced a 190-byte answer on a clean tree; the knobs cost more than the payload.

### Example output

```json
{
  "status": "ok",
  "summary": "3 changed files reach 41 nodes in 12 files. 2 entry points affected. 6 of 41 covered by tests.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": true },
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "impacted_total": 41, "unresolved_material": 188, "total_relation": "gte" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_31be07",
  "next_tools": ["trace", "expand"],
  "payload": {
    "changed": [
      { "path": "src/graph.py", "state": "modified", "symbols": 3, "entry_point": false },
      { "path": "src/legacy.py", "state": "renamed", "from": "src/old_legacy.py", "symbols": 1, "entry_point": false }
    ],
    "impacted": [
      { "id": 3233, "qn": "src/postprocessing.py::_resolve_bare_endpoints", "kind": "function", "hop": 1, "via": "calls", "tier": "extracted", "tested": true },
      { "id": 1180, "qn": "src/cli.py::_handle_build_update", "kind": "function", "hop": 2, "via": "calls", "tier": "inferred", "tested": false, "entry_point": true }
    ],
    "unmatched_files": [ { "path": "CHANGELOG.md", "reason": "no symbols" } ],
    "risk": { "score": 0.62, "band": "medium", "drivers": ["2 entry points affected", "6 of 41 impacted nodes tested"] },
    "unresolved": { "total": 321, "material": 188, "buckets": { "builtin_stdlib": 74, "external_package": 31, "internal_module": 18, "dynamic_property_call": 40, "receiver_type_unknown": 140, "ambiguous": 18 } }
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
- **`coverage` scoped to the change set, not to the repository** — `skipped` counts the skip rows
  among the changed paths, so a change under `src/` reports `0` while `status` reports `1 skipped of
  12 repo-wide` (§2). The list and the count always describe the same paths; the repository-wide
  number is not this tool's to publish.
- **`changed` rows carry the same `state`/`from` as `summary`** — one row shape across the two tools
  that report a change set, so a rename is visible as a move and a deletion keeps its place with
  `symbols: 0`. A path with no node at all stays out of `changed` and into `unmatched_files`, where
  the reason is stated.
- **`totals.unresolved_material` and `payload.unresolved`** — the count and its breakdown come from
  the one row set `coverage.unresolved_edges` counted, and the material part is in `totals` because
  it is the part of the blast radius the indexer may simply have failed to see. An explicit change
  set that matched nothing reports zeros: it measured nothing, and an absent census would read as
  "nothing unresolved".
- **`gte` on the total, not `eq`** — the list is truncated *and* 321 call sites are unresolved. Two
  independent reasons for the total to be a lower bound; the envelope says so once.

---

## 7. `expand`

**Description:**

> Fetch more detail for one item from a previous result. Pass the id, qualified name or path exactly as
> it appeared. Infers what to expand from the item's kind and says which it chose.

### Parameters

**Example call** (`inputSchema.examples`): `{"target": "src/indexer/resolve.ts", "what": "outline"}`

| name | type | required | default | why |
|---|---|---|---|---|
| `target` | string | **required** | — | A single opaque handle copied verbatim from a prior payload. This is the whole reason the other tools can stay small: detail is addressable instead of pre-shipped. |
| `what` | enum `members`\|`edges`\|`source`\|`coverage`\|`outline` | optional | inferred from kind | Inference keeps the common case to one argument; explicit because sometimes you know. The chosen value is echoed in `payload.what` — inference is never silent. `outline` is the one value never inferred: it is a table of contents for a container whose default detail is already `members`, so the flatter shape has to be asked for by name. |
| `path` | string | optional | repo root | Repo-relative prefix with `search`'s rule (§5), applied to resolution: "the `Widget` under `tests/`" is a handle collision the agent can already meet and has no other way to express. Optional because an unprefixed target is the common case. |
| `range` | array\<integer> | optional | the node's whole extent | `[from, to]`, 1-based and inclusive. Only `what: "source"` reads it. A source view is priced in lines, so a caller that already knows which lines matter should not pay for the rest — and the tool should not grow a second window argument beside it. |
| `budget` | integer | optional | `2500` | Detail is by definition the expensive part, so a larger default than `summary`. |
| `repo` | string | optional | — | The tree the target is resolved in, when the handle came from this server's other tree (§1.3). |

Inference table (disclosed in the response, documented in the instructions):

| target kind | default `what` |
|---|---|
| `file`, `class` | `members` |
| `function`, `method` | `edges` |
| `folder` | `coverage` |
| anything else | none — `not_found` with a hint to pass `what` |

`outline` is never inferred (see the `what` row above). A bare numeric id is not an address: it is
answered `not_found` with a hint naming the `qn` form rather than reinterpreted as a name.

**The `range` clamp.** A source window is read inside the node's own extent, and nothing else:

- both ends clamp into the extent and `why` says so — `requested: source; range 8-9999 clamped to the node's extent 1006-5859`;
- an inverted pair (`to < from`) selects no lines and says so — `range 8-5 is inverted, so no lines were selected` — rather than being silently reordered into a window the caller never asked for;
- a range that is not exactly two integers is ignored, with a note in `why`;
- none of these throws or returns `error`: a line the node does not occupy is a request this answer cannot honour, not a malformed call.

The effective window is echoed as `payload.window`, so there is one place to read what was returned,
and the clamp lives in `why` rather than in a second field.

A target nothing answers for is `not_found` with `{ target, why, hint }`: the reason it could not be
resolved, and the address forms that would have been accepted. A folder prefix is answered only by
`what: "coverage"`, and the hint says so instead of returning an empty expansion.

A `what: "coverage"` answer scopes its payload to the prefix it was asked about, and not its
`totals.not_indexed_total`: that key is the repo-wide count read from the graph, the same number
`status` and `summary` publish for that generation (§1.1). The prefix's own count is
`coverage.skipped`, which is `coverage.not_indexed.length` and is what the answer's summary sentence
prints — never `totals.not_indexed_total`. Read a folder's coverage as "what this prefix never read",
and `status` as "what the repository never read".

### Example output

```json
{
  "status": "ok",
  "summary": "members of GraphStore, showing 25 of 214.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "members_total": 214, "total_relation": "gte" },
  "truncated": true,
  "truncation_reason": "budget",
  "next": "cur_9a02cc",
  "payload": {
    "target": { "id": 1006, "qn": "src/graph.py::GraphStore", "kind": "class", "lines": [1006, 5859] },
    "what": "members",
    "why": "inferred: class -> members",
    "members": [
      { "id": 3211, "name": "resolve_bare_call_targets", "kind": "method", "lines": [3201, 3216], "degree": { "in": 2, "out": 15 } },
      { "id": 1108, "name": "_replace_file_data", "kind": "method", "lines": [1884, 1950], "degree": { "in": 1, "out": 9 } }
    ]
  }
}
```

### Example output — `what: "outline"`

```json
{
  "status": "ok",
  "summary": "outline of src/graph.py: 26.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "members_total": 26, "total_relation": "gte" },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["trace", "status"],
  "payload": {
    "target": { "id": 1003, "qn": "src/graph.py", "kind": "file", "lines": [1, 5859] },
    "what": "outline",
    "why": "requested: outline",
    "path": "src/graph.py",
    "members": [
      { "qn": "src/graph.py::parse_args", "kind": "function", "lines": [1, 40], "signature": "def parse_args(argv: list[str]) -> Args" },
      { "qn": "src/graph.py::GraphStore", "kind": "class", "lines": [1006, 5859], "signature": "class GraphStore" }
    ]
  }
}
```

The outline is the graph-only answer to "what does this container declare": `members` for a
class-like node, the file's own declarations for a file node, in the store's source order. Each
member carries `qn`, `kind`, `lines` and `signature` so a caller can choose the next target without
opening the file; `signature` is `null` when the grammar gave none, which is not an empty signature.
The row names the member by `qn` rather than by the bare `name` the `members` payload uses: an
outline is read to pick an address, and two methods of one class can share a name while differing in
arity.

### Example output — `what: "source"` with `range`

```json
{
  "status": "ok",
  "summary": "lines 3201-3216 of src/graph.py.",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false, "stale_checked_at": "2026-11-17T14:02:00Z" },
  "coverage": { "complete": false, "indexed": 530, "skipped": 0, "failed": 3, "unresolved_edges": 321, "not_indexed": [] },
  "totals": { "lines_total": 16, "total_relation": "gte" },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["trace", "status"],
  "payload": {
    "target": { "id": 3211, "qn": "src/graph.py::GraphStore.resolve_bare_call_targets", "kind": "method", "lines": [3201, 3216] },
    "what": "source",
    "why": "requested: source",
    "path": "src/graph.py",
    "source_drift": false,
    "window": [3201, 3216],
    "content": [ { "line": 3201, "text": "    def resolve_bare_call_targets(self) -> int:" } ]
  }
}
```

`source_drift: true` with `content: null` is the other source answer: the file's `sha256` no longer
matches the indexed row, so returning bytes would be returning a stale line the agent could quote as
current.

### Example output — `status: "ambiguous"`

```json
{
  "status": "ambiguous",
  "summary": "\"Widget\" matches 7 nodes, showing 5; pass the qn of the one you meant",
  "read_status": { "generation": "g-7f3a91", "indexed_at": "2026-11-17T13:50:15Z", "stale": false },
  "coverage": { "complete": false, "indexed": 0, "skipped": 0, "failed": 0, "unresolved_edges": 0, "not_indexed": [] },
  "totals": { "total_relation": "gte", "candidates_total": 7, "candidates_shown": 5 },
  "truncated": false,
  "truncation_reason": null,
  "next": null,
  "next_tools": ["trace", "status"],
  "payload": {
    "target": "Widget",
    "candidates": [
      { "id": 4102, "qn": "src/widgets.ts::Widget", "kind": "class", "file": "src/widgets.ts", "lines": [12, 88], "score": 0.58, "why": "exact name, 7 of 7 candidates", "next_call": { "tool": "expand", "target": "src/widgets.ts::Widget" } }
    ],
    "candidates_total": 7,
    "candidates_shown": 5,
    "hint": "pass the qn of the item you meant; expand never picks one of a tie"
  }
}
```

A tie is never answered with a guess: the payload carries `candidates_total` (the tie before the
five-candidate cap) and `candidates_shown` (how much of it this answer holds), in the payload and in
`totals`, both copied from the resolution so the summary and the numbers cannot disagree. Each
candidate's `next_call` restates its `qn` as an `expand` `target`, which is what turns "5 of 7
shown" into one call instead of a reassembly from `file` and `lines`.

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
  point at structure. `range` is what makes it affordable when it is wanted, and the clamp rule above
  is what keeps a bad window an answer instead of an error.
- **`outline` is asked for, never inferred** — it is the cheapest way to choose the next target from a
  container (one row per declaration, with signature), and it stays explicit because inferring a
  flatter shape than `members` would hide the detail the caller did ask for.

---

## 8. `index` (repair tool, registered with the rest)

**Description:**

> Admin: force a full or fast index, or repair a broken one. Not for normal use — the server keeps the
> graph current automatically.

### Parameters

**Example call** (`inputSchema.examples`): `{"mode": "fast"}`

| name | type | required | default | why |
|---|---|---|---|---|
| `force` | boolean | optional | `false` | The only real need an agent has: "the graph looks wrong, rebuild it". |
| `mode` | enum `full`\|`fast` | optional | `full` | `fast` republishes only what changed instead of rebuilding from an empty stage — the cheaper first pass on a huge repository. |
| `repo` | string | optional | — | The tree to reindex; §1.3 orders the fallbacks when it is omitted. |

### Example output

```json
{
  "status": "ok",
  "summary": "Full index published: 530 files, 13,955 nodes, 130,044 edges in 41.2s.",
  "read_status": { "generation": "g-9c02d1", "indexed_at": "2026-11-17T14:10:02Z", "stale": false },
  "coverage": { "complete": false, "indexed": 530, "skipped": 12, "failed": 3, "unresolved_edges": 0, "not_indexed": [] },
  "payload": {
    "mode": "full",
    "duration_ms": 41203,
    "previous_generation": "g-7f3a91",
    "files": { "scanned": 542, "parsed": 530, "failed": 3, "skipped": 12 }
  }
}
```

### Why this tool exists at all, and why it is a repair tool

The measured runs show indexing called **1 081 times** and failing **12.9 %** of the time — the worst
error rate of any real tool, for what is pure infrastructure. It stays in the protocol because *repair*
is a real need, registered with the rest of the seven; there is no profile that hides it. It is a repair
tool, so call it only when `status` reports a broken graph. The instructions say: if the graph looks
wrong, call `status` and report — do not rebuild.

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

Ceiling: the server refuses `budget > 32000` with `error.code = "budget_too_large"` and a hint. That
ceiling is the installation's `budgets.max_tokens` in `<home>/config.json` (default 32000, and the
file can only lower it; `INDEXING-PIPELINE.md` §3.0), and `budgets.default_tokens` is only the
fallback for a handler that declares no default of its own — the seven per-tool defaults above are
manifests, not settings. An agent that wants 100 KB should page.

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
| `status` | 0 | 3 | 3 |
| `trace` | 1 | 8 | 9 |
| `summary` | 0 | 6 | 6 |
| `search` | 1 | 5 | 6 |
| `impact` | 0 | 6 | 6 |
| `expand` | 1 | 5 | 6 |
| `index` | 0 | 3 | 3 |

The optional column includes the shared `repo` argument (§1.3), which every tool carries and no tool
defaults.

Every declared argument carries an enum or a description in the published `inputSchema`, and the seven
`example` exports the manifests carry are published as `inputSchema.examples` — §3, §5, §7 and §8
quote theirs in full.

**Seven tools, three required arguments in total.** For comparison: the two parents ship 47 tools and
CBM's `detect_changes` alone takes 9 optional arguments.

## 11. Anti-patterns this spec refuses, with the measurement

| Refused | Measured cost |
|---|---|
| A tool per output size (`detail_level`, `verbose`, `diagnostics`) | 13 of 30 CRG tools carry a `detail_level`; none is validated, so an unknown value silently means `standard` |
| `format: tree|json` | every CBM payload is a rendered table by default, so agents parse text that was meant for humans — the opposite failure from this spec's TOON default, which is the JSON data model in a cheaper spelling, not a drawing of it |
| `project` on every tool | 12.4 % of CBM calls were `list_projects` |
| Free-form query language | 0 calls vs 884 for a constrained enum |
| Source snippets in query payloads | `get_code_snippet` + `get_file_outline` ≈ 0 calls |
| `max_results` / `limit` / `offset` triples | replaced by `budget` + `cursor`; CRG's `max_results` has **no ceiling** on `query_graph_tool` |
| Silent grammar skips | CRG lists `xslt`/`liquid` as supported while both grammars fail to load |
| A bare `0` for "nothing found" | the reason the `total_relation` field exists |
