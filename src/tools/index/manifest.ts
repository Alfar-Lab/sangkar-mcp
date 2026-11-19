/**
 * `index`: the one tool that forces or repairs a reindex (`TOOLS-SPEC.md` §8, `DESIGN.md` §4).
 *
 * It is registered like every other tool. What keeps it out of the way is the instructions, not a
 * registration filter: they tell an agent to call it only when `status` reports a broken graph.
 * Measured, an agent called indexing 1 081 times and it failed 12.9 % of the time, for work the
 * server already does itself when the tree changes — so repair is a real need, but a routine one
 * it is not.
 *
 * The description is byte for byte `TOOLS-SPEC.md` §8's text (`tests/tools-index.test.ts` asserts
 * it). It is what an admin client reads before calling the one tool in this server that rewrites
 * the entire graph, so paraphrasing it would be a behaviour change, not an edit.
 */

export const manifest = {
  name: "index",
  description:
    "Admin: force a full or fast index, or repair a broken one. Not for normal use — the server " +
    "keeps the graph current automatically.",
} as const satisfies { name: string; description: string };

/**
 * The two optional arguments, in the shape `TECH-SPEC.md` §6 fixes for every tool's `input`
 * (`{type, default, …}` records, not JSON Schema): the composition root turns this into the JSON
 * Schema it registers, and the handler reads its defaults from here rather than restating them.
 *
 * There is deliberately no `budget` argument: `TOOLS-SPEC.md` §10 gives `index` two optional
 * parameters, and a repair call has no payload worth paging.
 */
export const input = {
  force: {
    type: "boolean",
    default: false,
    description:
      "Rebuild from an empty stage and publish even when the file hashes say nothing changed.",
  },
  mode: {
    type: "enum",
    values: ["full", "fast"],
    default: "full",
    description:
      "full rebuilds the graph from an empty stage; fast republishes only what changed and is the " +
      "cheaper first pass on a huge repository.",
  },
} as const;

/** The `generation.mode` the worker accepts; `full` rebuilds, `fast` republishes changes only. */
export type IndexMode = (typeof input.mode.values)[number];

/**
 * The one call the CLI usage block and the tool documentation show: a fast pass is the cheaper
 * repair, and naming the mode is the only argument here that changes what the worker does. `force`
 * stays out because an example that rebuilds from an empty stage is the expensive call, not the
 * representative one.
 *
 * Nothing imports it yet. It lives beside the schema rather than inside a doc so that one example
 * per tool has one home and cannot drift from the argument names it spells.
 */
export const example: Record<string, unknown> = { mode: "fast" };

/**
 * A call's arguments. Every field is optional because the schema carries the defaults and an MCP
 * client may send neither, one, or both; the handler applies {@link input}'s defaults itself so a
 * direct call and a protocol call agree.
 */
export interface IndexArgs {
  readonly force?: boolean;
  readonly mode?: IndexMode;
}
