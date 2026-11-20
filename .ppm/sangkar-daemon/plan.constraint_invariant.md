# Constraints and invariants

Rules every task must respect. Violating one is a defect, not a preference.

## Architecture

- Five module rules stand (`tests/boundary.test.ts:16-31`, `RULES` at 220) and TASK-006 adds a sixth,
  `daemon-privileges`:

  | Rule | Effect |
  |---|---|
  | `tool-isolation` | `src/tools/<tool>/**` imports `#shared/*` and `./` siblings only |
  | `indexer-isolation` | `src/indexer/**` imports `#shared/*` and its own siblings only |
  | `shared-isolation` | `src/shared/**` never imports `src/tools/**` or `src/indexer/**` |
  | `drizzle-location` | `drizzle-orm` only under `src/shared/store/**` |
  | `root-privileges` | `src/index.ts` is the only importer of a tool handler; `src/daemon/**` joins the
    tree that may import `src/indexer/**` |
  | `daemon-privileges` | `src/daemon/**` may import `#shared/*`, `src/indexer/**` and its own `./`
    siblings, **never** `src/tools/**` |

- `tests/boundary.test.ts` walks every `.ts` under `src` and `tests` (`walkTree`, line 288) and does **not**
  walk `bin/`. `bin/sangkar.ts` importing `../src/daemon/cli` is therefore outside enforcement; it is legal
  for the same reason it may import `../src/shared/telemetry/cli`, and it is a review item, not an automated
  one. `tsconfig.json` and `biome.json` already cover `src/daemon/**`; no config file changes.
- `src/index.ts` stays the composition root. No task moves a handler, a manifest or the tool registry out of
  it, and no tool handler is edited for the `repo` argument: the root resolves and strips it.
- `src/indexer/watch.ts` is not edited. The daemon consumes `startWatch({repoRoot, dataDir, intervalMs,
  spawn})` and `backoffMs` as they are; `SpawnWorker` is the spawn seam its tests already use.
- **`--repo-root` is a default, an explicit `repo` is a request.** `selectRepo` order is: a supplied
  `requested` wins (path, then basename, then `ambiguous`/`not_found`); otherwise a non-empty `pinnedRoot`
  from `--repo-root`; otherwise cwd → single registered repo → refusal. Without the pin branch every
  existing ad-hoc caller breaks: `sangkar status --repo-root`, `tests/mcp-surface.test.ts:111` and
  `tests/store-refresh.test.ts:312` (both spawn `mcp --repo-root <temp fixture>` from `cwd: APP_ROOT`), and
  the compiled binary at `tests/binary-smoke.test.ts:214`. Without `requested` first, a pinned session
  silently ignores the selector the instructions document.
- **One scheduler.** `runDaemon` owns the interval; `startReconcile()` returns a passive handle
  (`tick`/`stats`/`stop`) and starts no timer, so one daemon tick runs reconcile exactly once.
- **The pid lock precedes the bind.** `runDaemonForeground` acquires `daemon.pid` before any watcher starts
  and before `serveReport` binds; a second daemon rejects with `index_locked` and leaves nothing bound. A
  throwing first `onTick` releases the lock and propagates.
- `src/shared/config.ts` is the **only** reader of the environment. After TASK-008 the root no longer reads
  `SANGKAR_NO_WATCH`; the two remaining non-config reads are the child-environment passes in
  `src/indexer/watch.ts:342` and `src/tools/index/handler.ts:155`, which forward the parent env to a spawned
  child and are not settings reads. `grep -rn "process.env" src/` must report exactly those three files.

## Process and data rules

- **stdout is the MCP protocol.** Every diagnostic goes through `#shared/log` (stderr). The only stdout
  writes are the existing ones: one CLI envelope or status line, a usage block, and the one human line
  `sangkar telemetry` / `sangkar repo` / `sangkar daemon` prints.
- **One writer.** Only `sangkar worker` (spawned) opens a write connection; the daemon never opens
  `graph.db`, never takes `write.lock`, never imports `store/publish.ts`. Never publish a partial graph:
  stage → `seal()` → `rename()`, untouched.
- **No Drizzle builder in a handler or a loop.** The pool keeps one `Conn` and one prepared cache per
  canonical root; nothing else changes about `store/db.ts`.
- **One recorder decision.** `src/index.ts` builds a recorder only through `telemetryForRepo`
  (`#shared/telemetry/for-repo`), for every mode — `mcp`, `index`, `worker` and a one-shot tool call. The
  unconditional `createTelemetry` call at `src/index.ts:1073-1081` is retired, because its construction
  creates `<repo>/.sangkar/telemetry/` (`src/shared/telemetry/identity.ts:57-60`) even for an unregistered
  tree.
- **Privacy §5 unchanged.** No new telemetry record field. A record may still carry numbers, closed-union
  identifiers and timestamps only. The multi-repo report names each record set's **source directory** —
  that is report HTML on a loopback interface, not a record, and TASK-023 documents the distinction.
