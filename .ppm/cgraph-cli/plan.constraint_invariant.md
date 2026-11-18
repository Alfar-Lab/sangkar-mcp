# Constraints and invariants

Rules every task must respect. Violating one is a defect, not a preference.

## Architecture

- Three layers stay: `app/src/shared/**`, `app/src/tools/<name>/**`, `app/src/indexer/**`, with
  `app/src/index.ts` the only composition root. `app/tests/boundary.test.ts` enforces it.
- The new CLI code lives in `app/src/shared/cli/**`. `shared` may not import `tools/**` or
  `indexer/**`; the CLI modules therefore take an `Envelope` and an `InputLiteral` as data and never
  import a manifest or a handler. Tool dispatch, `openStore` and `ToolContext` stay in
  `app/src/index.ts`.
- `app/src/index.ts` imports only `#shared/*`, `./indexer/*` and `./tools/*/{manifest,handler}`. Do not
  add a fourth private alias and do not edit `boundary.test.ts` to allow one.
- No tool handler reads the environment, and no handler imports another tool.

## Surface

- After this plan there is exactly one tool surface: seven names (`status`, `summary`, `trace`,
  `search`, `impact`, `expand`, `index`). No profile, no `--profile` flag, no filtering function.
- Tool `name`, `description` and `input` stay byte-identical; only the `profile` field is removed.
  `tests/tools-*.test.ts` assert the verbatim descriptions, so a paraphrase is a regression.
- The envelope, its nine fields, `emit()`'s budget behaviour and `next_tools` semantics do not change.

## Runtime and I/O

- stdout belongs to the payload. In `mcp` mode stdout carries the MCP protocol and nothing else (no
  new writes, ever). In tool mode stdout carries exactly one JSON document (the envelope) or the four
  human `status` lines. `console.log` stays forbidden in `app/src/**`.
- Diagnostics remain JSON lines on stderr via `#shared/log`.
- `#shared/config.ts` is the only reader of `CODEGRAPH_CURSOR_KEY`; `app/src/index.ts` keeps the single
  `CODEGRAPH_NO_WATCH` read. After this plan no other `CODEGRAPH_*` variable exists.
- Exit codes: `0` for `ok` / `ambiguous` / `not_found`, `1` for `error` / `bootstrapping`, `2` for
  usage or configuration refusal.
- CLI grammar: `argv[0]` is the tool name (or the reserved `mcp`/`worker`); `splitGlobals` removes
  `--repo-root`, `--data-dir`, `--json` and `--help`/`-h` from the rest of argv, so they are legal
  before or after the tool name; only what remains reaches `parseCliArgs`. `--help` prints the tool's
  usage on stdout and exits 0. `cgraph --repo-root X status` (tool token not first) is not legal.
- `status` is a tool name, not a mode: `cgraph status` prints the human lines, `--json` prints the
  envelope, and the old `runStatus` path is deleted rather than kept beside it.
- Bun is **not on `PATH`** in this environment: use `$HOME/.bun/bin/bun` in every command.

## Data

- One database per working tree at `<repo>/.codegraph/graph.db`. `--data-dir` is the only override; a
  relative value resolves against the repo root. There is no environment override.
- The repo root is `--repo-root` when given, otherwise the nearest ancestor of cwd holding
  `.codegraph/`, otherwise the nearest ancestor holding `.git`, otherwise cwd.
- `guardRoot`'s three-way comparison and its `wrong_project_db` refusal are preserved unchanged; only
  its hint text changes (no `CODEGRAPH_DATA_DIR` mention).
- Do not modify `app/src/shared/store/**`, the Drizzle schema or `app/src/shared/store/migrations/**`.
  No live-database migration, no rebuild of an existing index.
- Never write into `code-review-graph/`, `codebase-memory-mcp/`, `tool-outputs/` or the repository's
  own `.codegraph/` from a test. Fixtures live in `mkdtemp` directories and are removed in `afterAll`.

## Verification environment

- Commands run from the repository root: `cd app && $HOME/.bun/bin/bun <script>`.
- Checks are the package scripts: `run typecheck` (tsc --noEmit, includes `src`, `tests`, `bin`),
  `run lint` (biome), `test` (bun test).
- Tests are offline and need no service. Fixtures use `mkdtemp` under the OS temp dir; `runWorker`
  builds small graphs, and `tests/binary-smoke.test.ts` builds the compiled binary (~105 MB) into a
  temp dir. Neither may touch the repository's own files.
- MCP sessions in tests set `CODEGRAPH_CURSOR_KEY` and `CODEGRAPH_NO_WATCH=1` and pass
  `--repo-root` / `--data-dir` as argv; a test that inherits a setting proves nothing.
- Forbidden for the executor: migrating a live database, running `bun link` / installing globally,
  force-pushing, editing `.ppm/*/tasks/*.json` by hand, or changing an unrelated test to make a
  failing check pass.
- Rollback: the whole change is code-only — no schema, no migration, `guardRoot` preserved — so
  reverting the phase commits restores the previous behaviour completely.
