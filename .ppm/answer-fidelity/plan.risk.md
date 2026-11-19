## Risks

- **The bucket taxonomy is a judgement, not a measurement.** A wrong allowlist entry can hide a real caller, which is the failure the issue exists to prevent. Mitigation: the allowlist is data in one module with a unit test per entry, `unresolved_material` is the headline rather than the total, and the raw rows stay available through `trace`.
- **`unresolved_edges` gates `total_relation: "eq"`.** Adding fields beside it must not change the downgrade. Mitigation: `envelope.test.ts` and `acceptance.test.ts` assert the `eq`/`gte` behaviour; the closing task runs both.
- **The census could drift from `coverage.unresolved_edges`.** Two numbers describing one fact is the defect #4 reports. Mitigation: the census is computed from the same `UnresolvedRef[]` the assembly already produced, carries `total` equal to `rows.length`, and each tool's test asserts the two are equal in one answer.
- **A path filter can empty a formerly `ok` answer.** `trace 'x' --path tests` on a symbol defined only under `src/` now answers `not_found` instead of returning the symbol. Mitigation: the near-miss list is filtered by the same predicate so the answer still says what exists elsewhere; the tool's test asserts the filtered `not_found` carries no unfiltered candidate.
- **Identity capture adds a git call to the publish path.** A failing `git config` must not fail a publish. Mitigation: identity capture is best-effort — a missing branch or remote records `null`, and `status` reports what it has; no warning is emitted for a repo with no remote.
- **Runtime guard placement can break the MCP handshake.** A diagnostic written to stdout instead of stderr would corrupt the protocol. Mitigation: the guard is a pure function whose test asserts the rendered line, and the wiring test asserts stdout is empty.
- **This plan deliberately leaves five real defects open.** #1, #2, #4, #6, #7 are confirmed and documented on their issues but not fixed here; a reader of this plan could mistake the tree for healthy. Mitigation: `plan.objective.md` lists them under Out, and the closing report repeats them as remaining findings.

## Open questions

- **Does `next_call` belong in the candidate object or beside it?** Decided in favour of inside the candidate: an agent that needs it has the candidate in hand. Not blocking.
- **Should `status` report buckets for the whole repo or only the requested paths?** Decided: only the requested paths, matching `skipped`'s existing scope semantics; a repo-wide census is #4's problem, not this plan's. Not blocking.
- **Session-level git operations** (commit, push, tagging the release) are outside this plan and need explicit approval when they come up. Blocking for shipping, not for execution.
