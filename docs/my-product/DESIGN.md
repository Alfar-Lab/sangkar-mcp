# Design: intent-first code graph MCP server

**Status:** brainstorming / pre-implementation. No code exists.
**Working name:** `<name>` (TBD — not a decision yet).
**Sources compared throughout:** **CRG** = [tirth8205/code-review-graph](https://github.com/tirth8205/code-review-graph) — Python MCP graph server, 2.4.0, observed at commit `bbdda4b9e4b4` (branch `staging`). **CBM** = [DeusData/codebase-memory-mcp](https://github.com/DeusData/codebase-memory-mcp) — C/Go MCP graph server, `dev`, 164 grammars.
**Evidence base:** `TOOL-SURFACE.md`, `TOOL-OUTPUTS.md`, `TOOL-USAGE-ANALYSIS.md`, `GRAPH-MODEL.md`, `INDEXING-PIPELINE.md`, `STORAGE-ARCHITECTURE.md` in this folder. Every "because" below points at one of them.

---

## 1. What this is

A local MCP server that gives a coding agent **three answers it repeatedly needs, in one call each**:

1. *Is what you're telling me true right now?* (**trust**)
2. *What is connected to this symbol?* (**trace**)
3. *What is going on here?* (**summary**)

Plus one lifecycle operation the agent should almost never have to think about (**index**).

It is **not** a query language, not an analytics suite, not a documentation generator, and not a refactoring engine. Those were measured at zero calls in real usage (see §3).

## 2. The four objectives

The original plan had three (index / trace / summarize). Telemetry forces a fourth.

| # | Objective | Agent question | Measured demand |
|---|---|---|---|
| 1 | **Index** | *build the graph* | 1 067 build calls + 14 `index_repository` — and 12.9 % of the CRG builds errored |
| 2 | **Trust** | *is the graph current, and does it cover what I'm asking about?* | **51.7 % of all CBM calls** (`index_status` 35, `check_index_coverage` 22, `list_projects` 18) |
| 3 | **Trace** | *what is connected to X?* | 11 CBM `trace_path` + 884 CRG pattern queries |
| 4 | **Summarize** | *what is going on here?* | **43 % of all CRG calls** (`get_minimal_context` 1284 + `detect_changes` 1249) |

Trust is not a helper tool. It is the contract that makes the other three safe to believe — see §6.

## 3. Non-goals (each one is a measured zero)

| Not building | Evidence |
|---|---|
| Free-form Cypher / any open query language | CBM `query_graph`: **0 calls** in 145; CRG's constrained pattern enum: **884 calls** |
| Insight suite (hubs, bridges, knowledge gaps, surprising connections, suggested questions) | 5 CRG tools, **0 calls** |
| Community/betweenness analytics as a tool | `list_communities` + `get_community`: 0 calls; `get_architecture_overview`: 3 calls |
| Wiki generation | 2 tools, 0 calls |
| Refactor preview/apply (writes source) | 2 tools, 0 calls |
| Docs-section reader | 0 calls |
| Cross-repo search | 0 calls (and it needs a registry nobody populates) |
| Embeddings as a hard dependency | both projects default it off; semantic path is an enhancement, never a prerequisite |
| Vendored grammars per language | CBM: 164 grammar TUs, 319 MB binary, ~7 min build — for a language census that telemetry says nobody exercises |
| Cross-file type resolution (LSP) | deferred; §9 keeps the door open |
| Agent-callable build | 12.9 % error rate on CRG's build tool; it is infrastructure wearing a tool's clothes |
| Per-tool hand-tuned size ceilings | CRG's were breached in 3 of 30 tools on a 530-file repo (§7) |
| `_tool` suffix in names | 174 invalid-name attempts, 166 errors ≈ 3 % of all calls |

---

## 4. Tool surface

Seven tools on one surface. Every name is a bare verb.

| Tool | Job | Required args | Why it exists |
|---|---|---|---|
| `status` | trust | none | 51.7 % of real demand |
| `trace` | trace | `query` | the main feature |
| `summary` | summarize | none (`scope` defaults to `change`) | hottest path (43 %) |
| `search` | find | `query` | `search_graph` + `search_code` were 25.5 % of CBM |
| `impact` | blast radius | none | `detect_changes` 1249 + `get_impact_radius` 281 |
| `expand` | detail on demand | `cursor` | replaces every `detail_level` variant and every per-item cap |
| `index` | repair | none | repair and bootstrap path; discouraged by the instructions (measured: 1 081 calls, 12.9 % failures) |

**One surface, no profiles.** CBM's `--tool-profile scout|analysis` motivated a split, and the
measurement behind it still stands: `index` was called **1 081 times** and failed **12.9 %** of the time,
the worst error rate of any real tool, for work the server already does itself. What shipped is not a
hard allow-list at the protocol layer: all seven tools are registered for every client, and the
instructions discourage `index` instead, keeping it as the repair and bootstrap path — a fresh tree has
no store for a tool call to answer from.

### 4.1 Signatures

Declarative spec, not code. Every arg beyond the required one has a default, so the agent can call with one string.

**`status`**

| arg | type | default | meaning |
|---|---|---|---|
| `paths` | array\<string> | `[]` (= repo root) | paths whose coverage to report |
| `budget` | int | 800 | token budget |

Returns: generation, `indexed_at`, staleness verdict, per-path coverage, language census, graph size, project identity, indexer health. See §6 for the exact envelope.

**`trace`**

| arg | type | default | meaning |
|---|---|---|---|
| `query` | string | — (**required**) | free text, dotted name, file path, or `@decorator` |
| `direction` | enum | `all` | `all` \| `in` \| `out` |
| `kinds` | array\<enum> | — | narrow resolution to these node kinds when a name collides: `file`,`module`,`class`,`function`,`method`,`test`,`endpoint` |
| `path` | string | — | repo-relative prefix applied when resolving the query |
| `depth` | int | 2 | hops of callers and callees to walk, clamped 1–4 |
| `budget` | int | 3200 | token budget |
| `unresolved` | enum | — | list only this unresolved bucket; the census stays unfiltered |
| `cursor` | string | — | continuation token from a previous `trace` |

Returns: resolution outcome (`ok` / `ambiguous` / `not_found`), the resolved node(s), neighbours ranked by hop distance, the edge type that connected each, confidence tier, and coverage. Never guesses on ties (§8).

**`summary`**

| arg | type | default | meaning |
|---|---|---|---|
| `scope` | enum | `change` | `change` \| `repo` \| `area` |
| `target` | string | cwd / repo root | path or package for `area` |
| `base` | string | last index generation's commit | diff base for `change` |
| `budget` | int | 1200 | token budget |
| `expand` | cursor | — | continuation |

Scope behaviour:

- `change` → changed files/symbols, risk, top blast radius, test gaps, staleness, next-tool hints
- `repo` → size, languages, entry points, top 3 flows, coupling hotspots, what is *not* indexed
- `area` → one path: its public surface, its dependents, its risk

`repo` and `area` must **not** be computed on the `change` path (architecture overview got 3 calls vs minimal-context's 1284 — never make the hot path pay for the cold payload).

**`search`**

| arg | type | default | meaning |
|---|---|---|---|
| `query` | string | — (**required**) | text, identifier, or natural language |
| `kind` | enum | any | `function`,`method`,`class`,`file`,`decorator` |
| `path` | string | — | repo-relative prefix to search under |
| `budget` | int | 2000 | token budget |
| `cursor` | string | — | continuation |

Hybrid: FTS5/BM25 first, semantic rerank only when an embedding index exists. Never fails because embeddings are absent.

**`impact`**

| arg | type | default | meaning |
|---|---|---|---|
| `paths` | array\<string> | changed files vs `base` | explicit change set |
| `base` | string | last generation commit | diff base |
| `depth` | int | 2 | expansion depth (clamped 1–4) |
| `budget` | int | 2500 | token budget |
| `cursor` | string | — | continuation |

**`expand`**

| arg | type | default | meaning |
|---|---|---|---|
| `target` | string | — (**required**) | opaque handle copied from a prior payload: an id, a qualified name or a path |
| `what` | enum | inferred | `members` \| `edges` \| `source` \| `coverage` \| `outline`; `outline` is never inferred |
| `path` | string | — | repo-relative prefix applied when resolving the target |
| `range` | array\<int> | the node's extent | 1-based inclusive line span for `source`, clamped to the resolved node's extent |
| `budget` | int | 2500 | token budget |

Returns detail the other tools did not pre-ship for one target, addressed by the handle that payload carried. Rules in §7.

**`index`** (repair tool, registered with the rest)

| arg | type | default | meaning |
|---|---|---|---|
| `force` | bool | false | rebuild from an empty stage instead of routing incrementally |
| `mode` | enum | `full` | `full` \| `fast` (fast republishes the changed paths only) |

---

## 5. Response envelope (every tool, no exceptions)

One shape for everything. This is the single most important contract in the design — it is what makes absence claims honest and truncation cheap.

| Field | Type | Always? | Meaning |
|---|---|---|---|
| `status` | enum | yes | `ok` \| `ambiguous` \| `not_found` \| `error` \| `bootstrapping` |
| `summary` | string | yes | one line, human-readable, `showing N of M` when truncated |
| `read_status` | object | yes | `{generation, indexed_at, stale, stale_reason?, stale_checked_at?, decisive?}` — freshness of the answer itself; `decisive` is `!stale`, written once by the composition root (rule 1, below) |
| `coverage` | object | yes | `{complete: bool, indexed: N, skipped: N, failed: N, unresolved_edges: N, not_indexed: [{path, reason}], tests?}` — `complete` is a boolean, not a tri-state (`TOOLS-SPEC.md` §1.1) |
| `totals` | object | when lists exist | `{<list>_total, total_relation: "eq"\|"gte"}` |
| `truncated` | bool | yes | true when `budget` cut the payload |
| `truncation_reason` | string | when truncated | `budget` \| `item_cap` \| `depth` \| `page` |
| `next` | string | when truncated | cursor for `expand` |
| `next_tools` | array\<string> | when useful | ≤3 suggested next calls |
| `error` | object | on failure | `{code, message, hint, retryable}` |

Rules:

1. `stale: true` is a **warning, not a failure** — the answer is returned, flagged, and never silently so: `read_status.decisive` is `false` exactly when `stale` is `true`, and is written once, by the composition root (`route()` in `src/index.ts`), after the `root_mismatch` downgrade — a handler knows its own verdict, not the freshness it answered under, so `stale: true` with `decisive: true` is unreachable.
2. `total_relation: "gte"` whenever the total is a lower bound (truncation, partial coverage, unresolved edges).
3. `status: "bootstrapping"` returns `{retry_after_ms}` instead of blocking the client past its deadline.
4. Errors name the tool, the argument, and the fix. No bare stack traces.

---

## 6. Absence semantics

The question this must answer correctly: *"nothing calls X — are you sure?"*

Both parents fail this differently. CRG demotes unbound edges to a `UNRESOLVED` tier (0.2) but still stores the edge; CBM deletes nothing and keeps a separate `index_unresolved_candidates` table. Neither can say *"I did not look there."*

This design uses **four distinct outcomes** and never collapses them:

| Outcome | Meaning | Payload |
|---|---|---|
| **absent** | The scope was indexed, fresh, and no edge exists | `status: ok`, `total: 0`, `total_relation: "eq"`, `coverage.complete: true` |
| **unresolved** | Call sites exist whose target did not bind | `total: 0`, `total_relation: "gte"`, `unresolved: [{file, line, name, evidence}]` |
| **out of scope** | The path/file was skipped, ignored, failed to parse, or exceeds limits | `coverage.complete: false`, `not_indexed: [{path, reason}]` |
| **stale** | The answer came from a generation older than the working tree | `read_status.stale: true`, `read_status.stale_reason` (`status` words the drift there as "N of M indexed files changed on disk since the index"), `read_status.decisive: false` |

The rule: **`total_relation: "eq"` is only permitted when all four hold** — coverage complete, no unresolved candidates in the queried files, generation fresh, no truncation. Anything else must be `gte`, and `summary` must say why in words (`"no callers in 530 indexed files; 321 call sites unresolved"`).

This is the design decision that replaces LSP-grade precision: we do not need to resolve every call, we need to *tell the truth about which ones we didn't*.

### 6.1 Storage consequence

Because absence is a first-class output, unresolved call sites are **stored, not dropped**:

- `edges` holds only **resolved** edges, id-keyed with `ON DELETE CASCADE` (CBM's model — no dangling text endpoints to repair)
- `unresolved` holds `(file, line, raw_target, evidence_kind, evidence_value)` per unbound site

That split is what lets `trace` answer "0 with certainty" vs "0 but 321 unresolved" without the whole resolver/tier machinery CRG needs.

### 6.2 Where the census lives

A bare unresolved count is not actionable — most rows in a real graph are a stdlib call the indexer will never bind, not a caller it missed. So every answer that carries an unresolved count also carries its **buckets**, and the bucket set is closed: `builtin_stdlib`, `external_package`, `internal_module`, `dynamic_property_call`, `receiver_type_unknown`, `ambiguous` (declared in `src/shared/domain/unresolved.ts`, in that order, zeros included).

The census sits in the **payload** — `payload.unresolved` on `status`, `summary` and `impact`, `payload.unresolved_census` on `trace` — not in `coverage`. The envelope's `coverage` object in §5 does not move: same fields, same meaning, `unresolved_edges` still the scope's count. The census is the explanation of that one number, so `total` is copied from `coverage.unresolved_edges` rather than computed a second time: two counts of the same rows cannot disagree, and a reader never has to decide which one to trust.

`material` is the narrower population, computed from its own predicate rather than read off a bucket name — `evidenceKind === "ambiguous"` (two candidates are indistinguishable, so user code can be one of them) or bucket `receiver_type_unknown`. A broken relative import is a defect, not a hidden caller, and `dynamic_property_call` is non-material by construction: the extractor records it exactly when no declared target exists. Adding buckets, or moving rows between them, never moves the total, and none of it changes the `eq`/`gte` rule — `emit()` still owns truncation and still downgrades `eq` to `gte` itself, so a census can explain a downgrade but never cause one.

---

## 7. Two cross-cutting contracts

### 7.1 `emit(payload, budget)` — one budget chokepoint

**Never** hand-tune caps per tool. CRG hand-tuned 30 of them and breached its own pinned table three times in a real run:

| Tool | Pinned worst case | Actual |
|---|---|---|
| `get_affected_flows` | 40 000 tok | **275 097 bytes (~69 k tok)** |
| `get_impact_radius` | 50 000 tok | 205 687 bytes (~51 k tok) |
| `get_review_context` | 50 000 tok | 173 951 bytes (~43 k tok) |

Root cause was per-item caps with no serialised-size check, and one item (a 153 KB flow) consuming the whole allowance.

Rules:

1. Build the full payload in memory, serialise, measure.
2. If over `budget`, drop the **lowest-ranked** items first, re-serialise, repeat.
3. Preserve `totals` and `total_relation` through truncation.
4. Every truncation emits `next` (a cursor) — never a silent cut.
5. Budget accounting is bytes/tokens with a declared ratio, checked in CI against a pinned table *and* against a worst-case fixture, not just unit-tested per tool.

### 7.2 `expand(cursor)` — the pressure valve

The cursor exists so the six main tools can stay small. It replaces `detail_level: minimal|compact|standard`, per-tool `max_*` knobs, and `include_source`/`include_neighbors` flags.

Cursor properties:

- **Opaque** and signed; carries the generation id, the tool name, a hash of the original arguments, and the pagination position.
- **Bound**: if the generation advanced or the arguments differ, the cursor is refused with `error.code = "cursor_expired"` and `hint = "re-run the original call"`. (CBM's `next` cursor requires "all other arguments identical" — the same rule, enforced structurally.)
- **Server-side resumable**: no client-side state, no re-execution of the whole query where avoidable; the cursor addresses a stable ordering.
- **Non-recursive**: `expand` never returns another `expand` of an `expand` beyond a bounded depth; deep work must re-enter through the original tool.

---

## 8. Resolution, ambiguity, and confidence

`trace` and `search` both accept free text. The failure mode to design out: **a confident answer about the wrong node.**

| Situation | Behaviour |
|---|---|
| 1 exact match | `status: ok`, answer |
| Several plausible matches | `status: ambiguous`, ≤5 candidates with `{qn, kind, file, lines, score, why}` — **no answer body** |
| 0 matches | `status: not_found`, plus `near_misses` and the canonical coverage envelope |

Additional rules:

- **`kind` is inferred, not required.** The tool decides from the shape of `query` (path → file; `foo.bar` → dotted symbol; `@name` → decorator filter).
- **`@decorator` is a filter, not a node kind.** Decorators are properties, not identity — in both parents they live in `extra`/`properties`, and CBM has a `Decorator` label that is not what "who uses @foo" means. Document the semantics; do not invent a node kind.
- **Locals are low-recall and must say so.** "Callers of a variable" is mostly empty by construction: CRG only models Liquid `Variable` nodes, CBM has a `Variable` label but no call edges through locals. `trace` on a local returns `status: ok` with `coverage.note: "variable references, not call edges"`.
- **Confidence tiers on every edge**, surfaced in the payload, never averaged away: `extracted` (the source names it and the node exists) · `inferred` (import/path evidence) · `ambiguous` (several nodes fit) · `unresolved` (no node has that name). Tier names and scores follow CRG's, because they read well and were already documented.
- **`path` narrows a resolution; it never widens one.** A repo-relative prefix, applied to every candidate a resolution step returns and to the near misses, *before* the ≤5 cap — so a filtered query answers from inside the prefix or says it did not find the name. The same prefix rule serves `trace`, `search` and `expand` (one implementation, `src/shared/domain/resolve.ts`). It cannot be pushed into a store lookup, so each step filters the rows it already got back; that is also why a near miss from outside the prefix can never leak into the answer.

### 8.1 One definition of a candidate count

An ambiguous answer carries two counts, and neither is recomputed by a tool:

| field | means |
|---|---|
| `candidates_total` | the size of the tie **before** the ≤5 cap — what actually matched |
| `candidates_shown` | how many candidates this payload carries — what the client can address |

Both come from the resolution itself (`Resolution`'s ambiguous branch), and `candidates_shown` is the candidate array's own length, so the numbers cannot drift between tools. `totals` carries the pair as well as the candidates payload, because "how large was the tie" and "how much of it did you send" are different facts and a single `<list>_total` would force one of them to lie. The prose is built from the same pair: `trace`'s summary says how many matched (`7 candidates matched, none preferred`), `expand`'s hint adds how much was served (`matches 7 nodes, showing 5`) — so a capped answer is readable without diffing arrays.

### 8.2 The change set

The change set (`ChangeSet` in `src/shared/domain/diff.ts`) keeps its two flat arrays — `changed` and `removed`, sorted repo-relative paths — and adds `entries`, one typed view over the same facts:

```ts
entries: { state: "modified" | "added" | "deleted" | "renamed"; path: string; from?: string }[]
```

`entries` is derived from `changed ∪ removed`, never from a third source, so a path appears in both views or in neither. `from` exists only on `renamed`: git's two halves of a rename are one fact, and a payload that rendered them as "one added, one deleted" would lose it; a copy is `added`, because its source still exists. The flat arrays stay because they are what a caller filters and rejoins by — `impact`'s `paths` argument takes that file list back — and a path list should not require reading a state machine.

---

## 9. Language layer

**Mechanism: CRG's, not CBM's.** Reasoning: CBM's 164 vendored grammars cost a 319 MB binary and a ~7 minute build; CRG gets ~43 languages from `tree-sitter-language-pack` with a pip install. Telemetry shows no demand that justified CBM's extra languages.

### 9.1 Structure (copied from CRG)

| Piece | Purpose |
|---|---|
| extension → language map | one literal table |
| `class_types`, `function_types`, `import_types`, `call_types` | four per-language node-type lists — this is the node-vs-edge decision, expressed as data |
| shebang sniff | extension-less files only (scripts, git hooks) |
| a handful of hand-written readers | for formats a tree-sitter walk cannot serve: notebooks, YAML paths, JSON paths, HTML inline scripts, templates |
| `.sangkar/languages.toml` | user-defined languages; **built-ins always win**; capped count; validated |

Node-vs-edge stays a table lookup, for the same reason both parents did it that way: it is the maintainable form of "what becomes a node."

### 9.2 Recall honesty (the part CRG got wrong)

CRG silently skips a file when its grammar is unavailable (observed: `xslt`, `liquid` → `LookupError`, file gets no symbols but still appears in the language census). This design surfaces it:

- a missing/unloadable grammar → `coverage.not_indexed += {path, reason: "grammar_unavailable", language}`
- a parse with ERROR/MISSING regions → `coverage.failed += {path, ranges}` (CBM's `parse_partial` idea, which the user can act on)
- the language census reports `{language, files, parsed, failed}` — never a bare list

### 9.3 Deliberately deferred

- **Cross-file type resolution / LSP.** Not needed now. The door stays open: `edges.confidence` already distinguishes `inferred` from `extracted`, and an LSP layer would only *promote* tiers, not change the schema. No design decision here blocks it.
- **Vendored grammars.** If a language matters and the pack lacks it, vendor it — as an explicit, measured addition, not as a starting assumption.

---

## 10. Indexing lifecycle

### 10.1 Bind the repo once

The server binds `repo_root` at startup from the working directory (overridable by flag). **No tool takes a `project` argument.** CBM spent 18 of 145 calls (12.4 %) on `list_projects` because its project identity is an unmemorable slug derived from the absolute path
(`home-…-<repo>`), so the agent must re-discover it instead of remembering it.

Identity: short name = repo basename + 4-char hash of the absolute path.

**The topology is a sidecar: one daemon per machine, one thin server per session.** The daemon (`sangkar daemon run`) is the only process that holds the registry (`$SANGKAR_HOME/sangkar.json`), the only one that watches anything — exactly one watcher per registered, watchable repository — and the only one that serves the usage report. It never speaks MCP and never opens a graph. Each MCP session is still a `sangkar mcp` stdio server, but a thin one: it reads the registry once at start, answers for any of the N registered repositories from a pooled reader keyed by canonical root, and starts no watcher of its own.

Three reasons:

1. **No duplicate pollers.** An agent working in two repositories needs two sessions, so one process per tree gave every session a poller over its own tree, and a third one the moment anything else watched that tree. In the sidecar shape the number of pollers follows the number of registered repositories, not the number of sessions.
2. **One policy file per repo instead of an env var.** Whether a tree is watched, how often, and whether its usage may be recorded were process settings, so two trees with two preferences needed two launchers and the preference died with the session. They are now `<repo>/.sangkar/config.json` — `watch`, `watch_interval`, `telemetry` — hand-editable, outliving any process, and re-read by the daemon on every reconcile tick, over the installation's own defaults in `<home>/config.json` beside the registry.
3. **One machine-wide report.** Usage records stay in each repository's `.sangkar/telemetry/`, so a report rendered by a process could only ever describe the one tree that process was bound to. The daemon serves one loopback page that covers every registered repository instead.

Compatibility is one selector wide. Every tool takes an optional `repo`: a path, or a registered basename that is unique. It is a registry key, not CBM's project slug, and a call that names no repository keeps the earlier behaviour exactly — the `--repo-root` pinned at start, else the working directory when it is registered, else the only registered tree. The cursor carries the repository it was minted in, so a page minted in one tree is refused `cursor_mismatch` in another even though a single process can serve both.

### 10.2 Bootstrap lazily, never block

| Trigger | Behaviour |
|---|---|
| server start, index missing | do **not** index inline; first tool call returns `status: "bootstrapping"` + `retry_after_ms` (CBM does this for architecture jobs; the agent retries instead of dying on a client deadline) |
| first tool call, index stale | the handle re-opens when the file on disk is a newer generation and answers from it; a tree that is stale but unchanged answers from the current generation with `stale: true`, because the watcher's first poll only records a baseline and spawns a run on a later change |
| `index` (repair) | forced/repair path; call only when `status` reports a broken graph |
| file watcher / VCS hook | coalesced debounce → incremental |

A re-open that cannot be served — a newer schema, another tree's graph, a torn file — leaves the previous reader in place and keeps answering with `stale_reason: "a newer generation could not be opened"`, so a publish this build cannot read degrades to a disclosed staleness instead of an error the session cannot recover from. A vanished file is not a newer generation: an absent store stays absent and a ready one keeps serving what it already read.

### 10.3 Publish protocol (copied from CBM, because it is what survived concurrency)

1. Write into a **staging** database.
2. Seal it: checkpoint WAL, `journal_mode=DELETE`, `synchronous=FULL`, integrity check.
3. `rename()` over the live file; readers keep their open handle on the previous generation.
4. Discard the stage on any failure or cancellation — **never publish a partial graph**.

CRG's in-place `BEGIN IMMEDIATE` + `flock` model produced the 12.9 % build error rate. Do not copy it.

### 10.4 Freshness model

- Authoritative signal: **content hash per file** (not mtime). Both parents agree; CRG re-reads every indexed file and compares SHA-256.
- `generation`: monotonic id, bumped on publish, carried in every response and every cursor.
- Staleness check per query is cheap: compare the file set's hashes against the stored manifest. If it is expensive on large repos, run it in the background and expose `stale_checked_at` in `status` so the agent knows how fresh the freshness check itself is.

---

## 11. Graph model

Pragmatic subset, taking the better half of each parent.

**Nodes (v1):** `project`, `folder`, `file`, `module`, `class`, `function`, `method`, `test`, `import_alias`, plus exactly one synthesised kind: `endpoint` (routes). Optional later: `config_key`, `topic`.

Dropped from parents: `struct/trait/enum/type/field/constant/macro/decorator` as separate labels (they become `class`/`function` with `subkind` in properties), `section`/`doc_section`/`yaml_path`/`json_path` (out of scope for v1), `signal` (display-only in CRG).

**Edges (v1, resolved only):** `contains`, `defines`, `imports`, `calls`, `references`, `inherits`, `implements`, `tested_by`.

Dropped: `similar_to`/`semantically_related` (≤10/node caps in CBM, never queried in telemetry), `co_cha ged` (needs git history mining), all `cross_*` (no registry), `deploys`/`configures`/`infra_maps` (infra modelling is a separate product).

**Properties / columns, never nodes:** `params`, `return_type`, `signature`, `docstring`, `modifiers`, decorator list, import paths, config values. This is the rule both parents follow and it is right: a parameter has no independent identity.

**Qualified name:** `<repo-relative-path>::<scope chain>.<name>` — readable, stable across machines, and not an absolute path (CRG used absolute paths, which leak the developer's home directory in every payload).

**Cross-file resolution in v1:** same-file, import-based, and inheritance-based only. Everything else lands in `unresolved` (§6).

---

## 12. Storage

One SQLite file, one schema, forward-only migrations.

| Decision | Choice | Why |
|---|---|---|
| Location | `~/.cache/<name>/<shortname>-<hash>/graph.db`, `--data-dir` override | keeps repos clean; CRG's in-repo dir needs gitignore juggling and gets committed by mistake |
| Journal | WAL, readers use immutable read-only handles | both parents; enables the staged publish |
| Keying | integer ids + FK `ON DELETE CASCADE` | CBM's model; no dangling text endpoints to repair |
| Absence | separate `unresolved` table | §6 — the whole point |
| Full text | FTS5 external-content + a state mirror | CRG's pattern; avoids re-running the tokeniser to delete |
| Embeddings | optional table, off by default, no ANN | never a prerequisite for `search` |
| Schema version | `metadata` row, forward-only, no down-migrations | CRG's; simple and honest |
| Size control | report it, do not silently enforce it | CRG reached 5.36 GB; surfacing beats surprising the user |

---

## 13. The instructions layer (not optional)

The single largest adoption lever in the measured data was not a tool — it was the prompt text. CRG's five prompts produced 43 % of its call volume by mandating a first call; CBM shipped two prompts its installer never surfaced and landed at **3.9 % session adoption** (40 of 1 034 sessions; 2 of the last 100).

So this project treats instructions as a deliverable, shipped with the server:

- **A one-screen "how to use me" text** installed into the agent's rules file, with the intended chains:
  - start of a task → `summary(scope="change")`
  - unfamiliar area → `summary(scope="area", target=path)`
  - "where is X / who uses X" → `trace(query)`
  - "what breaks if I change this" → `impact`
  - detail → `expand(next)`
- **≤3 graph calls per turn** guidance, and an explicit "do not call `trace` to read source — use your file reader" rule. (Both `get_code_snippet` and `get_file_outline` got near-zero calls where measured; the agent reads files itself.)
- **Anti-pattern list**: do not treat a `gte` total as "none"; do not call `index`; do not pass `project`.

---

## 14. Acceptance criteria (measurable, from this telemetry)

| Criterion | Target | Derived from |
|---|---|---|
| Tools with zero calls after 30 days | **0** | 18 of 47 tools had zero calls |
| Invalid tool-name calls | **0** | 174 attempts / 166 errors |
| p95 payload ≤ declared budget + 10 % | enforced in CI on a worst-case fixture | 3 of 30 CRG tools breached |
| Error rate | **< 2 %** | CRG 9.5 %, CBM build 12.9 % |
| Absence claims carrying a coverage envelope | **100 %** | design invariant (§5) |
| `total_relation: "eq"` on incomplete coverage | **0 occurrences** | design invariant (§6) |
| Sessions per week using at least one graph tool | > 50 % of graph-relevant sessions | CBM 3.9 % |

---

## 15. Open questions (decide before implementation)

1. **Name.** Working title only.
2. **Scope of v1 languages.** CRG's mechanism reaches ~43; do we ship the full pack or a curated 12 (python/ts/tsx/js/go/java/rust/c/cpp/cs/php/ruby/kotlin) and let `languages.toml` cover the rest?
3. **`impact` vs `summary(scope=change)`.** Keep both (observed ratio 4.4:1 suggests one is enough) or one with `expand`?
4. **Where does `base` come from?** Last indexed commit (cheap, can be stale) vs live `git merge-base` (accurate, needs git at query time, and CRG's discovery timeouts were a real problem).
5. **Do we ship a daemon?** A watcher process is the only way auto-update is real, but it doubles the surface (IPC, lifecycle, one-writer enforcement). Alternative: VCS hooks only, accepting staleness between commits.
6. **Semantic search in v1?** FTS5 is enough for the measured queries; embeddings add a dependency and a rebuild cost. Tentative answer: BM25 in v1, embeddings behind a flag.
7. **Diagram/docs indexing** (CBM's differentiator): out of v1, or an `area`-scope extra?

---

## 16. Milestones (shape only — no tasks until a PPM plan is authorized)

| M | Outcome | Proves |
|---|---|---|
| M0 | skeleton: bind repo, `status`, `index`, one language, FTS5, envelope | the contracts compile |
| M1 | 12 languages, node/edge tables, containment + defines + imports | the model holds |
| M2 | `trace` with resolve/ambiguity/**absence** | the main feature |
| M3 | `summary(change)`, `impact`, `expand`, `emit` | the hot path is cheap |
| M4 | watcher/hooks, staged publish, generation, staleness | auto-update is real |
| M5 | instructions + one seven-tool surface (no profiles); rename pass on any `*_tool` leftovers | adoption |
| M6 | evaluation harness: both parent repos as fixtures, telemetry-style counters, absence-claim audit | the acceptance table in §14 |

---

## Appendix: decision provenance

| Decision | Came from | Evidence in this folder |
|---|---|---|
| Six tools, bare verbs | both parents' zero-call sets | `TOOL-USAGE-ANALYSIS.md` §1–2, §5 |
| Trust as a first-class objective | CBM 51.7 % of calls | `TOOL-USAGE-ANALYSIS.md` §1 |
| Patterns not Cypher | 0 vs 884 calls | `TOOL-USAGE-ANALYSIS.md` §3 |
| Instructions layer is a deliverable | 43 % vs 3.9 % adoption | `TOOL-USAGE-ANALYSIS.md` §4 |
| Single budget chokepoint | 3 of 30 CRG tools breached | `TOOL-OUTPUTS.md` findings table |
| Stage → seal → rename publish | CRG build 12.9 % error | `STORAGE-ARCHITECTURE.md` §2.3, `TOOL-USAGE-ANALYSIS.md` §7 |
| id-keyed edges + `unresolved` table | CRG's text endpoints need a whole tier machinery; CBM's candidates table is the honest form | `GRAPH-MODEL.md` §7–8 |
| CRG's language mechanism | 319 MB / 7 min vs pip install; no demand for breadth | `STORAGE-ARCHITECTURE.md` §2.1, `INDEXING-PIPELINE.md` §2.2 |
| Decorators are properties, not kinds | both parents store them in `extra`/`properties` | `GRAPH-MODEL.md` §3–4 |
| Repo bound at startup, no `project` arg | CBM 18 `list_projects` calls | `TOOL-USAGE-ANALYSIS.md` §1 |
