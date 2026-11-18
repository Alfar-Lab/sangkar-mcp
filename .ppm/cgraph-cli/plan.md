# cgraph-cli

Created: 2026-11-18
Last updated: 2026-11-18
Base commit: 4a42eb0

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-18: created; 3 phases, 21 tasks. Decisions taken with the user before planning: drop
  `CODEGRAPH_REPO_ROOT`, `CODEGRAPH_DATA_DIR` and `CODEGRAPH_PROFILE`; repo root comes from
  `--repo-root` or autodetect from cwd; data dir is `<repo>/.codegraph` unless `--data-dir`; the
  profile concept is removed, so all seven tools are always registered; CLI additions live under
  `app/src/shared/cli/`.
- 2026-11-18: `docs/` was reorganised into `docs/my-product/` and `docs/research/` after the plan was
  drafted. TASK-020/TASK-021 and the plan files now name `docs/my-product/{DATABASE,TECH-SPEC,DESIGN,
  TOOLS-SPEC}.md`; `docs/research/**` is out of scope. `plan_validate` and `plan_waves` re-run clean.
