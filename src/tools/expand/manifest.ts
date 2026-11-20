/**
 * The `expand` manifest (`TOOLS-SPEC.md` §7, `TECH-SPEC.md` §6).
 *
 * The description is the spec's text verbatim, and deliberately so: it is what the model reads
 * when it chooses a tool, and its last clause — "says which it chose" — is the promise the
 * handler keeps with `payload.what` and `payload.why`. A paraphrase here would be a different
 * contract from the one the spec argues for.
 *
 * These arguments are the whole surface, and that smallness is the point: `expand` is the one
 * address for detail, which is what lets the other six tools ship summaries instead of
 * `detail_level` and per-item flags (DESIGN.md §7). `path` is the same prefix filter `trace` and
 * `search` take, because "the `Widget` under `tests/`" is a handle collision an agent can already
 * meet and has no other way to express. `budget` defaults higher than `summary`'s because detail
 * is by definition the expensive part.
 *
 * `range` exists because a source view is priced in lines: a caller that already knows which
 * lines of the node matter should not pay for the rest, and the tool should not have to grow a
 * second window argument beside it. It is a pair — `[from, to]`, 1-based and inclusive — and it
 * narrows `what: "source"` only. The handler clamps it into the node's own extent and says so in
 * `why`; an offset outside the node is a request that cannot be honoured, not a bad call.
 */

export const manifest = {
  name: "expand",
  description:
    "Fetch more detail for one item from a previous result. Pass the id, qualified name or path " +
    "exactly as it appeared. Infers what to expand from the item's kind and says which it chose.",
} as const;

/**
 * `TOOLS-SPEC.md` §7's default, and the fallback a handler uses when a direct call (a test, a
 * future in-process caller) sends none. Per-tool defaults live here, never in the handler: the
 * manifest is the one place a tool's sizing is reviewable (`TECH-SPEC.md` §5.3).
 */
export const DEFAULT_BUDGET_TOKENS = 2500;

/**
 * The declared input schema, in the shape `TECH-SPEC.md` §6 fixes for every tool: `required`
 * marks the one mandatory argument, enums carry `values`, numbers carry `default`. There is no
 * `cursor` field because §7's argument table has none; the consequence for truncation is
 * documented in `handler.ts`.
 */
export const input = {
  target: {
    type: "string",
    required: true,
    description:
      "One opaque handle copied verbatim from a prior payload: an id, a qualified name or a path. " +
      "Detail is addressable here instead of pre-shipped in every other tool's answer.",
  },
  what: {
    type: "enum",
    values: ["members", "edges", "source", "coverage", "outline"],
    description:
      "Which detail to return — one of `members`, `edges`, `source`, `coverage` or `outline` — " +
      "inferred from the target's kind when omitted and always echoed in the answer. `outline` is " +
      "the one value never inferred: ask for it by name.",
  },
  path: {
    type: "string",
    description:
      "Repo-relative prefix applied when resolving the target, so the `Widget` under `tests/` can " +
      "be named. Optional: an unprefixed target is the common case.",
  },
  range: {
    type: "array",
    items: { type: "integer" },
    description:
      "The 1-based, inclusive line span [from, to] to return for a source request, read inside the " +
      "resolved node's own extent and clamped to it; the answer says when a request was narrowed. " +
      "Defaults to the node's whole extent.",
  },
  budget: {
    type: "integer",
    default: DEFAULT_BUDGET_TOKENS,
    description:
      "Token budget for the answer, larger by default than `summary`'s because detail is by " +
      "definition the expensive part.",
  },
  // No default: an absent `repo` means "unselected", which the root answers from the working
  // directory; a default here would make that indistinguishable from a named repository.
  repo: {
    type: "string",
    description:
      "Registered repository to answer from — its path or basename. " +
      "Omit it to use the working directory's repository, or the only registered one.",
  },
} as const;

/**
 * The one call the CLI usage block and the tool documentation show, and the smallest one that
 * exercises what this tool exists for: a file handle plus `outline` answers "what does this file
 * declare" in one row per declaration, which is the cheapest way to pick the next target.
 *
 * Nothing imports it yet. It lives beside the schema rather than inside a doc so that one example
 * per tool has one home and cannot drift from the argument names it spells.
 */
export const example: Record<string, unknown> = {
  target: "src/indexer/resolve.ts",
  what: "outline",
};
