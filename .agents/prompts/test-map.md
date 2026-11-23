# Test map

Which tests guard which change. Entry point: [`AGENTS.md`](../../AGENTS.md).

| If you change… | These will tell you |
|---|---|
| module imports, layer rules | `boundary.test.ts` |
| the response shape, `emit()`, truncation, cursors | `envelope.test.ts`, `budget.test.ts`, `cursor.test.ts`, `contract.test.ts` |
| schema, migrations, indexes | `schema-parity.test.ts` (live DB vs `getTableConfig`), `db:drift` |
| connection, lock, publish | `lock.test.ts`, `publish.test.ts`, `smoke.test.ts` |
| the reader surface | `read.test.ts`, `read-files.test.ts` |
| resolution, neighbours, absence, risk, diff, freshness | `domain.test.ts`, `diff.test.ts`, `freshness.test.ts` |
| language tables or the parser pool | `lang.test.ts`, `parse.test.ts` |
| discovery, extraction, binding, persistence | `discover.test.ts`, `extract.test.ts`, `resolve.test.ts`, `persist.test.ts` |
| the index pipeline end to end | `worker.test.ts`, `index-integration.test.ts`, `watch.test.ts`, `parser-parity.test.ts` |
| one tool's payload or defaults | `tools-<name>.test.ts` |
| registration, profiles, stdio discipline | `mcp-surface.test.ts`, `tools-smoke.test.ts` |
| the CLI surface | `cli.test.ts`, `cli-args.test.ts`, `cli-render.test.ts` |
| the registry file, and how a call selects a repository | `registry.test.ts` |
| the per-repo settings file, its defaults and its clamp | `repo-config.test.ts` |
| the machine-level `config.json`, its creation and its precedence | `global-config.test.ts` |
| the reader pool and its idle eviction | `pool.test.ts` |
| the daemon's pid file | `pidlock.test.ts` |
| one reconcile pass: missing, unwatched, retuned, forgotten | `daemon-reconcile.test.ts` |
| the daemon's pid lock, tick and lifecycle verbs | `daemon-lifecycle.test.ts` |
| the daemon's composition order | `daemon-main.test.ts` |
| the `daemon` verb's usage and exit codes | `cli-daemon.test.ts` |
| two registered repositories through one process | `multi-repo.test.ts`, `daemon-integration.test.ts` |
| the instructions text shipped to an agent | `instructions.test.ts` |
| the telemetry record shape, the consent switch, the daily id rotation and the privacy invariant | `telemetry.test.ts`, `telemetry-root.test.ts` |
| the report's aggregation, escaping and self-containment, and the verb | `telemetry-report.test.ts`, `cli.test.ts` |
| the served multi-repo report, and which repo may record | `telemetry-serve.test.ts`, `telemetry-precedence.test.ts` |
| budgets and latency end to end | `acceptance.test.ts` |
| the shipped binary | `binary-smoke.test.ts` |

What this map does **not** cover is listed in [`state-gaps.md`](state-gaps.md).
