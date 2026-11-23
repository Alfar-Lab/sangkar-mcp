## sangkar-mcp

A read-only MCP server over one repository's call graph, and the `sangkar` command that serves the
same seven tools from a shell. It indexes the tree into a SQLite database and answers four things:
what state the graph is in, what is connected to a symbol, what a change reaches, and what the index
could not cover. Each call answers from one repository — `repo`, a registered path or basename,
selects it; omitting the argument takes the working directory's repository, or the only registered
one.

## Installation & Requirements

### Requirements

- **Bun 1.4.4 or newer.** This is a Bun program — `bun:sqlite`, `bun build`, `Bun.spawn` — so Node
  cannot run it. The package ships TypeScript and the `sangkar` binary runs under Bun through its
  shebang, so the requirement is not optional.
- Installing creates no database. The first run writes one settings file, `<home>/config.json` beside
  the registry, and nothing else outside Bun's global directory. Reading a graph needs no network, no
  server and no index step: the daemon keeps it fresh, the server only reads.

### Install

```sh
bun add -g sangkar-mcp@beta   # or: npm i -g sangkar-mcp@beta
```

Pin `@beta` rather than the bare name: `latest` is a moving target, because npm created a
`0.0.0-stage` holding version that took the tag when the name was first claimed. Today `beta` serves
`0.3.0-beta.2`; `0.4.0-beta.1` is tagged and released on GitHub and reaches the registry on the next
publish, which needs the maintainer's OTP. Before it lands:

```sh
bun install -g github:Alfar-Lab/sangkar-mcp#v0.4.0-beta.1
```

### From a checkout

```sh
bun install
bun link                            # puts `sangkar` on PATH, and touches nothing else
bun run bin/sangkar.ts status       # or skip bun link: the same program, invoked directly
```

### First index

`sangkar index` is the bootstrap: the one command that builds the graph with no client attached and
no file change needed to start it. It is the same work as the MCP `index` tool, with identical
arguments.

```sh
cd /path/to/repo
sangkar index      # index the tree into <repo>/.sangkar/graph.db
sangkar status     # five human lines, or the envelope with --json
```

Until it has published once there is no store to answer from, so every other command answers
`bootstrapping` and exits 1. `status` is the exception: it prints the one line naming the missing
database and exits 0, because "no index yet" is the fact it exists to report.

The repository is found by walking up from the current directory: the nearest ancestor holding
`.sangkar`, else the nearest holding `.git`, else the current directory itself. `--repo-root` states
it explicitly, which is what a script should do.

### Where the data lives

One directory per working tree, beside the code it describes, and one directory per installation
for what belongs to the machine rather than to a tree:

```
${XDG_CONFIG_HOME:-$HOME/.config}/sangkar/
  config.json    this installation's defaults, created on the first run
  sangkar.json   the registry of registered trees
  daemon.pid     the daemon's singleton lock
  daemon.log     the running daemon's log
```

And, per tree:
```
<repo>/.sangkar/graph.db          the graph
<repo>/.sangkar/config.json       watch and telemetry settings, written by sangkar repo config
<repo>/.sangkar/graph.db-wal      WAL sidecars, never committed
<repo>/.sangkar/graph.db-shm
<repo>/.sangkar/write.lock        single-writer lock, outside the database
<repo>/.sangkar/.gitignore        a single `*`, written on first index
```

The data dir carries its own `.gitignore` holding one `*`, so git ignores everything inside without
the repository's tracked `.gitignore` being edited, and `.sangkar` is excluded from discovery the way
`.git` is — otherwise each index would index the previous database. One database belongs to one
working tree, including a linked `git worktree`, reported at `status.payload.project.db_path`; a data
dir holding an index of a different tree is refused with `wrong_project_db`, so a mis-pointed
override is an error, not a quietly wrong answer.

`--data-dir` is the only override, and a relative value resolves against the repository root. It
exists because the checkout is not always somewhere you can write:

- **read-only mounts.** A source tree mounted read-only, an image layer, or a CI checkout cannot hold
  `.sangkar/` at all. Indexing into a writable directory leaves the tree byte-for-byte untouched.
- **network filesystems.** SQLite needs working locks and a local filesystem for WAL; NFS, SMB, most
  FUSE mounts and synced folders do not provide that, and a half-checkpointed `graph.db-wal` is a
  corrupt graph. Keep the database on local disk and serve the tree over the mount.

### Global settings

The first run writes `<home>/config.json` with every default spelled out, and it is meant to be
edited by hand afterwards:

```json
{
  "watch": true,
  "watch_interval": 2000,
  "telemetry": true,
  "daemon": { "host": "127.0.0.1", "port": 7317 },
  "budgets": { "default_tokens": 1200, "max_tokens": 32000 }
}
```

