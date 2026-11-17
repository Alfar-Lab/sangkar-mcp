# Approach

## Current state (verified)

- No product code exists. The repository holds only design documentation (8 markdown files, ~1.4 MB with
  captured payloads) plus two ignored reference clones.
- Runtime capabilities were probed on this machine and recorded in `TECH-SPEC.md` §1: `bun:sqlite` is
  **SQLite 3.53.4 with FTS5**, JSON1 and `VACUUM INTO` available; URI/immutable filenames are **not**
  supported; `package.json#imports` **is** enforced by the resolver; `Bun.Transpiler.scan()` gives
  structured imports without executing.
- `drizzle-orm@0.45.4` + `drizzle-kit@0.31.11` work on `bun:sqlite`; `db.transaction(cb, {behavior:"immediate"})`
  exists; `drizzle-kit generate` emits SQLite migrations; `getTableConfig` lives in `drizzle-orm/sqlite-core`.
- Measured query cost that shapes the store layer: raw prepared 0.69 µs, reused Drizzle prepared 1.7 µs,
  re-built Drizzle builder **42.7 µs** per read on the same indexed table.
- The reference corpus for parity is `code-review-graph/` (530 files), where CRG measured 13 955 nodes /
  130 044 edges and CBM 17 230 / 87 766 on the same tree; the by-kind oracle those captures record is
  File 530, Class 877, Function 5 197, Test 6 351, and CBM recorded per-language **file** counts
  (python 408, typescript 28, yaml 24 …). No capture records per-language node counts, so TASK-019
  asserts only oracles that exist.
- `tree-sitter-language-pack` (CRG's parser) is PyPI-only; a WASM stack (`web-tree-sitter@0.27.2` +
  `tree-sitter-wasms@0.1.13`) replaces it.

## Design

1. **Contracts before behavior.** `shared/` ships config, logging, error codes, the nine-field envelope,
   the single `emit()` budget chokepoint, signed cursors, the Drizzle schema, the migration runner, the
   publish protocol and typed read primitives — each with tests — before any tool or extractor is written.
   Rationale: the two contracts that failed in the reference servers (payload size, absence honesty) are
   cheapest to get right once, centrally.
2. **The language layer is data.** An extension map plus five node-type lists per language (class,
   function, import, call, inheritance) drive one generic extractor. Adding a language is a table row, not a
   code path.
3. **The indexer is a separate process** that owns the writer. Tools only ever read a published generation.
   Rationale: a WASM parser panic must not kill the MCP session, and it keeps `tools/` unable to depend on
   the writer.
4. **Publishing replaces a file, not rows.** `stage → seal → rename` makes a partial graph unpublishable and
   lets readers keep serving the previous generation. This is the only publish model that survived
   concurrent sessions in the reference implementations.
5. **Absence is a first-class output.** `unresolved` and `not_indexed` are tables, `total_relation` is
   *derived* from them, and every response carries a coverage envelope. The design intentionally trades
   LSP-grade precision for honest disclosure of what was not resolved.
6. **Drizzle for types and migrations, raw SQL for the hot three.** Prepared statements only; the hottest
   primitives may bypass Drizzle inside `store/read/**` without any caller noticing.
7. **Six agent tools, one admin tool.** No free-form query language, no analytics suite, no wiki. The
   measured zero-call sets are excluded by construction.

### phase_0: foundation and contracts

Deliverable: an `app/` that typechecks, lints, has the module boundary enforced in CI, a migrated empty
database, the publish protocol, typed read primitives and the domain functions (resolve, neighbors,
coverage, risk, diff) — each proven by tests, with no tool and no extractor yet.

Barrier because: every later task consumes these signatures (`Envelope`, `emit`, `GraphReader`,
`Coverage`, `Cursor`). Changing them after seven tools exist is seven rewrites.

### phase_1: indexer

Deliverable: a real generation published from the reference corpus — discovery, 12-language extraction
driven by the node-type tables, same-file/import/inheritance binding, the inheritance and reference edges
the payloads group by, `tested_by` derivation, metrics, FTS, tombstones, and the worker entry point — plus
the parser acceptance evidence (totals and by-kind counts against the recorded oracles, per-language file
counts, p95 parse time, peak RSS).

Barrier because: every tool's integration test needs a real indexed database, and the absence envelope
cannot be verified without real `unresolved` rows.

### phase_2: tools

Deliverable: the seven tool modules, each a private module with manifest, handler and handler test against
a fake `GraphReader`; the two modules the composition root imports before registration (the watcher and the
instructions/`nextToolsFor` module); and registration with the `agent`/`admin` profiles, watcher startup,
signal handlers and `next_tools` wiring.

Barrier because: the tool set must exist and be registered before packaging and acceptance can run. The
watcher and instructions live in this phase rather than productization because `src/index.ts` imports them
statically — a task cannot register a root that imports files created a phase later.

### phase_3: productization

Deliverable: freshness and health reporting on `status`, packaging (README + a binary smoke that performs a
real `tools/call`), and the acceptance gate that encodes `DESIGN.md` §14 plus spikes S4 and S6 as tests.

Barrier because: these observe the finished surface and the acceptance gate must run last.
