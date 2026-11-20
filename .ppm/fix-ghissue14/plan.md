# fix-ghissue14

Created: 2026-11-20
Last updated: 2026-11-20
Base commit: dc5a548

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-20: created against `dc5a548`, from GitHub issue #14 (`Alfar-Lab/sangkar-mcp`). Owner-ratified
  design: no new payload key — `expand` publishes the graph's count in `totals.not_indexed_total`, the
  scope count stays in `coverage.skipped`, and `expand`'s own summary sentence reads `coverage.skipped`.
- 2026-11-20: revised after the independent audition (`not_ready`, 0 blocker / 3 major / 7 minor).
  F1: the spec edit moves into phase 0, so handler, tests and `TOOLS-SPEC` are one commit, and phase 1
  is the repository Definition of Done gate alone. F2: the regression proof no longer uses `git stash`;
  it breaks a scratch copy of the tree under `/tmp`. F3: the docs baselines are corrected to the nine
  real `not_indexed_total` lines and the §7 example range `800-916`. F4: the boundary-test claim is
  restated as the real mechanism (`tests/boundary.test.ts:324-331`, rule 5's `tests/**` exemption).
  F5/F6/F7/F8/F9/F10: two gate scopes, "untruncated coverage answer", six literal out-of-prefix rows
  with a computed negative assertion, corrected line citations, no cross-task reference in TASK-001's
  verify, and the issue's 115 figure attributed to an earlier tree.
- 2026-11-20: added `TASK-006` as the last task of the last phase — publish the shipped evidence as a
  comment on GitHub issue #14 and close it only when both acceptance criteria are demonstrated. It
  owns no repository file; Done-when #7 records the same condition.
