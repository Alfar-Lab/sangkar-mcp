//! `summary` — the hot path (`TOOLS-SPEC.md` §4): a few hundred tokens that say what changed, how
//! risky it is, and what to call next.
//!
//! Ported from `src/tools/summary/handler.ts` (909 lines). One envelope, three payloads: the scope
//! dispatch happens after the work every scope shares, and the `repo`/`area` aggregates are never
//! computed on the `change` path — the measured failure in the parent tools was a "minimal context"
//! call paying for an architecture overview.
//!
//! Every read comes from `ctx.reader`: no connection, no environment, no stdout. What this module
//! decides is *what to show* — which list leads, how many entry points are worth naming, where a page
//! boundary falls. Whether an absence may claim `eq` is [`sangkar_domain::coverage`]'s decision, taken
//! once over the scope's paths; the risk arithmetic is [`sangkar_domain::risk`]'s; the change set is
//! [`sangkar_domain::diff`]'s; and how the payload is sized is [`sangkar_core::budget`]'s.
//!
//! Pagination is one integer. Each scope names exactly one *paged* list — the ranked one for `change`
//! and `area`, the only unbounded one for `repo` — and the cursor's `position` is the number of its
//! items already delivered. The supporting lists (the changed files, the suggested calls, the entry
//! points) are module-sized and are rebuilt on every page, because they are the context that makes a
//! continuation readable rather than a second half of a ranking.
//!
//! ## The decisions this file makes, in the order a call meets them
//!
//! - **the generation guard comes before the arguments.** An unpublished database answers
//!   `bootstrapping` with a retry interval, never an error: `status` and `index` say the same, and the
//!   agent's next move is to wait, not to repair.
//! - **`argsHash` hashes the resolved identity**, `[scope, target, base, budget]` as compact JSON, so
//!   an omitted `base` and the generation's own base hash the same call and a cursor cannot resume a
//!   different one.
//! - **a cursor is verified before any work.** A stale or foreign token must never produce a partial
//!   page, so a refusal returns before the change set is read.
//! - **scope paths are prefixes.** The reader answers path *prefixes*, so a whole-repository scope
//!   queries one prefix per top-level segment: that is the documented coverage of the change payload
//!   (530 indexed files, not the 3 changed ones), while a directory no indexed file lives in is
//!   outside every prefix — the reader exposes no "every skipped path" query, and
//!   [`CoverageAssembly::not_indexed_total`] is the graph-wide count that keeps the two apart.
//! - **a rename is one entry, not two.** `ChangeSet::entries` is derived from `changed ∪ removed`, and
//!   the two halves come from different sources — git owns the destination and its `from`, the hash
//!   comparison owns the source path once its bytes are gone. Rendering both would report one move as
//!   two changed files, so [`collapse_renames`] drops the `deleted` half of a pair. `--no-renames` is
//!   gone: git's detection always runs, because only it can say "this moved".
//! - **an empty change set is not a tested one** (the filed bug this task must preserve): the
//!   coverage sentence is gated on there being something to assess, and `test_gaps_total` says
//!   `null` — "not applicable" — rather than `0`, which would claim the tree has no untested code.
//!   The two *fact* drivers `risk::drivers_of` always emits stay: a risk answer is never a bare
//!   number, and "0 changed symbols span 0 files" is the fact, not a coverage claim.
//! - **the scope's census is the rows `assemble` already returned**, never a second look at the
//!   unresolved table: `total` is the coverage count of that same array, so the payload and
//!   `coverage.unresolved_edges` cannot disagree.
//! - **a bounded list that hit its bound says so.** Entry points and hotspots are statement bounds;
//!   a full list means the repository has more than one summary line can name, which is `item_cap`.
//! - **a failed read is an error envelope with the error's own code**, never `not_found`: a failed
//!   read is not an absence claim.
//!
//! Two porting decisions are visible in the code:
//!
//! - **`score` is serialised through [`RiskValue`].** The domain returns an `f64`, and `ryu` writes an
//!   integral one as `0.0` where `JSON.stringify` writes `0` — a clean tree's `{"score":0}` would
//!   become `{"score":0.0}`, a byte-level divergence in the one field an agent quotes. `RiskValue`
//!   writes an integer for an integral value and an `f64` otherwise, `-0.0` included (it becomes `0`,
//!   which is what the clamp and `Math.round` produce on the TypeScript side).
//! - **`read_status.indexed_at` is the served generation's `published_at`.** `sangkar_store`'s
//!   [`GraphMeta`] carries no `indexed_at` (the phase-1 deviation on the record), and the newest
//!   publish is the record of when the bytes being described were written. `ctx.now` speaks the wire's
//!   ISO-8601-with-milliseconds string, so `stale_checked_at` is [`ToolContext::now`]'s value as it
//!   stands.
//!
//! `println!`/`print!`/`eprintln!` never appear here: stdout carries the protocol and
//! [`sangkar_core::log`] owns stderr.

use std::collections::{HashMap, HashSet};
use std::fmt;

use sangkar_core::budget::{budget_floor_of, emit};
use sangkar_core::cursor::{CursorExpectation, CursorPayload, CursorResult};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, TotalRelation, Totals,
    TruncationReason, bootstrapping, fail, ok,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::paths::normalize_rel;
use sangkar_domain::coverage::{CoverageAssembly, CoverageScope, assemble, coverage_source};
use sangkar_domain::diff::{
    ChangeEntry, ChangeSet, ChangeState, ChangedSinceOptions, ReaderDigests, changed_since,
    sha256_hex,
};
use sangkar_domain::risk::{ChangedFacts, ImpactedFacts, RiskScore, score};
use sangkar_domain::unresolved::{UnresolvedBucket, UnresolvedCensus, bucket_census};
use sangkar_store::read::context::{LogField, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage};
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{GenerationRow, GraphMeta, GraphReader, TEST_GAP_MAX_DEPTH};
use serde::Serialize;
use serde::ser::Serializer;
use serde_json::Value;

use crate::summary::manifest::{NAME, Scope, SummaryArgs};

