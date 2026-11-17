# codegraph-mvp

Created: 2026-11-17
Last updated: 2026-11-17
Base commit: b4cefc7

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-17: created; 4 phases, 33 tasks, phase_0 is the current phase
- 2026-11-17: audit round 1 (plan-auditor) returned NOT READY with 9 blocking findings; all 9 plus the
  8 non-blocking findings were applied. Contracts amended: envelope gains a conditional `error` and
  `read_status.stale_checked_at`, `emit()` takes a `mint` callback, downgrades `total_relation` to `gte`
  when it truncates and refuses budgets above 32 000. New tasks: TASK-034 (file census + `GraphReader`),
  TASK-035 (change-set diff with non-git degradation), TASK-036 (signed cursors). Edge producers added:
  `inherits`/`implements` in TASK-015/016, `tested_by` in TASK-017. Composition root now owns profile
  selection, watcher startup, signal handlers and `next_tools` (TASK-027). Instructions moved from a
  fourth `src/cli` layer to `src/shared/instructions.ts` (TASK-031). Total is now 36 tasks.
- 2026-11-17: audit round 2 returned NOT READY with 7 blocking findings, all introduced or exposed by round
  1's fixes; all applied. Ordering: TASK-029 (watcher) and TASK-031 (instructions) moved from phase_3 to
  phase_2 because `src/index.ts` imports both statically, so registration cannot precede them; TASK-033 now
  pre-requests only TASK-030 and TASK-032. Contracts: TASK-015 now emits value-position `references` sites
  (TASK-016 resolves and drops ambiguous ones), TASK-017 records the tested_by direction decision
  (test to symbol, inverting CRG), TASK-034 owns `ToolContext` plus the full reader surface (meta,
  generation, tombstones, entry points, hotspots, test_gaps, ordered `readNodeFts`), TASK-030 takes a reader
  instead of a connection, TASK-002 owns every `process.env` read including `cursorKey`, TASK-022/023/024
  verify and mint cursors, TASK-004 gains the composition-root rule. Oracles: TASK-019 restricted to the 12
  supported languages with yaml/bash/r asserted as `unsupported_ext`, and the by-kind mapping
  (CRG Function = our function + method) stated. Objective Done-when 4 corrected to match.
