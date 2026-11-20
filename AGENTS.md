# AGENTS.md

Guidance for an agent working **in** this repository. `README.md` is for users; this file routes to the
prompts under `.agents/prompts/`, which are for whoever edits the code.

## What this is

`sangkar-mcp` indexes one repository into a SQLite call graph and serves **seven read-only tools** over two
surfaces with identical behaviour: an MCP server (`sangkar mcp`, stdio) and a shell command
(`sangkar trace <name>`). Four answers: *is the graph current and what does it cover*, *what is connected to
X*, *what is going on here*, *what can this change reach*.

A call answers from one repository: `repo` selects among the registered ones, and a call that names
none falls back to `--repo-root` or the tree autodetected from `cwd` — so no tool takes a `project`
argument.

## Where to look

| File | Holds |
|---|---|
| [`.agents/prompts/rules.md`](.agents/prompts/rules.md) | The six module rules enforced by `tests/boundary.test.ts`, the invariants that break the product when violated, the coding conventions, and the rule that these files move with the change they describe. **Read this before your first edit.** |
| [`.agents/prompts/project-layout.md`](.agents/prompts/project-layout.md) | The module tree with one line per file, the recipes (add a language, a tool, a payload field, a telemetry field, a table), and the data model in one screen. |
| [`.agents/prompts/run-build.md`](.agents/prompts/run-build.md) | How to run the CLI and the servers on this machine, the script table, and the definition of done. |
| [`.agents/prompts/test-map.md`](.agents/prompts/test-map.md) | The map from "what you changed" to "the tests that will tell you". |
| [`.agents/prompts/state-gaps.md`](.agents/prompts/state-gaps.md) | Plan state and the three measured gaps the suite does **not** catch (test/method kinds, binding recall). |
| [`docs/`](docs/) | The design record — read before changing behaviour. |

## Docs and plans

- `docs/my-product/` — the design record: `DESIGN.md` (objectives, contracts, absence semantics, acceptance),
  `TELEMETRY.md` (record schemas, the privacy prohibition, the verb, the report),
  `TOOLS-SPEC.md` (per-tool params, payload keys, budgets), `DATABASE.md` (tables, index plan, publish
  protocol), `TECH-SPEC.md` (runtime facts, module isolation, Drizzle, parser strategy), `GRAPH-MODEL.md`,
  `INDEXING-PIPELINE.md`, `STORAGE-ARCHITECTURE.md`.
- `docs/research/` — the two reference implementations' analysis and the real MCP payloads captured from them
  (`tool-outputs/`). Those numbers are the oracles the parity tests use.
- `docs/maintainers.md` — for whoever builds the artifact or cuts a release: distribution, the npm
  traps, the checks. It does not ship in the npm tarball; the procedure itself is the project skill
  `.agents/skills/build-n-deploy/SKILL.md`.
- `README.md` — for users, in a fixed shape: `## sangkar-mcp`, `## Installation & Requirements`,
  `## Usage` (`### MCP & Skill`, `### CLI`), `## License`, `## Future Works`. Maintainer material
  belongs in `docs/maintainers.md`, not in a new top-level README section.
- `src/prompts/INSTRUCTIONS.md` — the agent-facing text shipped as `INSTRUCTIONS_TEXT`; the suite pins
the two byte for byte.
- `.ppm/codegraph-mvp/`, `.ppm/cgraph-cli/`, `.ppm/sangkar-telemetry/` — completed plans;
  `.ppm/sangkar-daemon/` — in progress; task JSON is the execution record for each. Do not edit
  `.ppm/*/tasks/*.json` by hand.
- `.ppm/issue-remediation/` — the plan that closed issues #1, #2, #4, #6, #7, #8 and the remainder of #9:
  relative imports bind, every read answers from a refreshed store, `coverage.tests` and the repo-wide
  skipped total are published, and the two-worktree registration in `README.md` is documented.

Changing behaviour that a doc specifies means changing the doc in the same commit.
