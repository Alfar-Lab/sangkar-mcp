# Parity harness: fixtures, goldens, comparator

Three fixture repositories, a checked-in bank of the TypeScript CLI's answers, and the structural
comparator that judges the Rust port against them. While `src/` exists it is the oracle; after phase
5 deletes it, `rust/tests/golden/**` is the only specification of what the seven tools must answer.

```
rust/tests/parity/fixtures/     the three trees (this repo's own index ignores them, see .sangkarignore)
rust/tests/golden/<fixture>/    57 captured answers: {args, stdout, exit_code}
scripts/parity/capture-goldens.ts   recaptures them from bin/sangkar.ts
scripts/parity/compare.py           structural comparison of two goldens
```

## The fixtures

| fixture | files | what it is for |
| --- | --- | --- |
| `polyglot` | 15 | every one of the 12 language rows (`.py .ts .tsx .js .go .java .rs .c .cpp .cs .php .rb`), a caller chain `alpha → beta → gamma`, `class Shape`/`class Circle(Shape)`, a `.test.ts` that calls production functions, the name `parseInput` in two files, and one import that cannot resolve (`./ghost-module`) |
| `broken` | 6 | a syntax error (`syntax-error.ts`), a binary file (`binary.ts`), a 600 KB unit (`big.ts`), a unicode path (`src/héllo-世界.ts`), an empty file (`empty.ts`), plus a directory made unreadable **at capture time** (`locked/`, never committed) |
| `empty` | 1 | only a `.gitignore`, so git can carry the directory: an indexed tree with zero files |

Two details are deliberate:

- `binary.ts` carries a language extension because discovery classifies extension-first: a `.png`
  would report `unsupported_ext` and never reach the NUL probe. `binary` is the reason the fixture
  must produce, and the file's first bytes are a real 1×1 PNG (then NUL padding to 1 KB), so the
  content is genuinely binary.
- `locked/` is created in the archive and chmod'ed `000` by the capture script, then restored and
  removed with the archive. A mode bit in git is not portable (and a fresh checkout would carry it
  forever), so the unreadable-directory case exists only while a capture or a parity run is
  executing. A harness that does not create it simply sees one fewer coverage row.

## Capturing

```sh
~/.bun/bin/bun run scripts/parity/capture-goldens.ts
```

The script runs `<bun> run bin/sangkar.ts` from the repository root with `SANGKAR_HOME` under a temp
directory, `SANGKAR_TELEMETRY=off`, and the fixed cursor key below. Per fixture it:

1. copies the checked-in tree to `$TMPDIR/sangkar-parity-capture/<fixture>/<fixture>` and creates
   `locked/` for `broken`;
2. publishes once with `index --mode fast` (the setup run is not a golden);
3. runs the matrix below and writes `rust/tests/golden/<fixture>/<name>.json`, asserting on the way
   that every call exits 0 with a JSON envelope, that `polyglot` reports `coverage.failed == 0` and
   every `*.page` capture minted a `next` token, and that `broken` reports both a `binary` and an
   `unreadable` entry in `coverage.not_indexed`;
4. runs `index --mode fast` once more on a *second*, cold archive — the only tool that writes — and
   asserts its stdout is empty.

`SANGKAR_PARITY_KEEP=1` leaves the archives and their databases in place for inspection.

