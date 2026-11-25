//! `impact`: what a change set can reach, how far, through what, and whether it is tested
//! (`TOOLS-SPEC.md` §6).
//!
//! Ported from `src/tools/impact/handler.ts` (695 lines). The handler computes nothing a domain
//! module already owns: the change set is [`sangkar_domain::diff::changed_since`] (which takes the
//! injected digest source — a handler has no connection), the blast radius is one inbound walk
//! through [`sangkar_domain::neighbors::expand`], the risk number is [`sangkar_domain::risk`],
//! coverage is [`sangkar_domain::coverage`], the unresolved breakdown is
//! [`sangkar_domain::unresolved`] and sizing is [`sangkar_core::budget::emit`]. This file decides
//! *which* of those to call, in *what order*, and how their facts become the documented payload.
//!
//! ## The decisions this file makes, in the order a call meets them
//!
//! - **the generation guard comes before the arguments.** No published generation means no cursor
//!   can bind and no absence can be claimed, so the answer is `bootstrapping` with a retry interval
//!   rather than a refusal.
//! - **`args_hash_of` hashes the resolved identity**: `{paths, base, depth, budget}` as compact JSON,
//!   in that key order, truncated to 32 hex characters with no `sha256:` prefix. All three
//!   properties are wire contract — the cursor's `argsHash` member is the same 32 characters
//!   (`rust/tests/parity/README.md`).
//! - **a cursor is verified before any work.** A stale or foreign token must never be answered with
//!   a partial page that looks like a real one (`DATABASE.md` §8), so the refusal returns before the
//!   change set is read or the graph walked.
//! - **inbound only.** A changed symbol is reached *by* its dependents, so `direction: in` is the
//!   direction that answers "what breaks". Outbound reach is `trace`'s question.
//! - **`via` is the edge kind that reached the node first.** Deduping by node id at the lowest hop,
//!   with `via`/`tier` from the group that found it, is what makes "impacted" mean something: a bare
//!   id list would say a node is affected without saying whether that is a call, an import or an
//!   inheritance.
//! - **a changed path the graph cannot explain never disappears.** It lands in `unmatched_files`
//!   with a reason, never in `changed` and never silently: `not_indexed` when there is no `file` row
//!   at all, `no_nodes_in_graph` when the row exists but the file defines no symbols, and the diff's
//!   own reasons when git reported a change this graph cannot parse.
//! - **a removal is a change, so it stays in `changed`.** A deleted path is in `changed` with
//!   `symbols: 0`, but it seeds nothing and scopes nothing: its bytes are gone, so there is no symbol
//!   to rank and no call site left to repair, and walking the graph's surviving rows for it would
//!   invent a blast radius for a deletion. This is the filed bug
//!   `tests/tools-impact.test.ts:695-700` names — before it, a deletion was only the number
//!   `changeSet.removed.length`, and a review could not see what the change set contained.
//! - **freshness is never assumed.** With an explicit `paths` argument the working tree is never
//!   compared to the index, so `read_status.stale` is `true` and the total can only be `gte`:
//!   the tool refuses to claim its emptiness is complete.
//! - **a failed read is an error envelope with the error's own code**, never `not_found`: a failed
//!   read is not an absence claim.
//!
//! Two porting decisions are visible in the code:
//!
//! - **`risk.score` is serialised through [`RiskValue`].** The domain returns an `f64`, and `ryu`
//!   writes an integral one as `0.0` where `JSON.stringify` writes `0` — a clean tree's
//!   `{"score":0}` would become `{"score":0.0}`, a byte-level divergence in the one field an agent
//!   quotes, and exactly the shape the `impact.base.json` golden pins. `RiskValue` writes an integer
//!   for an integral value and an `f64` otherwise. It is a *local* newtype on purpose: `summary` has
//!   the same requirement and its own copy, and a shared helper is TASK-034's reconciliation, not
//!   something two tools should each invent under each other.
//! - **`read_status.indexed_at` is the served generation's `published_at`.** `sangkar_store`'s
//!   [`GraphMeta`] carries no `indexed_at` (the phase-1 deviation on the record), and the newest
//!   publish is the record of when the bytes being described were written. `ctx.now` speaks the
//!   wire's ISO-8601-with-milliseconds string, so `stale_checked_at` is [`ToolContext::now`]'s value
//!   as it stands.
//!
//! `println!`/`print!`/`eprintln!` never appear here: stdout carries the protocol and
//! [`sangkar_core::log`] owns stderr.

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use sangkar_core::budget::{budget_floor_of, emit};
use sangkar_core::cursor::{CursorExpectation, CursorPayload, CursorResult};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, TotalRelation, Totals,
    bootstrapping, fail, ok,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::paths::normalize_rel;
use sangkar_domain::coverage::{CoverageAssembly, CoverageScope, assemble, coverage_source};
use sangkar_domain::diff::{
    ChangeEntry, ChangeSet, ChangeState, ChangedSinceOptions, ReaderDigests, UnmatchedReason,
    changed_since, sha256_hex,
};
use sangkar_domain::neighbors::{Direction, NeighbourGroup, NeighbourSource, expand};
use sangkar_domain::risk::{ChangedFacts, ImpactedFacts, RiskScore, score};
use sangkar_domain::unresolved::{UnresolvedBucket, UnresolvedCensus, bucket_census};
use sangkar_store::read::context::ToolContext;
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::reader::{GenerationRow, GraphReader};
use serde::Serialize;
use serde::ser::Serializer;
use serde_json::{Map, Value};

