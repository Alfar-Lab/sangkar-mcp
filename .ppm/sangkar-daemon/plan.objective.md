# Objective

Turn `sangkar` from a one-repository server into **one background daemon that manages N registered
repositories**, while each MCP session stays a thin `sangkar mcp` stdio server that serves those same N
repositories and never starts a watcher.

Why: today every `sangkar mcp` process is bound to one tree at startup (`src/index.ts` `loadConfig` +
`startWatch` at ~line 878), so an agent working in two repositories needs two servers, each owning a
poller, and the usage telemetry report can only ever be rendered for the one tree the process was bound to.
A single daemon that reconciles a global registry (`sangkar.json`) fixes the duplicate-poller problem, makes
per-repo watch/telemetry policy a file instead of an env var, and gives the telemetry HTML report a
machine-wide view served on loopback instead of one file per repo.

## Scope

**In:**

- `src/shared/`: registry (`sangkar.json`), per-repo config (`<repo>/.sangkar/config.json`), `SANGKAR_HOME`
  in `config.ts`, cursor repo binding, per-root store pool, pid lock helper, repo selection.
- `src/daemon/**` (new): singleton lifecycle, reconcile loop per registered repo, report-server host,
  daemon CLI; plus the new `daemon-privileges` boundary rule.
- `src/index.ts`: pooled per-root readers, the `repo` argument's resolution, no watcher in `mcp` mode,
  recorder chosen per selected repo.
- `src/tools/*/manifest.ts`: one optional `repo` argument each; `src/tools/index/**` auto-registration.
- `src/shared/telemetry/`: multi-repo report renderer, loopback report server, `--serve` on the verb.
- `bin/sangkar.ts`: `daemon` and `repo` verbs.
- Docs: `AGENTS.md`, `README.md`, `agent/SKILL.md`, `docs/INSTRUCTIONS.md`, `docs/my-product/TOOLS-SPEC.md`,
  `TELEMETRY.md`, `DESIGN.md`, `DATABASE.md`, `INDEXING-PIPELINE.md`.

**Out:**

- The daemon serving MCP. It never speaks the protocol; `sangkar mcp` stays a per-session stdio server.
- Any new telemetry record field, any network egress, any non-loopback binding, any new runtime dependency.
- A live schema change: the eleven tables, the single ledger, `migrations/**` and `store/publish.ts` are
  untouched.
- Editing any tool handler for the `repo` argument (the root strips the selector), editing
  `src/indexer/watch.ts`, or changing a payload key other than the new argument.

## Sources

- [AGENTS.md](../../AGENTS.md) — non-negotiables, layout, test map, definition of done.
- [docs/my-product/TELEMETRY.md](../../docs/my-product/TELEMETRY.md) §4–§5 — record schemas, privacy
  prohibition, the verb and the report.
- [docs/my-product/TOOLS-SPEC.md](../../docs/my-product/TOOLS-SPEC.md) — per-tool params and payload keys.
- [docs/my-product/DATABASE.md](../../docs/my-product/DATABASE.md) §3.0, §5 — data dir, root guard, publish.
- [docs/my-product/TECH-SPEC.md](../../docs/my-product/TECH-SPEC.md) §7–§8 — composition root, process split.
- [src/index.ts](../../src/index.ts), [src/shared/config.ts](../../src/shared/config.ts),
  [bin/sangkar.ts](../../bin/sangkar.ts), [src/indexer/watch.ts](../../src/indexer/watch.ts),
  [tests/boundary.test.ts](../../tests/boundary.test.ts) — the code this plan moves.
- `.ppm/sangkar-telemetry/`, `.ppm/cgraph-cli/` — the completed plans whose conventions this one copies.

## Done when

1. `PATH="$HOME/.bun/bin:$PATH" ~/.bun/bin/bun run typecheck && … run lint && … run test && … run db:drift`
   all pass from the repository root (TASK-026).
2. With a `SANGKAR_HOME` holding `sangkar.json` with two indexed repos, one `sangkar daemon start` watches
   both, spawns `sangkar worker --repo-root <repo>` per repo on change, and a second `daemon start` refuses
   (pid file) while `daemon status` names both repos and their config.
3. `sangkar mcp` started in repo A answers a `trace` call carrying `repo: <basename of B>` from B's graph,
   refuses an unregistered name with `not_found` and a hint listing the registry, and starts no watcher
   (`grep -rn "startWatch" src/index.ts` is empty). An explicit `--repo-root <tree>` supplies
   the **default** tree, so a call without `repo` is served from it even when it is registered nowhere,
   while a call that names a registered repo answers from that repo (the per-call argument outranks the
   process pin) and a call that names an unregistered one still fails `not_found`.
4. A cursor minted while serving repo A is refused with `cursor_mismatch` when a call in repo B resumes it.
5. `sangkar telemetry --serve` on `127.0.0.1` renders one self-contained HTML page with a section per
   registered repo, labelled by the directory each record set was read from, plus a machine total; a
   `--host 0.0.0.0` call exits 2 with usage and binds nothing; `sangkar telemetry --out` still writes one
   file as today.
6. `sangkar index` **and the MCP `index` tool** add the tree to `sangkar.json` (unless `--no-register`), and
   a registration write failure leaves the index run successful with one logged warning. The `index` verb
   reaches `runWorker` directly (`src/index.ts:1088`), so the verb's registration lives in the root, not in
   the tool handler.
7. Recording is written for a repo only when its `config.json` says `telemetry: true`, it is registered and
   `SANGKAR_TELEMETRY` is not `off`; an unregistered repo's `.sangkar/telemetry/` stays absent even after an
   `index --no-register` run, and `src/index.ts` builds no recorder except through `telemetryForRepo`
   (`grep -n "createTelemetry" src/index.ts` is empty).
8. `tests/boundary.test.ts` runs the six rules including `daemon-privileges`, and no `src/daemon/**` file
   imports `src/tools/**`.
9. `grep -rn "process.env" src/` reports three files — `src/shared/config.ts`,
   `src/indexer/watch.ts`, `src/tools/index/handler.ts` — and `SANGKAR_HOME` is read only in `config.ts`.
10. Docs and behaviour move together (D14), and `git status --porcelain agent/SKILL.md` still reports the
    pre-existing `name: use-sangkar-mcp` edit.
11. `sangkar status --repo-root <unregistered tree>` still answers from that tree (exit 0) while two repos
    are registered, and `sangkar mcp --repo-root <unregistered fixture>` still answers `tools/call`.
12. No suite pollutes the developer's real home: every suite that spawns a real `sangkar index` run sets
    `SANGKAR_HOME` to a temp directory — `tests/cli.test.ts:206`, `tests/store-refresh.test.ts:311-330`
    (called at `:481` and `:505`), `tests/telemetry-root.test.ts:324,451,458` — and a full `bun run test`
    leaves `~/.config/sangkar` as it found it. `tests/binary-smoke.test.ts` only spawns `mcp` (`:214`) and
    calls `runWorker` directly (`:146`), so it writes no registry and needs no change.
13. One scheduler: `runDaemon` owns the daemon's interval and `startReconcile()` is a passive handle, so a
    reconcile tick happens once per daemon tick and never on a second independent timer.
