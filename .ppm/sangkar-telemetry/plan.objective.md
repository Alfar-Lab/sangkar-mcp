## Objective

Give `sangkar` local-only usage telemetry — three JSON-Lines record kinds written to
`<dataDir>/telemetry/YYYY-MM-DD.jsonl` — and one reserved CLI verb, `sangkar telemetry`, that folds those
records into a **single self-contained `.html`** file. Nothing leaves the machine and nothing changes about
how a tool answers.

Why: the three known gaps in `AGENTS.md` §"Current state" (mislabelled `test`/`method` kinds, 9 166
unresolved call sites against 7 012 edges) are visible only to whoever runs SQL. A recorded call/run history
turns "binding recall is the bottleneck" into a number that can be watched across generations, and the same
records make a tool's truncation, budget and staleness behaviour observable without a server-side sink.

## Scope

**In:**

- `src/shared/telemetry/`: record types, a two-implementation sink (`jsonlSink`, `nullSink`), the rotating
  install id, the buffered recorder, the aggregator and the HTML renderer.
- `src/shared/config.ts`: `SANGKAR_TELEMETRY` read once, exposed as `AppConfig["telemetry"]`.
- `src/index.ts`: the one `call` wrapper + startup `session` record + flush on the existing exit path.
- `src/indexer/worker.ts`: the one `index_run` record at the worker's single exit path.
- `bin/sangkar.ts`: `telemetry` joins `RESERVED`, `USAGE` and `VERB_USAGE`, plus its own lazy dispatch.
- `tests/telemetry.test.ts`, `tests/telemetry-root.test.ts`, `tests/telemetry-report.test.ts`; updates to
  `tests/config.test.ts`, `tests/tools-index.test.ts`, `tests/cli.test.ts`, `tests/cli-args.test.ts`.
- `docs/my-product/TELEMETRY.md` (new), a `TECH-SPEC.md` §11 cross-reference, `AGENTS.md`.

**Out:**

- Any network sink, any upload, any server-side collector, any `posthog`/`segment`-style dependency. The
  sink seam exists so a remote sink is a later, additive change — it is not built here.
- Pre-bucketed numbers. Records carry raw values; bucketing belongs to a future network transform.
- Auto-opening a browser, a watcher for the report, retention/pruning of `.jsonl` files, an `install_id`
  reset verb, per-call sampling, and any edit to a tool handler, manifest, payload or the envelope.
- Any new runtime dependency, and any change to the pinned versions in `package.json`.
- Editing `docs/my-product/DESIGN.md`: its six "telemetry" mentions (lines 24, 47, 270, 344, 388, 424) mean
  the **reference servers'** captured usage data, and pointing them at our own records would merge two
  different things. `TECH-SPEC.md` §11 is the section that governs our logging, so the cross-reference goes
  there.

## Sources

- [AGENTS.md](../../AGENTS.md) — module rules, stdout discipline, "Current state and known gaps", the
  test map, the definition of done.
- [docs/my-product/TECH-SPEC.md](../../docs/my-product/TECH-SPEC.md) §3–§4 (isolation), §11 (logging and
  stdout discipline), §12 (test levels).
- [docs/my-product/DATABASE.md](../../docs/my-product/DATABASE.md) §3.0 (data dir and its own `.gitignore`).
- [../../src/index.ts](../../src/index.ts) — `TOOLS` (147), `callTool` (432), `shutdown` (517), `runTool`
  (688), `main` (784), `watcherDisabled` (595).
- [../../src/indexer/worker.ts](../../src/indexer/worker.ts) — `WORKER_EXIT` (112), `runWorker` (195),
  `indexGraph` (311), `durationMs` (332).
- [../../src/shared/cli/args.ts](../../src/shared/cli/args.ts) — `positionalFieldOf` (132), `parseArgs`
  (194), its defaults loop (200-211), `parseCliArgs` (232) and the hand-off at 314.
- [../../src/shared/config.ts](../../src/shared/config.ts) — `AppConfig` (19), `loadConfig` (91).
- [../../bin/sangkar.ts](../../bin/sangkar.ts) — `RESERVED` (11), `USAGE` (14), `VERB_USAGE` (45).
- [../../tests/boundary.test.ts](../../tests/boundary.test.ts) — the five rule ids (17-31), `walkTree`
  (288) walking all of `src` and `tests`.
- `.ppm/codegraph-mvp/`, `.ppm/cgraph-cli/` — the two completed plans this one copies conventions from.

## Done when

1. `~/.bun/bin/bun run typecheck && ~/.bun/bin/bun run lint && ~/.bun/bin/bun test &&
   ~/.bun/bin/bun run db:drift` all pass from the repository root.
2. In an unrelated temp tree, `sangkar status` creates `<dataDir>/telemetry/<UTC date>.jsonl` holding one
   `session` record and one `call` record whose `kind`, `status` and field set match plan.approach.md §3;
   the same command with `SANGKAR_TELEMETRY=off` creates no `telemetry/` directory at all.
3. `sangkar index` in that tree adds exactly one `index_run` record with `mode: "full"`, `exit_code: 0`,
   `generation_advanced: true`, and non-zero `files_seen`, `files_indexed` and `languages`.
4. The redaction invariant is a test, not a paragraph: a record built from a payload that contains a real
   repo path and a real symbol name serialises to a line containing neither (nor `search.query`), and a
   `call` record built from an envelope whose `stale_reason` is the `trace` prose records `"other"` and
   serialises neither the path nor `generation`.
5. `sangkar telemetry` writes exactly one file at `<dataDir>/telemetry/report.html`, containing no
   `http://`, `https://`, `src=`, `<link`, `@import` or `url(`; `sangkar telemetry --help` prints its usage
   and writes nothing; a corrupt JSONL line is skipped and counted, never fatal.
6. `git diff --name-only <base> -- src/tools` is empty: no tool handler, manifest or payload changed. (No
   `..HEAD`: nothing is committed between tasks or phases, so a commit range would print nothing whatever
   the tree holds — the base-to-worktree form is the one that can fail.)
7. `tests/boundary.test.ts` passes **without being edited**, and `grep -rn "process.env" src/` still reports
   only the baseline's four files — `src/shared/config.ts`, `src/index.ts` (the deliberate `SANGKAR_NO_WATCH`
   read at 596), `src/indexer/watch.ts` and `src/tools/index/handler.ts` — with no fifth file and with
   `SANGKAR_TELEMETRY` read only in `src/shared/config.ts`.
8. `docs/my-product/TELEMETRY.md` documents the three record schemas (each led by `ts, v`), the privacy
   prohibition with its reason, the variable and the verb; `README.md`'s environment-variable table carries a
   `SANGKAR_TELEMETRY` row and the `telemetry` verb's `USAGE` line says recording is on by default and local
   only; and `AGENTS.md`'s layout, env exception sentence, test map and "Current state" name what now exists.
