# sangkar

A read-only MCP server over one repository's call graph, and the `sangkar` command that serves the
same seven tools from a shell. It indexes the tree into a SQLite database and answers what changed,
who calls what, what a change reaches, and what the index could not cover. The repository is bound
at startup, so no tool takes a project argument.

## Install

Bun 1.4.4 or newer. The app is a Bun program — `bun:sqlite`, `bun build`, `Bun.spawn` — so Node
cannot run it.

```sh
bun add -g sangkar-mcp@beta   # or: npm i -g sangkar-mcp@beta
```

`0.1.0-beta.2` publishes under the `beta` tag. Pin it explicitly — `latest` on the registry is a
moving target: when npm first claimed this name it created a `0.0.0-stage` holding version that took
`latest`, and the real version took it over on publish, so an untagged install happens to get the
prerelease today. That is a property of the registry's staged-publish flow, not of this package, and
it stops being true as soon as a non-prerelease ships. `sangkar-mcp@beta` is the stable address.

The package ships TypeScript, so the `sangkar` binary runs under Bun through its shebang; the Bun
prerequisite is not optional. Installing creates no database and writes nothing outside Bun's global
directory.

### From a checkout

```sh
bun install
bun link            # puts `sangkar` on PATH
```

`bun link` is the only global step: it registers `bin/sangkar.ts` as `sangkar` and touches nothing
else. Skip it and invoke the entry point directly, which is the same program:

```sh
bun run bin/sangkar.ts status
```

Installing creates no database and writes nothing outside `node_modules`.

## First run

`sangkar index` is the bootstrap: the one command that builds the graph by itself, with no client
attached and no file change needed to start it.

```sh
cd /path/to/repo
sangkar index        # index the tree into <repo>/.sangkar/graph.db
```

It is the same work as the MCP `index` tool, with identical arguments. Until it has published once
there is no store for a tool call to answer from, so every other command answers `bootstrapping`
and exits 1. `status` is the one exception: it prints the single line naming the missing database
and exits 0, because "no index yet" is the fact it exists to report. Once the graph exists, any tool
answers:

```sh
sangkar status
```

The repository is found by walking up from the current directory: the nearest ancestor holding
`.sangkar`, else the nearest holding `.git`, else the current directory itself. `--repo-root`
states it explicitly, which is what a script should do:

```sh
sangkar status --repo-root /path/to/repo
```

## CLI usage

```sh
sangkar <tool> [arguments] [global flags] — the name comes first
```

| Example | What it does |
|---|---|
| `sangkar status` | The five human lines described below. |
| `sangkar status --json` | The same answer as the envelope. |
| `sangkar search 'handleRequest'` | Symbols matching a keyword. |
| `sangkar trace 'db.write' --depth 1 --direction out` | Callees of one symbol, one hop. |
| `sangkar impact` | What the working-tree diff reaches. |
| `sangkar index --mode fast --force` | Force a fast re-index. |
| `sangkar trace --help` | `trace`'s own usage, exit 0. |

Output:

- `status` prints five human lines — the status line, then `project`, `database`, `graph` and
  `health`. On a tree with no index yet it prints the one-line reason instead, and exits 0.
- Every other tool prints one JSON envelope.
- `--json` is accepted by any tool and is the default output for all of them; it is `status` alone
  that swaps the envelope for human lines when the flag is absent.
- `--repo-root <path>` and `--data-dir <path>` are globals: they follow the tool name and may sit
  anywhere among that tool's own arguments, in `--flag value` or `--flag=value` form; `--json` and
  `--help` are globals in the same sense. The name comes first — a global written before it is read
  as the tool name and refused with exit 2.
- `--help` exits 0 with that tool's usage block, or, on a verb, the verb's own usage line:
  `sangkar mcp --help`, `sangkar index --help`, `sangkar worker --help`. It is answered before any
  argument is checked, so it works with a required argument still missing.
- `sangkar index` streams progress as JSON log lines on stderr and exits 0 once it publishes.
  Nothing else reaches stdout.

Exit codes, from the envelope's status: `0` for `ok`, `ambiguous` and `not_found` — a negative
answer is still an answer — `1` for `error` and `bootstrapping`, and `2` for a usage refusal
(unknown tool, unknown option, missing value, bad type). `sangkar index` has two of its own, because
it is the worker rather than a tool call: `2` when nothing changed and there was nothing to publish,
and `75` when another indexer holds the lock.

## MCP

The command a client registers as its stdio server:

```sh
sangkar mcp --repo-root /path/to/repo
```

