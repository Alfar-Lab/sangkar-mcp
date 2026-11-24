# rust-migration

Created: 2026-11-24
Last updated: 2026-11-24
Base commit: 3592da3

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: parity bar, module boundaries, verification environment
- plan.approach.md: verified current state, design decisions, phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-24: created from a full inventory of `src/` + `bin/` (76 files, 25 773 lines), `tests/` (60 files, 31 338 lines), the 11-table schema, the 12 language rows and the two surfaces. Design record: `docs/my-product/RUST-MIGRATION.md`.
- 2026-11-24: first audit closed three coverage gaps (`TASK-070` store lifecycle, `TASK-071` `repo` verb, `TASK-072` index gate and `--yes`), split `runtime.rs` into `TASK-073`, added `TASK-074` for the shipped agent skill, and fixed the parity bar where it was stronger or weaker than the code: rowid order now compared as sequences (`TASK-048`), the telemetry field rename withdrawn, exit `70` scoped to the npm shim, grammar crates corrected to eleven. 68 → 73 tasks.
- 2026-11-24: maintainer decisions recorded — `0.6.0-beta.1` stays the published artifact until the Phase 5 gate; the first Rust release is **Linux x64 only** (one platform package, one release target, a shim that refuses other platforms by name and names the version to pin); the package keeps the name `sangkar-mcp`. Narrowed `TASK-058`, `TASK-059`, `TASK-060`, `TASK-065`, `TASK-074` and `TASK-068`; risk R7 rescoped to glibc and the shim refusal; open questions 1–4 answered, with the cutover version and the glibc floor still open. Plan stays idle until the maintainer starts it.
