## Objective

Ship a working local MCP server (`app/`, Bun + TypeScript) that gives a coding agent four answers it
measurably needs — **is the graph current and what does it cover**, **what is connected to X**, **what is
going on here**, **what can this change reach** — through **6 agent tools + 1 admin tool**, with a graph
built from 12 languages and published atomically.

The design exists because the two reference servers fail in measured ways: 47 % of each tool surface had
zero calls, CRG's `build` tool errored 12.9 % of the time, CBM's free-form Cypher tool got 0 calls, and
CRG breached its own pinned payload budget 3 times on a 530-file repo.

## Scope

**In:**
- `status`, `trace`, `summary`, `search`, `impact`, `expand` (agent profile) + `index` (admin profile)
- indexing pipeline for 12 languages with the language layer expressed as data
- staging publish (`stage → seal → rename`), generation ids, content-hash staleness
- **absence semantics**: `eq` vs `gte`, `unresolved` and `not_indexed` surfaced in every response
- one size chokepoint (`emit`) and one cursor mechanism (`expand`)
- the instructions text that tells an agent how to use the six tools

**Out:**
- cross-file type resolution / LSP, free-form query language, communities/hubs/bridges/wiki/refactor,
  cross-repo search, ADR, runtime trace ingestion, embeddings as a requirement
- multi-repo (one server = one repo), HTTP transport, CUI/dashboard

## Sources

- `DESIGN.md` — objectives, tools, contracts, milestones §14 acceptance criteria
- `TOOLS-SPEC.md` — names, descriptions, params, payload shapes, per-field rationale
- `DATABASE.md` — tables, index plan, publish protocol, absence implementation, size budget
- `TECH-SPEC.md` — runtime baseline, module isolation, Drizzle usage, parser strategy, spikes
- `TOOL-USAGE-ANALYSIS.md`, `TOOL-OUTPUTS.md`, `GRAPH-MODEL.md` — the measurements behind each decision

## Done when

1. `cd app && bun run typecheck && bun run lint && bun test` all pass.
2. `bun run index -- <reference corpus>` publishes a generation; `status` reports `complete: true` for the
   indexed paths and lists nothing spurious.
3. All seven tools are callable over MCP stdio (`initialize` → `tools/list` returns 7 → each `tools/call`
   returns the 9-field envelope). `tools/list` on the `agent` profile returns exactly 6.
4. On the reference corpus: node count per language is within 5 % of CRG's measured counts, `trace` p95
   < 40 ms, `search` p95 < 60 ms, `summary(change)` p95 < 120 ms.
5. Absence invariants hold as tests: `total_relation: "eq"` appears only with complete coverage, no
   unresolved call sites in scope, and a fresh generation.
6. `tests/boundary.test.ts` fails when a tool imports another tool, and passes otherwise.
7. Every payload is within its declared budget + 10 %.

Final verification command:

```bash
cd app && bun run typecheck && bun test && bun run acceptance
```
