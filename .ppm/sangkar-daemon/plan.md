# sangkar-daemon

Created: 2026-11-19
Last updated: 2026-11-19
Base commit: 9cedf8a

## Files
- plan.objective.md: objective, scope, Done when
- plan.constraint_invariant.md: constraints and invariants
- plan.approach.md: current state, design, phases
- plan.risk.md: risks and open questions

## Changelog
- 2026-11-19: created
- 2026-11-19: first independent audit (NOT READY, 5 blocking / 5 major); tasks amended: `--repo-root`
  precedence, index-verb registration, recorder retirement, daemon tick/watcher seam, lock-before-bind,
  SANGKAR_HOME hermeticity in the suites that index through the CLI, plus new task `TASK-027` (27 tasks,
  3 phases). The owner fixed the topology (D1 sidecar daemon, thin
  per-session `sangkar mcp`), the global home (D2 `$SANGKAR_HOME`), both file shapes (D3 registry, D4
  per-repo config), auto-registration (D5), the `repo` argument and its default resolution (D6), cursor
  repo binding (D7), the per-root reader pool (D8), the daemon singleton and reconcile loop (D9), the
  telemetry precedence and loopback report server (D10), the new `daemon-privileges` rule (D11), the
  ad-hoc path (D12), the config split (D13) and the in-commit documentation set (D14). Those are
  requirements, not options.
- 2026-11-19: worktree note — `agent/SKILL.md` carries one pre-existing uncommitted line
  (`name: use-sangkar-mcp`). TASK-022 owns that file and must preserve it; nothing in this plan reverts
  it, and the closing gate checks it is still modified.
