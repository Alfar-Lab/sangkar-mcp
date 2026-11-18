/**
 * `status` as the protocol layer sees it (TOOLS-SPEC.md §2).
 *
 * The description is model-facing text, so it is reproduced verbatim from the spec: the wording is
 * what tells an agent to call this *before* claiming that something does not exist, and a
 * paraphrase would quietly drop that instruction. The em dash and the trailing "call it freely"
 * are part of it for the same reason.
 *
 * Both constants are here because a tool owns its own defaults (TECH-SPEC.md §5.2): 800 tokens is
 * the budget the spec measured, and the retry interval is short because `status` is the cheap,
 * idempotent call an agent polls while the index is still being built — a long interval would make
 * the poll cost more than the answer.
 */

/** Default token budget of a `status` call (TOOLS-SPEC.md §2). */
export const STATUS_DEFAULT_TOKENS = 800;

/** How long an unpublished database asks the agent to wait before asking again. */
export const STATUS_RETRY_AFTER_MS = 5_000;

export const manifest = {
  name: "status",
  description:
    "Report whether the graph is current and what it covers. Call this before telling the user " +
    "that something does not exist, and whenever a previous answer looked incomplete. Returns " +
    "the index generation, how stale the working tree is, per-path coverage, and the language " +
    "census. Cheap — call it freely.",
  profile: "agent",
} as const;

/**
 * Two optional arguments, which is the whole point: `paths` being absent means "the whole
 * repository", so its default is the empty list rather than the root path (a root path would have
 * to be normalised against `config.repoRoot` and would then be indistinguishable from an explicit
 * request for that one path).
 */
export const input = {
  paths: { type: "array", items: { type: "string" }, default: [] },
  budget: { type: "integer", default: STATUS_DEFAULT_TOKENS },
} as const;
