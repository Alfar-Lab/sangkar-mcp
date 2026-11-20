# Approach

## Current state (verified at 9cedf8a)

- `src/shared/config.ts` (137 lines): `loadConfig()` resolves `--repo-root` (else `detectRepoRoot(cwd)`),
  `--data-dir`, and reads the environment at one place only — `SANGKAR_CURSOR_KEY` and
  `SANGKAR_TELEMETRY` (`off` disables; anything else enables, line 105). `AppConfig` is
  `{dataDir, repoRoot, cursorKey, budgets, telemetry}`; `telemetry.dir = <dataDir>/telemetry`.
- `bin/sangkar.ts` (102 lines): `RESERVED = ["mcp","worker","index","telemetry"]` (line 23), `USAGE`,
  `VERB_USAGE`; `telemetry` dispatches lazily to `src/shared/telemetry/cli.ts` before `main` is imported.
  Any other first token is a tool name.
- `src/index.ts` (1112 lines): `main()` builds the one recorder from `config.telemetry.enabled` (1070-1081);
  `runMcp` opens one `Store` (`openStore`, 400-470), refreshes it per call (`refreshStore`, 479), registers
  the seven `TOOLS` (179), answers through `callTool` (616) which builds `context` from `store.context`, and
  starts `startWatch({repoRoot, dataDir})` at 878 gated by `watcherDisabled(process.env)` (`SANGKAR_NO_WATCH`,
  816-823). `guardRoot()` (362) refuses `wrong_project_db`.
- `src/shared/store/read/context.ts`: `ToolContext {reader, cursor, log, now, config}`. Handlers read
  `ctx.config.repoRoot` / `.dataDir` / `.budgets` only (8 call sites in 5 handlers: `expand` 1, `status` 2,
  `impact` 2, `index` 2, `summary` 1).
- `src/shared/cursor.ts`: `CursorPayload {generation, tool, argsHash, position}`; handlers sign their own
  tokens (8 call sites in 6 handlers), so the payload shape must stay the handler-facing shape.
- `src/shared/store/db.ts`: `StatementCache` per `Conn`, cap 200, `WeakMap<Conn, GraphDb>`.
- `src/indexer/watch.ts` (588 lines): `startWatch({repoRoot, dataDir, intervalMs, debounceMs, spawn,
  onSpawn})`, `DEFAULT_INTERVAL_MS = 2_000`, `DEFAULT_DEBOUNCE_MS = 500`, `backoffMs()` capped at
  `MAX_BACKOFF_MS = 300_000`, spawns `bun run bin/sangkar.ts worker`.
- `src/shared/telemetry/`: `readRecords(dir, {since})` + `aggregate({records, unparsed, files, since})` →
  `ReportData` (`report.ts`), `renderHtml(data, {generated_at, since})` → one self-contained document
  (`html.ts`), `runTelemetry(argv)` → one file (`cli.ts`).
- `src/shared/store/lock.ts` (228 lines): create-exclusive `write.lock`, `STALE_AFTER_MS = 60_000`, reclaim
  by dead pid or age, `heartbeat()`. It is the writer's lock and stays untouched.
- `tests/boundary.test.ts` (430 lines): five rules, synthetic self-checks, `walkTree` over `src` and `tests`
  only; `root-privileges`' else-branch already exempts `src/indexer/**` and `tests/**`.
- `tests/tools-index.test.ts:111` holds a hand-written `AppConfig` object literal — a new required field
  breaks typecheck there, so that file joins TASK-001's ownership.
- `process.env` readers: `src/shared/config.ts:105`, `src/index.ts:878`, `src/indexer/watch.ts:342`,
  `src/tools/index/handler.ts:155`.

## Design

1. **Home is machine-level, config is per-repo.** `AppConfig` gains `home`, `registryPath` and
   `daemon {pidPath, logPath}`; `withRepo(config, root)` returns the same machine fields with `repoRoot`,
   `dataDir` and `telemetry.dir` swapped. `RepoConfig {watch, watchIntervalMs, telemetry}` is read from
   `<dataDir>/config.json` by a separate module. Rationale: handlers keep reading `ctx.config.repoRoot`
   and never learn that a process serves several repos (D13), and the daemon can derive one config per
   registry entry without re-reading the environment.
2. **Registry as data, selection as a function.** `readRegistry(path)` never throws; `registerRepo()`
   canonicalises, dedupes, writes atomically (temp + rename) and swallows its own write error into one
   warning; `selectRepo({registry, requested, cwd})` implements D6's three rules and reuses `not_found` /
   `ambiguous`. Rationale: one module owns "which repos exist", so MCP, CLI, daemon and report all agree.
3. **The reader pool is generic and passive.** `createStorePool<T>({open, close, idleTimeoutMs, now})` with
   `acquire/replace/releaseIdle/closeAll`; no timers inside. `src/index.ts` instantiates it with
   `openStore`/`conn.close` and calls `releaseIdle()` on each call; `POOL_IDLE_TIMEOUT_MS` is exported for
   tests. Rationale: D8 needs one `Conn` per canonical root, yet the pool must be unit-testable without a
   database and must not keep a process alive.
4. **Cursor binding without touching handlers.** The wire payload gains `repo`, while `CursorPayload` — what
   a handler passes to `sign` — keeps its four fields; `createCursors({key, repoId?})` stamps and compares
   it, so nothing in `src/tools/**` changes and every existing cursor test keeps its meaning.
