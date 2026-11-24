# Objective

Move the whole product from TypeScript-on-Bun to Rust with **semantic** output parity, keeping one shared
`graph.db` readable and writable by either implementation for the length of the migration, so that at no
point is there a half-migrated artifact in a user's hands.

Why: the shipped Bun package requires Bun ≥ 1.4.4 on the user's machine and the compiled alternative
measures 104–160 MB, because Bun and 12 WASM grammars are embedded (`docs/maintainers.md` § Distribution,
`docs/my-product/TECH-SPEC.md` §15). A native Rust binary replaces the interpreter *and* the grammar
payload, and removes two coupling points the code documents as failures: the `drizzle-orm` private-field
read in `src/indexer/persist.ts:234-245` and the `bun:sqlite` statement-close ordering in
`src/shared/store/publish.ts:20-30`.

# Scope

**In**

- A `rust/` Cargo workspace of nine crates whose dependency DAG enforces the six module rules of
  `.agents/prompts/rules.md` (`docs/my-product/RUST-MIGRATION.md` §4).
- Ports of every module under `src/shared/**`, `src/indexer/**`, `src/daemon/**`, `src/tools/**`,
  `src/index.ts` and `bin/sangkar.ts`, in six phases ending in verifiable gates.
- Both surfaces: the MCP stdio server and the `sangkar` CLI, with the same verbs, arguments, exit codes
  and stream split.
- The parity harness, checked-in polyglot/broken/empty fixture repositories, and captured goldens that
  replace the TypeScript suite at cutover.
- npm distribution as one platform package plus a Node shim, so `npm i -g sangkar-mcp` keeps working on
  Linux x64 with no Bun, and the maintainer docs/skill that change with it. macOS and Windows stay on the
  last TypeScript release until their platform is added — a stated consequence, not an oversight.

**Out**

- Any schema change, payload change, manifest change, tool rename, new language, new argument, or budget
  change. `docs/my-product/{TOOLS-SPEC,DATABASE,TELEMETRY}.md` must need **no** edit; a task that finds
  itself editing one has broken the parity bar and stops.
- A user-facing engine switch (`--engine=bun|rust`). The two binaries are development artifacts; the npm
  package flips once, in Phase 5.
- `notify`-based watching, an async database layer, `git2`, LSP, embeddings, or any redesign of the seven
  tools.
- Publishing a beta of the Rust build before Phase 5. The TypeScript artifact stays the published one until
  then.

# Sources

- `docs/my-product/RUST-MIGRATION.md` — the design record this plan executes (parity bar §3, crate map §4,
  dependency pins §5, strangler mechanics §6, phases §7, risks §8).
- `docs/my-product/{TOOLS-SPEC,DATABASE,TECH-SPEC,TELEMETRY,INDEXING-PIPELINE}.md` — the contracts a port
  must reproduce.
- `.agents/prompts/rules.md` — the six module rules, the stdout discipline, the publish protocol, the
  eleven-tables-one-ledger rule.
- `.agents/prompts/project-layout.md` — one line per file, i.e. the port's file-by-file index.
- `docs/research/tool-outputs/` — 49 real MCP payloads; shape oracles that survive the migration.

# Done when

1. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and
   `cargo test --workspace` are green from a clean checkout, and the crate DAG test in
   `rust/crates/sangkar-boundary` rejects a forbidden edge (proven by a deliberate temporary edit in its own
   test).
2. For all seven tools × three fixtures, the Rust envelope equals the captured TypeScript golden:
   identical key sets at every depth, identical primitive values, identical array order and length,
   identical `status`/`error.code`/exit code; JSON rendering byte-identical; TOON rendering decodes to the
   same value through `@toon-format/toon` 4.4.0.
3. A repository indexed by the Rust binary is served correctly by the **TypeScript** reader, and the
   per-language census (`nodes`, `edges` by kind and tier, `unresolved` by kind, files, skip reasons)
   matches the TypeScript index of the same corpus, with any remaining delta measured and written into
   `.agents/prompts/gaps-n-future-works.md` rather than hidden.
4. `sangkar daemon run|start|stop|status` and `sangkar telemetry --out|--serve` behave as
   `tests/daemon-*.test.ts` and `tests/telemetry-*.test.ts` describe, against the same `<home>` layout.
5. `npm pack` tarball on **Linux x64** (the first Rust release's only target): `sangkar status|trace|search|impact|summary|expand|
   index` answers the goldens, `sangkar mcp` completes an `initialize` → `tools/list` → `tools/call`
   session, `sangkar index` builds a graph, and no interpreter is required beyond Node for the shim.
6. `src/`, `tests/`, `bin/`, `biome.json`, `drizzle.config.ts`, `bunfig.toml` and the five TypeScript
   dependency pins are deleted; `git grep -l 'bun:sqlite\|drizzle-orm\|web-tree-sitter\|@toon-format'` is
   empty; `AGENTS.md` and `.agents/prompts/*` describe the Rust tree as the only tree.
