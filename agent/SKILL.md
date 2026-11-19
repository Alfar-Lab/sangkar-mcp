---
name: sangkar-graph
description: "Answer code-structure questions from an indexed call graph instead of grepping: who calls this, what does this call, what does my change reach, is it tested, does it exist at all. Use when the repository is indexed by sangkar (MCP tools status, summary, trace, search, impact, expand, index; or the sangkar CLI): callers, callees, call chain, dependencies, impact analysis, blast radius, risk before an edit or merge, test gaps, or an absence claim."
license: MIT
compatibility: sangkar MCP server bound to this repository, or the sangkar CLI on PATH. Bun 1.4.4+.
---

# sangkar: four questions, seven tools, one bound repository

The server is bound to one repository at startup. There is no `project` argument, no query
language, and no index to build before asking: `index` is a repair, not a step.

Some clients prefix the tool names with the server name (`sangkar_trace`,
`mcp__sangkar__trace`). Same tools, same arguments.

| Question | Call |
|---|---|
| What state is this tree in? | `summary` (no arguments: `scope: "change"`, the working-tree diff) |
| What is inside this one directory or file? | `summary` `{scope: "area", target: "<path>"}` |
| What is this called? | `search` `{query: "<keyword>"}` |
| Who calls it, what does it call? | `trace` `{query: "<name>"}` |
| Is it safe to change this? | `impact` (no arguments, before editing and again before merging) |
| Give me detail on one handle from that answer | `expand` `{target: "<id, qualified name or path as printed>"}` |
| Does this exist at all? | `status` `{paths: ["<path>"]}`, before any absence claim |

Start with `summary` unless the task already names a symbol. It costs a few hundred tokens and
carries risk, blast radius, and missing tests at once, which is usually the whole first question.

## Read the envelope before the payload

Every answer has the same nine fields. Read them in this order, because each one can invalidate a
conclusion drawn from `payload`:

1. `status`: `ok` | `ambiguous` | `not_found` | `error` | `bootstrapping`. `error` and
   `bootstrapping` carry `error{code, message, hint, retryable, retry_after_ms}`.
2. `coverage.complete`: if `false`, something was skipped (`coverage.skipped`, `coverage.failed`,
   `coverage.not_indexed[].reason`), so the graph is honest but partial.
3. `totals.total_relation`: `"eq"` is an exact count and `"gte"` a lower bound. See below.
4. `truncated` / `truncation_reason`: `budget` | `item_cap` | `depth` | `page`. `depth` means the
   walk stopped at the depth you asked for. It is not the end of the list.
5. `next`: `null`, or a cursor that continues *this same call*. Pass it back as `cursor`, unchanged.
6. `read_status`: `generation`, `indexed_at`, `stale`, `stale_reason`, `decisive`. A stale or
   non-decisive generation is a reason to re-ask later, not to state a fact now.
7. `next_tools`: the tools worth calling next, already filtered for this answer. Prefer it over
   guessing.

## Absence: `eq` is a claim, `gte` is a lower bound

- `total_relation: "eq"` is legal only when coverage is complete, no call site in scope failed to
  bind, the generation is fresh, and nothing was truncated. Then `calls_total: 0` really is zero.
- `total_relation: "gte"` never supports "there are none". It means *at least* the number shown.
  Say "at least N", or call `status` on the paths you care about first.
- `status: "not_found"` is the positive "I resolved no such symbol" answer, and it is trustworthy
  only next to `coverage.complete: true`. Check `payload` for near misses before concluding.
- `coverage.not_indexed` names what the walker refused (usually `unsupported_ext`) and
  `coverage.unresolved_edges` counts call sites that did not bind. Read both before you say a
  symbol is unused, and read `coverage.tests` before you say a change is untested: `nodes: 0` is
  "no test symbols were indexed", which is a different claim from "nothing covers this".

## Truncation: page, do not raise the budget

Every tool ships a budget sized for its own job (`status` 800, `summary` 1200, `search` 2000,
`expand` 2500, `impact` 2500, `trace` 3200 tokens). When an answer is cut, `next` continues it and
`totals` keeps the untruncated counts.

- Continue with `cursor`, never with a bigger `budget`. 32 000 is the hard ceiling
  (`budget_too_large`); asking for it to avoid one follow-up is how a small question becomes a
  40 KB answer.
- For one item from a list, reach for `expand` with the handle you were given. It infers the detail
  from the item's kind (`members` for a file or class, `edges` for a function or method, `coverage`
  for a folder) and echoes the choice in `payload.what` plus a reason in `payload.why`.
- Stop paging when you have the answer. `*_total` in `totals` is already the count you would page
  through to.

## Per-tool arguments worth knowing

