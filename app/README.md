# codegraph

A read-only MCP server over one repository's call graph. It indexes the tree into a SQLite database
and answers seven tools about it on stdio: what changed, who calls what, what a change reaches, and
what the index could not cover. The repository is bound at startup, so no tool takes a project
argument.

## Install

Bun 1.4.4 or newer. The app is a Bun program — `bun:sqlite`, `bun build`, `Bun.spawn` — so Node
cannot run it.

```sh
cd app
bun install
```

Installing creates no database and writes nothing outside `app/node_modules`.

## First run

```sh
cd app
bun run index     # index the tree into <repo>/.codegraph/graph.db
bun run mcp       # MCP server on stdio
```

Both scripts run from `app/`, and the repository is the process's working directory unless
`CODEGRAPH_REPO_ROOT` says otherwise, so point it at the tree you want answered about:

```sh
CODEGRAPH_REPO_ROOT=/path/to/repo bun run index
CODEGRAPH_REPO_ROOT=/path/to/repo bun run mcp
```

`bun run index` is a full index (`bin/codegraph.ts index`); `bun run mcp` is the default mode and the
command a client registers as its stdio server. `bun run status` prints the same facts as the tool
without speaking the protocol, and `bun run dev` runs the server under `bun --watch`.

| Variable | Default | Meaning |
|---|---|---|
| `CODEGRAPH_REPO_ROOT` | process cwd | tree to index and answer about |
| `CODEGRAPH_DATA_DIR` | `<repo>/.codegraph` | where `graph.db` lives |
| `CODEGRAPH_PROFILE` | `agent` | `agent` (six tools) or `admin` (adds `index`) |
| `CODEGRAPH_CURSOR_KEY` | empty | HMAC key for pagination cursors, 32 bytes or more; a response that gets truncated cannot mint a cursor without it |
| `CODEGRAPH_NO_WATCH` | unset | any value stops the watcher; set by tests |

stdout carries the MCP protocol and nothing else. Diagnostics are JSON lines on stderr, one record
per event.

## The seven tools

| Tool | Profile | What it answers |
|---|---|---|
| `status` | agent | The generation being served, how stale the tree is, per-path coverage and the language census. Call it before saying something does not exist. |
| `summary` | agent | The situation in a few hundred tokens: the working-tree diff (default), the whole repo, or one area, with risk, blast radius and test gaps. |
| `trace` | agent | Callers, callees, implementers, importers and tests for one query, grouped by edge and tier; several matches come back as candidates, not a guess. |
| `search` | agent | Symbols by keyword (BM25) with file:line and in/out degree — locations and shape, not source. |
| `impact` | agent | What a change set reaches: dependents, affected entry points, the tests that cover them, and a risk score; defaults to the working-tree diff. |
| `expand` | agent | More detail for one item from an earlier answer — `members`, `edges`, short `source`, or `coverage`. |
| `index` | admin | Force a full or fast re-index, or repair a graph the watcher could not fix. Not for normal use. |

Registered names are exactly these seven: no `_tool` suffix, no aliases, and no eighth name in the
agent profile.

Every tool takes an optional `budget` in tokens and defaults it per tool (`status` 800, `summary`
1200, `trace` 3200, `search` 2000, `impact` 2500, `expand` 2500). Above 32000 the call is refused
with `budget_too_large`; so is a budget below the smallest size an answer can take, with a hint
naming that floor — one refusal instead of a payload several times what you asked for.

## `.codegraph`

One directory per working tree, beside the code it describes:

```
<repo>/.codegraph/graph.db          the graph
<repo>/.codegraph/graph.db-wal      WAL sidecars, never committed
<repo>/.codegraph/graph.db-shm
<repo>/.codegraph/write.lock        single-writer flock, outside the database
<repo>/.codegraph/.gitignore        a single `*`, written on first index
```

The data dir carries its own `.gitignore` holding one `*`, so git ignores everything inside without
the repository's tracked `.gitignore` being edited. `.codegraph` is excluded from discovery the way
`.git` is — otherwise each index would index the previous database. One database belongs to one
working tree, including a linked `git worktree`; the database answering the session is always
reported at `status.payload.project.db_path`.

## Moving the data directory

`CODEGRAPH_DATA_DIR` overrides the location; a relative value resolves against the repository root.
It exists because the checkout is not always somewhere you can write:

- **read-only mounts.** A source tree mounted read-only, an image layer, or a CI checkout cannot hold
  `.codegraph/` at all. Indexing into a writable directory leaves the tree byte-for-byte untouched.
- **network filesystems.** SQLite needs working locks and a local filesystem for WAL; NFS, SMB, most
  FUSE mounts and synced folders do not provide that, and a half-checkpointed `graph.db-wal` is a
  corrupt graph. Keep the database on local disk and serve the tree over the mount.

A data dir holding an index of a different tree is refused rather than answered from, so a
mis-pointed override is an error, not a quietly wrong answer.

## Distribution

The order is TECH-SPEC.md §15's, and it is deliberate:

1. **Ship the Bun package first.** A `#!/usr/bin/env bun` entry point is a small artifact, needs no
   rebuild per platform, and only requires Bun on the user's machine (102 MB itself). This is the
   route the seven tools are exercised through today.
2. **Add the compiled binary once the grammar set is fixed.** `bun build --compile` produces a single
   file with no Bun prerequisite, but it is large and must be rebuilt per platform: 104 MB measured
   for a hello-world on Bun 1.4.4, 105.1 MB for this app (393 modules), and roughly 1–3 MB per
   embedded WASM grammar — an estimated 130–160 MB for the 12-language v1 set. Compiling grammars in
   is what makes the artifact big, so it waits until the language list stops moving.

Two facts about the standalone artifact today:

- **Migrations must be embedded.** The migration runner reads `.sql` files from
  `import.meta.dir/migrations`; inside a compiled binary that directory is the virtual
  `/$bunfs/root`, which holds only what the build put there. Without `--asset` the binary ships zero
  migrations, computes a shipped schema version of 0, and refuses every existing index with
  `schema_too_new`. The working build command is:

  ```sh
  bun build --compile bin/codegraph.ts \
    --asset "$PWD/src/shared/store/migrations" \
    --outfile dist/codegraph
  ```

  `bun run build` passes exactly this flag, so `dist/codegraph`
  can read and serve an index; `tests/binary-smoke.test.ts` builds and drives that artifact, which is
  the evidence the flag is required rather than merely tidy.
- **Grammar WASM is not embedded.** Grammars load lazily from `web-tree-sitter` and
  `tree-sitter-wasms` in `node_modules` at index time, never at startup, so a compiled binary serves
  an index with no `node_modules` beside it but cannot build one. Until the grammars are compiled in,
  indexing stays with the Bun install and the binary serves the graph it produced.

## Checks

```sh
bun test                            # fixtures plus temp dirs; no network, no writes into the repo
bun run typecheck                   # tsc --noEmit, strict
bun run lint                        # biome check src tests bin
bun test tests/binary-smoke.test.ts  # builds the standalone artifact and drives one tools/call
```
