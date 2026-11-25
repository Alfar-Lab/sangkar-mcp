# AGENTS.md

Guidance for an agent working **in** this repository. `README.md` is for users; this file routes to the
prompts under `.agents/prompts/`, which are for whoever edits the code.

## What this is

`sangkar-mcp` indexes one repository into a SQLite call graph and serves **seven read-only tools** over two
surfaces with identical behaviour: an MCP server (`sangkar mcp`, stdio) and a shell command
(`sangkar trace <name>`). Four answers: *is the graph current and what does it cover*, *what is connected to
X*, *what is going on here*, *what can this change reach*.

It is one native binary: the Cargo workspace under `rust/`, whose `sangkar-cli` crate is the `sangkar` command
and whose `main.rs` is the entry point. The npm package is a launcher (`bin/sangkar.js`, Node ≥ 18) plus one
platform package; the server, the indexer, the daemon and the report are that binary, with no interpreter in
the path.

A call answers from one repository: `repo` selects among the registered ones, and a call that names
none falls back to `--repo-root` or the tree autodetected from `cwd` — so no tool takes a `project`
argument.

## Where to look

| File | Holds |
|---|---|
| [`.agents/prompts/rules.md`](.agents/prompts/rules.md) | The six module rules `cargo test -p sangkar-boundary` enforces — the crate DAG plus the `rusqlite`, stdout, tool-isolation and marker scans — the invariants that break the product when violated, the coding conventions, and the rule that these files move with the change they describe. **Read this before your first edit.** |
| [`.agents/prompts/project-layout.md`](.agents/prompts/project-layout.md) | The crate tree with one line per crate and module, the recipes (add a language, a tool, a payload field, a telemetry field, a table), and the data model in one screen. |
| [`.agents/prompts/run-build.md`](.agents/prompts/run-build.md) | How to build and run the binary on this machine, the command table, and the definition of done. |
| [`.agents/prompts/test-map.md`](.agents/prompts/test-map.md) | The map from "what you changed" to "the tests that will tell you". |
| [`.agents/prompts/gaps-n-future-works.md`](.agents/prompts/gaps-n-future-works.md) | Plan state, the four measured gaps the suite does **not** catch (test/class/method kinds, binding recall, a nested `target/` the ignore rules cannot see), and the work left after them. |
| [`rust/`](rust/) | The implementation — a nine-crate Cargo workspace (`core`, `store`, `domain`, `tools`, `index`, `telemetry`, `daemon`, `cli`, `boundary`), and the only tree that ships. [`RUST-MIGRATION.md`](docs/my-product/RUST-MIGRATION.md) records how the port was cut over (plan: `.ppm/rust-migration/`). |
| [`docs/`](docs/) | The design record — read before changing behaviour. |

## Docs

- `docs/my-product/` — the **product design record**: `DESIGN.md` (objectives, contracts, absence
  semantics, acceptance), `TELEMETRY.md` (record schemas, the privacy prohibition, the verb, the report),
  `TOOLS-SPEC.md` (per-tool params, payload keys, budgets), `DATABASE.md` (tables, index plan, publish
  protocol), `TECH-SPEC.md` (runtime shape and the crate module system, and — in §2 and §5 — the
  TypeScript-era detail the port reproduced: Drizzle confinement, the parser strategy),
  `INDEXING-PIPELINE.md` (triggers, registration, the per-repo policy file), and
  `RUST-MIGRATION.md` (the decided Rust migration: parity bar, crate map, strangler mechanics, phases — plan
  in `.ppm/rust-migration/`).
- `docs/research/` — the **product-related research record**: the two reference implementations analysed
  from their own source (`TOOL-SURFACE.md`, `TOOL-USAGE-ANALYSIS.md`, `GRAPH-MODEL.md`,
  `STORAGE-ARCHITECTURE.md`, `INDEXING-PIPELINE.md`) and `tool-outputs/`, the real MCP payloads
  `TOOL-OUTPUTS.md` documents — the surface the seven tools were designed against.
- `docs/maintainers.md` — for whoever builds the artifact or cuts a release: distribution, the npm
  traps, the checks. It does not ship in the npm tarball; the procedure itself is the project skill
  `.agents/skills/build-n-deploy/SKILL.md`.
- `README.md` — for users, in a fixed shape: `## sangkar-mcp`, `## Installation & Requirements`,
  `## Usage` (`### MCP & Skill`, `### CLI`), `## License`. Maintainer material
  belongs in `docs/maintainers.md`, not in a new top-level README section.
- `rust/crates/sangkar-core/assets/INSTRUCTIONS.md` — the agent-facing text embedded as `INSTRUCTIONS_TEXT`;
  `rust/crates/sangkar-core/tests/instructions.rs` pins it byte for byte (4 087 bytes, 4 075 characters).

Changing behaviour that a doc specifies means changing the doc in the same commit.