use crate::impact::manifest::{ImpactArgs, NAME};

/// Where an agent goes next from a blast radius: widen one node, or the callers behind it.
const NEXT_TOOLS: [&str; 2] = ["trace", "expand"];

/// 16 bytes of SHA-256 is ample for "is this the same call"; the full digest only lengthens `next`.
const ARGS_HASH_CHARS: usize = 32;

/// What an unpublished database asks a client to wait before asking again.
const BOOTSTRAP_RETRY_MS: u64 = 1_000;

/// The bootstrap answer's wording, fixed here so the message and the hint cannot drift apart.
const UNPUBLISHED_SUMMARY: &str =
    "the graph has no published generation yet; impact needs an index";
const UNPUBLISHED_REASON: &str = "no generation has been published";
const UNPUBLISHED_MESSAGE: &str = "impact needs a published generation to answer from";
const UNPUBLISHED_HINT: &str = "index the repository, then call impact again";

/// The one staleness sentence an explicit change set can honestly produce: the working tree was
/// never compared to the index, so no claim about drift was made.
const EXPLICIT_STALENESS: &str =
    "the change set was given explicitly, so the working tree was not compared to the index";

/// Why a refused call has no freshness verdict.
const REFUSED_STALENESS: &str = "the cursor was refused before the change set was read";
const UNREAD_STALENESS: &str = "the change set could not be read";
const UNSIZED_STALENESS: &str = "the answer could not be sized to the requested budget";

/// The two summaries a failed call can carry, so a client never has to parse the reason out of
/// `error.code` alone.
const REFUSED_SUMMARY: &str = "the request was refused before the change set was read";
const UNREAD_SUMMARY: &str = "impact: the answer could not be read";

/// Coverage of a request that answered before any scope was read: nothing is claimed by it.
const NO_COVERAGE: Coverage = Coverage {
    complete: false,
    indexed: 0,
    skipped: 0,
    failed: 0,
    unresolved_edges: 0,
    not_indexed: Vec::new(),
    tests: None,
};

/// `Number.MAX_SAFE_INTEGER`: past it an `f64` cannot represent every integer, so writing one would
/// claim a precision the value does not have.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_991.0;

/// A JSON float the wire carries, serialised so an integral value loses the `.0`.
///
/// `JSON.stringify(0)` is `0`; serde_json's shortest-round-trip writer for `0.0f64` is `0.0`. The
/// domain's clamp and two-decimal rounding produce exact integers routinely (a clean tree's `0.0`,
/// a saturated `1.0`), so without this wrapper the tool's own answer to an empty change set would
/// differ from the captured one (`impact.base.json`: `"score":0`) in the field an agent quotes most.
/// `f64::fract` is `0.0` for every integral value including `-0.0`, which therefore also writes `0`.
///
/// Local to this tool: `summary` needs the same spelling and carries its own copy, because a tool
/// module may not name another and the shared home for it is the registry work, not this file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RiskValue(f64);

impl RiskValue {
    /// A score exactly as the domain computed it.
    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self(value)
    }

    /// The number itself, for a comparison that is not about its spelling.
    #[must_use]
    pub const fn value(self) -> f64 {
        self.0
    }

    /// The value as an integer, when it renders without a fractional part.
    ///
    /// `None` for `NaN`/infinite values (which `serde_json` writes as `null`, as `JSON.stringify`
    /// does) and for anything with a fraction. The `[0, 1]` bound is the domain's; this wrapper does
    /// not re-clamp, because a second clamp would be a second opinion about what a score is.
    fn integral(self) -> Option<i64> {
        if self.0.is_finite() && self.0.fract() == 0.0 && self.0.abs() <= MAX_EXACT_INTEGER {
            Some(self.0 as i64)
        } else {
            None
        }
    }
}

impl Serialize for RiskValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.integral() {
            Some(whole) => serializer.serialize_i64(whole),
            None => serializer.serialize_f64(self.0),
        }
    }
}

/// The same spelling inside a sentence, so a summary line that quotes the score matches the payload.
impl fmt::Display for RiskValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.integral() {
            Some(whole) => write!(f, "{whole}"),
            None => write!(f, "{}", self.0),
        }
    }
}

/// The score, its band and the sentences that justify it. A number without a driver list makes every
/// reader invent thresholds, so the three always travel together.
#[derive(Clone, Debug, Serialize)]
struct RiskPayload {
    score: RiskValue,
    band: &'static str,
    drivers: Vec<String>,
}

impl RiskPayload {
    fn of(risk: &RiskScore) -> Self {
        Self {
            score: RiskValue::from_f64(risk.score),
            band: risk.band.as_str(),
            drivers: risk.drivers.clone(),
        }
    }
}

