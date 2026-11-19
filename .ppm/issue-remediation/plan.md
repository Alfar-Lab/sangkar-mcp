# Issue Remediation

Created: 2026-11-19
Last updated: 2026-11-19
Base commit: 58a2103eb86863bd17150afda14f70aff02e5b5c

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-19: created against `58a2103` (tag `v0.1.0-beta.1`), from the validation of open issues #1, #2, #4, #6, #7, #8, #9.
- 2026-11-19: revised after the independent plan audit. Blocking fixes: the store refresh now covers
  absent → ready (TASK-005); the fourth exact-shape assertion `tests/mcp-surface.test.ts:238` is named
  (TASK-017); the `example` exports land before the registry plumbing that imports them, so
  TASK-013 depends on TASK-014/015/016 instead of the reverse. Also folded in: the `CoverageSource`
  literal in `summary/handler.ts:821` (TASK-007), the remaining `status` pinned assertions
  (TASK-006), untracked-as-drift plus `tests/tools-smoke.test.ts:592` ownership (TASK-009), the
  drift-file deletion step (TASK-012), the exact SQL runner (TASK-008/TASK-020), and the
  `summary.unmatched_files` decision written as closed rather than conditional (D6).
