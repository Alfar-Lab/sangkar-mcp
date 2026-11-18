# Risks and open questions

## Risks

- **An MCP client that cannot pass argv.** Removing `CODEGRAPH_REPO_ROOT` moves repo selection into
  `cgraph mcp --repo-root <path>`. Every client in use supports `args`, but a client that only sets
  `env` would silently serve its own cwd. Mitigation: README documents the exact registration line;
  `status.payload.project.db_path` names the database being served, so a wrong tree is visible on the
  first call rather than after a wrong answer.
- **Autodetect picks the wrong ancestor.** A nested repository without an index, or a checkout inside a
  directory that has a stale sibling `.codegraph`, could bind the wrong tree. Mitigation: the walk
  prefers `.codegraph` over `.git` at each level and stops at the first hit, `guardRoot` still refuses a
  database recorded for another tree, and `tests/config.test.ts` plus `tests/cli.test.ts` pin the walk.
- **Test fixtures silently stop pointing at their tree.** A `loadConfig({env:{CODEGRAPH_REPO_ROOT}})` call
  that is missed will fall back to cwd and open the repository's own `app/.codegraph`, so the test could
  pass for the wrong reason. Mitigation: every migrated test passes `--repo-root` explicitly through
  `argv`; the phase_1 closing check greps for the three variable names in `app/tests` and must return
  nothing.
- **Widening the agent surface to `index`.** Measured, an agent called indexing 1 081 times at 12.9 %
  failure. All-seven registration re-exposes that. Mitigation: the instructions keep an explicit rule
  (call `index` only when `status` says the graph is broken, and never as a query); it is now
  prose-level, so this is an accepted, documented trade — not a silent one.
- **`parseArgs` does not clamp `min`/`max`.** It only checks the type; handlers clamp. The CLI inherits
  exactly that behaviour on purpose (parity with MCP). Not fixed here, so no divergence is introduced.
- **`tests/binary-smoke.test.ts` takes minutes** and builds a 105 MB artifact into a temp dir. It stays
  in the phase_1 and phase_2 closing runs; a red result there is investigated, not skipped.
- **Bun is not on `PATH`.** Every verify command must use `$HOME/.bun/bin/bun`; a bare `bun` fails with
  `No such file or directory` and would read as a task failure.
- **The docs tree moved while this plan was written.** `docs/{DATABASE,DESIGN,TECH-SPEC,TOOLS-SPEC}.md`
  became `docs/my-product/*.md` and the reference captures became `docs/research/**`, staged but
  uncommitted at planning time. TASK-020 owns the `docs/my-product/` paths as they exist now; the
  executor must re-check those four paths before dispatch and treat a further move as a plan fix, not
  as an improvisation.

## Open questions

- None blocking. Two items are deliberately left to the user, both outside task scope: running
  `bun link` (or `bun install -g app`) to put `cgraph` on `PATH`, and whether a `cgraph instructions`
  mode should print `INSTRUCTIONS_TEXT` (the constant already exists; no mode calls it today).
- `docs/my-product/DESIGN.md` keeps its historical narrative about why profiles were measured in the
  first place; the task rewrites the normative paragraphs only, and does not erase the measurement.