`--data-dir /path/to/dir` is the optional second flag. stdout carries the MCP protocol and nothing
else; diagnostics are JSON lines on stderr, one record per event.

## Agent skill

`agent/SKILL.md` is a portable [Agent Skills](https://agentskills.io/specification) skill that tells
an agent which of the seven tools to call, in what order, and how to read the envelope it gets back.
`agent/install.sh` symlinks it into the skill directories of Claude Code, Codex, OpenCode and Pi
(`--scope project` writes a `.agents/skills`-style path inside a repository instead):

```sh
./agent/install.sh --dry-run
./agent/install.sh
```

See [agent/README.md](agent/README.md) for the per-harness paths and the server registration each
client needs before the tools exist at all.

## The seven tools

| Tool | What it answers |
|---|---|
| `status` | The generation being served, how stale the tree is, per-path coverage and the language census. Call it before saying something does not exist. |
| `summary` | The situation in a few hundred tokens: the working-tree diff (default), the whole repo, or one area, with risk, blast radius and test gaps. |
| `trace` | Callers, callees, implementers, importers and tests for one query, grouped by edge and tier; several matches come back as candidates, not a guess. |
| `search` | Symbols by keyword (BM25) with file:line and in/out degree — locations and shape, not source. |
| `impact` | What a change set reaches: dependents, affected entry points, the tests that cover them, and a risk score; defaults to the working-tree diff. |
| `expand` | More detail for one item from an earlier answer — `members`, `edges`, short `source`, or `coverage`. |
| `index` | Force a full or fast re-index, or repair a graph the watcher could not fix. Not for normal use. |

Registered names are exactly these seven, in this order: no `_tool` suffix, no aliases, and no
eighth name. MCP and the `sangkar` command publish the same surface; the instructions tell an agent
to call `index` only when `status` reports a broken graph.

Six of the seven take an optional `budget` in tokens, defaulted per tool (`status` 800, `summary`
1200, `trace` 3200, `search` 2000, `impact` 2500, `expand` 2500); `index` has none, because a
repair call has no payload worth paging. Above 32000 the call is refused with `budget_too_large`; so
is a budget below the smallest size an answer can take, with a hint naming that floor — one refusal
instead of a payload several times what you asked for.

## `.sangkar`

One directory per working tree, beside the code it describes:

```
<repo>/.sangkar/graph.db          the graph
<repo>/.sangkar/graph.db-wal      WAL sidecars, never committed
<repo>/.sangkar/graph.db-shm
<repo>/.sangkar/write.lock        single-writer flock, outside the database
<repo>/.sangkar/.gitignore        a single `*`, written on first index
```

The data dir carries its own `.gitignore` holding one `*`, so git ignores everything inside without
the repository's tracked `.gitignore` being edited. `.sangkar` is excluded from discovery the way
`.git` is — otherwise each index would index the previous database. One database belongs to one
working tree, including a linked `git worktree`; the database answering the session is always
reported at `status.payload.project.db_path`.

A linked worktree is therefore a second tree with a second database, and a client registers it as
a second server — one `mcpServers` entry each, each passing its own `--repo-root` and its own
`--data-dir`:

```json
{
  "mcpServers": {
    "sangkar-app": {
      "command": "~/.bun/bin/bun",
      "args": [
        "run", "/path/to/bin/sangkar.ts", "mcp",
        "--repo-root", "/work/app",
        "--data-dir", "/work/app/.sangkar"
      ]
    },
    "sangkar-app-feature": {
      "command": "~/.bun/bin/bun",
      "args": [
        "run", "/path/to/bin/sangkar.ts", "mcp",
        "--repo-root", "/work/app.feature",
        "--data-dir", "/work/app.feature/.sangkar"
      ]
    }
  }
}
```

Sharing one data dir between the two is not a merge: the second server opens a graph recorded against
a different root and answers every call with `wrong_project_db` — `the graph at … belongs to
/work/app, not /work/app.feature` — plus the hint to point `--data-dir` at its own `.sangkar`. That is
the guard whose `root_mismatch` case treats a different tree still sitting there as a refusal rather
than a disclosure, because serving another tree's graph is the worst answer this design could give.

## Moving the data directory

`--data-dir` is the only override; a relative value resolves against the repository root. It exists
because the checkout is not always somewhere you can write:

- **read-only mounts.** A source tree mounted read-only, an image layer, or a CI checkout cannot hold
  `.sangkar/` at all. Indexing into a writable directory leaves the tree byte-for-byte untouched.
- **network filesystems.** SQLite needs working locks and a local filesystem for WAL; NFS, SMB, most
  FUSE mounts and synced folders do not provide that, and a half-checkpointed `graph.db-wal` is a
  corrupt graph. Keep the database on local disk and serve the tree over the mount.

A data dir holding an index of a different tree is refused rather than answered from, so a
mis-pointed override is an error, not a quietly wrong answer.

## Environment

Three settings remain, and all three are read from the environment rather than a flag:

| Variable | Default | Meaning |
|---|---|---|
| `SANGKAR_CURSOR_KEY` | empty | HMAC key for pagination cursors, 32 bytes or more; when unset the process generates its own key, so cursors do not survive across invocations — set it to share cursors between processes |
| `SANGKAR_NO_WATCH` | unset | any value stops the watcher; set by tests |
| `SANGKAR_TELEMETRY` | unset | usage telemetry under `<dataDir>/telemetry/`, on by default and local only; the exact value `off` disables it, and when off no file and no directory is created |

## Distribution

The order is TECH-SPEC.md §15's, and it is deliberate:

1. **Ship the Bun package first.** A `#!/usr/bin/env bun` entry point is a small artifact, needs no
   rebuild per platform, and only requires Bun on the user's machine (102 MB itself). This is the
   route the seven tools are exercised through today.
2. **Add the compiled binary once the grammar set is fixed.** `bun build --compile` produces a single
   file with no Bun prerequisite, but it is large and must be rebuilt per platform: 104 MB measured
   for a hello-world on Bun 1.4.4, 105.1 MB for this app (393 modules), and roughly 1–3 MB per
   embedded WASM grammar — an estimated 130–160 MB for the 12-language v1 set. Compiling grammars
   in is what makes the artifact big, so it waits until the language list stops moving.

Two facts about the standalone artifact today:

- **Migrations must be embedded.** The migration runner reads `.sql` files from
  `import.meta.dir/migrations`; inside a compiled binary that directory is the virtual
  `/$bunfs/root`, which holds only what the build put there. Without `--asset` the binary ships zero
  migrations, computes a shipped schema version of 0, and refuses every existing index with
  `schema_too_new`. The working build command is:

  ```sh
  bun build --compile bin/sangkar.ts \
    --asset "$PWD/src/shared/store/migrations" \
    --outfile dist/sangkar
  ```

  `bun run build` passes exactly this flag, so `dist/sangkar` can read and serve an index.
  `dist/sangkar mcp --repo-root /path/to/repo` is the same command line the Bun install answers,
  with the binary in place of `sangkar`, and `dist/sangkar status --repo-root /path/to/repo`
  answers from it too. `tests/binary-smoke.test.ts` builds and drives that artifact, which is the
  evidence the flag is required rather than merely tidy.
- **Grammar WASM is not embedded.** Grammars load lazily from `web-tree-sitter` and
  `tree-sitter-wasms` in `node_modules` at index time, never at startup, so a compiled binary serves
  an index with no `node_modules` beside it but cannot build one. Until the grammars are compiled in,
  indexing stays with the Bun install and the binary serves the graph it produced.

## Publishing to npm

```sh
bun run release:beta          # npm publish --tag beta
```

Two traps, both hit on 2026-11-18 and both proven with `npm publish --dry-run`:

- **`--tag beta` is not optional.** `publishConfig.tag` is *ignored* by npm 11 (it only suppresses
the "you must specify a tag" guard for prereleases): without the flag on the command line the
publish goes to `latest` anyway, which is the one tag a `0.1.0-beta.2` must never take.
- **The `bin` warning is cosmetic.** npm prints
`bin[sangkar] script name bin/sangkar.ts was invalid and removed` when it rewrites the field to
strip a leading `./`; the entry survives as `bin/sangkar.ts`. `npm pkg fix` shows the same result,
so the canonical form is written in `package.json` and the warning no longer appears.

Publishing then needs two-factor authentication: either `--otp <code>` from an authenticator app,
or a granular access token with 2FA bypass in `~/.npmrc`. Without one the registry answers
`E403 ... Two-factor authentication or granular access token with bypass 2fa enabled is required`,
and nothing is published.

## Checks

```sh
bun test                            # fixtures plus temp dirs; no network, no writes into the repo
bun run typecheck                   # tsc --noEmit, strict
bun run lint                        # biome check src tests bin
bun test tests/binary-smoke.test.ts  # builds the standalone artifact and drives one tools/call
```

## License

MIT — see [LICENSE](LICENSE). The grant covers the package as published, including the migration
SQL it ships; `tree-sitter-wasms`, `web-tree-sitter`, `drizzle-orm` and the MCP SDK keep their own
licenses.
