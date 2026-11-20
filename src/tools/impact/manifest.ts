/**
 * The `impact` manifest (`TOOLS-SPEC.md` §6, `TECH-SPEC.md` §6).
 *
 * `description` is the spec's text verbatim: it is the only thing a model sees before choosing
 * the tool, and the spec fixes the wording so a change to it is a reviewed diff rather than a
 * drift in what agents are told the tool does.
 *
 * `input` is data, not validation: the composition root parses arguments against this table, so
 * the argument surface (plus the 1–4 depth clamp every caller is promised) is declared once here,
 * and the handler reads its defaults from this same literal instead of restating them. `paths`,
 * `repo` and `cursor` carry no default because their absence is meaningful: no `paths` means the
 * working-tree diff and no `cursor` means the first page.
 */
export const manifest = {
  name: "impact",
  description:
    "Given a change set, list what it can reach: dependents, affected entry points, and the tests that cover it, with a risk score. Defaults to the working-tree diff. Call before editing, or before merging.",
} as const;

export const input = {
  paths: {
    type: "array",
    items: { type: "string" },
    description: "Explicit change set to analyse; omit it to measure the working-tree diff.",
  },
  base: {
    type: "string",
    description: "The git revision the change set is diffed against.",
  },
  depth: {
    type: "integer",
    default: 2,
    min: 1,
    max: 4,
    description: "How many caller hops the impact walk follows from the change set.",
  },
  budget: {
    type: "integer",
    default: 2500,
    description: "Token ceiling for the answer; a longer dependents list continues via cursor.",
  },
  cursor: {
    type: "string",
    description: "Continuation token a previous truncated answer returned.",
  },
  // No default: an absent `repo` means "unselected", which the root answers from the working
  // directory; a default here would make that indistinguishable from a named repository.
  repo: {
    type: "string",
    description:
      "Registered repository to answer from — its path or basename. " +
      "Omit it to use the working directory's repository, or the only registered one.",
  },
};

/**
 * A call an agent can paste: the working-tree diff, two hops out, which is the question asked
 * before an edit rather than after one.
 */
export const example: Record<string, unknown> = { depth: 2 };
