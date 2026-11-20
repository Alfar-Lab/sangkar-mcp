/**
 * `trace`'s manifest: the description the model sees, and the arguments the handler parses
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
import { UNRESOLVED_BUCKETS, type UnresolvedBucket } from "#shared/domain/unresolved";

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
  // Always registered: exploring relationships is an everyday question, and the description above
  // is what tells a caller when to reach for it.
} as const;

/**
 * The argument schema. `query` is the only required argument (`TOOLS-SPEC.md` §1: at most one
 * required arg per tool, because argument count is a bigger error source than argument type).
 * `cursor` continues *this* query and replaces `limit`/`offset`/`max_results`.
 *
 * Each `description` says what the argument selects, and nothing else: the type, the enum values
 * and the default are already in the same literal, and a caller reads this text in `tools/list`
 * and in `sangkar trace --help`. The substance is §3's `why` column, one clause per argument.
 */
export const input = {
  query: {
    type: "string",
    required: true,
    description: "The symbol to look up: a name, dotted name, path or `@decorator`.",
  },
  direction: {
    type: "enum",
    values: DIRECTIONS,
    default: "all",
    description: "Which way to walk from the symbol: in, out or both.",
  },
  kinds: {
    type: "array",
    items: { type: "enum", values: NODE_KINDS },
    description: "Narrow resolution to these node kinds when a name collides across kinds.",
  },
  path: {
    type: "string",
    description: "Repo-relative prefix: the path itself and everything under it.",
  },
  depth: {
    type: "integer",
    default: DEFAULT_DEPTH,
    min: MIN_DEPTH,
    max: MAX_DEPTH,
    description: "How many hops of callers and callees to walk from the symbol.",
  },
  budget: {
    type: "integer",
    default: DEFAULT_BUDGET_TOKENS,
    description: "Token ceiling for this result; exceeding it truncates and sets `next`.",
  },
  // The taxonomy's own list, spread rather than restated: a bucket added to
  // `#shared/domain/unresolved.ts` is filterable here in the same commit, and a stale copy would
  // refuse a real bucket with `must be one of:`.
  unresolved: {
    type: "enum",
    values: [...UNRESOLVED_BUCKETS],
    description: "List only this unresolved bucket; the census stays unfiltered.",
  },
  cursor: {
    type: "string",
    description: "Continue this same query from the `next` cursor of a previous page.",
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
export const example: Record<string, unknown> = { query: "handleRequest", depth: 1 };

/** The TypeScript mirror of {@link input}; every field but `query` is optional. */
export interface TraceArgs {
  readonly query: string;
  readonly direction?: TraceDirection;
  /** Node-kind narrowing, applied at resolution only — `expand` filters edge kinds, not node kinds. */
  readonly kinds?: readonly NodeKindName[];
  /**
   * Repo-relative prefix, `search`'s semantics: it narrows resolution and the near misses, so a
   * filtered tie can resolve and a filtered miss never answers from outside the prefix.
   */
  readonly path?: string;
  readonly depth?: number;
  readonly budget?: number;
  /**
   * Narrow the `unresolved` list to one bucket. The census describes the unfiltered scope either
   * way, because a filter chooses what to read, not which call sites failed to bind.
   */
  readonly unresolved?: UnresolvedBucket;
  readonly cursor?: string;
}
