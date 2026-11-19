## Current state

Verified at `ed16170` by reading the code and probing the live store (`g-d975f0`); verdicts are posted on the issues.

- **Ambiguity counts disagree.** `src/shared/domain/resolve.ts:408` slices winners to `CANDIDATE_CAP` (5) before returning, while the `why` text is built from the full match set (`:176`, `total = nodes.length`). `src/tools/trace/handler.ts:991` then reports `candidates_total: resolution.candidates.length` — the sliced length. Live: `messageOf` → `candidates_total: 5`, every `why` "of 8 candidates", summary line "5 candidates matched".
- **No path filter on `trace` or `expand`.** `src/tools/trace/manifest.ts:60-67` and `src/tools/expand/manifest.ts:35-39` declare none; `search` has one (`src/tools/search/handler.ts:119` over `isUnderPrefix`). `src/shared/domain/resolve.ts` accepts only `kinds`.
- **No `next_call`.** Candidates are `{id, qn, kind, file, lines, score, why}`; `resolve.ts:44-53`.
- **Unresolved is one flat number.** `Coverage` (`src/shared/envelope.ts:56-63`) has only `unresolved_edges`, filled as `unresolved.length` (`src/shared/domain/coverage.ts:114`, `src/tools/status/handler.ts:153`). The store classifies the same rows: live `evidence_kind` = receiver_type_unknown 4570, dynamic 4411, ambiguous 185; `evidence` prefix = call 8593, import 541. `trace` flattens a row to `{file, line, name, evidence, tier}` (`src/tools/trace/handler.ts:519-527`) and no row has a NULL `evidence`, so `evidence_kind` never reaches a client.
- **No repository identity.** `meta` holds only `schema_version, generation, generation_seq, indexer_state, consecutive_failures`. `project_name`/`project_root` are read (`src/shared/store/read/reader.ts:392`) and never written, so `status.payload.project.root` is `null` and every process start logs `store.identity_unrecorded` (`src/index.ts:334`). `branch`/`remote` exist nowhere; `DATABASE.md:180` specifies the keys.
- **`expand` source window is fixed.** `SourcePayload` (`src/tools/expand/handler.ts:166-175`) carries `window: [startLine, endLine]` derived from the node's own extent, plus `source_drift`. `what` is `["members","edges","source","coverage"]`; no `range`, no `outline`.
- **No runtime preflight.** `bin/sangkar.ts` is 77 lines of shebang and argument dispatch. `engine.bun` is metadata only and Bun ignores it; a non-Bun or old-Bun invocation dies without a diagnostic.

## Design

1. **Path filtering belongs inside `resolve`, before the cap.** Filtering in a handler would make the `ok`/`ambiguous` verdict depend on presentation and would let the same query answer differently per tool. One signature: `resolve(source, query, { kinds?, path? })`, with `path` applied as `isUnderPrefix(node.file, path)` to every step's hits and to near misses.
2. **`resolve` owns the counts; handlers own the call.** The ambiguous variant gains `candidates_total` (pre-cap winners) and `candidates_shown` (returned), and every `why` is built from the same total. `next_call` is built by each handler because it is tool-shaped: `trace` echoes the caller's `direction`, `expand` does not.
3. **Bucket at read time, from rows the code already has, and keep the census in the payload.** `assemble()` already returns `unresolved: readonly UnresolvedRef[]` beside the coverage (`src/shared/domain/coverage.ts:65-69`), and `status` fetches the same rows, so each tool can call `bucketCensus(assembly.unresolved)` with no new query, no reader change and no schema change. The census goes in each tool's `payload.unresolved` (`{total, material, buckets}`) rather than into `Coverage`: adding required fields to `Coverage` would break five source literals and eight test literals, while `coverage.unresolved_edges` keeps its documented meaning and its role gating `total_relation: "eq"`. `total` and `unresolved_edges` are the same number from the same rows, so they cannot disagree.
4. **Material is a claim about possible user code.** `material = ambiguous + (receiver_type_unknown after the builtin allowlist)`. `dynamic_property_call` is non-material by construction: the extractor records a call as dynamic exactly when no declared target exists. `internal_module` and `external_package` imports are non-material.
5. **Identity is recorded by the publisher, read through `GraphMeta`, exposed in `status.payload`.** Not in the envelope: `read_status` is per-answer freshness, and `DESIGN.md` §6 keeps repository identity in `status`. Branch comes from parsing `.git/HEAD` (reusing the freshness reader, so no child process at query time); the remote URL comes from `git config --get remote.origin.url` at publish time, where the worker already spawns git.
6. **`range` and `outline` extend `expand` without touching the reader.** `members(parentId)` already returns children in source order, so `outline` needs no query; `range` clamps to the node's own extent and rejects `from > to` with the existing refusal vocabulary.
7. **The runtime guard cannot catch a missing interpreter, and does not pretend to.** A missing `bun` makes `env` fail before any line of our code runs, so the guard covers the two cases it can observe — a non-Bun runtime and a Bun older than 1.4.4 — and the docs say so instead of implying a cure.
8. **`unresolved_edges` stays.** It gates `total_relation: "eq"` and is a documented payload key; the buckets and the material count are added beside it rather than replacing it.

## Phases

### phase_0: foundation contracts

Deliverable: the shared contracts every tool will consume — `resolve`'s path-and-counts signature, the pure unresolved classifier and its census, `meta` identity capture with a `GraphMeta` that reads it, and the runtime guard as a tested pure function. Sync barrier because phase 1 handlers cannot be written against a contract that does not compile, and every one of those handlers imports one of these modules. The envelope's `Coverage` is deliberately untouched in this plan, so no zero-coverage literal and no coverage fixture moves.

### phase_1: tool surfaces

Deliverable: `trace` (path filter, honest totals, `next_call`, bucket filter), `expand` (path filter and `next_call`, then `range` and `outline`), `status` (identity and bucket headline), and `summary`/`impact` (material unresolved in totals). Each task owns its own tool's manifest, handler and test, so they run concurrently; the two `expand` tasks are serialized because they own the same three files. Sync barrier because phase 2 documents payload shapes that must exist first.

### phase_2: documents and close

Deliverable: `TOOLS-SPEC.md` describing the new arguments and payload keys, `DATABASE.md`/`DESIGN.md` describing the recorded identity and the coverage fields, and the full definition-of-done run with the acceptance assertions. Sync barrier because it is the gate: nothing ships until it passes.