| Key | Effect |
|---|---|
| `watch` | whether the daemon watches a registered tree that has no `watch` of its own; `false` reports it `skipped` |
| `watch_interval` | that tree's poll period in milliseconds, clamped to 1000–300000 |
| `telemetry` | the installation's consent switch: `false` disables usage recording for every repository |
| `daemon.host` / `daemon.port` | what `sangkar daemon run` and `sangkar telemetry --serve` bind when the flag is absent, so `daemon start` and the foreground run agree; loopback only, `0` asks the kernel for a port |
| `budgets.default_tokens` / `budgets.max_tokens` | the fallback per-call budget and the ceiling above which a call is refused `budget_too_large`; `default_tokens` is clamped to `max_tokens`, and `max_tokens` can only lower the hard 32000 ceiling |

A repository's own `<repo>/.sangkar/config.json` narrows these three repository keys for that tree —
`watch`, `watch_interval`, `telemetry` — and `sangkar repo config <path>` writes it. A missing or
malformed key takes the machine value, never a silently invented one. Precedence, widest override
first: command-line flag, then environment variable, then this file, then the built-in default.

### Environment

Three settings that a launcher, not the file, sets — so they outrank it:

| Variable | Default | Meaning |
|---|---|---|
| `SANGKAR_CURSOR_KEY` | empty | HMAC key for pagination cursors, 32 bytes or more; when unset the process generates its own key, so cursors do not survive across invocations — set it to share cursors between processes |
| `SANGKAR_HOME` | `${XDG_CONFIG_HOME:-$HOME/.config}/sangkar` | the machine-level home, holding `config.json` (the defaults above), `sangkar.json` (the registry), `daemon.pid` and `daemon.log`; an empty value counts as unset |
| `SANGKAR_TELEMETRY` | the file's `telemetry` | usage records under `<dataDir>/telemetry/`, on by default and local only; the exact value `off` disables them, an empty or any other value enables them, and when off no file and no directory is created |

Maintainer detail — how the artifact is distributed, how a release reaches npm, and the check list to
run before one — is in [docs/maintainers.md](docs/maintainers.md).

## Usage

### MCP & Skill

The command a client registers as its stdio server:

```sh
sangkar mcp --repo-root /path/to/repo   # --data-dir /path/to/dir is the optional second flag
```

stdout carries the MCP protocol and nothing else; diagnostics are JSON lines on stderr, one record per
event.

| Tool | What it answers |
|---|---|
| `status` | The generation being served, how stale the tree is, per-path coverage and the language census. Call it before saying something does not exist. |
| `summary` | The situation in a few hundred tokens: the working-tree diff (default), the whole repo, or one area, with risk, blast radius and test gaps. |
| `trace` | Callers, callees, implementers, importers and tests for one query, grouped by edge and tier; several matches come back as candidates, not a guess. |
| `search` | Symbols by keyword (BM25) with file:line and in/out degree — locations and shape, not source. |
| `impact` | What a change set reaches: dependents, affected entry points, the tests that cover them, and a risk score; defaults to the working-tree diff. |
| `expand` | More detail for one item from an earlier answer — `members`, `edges`, short `source`, or `coverage`. |
| `index` | Force a full or fast re-index, or repair a graph the daemon could not fix. Not for normal use. |

Registered names are exactly these seven, in this order: no `_tool` suffix, no aliases, no eighth name.
Six take an optional `budget` in tokens, defaulted per tool (`status` 800, `summary` 1200, `trace`
3200, `search` 2000, `impact` 2500, `expand` 2500); `index` has none, because a repair call has no
payload worth paging. Above the installation's ceiling — `budgets.max_tokens`, 32000 unless the
global config lowers it — and below the smallest size an answer can take, the call is refused with
`budget_too_large` and a hint naming the floor.

A tree is watched only once it is registered, and registration leaves the tree untouched:

```sh
sangkar repo add .            # register this tree in ${XDG_CONFIG_HOME:-$HOME/.config}/sangkar/sangkar.json
sangkar daemon start          # one process: a watcher per registered repo, plus the report server
sangkar telemetry --serve     # that server's page: every registered repo, on 127.0.0.1:7317
```

`sangkar repo list`, `repo remove <path>` and `repo config <path>` edit the same registry; `config`
writes `<path>/.sangkar/config.json` — the three per-repo switches (watch, watch interval 1000–300000
ms, telemetry), which the daemon re-reads every five seconds. An unregistered tree is still usable one
command at a time by naming it with `--repo-root`, but its graph is only as fresh as the last
`sangkar index`.

A linked worktree is a second tree with a second database, so a client registers it as a second
`mcpServers` entry with its own paths:

