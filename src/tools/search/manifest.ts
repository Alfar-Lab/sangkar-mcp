/**
 * `search` manifest (TOOLS-SPEC.md §5, TECH-SPEC.md §6).
 *
 * The description is the spec text verbatim, because it is the entire routing prompt an agent
 * sees for this tool: "declarations with file:line and their in/out degree — not source" is what
 * separates `search` from a file reader, and "hybrid keyword (BM25) with optional semantic
 * rerank" is softened only where the payload can say so — `payload.mode` is `bm25` in the MVP, so
 * an agent that expected a rerank is told instead of being quietly served keyword order.
 *
 * The five-argument surface is the §10 decision, not a local choice: at most one required
 * argument (a second required arg is where CRG's 11.2 % argument-error rate came from), no
 * `provider`/`model`/`regex`/`source_max_lines`. `kind` is optional because a wrong `kind` hides
 * the right answer, which is worse than a noisy list; `path` exists because "find this name under
 * tests/" has no other expression once every tool lost its structured arguments.
 */

export const manifest = {
  name: "search",
  description:
    "Find symbols or text in the indexed code. Hybrid keyword (BM25) with optional semantic rerank. " +
    "Returns declarations with file:line and their in/out degree — not source. Use when you know " +
    "roughly what it is called but not where it lives.",
} as const;

/**
 * The kinds a caller may ask for. `file` is a node kind (a file declares itself) and `decorator`
 * is recorded as a symbol because it is a name someone searches for; `test` is absent even though
 * §5's parameter table mentions "only test", because the index stores that as `node.is_test`, not
 * as a kind — a toggled flag, not a value that can be filtered on.
 */
export const SEARCH_KINDS = ["function", "method", "class", "file", "decorator"] as const;
export type SearchKind = (typeof SEARCH_KINDS)[number];

/**
 * The default token budget, kept here because it is a property of the schema the root advertises;
 * the handler imports it so the advertised default and the fallback can never disagree.
 * TOOLS-SPEC.md §9 gives `search` 2000 tokens: about twenty hits with their degrees.
 */
export const DEFAULT_BUDGET_TOKENS = 2000;

/**
 * The schema the composition root parses arguments against (`TECH-SPEC.md` §6). `required`
 * appears exactly once, on `query`.
 */
export const input = {
  query: { type: "string", required: true },
  kind: { type: "enum", values: SEARCH_KINDS },
  path: { type: "string" },
  budget: { type: "integer", default: DEFAULT_BUDGET_TOKENS },
  cursor: { type: "string" },
} as const;
