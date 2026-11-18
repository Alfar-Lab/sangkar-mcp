# cgraph-cli

Created: 2026-11-18
Last updated: 2026-11-18
Base commit: 1f7e0c6

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
- 2026-11-18: audit round 1 (plan-auditor) returned `not_ready` with 3 blockers, 5 majors, 8 minors and
  6 notes. All 3 blockers and 5 majors applied: (B1) `cgraph status` now has one owner — `runTool` prints
  `statusLines` unless `--json`, and `runStatus`/the `status` mode are deleted; (B2) the CLI grammar is
  pinned (`argv[0]` is the tool token, `splitGlobals` strips `--repo-root`/`--data-dir`/`--json`/`--help`
  before `parseCliArgs`, positional rules stated, `--help` exits 0); (B3) the four unowned `profile`
  assertions in `tools-search`, `tools-impact`, `tools-status` and `read-files` are now assigned.
  Majors: `AGENT_TOOL_NAMES` imports in the three process-level suites assigned to TASK-013/014/015 and
  TASK-016's verify narrowed to its own file; TASK-008's verify no longer claims a green typecheck it
  cannot have; TASK-010's watch-test step replaced (its stub cannot observe the appended flags) and its
  doc-comment/hint edits made explicit; every `profile` mention in `app/src/**` assigned; `type InputField`
  restored to TASK-008's import list. Minors fixed: script count (12), `cd ..` in TASK-020's verify,
  `(expect no output)` removed from command strings, TASK-013's phantom second `loadConfig` site and its
  `fixture.corpus`/`fixture.dataDir` names, TASK-017's absolute entry path, TASK-002's
  `unknown`/`unrecorded` fallbacks and the five-line status count, TASK-015's `Session.profile` fate, and
  `DESIGN.md:423`. Dependency edges corrected: TASK-010/013/014/015 now pre-request TASK-005/TASK-008
  only. Base commit corrected to 1f7e0c6.
- 2026-11-18: audit round 2 (plan-auditor) returned `not_ready` with 2 blockers and 2 majors; all
  applied. (B1) `cgraph index` could no longer bootstrap: `index` stays a reserved verb routed to
  `runWorker` (an absent store short-circuits every tool), so TASK-008/009 now keep
  `RESERVED = [mcp, worker, index]`, TASK-009 adds `--help` handling for verbs, TASK-018 documents the
  bootstrap and TASK-019 bootstraps its fixture through the verb instead of `runWorker`. (B2) the gate
  sweeps contradicted the tests they require: all sweeps are now production-side (`app/src`, `app/bin`,
  `app/docs`, `app/README.md`, `docs/my-product`), `app/tests/**` is explicitly exempt, and
  TASK-010/011/012/017/018/020/021 verifies were rewritten with `! grep` so they exit 0 when the
  criterion holds. Majors: TASK-016 now names the six-tool sentence, the `with six tools` comment and
  the test names/comments to reword; TASK-015 itemises the remaining `profile` sites in
  `tools-smoke.test.ts`. Minors: TASK-008 deletes the `type Profile` import and swallows `splitGlobals`
  errors as usage; TASK-019 fixes case 6 positions/equals form, asserts the ten envelope members and
  adds an `index --help` case; objective Done-when 3/5/6 restated for the bootstrap, the test exemption
  and a generation comparison.