/// Why a changed path contributed nothing. [`UnmatchedReason`] plus the two answers only an
/// explicit change set can produce, each of which is one distinct fact:
///
/// - the diff's own reasons, reused verbatim so `impact` and `summary` word the same fact the same
///   way;
/// - [`Self::NotIndexed`] — an explicit `paths` entry with no `file` row: never looked at, rather
///   than looked at and refused;
/// - [`Self::NoNodesInGraph`] — indexed, but the file defines no symbols (empty, or the publish wrote
///   no nodes for it), so nothing can be reached from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImpactUnmatchedReason {
    Diff(UnmatchedReason),
    NotIndexed,
    NoNodesInGraph,
}

impl ImpactUnmatchedReason {
    /// The wire spelling, one value per distinct answer.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Diff(reason) => reason.as_str(),
            Self::NotIndexed => "not_indexed",
            Self::NoNodesInGraph => "no_nodes_in_graph",
        }
    }
}

impl Serialize for ImpactUnmatchedReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// One changed path the graph can explain. `symbols` is a count, per the documented payload;
/// `state`/`from` come from the diff's typed entries so a deletion and a rename read as what they
/// are instead of as an unlabelled path.
#[derive(Clone, Debug, Serialize)]
struct ChangedFileRow {
    path: String,
    state: &'static str,
    /// The path a rename came from; absent for every other state.
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<String>,
    /// Declarations the graph holds for this path; 0 for a deleted path, whose symbols are gone.
    symbols: u64,
    entry_point: bool,
}

/// One dependant of the change set, with how it was reached and whether a test covers it.
#[derive(Clone, Debug, Serialize)]
struct ImpactedNodeRow {
    id: i64,
    qn: String,
    kind: String,
    hop: u8,
    /// The edge kind that first reached this node: `calls`, `imports`, `inherits`, …
    via: String,
    /// `extracted` | `inferred` | … — extracted facts outrank inferred ones in reviews.
    tier: String,
    /// A test is reachable at `metrics.tested_depth`, at whatever depth.
    tested: bool,
    /// Present only when true, as the documented payload shows it.
    #[serde(skip_serializing_if = "Option::is_none")]
    entry_point: Option<bool>,
}

/// One changed path that produced nothing, with the reason it produced nothing.
#[derive(Clone, Debug, Serialize)]
struct UnmatchedFileRow {
    path: String,
    reason: ImpactUnmatchedReason,
}

/// The six counts of the closed taxonomy, every bucket present at zero rather than omitted: a client
/// reads this block by key, and an absent `ambiguous: 0` would read as "not asked". The order is
/// [`UNRESOLVED_BUCKETS`]'s, which is declaration order here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct UnresolvedBuckets {
    builtin_stdlib: u64,
    external_package: u64,
    internal_module: u64,
    dynamic_property_call: u64,
    receiver_type_unknown: u64,
    ambiguous: u64,
}

impl UnresolvedBuckets {
    fn of(census: &UnresolvedCensus) -> Self {
        Self {
            builtin_stdlib: count(census.count(UnresolvedBucket::BuiltinStdlib)),
            external_package: count(census.count(UnresolvedBucket::ExternalPackage)),
            internal_module: count(census.count(UnresolvedBucket::InternalModule)),
            dynamic_property_call: count(census.count(UnresolvedBucket::DynamicPropertyCall)),
            receiver_type_unknown: count(census.count(UnresolvedBucket::ReceiverTypeUnknown)),
            ambiguous: count(census.count(UnresolvedBucket::Ambiguous)),
        }
    }
}

/// The change set's unresolved call sites, explained instead of totalled (#5). `total` is copied from
/// `coverage.unresolved_edges` — one count of one array — and `material` is the narrower population
/// that could still be user-defined code: a lower bound because the indexer gave up, and a lower
/// bound because the target was stdlib, are not the same fact.
///
/// An explicit change set that matched nothing reports zeros: it measured nothing, and the key is
/// present because an absent census would read as "nothing unresolved".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct UnresolvedCensusPayload {
    total: u64,
    material: u64,
    buckets: UnresolvedBuckets,
}

/// The payload `TOOLS-SPEC.md` §6 documents, and no other key.
#[derive(Clone, Debug, Serialize)]
struct ImpactPayload {
    changed: Vec<ChangedFileRow>,
    impacted: Vec<ImpactedNodeRow>,
    unmatched_files: Vec<UnmatchedFileRow>,
    risk: RiskPayload,
    unresolved: UnresolvedCensusPayload,
}

/// One visited node before its metrics are attached; `file` is for counting, not for the payload.
#[derive(Clone, Debug)]
struct Candidate {
    id: i64,
    qn: String,
    kind: String,
    hop: u8,
    via: String,
    tier: String,
    file: String,
}

/// What the summary sentence needs; a fact object keeps the wording out of the control flow.
struct SummaryFacts {
    explicit: bool,
    requested: usize,
    base_label: String,
    depth: u8,
    changed: usize,
    impacted: usize,
    impacted_files: usize,
    entry_points: usize,
    tested: usize,
    /// The graph's whole test corpus (`reader.tests_census().nodes`), so a zero numerator can be
    /// explained instead of being read as a clean bill of health.
    test_symbols: u64,
    unmatched: usize,
}

