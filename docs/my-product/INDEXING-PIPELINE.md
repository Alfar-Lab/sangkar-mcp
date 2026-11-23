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

## 3. Policy: the machine defaults and the per-repo override

Two files, one precedence. `<home>/config.json` (`shared/global-config.ts`) is the installation's
default and holds more than a repository can: the three repository keys below, the report server's
default address, and the two budgets. `<repo>/.sangkar/config.json` (`shared/repo-config.ts`) narrows
the three repository keys for one tree. A wider override wins: command-line flag, then environment
variable, then the machine file, then the built-in default.

### 3.0 The machine file: `<home>/config.json`

Written with every default by the first real run, hand-editable afterwards, and resolved from the file
whenever a process loads its configuration. The daemon's long-lived process reads it once at start, so
an edit retunes it on the next start rather than on the next reconcile tick — the per-repo file is the
one that is re-read every tick. The registry beside it, `sangkar.json`, is §2.

```json
{
  "watch": true,
  "watch_interval": 2000,
  "telemetry": true,
  "daemon": { "host": "127.0.0.1", "port": 7317 },
  "budgets": { "default_tokens": 1200, "max_tokens": 32000 }
}
```

`daemon.host` and `daemon.port` are what `daemon run` and `telemetry --serve` bind when `--host` /
`--port` are absent, so `daemon start` — which passes neither — agrees with the foreground run. The
loopback policy is `telemetry/serve.ts`'s: a non-loopback host is refused before the socket binds. A
port must be an integer in 0..65535, where `0` asks the kernel for one. `budgets.default_tokens` is the
fallback for a call that sends none and is clamped to `max_tokens`, which is the ceiling above which a
call is refused `budget_too_large`. The file can lower that ceiling, never raise it: `budget.ts`'s
32000 is a hard limit, so a larger `max_tokens` is clamped to it.

An absent file is the fresh installation and is silent; an unreadable one, invalid JSON, or a field of
the wrong type is one warning naming the path and the field, with that field's built-in default
substituted. A missing key is not a warning: this file is written by hand, so a partial file is
expected. `watch_interval` is the exception on both sides of the two files: an out-of-band number is
clamped and a nonsensical one takes the default without a warning, because the interval is a
measurement rather than a consent.

### 3.1 Per-repo policy: `<repo>/.sangkar/config.json`

The daemon re-reads this file on every reconcile tick; the server reads it once per process, next to
`sangkar.json`. It is the file that replaced the per-tree environment variables, and each key it omits
inherits the machine value from §3.0.

```json
{ "watch": true, "watch_interval": 2000, "telemetry": true }
```

| Field | Accepted | Otherwise |
|---|---|---|
| `watch` | a JSON boolean — `false` means the daemon reports the repo as `skipped` and holds no watcher for it | the machine value, one warning |
| `watch_interval` | an integer, clamped into **1000–300000 ms** | a non-integer, a zero, a negative or a wrong type takes the machine interval |
| `telemetry` | a JSON boolean — narrows recording for this tree | the machine value, one warning |

Both ends of the interval are clamped rather than honoured: a number outside the band is a mistake in
the file, not a reason to stop watching. A missing, unreadable or unparseable file is not an error
either — it is the normal state of a repository that was indexed but never configured, and it degrades
to all three machine values with one warning naming the path and the reason. Both switches only ever
narrow what the machine allows: `telemetry: true` here cannot opt a tree in past the installation's
consent switch, and an unregistered tree is never recorded whatever its file says.
