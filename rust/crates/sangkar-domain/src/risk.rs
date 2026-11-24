//! One risk formula, so `summary` and `impact` cannot disagree (`TECH-SPEC.md` §5.1).
//!
//! Ported from `src/shared/domain/risk.ts`. A score on its own is unusable for deciding what to do,
//! so the return value is a score, a band and driver sentences that carry the numbers: "41
//! dependents sit inside the expanded radius" is what the agent quotes, "0.62" is not
//! (`TOOLS-SPEC.md` §4).
//!
//! The inputs are facts, not payloads. The change set is produced by `domain/diff` and the blast
//! radius by the `impact` walk, so this module takes their counts and duplicates neither: there is
//! no git call and no graph read here.
//!
//! The facts are counts, and they are `usize` rather than `f64`. The TypeScript guards every one of
//! them with `Number.isFinite(value) && value > 0` because a `number` can be `NaN` or negative; a
//! count that cannot be either makes that guard a property of the type. Saturation is unchanged: a
//! count past its term's scale contributes the term's full weight, so no fact set can push the score
//! outside `[0, 1]`.

use std::fmt;

/// Three bands, because a number without a band makes every reader invent thresholds.
pub const RISK_BANDS: [&str; 3] = ["low", "medium", "high"];

/// The band a score falls in. [`RISK_BANDS`] is its wire spelling, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RiskBand {
    Low,
    Medium,
    High,
}

impl RiskBand {
    /// The three, in the order the TypeScript module publishes them.
    pub const ALL: [Self; 3] = [Self::Low, Self::Medium, Self::High];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for RiskBand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What changed. `removed` counts symbols deleted while still referenced, the highest-value review
/// signal (`TOOLS-SPEC.md` §4); `untested` counts changed symbols with no reachable `tested_by` edge.
///
/// Every field is zero for a clean tree, which is the state the risk answer must not dress up as an
/// assessment (`#[derive(Default)]` is that tree).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangedFacts {
    pub symbols: usize,
    pub files: usize,
    pub entry_points: usize,
    pub untested: usize,
    pub removed: usize,
}

/// What the change reaches. `top_qn`/`max_indegree` name the widest single dependant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImpactedFacts {
    pub nodes: usize,
    pub files: usize,
    pub max_indegree: usize,
    pub top_qn: Option<String>,
}

/// The score, its band, and the sentences that justify it.
#[derive(Clone, Debug, PartialEq)]
pub struct RiskScore {
    pub score: f64,
    pub band: RiskBand,
    pub drivers: Vec<String>,
}

/// One term per fact: `weight` is its share of the score and `scale` is the count at which the term
/// saturates.
///
/// The weights sum to 1, so the score is always in `[0, 1]`; the scales are the "how many is a lot"
/// judgements the formula encodes, and they live here and nowhere else.
struct Term {
    weight: f64,
    scale: f64,
}

/// Dependants are the strongest single signal of blast radius.
const DEPENDENTS: Term = Term {
    weight: 0.3,
    scale: 50.0,
};
/// Breadth matters less than depth: many files, shallow damage.
const IMPACTED_FILES: Term = Term {
    weight: 0.15,
    scale: 20.0,
};
const CHANGED_SYMBOLS: Term = Term {
    weight: 0.2,
    scale: 10.0,
};
/// An untested change is where a regression is found by a user instead of CI.
const UNTESTED: Term = Term {
    weight: 0.2,
    scale: 5.0,
};
const ENTRY_POINTS: Term = Term {
    weight: 0.1,
    scale: 2.0,
};
const REMOVED: Term = Term {
    weight: 0.05,
    scale: 3.0,
};

/// Band floors, read off the published example: `0.62` is medium (`TOOLS-SPEC.md` §4).
///
/// Public because a boundary is a fact about the formula rather than a private layout detail: a
/// caller that has to explain why a score is medium quotes the floor, and a test can straddle it
/// without conjuring a fact set that lands exactly on `0.34` or `0.67`.
pub const MEDIUM_FROM: f64 = 0.34;
pub const HIGH_FROM: f64 = 0.67;

/// Two decimals, the precision the published example quotes. Every input has been clamped to
/// `[0, 1]` by then, so `f64::round` and the TypeScript's `Math.round` agree on every value that can
/// reach this function.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

/// `count / scale`, saturated at 1: the fifty-first dependant adds no further risk.
fn contribution(value: usize, term: &Term) -> f64 {
    term.weight * (value as f64 / term.scale).min(1.0)
}

/// Named so a band is never derived from a bare literal at a call site.
#[must_use]
pub fn band_of(score: f64) -> RiskBand {
    if score >= HIGH_FROM {
        return RiskBand::High;
    }
    if score >= MEDIUM_FROM {
        RiskBand::Medium
    } else {
        RiskBand::Low
    }
}

/// Driver sentences, in a fixed order so the same facts always produce the same list. The order is
/// declared here rather than sorted by contribution: a caller that reordered drivers would be
/// re-ranking risk, which is not a display decision.
///
/// Drivers are gated on the change set, never on a census: an empty change set has nothing to
/// assess, so it earns no test-coverage sentence — "every changed symbol has a test" would be a
/// claim about zero symbols (#10). The changed-symbols sentence is unconditional, so a risk answer
/// can never be a bare number.
fn drivers_of(changed: &ChangedFacts, impacted: &ImpactedFacts) -> Vec<String> {
    let mut drivers = Vec::with_capacity(6);
    drivers.push(if impacted.nodes == 0 {
        "no dependents were found inside the expanded radius".to_owned()
    } else {
        format!(
            "{} dependents sit inside the expanded radius",
            impacted.nodes
        )
    });

    if let Some(top_qn) = &impacted.top_qn
        && impacted.max_indegree > 0
    {
        drivers.push(format!(
            "the widest single impact is {top_qn} with {} direct dependents",
            impacted.max_indegree
        ));
    }

    drivers.push(format!(
        "{} changed symbols span {} files",
        changed.symbols, changed.files
    ));

    if changed.symbols > 0 {
        drivers.push(if changed.untested == 0 {
            "every changed symbol has a test within the tested-depth limit".to_owned()
        } else {
            format!(
                "{} changed symbols have no reachable test",
                changed.untested
            )
        });
    }

    if changed.entry_points > 0 {
        drivers.push(format!(
            "{} changed symbols are entry points",
            changed.entry_points
        ));
    }

    if changed.removed > 0 {
        drivers.push(format!(
            "{} removed symbols are still referenced",
            changed.removed
        ));
    }

    drivers
}

/// Scores a change set against its blast radius. Pure: the same facts always yield the same score,
/// band and drivers, which is what lets `summary` and `impact` agree.
#[must_use]
pub fn score(changed: &ChangedFacts, impacted: &ImpactedFacts) -> RiskScore {
    let total = contribution(impacted.nodes, &DEPENDENTS)
        + contribution(impacted.files, &IMPACTED_FILES)
        + contribution(changed.symbols, &CHANGED_SYMBOLS)
        + contribution(changed.untested, &UNTESTED)
        + contribution(changed.entry_points, &ENTRY_POINTS)
        + contribution(changed.removed, &REMOVED);

    let rounded = round2(total.clamp(0.0, 1.0));
    RiskScore {
        score: rounded,
        band: band_of(rounded),
        drivers: drivers_of(changed, impacted),
    }
}
