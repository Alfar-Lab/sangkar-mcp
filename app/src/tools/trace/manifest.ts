/**
 * `trace`'s manifest: the description the model sees, and the six arguments the handler parses
 * (`TECH-SPEC.md` §6).
 *
 * The description is verbatim from `TOOLS-SPEC.md` §3 — it is the only text a model reads before
 * deciding to call the tool, so paraphrasing it would change the routing behaviour the spec was
 * written to produce. `input` is data, not a validator: the composition root parses arguments
 * against it, and {@link TraceArgs} is its TypeScript mirror so the handler never re-derives a
 * default.
 *
 * The node-kind list is local on purpose. A tool may import only `#shared/*` (rule 1 of
 * `tests/boundary.test.ts`), so it cannot read `NODE_KINDS` from the indexer; this table is the
 * read-side vocabulary `DATABASE.md` §3 fixes for `node.kind`.
 */

/** `node.kind` values a query may be narrowed to; `kinds` is all-or-nothing like `findByName`. */
export const NODE_KINDS = [
  "file",
  "module",
  "class",
  "function",
  "method",
  "test",
  "endpoint",
] as const;

export type NodeKindName = (typeof NODE_KINDS)[number];

/** Which way to walk. `all` is the common question; `in`/`out` exist for when the agent knows. */
export const DIRECTIONS = ["all", "in", "out"] as const;

export type TraceDirection = (typeof DIRECTIONS)[number];

/** `TOOLS-SPEC.md` §3's defaults. The budget is CRG's `detect_changes` size: two hops of a dense symbol. */
export const DEFAULT_DEPTH = 2;
export const DEFAULT_BUDGET_TOKENS = 3_200;

/** The hop budget ceiling. `expand` clamps to the same range; declared here so the schema says so too. */
export const MIN_DEPTH = 1;
export const MAX_DEPTH = 4;

export const DESCRIPTION =
  "Show what is connected to one symbol: callers, callees, implementers, importers and tests. " +
  "Accepts a function or method name, a dotted name, a file path, or `@decorator`. Call this " +
  "instead of reading files to discover relationships. When several symbols match it returns " +
  '`status: "ambiguous"` with candidates instead of guessing. Does not return source code.';

export const manifest = {
  name: "trace",
  description: DESCRIPTION,
  // `agent`: exploring relationships is an agent's everyday question, unlike `index`.
  profile: "agent",
} as const;

/**
 * The six-argument schema. `query` is the only required argument (`TOOLS-SPEC.md` §1: at most one
 * required arg per tool, because argument count is a bigger error source than argument type).
 * `cursor` continues *this* query and replaces `limit`/`offset`/`max_results`.
 */
export const input = {
  query: { type: "string", required: true },
  direction: { type: "enum", values: DIRECTIONS, default: "all" },
  kinds: { type: "array", items: { type: "enum", values: NODE_KINDS } },
  depth: { type: "integer", default: DEFAULT_DEPTH, min: MIN_DEPTH, max: MAX_DEPTH },
  budget: { type: "integer", default: DEFAULT_BUDGET_TOKENS },
  cursor: { type: "string" },
} as const;

/** The TypeScript mirror of {@link input}; every field but `query` is optional. */
export interface TraceArgs {
  readonly query: string;
  readonly direction?: TraceDirection;
  /** Node-kind narrowing, applied at resolution only — `expand` filters edge kinds, not node kinds. */
  readonly kinds?: readonly NodeKindName[];
  readonly depth?: number;
  readonly budget?: number;
  readonly cursor?: string;
}