/// Where an agent goes next from a change summary: the blast radius, the callers, one node's detail.
const CHANGE_NEXT_TOOLS: [&str; 3] = ["impact", "trace", "expand"];

/// Where an agent goes next from a repository overview: the callers, the blast radius, a broader
/// query. `expand` is not offered because a repository is not one node.
const REPO_NEXT_TOOLS: [&str; 3] = ["trace", "impact", "search"];

/// Where an agent goes next from one path: the callers behind its symbols, their blast radius, the
/// detail of one of them.
const AREA_NEXT_TOOLS: [&str; 3] = ["trace", "impact", "expand"];

/// Display bounds, not product knobs: ten entry points and three flows answer "what is this
/// repository" and ten hotspots answer "what is coupled". The payload is ~300 tokens against the
/// 1200 budget, because a cold answer that does not fit is not a summary.
const ENTRY_POINT_LIMIT: usize = 10;
const HOTSPOT_LIMIT: usize = 10;
const FLOW_LIMIT: usize = 3;
const SUGGESTION_LIMIT: usize = 3;

/// What `status`/`index` ask a client to wait when there is no published generation yet.
const BOOTSTRAP_RETRY_MS: u64 = 2_000;

/// The bootstrap answer's wording, fixed here so the message and the hint cannot drift apart.
const UNPUBLISHED_SUMMARY: &str = "the index has no published generation yet";
const UNPUBLISHED_REASON: &str = "no generation has been published";
const UNPUBLISHED_MESSAGE: &str = "summary needs a published generation to answer from";
const UNPUBLISHED_HINT: &str = "run the index tool, then retry";

/// The two summaries a refused call can carry: one per refusal kind, so a client never has to parse
/// which precondition failed out of `error.code` alone.
const REFUSED_SUMMARY: &str = "the request was refused before any page was built";
const UNREAD_ANSWER: &str = "the read that would have answered it failed";

/// The one symbol state this tool can name. `ChangeSet` says whether a *path* was modified, added,
/// deleted or renamed, and a symbol this tool can name belongs to a file the index has a row for — so
/// the symbol is "modified". Inventing "added" from line numbers would be a guess.
const SYMBOL_CHANGE_MODIFIED: &str = "modified";

/// A symbol's risk, the aggregate's, and every other JSON float the wire carries — serialised so an
/// integral value loses the `.0`.
///
/// `JSON.stringify(0)` is `0`; serde_json's shortest-round-trip writer for `0.0f64` is `0.0`. The
/// domain's clamp and two-decimal rounding produce exact integers routinely (a clean tree's `0.0`,
/// a saturated `1.0`), so without this wrapper the tool's own golden answer would differ from the
/// TypeScript's at the byte level in the field an agent quotes most. `f64::fract` is `0.0` for every
/// integral value including `-0.0`, which therefore also writes `0` — the value `Math.round(-0)`
/// produces on the other side.
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

/// `Number.MAX_SAFE_INTEGER`: past it an `f64` cannot represent every integer, so writing one would
/// claim a precision the value does not have.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_991.0;

impl Serialize for RiskValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.integral() {
            Some(whole) => serializer.serialize_i64(whole),
            None => serializer.serialize_f64(self.0),
        }
    }
}

/// The same spelling inside a sentence: `risk 0 (low)` is the TypeScript's template output, and
/// `risk 0.0 (low)` would be a different summary line.
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

/// One path of the change set, labelled with what happened to it. `state` and `from` come from the
/// diff's typed entries instead of git's letter, because a rename is one fact with two paths and a
/// deletion is still part of the change a reviewer has to read.
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

/// One changed symbol, ranked by its own risk. `lines` is the `[start, end]` pair, not two named
/// fields: it is one range, and the TypeScript payload spells it as a tuple.
#[derive(Clone, Debug, Serialize)]
struct ChangedSymbol {
    id: i64,
    qn: String,
    kind: String,
    change: &'static str,
    lines: [i64; 2],
    risk: RiskValue,
}

/// One changed symbol with no test within reach; `reason` carries the measurement.
#[derive(Clone, Debug, Serialize)]
struct TestGap {
    qn: String,
    reason: String,
}

/// A symbol that disappeared from the tree but is still referenced.
#[derive(Clone, Debug, Serialize)]
struct RemovedSymbol {
    qn: String,
    kind: String,
    had_callers: bool,
}

/// A tool call an agent can paste, with the reason this answer suggests it.
#[derive(Clone, Debug, Serialize)]
struct SuggestedNext {
    tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    why: String,
}

/// The six counts of the closed taxonomy, every bucket present at zero rather than omitted: a client
/// reads this block by key, and an absent `ambiguous: 0` would read as "not asked". The order is
/// [`sangkar_domain::unresolved::UNRESOLVED_BUCKETS`]'s, which is declaration order here.
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

/// The scope's unresolved call sites, explained instead of totalled: a flat count tells an agent that
/// an absence answer is a caveat and nothing about which part of it to go and fix.
///
/// `total` is copied from `coverage.unresolved_edges` — the same rows, counted once — and `material`
/// is the narrower population that could still be user-defined code, which is the only part an
/// absence claim has to answer for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
struct UnresolvedCensusPayload {
    total: u64,
    material: u64,
    buckets: UnresolvedBuckets,
}

/// `scope: "change"` — the working-tree diff with risk, the untested symbols and the next calls.
#[derive(Clone, Debug, Serialize)]
struct ChangePayload {
    base: String,
    changed_files: Vec<ChangedFileRow>,
    changed_symbols: Vec<ChangedSymbol>,
    risk: RiskPayload,
    test_gaps: Vec<TestGap>,
    removed_symbols: Vec<RemovedSymbol>,
    suggested_next: Vec<SuggestedNext>,
    unresolved: UnresolvedCensusPayload,
}

/// The generation's own identity counts, copied from the publish row rather than re-counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct SizeBlock {
    files: i64,
    nodes: i64,
    edges: i64,
}

/// One language row of the census. `language` is `null` for a skipped path whose grammar the indexer
/// could not name; the row is kept rather than dropped, because an unattributed path is still a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct LanguageRow {
    language: Option<String>,
    files: i64,
}

