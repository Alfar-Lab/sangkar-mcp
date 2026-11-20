# Fix GH Issue 13

Created: 2026-11-20
Last updated: 2026-11-20
Base commit: dc5a548

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-20: created against `dc5a548`, from GitHub issue #13
  ("`payload.graph.size_bytes` is a live stat beside the served generation") and the owner's ratified
  decision to report the size recorded in the store fingerprint instead of a live `statSync`.
- 2026-11-20: revised after the independent audition. Findings folded in: the `git status` allow-list
  now matches porcelain's collapsed `?? .ppm/…` entries and admits the sibling `.ppm/fix-ghissue14/`
  (F1); Done-when #4 matches what `TASK-006` actually asserts (F2); the `ToolContext` table is
  `TECH-SPEC.md:358-366`, three columns (F3); drifted line references corrected against `dc5a548`
  (F4); `TASK-003` no longer cross-references a plan file (F5); the commit-scheduling resolution is
  recorded in `plan.constraint_invariant.md` (F7); `TASK-006` merges `statSync` into the existing
  `node:fs` import (F8).
- 2026-11-20: the single `plan_validate` warning ("phase_1: fully serial") is **accepted, not
  defects**: phase 1 is serial by design — `TASK-005` (handler preference rule) must exist before
  `TASK-006` (integration cases assert it) and `TASK-007` gates both — so nothing may be merged or
  enlarged to widen it; the justification is written out in `plan.approach.md` `### phase_1`.
- 2026-11-20: added `TASK-008` as the last task of the last phase — publish the shipped evidence as a
  comment on GitHub issue #13 and close it only when both acceptance criteria are demonstrated. It
  owns no repository file; Done-when #8 records the same condition. `TASK-003`'s TECH-SPEC row was
  narrowed to the seam alone, so no phase-0 document claims what `status` publishes before phase 1
  changes it (F7).
