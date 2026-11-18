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
  profile: "agent",
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
  },
  /** Only meaningful for `area`; one param serving one scope keeps the scopes discoverable together. */
  target: { type: "string" },
  /** Defaults to the generation's `base_commit`, so the ordinary call spawns no git at all. */
  base: { type: "string" },
  budget: { type: "integer", default: 1200 },
  /** Continuation for the scopes whose lists do not fit one page. */
  cursor: { type: "string" },
} as const;
