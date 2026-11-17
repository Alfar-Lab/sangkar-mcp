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
  130 044 edges and CBM 17 230 / 87 766 on the same tree. These are the oracles for TASK-020.
- `tree-sitter-language-pack` (CRG's parser) is PyPI-only; a WASM stack (`web-tree-sitter@0.27.2` +
  `tree-sitter-wasms@0.1.13`) replaces it.

## Design

1. **Contracts before behavior.** `shared/` ships config, logging, error codes, the nine-field envelope,
   the single `emit()` budget chokepoint, signed cursors, the Drizzle schema, the migration runner, the
   publish protocol and typed read primitives — each with tests — before any tool or extractor is written.
   Rationale: the two contracts that failed in the reference servers (payload size, absence honesty) are
   cheapest to get right once, centrally.
2. **The language layer is data.** An extension map plus four node-type lists per language (class,
   function, import, call) drive one generic extractor. Adding a language is a table row, not a code path.
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
driven by the node-type tables, same-file/import/inheritance binding, metrics, FTS, tombstones, and the
worker entry point — plus the parser acceptance evidence (node counts against the CRG oracle, p95 parse
time).

Barrier because: every tool's integration test needs a real indexed database, and the absence envelope
cannot be verified without real `unresolved` rows.

### phase_2: tools

Deliverable: the seven tool modules, each a private module with manifest, handler and handler test against
a fake `GraphReader`, plus composition-root registration with the `agent`/`admin` profiles.

Barrier because: the tool set must exist and be registered before packaging and acceptance can run; the
modules are independent of each other, so this phase is a single wide wave.

### phase_3: productization

Deliverable: the watcher and staleness/health reporting, the instructions text that makes an agent call the
six tools correctly, packaging (compiled binary + npm scripts), and the acceptance gate that encodes
`DESIGN.md` §14 as tests.

Barrier because: these all observe the finished surface (a tool can only be documented and packaged once it
exists) and the acceptance gate must run last.
