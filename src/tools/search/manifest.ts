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
 * appears exactly once, on `query`. Each `description` says what the argument selects, sourced
 * from §5's `why` column; the type, values and default it would otherwise restate live in the
 * same literal and reach the caller through `tools/list` and `sangkar search --help`.
 */
export const input = {
  query: {
    type: "string",
    required: true,
    description: "Free text to find: an identifier, a substring or a phrase.",
  },
  kind: {
    type: "enum",
    values: SEARCH_KINDS,
    description: "Return only declarations of this kind, when you already know it.",
  },
  path: {
    type: "string",
    description: "Repo-relative prefix: the path itself and everything under it.",
  },
  budget: {
    type: "integer",
    default: DEFAULT_BUDGET_TOKENS,
    description: "Token ceiling for this result; exceeding it truncates and sets `next`.",
  },
  cursor: {
    type: "string",
    description: "Continue this same search from the `next` cursor of a previous page.",
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
 * One call a caller can paste verbatim: the schema says which arguments exist, not what a real
 * one looks like, and an agent with no example reaches for a file reader instead.
 */
export const example: Record<string, unknown> = { query: "publish", kind: "function" };