**Why a copy outside git, and a temp data dir.** `--data-dir` keeps the database out of the
checked-in tree, so a fixture can never be polluted by a run. The copy matters just as much:
`payload.project` and the change-scope tools read git, so indexing a fixture *inside this
repository* would stamp every golden with the commit it was captured at (and answer `summary
--scope change` from this repository's own diff). An archive under `$TMPDIR` has no git identity
(`branch`, `remote`, `commit` are all `null`) and no working-tree diff, which is stable across
commits.

### The matrix

`args` in a golden is the argv tail after the tool name; a replay is
`<sangkar> <tool> <args…> --json --repo-root <fixture> --data-dir <temp>`.

| file | tool | args |
| --- | --- | --- |
| `status.json` | status | *(none)* |
| `status.paths.json` | status | `--paths src` |
| `status.page.json` | status | `--paths src --budget 600` |
| `summary.json` | summary | `--scope repo` |
| `summary.area.json` | summary | `--scope area --target src` |
| `summary.change.json` | summary | `--scope change --base HEAD` |
| `summary.page.json` | summary | `--scope repo --budget 400` |
| `trace.json` | trace | `--query alpha --depth 2` |
| `trace.duplicate.json` | trace | `--query parseInput --depth 1` |
| `trace.page.json` | trace | `--query alpha --depth 2 --budget 500` |
| `search.json` | search | `--query request` |
| `search.kind.json` | search | `--query request --kind function --path src` |
| `search.page.json` | search | `--query request --budget 300` |
| `impact.json` | impact | `--paths src/chain.ts --depth 2` |
| `impact.base.json` | impact | `--base HEAD` |
| `impact.page.json` | impact | `--paths src/chain.ts --depth 2 --budget 500` |
| `expand.json` | expand | `--target <the first path status.paths reported> --what members` |
| `expand.outline.json` | expand | the same target, `--what outline` |
| `index.json` | index | `--mode fast` (on the cold archive; stdout is `""`) |

The probes are polyglot's names on purpose: the identical call runs against `broken` and `empty`, so
those goldens are the not-found and empty-graph answers to the same question. The `*.page` budgets
are small enough to cut `polyglot` (all five mint a `next`); the smaller fixtures have less to page
and truthfully answer untruncated.

`empty`'s status answer reports the *requested* path as `{"path": "src", "reason":
"not_indexed_unknown"}`, so the expand target for that fixture is `src` and both expand goldens are
`not_found` — the empty tree's honest answer, not a special case in the script.

## The golden file

```json
{ "args": ["--paths", "src"], "stdout": "…exact bytes…\n", "exit_code": 0 }
```

`stdout` is byte-for-byte what the CLI wrote, including the trailing newline of the `--json`
rendering. Nothing is normalized and nothing is ever hand-edited: a capture that looks wrong is a
fixture or an invocation to fix, then a recapture. `index.json` is the one golden with empty stdout,
because `sangkar index` is the bootstrap verb and its contract is the worker's exit code, not an
envelope (`bin/sangkar.ts`, `tests/cli.test.ts`).

## The fixed cursor key

Every capture runs with:

```
SANGKAR_CURSOR_KEY=sangkar-parity-fixed-cursor-key-0123456789abcdef
```

48 bytes, above the 32-byte floor `src/shared/cursor.ts` enforces, and not a secret. Without it each
process mints 32 random bytes (`src/shared/config.ts:290-294`), so no `next` token would survive the
process that produced it and the pagination goldens would be unusable. With it a token is
deterministic for a given payload, and the Rust implementation can sign and verify the same bytes:

```
token     = base64url(JSON.stringify({generation, tool, argsHash, position, repo})) + "." +
            base64url(HMAC-SHA256(key, payloadSegment)[0..16])
verify    = MAC, then generation, then repo, then tool/argsHash
```

`argsHash` keeps each tool's existing format (`sha256:<hex>` for trace/summary/status, bare hex for
search, a 32-character truncation for impact), so the goldens pin those inconsistencies too.

What the fixed key does **not** make reproducible is the generation: `newGenerationId()` is
`g-` plus three random bytes per publish, and it is inside the cursor payload *and* in every
`read_status`. A `next` token in a golden is therefore reproducible **given its generation**, not
across publishes; compare it by verifying the MAC with the key and comparing the payload with
`generation` (and `repo`) masked, or by replaying the page against the same database.

## Fields that are per-run, not contract

Verified by capturing twice and diffing the parsed `stdout` — these are the only paths that moved
(54 of 57 files):

| path in `stdout` | why |
| --- | --- |
| `/read_status/generation` | `g-` + three random bytes per publish |
| `/read_status/indexed_at` | wall clock at publish |
| `/read_status/stale_checked_at` | wall clock at the call |
| `/summary` | the sentence embeds the generation (`"15 files indexed, generation g-…"`) |
| `/next` | the cursor carries the generation (see above) |
| `/payload/health/last_publish_ms` | the worker's measured duration |

Environmental, stable only when a replay archives the fixture at the same absolute location:

| path in `stdout` | why |
| --- | --- |
| `/payload/project/root` | the absolute path of the served tree |
| `/payload/project/db_path`, `/payload/graph/db_path` | the absolute `--data-dir` |
| `/payload/project/branch`, `/payload/project/remote`, `/payload/project/commit` | git identity; `null` for the archive |
| the cursor payload's `repo` | canonical repo root, stamped into the token |

Measurements the parity bar releases (`RUST-MIGRATION.md` §3) that happened to be stable in the two
captures: `/payload/graph/size_bytes` (196 608 for `polyglot`, 172 032 for `broken` and `empty`) and
`/payload/health/statement_cache_entries` (10 for `polyglot` and `broken`, 9 for `empty`). Compare
them as numbers of the same shape, not as values to reproduce. No golden carries a token estimate or
a `duration_ms`: the only tool whose payload reports a duration is `index`, whose CLI surface has no
envelope.

Everything else — key sets, primitive values, array order and length, `status`, `error.code`,
`totals`, exit codes — is the comparison.

## Comparing

```sh
python3 scripts/parity/compare.py <expected-golden.json> <actual-golden.json>
```

Dependency-free python3. It compares `args`, `exit_code` and the JSON document inside `stdout`
structurally: the same key sets at every depth, the same primitive values (booleans are not
numbers), the same array order and length (a move is a difference at the position that moved). Every
difference prints as a JSON pointer, and the exit code is `1` on any difference, `0` when the two
agree, `2` for a usage error or a golden it cannot read. A `stdout` that is not JSON on both sides —
TOON, or an empty string from `index` — cannot be compared key by key; it falls back to a byte
comparison and says on stderr that structural comparison needs `--json` goldens.
