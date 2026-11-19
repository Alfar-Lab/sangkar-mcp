# sangkar-telemetry

Created: 2026-11-18
Last updated: 2026-11-18
Base commit: ed16170

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: approach and phases (holds the normative record schema)
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-18: created; 4 phases, 22 tasks. The owner fixed storage (JSONL under `<dataDir>/telemetry/`),
  consent (`SANGKAR_TELEMETRY=off`, on by default, local only), the identifier model (daily-rotated install
  id + per-process session id), and the record field lists; those are requirements, not options.
- 2026-11-18: planning-time verification found one break the brief did not name: `AppConfig` has a
  hand-written object literal at `tests/tools-index.test.ts:108`, so adding a required `telemetry` field
  breaks typecheck there; that file joins the config task's ownership. It also found that the brief's
  "`src/shared/config.ts` is the ONLY reader of the environment" is not literally true
  (`src/index.ts:595-597` reads `SANGKAR_NO_WATCH`, deliberately) — `SANGKAR_TELEMETRY` still goes into
  config because it gates a process-wide behaviour, not a root-only one.
- 2026-11-18: phase boundaries are file-disjoint, so the `telemetry` verb dispatches from `bin/sangkar.ts`
  (which already owns `RESERVED`, `USAGE` and `VERB_USAGE`) instead of `main()`'s switch in `src/index.ts`.
  That keeps `src/index.ts` owned by exactly one phase (phase_1) and `bin/sangkar.ts` by exactly one
  (phase_2). Rationale and the rejected alternative are in plan.approach.md §Design 8.
- 2026-11-18: second audit's non-blocking fixes applied; task content changed for TASK-002, TASK-003, TASK-006,
  TASK-008, TASK-009, TASK-010, TASK-012, TASK-013, TASK-014, TASK-015, TASK-016, TASK-017, TASK-019,
  TASK-020, TASK-021 and TASK-022, plus plan.approach.md, plan.risk.md and plan.objective.md. The execution
  model is fixed: **nothing is committed between tasks or phases**, so every per-task `git status` check now
  reads "no path outside this task's own files" instead of "only this task's files exist", and the two
  commit-range diffs became base-to-worktree diffs. Content fixes: the phase_0 gate whitelists three test
  files; `TelemetryRecord = CallRecord | IndexRunRecord | SessionRecord` is declared in `record.ts` (TASK-012
  and TASK-016 import it); six tools write prose into `stale_reason`, not five; `parseArgs`' undeclared-key
  drop is the loop at `args.ts:200-218`; the exact-listing citation is `publish.test.ts` + `smoke.test.ts`
  only (`worker.test.ts:141` is a `.stage.` filter); TASK-009 hoists `mode` without re-resolving it, and
  TASK-010 Case A3 pins `mode: "fast"` on a default run over a changed tree; TASK-019 also fixes `README.md`'s
  "Two settings remain" sentence; TASK-021 marks the third plan in progress and TASK-022 owns the completion
  claim it writes only when the gate is green; TASK-006's vacuous "no directory" clause was dropped in favour
  of TASK-010 Case B.
- 2026-11-18: audit remediation. Three blockers fixed: `src/shared/telemetry/sink.ts` had no owning task
  (TASK-005) and was missing from the phase_0 gate whitelist (TASK-007); `arg_names_set` was specified from
  the defaulted `parseArgs` record, which makes it a constant (TASK-002, TASK-008, TASK-010);
  `stale_reason` was copied verbatim from an envelope that carries prose with a repo path in it
  (TASK-002, TASK-006, TELEMETRY.md §3). Eleven non-blocking findings also applied: a client-supplied tool
  name and a mislabelled status in the `call` record (TASK-008, TASK-010), a record `v` and a reader skip
  rule (TASK-002, TASK-012), the incoherent `samples` parameter (TASK-012), concurrent appends between the
  server and its worker (TASK-005, plan.constraint_invariant.md), the full suite in the phase_0 gate
  (TASK-007), two weak assertions in the telemetry suite (TASK-006), consent disclosure in `README.md` and
  `bin/sangkar.ts` (TASK-019), `lang` and the drifted line citations, and the `bin/` gap in the boundary
  walk (plan.constraint_invariant.md).
