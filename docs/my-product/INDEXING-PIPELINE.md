# Indexing pipeline: triggers, registration, and the per-repo policy file

**Scope:** who starts an index run, what a successful run does to the machine registry, and where a
repository's watch policy comes from. The stages are deliberately not restated here — `DESIGN.md` §10
owns the lifecycle and the publish protocol, `DATABASE.md` §5 owns the write path, and
`docs/research/INDEXING-PIPELINE.md` is the CRG/CBM comparison this design was drawn from.

---

## 1. Who triggers a run

Every way in ends in the same child, `sangkar worker` (`bin/sangkar.ts`). No tool handler indexes
in-process, and no process writes a graph except that child.

| Trigger | Started by | What it is |
|---|---|---|
| the per-repo watcher | `sangkar daemon run` | one watcher per registered, watchable repository; it polls `git HEAD` + mtimes, debounces, and spawns one `sangkar worker` per change |
| `sangkar index` | the operator | the first index, or a forced repair — works with no daemon running |
| `sangkar index --no-register` | the operator | the same run, with the registry left exactly as it was |
| the `index` tool | a `sangkar mcp` session | the repair path; the same child, spawned by the root that owns the process |

An MCP session never polls a tree. The daemon owns every watcher, which is what keeps a session's cost
per call independent of how many repositories are registered.

## 2. A successful run registers the repository

Registration is a convenience for the next call, so it is never allowed to fail the run that earned it.

- **What counts as success:** exit 0 (published) or exit 2 (nothing to do). Exit 1, exit 75 (another
  index holds the lock) and a rejected spawn do not register: a repository the next call cannot read is
  worse than one nobody added.
- **The `index` verb** registers after the run, unless `--no-register` was given.
- **The `index` tool** registers after its envelope is mapped, when the `register` argument is on (it
  defaults to `true`; `--no-register` turns it off).
- **`worker` never registers.** It is spawned by a server for a tree the registry already names, and a
  watcher run must never resurrect a repository an operator removed.
- **A failed registration is one warning.** The write is logged as `index.register_failed` with the
  reason, and the run keeps the exit code and the envelope it already had. `registerRepo` swallows the
  IO error and answers `added: false`, so the caller checks that the returned registry contains the
  root instead of trusting the flag — "already registered" reports `added: false` too.
- **The write is atomic:** a temp file plus a `rename()` of `$SANGKAR_HOME/sangkar.json`, so the daemon
  reading that file sees the previous list or the new one, never a truncated registry.

## 3. Per-repo policy: `<repo>/.sangkar/config.json`

The daemon re-reads this file on every reconcile tick; the server reads it once per process, next to
`sangkar.json`. It is the file that replaced the per-tree environment variables.

```json
{ "watch": true, "watch_interval": 2000, "telemetry": true }
```

| Field | Accepted | Otherwise |
|---|---|---|
| `watch` | a JSON boolean — `false` means the daemon reports the repo as `skipped` and holds no watcher for it | default `true`, one warning |
| `watch_interval` | an integer, clamped into **1000–300000 ms** | a non-integer, a zero, a negative or a wrong type takes `2000` ms, one warning |
| `telemetry` | a JSON boolean — narrows recording for this tree | default `true`, one warning |

Both ends of the interval are clamped rather than honoured: a number outside the band is a mistake in
the file, not a reason to stop watching. A missing, unreadable or unparseable file is not an error
either — it is the normal state of a repository that was indexed but never configured, and it degrades
to all three defaults with one warning naming the path and the reason. Both switches only ever narrow
what the machine allows: `telemetry: true` here cannot opt a tree in past the installation's consent
switch, and an unregistered tree is never recorded whatever its file says.