5. **`repo` is a manifest argument the root consumes.** Each manifest declares
   `repo: {type: "string", description: …}` (D6), so `tools/list` publishes it and `parseArgs` keeps it; the
   root resolves it, opens the pooled store for that root, drops the key, and hands the handler the rest.
   Rationale: the manifest stays the only schema, no handler grows a selector, and `--repo <name>` works on
   the CLI for free.
6. **The MCP server stops watching.** `watcherDisabled`, the `startWatch` call and the `watching` field are
   removed from `src/index.ts`; `SANGKAR_NO_WATCH` is no longer read anywhere (tests that set it stay
   harmless). Ad-hoc trees keep working through `--repo-root` with no watcher and no recording (D12).
7. **The daemon is a sidecar, never a server.** `src/daemon/**` owns the pid lock, one `startWatch` per
   registered repo (one timer, not a thread), the `sangkar worker` spawn it inherits from the watcher, and
   the report host. It never opens a database and never speaks MCP. A `daemon-privileges` rule keeps it out
   of `src/tools/**`.
8. **The report server is one shared module with two hosts.** `src/shared/telemetry/serve.ts` binds loopback,
   reads the registry on each request, aggregates one section per source directory plus a machine total, and
   renders with `html-multi.ts`. Both `sangkar telemetry --serve` (foreground, for an operator without a
   daemon) and `src/daemon/main.ts` (always-on) start that same module — one implementation, two lifetimes.
   Rationale: D1 gives the daemon the report as a duty; D10 gives it a verb; duplicating the HTTP layer to
   satisfy both would be the only alternative.
9. **Recorder per selected repo.** `src/shared/telemetry/for-repo.ts` turns (consent, `RepoConfig`,
   `registered`) into a `Telemetry` or `nullTelemetry` for that repo's dir; the root keeps a
   `Map<canonicalRoot, Telemetry>` closed on shutdown. Rationale: D10's precedence in one tested place, and a
   call served from repo B writes B's JSONL.
10. **No new error code, no migration, no dependency.** `not_found`, `ambiguous` and `index_locked` already
    express every new refusal; doc TASK-023 records that mapping in `TOOLS-SPEC.md`.
11. **Why three phases and no phase_3.** phase_0 fixes every cross-slice contract (config, registry,
    per-repo config, cursor, pool, boundary/daemon types) and gates them with targeted tests; phase_1 fans
    out against those contracts with one owner of `src/index.ts` and one owner of `bin/sangkar.ts`; phase_2
    integrates (one end-to-end daemon test), moves the documentation in the same change, and closes with the
    single full-definition-of-done gate. A fourth barrier would only serialise doc work that is already
    file-disjoint, so `plan.approach.md` deliberately lists no `phase_3` and no `tasks/phase_3.json` exists.
12. **`--repo-root` is a default, `repo` is a request.** `selectRepo` resolves a supplied `requested` first,
    then a non-empty `pinnedRoot`, then cwd → single registered repo → refusal. The pin keeps every
    existing ad-hoc caller working (mcp-surface, store-refresh and binary-smoke fixtures,
    `sangkar status --repo-root <unregistered>`), while an explicit `repo` still means what the shipped
    instructions say and an unregistered name is still refused.
13. **The `index` verb registers in the root.** `sangkar index` dispatches to `runWorker`
    (`src/index.ts:1088`) and never reaches the tool handler, so `src/index.ts` calls
    `registerRepo(...)` on a successful verb run (exit 0 or 2) unless `--no-register` is present; the MCP
    `index` tool handler registers separately for its own path.
14. **A passive reconcile and a locked-first daemon.** `startReconcile()` starts no timer (the daemon's
    loop drives `tick()`), its watcher factory is injectable so tests assert watcher creation without
    sleeping on a poll, and `runDaemonForeground` acquires the pid lock before any watcher or bind.
15. **The recorder is per repo for every mode.** The single unconditional recorder is replaced by
    `telemetryForRepo`, so an unregistered or `telemetry:false` tree gets no `.sangkar/telemetry/` at all.

## Phases

### phase_0: contracts and shared plumbing

Deliverable: six modules with tests and no behaviour change — `config.ts` machine fields + `withRepo`,
`registry.ts` (read/write/select), `repo-config.ts` (defaults, clamping, recording precedence), cursor repo
binding, `store/pool.ts`, and `daemon-privileges` + `src/daemon/types.ts` — all green under `typecheck` and
their own test files. Why a barrier: phase_1 has seven tasks that each consume one of these signatures, and
a worker must not invent the shape another worker is implementing at the same time.

### phase_1: fan-out slices against the contracts

Deliverable: the running feature, slice by slice — the composition root routes a call to a registered repo
through the pool, six manifests and the index tool carry the new argument and auto-registration, the
telemetry renderer/server/verb serve every registered repo, the daemon has a singleton lifecycle, a
reconcile loop, a composition root and CLI verbs, and the shipped instructions describe the new surface.
Why a barrier: the end-to-end test in phase_2 needs every slice present, and the two same-file serial
chains (`src/index.ts`, then `bin/sangkar.ts` after `src/daemon/main.ts`) must be resolved before integration
starts.

### phase_2: integration, documentation, closing gate

Deliverable: one end-to-end test (two registered repos, a daemon serving both, one unregistered ad-hoc tree)
and the whole D14 documentation set, followed by the closing gate that runs the definition of done and the
four greps that encode this plan's invariants. Why a barrier: the docs must describe what the merged code
does, so they cannot start before phase_1 is complete, and the gate must run last.