```json
{
  "mcpServers": {
    "sangkar-app": {
      "command": "~/.bun/bin/bun",
      "args": ["run", "/path/to/bin/sangkar.ts", "mcp",
               "--repo-root", "/work/app", "--data-dir", "/work/app/.sangkar"]
    }
  }
}
```

Add a second entry beside it (`sangkar-app-feature`) with `--repo-root /work/app.feature` and
`--data-dir /work/app.feature/.sangkar`; sharing one data dir is not a merge, and the second server
answers `wrong_project_db` naming the tree the graph belongs to.

`agent/SKILL.md` is a portable [Agent Skills](https://agentskills.io/specification) skill telling an
agent which tool to call, in what order, and how to read the envelope it gets back. `agent/install.sh`
symlinks it into the skill directories of Claude Code, Codex, OpenCode and Pi (`--scope project`
writes a `.agents/skills`-style path inside a repository instead):

```sh
./agent/install.sh --dry-run && ./agent/install.sh
```

See [agent/README.md](agent/README.md) for the per-harness paths and the server registration each
client needs before the tools exist at all.

### CLI

```sh
sangkar <tool> [arguments] [global flags] — the name comes first
```

| Example | What it does |
|---|---|
| `sangkar status` | Five human lines: the status line, then `project`, `database`, `graph`, `health`. |
| `sangkar status --json` | The same answer as the envelope. |
| `sangkar search 'handleRequest'` | Symbols matching a keyword. |
| `sangkar trace 'db.write' --depth 1 --direction out` | Callees of one symbol, one hop. |
| `sangkar impact` | What the working-tree diff reaches. |
| `sangkar index --mode fast --force` | Force a fast re-index. |
| `sangkar repo add .` / `sangkar daemon start` | Register this tree, then start the daemon that watches it. |
| `sangkar trace --help` | `trace`'s own usage, exit 0. |

- Every tool prints one JSON envelope, and `--json` is accepted by all of them; it is `status` alone
  that swaps the envelope for human lines when the flag is absent.
- `--repo-root <path>` and `--data-dir <path>` are globals: they follow the tool name and may sit
  anywhere among that tool's own arguments, in `--flag value` or `--flag=value` form, and `--json` and
  `--help` are globals in the same sense. A global written before the name is read as the tool name
  and refused with exit 2.
- `--help` exits 0 with that tool's usage block, or, on a verb, the verb's own usage line — `sangkar
  daemon --help`, `sangkar repo --help`, and so on — answered before any argument is checked.
- `sangkar index` streams progress as JSON log lines on stderr and exits 0 once it publishes. Nothing
  else reaches stdout.
- Exit codes, from the envelope's status: `0` for `ok`, `ambiguous` and `not_found` — a negative
  answer is still an answer — `1` for `error` and `bootstrapping`, `2` for a usage refusal. `sangkar
  index` has two of its own, because it is the worker rather than a tool call: `2` when nothing
  changed and there was nothing to publish, `75` when another indexer holds the lock.

## License

MIT — see [LICENSE](LICENSE). The grant covers the package as published, including the migration SQL
it ships; `tree-sitter-wasms`, `web-tree-sitter`, `drizzle-orm` and the MCP SDK keep their own
licenses.

## Future Works

Measured on a scratch index of this tree (2026-11-19, after phase 1 of `issue-remediation`): 1 640
nodes, 9 347 edges (5 894 `calls`), 11 519 unresolved call sites, 115 `not_indexed` paths — all
`unsupported_ext`, no source file among them.

1. **`kind: "test"` is not applied.** 547 nodes carry `is_test = 1` but only 12 carry `kind = "test"`,
   and 403 of the 547 are labelled `function`: extraction reaches `test` only through the name regex
   on the declaration, never from `is_test`. Every payload that renders `kind` mislabels test
   functions, and `search(kind='test')` is nearly empty.
2. **`kind: "method"` is nearly unused.** 422 nodes are labelled `class` and 31 `method`; most of the
   422 are TypeScript interfaces, type aliases and enums, which the language tables list as class
   types.
3. **Binding recall is the bottleneck.** 11 519 unresolved call sites against 5 894 `calls` edges
   (≈66 %), and only 368 of 1 640 nodes have a test within two hops. `total_relation` is therefore
   `gte` almost everywhere and `test_gaps` under-reports: absence answers are honest but thin.
4. **Grammar WASM is not embedded in the compiled binary.** Grammars load from `node_modules` at index
   time, so `dist/sangkar` serves an index but cannot build one. Compiling the 12-language set in is
   what the distribution plan waits for.
5. **`0.4.0-beta.1` is not on npm yet.** The `beta` tag still serves `0.3.0-beta.2`; the publish needs
   the maintainer's OTP. Until then, install from the tag.
