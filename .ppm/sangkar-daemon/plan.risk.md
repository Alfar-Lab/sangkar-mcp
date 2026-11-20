# Risks and open questions

## Resolved by the first audit

The first independent review returned NOT READY with five blocking defects and three major ones. All are
resolved in the tasks and in these documents; the resolutions are listed here so a later reader does not
reopen them.

| Finding | Resolution |
|---|---|
| B1 an explicit `--repo-root` had no precedence, so every ad-hoc caller was refused | `selectRepo` takes `pinnedRoot` and returns it before consulting the registry (TASK-002, TASK-008); Done-when 3 and 11 |
| B2 the `index` **verb** dispatched to `runWorker`, so TASK-011's handler patch could never register it | `src/index.ts` registers on a successful verb run unless `--no-register` (TASK-008 step 4) |
| B3 the unconditional recorder at `src/index.ts:1073` survived and created `.sangkar/telemetry/` for unregistered trees | the recorder is built only through `telemetryForRepo`, for every mode (TASK-008 step 3); Done-when 7 |
| B4 `runDaemonForeground` exposed no tick/watcher seam, so the end-to-end spawn assertion needed ~2.5 s of real polling | `ForegroundOptions` gains `intervalMs`, `onTick` and an injectable watcher factory (TASK-017, TASK-018); TASK-025 asserts watcher creation, not a spawn |
| B5 the composition order bound the server before the pid lock, contradicting its own invariant | the lock is acquired in `runDaemonForeground` before reconcile and bind; a throwing first `onTick` releases it (TASK-016, TASK-018) |
| M1 `TELEMETRY.md` §1 ("no network code path") and `TECH-SPEC.md` §7-§8 became false | both files join TASK-023 and TASK-024 |
| M2 auto-registration made five suites write the real `~/.config/sangkar/sangkar.json` | TASK-015 sets `SANGKAR_HOME` in `tests/cli.test.ts`; TASK-027 sets it in `tests/telemetry-root.test.ts` and `tests/binary-smoke.test.ts` |
| M3 a loopback refusal threw out of `serveReport` instead of exiting 2, and two schedulers drove one tick | TASK-015 wraps the call into the usage refusal; TASK-017 makes `startReconcile` passive |

### Second audit round

The resumed review confirmed B1–B5 and M1/M3 fixed and returned four new findings, because the B1 fix had
been written as "pin outranks everything", which silently ignored an explicit `repo` in every
`--repo-root` session. All four are now folded in:

| Finding | Resolution |
|---|---|
| N1 `pinnedRoot` outranked an explicit `repo`, so the selector was dead in a pinned session and `not_found` became unreachable | precedence is now `requested` → `pinnedRoot` → cwd/single/refusal (TASK-002, TASK-008, TASK-023, Done-when 3) |
| N2 `tests/store-refresh.test.ts` also spawns the `index` verb through a `cleanEnv()` that strips `SANGKAR_*`, so it would write the real registry; `tests/binary-smoke.test.ts` was the wrong target | the temp `SANGKAR_HOME` for `store-refresh` joins TASK-008 (it already owns the file and its own `verify` runs it); TASK-027 keeps only `tests/telemetry-root.test.ts` |
| N3 a bind failure left `server` unassigned, so `finally` would throw before `releasePidLock` | TASK-018 declares `let server: ReportServer | null = null` and closes with `server?.close()` |
| N4 the pid lock could be released twice (owner and `runDaemon`) | TASK-016 states `runDaemon` never releases a caller-supplied `lock`, and `releasePidLock` is idempotent |

## Risks