/// One ranked symbol with its reachable count, when the publish measured it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct EntryPointPayload {
    qn: String,
    kind: String,
    reachable: Option<i64>,
}

/// One highly connected symbol. The wire spells the two degrees `in` and `out`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct HotspotPayload {
    qn: String,
    #[serde(rename = "in")]
    in_degree: i64,
    #[serde(rename = "out")]
    out_degree: i64,
}

/// One entry point's reachable component: the name to read, how many nodes it reaches, and how many
/// files the first call hop touches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct FlowRow {
    name: String,
    steps: i64,
    files: u64,
}

/// One path the indexer looked at and did not index, with the reason it says so.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct NotIndexedRow {
    path: String,
    reason: String,
}

/// `scope: "repo"` — what the repository is, at the size of one screen.
#[derive(Clone, Debug, Serialize)]
struct RepoPayload {
    size: SizeBlock,
    languages: Vec<LanguageRow>,
    entry_points: Vec<EntryPointPayload>,
    coupling_hotspots: Vec<HotspotPayload>,
    flows: Vec<FlowRow>,
    not_indexed: Vec<NotIndexedRow>,
    unresolved: UnresolvedCensusPayload,
}

/// One declaration a reader of an area sees first: identity, not detail.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct SurfaceRow {
    qn: String,
    kind: String,
}

/// `scope: "area"` — one path's surface and how much depends on it.
#[derive(Clone, Debug, Serialize)]
struct AreaPayload {
    target: String,
    surface: Vec<SurfaceRow>,
    dependents_total: u64,
    risk: RiskPayload,
}

/// An envelope's parts plus the size of the paged list they were taken from.
struct Built {
    payload: Value,
    summary: String,
    totals: Totals,
    next_tools: Vec<String>,
    /// Items in the scope's paged list, before this page was taken.
    paged_length: usize,
    /// A bounded list hit its bound, so the payload shows a leading sample rather than everything.
    item_capped: bool,
}

/// The freshness verdict this scope's own two sources support, already worded.
struct Freshness {
    stale: bool,
    reason: Option<String>,
}