/// A count as the wire's `u64`: a `usize` is a length, and the payload spells lengths as numbers.
fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// The caller's `paths`, normalised, deduplicated and sorted — the form the digest hashes, so two
/// spellings of one path (`./src/a.ts`, `src//a.ts`) are the same change set.
fn requested_paths(paths: &[String]) -> Vec<String> {
    let mut kept: Vec<String> = paths
        .iter()
        .map(|path| normalize_rel(path))
        .filter(|path| !path.is_empty())
        .collect();
    kept.sort();
    kept.dedup();
    kept
}

/// The identity a cursor binds to: `{paths, base, depth, budget}` as compact JSON, hashed with
/// SHA-256 and written as the first 32 characters of the bare lowercase hex digest.
///
/// Three properties are contract and none is cosmetic (`rust/tests/parity/README.md`): the digest is
/// **bare** (no `sha256:` prefix, unlike `trace`/`summary`/`status`), **truncated** to 32 characters,
/// and computed over an object whose key order is the insertions below. `preserve_order` keeps that
/// order, so the text is byte-for-byte the `JSON.stringify` the TypeScript hashes. `cursor` is
/// excluded on purpose: a page request must verify against the same call identity as the page it
/// continues, or every resumed call would read as `cursor_mismatch`.
#[must_use]
pub fn args_hash_of(paths: &[String], base: &str, depth: u8, budget: u64) -> String {
    // `JSON.stringify({paths, base, depth, budget})`: one object, no whitespace, strings quoted by
    // the same escaping rules serde_json applies. The insertions below *are* the key order.
    let mut wire = Map::new();
    wire.insert(
        "paths".to_owned(),
        Value::Array(
            paths
                .iter()
                .map(|path| Value::String(path.clone()))
                .collect(),
        ),
    );
    wire.insert("base".to_owned(), Value::String(base.to_owned()));
    wire.insert("depth".to_owned(), Value::from(depth));
    wire.insert("budget".to_owned(), Value::from(budget));
    let digest = sha256_hex(Value::Object(wire).to_string().as_bytes());
    digest.chars().take(ARGS_HASH_CHARS).collect()
}

/// The envelope's freshness block. `stale_reason` is absent exactly when the generation is fresh,
/// matching the envelope's own rule that an optional field is omitted rather than sent as `null`.
fn read_status_of(
    generation: &str,
    indexed_at: &str,
    checked_at: &str,
    staleness: Option<&str>,
) -> ReadStatus {
    ReadStatus {
        generation: generation.to_owned(),
        indexed_at: indexed_at.to_owned(),
        stale: staleness.is_some(),
        stale_reason: staleness.map(str::to_owned),
        stale_checked_at: Some(checked_at.to_owned()),
        decisive: None,
    }
}

/// Why the tree moved since the last publish, or `None` when it did not.
fn staleness_of(changed: usize, removed: usize) -> Option<String> {
    if changed == 0 && removed == 0 {
        return None;
    }
    let mut parts: Vec<String> = Vec::with_capacity(2);
    if changed > 0 {
        parts.push(format!(
            "{changed} file{} changed",
            if changed == 1 { "" } else { "s" }
        ));
    }
    if removed > 0 {
        parts.push(format!(
            "{removed} path{} removed",
            if removed == 1 { "" } else { "s" }
        ));
    }
    Some(format!("{} since index", parts.join(" and ")))
}

/// The census of the rows `assemble` already returned, never a second query: `total` is that
/// array's own coverage count, so `payload.unresolved.total` and `coverage.unresolved_edges` cannot
/// disagree.
fn census_of(assembly: &CoverageAssembly) -> UnresolvedCensusPayload {
    let census = bucket_census(&assembly.unresolved);
    UnresolvedCensusPayload {
        total: assembly.coverage.unresolved_edges,
        material: count(census.material),
        buckets: UnresolvedBuckets::of(&census),
    }
}

