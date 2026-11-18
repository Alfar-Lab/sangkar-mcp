/**
 * The one-screen usage guide, as text and as routing (TOOL-USAGE-ANALYSIS.md §4: with seven tools
 * and no prompt scaffolding, the instructions layer is the adoption lever, not the tool count).
 *
 * The text lives in this file rather than in a fourth `src/cli` layer so the three-layer layout
 * in plan.constraint_invariant.md stays true: `src/index.ts` imports it before registration, and
 * `src/shared/**` may not import tools or the indexer. It therefore carries the guide and the
 * routing table as plain data — no tool manifest, no dynamic import, no registration knowledge.
 */

/**
 * The seven tools this server publishes (TOOLS-SPEC.md §10). Every suggestion this module makes
 * is drawn from this list, so `next_tools` can never name a tool the surface does not have.
 *
 * `index` sits last on purpose: measured, it was called 1 081 times and failed 12.9 % of the time
 * for work the server already does itself (TOOLS-SPEC.md §8), so it is a repair, not a query.
 */
export const TOOL_NAMES = [
  "status",
  "summary",
  "trace",
  "search",
  "impact",
  "expand",
  "index",
] as const;
export type ToolName = (typeof TOOL_NAMES)[number];

/**
 * Where each tool sends the agent next. Keys are exactly the seven tool names, so a tool added
 * without a route is a type error rather than a silent empty `next_tools`; the values are the
 * tools worth calling next, including for `index`, whose only suggestion is `status`.
 */
const NEXT_TOOLS: Readonly<Record<ToolName, readonly ToolName[]>> = {
  status: ["summary", "search"],
  summary: ["impact", "trace", "expand"],
  trace: ["expand"],
  search: ["trace", "expand"],
  impact: ["trace", "expand"],
  expand: ["trace"],
  index: ["status"],
};

function isToolName(value: string): value is ToolName {
  return (TOOL_NAMES as readonly string[]).includes(value);
}

/**
 * The tools worth calling after `tool`, for the envelope's `next_tools`. A name this surface
 * does not know (an older client, a typo) gets an empty array: suggesting a plausible-looking
 * tool for an unknown one would be a guess, and `next_tools` is a routing promise.
 *
 * Returns a fresh array so a caller cannot mutate the table through one envelope.
 */
export function nextToolsFor(tool: string): string[] {
  return isToolName(tool) ? [...NEXT_TOOLS[tool]] : [];
}

/**
 * The guide, byte for byte the body of `docs/INSTRUCTIONS.md`
 * (tests/instructions.test.ts fails if the two drift). Rule order is deliberate: orient, scope,
 * look up, judge blast radius, then fetch detail — the order an agent needs them in one task.
 * The anti-patterns come last because each one names a call the reader would otherwise make.
 */
export const INSTRUCTIONS_TEXT = `# Using the graph: five rules, seven tools, one bound repository

The server already knows which repository this is: it was bound at startup, so nothing here
selects a project or builds an index. Every answer carries \`read_status\` (which generation
answered) and \`coverage\` (what could not be indexed), so freshness and gaps are in the response
rather than in a follow-up call.

## 1. Start a task with \`summary\`

Call \`summary\` and let \`scope\` default to \`change\`. It answers what the state of this working
tree is — changed files, changed symbols, a risk score with its drivers, the largest blast
radius, and the tests that are missing — in a few hundred tokens. It is the cheapest complete
answer in the surface, and the job CRG's equivalent absorbed 43 % of its traffic with.

## 2. Confine work to an area with \`summary\` and \`target\`

When the task lives in one directory or file, call \`summary\` with \`scope: area\` and pass the path
as \`target\`. Same envelope, same fields, scoped to that subtree. A repository-wide summary is a
selection, not a census, so do not infer an area from it.

## 3. Look a symbol up with \`trace\` and one query

\`trace\` takes a single \`query\`: a bare name, a dotted name, a file path, or a \`@decorator\`. It
returns callers, callees, implementers, importers and tests grouped by edge and tier, plus the
resolution it chose. When several symbols match, it answers \`status: ambiguous\` with candidates
instead of picking one. Without a name yet, call \`search\` first — it ranks declarations by
keyword and shows in/out degree, which usually decides between them.

## 4. Call \`impact\` before editing and before merging

\`impact\` defaults to the working-tree diff, so the common call takes no arguments. It lists what
the change can reach with \`hop\` and \`via\` per node, flags the affected entry points, and marks
which impacted nodes are tested. Run it before you edit to see what you are about to break, and
again before you merge to see what the reviewer should look at. A \`test_gaps\` reason is a
pointer, not a verdict.

## 5. Reach for \`expand\`, never for a bigger \`budget\`

Every tool takes \`budget\`, and each default is sized for that tool's job. When an answer is
truncated, \`next\` holds the cursor for the rest of the list while \`totals\` keeps the full count.
For one item, \`expand\` takes the id, qualified name or path exactly as it appeared and infers the
detail from the kind — \`members\` for a file or class, \`edges\` for a function or method,
\`coverage\` for a folder — echoing the choice in \`payload.what\` and \`payload.why\`. Asking for
32000 tokens to avoid a follow-up call is how a small question becomes a 40 KB answer.

## Do not

- **Do not read \`total_relation: gte\` as none.** \`gte\` means lower bound: coverage is incomplete,
  call sites are unresolved, the generation is stale, or the list was cut. Say at least N, and
  call \`status\` with the paths you care about before claiming nothing exists.
- **Do not use \`trace\` to read source.** It returns relationships, not code. Read files with your
  own reader; \`expand\` with \`what: source\` exists for the twenty-line case, not as a viewer.
- **Do not pass a project argument.** There is none: the repository is bound at startup, so a
  name or a path never needs a project to disambiguate it.

The surface is seven tools: \`status\`, \`summary\`, \`trace\`, \`search\`, \`impact\`, \`expand\` and
\`index\`. \`status\` says what is indexed, \`summary\`, \`trace\` and \`search\` answer questions about
the code, \`impact\` says what a change reaches, \`expand\` answers a follow-up about one item, and
\`index\` rebuilds the graph: call it only when \`status\` says the graph is broken, and never as a
query — a rebuild is a repair. \`next_tools\` on every response names the tools worth calling
next, always from this list.`;

/**
 * MCP servers publish tools, not prompt text, so the guide reaches the agent through the client:
 * a human reads this, or a CLI mode prints it. Writes the whole guide to stdout once, with a
 * trailing newline — the only stdout writer in `shared`, and only when an explicit CLI mode calls
 * it (log.ts owns stderr, and stdout carries the protocol).
 */
export function printInstructions(): void {
  process.stdout.write(`${INSTRUCTIONS_TEXT}\n`);
}
