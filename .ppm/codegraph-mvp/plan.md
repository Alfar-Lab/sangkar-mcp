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