/// A rename arrives as two entries, not one: [`ChangeSet::entries`] is derived from
/// `changed ∪ removed`, and the two halves come from different sources — the git half owns the
/// destination and its `from`, the hash comparison owns the source path once its bytes are gone.
/// Rendering both would report one move as two changed files, so the `deleted` half of a pair is
/// dropped here: the `renamed` entry already carries the fact, `from` included.
fn collapse_renames(entries: &[ChangeEntry]) -> Vec<ChangeEntry> {
    let renamed_from: HashSet<&str> = entries
        .iter()
        .filter_map(|entry| match entry.state {
            ChangeState::Renamed => entry.from.as_deref(),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .filter(|entry| {
            !(entry.state == ChangeState::Deleted && renamed_from.contains(entry.path.as_str()))
        })
        .cloned()
        .collect()
}

/// `showing N of M` when a page was cut, and the window when it was resumed.
fn page_summary(summary: &str, start: u64, shown: usize, total: usize, truncated: bool) -> String {
    if truncated {
        return format!("{summary} Showing {shown} of {total}.");
    }
    if start == 0 {
        return summary.to_owned();
    }
    if shown == 0 {
        return format!("{summary} No impacted nodes remain after position {start} of {total}.");
    }
    let last = start.saturating_add(count(shown));
    format!(
        "{summary} Showing {}-{last} of {total}.",
        start.saturating_add(1)
    )
}

/// The one sentence every answer carries, assembled from the facts rather than from the control
/// flow, so an empty change set and a wide one are worded by the same rules.
fn summary_of(facts: &SummaryFacts) -> String {
    if facts.changed == 0 {
        if facts.unmatched == 0 {
            return format!(
                "no changed files against {}: nothing to impact",
                facts.base_label
            );
        }
        return format!(
            "no changed files in the graph; {} changed path{} could not be matched: nothing to impact",
            facts.unmatched,
            if facts.unmatched == 1 { "" } else { "s" }
        );
    }
    let scope = if facts.explicit {
        format!("{} of {} requested paths", facts.changed, facts.requested)
    } else {
        format!("{} changed files", facts.changed)
    };
    if facts.impacted == 0 {
        return format!("{scope} reach no dependents within {} hops", facts.depth);
    }
    // A zero numerator means one of two very different things, and only the census can say which:
    // "nothing covers this change" is news about the code, "no test symbol was indexed" is news
    // about the index. Without the clause the second reads as the first.
    let coverage = if facts.test_symbols == 0 {
        format!(
            "{} of {} covered by tests; this graph indexes no test symbols (test census: {} nodes).",
            facts.tested, facts.impacted, facts.test_symbols
        )
    } else {
        format!("{} of {} covered by tests.", facts.tested, facts.impacted)
    };
    format!(
        "{scope} reach {} nodes in {} files. {} entry points affected. {coverage}",
        facts.impacted, facts.impacted_files, facts.entry_points
    )
}

/// The read surface the inbound walk needs, with the reader already bound.
///
/// [`sangkar_domain::neighbors`] implements [`NeighbourSource`] for `dyn GraphReader`, but a
/// `&dyn GraphReader` does not coerce to `&dyn NeighbourSource` (that would be a trait-object
/// upcast, not an unsizing coercion). A one-field adapter is the whole fix, and it is also the
/// seam a test could substitute.
struct ReaderNeighbours<'a>(&'a dyn GraphReader);

impl NeighbourSource for ReaderNeighbours<'_> {
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        self.0.inbound(node_id, kinds)
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        self.0.outbound(node_id, kinds)
    }
}

/// Every dependant one seed reaches inbound, deduped by node id at the lowest hop.
///
/// `expand` reports a node once per direction at the hop that first reached it *from one seed*; the
/// merge across seeds is this loop's job, and it keeps the lowest hop and the group (`via`/`tier`)
/// of whichever seed reached it first, so the answer does not depend on the order the frontier was
/// built in.
fn collect_candidates(
    reader: &dyn GraphReader,
    seeds: &[i64],
    depth: u8,
) -> Result<Vec<Candidate>, AppError> {
    let source = ReaderNeighbours(reader);
    // Every seed is pre-marked, so a seed reached through another seed is not new information:
    // `src/shared/domain/neighbors.ts:181` starts its walk with `new Set<number>(seeds)`. Without
    // this, two symbols in the same file reach each other at hop 1 through a `contains` edge and the
    // changed file's own symbols are reported as things the change impacts.
    let seed_set: HashSet<i64> = seeds.iter().copied().collect();
    let mut candidates: BTreeMap<i64, Candidate> = BTreeMap::new();
    for seed in seeds {
        let grouped = expand(&source, *seed, Direction::In, depth, None)?;
        for group in &grouped.inbound {
            let NeighbourGroup {
                kind, tier, items, ..
            } = group;
            for item in items {
                if seed_set.contains(&item.id) {
                    continue;
                }
                if let Some(seen) = candidates.get(&item.id)
                    && seen.hop <= item.hop
                {
                    continue;
                }
                candidates.insert(
                    item.id,
                    Candidate {
                        id: item.id,
                        qn: item.qn.clone(),
                        kind: item.kind.clone(),
                        hop: item.hop,
                        via: kind.clone(),
                        tier: tier.clone(),
                        file: item.file.clone(),
                    },
                );
            }
        }
    }
    let mut ordered: Vec<Candidate> = candidates.into_values().collect();
    // Ordered by hop then id, so a page boundary is a property of the graph rather than of the order
    // the frontier happened to be built in — which is what makes a cursor page stable.
    ordered.sort_by(|left, right| left.hop.cmp(&right.hop).then(left.id.cmp(&right.id)));
    Ok(ordered)
}

/// Answers one `impact` call. A read that cannot answer becomes an `error` envelope carrying the
/// error's own code — a failed read is not an absence claim, so it is never `not_found`.
pub fn handle(ctx: &ToolContext, args: &ImpactArgs) -> Envelope<serde_json::Value> {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => fail(
            error,
            EnvelopeSpec::new(
                UNREAD_SUMMARY.to_owned(),
                read_status_of("", "", &(ctx.now)(), Some(UNREAD_STALENESS)),
                NO_COVERAGE,
            ),
        ),
    }
}

