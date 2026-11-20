/**
 * `summary`'s manifest: what the composition root registers and where the tool's defaults live
 * (`TECH-SPEC.md` §6). The description is the verbatim text of `TOOLS-SPEC.md` §4 — an agent
 * routes on it, so it is the spec's sentence and not a paraphrase.
 *
 * Three scopes, one enum, deliberately: six behaviours (three scopes × two sizes) would be the
 * `detail_level` knob the spec rejects. `change` stays the default because "what did I just do"
 * is the most common first question and the diff is the cheapest scope to answer it from.
 */

export const manifest = {
  name: "summary",
  description:
    "Summarize the current situation in a few hundred tokens. " +
    '`scope="change"` (default) describes the working-tree diff with risk, the top blast radius ' +
    'and test gaps; `scope="repo"` describes the repository; `scope="area"` describes one path. ' +
    "Call this first in any task to decide what to do next.",
} as const;

/**
 * The JSON-schema-shaped argument list (`TECH-SPEC.md` §6). Only `scope` carries a default here
 * that the handler reads back, so the manifest is the single owner of the hot path's 1200-token
 * budget: `repo` is the cold scope and must not raise it (`TOOLS-SPEC.md` §4).
 */
export const input = {
  scope: {
    type: "enum",
    values: ["change", "repo", "area"],
    default: "change",
    description: "Which slice to summarize: working-tree diff, whole repository, or one path.",
  },
  /** Only meaningful for `area`; one param serving one scope keeps the scopes discoverable together. */
  target: {
    type: "string",
    description: "The path the `area` scope describes; ignored by the other two scopes.",
  },
  /** Defaults to the generation's `base_commit`, so the ordinary call spawns no git at all. */
  base: {
    type: "string",
    description: "The git revision the working-tree diff is taken against.",
  },
  budget: {
    type: "integer",
    default: 1200,
    description: "Token ceiling for the answer; this hot path must stay small.",
  },
  /** Continuation for the scopes whose lists do not fit one page. */
  cursor: {
    type: "string",
    description: "Continuation token a previous truncated answer returned.",
  },
  /**
   * Which repository to answer from. No default: an absent `repo` means "unselected", which the
   * root answers from the working directory; a default would erase that distinction.
   */
  repo: {
    type: "string",
    description:
      "Registered repository to answer from — its path or basename. " +
      "Omit it to use the working directory's repository, or the only registered one.",
  },
} as const;

/**
 * A call an agent can paste: the default scope, which needs no other argument because the diff is
 * read from the generation's own base commit.
 */
export const example: Record<string, unknown> = { scope: "change" };
