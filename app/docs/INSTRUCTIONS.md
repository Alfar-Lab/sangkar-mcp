# Using the graph: five rules, six tools, one bound repository

The server already knows which repository this is: it was bound at startup, so nothing here
selects a project or builds an index. Every answer carries `read_status` (which generation
answered) and `coverage` (what could not be indexed), so freshness and gaps are in the response
rather than in a follow-up call.

## 1. Start a task with `summary`

Call `summary` and let `scope` default to `change`. It answers what the state of this working
tree is — changed files, changed symbols, a risk score with its drivers, the largest blast
radius, and the tests that are missing — in a few hundred tokens. It is the cheapest complete
answer in the surface, and the job CRG's equivalent absorbed 43 % of its traffic with.

## 2. Confine work to an area with `summary` and `target`

When the task lives in one directory or file, call `summary` with `scope: area` and pass the path
as `target`. Same envelope, same fields, scoped to that subtree. A repository-wide summary is a
selection, not a census, so do not infer an area from it.

## 3. Look a symbol up with `trace` and one query

`trace` takes a single `query`: a bare name, a dotted name, a file path, or a `@decorator`. It
returns callers, callees, implementers, importers and tests grouped by edge and tier, plus the
resolution it chose. When several symbols match, it answers `status: ambiguous` with candidates
instead of picking one. Without a name yet, call `search` first — it ranks declarations by
keyword and shows in/out degree, which usually decides between them.

## 4. Call `impact` before editing and before merging

`impact` defaults to the working-tree diff, so the common call takes no arguments. It lists what
the change can reach with `hop` and `via` per node, flags the affected entry points, and marks
which impacted nodes are tested. Run it before you edit to see what you are about to break, and
again before you merge to see what the reviewer should look at. A `test_gaps` reason is a
pointer, not a verdict.

## 5. Reach for `expand`, never for a bigger `budget`

Every tool takes `budget`, and each default is sized for that tool's job. When an answer is
truncated, `next` holds the cursor for the rest of the list while `totals` keeps the full count.
For one item, `expand` takes the id, qualified name or path exactly as it appeared and infers the
detail from the kind — `members` for a file or class, `edges` for a function or method,
`coverage` for a folder — echoing the choice in `payload.what` and `payload.why`. Asking for
32000 tokens to avoid a follow-up call is how a small question becomes a 40 KB answer.

## Do not

- **Do not call `index`.** It is not in the agent profile; the server keeps the graph current on
  its own. If the graph looks wrong, call `status`, report what it did not cover, and carry on.
  A rebuild is a repair, not a query.
- **Do not read `total_relation: gte` as none.** `gte` means lower bound: coverage is incomplete,
  call sites are unresolved, the generation is stale, or the list was cut. Say at least N, and
  call `status` with the paths you care about before claiming nothing exists.
- **Do not use `trace` to read source.** It returns relationships, not code. Read files with your
  own reader; `expand` with `what: source` exists for the twenty-line case, not as a viewer.
- **Do not pass a project argument.** There is none: the repository is bound at startup, so a
  name or a path never needs a project to disambiguate it.

The agent profile has exactly six tools: `status`, `summary`, `trace`, `search`, `impact`,
`expand`. `status` says what is indexed, `summary`, `trace` and `search` answer questions about
the code, `impact` says what a change reaches, and `expand` answers a follow-up about one item.
`next_tools` on every response names the tools worth calling next, always from this list.