/// The spec a refusal carries: no page, no payload, and a staleness sentence that says why the
/// freshness check never ran.
/// The totals every envelope of this tool carries, the refusals included: an agent parses one
/// totals shape per tool, so a key that disappears on an error path is a key it has to special-case.
fn totals_of(impacted_total: usize, material: u64, relation: TotalRelation) -> Totals {
    // `impact`'s totals put `total_relation` LAST (`handler.ts:289-295`), unlike the relation-first
    // shape most tools use.
    Totals::new(relation)
        .with_total("impacted", Some(count(impacted_total)))
        .with_unresolved_material(material)
        .with_relation_last()
}

/// The spec a refusal carries: no page, no payload, and a staleness sentence that says why the
/// freshness check never ran.
fn refused_spec(
    generation: &str,
    indexed_at: &str,
    checked_at: &str,
    reason: &'static str,
) -> EnvelopeSpec {
    EnvelopeSpec::new(
        REFUSED_SUMMARY.to_owned(),
        read_status_of(generation, indexed_at, checked_at, Some(reason)),
        NO_COVERAGE,
    )
    .with_totals(totals_of(0, 0, TotalRelation::Gte))
    .with_next_tools(NEXT_TOOLS)
}

/// The body of [`handle`], in the contract's order: read meta, guard the generation, hash the call,
/// verify the cursor, assemble the change set, walk, risk, assemble coverage, sign and measure.
fn answer(ctx: &ToolContext, args: &ImpactArgs) -> Result<Envelope<serde_json::Value>, AppError> {
    let paths = requested_paths(args.paths_slice());
    let depth = args.depth();
    let budget = args.budget_or_default().max(1);

    // Read status is needed by every branch below, including the refusals, so it is read once here.
    let meta = ctx.reader.meta_for()?;
    let latest: Option<GenerationRow> = ctx.reader.latest_generation()?;
    let generation = meta
        .generation
        .clone()
        .or_else(|| latest.as_ref().map(|row| row.id.clone()))
        .unwrap_or_default();
    let indexed_at = latest
        .as_ref()
        .map(|row| row.published_at.clone())
        .unwrap_or_default();
    let checked_at = (ctx.now)();

    if generation.is_empty() {
        // No generation means no cursor can bind and no absence can be claimed: say so, do not guess.
        return Ok(bootstrapping(
            BootstrappingSpec::new(
                EnvelopeSpec::new(
                    UNPUBLISHED_SUMMARY.to_owned(),
                    read_status_of("", &indexed_at, &checked_at, Some(UNPUBLISHED_REASON)),
                    NO_COVERAGE,
                ),
                BOOTSTRAP_RETRY_MS,
            )
            .with_message(UNPUBLISHED_MESSAGE)
            .with_hint(UNPUBLISHED_HINT),
        ));
    }

    // The generation's own base commit is the default: the ordinary call spawns no git at all when
    // the publish recorded one, and `changed_since` treats the empty string as `HEAD`.
    let base = args
        .base_or_none()
        .map(str::to_owned)
        .or_else(|| latest.as_ref().and_then(|row| row.base_commit.clone()))
        .unwrap_or_default();
    let args_hash = args_hash_of(&paths, &base, depth, budget);

    // Verify before any work: a refused cursor must not cost a diff and a graph walk, and must never
    // be answered with a partial page that looks like a real one (`DATABASE.md` §8).
    let mut start: u64 = 0;
    if let Some(token) = args.cursor_or_none() {
        let expected = CursorExpectation {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
        };
        match ctx.cursor.verify(token, &expected) {
            CursorResult::Accepted { position } => start = position,
            CursorResult::Refused(refusal) => {
                let error = AppError::new(
                    refusal.code,
                    format!("the cursor was refused: {}", refusal.code),
                )
                .with_hint(refusal.hint);
                return Ok(fail(
                    error,
                    refused_spec(&generation, &indexed_at, &checked_at, REFUSED_STALENESS),
                ));
            }
        }
    }

    // The change set is the diff when no explicit paths were given; `changed_since` reads the
    // reader's own digests, so this stays a handler with no connection.
    let change_set: Option<ChangeSet> = if paths.is_empty() {
        Some(changed_since(
            &ctx.config.repo_root,
            (!base.is_empty()).then_some(base.as_str()),
            &ReaderDigests::new(ctx.reader.as_ref()),
            &ChangedSinceOptions::default(),
        ))
    } else {
        None
    };

    // An explicit `paths` list carries no git status, so its rows are labelled `modified`: that is
    // what "these paths changed" asserts, and inventing `added` from a missing row would be a guess.
    let entries: Vec<ChangeEntry> = match &change_set {
        Some(changes) => collapse_renames(&changes.entries),
        None => paths
            .iter()
            .map(|path| ChangeEntry {
                state: ChangeState::Modified,
                path: path.clone(),
                from: None,
                sha256: None,
            })
            .collect(),
    };
    // A deleted path is in `changed` but seeds nothing and scopes nothing: its bytes are gone, so
    // there is no symbol to rank and no call site left to repair, and counting the graph's surviving
    // rows for it as reachable would inflate the blast radius of a deletion.
    let changed_paths: Vec<String> = entries
        .iter()
        .filter(|entry| entry.state != ChangeState::Deleted)
        .map(|entry| entry.path.clone())
        .collect();
    let removed = change_set
        .as_ref()
        .map_or(0, |changes| changes.removed.len());
    let staleness: Option<String> = match &change_set {
        Some(_) => staleness_of(changed_paths.len(), removed),
        None => Some(EXPLICIT_STALENESS.to_owned()),
    };

    let mut unmatched: BTreeMap<String, UnmatchedFileRow> = BTreeMap::new();
    if let Some(changes) = &change_set {
        for row in &changes.unmatched {
            unmatched.insert(
                row.path.clone(),
                UnmatchedFileRow {
                    path: row.path.clone(),
                    reason: ImpactUnmatchedReason::Diff(row.reason),
                },
            );
        }
    }

    let mut changed: Vec<ChangedFileRow> = Vec::with_capacity(entries.len());
    let mut seeds: Vec<i64> = Vec::new();
    let mut changed_symbols: usize = 0;
    let mut changed_entry_points: usize = 0;
    let mut changed_untested: usize = 0;

    for entry in &entries {
        if entry.state == ChangeState::Deleted {
            // Nothing to rank and nothing to reach, but a review that cannot see the deletion cannot
            // see the change: the row stays, with the zero symbol count the documented payload
            // promises.
            changed.push(ChangedFileRow {
                path: entry.path.clone(),
                state: entry.state.as_str(),
                from: entry.from.clone(),
                symbols: 0,
                entry_point: false,
            });
            continue;
        }
        let Some(file) = ctx.reader.file_by_path(&entry.path)? else {
            // Not in the graph at all: the caller still needs to know its change was not analysed.
            unmatched
                .entry(entry.path.clone())
                .or_insert_with(|| UnmatchedFileRow {
                    path: entry.path.clone(),
                    reason: ImpactUnmatchedReason::NotIndexed,
                });
            continue;
        };
        let nodes = ctx.reader.nodes_by_file(file.id)?;
        if nodes.is_empty() {
            unmatched
                .entry(entry.path.clone())
                .or_insert_with(|| UnmatchedFileRow {
                    path: entry.path.clone(),
                    reason: ImpactUnmatchedReason::NoNodesInGraph,
                });
            continue;
        }

        let mut entry_point = false;
        for node in &nodes {
            seeds.push(node.id);
            let metrics = ctx.reader.metrics_for(node.id)?;
            if is_entry(metrics.as_ref()) {
                entry_point = true;
                changed_entry_points += 1;
            }
            // A node the publish never measured reads as untested: the conservative direction,
            // because "probably covered" is the claim that gets a regression merged.
            if tested_depth(metrics.as_ref()).is_none() {
                changed_untested += 1;
            }
        }
        changed_symbols += nodes.len();
        changed.push(ChangedFileRow {
            path: entry.path.clone(),
            state: entry.state.as_str(),
            from: entry.from.clone(),
            symbols: count(nodes.len()),
            entry_point,
        });
    }

    // One inbound walk from every changed node.
    let ordered = collect_candidates(ctx.reader.as_ref(), &seeds, depth)?;

    let mut impacted: Vec<ImpactedNodeRow> = Vec::with_capacity(ordered.len());
    let mut impacted_files: HashSet<String> = HashSet::new();
    let mut tested_count: usize = 0;
    let mut impacted_entry_points: usize = 0;
    let mut max_indegree: usize = 0;
    let mut top_qn: Option<String> = None;
    for candidate in &ordered {
        let metrics = ctx.reader.metrics_for(candidate.id)?;
        let tested = tested_depth(metrics.as_ref()).is_some();
        let entry = is_entry(metrics.as_ref());
        if entry {
            impacted_entry_points += 1;
        }
        if tested {
            tested_count += 1;
        }
        impacted.push(ImpactedNodeRow {
            id: candidate.id,
            qn: candidate.qn.clone(),
            kind: candidate.kind.clone(),
            hop: candidate.hop,
            via: candidate.via.clone(),
            tier: candidate.tier.clone(),
            tested,
            entry_point: entry.then_some(true),
        });
        impacted_files.insert(candidate.file.clone());
        let in_degree = in_degree_of(metrics.as_ref());
        if in_degree > max_indegree {
            max_indegree = in_degree;
            top_qn = Some(candidate.qn.clone());
        }
    }

    let risk = score(
        &ChangedFacts {
            symbols: changed_symbols,
            files: changed.len(),
            entry_points: changed_entry_points,
            untested: changed_untested,
            removed,
        },
        &ImpactedFacts {
            nodes: impacted.len(),
            files: impacted_files.len(),
            max_indegree,
            top_qn,
        },
    );

    // The change scope's own coverage: a changed file with an unresolved call site is why the total
    // can only be a lower bound, and `assemble` is the module that decides that.
    let assembly = assemble(
        &coverage_source(ctx.reader.as_ref()),
        &CoverageScope {
            paths: Some(changed_paths),
            generation: generation.clone(),
            stale: staleness.is_some(),
            file_ids: None,
            include_tests: false,
        },
    )?;
    let unresolved = census_of(&assembly);

    // One census per answer, never per node: the sentence explains its own numerator, and the
    // published census itself belongs to `status` and `summary`.
    let test_symbols = ctx.reader.tests_census()?.nodes;
    let summary = summary_of(&SummaryFacts {
        explicit: !paths.is_empty(),
        requested: paths.len(),
        base_label: if base.is_empty() {
            "HEAD".to_owned()
        } else {
            base.clone()
        },
        depth,
        changed: changed.len(),
        impacted: impacted.len(),
        impacted_files: impacted_files.len(),
        entry_points: impacted_entry_points,
        tested: tested_count,
        test_symbols,
        unmatched: unmatched.len(),
    });

    let page: Vec<ImpactedNodeRow> = impacted
        .iter()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .cloned()
        .collect();
    let page_length = page.len();
    let payload = ImpactPayload {
        changed,
        impacted: page,
        unmatched_files: unmatched.into_values().collect(),
        risk: RiskPayload::of(&risk),
        unresolved,
    };
    let payload = serde_json::to_value(&payload).map_err(|error| {
        AppError::internal(format!("the impact payload is not serialisable: {error}"))
    })?;

    let read_status = read_status_of(&generation, &indexed_at, &checked_at, staleness.as_deref());
    let totals = totals_of(impacted.len(), unresolved.material, assembly.relation);
    let spec = EnvelopeSpec::new(summary.clone(), read_status, assembly.coverage.clone())
        .with_totals(totals.clone())
        .with_next_tools(NEXT_TOOLS);
    let envelope = ok(payload, spec);

    // `emit` is the only place a payload is cut, and the position it mints counts every list it
    // shrank — so the continuation that is true for *this* list is restated from what survived.
    let minted = emit(envelope, budget, |dropped| {
        let dropped = count(dropped);
        let shown = count(page_length).saturating_sub(dropped);
        ctx.cursor.sign(&CursorPayload {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
            position: start.saturating_add(shown),
        })
    });

    let mut emitted = match minted {
        Ok(emitted) => emitted,
        // A budget above the ceiling is client input and part of the closed error union, so it comes
        // back as an envelope rather than as a thrown handler. The scope *was* read by then, so the
        // refusal reports the coverage it measured instead of claiming nothing was looked at.
        Err(error) => {
            return Ok(fail(
                error,
                EnvelopeSpec::new(
                    summary.clone(),
                    read_status_of(
                        &generation,
                        &indexed_at,
                        &checked_at,
                        Some(UNSIZED_STALENESS),
                    ),
                    assembly.coverage.clone(),
                )
                .with_totals(totals.clone())
                .with_next_tools(NEXT_TOOLS),
            ));
        }
    };

    // Below the floor this answer needs, `emit` returns it untouched: an impact report many times
    // the allowance is not an answer to the budget, so the refusal names the floor to raise to.
    let floor = budget_floor_of(&emitted);
    if floor > budget {
        return Ok(fail(
            AppError::new(
                ErrorCode::BUDGET_TOO_LARGE,
                format!("budget {budget} is below the floor this impact report needs"),
            )
            .with_hint(format!("raise the budget to at least {floor} tokens")),
            EnvelopeSpec::new(
                summary.clone(),
                read_status_of(&generation, &indexed_at, &checked_at, staleness.as_deref()),
                assembly.coverage,
            )
            .with_totals(totals)
            .with_next_tools(NEXT_TOOLS),
        ));
    }

    if !emitted.truncated {
        emitted.summary = page_summary(&summary, start, page_length, impacted.len(), false);
        return Ok(emitted);
    }

    // `emit` dropped items and minted a cursor from its own drop count. The continuation that is
    // true for *this* list is the offset of the row after the last one actually shown, so it is
    // restated from the payload `emit` returned; `next` and `summary` are the two contract fields
    // defined as "derived from the truncation", and nothing about the size decision is re-done here.
    let shown = emitted
        .payload
        .as_ref()
        .and_then(|payload| payload.get("impacted"))
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    emitted.summary = page_summary(&summary, start, shown, impacted.len(), true);
    emitted.next = Some(ctx.cursor.sign(&CursorPayload {
        generation: generation.clone(),
        tool: NAME.to_owned(),
        args_hash: args_hash.clone(),
        position: start.saturating_add(count(shown)),
    }));
    Ok(emitted)
}

/// `metrics.is_entry`, with an unmeasured node counted as not an entry point: the flag is a
/// measurement, so its absence is "no".
fn is_entry(metrics: Option<&NodeMetrics>) -> bool {
    metrics.is_some_and(|metrics| metrics.is_entry)
}

/// Minimum hops to a test, `None` when no test is reachable — including when the publish never
/// measured the node, because "not measured" is not "covered".
fn tested_depth(metrics: Option<&NodeMetrics>) -> Option<i64> {
    metrics.and_then(|metrics| metrics.tested_depth)
}

/// A node's in-degree as a count: `node_metrics` is written by this project, so a negative value is
/// a corrupt row rather than a fact about the graph.
fn in_degree_of(metrics: Option<&NodeMetrics>) -> usize {
    metrics.map_or(0, |metrics| {
        usize::try_from(metrics.in_degree).unwrap_or(usize::MAX)
    })
}