| Risk | Mitigation |
|---|---|
| TASK-008 reorders `callTool` (parse → select repo → open store) and is the single riskiest change; a mistake breaks every tool. | The reorder is mechanical and `repo` is stripped before dispatch, so handlers never see it. `tests/multi-repo.test.ts` asserts all paths, and the existing `mcp-surface.test.ts`, `tools-smoke.test.ts`, `contract.test.ts` and `store-refresh.test.ts` are the regression net; TASK-021 runs the whole suite. |
| The pool caches a `Store` whose file was republished underneath it. | `refreshStore()` already decides on the file fingerprint; the pool grows `replace(root, value)` so the root hands the refreshed store back instead of reopening per call, and `tests/store-refresh.test.ts` (owned by TASK-008) keeps its generation-refresh cases. |
| Two workers (daemon-spawned and ad-hoc CLI) append to one JSONL file. | Unchanged from the completed telemetry plan: one flush is one `appendFileSync` of the whole ≤32-record buffer, so writes land whole and interleave only at line boundaries; the reader already skips a torn trailing line. |
| Concurrent `sangkar.json` writes (two index runs registering at once). | `registerRepo()` is read-modify-write into a temp file plus `rename()`, so a reader sees the old or the new file, never a partial one; last writer wins and the loser's re-read on the next tick recovers. Registration failure is a logged warning only (D5). |
| The daemon leaks no watcher but could leak a *timer chain* across reconcile ticks (one watcher per repo per tick). | `reconcile.tick()` diffs the registry against a `Map<root, WatchHandle>`: start only new roots, `stop()` removed or `watch:false` ones, restart only when `watchIntervalMs` changed. `tests/daemon-reconcile.test.ts` asserts the watcher count stays 1 per repo across three ticks. |
| Removing the watcher from `sangkar mcp` changes behaviour that tests may assume. | `SANGKAR_NO_WATCH` is set by six test files (`store-refresh`, `binary-smoke`, `acceptance`,
`tools-smoke`, `telemetry-root`, `mcp-surface`) and becomes inert rather than wrong; nothing asserts a running watcher. `AGENTS.md`/`README.md` drop the variable in TASK-022, and `tests/mcp-surface.test.ts` is untouched. |
| The report naming source directories looks like a privacy regression (`TELEMETRY.md` §5). | §5 governs *records*; the report already prints aggregates on a loopback page. TASK-023 states the boundary explicitly ("records carry no path; the report names the directory it read"), `src/shared/telemetry/record.ts` is not edited by any task (TASK-023 asserts the schema is unchanged), and TASK-014 asserts the page's self-containment list. |
| A long-running HTTP server keeps the process alive in tests. | `serveReport()` returns a `ReportServer` with `close()`; every test closes it in `afterAll`, and `runDaemonForeground` accepts an `AbortSignal` so no test kills a process by pid. |
| The end-to-end test indexes real trees and could be slow or flaky. | TASK-025 indexes two one-file trees under `mkdtemp` (the worker's own integration test indexes the big corpus; this one does not), drives `tick()` explicitly instead of waiting for a poll, and injects `spawn` so no child process is required for the assertion. |

## Open questions

1. **Default report port.** No port is specified anywhere. The plan fixes `DEFAULT_REPORT_PORT = 7317`,
   exported and documented, with `--port 0` for tests and any other value accepted on loopback. Blocks:
   no — the owner can change one constant plus one doc line. Answered by: the owner, non-blocking.
2. **Is `SANGKAR_NO_WATCH` retired or kept as a no-op?** The plan retires it: `src/index.ts` stops reading
   it, tests that set it keep working, docs drop the row. If the owner wants it to keep meaning "the daemon
   in this environment must not watch", that is a different feature (a daemon-side switch) and is not built
   here. Blocks: no. Answered by: the owner, non-blocking.
3. **Contract encoding of "clamp at 300000 ms".** D4 says non-integer/negative falls back to the default and
   the floor is 1000 ms; the plan treats `0` and `<1000` as "clamp to 1000" and `>300000` as "clamp to
   300000", reserving the default fallback for non-integers, negatives and wrong JSON types. Blocks: no,
   but TASK-003's test states the matrix so a reviewer can disagree with one line. Answered by: the owner.
   **Resolved:** the clamp ladder as written is kept; `0`, `1`, `999` → 1000 ms.

3a. **Does a registered repo make an unregistered `--repo-root` unreachable?** No: the pin wins (see
   `plan.approach.md` §12). Blocks: no.

3b. **Does `sangkar index --no-register` still create a telemetry directory?** No, after the recorder
   retirement: recording is per repo and gated on registration. Blocks: no.
4. **Does `sangkar repo remove` exist?** D3/D4 define the files, not the verbs. The plan adds
   `repo add|remove|list|config` because a registry with no writer is unusable and `config.json` needs one
   too (TASK-019). Blocks: no. Answered by: the owner.
5. **Lifecycle of a repo dropped from `sangkar.json` while the daemon runs.** The plan stops its watcher on
   the next tick and leaves its graph and JSONL in place. Blocks: no. Answered by: the owner, default
   proposed here.
6. **`SANGKAR_HOME` with a leading `~`.** The plan expands a leading `~/` using `$HOME` (same treatment the
   registry gives its entries). Blocks: no. Answered by: the owner.
7. **Locale/`realpath` on a path that does not exist yet.** D3 says canonicalise with `realpath` *when the
   path exists*; a registered-but-missing path stays as `resolve()` output and the daemon reports it as
   missing every tick (one warning). Blocks: no. Answered by: TASK-002's test, which pins both branches.