- **Loopback only.** `serveReport` binds `127.0.0.1`/`::1`/`localhost` and refuses any other host with a
  usage refusal (exit 2) before binding. The HTML is self-contained: no `http://`, `https://`, `src=`,
  `<link`, `@import` or `url(`.
- **Registry and config files never crash a process.** A missing or invalid `sangkar.json` or
  `config.json` yields the documented defaults plus one logged reason; a failed registration write is one
  logged warning and the index run still succeeds.
- **Tool surface**: seven tools, unchanged payload keys except the new optional `repo` argument. No new
  error code — `not_found` (a name that is not registered) and `ambiguous` (no default and several repos)
  reuse the closed union in `src/shared/errors.ts`.
- Exit codes keep their meaning: 0 ok, 1 error/not-running, 2 usage/configuration refusal.

## Compatibility

- No new dependency, no change to the pins (`@modelcontextprotocol/sdk` 1.32.2, `drizzle-orm` 0.45.4,
  `web-tree-sitter` 0.27.2, `tree-sitter-wasms` 0.1.13), no `package.json` edit.
- No migration, no schema change, no `migrations/**` edit: `git diff --name-only 9cedf8a -- src/shared/store/migrations`
  stays empty.
- Nothing is committed between tasks or phases. Every check is a base-to-worktree or base-to-file
  comparison, never a commit range.
- `agent/SKILL.md` carries one pre-existing uncommitted line (`name: use-sangkar-mcp`). TASK-022 must
  preserve it; no task reverts, stashes or discards it, and no `git checkout`/`git restore` runs on that file.

## Verification environment

- Commands run from the repository root. Spell every one `PATH="$HOME/.bun/bin:$PATH" ~/.bun/bin/bun …`:
  the scripts shell out to a bare `bun`, so they need the binary on `PATH` even though `bun` also resolves
  from `~/.local/bin` on this machine.
- Per-task checks are targeted: `… ~/.bun/bin/bun test tests/<file>.test.ts`, plus `… run typecheck` when
  the task changes a signature another file already calls. A phase gate runs `run typecheck && run lint &&
  run test` (whole suite, ≈52 s, 726 tests at the base commit).
- The closing gate (TASK-026) is the full definition of done:
  `run typecheck && run lint && run test && run db:drift`. `db:drift` runs `drizzle-kit generate` and
  `git diff --exit-code` on `src/shared/store/migrations`; it tolerates the pre-existing `agent/SKILL.md`
  modification.
- Tests are offline and need no service. Fixtures are `mkdtemp` directories removed in `afterAll`; no test
  writes into the repository's own `.sangkar/`, into `$HOME/.config/sangkar`, or into the nested
  `code-review-graph/` and `codebase-memory-mcp/` checkouts. Every daemon test sets `SANGKAR_HOME` to its
  own temp directory and deletes every inherited `SANGKAR_*` first. The three suites that already spawn a
  real `sangkar index` run without a home (`tests/cli.test.ts:206`, `tests/store-refresh.test.ts:311-330`
  via `cleanEnv()` at `:295-307`, and `tests/telemetry-root.test.ts:324,451,458`) must set `SANGKAR_HOME`
  in the same change that makes an index run register the tree — otherwise the suite writes the
  developer's real registry. `tests/binary-smoke.test.ts` spawns only `mcp` (`:214`) and calls `runWorker`
  in-process (`:146`), so it writes no registry.
- `db:drift` runs `drizzle-kit generate` and can therefore write `src/shared/store/migrations/` when it
  fails. The closing gate reports that failure as a blocker and never commits what the tool wrote.
- Time-dependent daemon tests drive `tick()`/`releaseIdle()` explicitly with an injected clock and an
  injected `spawn`; no test sleeps for a real poll interval and no test starts a real worker process.

## Forbidden for the executor

Editing `.ppm/*/tasks/*.json` by hand, editing `tests/boundary.test.ts` other than in TASK-006, weakening an
assertion to make a check pass, adding a dependency, running `bun link`, touching `docs/research/**`,
`code-review-graph/**`, `codebase-memory-mcp/**`, or running `git checkout`/`git restore`/`git stash`.

## Rollback

Additive code plus three new filesystem artefacts (`$SANGKAR_HOME/sangkar.json`,
`$SANGKAR_HOME/daemon.pid`, `<repo>/.sangkar/config.json`), all outside the published graph: reverting the worktree restores the previous
behaviour completely — `sangkar mcp` binds one tree from cwd and watches it again, no daemon is required, and
the offline `sangkar telemetry --out` report still reads the same JSONL. Deleting `$SANGKAR_HOME` and the
per-repo `config.json` files is a complete data rollback; nothing in `.sangkar/`'s graph is rewritten by any
task in this plan.