/// `usize` → `u64` without a panic on a 32-bit target: a count that cannot be a total is a bug in the
/// source, not a reason to abort a call.
fn count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// `1 file` / `3 files`: a stale reason is read by a person, not parsed by a machine.
fn counted(count: u64, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

/// Freshness has two sources and needs both: a file row written by an older publish is stale even
/// when its bytes are unchanged, and a working tree that differs from the generation's base commit is
/// stale even when every row was written by the served generation (`DESIGN.md` §10.4).
///
/// The graph-side fact wins when both hold, exactly as the TypeScript's `freshnessOf` decides: one
/// `stale_reason` is a sentence a caller reads, and a file the served generation did not write is the
/// more surprising of the two. `status` joins the two because its report has room for both causes;
/// this hot path names the one that leads.
fn freshness_of(older_files: u64, drift: u64) -> Freshness {
    if older_files > 0 {
        return Freshness {
            stale: true,
            reason: Some(format!(
                "{} in scope {} indexed by an older generation",
                counted(older_files, "file"),
                if older_files == 1 { "was" } else { "were" }
            )),
        };
    }
    if drift > 0 {
        return Freshness {
            stale: true,
            reason: Some(format!("{} changed since index", counted(drift, "file"))),
        };
    }
    Freshness {
        stale: false,
        reason: None,
    }
}

/// The envelope's freshness block. `stale_checked_at` is separate from `indexed_at` because "when was
/// this built" and "when did we last look" are different questions, and only the second one lets an
/// agent decide whether re-asking is worth it.
fn read_status_of(
    generation: &str,
    indexed_at: &str,
    freshness: &Freshness,
    now: &str,
) -> ReadStatus {
    ReadStatus {
        generation: generation.to_owned(),
        indexed_at: indexed_at.to_owned(),
        stale: freshness.stale,
        stale_reason: freshness.reason.clone(),
        stale_checked_at: Some(now.to_owned()),
        decisive: None,
    }
}

/// The identity of the call a cursor belongs to: `sha256:` plus the hex digest of the compact JSON of
/// `[scope, target, base, budget]` (`handler.ts:262-277`).
///
/// The cursor argument is excluded — it is the page being resumed, not part of the query — and the
/// base and budget are the *resolved* values, so an omitted base and an explicit one hash the same
/// call. Public because it is wire contract: the token a truncated answer mints carries this exact
/// string, and `tests/summary.rs` pins it against the digest inside the captured page golden.
#[must_use]
pub fn args_hash_of(scope: Scope, target: &str, base: &str, budget: u64) -> String {
    // `JSON.stringify([scope, target, base, budget])`: one array, no whitespace, strings quoted by the
    // same escaping rules serde_json applies.
    let canonical = Value::Array(vec![
        Value::String(scope.as_str().to_owned()),
        Value::String(target.to_owned()),
        Value::String(base.to_owned()),
        Value::from(budget),
    ]);
    format!("sha256:{}", sha256_hex(canonical.to_string().as_bytes()))
}

/// The prefixes a scope is about: one per top-level segment of the indexed files.
///
/// The reader answers path *prefixes*, so a directory that no indexed file lives in is outside every
/// prefix — only the graph-wide `not_indexed_total` knows about those paths, and that is the number
/// every scope publishes.
fn top_level_prefixes(files: &[IndexedFileRow]) -> Vec<String> {
    let mut prefixes: Vec<String> = files
        .iter()
        .map(|file| match file.path.split_once('/') {
            Some((top, _)) => top.to_owned(),
            None => file.path.clone(),
        })
        .collect();
    prefixes.sort();
    prefixes.dedup();
    prefixes
}

/// The part of a paged list that belongs to a page starting at `start`; a position past the end is an
/// empty page rather than an error, because the last page is allowed to be the short one.
fn slice_from<T: Clone>(items: &[T], start: u64) -> Vec<T> {
    items
        .iter()
        .skip(usize::try_from(start).unwrap_or(usize::MAX))
        .cloned()
        .collect()
}

/// The `test_gaps` sentence: depth is the measurement, so it is what the reason must carry.
fn gap_reason(tested_depth: Option<i64>) -> String {
    match tested_depth {
        None => format!("no tested_by edge within {TEST_GAP_MAX_DEPTH} hops"),
        Some(depth) => {
            format!("nearest test is {depth} hops away, beyond the {TEST_GAP_MAX_DEPTH}-hop limit")
        }
    }
}

/// A node's in-degree as a count: `node_metrics` is written by this project, so a negative value is a
/// corrupt row rather than a fact about the graph.
fn in_degree_of(metrics: Option<&NodeMetrics>) -> u64 {
    metrics.map_or(0, |metrics| u64::try_from(metrics.in_degree).unwrap_or(0))
}

/// One symbol's risk, from the same formula as the aggregate (`sangkar_domain::risk`): its
/// precomputed in-degree is the radius a summary can afford, because the full walk is `impact`'s job.
fn symbol_risk(node: &NodeRow, metrics: Option<&NodeMetrics>, untested: bool) -> f64 {
    let dependents = usize::try_from(in_degree_of(metrics)).unwrap_or(usize::MAX);
    let changed = ChangedFacts {
        symbols: 1,
        files: 1,
        entry_points: usize::from(metrics.is_some_and(|metrics| metrics.is_entry)),
        untested: usize::from(untested),
        removed: 0,
    };
    let impacted = ImpactedFacts {
        nodes: dependents,
        files: usize::from(dependents > 0),
        max_indegree: dependents,
        top_qn: Some(node.qn.clone()),
    };
    score(&changed, &impacted).score
}

/// The census of the rows `assemble` already returned, never a second look at the unresolved table:
/// `total` is the coverage count of that same array, so the two fields are one number and cannot
/// drift. A scope that measured nothing reports zeros, because a missing key would read as "no
/// caveat" — the opposite of what an empty answer means.
fn census_of(assembly: &CoverageAssembly) -> UnresolvedCensusPayload {
    let census = bucket_census(&assembly.unresolved);
    UnresolvedCensusPayload {
        total: assembly.coverage.unresolved_edges,
        material: count(census.material),
        buckets: UnresolvedBuckets::of(&census),
    }
}

/// A rename arrives as two entries, not one: `ChangeSet::entries` is derived from `changed ∪ removed`,
/// and the two halves come from different sources — the git half owns the destination and its `from`,
/// the hash comparison owns the source path once its bytes are gone. Rendering both would report one
/// move as two changed files, so the `deleted` half of a pair is dropped here: the `renamed` entry
/// already carries the fact, `from` included.
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

/// `reachable_count` is the whole component; the file set is only measured at the first call hop.
fn first_hop_files(reader: &dyn GraphReader, node_id: i64) -> Result<u64, AppError> {
    let files: HashSet<String> = reader
        .outbound(node_id, None)?
        .into_iter()
        .map(|edge| edge.node.path)
        .collect();
    Ok(count(files.len()))
}

/// `scope: "change"`: the diff, ranked and risked.
///
/// `changed_files` is every entry (a deletion included), `changed_symbols` is the ranked list and the
/// paged one: it is the largest array, so a budget cut takes its tail and the next position addresses
/// exactly what was not delivered.
fn build_change(
    ctx: &ToolContext,
    changes: &ChangeSet,
    generation: &str,
    base: &str,
    start: u64,
    relation: TotalRelation,
    census: &UnresolvedCensusPayload,
) -> Result<Built, AppError> {
    let entries = collapse_renames(&changes.entries);

    // A changed path with no `file` row is a new (or unparseable) file: it belongs to
    // `changed_files`, which is where an agent looks for it, but it has no symbols to rank. A deleted
    // path is skipped entirely: its bytes are gone, so the nodes the graph still holds for it are
    // history rather than a change to review, and `symbols: 0` is the honest count.
    let mut nodes: Vec<NodeRow> = Vec::new();
    for entry in &entries {
        if entry.state == ChangeState::Deleted {
            continue;
        }
        let Some(file) = ctx.reader.file_by_path(&entry.path)? else {
            continue;
        };
        nodes.extend(ctx.reader.nodes_by_file(file.id)?);
    }

    let mut metrics: HashMap<i64, Option<NodeMetrics>> = HashMap::with_capacity(nodes.len());
    for node in &nodes {
        metrics.insert(node.id, ctx.reader.metrics_for(node.id)?);
    }
    let metric_of = |id: i64| metrics.get(&id).and_then(Option::as_ref);

    // Per-file counts from the rows already collected: one walk over `nodes`, no extra read, and a
    // path the graph cannot explain (a new file, a deletion) reports 0 rather than disappearing.
    let mut per_file: HashMap<&str, (u64, bool)> = HashMap::with_capacity(entries.len());
    for node in &nodes {
        let entry = per_file.entry(node.path.as_str()).or_insert((0, false));
        entry.0 += 1;
        if metric_of(node.id).is_some_and(|metrics| metrics.is_entry) {
            entry.1 = true;
        }
    }

    let changed_files: Vec<ChangedFileRow> = entries
        .iter()
        .map(|entry| {
            let (symbols, entry_point) = per_file
                .get(entry.path.as_str())
                .copied()
                .unwrap_or((0, false));
            ChangedFileRow {
                path: entry.path.clone(),
                state: entry.state.as_str(),
                from: entry.from.clone(),
                symbols,
                entry_point,
            }
        })
        .collect();

    let node_ids: Vec<i64> = nodes.iter().map(|node| node.id).collect();
    let gaps = ctx.reader.test_gaps(&node_ids)?;
    let gap_ids: HashSet<i64> = gaps.iter().map(|gap| gap.id).collect();

    let mut symbols: Vec<ChangedSymbol> = nodes
        .iter()
        .map(|node| ChangedSymbol {
            id: node.id,
            qn: node.qn.clone(),
            kind: node.kind.clone(),
            change: SYMBOL_CHANGE_MODIFIED,
            lines: [node.start_line, node.end_line],
            risk: RiskValue::from_f64(symbol_risk(
                node,
                metric_of(node.id),
                gap_ids.contains(&node.id),
            )),
        })
        .collect();
    // Riskiest first, then by id so two equally risky symbols keep a stable order for a cursor.
    symbols.sort_by(|left, right| {
        right
            .risk
            .value()
            .total_cmp(&left.risk.value())
            .then_with(|| left.id.cmp(&right.id))
    });

    // A tombstone that still had callers is the highest-value review signal, so it leads.
    let mut removed: Vec<RemovedSymbol> = ctx
        .reader
        .removed_symbols_for(generation)?
        .into_iter()
        .map(|row| RemovedSymbol {
            qn: row.qn,
            kind: row.kind,
            had_callers: row.had_callers,
        })
        .collect();
    removed.sort_by(|left, right| {
        right
            .had_callers
            .cmp(&left.had_callers)
            .then_with(|| left.qn.cmp(&right.qn))
    });

    let mut dependents = 0_u64;
    let mut changed_files_with_dependents: HashSet<&str> = HashSet::new();
    let mut max_indegree = 0_u64;
    let mut top_qn: Option<&str> = None;
    for node in &nodes {
        let indegree = in_degree_of(metric_of(node.id));
        dependents += indegree;
        if indegree > 0 {
            changed_files_with_dependents.insert(node.path.as_str());
        }
        if indegree > max_indegree {
            max_indegree = indegree;
            top_qn = Some(node.qn.as_str());
        }
    }

    let changed_facts = ChangedFacts {
        symbols: nodes.len(),
        // The collapsed entry count: one rename is one changed file, not a rename plus a deletion.
        files: entries.len(),
        entry_points: nodes
            .iter()
            .filter(|node| metric_of(node.id).is_some_and(|metrics| metrics.is_entry))
            .count(),
        untested: gaps.len(),
        removed: removed.iter().filter(|row| row.had_callers).count(),
    };
    let impacted_facts = ImpactedFacts {
        nodes: usize::try_from(dependents).unwrap_or(usize::MAX),
        files: changed_files_with_dependents.len(),
        max_indegree: usize::try_from(max_indegree).unwrap_or(usize::MAX),
        top_qn: top_qn.map(str::to_owned),
    };
    let risk = score(&changed_facts, &impacted_facts);

    let test_gaps: Vec<TestGap> = gaps
        .iter()
        .map(|gap| TestGap {
            qn: gap.qn.clone(),
            reason: gap_reason(gap.tested_depth),
        })
        .collect();

    let riskiest = symbols.first();
    let first_gap = test_gaps.first();
    let mut suggested_next: Vec<SuggestedNext> = Vec::with_capacity(SUGGESTION_LIMIT);
    if !nodes.is_empty() {
        suggested_next.push(SuggestedNext {
            tool: "impact".to_owned(),
            query: None,
            why: format!(
                "{dependents} direct dependents on the changed symbols, before expanding the radius"
            ),
        });
    }
    if let Some(riskiest) = riskiest {
        suggested_next.push(SuggestedNext {
            tool: "trace".to_owned(),
            query: Some(riskiest.qn.clone()),
            why: format!(
                "the riskiest changed symbol ({}), so read its callers before editing",
                riskiest.risk
            ),
        });
    }
    if let Some(first_gap) = first_gap {
        suggested_next.push(SuggestedNext {
            tool: "trace".to_owned(),
            query: Some(first_gap.qn.clone()),
            why: first_gap.reason.clone(),
        });
    }
    if suggested_next.is_empty() {
        suggested_next.push(SuggestedNext {
            tool: "summary".to_owned(),
            query: Some("repo".to_owned()),
            why: "no working-tree changes; summarize the repository instead".to_owned(),
        });
    }
    suggested_next.truncate(SUGGESTION_LIMIT);

    // An empty change set is not a tested one: the coverage sentence is gated on there being
    // something to assess, and the total says "not applicable" rather than "zero gaps" — a zero would
    // claim this tree has no untested code, which is a claim about work nobody did (#10).
    let assessable = !nodes.is_empty();
    let coverage_sentence = if !assessable {
        "no changed symbols to assess.".to_owned()
    } else if test_gaps.is_empty() {
        "every changed symbol has a test within the tested-depth limit.".to_owned()
    } else {
        format!("{} untested changes.", test_gaps.len())
    };
    let impact_sentence = top_qn.map_or_else(
        || "no dependants were found for the changed symbols.".to_owned(),
        |qn| format!("Top impact: {qn} ({dependents} dependents)."),
    );
    let summary = format!(
        "{} files changed, {} symbols touched, risk {} ({}). {impact_sentence} {coverage_sentence}",
        entries.len(),
        nodes.len(),
        RiskValue::from_f64(risk.score),
        risk.band
    );

    // Read before the payload takes ownership: the totals are the whole sequence's, never this page's.
    let removed_total = count(removed.len());
    let test_gaps_total = assessable.then(|| count(test_gaps.len()));
    let payload = ChangePayload {
        base: base.to_owned(),
        changed_files,
        changed_symbols: slice_from(&symbols, start),
        risk: RiskPayload::of(&risk),
        test_gaps,
        removed_symbols: removed,
        suggested_next,
        unresolved: *census,
    };
    let totals = Totals::new(relation)
        .with_total("changed_files", Some(count(entries.len())))
        .with_total("changed_symbols", Some(count(nodes.len())))
        .with_total("test_gaps", test_gaps_total)
        .with_total("removed_symbols", Some(removed_total))
        .with_total("impacted", Some(dependents))
        .with_unresolved_material(census.material);

    Ok(Built {
        payload: to_value(&payload)?,
        summary,
        totals,
        next_tools: names(CHANGE_NEXT_TOOLS),
        paged_length: symbols.len(),
        item_capped: false,
    })
}

/// `scope: "repo"`: the repository at the size of one screen.
fn build_repo(
    ctx: &ToolContext,
    generation: &GenerationRow,
    rows: &PathCoverage,
    start: u64,
    relation: TotalRelation,
    census: &UnresolvedCensusPayload,
) -> Result<Built, AppError> {
    let languages: Vec<LanguageRow> = ctx
        .reader
        .census_by_language()?
        .into_iter()
        .map(|row| LanguageRow {
            language: row.language,
            files: row.files,
        })
        .collect();

    let entry_rows = ctx.reader.entry_points(ENTRY_POINT_LIMIT)?;
    let hotspots: Vec<HotspotPayload> = ctx
        .reader
        .hotspots(HOTSPOT_LIMIT)?
        .into_iter()
        .map(|row| HotspotPayload {
            qn: row.qn,
            in_degree: row.in_degree,
            out_degree: row.out_degree,
        })
        .collect();

    let mut flows: Vec<FlowRow> = Vec::with_capacity(FLOW_LIMIT);
    for entry in &entry_rows {
        if flows.len() >= FLOW_LIMIT {
            break;
        }
        // A flow needs a measured component; an unmeasured entry point is a name, not a flow.
        let Some(steps) = entry.reachable else {
            continue;
        };
        flows.push(FlowRow {
            name: entry.qn.clone(),
            steps,
            files: first_hop_files(ctx.reader.as_ref(), entry.id)?,
        });
    }

    let not_indexed: Vec<NotIndexedRow> = rows
        .not_indexed
        .iter()
        .map(|row| NotIndexedRow {
            path: row.path.clone(),
            reason: row.reason.clone(),
        })
        .collect();
    let entry_points: Vec<EntryPointPayload> = entry_rows
        .iter()
        .map(|row| EntryPointPayload {
            qn: row.qn.clone(),
            kind: row.kind.clone(),
            reachable: row.reachable,
        })
        .collect();

    // The dominant language is the first row with the most files, so a tie keeps the census order.
    let mut dominant: Option<&LanguageRow> = None;
    for row in &languages {
        if dominant.is_none_or(|best| row.files > best.files) {
            dominant = Some(row);
        }
    }
    let largest_coupling = hotspots.first();

    // `not_indexed` is the one list this scope does not bound, so it is the paged list: it is what a
    // budget cuts, and a continuation resumes exactly the paths that were not shown.
    let dominance = dominant.map_or_else(
        || "no language was indexed".to_owned(),
        |row| {
            format!(
                "{}-dominant",
                row.language.as_deref().unwrap_or("unattributed")
            )
        },
    );
    let coupling = largest_coupling.map_or_else(
        || "no coupling hotspots were measured.".to_owned(),
        |row| {
            format!(
                "Largest coupling: {} ({} in, {} out).",
                row.qn, row.in_degree, row.out_degree
            )
        },
    );
    // Entry points and hotspots are statement bounds; a full list means the bound was reached and the
    // repository has more of them than one summary line can name.
    let item_capped = entry_rows.len() >= ENTRY_POINT_LIMIT || hotspots.len() >= HOTSPOT_LIMIT;
    let entry_sentence = format!(
        "{} entry points{}.",
        entry_points.len(),
        if item_capped {
            " (top of a longer list)"
        } else {
            ""
        }
    );

    let payload = RepoPayload {
        size: SizeBlock {
            files: generation.files_indexed,
            nodes: generation.nodes,
            edges: generation.edges,
        },
        languages,
        entry_points,
        coupling_hotspots: hotspots,
        flows,
        not_indexed: slice_from(&not_indexed, start),
        unresolved: *census,
    };
    let totals = Totals::new(relation)
        .with_total("languages", Some(count(payload.languages.len())))
        .with_total("entry_points", Some(count(payload.entry_points.len())))
        .with_total(
            "coupling_hotspots",
            Some(count(payload.coupling_hotspots.len())),
        )
        .with_total("flows", Some(count(payload.flows.len())))
        .with_unresolved_material(census.material);
    let paged_length = not_indexed.len();

    Ok(Built {
        summary: format!(
            "{} files, {} nodes, {} edges. {dominance}. {entry_sentence} {coupling}",
            generation.files_indexed, generation.nodes, generation.edges
        ),
        payload: to_value(&payload)?,
        totals,
        next_tools: names(REPO_NEXT_TOOLS),
        paged_length,
        item_capped,
    })
}

/// `scope: "area"`: the declarations of one path, and how much depends on them.
fn build_area(
    ctx: &ToolContext,
    target: &str,
    rows: &PathCoverage,
    start: u64,
    relation: TotalRelation,
    census: &UnresolvedCensusPayload,
) -> Result<Built, AppError> {
    // The surface is the declarations a reader of this area sees first; a member is reached through
    // its parent, which is what `expand(what="members")` is for. Source order inside a file, files in
    // path order, so the list is stable for a cursor.
    let mut nodes: Vec<NodeRow> = Vec::new();
    for file in &rows.files {
        for node in ctx.reader.nodes_by_file(file.id)? {
            if node.parent_id.is_none() {
                nodes.push(node);
            }
        }
    }

    let mut metrics: HashMap<i64, Option<NodeMetrics>> = HashMap::with_capacity(nodes.len());
    for node in &nodes {
        metrics.insert(node.id, ctx.reader.metrics_for(node.id)?);
    }
    let metric_of = |id: i64| metrics.get(&id).and_then(Option::as_ref);
    let node_ids: Vec<i64> = nodes.iter().map(|node| node.id).collect();
    let gaps = ctx.reader.test_gaps(&node_ids)?;

    let mut dependents_total = 0_u64;
    let mut max_indegree = 0_u64;
    let mut top_qn: Option<&str> = None;
    let mut files_with_dependents: HashSet<&str> = HashSet::new();
    for node in &nodes {
        let indegree = in_degree_of(metric_of(node.id));
        dependents_total += indegree;
        if indegree > 0 {
            files_with_dependents.insert(node.path.as_str());
        }
        if indegree > max_indegree {
            max_indegree = indegree;
            top_qn = Some(node.qn.as_str());
        }
    }

    let changed = ChangedFacts {
        symbols: nodes.len(),
        files: rows.files.len(),
        entry_points: nodes
            .iter()
            .filter(|node| metric_of(node.id).is_some_and(|metrics| metrics.is_entry))
            .count(),
        untested: gaps.len(),
        removed: 0,
    };
    let impacted = ImpactedFacts {
        nodes: usize::try_from(dependents_total).unwrap_or(usize::MAX),
        files: files_with_dependents.len(),
        max_indegree: usize::try_from(max_indegree).unwrap_or(usize::MAX),
        top_qn: top_qn.map(str::to_owned),
    };
    let risk = score(&changed, &impacted);

    let surface: Vec<SurfaceRow> = nodes
        .iter()
        .map(|node| SurfaceRow {
            qn: node.qn.clone(),
            kind: node.kind.clone(),
        })
        .collect();

    let label = if target.is_empty() {
        "the repository root"
    } else {
        target
    };
    let payload = AreaPayload {
        target: target.to_owned(),
        surface: slice_from(&surface, start),
        dependents_total,
        risk: RiskPayload::of(&risk),
    };
    let totals = Totals::new(relation)
        .with_total("surface", Some(count(surface.len())))
        .with_unresolved_material(census.material);
    let summary = format!(
        "{label}: {} symbols in {} files. {dependents_total} dependents. risk {} ({}).",
        surface.len(),
        rows.files.len(),
        RiskValue::from_f64(risk.score),
        risk.band
    );

    Ok(Built {
        payload: to_value(&payload)?,
        summary,
        totals,
        next_tools: names(AREA_NEXT_TOOLS),
        paged_length: surface.len(),
        item_capped: false,
    })
}

/// The payload as the [`Value`] the envelope carries, in declaration order.
///
/// A serialisation failure cannot happen for these shapes (no non-string keys, no non-finite numbers
/// the writer would reject), so it is reported as `internal` rather than guessed around.
fn to_value<T: Serialize>(payload: &T) -> Result<Value, AppError> {
    serde_json::to_value(payload).map_err(|error| {
        AppError::internal(format!("the summary payload is not serialisable: {error}"))
    })
}

/// A fixed tool list as the envelope's owned names.
fn names(tools: [&str; 3]) -> Vec<String> {
    tools.iter().map(|name| (*name).to_owned()).collect()
}

/// A refusal or a bootstrap answer still has to carry `read_status` and `coverage`.
fn empty_coverage() -> Coverage {
    Coverage {
        complete: false,
        indexed: 0,
        skipped: 0,
        failed: 0,
        unresolved_edges: 0,
        not_indexed: Vec::new(),
        tests: None,
    }
}

/// The spec a cursor refusal carries: the refusal names no page and no payload, and its staleness
/// says why the freshness check never ran.
fn refused_before_page(generation: &str, indexed_at: &str, now: &str) -> EnvelopeSpec {
    EnvelopeSpec::new(
        REFUSED_SUMMARY.to_owned(),
        ReadStatus {
            generation: generation.to_owned(),
            indexed_at: indexed_at.to_owned(),
            stale: true,
            stale_reason: Some(
                "the freshness check did not run because the call was refused".to_owned(),
            ),
            stale_checked_at: Some(now.to_owned()),
            decisive: None,
        },
        empty_coverage(),
    )
}

/// `bootstrapping` with the fields an unpublished database has: no generation and no coverage.
fn unpublished(ctx: &ToolContext, meta: &GraphMeta) -> Envelope<Value> {
    bootstrapping(
        BootstrappingSpec::new(
            EnvelopeSpec::new(
                UNPUBLISHED_SUMMARY.to_owned(),
                ReadStatus {
                    // The empty id is the truthful value: a fabricated one would give a cursor
                    // something to bind to that no publish will ever match.
                    generation: meta.generation.clone().unwrap_or_default(),
                    indexed_at: String::new(),
                    stale: true,
                    stale_reason: Some(UNPUBLISHED_REASON.to_owned()),
                    stale_checked_at: Some((ctx.now)()),
                    decisive: None,
                },
                empty_coverage(),
            ),
            BOOTSTRAP_RETRY_MS,
        )
        .with_message(UNPUBLISHED_MESSAGE)
        .with_hint(UNPUBLISHED_HINT),
    )
}

/// Answers one `summary` call. A read that cannot answer becomes an `error` envelope carrying the
/// error's own code — a failed read is not an absence claim, so it is never `not_found`.
pub fn handle(ctx: &ToolContext, args: &SummaryArgs) -> Envelope<Value> {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => read_failure(ctx, error),
    }
}

