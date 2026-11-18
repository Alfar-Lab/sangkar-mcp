/**
 * One risk formula, so `summary` and `impact` cannot disagree (`TECH-SPEC.md` §5.1).
 *
 * A score on its own is unusable for deciding what to do, so the return value is a score, a
 * band and driver sentences that carry the numbers: "41 dependents sit inside the expanded
 * radius" is what the agent quotes, "0.62" is not (`TOOLS-SPEC.md` §4).
 *
 * The inputs are facts, not payloads. The change set is produced by `domain/diff` and the blast
 * radius by the `impact` walk, so this module takes their counts and duplicates neither: there
 * is no git call and no graph read here.
 */

/** Three bands, because a number without a band makes every reader invent thresholds. */
export const RISK_BANDS = ["low", "medium", "high"] as const;
export type RiskBand = (typeof RISK_BANDS)[number];

/**
 * What changed. `removed` counts symbols deleted while still referenced, the highest-value
 * review signal (`TOOLS-SPEC.md` §4); `untested` counts changed symbols with no reachable
 * `tested_by` edge.
 */
export interface ChangedFacts {
  readonly symbols: number;
  readonly files: number;
  readonly entryPoints: number;
  readonly untested: number;
  readonly removed: number;
}

/** What the change reaches. `topQn`/`maxIndegree` name the widest single dependant. */
export interface ImpactedFacts {
  readonly nodes: number;
  readonly files: number;
  readonly maxIndegree: number;
  readonly topQn: string | null;
}

/** The score, its band, and the sentences that justify it. */
export interface RiskScore {
  readonly score: number;
  readonly band: RiskBand;
  readonly drivers: readonly string[];
}

/**
 * One term per fact: `weight` is its share of the score and `scale` is the count at which the
 * term saturates. The weights sum to 1, so the score is always in `[0, 1]`; the scales are the
 * "how many is a lot" judgements the formula encodes, and they live here and nowhere else.
 */
const TERMS = {
  /** Dependants are the strongest single signal of blast radius. */
  dependents: { weight: 0.3, scale: 50 },
  /** Breadth matters less than depth: many files, shallow damage. */
  impactedFiles: { weight: 0.15, scale: 20 },
  changedSymbols: { weight: 0.2, scale: 10 },
  /** An untested change is where a regression is found by a user instead of CI. */
  untested: { weight: 0.2, scale: 5 },
  entryPoints: { weight: 0.1, scale: 2 },
  removed: { weight: 0.05, scale: 3 },
} as const;

/** Band floors, read off the published example: `0.62` is medium (`TOOLS-SPEC.md` §4). */
const MEDIUM_FROM = 0.34;
const HIGH_FROM = 0.67;

function round2(value: number): number {
  return Math.round(value * 100) / 100;
}

/** A fact is a count; a negative or non-finite one is a caller bug, so it reads as zero. */
function count(value: number): number {
  return Number.isFinite(value) && value > 0 ? value : 0;
}

/** `count / scale`, saturated at 1: the fifty-first dependant adds no further risk. */
function contribution(value: number, term: { readonly weight: number; readonly scale: number }) {
  return term.weight * Math.min(1, count(value) / term.scale);
}

/** Named so a band is never derived from a bare literal at a call site. */
function bandOf(score: number): RiskBand {
  if (score >= HIGH_FROM) {
    return "high";
  }
  return score >= MEDIUM_FROM ? "medium" : "low";
}

/**
 * Driver sentences, in a fixed order so the same facts always produce the same list. The order
 * is declared here rather than sorted by contribution: a caller that reordered drivers would be
 * re-ranking risk, which is not a display decision.
 *
 * At least one driver is always produced — the changed-symbols sentence is unconditional — so a
 * risk answer can never be a bare number.
 */
function driversOf(changed: ChangedFacts, impacted: ImpactedFacts): string[] {
  const dependents = count(impacted.nodes);
  const symbols = count(changed.symbols);
  const files = count(changed.files);
  const untested = count(changed.untested);
  const entryPoints = count(changed.entryPoints);
  const removed = count(changed.removed);

  const drivers: string[] = [
    dependents === 0
      ? "no dependents were found inside the expanded radius"
      : `${dependents} dependents sit inside the expanded radius`,
  ];

  if (impacted.topQn !== null && count(impacted.maxIndegree) > 0) {
    drivers.push(
      `the widest single impact is ${impacted.topQn} with ${count(impacted.maxIndegree)} direct dependents`,
    );
  }

  drivers.push(`${symbols} changed symbols span ${files} files`);

  drivers.push(
    untested === 0
      ? "every changed symbol has a test within the tested-depth limit"
      : `${untested} changed symbols have no reachable test`,
  );

  if (entryPoints > 0) {
    drivers.push(`${entryPoints} changed symbols are entry points`);
  }

  if (removed > 0) {
    drivers.push(`${removed} removed symbols are still referenced`);
  }

  return drivers;
}

/**
 * Scores a change set against its blast radius. Pure: the same facts always yield the same
 * score, band and drivers, which is what lets `summary` and `impact` agree.
 */
export function score(changed: ChangedFacts, impacted: ImpactedFacts): RiskScore {
  const total =
    contribution(impacted.nodes, TERMS.dependents) +
    contribution(impacted.files, TERMS.impactedFiles) +
    contribution(changed.symbols, TERMS.changedSymbols) +
    contribution(changed.untested, TERMS.untested) +
    contribution(changed.entryPoints, TERMS.entryPoints) +
    contribution(changed.removed, TERMS.removed);

  const rounded = round2(Math.min(1, Math.max(0, total)));
  return { score: rounded, band: bandOf(rounded), drivers: driversOf(changed, impacted) };
}