- `summary`: `scope` (`change` default, `repo`, `area`), `target` (the path for `area`, ignored
  otherwise), `base`, `budget`, `cursor`. A repository-wide summary is a selection, not a census.
- `trace`: `query` is the only required argument and accepts a bare name, a dotted name
  (`File.rename`), a repo-relative path, or `@decorator`. Also `direction` (`all` default, `in`,
  `out`), `kinds` to break a name collision across kinds, `path` (repo-relative prefix), `depth`
  1-4 (default 2), `unresolved` to list one unresolved bucket, `cursor`.
- `search`: `query`, `kind` (`function`, `method`, `class`, `file`, `decorator`), `path`,
  `budget`, `cursor`. It returns declarations with `file:line` and in/out degree, ranked by
  keyword; `payload.mode` says `bm25` in this release, so expect keyword order.
- `impact`: `paths` (omit it and the change set is the working-tree diff), `base`, `depth` 1-4
  (default 2), `budget`, `cursor`. Each node carries `hop` and `via`, entry points are flagged, and
  impacted nodes are marked tested or not. A `test_gaps` entry is a pointer, not a verdict.
- `expand`: `target` (copy the handle verbatim), `what` (`members`, `edges`, `source`, `coverage`,
  `outline`; `outline` is the one value never inferred, ask for it by name), `path`, `range`
  (`[from, to]`, 1-based and inclusive, narrows `source` only and is clamped to the node),
  `budget`.
- `status`: `paths` (default: the whole repository), `budget`. Cheap and idempotent: call it
  freely, and poll it while `bootstrapping` tells you to wait `retry_after_ms`.
- `index`: `force`, `mode` (`full`, `fast`). Only when `status` reports a broken or unbuildable
  graph. A rebuild is a repair.

## When `status` says `ambiguous`

`trace` and `expand` answer `status: "ambiguous"` with the candidates instead of picking one. Do
not retry with the same string. Narrow with `kinds` (which kind of symbol) or `path` (which subtree),
then call again. `search` first is usually cheaper than guessing: it ranks declarations and shows
degree, which is what decides between two same-named functions.

## Do not

- Do not use `trace` to read source. It returns relationships, not code. Read files with your own
  reader; `expand` `{what: "source"}` exists for the twenty-line case, not as a viewer.
- Do not pass a `project` or repository argument. There is none.
- Do not rebuild the graph to answer a question: the server keeps the graph current itself, and a
  reindex is expensive next to a `status` call.
- Do not infer an area from `scope: "repo"` output, and do not raise `depth` to 4 to avoid a second
  call: two one-hop calls read better than one truncated four-hop answer.
- Do not report a lower bound as a count. Quote `total_relation` with the number, or say "at least".

## Errors, and the next action

| `error.code` | What to do |
|---|---|
| `bootstrapping` | No graph yet. Wait `retry_after_ms` and re-call `status`; read files directly meanwhile. |
| `index_locked` | Another indexer holds the writer lock. Retry after `retry_after_ms`; do not loop. |
| `not_found` / `ambiguous` | The answer, not a failure. Check near misses, narrow with `kinds` or `path`. |
| `cursor_invalid`, `cursor_expired`, `cursor_mismatch` | Drop the cursor and re-ask for the first page. Cursors are signed and bound to one generation, so an edit invalidates them; never reuse a cursor on a different query. |
| `budget_too_large` | You asked outside the floor/ceiling (ceiling 32 000). Use the default and page instead. |
| `wrong_project_db` | The server is bound to a different repository than the one you mean. Tell the user; do not retry. |
| `schema_too_new`, `unsupported_filesystem` | Environment problem, not a query problem. Report it with the message and hint. |
| `internal` | Retry once, then report the message. |

## Without an MCP client

The same seven tools are shell verbs, and every tool except `status` prints one JSON envelope:

```sh
sangkar status                       # five human lines; --json for the envelope
sangkar summary --scope area --target src/tools
sangkar search 'publish' --kind function
sangkar trace 'db.write' --depth 1 --direction out
sangkar impact                       # working-tree diff
sangkar expand src/indexer/resolve.ts --what outline
sangkar index --mode fast            # repair only
```

Exit codes come from `status`: `0` for `ok`/`ambiguous`/`not_found` (a negative answer is still an
answer), `1` for `error`/`bootstrapping`, `2` for a usage refusal. `sangkar index` exits `2` when
nothing changed and `75` when another indexer holds the lock.

## First call in a session

1. `status`, when freshness matters or a previous answer looked thin. Otherwise skip it.
2. `summary` with no arguments.
3. `search` or `trace` on the symbol the task names, then `impact` before you edit.
4. `status` with `paths` before you tell the user that something does not exist.

Installing this skill and registering the server: see `README.md` beside this file.