/// The envelope of a read that failed: the error's own code and message on the unscoped skeleton.
fn read_failure(ctx: &ToolContext, error: AppError) -> Envelope<Value> {
    fail(
        error,
        EnvelopeSpec::new(
            "summary: the answer could not be read".to_owned(),
            ReadStatus {
                generation: String::new(),
                indexed_at: String::new(),
                stale: true,
                stale_reason: Some(UNREAD_ANSWER.to_owned()),
                stale_checked_at: Some((ctx.now)()),
                decisive: None,
            },
            empty_coverage(),
        ),
    )
}

/// The body of [`handle`], in the contract's order: read meta, guard the generation, hash the call,
/// verify the cursor, read the change set, assemble coverage, build the scope, sign and measure.
fn answer(ctx: &ToolContext, args: &SummaryArgs) -> Result<Envelope<Value>, AppError> {
    let scope = args.scope_or_default();
    let budget = args.budget_or_default();
    let target = normalize_rel(args.target.as_deref().unwrap_or(""));

    let meta = ctx.reader.meta_for()?;
    let Some(generation_row) = ctx.reader.latest_generation()? else {
        return Ok(unpublished(ctx, &meta));
    };
    // `meta.generation` is what a cursor binds to; a reader that published without stamping the key
    // still served a generation, so its id is the fallback rather than a refusal.
    let generation = meta
        .generation
        .clone()
        .unwrap_or_else(|| generation_row.id.clone());
    let indexed_at = generation_row.published_at.clone();
    let base = args
        .base
        .clone()
        .or_else(|| generation_row.base_commit.clone())
        .unwrap_or_default();

    let args_hash = args_hash_of(scope, &target, &base, budget);
    let now = (ctx.now)();
    // Verify before any work: a stale or foreign token must never produce a partial page.
    let mut start = 0_u64;
    if let Some(token) = args.cursor.as_deref().filter(|token| !token.is_empty()) {
        let expected = CursorExpectation {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
        };
        match ctx.cursor.verify(token, &expected) {
            CursorResult::Accepted { position } => start = position,
            CursorResult::Refused(refusal) => {
                return Ok(fail(
                    AppError::new(
                        refusal.code,
                        "the summary cursor cannot be used for this call",
                    )
                    .with_hint(refusal.hint),
                    refused_before_page(&generation, &indexed_at, &now),
                ));
            }
        }
    }

    // The change set is read on every scope: its drift is part of the freshness verdict, and reading
    // it only on `change` would make one scope's `stale` answer cheaper than another's claim.
    let changes = changed_since(
        &ctx.config.repo_root,
        (!base.is_empty()).then_some(base.as_str()),
        &ReaderDigests::new(ctx.reader.as_ref()),
        &ChangedSinceOptions::default(),
    );
    // `changed_since` already logs its own failure with the reason; what it cannot attribute is the
    // count, and an agent reading a short change set deserves to know one existed.
    if !changes.unmatched.is_empty() {
        ctx.log.info(
            "summary.change.unmatched",
            &[(
                "count",
                LogField::Int(i64::try_from(changes.unmatched.len()).unwrap_or(i64::MAX)),
            )],
        );
    }

    let prefixes = if scope == Scope::Area && !target.is_empty() {
        vec![target.clone()]
    } else {
        top_level_prefixes(&ctx.reader.all_files()?)
    };
    let rows = ctx.reader.rows_for_paths(&prefixes)?;
    let older_files = count(
        rows.files
            .iter()
            .filter(|file| file.indexed_generation != generation)
            .count(),
    );
    let drift = count(changes.changed.len() + changes.removed.len());
    let freshness = freshness_of(older_files, drift);
    let read_status = read_status_of(&generation, &indexed_at, &freshness, &now);

    // One assembly per answer, over exactly the prefixes the scope's payload describes. `include_tests`
    // is set because every scope publishes the test census on `coverage`, and the rows the builders
    // need were read above: a second identical prefix query is a statement-cache hit, not a second
    // population (the same trade `status` makes).
    let coverage_scope = CoverageScope {
        paths: Some(prefixes),
        generation: generation.clone(),
        stale: freshness.stale,
        file_ids: None,
        include_tests: true,
    };
    let assembly = assemble(&coverage_source(ctx.reader.as_ref()), &coverage_scope)?;
    let coverage = assembly.coverage.clone();
    let census = census_of(&assembly);

    let built = match scope {
        Scope::Repo => build_repo(
            ctx,
            &generation_row,
            &rows,
            start,
            assembly.relation,
            &census,
        )?,
        Scope::Area => build_area(ctx, &target, &rows, start, assembly.relation, &census)?,
        Scope::Change => build_change(
            ctx,
            &changes,
            &generation,
            &base,
            start,
            assembly.relation,
            &census,
        )?,
    };

    // Every scope publishes the graph's own skip count, not the length of a scoped list: a caller
    // asking about one area must not read a smaller repository than the one `status` reports.
    let totals = built
        .totals
        .clone()
        .with_total("not_indexed", Some(assembly.not_indexed_total));
    let mut spec = EnvelopeSpec::new(built.summary.clone(), read_status.clone(), coverage.clone())
        .with_totals(totals.clone())
        .with_next_tools(built.next_tools.clone());
    if built.item_capped {
        // A bounded sample is a cut the tool made before the budget did, and it must say so.
        spec = spec.with_truncation(TruncationReason::ItemCap, None);
    }
    let envelope: Envelope<Value> = ok(built.payload.clone(), spec);

    let paged_length = u64::try_from(built.paged_length).unwrap_or(u64::MAX);
    let emitted = emit(envelope, budget, |dropped| {
        let dropped = u64::try_from(dropped).unwrap_or(u64::MAX);
        // `min(paged_length, max(start, paged_length - dropped))`: `emit` counts what it removed from
        // every list, so on a page whose supporting lists were cut too this resumes slightly early
        // rather than skipping a ranked item.
        let position = paged_length.min(start.max(paged_length.saturating_sub(dropped)));
        ctx.cursor.sign(&CursorPayload {
            generation: generation.clone(),
            tool: NAME.to_owned(),
            args_hash: args_hash.clone(),
            position,
        })
    });

    let emitted = match emitted {
        Ok(emitted) => emitted,
        // A budget above the ceiling is client input and part of the closed error union, so it comes
        // back as an envelope rather than as a thrown handler.
        Err(error) => {
            return Ok(fail(
                error,
                refused_before_page(&generation, &indexed_at, &now),
            ));
        }
    };

    // Below the floor this summary needs nothing can be cut to fit, so `emit` returns the envelope
    // untouched: a full repository summary is many times a tiny allowance, and `summary` is the hot
    // path — the refusal names the floor rather than spending the caller's budget on an answer it
    // cannot afford to read.
    let floor = budget_floor_of(&emitted);
    if floor > budget {
        return Ok(fail(
            AppError::new(
                ErrorCode::BUDGET_TOO_LARGE,
                format!("budget {budget} is below the floor this summary needs"),
            )
            .with_hint(format!("raise the budget to at least {floor} tokens")),
            EnvelopeSpec::new(
                built.summary.clone(),
                read_status,
                coverage_without_paths(&coverage),
            )
            .with_totals(totals)
            .with_next_tools(built.next_tools.clone()),
        ));
    }
    Ok(emitted)
}

/// The coverage of a budget refusal: the counts stay exact (`skipped` says how many were not
/// indexed) while the 80 entries that would make this envelope several kilobytes are what the caller
/// was told to make room for.
///
/// A named helper rather than a clone with one field cleared at the call site: no refusal may publish
/// the path list, and a second refusal branch should inherit that instead of repeating it.
fn coverage_without_paths(coverage: &Coverage) -> Coverage {
    let mut refusal = coverage.clone();
    refusal.not_indexed = Vec::new();
    refusal
}
