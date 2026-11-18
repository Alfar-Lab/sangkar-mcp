/**
 * The `expand` manifest (`TOOLS-SPEC.md` §7, `TECH-SPEC.md` §6).
 *
 * The description is the spec's text verbatim, and deliberately so: it is what the model reads
 * when it chooses a tool, and its last clause — "says which it chose" — is the promise the
 * handler keeps with `payload.what` and `payload.why`. A paraphrase here would be a different
 * contract from the one the spec argues for.
 *
 * Three arguments are the whole surface, and that smallness is the point: `expand` is the one
 * address for detail, which is what lets the other six tools ship summaries instead of
 * `detail_level` and per-item flags (DESIGN.md §7). `budget` defaults higher than `summary`'s
 * because detail is by definition the expensive part.
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
  target: { type: "string", required: true },
  what: { type: "enum", values: ["members", "edges", "source", "coverage"] },
  budget: { type: "integer", default: DEFAULT_BUDGET_TOKENS },
} as const;
