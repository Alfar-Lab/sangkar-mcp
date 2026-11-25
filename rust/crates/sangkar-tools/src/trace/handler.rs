//! `trace`: one free-text input in, the resolved symbol and its neighbours grouped by relation out
//! (`TOOLS-SPEC.md` §3, `DESIGN.md` §8).
//!
//! Ported from `src/tools/trace/handler.ts` (1193 lines, the largest handler). The handler is the
//! tool's whole policy and does no work of its own: resolution is `sangkar_domain::resolve`, the walk
//! is `sangkar_domain::neighbors`, the absence arithmetic is `sangkar_domain::coverage`, sizing is
//! [`sangkar_core::budget::emit`] and the envelope comes from [`sangkar_core::envelope`]. What is
//! decided here is:
//!
//! - **the cursor comes first.** A resumed call that started at the first item and only then failed
//!   would re-serve a page the client already saw, so a refused cursor returns before resolution runs;
//! - **grouping is the payload's semantic content**, so groups follow a fixed edge order rather than
//!   whatever order the walk produced, and `tier` is hoisted onto the group because every edge in one
//!   group shares it by construction;
//! - **the page boundary lies on the flattened neighbour sequence.** `position` indexes that sequence
//!   (`in` groups in the fixed order, then `out`), so a resumed page continues exactly where the last
//!   one stopped: `mint` mints `start + entries.len()` of what was actually served, never a count of
//!   what `emit` dropped;
//! - **the page is sized before it is emitted.** `emit` cuts the *largest* list first, which on a
//!   dense symbol is the neighbour sequence itself: handing it 3 000 rows so it can tell us what fits
//!   leaves the flat `unresolved` evidence list as the only survivor and mints a page that advances by
//!   zero rows. So the sequence is measured here, the longest prefix that fits is materialised, and
//!   `emit` is handed a page it can only confirm — the boundary is still emit's to move, and a cut it
//!   makes is read back out of what survived ([`leading_survivors`]);
//! - **content may not be outbid by evidence.** `unresolved` is the explanation of a miss, not the
//!   answer, so it is capped to a share of the allowance instead of eating the page;
//! - **a truncated page always advances.** The continuation token is minted past the rows served; the
//!   only zero-row page is a terminal one, which says so and points at the cheaper query;
//! - **a direction that was not walked is not a zero.** `callers_total` is omitted for
//!   `direction: "out"` rather than reported as `0`;
//! - **an unresolved count is never bare.** Each evidence row carries the bucket the store already
//!   knows and the payload carries the census over the same rows `coverage.unresolved_edges`
//!   counted, so a flat number always arrives with its breakdown. The `unresolved` argument narrows
//!   the list and never the census, and the summary says how many rows the filter hid — a bucket
//!   answer must not read as the whole scope's evidence.
//!
//! Never source text: nothing here reads a file, and `emit` is the only place the payload shrinks.
//!
//! Four porting decisions are visible in the code:
//!
//! - **a read error is an error envelope, never `not_found`.** `resolve`, `expand`, `assemble` and
//!   `meta_for` all return `Result`; a failed read must not be flattened into "this symbol has no
//!   callers", so [`handle`] answers it with [`fail`] and the error's own code, exactly as the
//!   TypeScript composition root does with a thrown `AppError`.
//! - **the payload is a `serde_json::Value`.** The TypeScript returns a union of three envelope
//!   shapes; Rust has one envelope type per payload type, so each branch serialises its own payload
//!   struct first ([`payload_value`]) and the envelope is `Envelope<Value>`. That is also what lets
//!   the page loop read back the groups `emit` kept without a second deserialiser per shape.
//! - **`read_status.indexed_at` comes from [`GraphReader::latest_generation`].** `sangkar_store`'s
//!   `GraphMeta` carries no `indexed_at` (the phase-1 deviation on the record), and the served
//!   generation's `published_at` is the only value the reader publishes; the newest row is the one a
//!   published database is serving, and a fifth meta key is not this task's to add.
//! - **the domain traits are bridged by a four-method wrapper.** `sangkar_domain` implements
//!   `ResolutionSource`/`NeighbourSource` *for* `dyn GraphReader`, but Rust cannot coerce
//!   `&dyn GraphReader` into `&dyn ResolutionSource`, so [`ReaderSources`] forwards. It is not a
//!   second database adapter: it holds the aggregate and calls its own methods.

use std::collections::HashSet;

use sangkar_core::budget::{BYTES_PER_TOKEN, emit};
use sangkar_core::cursor::{CursorExpectation, CursorPayload, CursorRefusal, CursorResult};
use sangkar_core::envelope::{
    Coverage, Envelope, EnvelopeSpec, NotIndexedEntry, ReadStatus, TotalRelation, Totals,
    TruncationReason, ambiguous, fail, not_found, ok,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_domain::coverage::{CoverageScope, assemble, coverage_source};
use sangkar_domain::diff::sha256_hex;
use sangkar_domain::neighbors::{
    Direction, GroupedNeighbors, NeighbourGroup, NeighbourItem, NeighbourSource, expand,
};
use sangkar_domain::resolve::{
    CANDIDATE_CAP, Resolution, ResolutionCandidate, ResolutionSource, ResolutionStep,
    ResolveOptions, resolve,
};
use sangkar_domain::unresolved::{UnresolvedBucket, bucket_census, bucket_of};
use sangkar_store::read::context::{LogField, ToolContext};
use sangkar_store::read::coverage::UnresolvedRef;
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{GraphMeta, GraphReader};
use serde::Serialize;
use serde_json::{Map, Value};

use crate::trace::manifest::{DEFAULT_DEPTH, NAME, TraceArgs};

/// The fixed group order (`TOOLS-SPEC.md` §3). It is a contract, not a preference: an agent reads a
/// trace by scanning "who calls me" before "what I import", and the order has to be identical on every
/// page or a resumed page would interleave with the first one.
///
/// `contains` and `defines` are structural edges the walk can also return. They are outside the
/// documented sequence and are appended after it, by edge name, rather than dropped.
const EDGE_ORDER: [&str; 6] = [
    "calls",
    "inherits",
    "implements",
    "references",
    "tested_by",
    "imports",
];

/// Where an agent goes next from a trace: one item's detail. `expand` is the only address for it.
const NEXT_TOOLS: [&str; 1] = ["expand"];

/// Where a page that fits no neighbour sends the agent: the same query one hop cheaper. `expand` is
/// not offered because a neighbour list is what could not be served, and detail on one node is not a
/// substitute for it.
const NEXT_TOOLS_NARROWER: [&str; 1] = ["trace"];

/// A miss is routed to `search` (a broader query) or `status` (is the graph even complete).
const NEXT_TOOLS_AFTER_MISS: [&str; 2] = ["search", "status"];

/// `retry_after_ms` for an unpublished database. The watcher publishes on a debounce, so the useful
/// instruction is "wait one debounce and ask again", not a bare failure.
const UNPUBLISHED_RETRY_MS: u64 = 1_000;

/// An `unresolved` row with no recorded evidence still has to say something; never an empty string.
const NO_EVIDENCE: &str = "no evidence recorded";

/// `DESIGN.md` §8's tier for a call site whose target no node matches.
const UNRESOLVED_TIER: &str = "unresolved";

/// Characters reserved for the continuation token while the page is being sized. `budget.rs` charges
/// 384 for the same token, and a real one is ~150; over-reserving costs a few bytes of page, while
/// under-reserving would make `emit` cut a page that was meant to fit.
const CURSOR_RESERVE_CHARS: usize = 192;

/// Bytes of slack between the measured envelope floor and the page that is added to it: the summary
/// digits move with the counts, and a group contributes a separator that is charged at its start.
/// A page that lands a little short is fine; one that lands long is cut by `emit`.
const SKELETON_SLACK_BYTES: u64 = 128;

/// Bytes a row may cost beyond its own field lengths: JSON escaping in a name (a quote or a backslash
/// adds one) and the group separators. Escapes in indexed names are rare, so this is a floor under the
/// estimate rather than a per-row allowance — `emit` still measures the page it is handed.
const ITEM_SLACK_BYTES: u64 = 8;

/// How much of the byte allowance the evidence lists may claim: `unresolved` plus `not_indexed` may
/// take a quarter, and the neighbour rows — the answer — keep the rest. Before this cap, a dense
/// symbol's 94 unresolved call sites alone were ~90 % of a 3 200-token budget.
const EVIDENCE_SHARE: u64 = 4;

/// Page emits per response. Sizing is one emit; a cut `emit` makes anyway is re-sized from what
/// survived, and five attempts is far past the one or two a real payload needs. With the terminal
/// page's own emit, six calls size any call this tool can make.
const MAX_SIZING_PASSES: usize = 5;

/// `,"external":true` and `,"is_test":true`, present only when true (see [`to_item`]).
const EXTERNAL_FLAG_BYTES: u64 = r#","external":true"#.len() as u64;
const IS_TEST_FLAG_BYTES: u64 = r#","is_test":true"#.len() as u64;

/// One neighbour as the payload shows it: identity, where to read it, which hop found it.
///
/// `external` and `is_test` are present only when true: a repo-relative, non-test neighbour is the
/// normal case, and a repeated `false` is pure payload weight.
#[derive(Clone, Debug, Serialize)]
struct TraceItem {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    line: i64,
    hop: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    external: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_test: Option<bool>,
}

/// All items of one `(edge, tier)` pair; `tier` is shared by construction, hence hoisted here.
#[derive(Clone, Debug, Serialize)]
struct TraceGroup {
    edge: String,
    tier: String,
    items: Vec<TraceItem>,
}

/// The resolution block that lets the agent judge the match without a second call.
#[derive(Clone, Debug, Serialize)]
struct TraceResolution {
    query: String,
    matched: String,
    kind: String,
    how: String,
    confidence: &'static str,
}

/// The resolved node, in the payload's own shape: `lines` is the pair, not `start_line`/`end_line`.
#[derive(Clone, Debug, Serialize)]
struct TraceNode {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    lines: [i64; 2],
    signature: Option<String>,
    is_test: bool,
}

/// One call site the index could not bind, named with the reason so the agent can decide to read it.
#[derive(Clone, Debug, Serialize)]
struct TraceUnresolved {
    file: String,
    line: i64,
    name: String,
    /// Why the row exists, from the closed taxonomy — never a second guess at the store's answer.
    bucket: String,
    evidence: String,
    tier: &'static str,
}

/// The scope's unresolved call sites, explained instead of totalled. `total` is copied from
/// `coverage.unresolved_edges` over the same rows, so the two cannot disagree, and the census
/// describes the *unfiltered* scope: the `unresolved` argument chooses what to read, not which call
/// sites failed to bind.
///
/// Unlike `sangkar_domain`'s census this one lists all six buckets, zeros included: the payload's
/// field is a record a client reads by key, and an absent `ambiguous: 0` would read as "not asked".
#[derive(Clone, Debug, Serialize)]
struct TraceCensus {
    total: u64,
    material: u64,
    buckets: Map<String, Value>,
}

/// The call that turns one candidate into an answer, so the agent issues it unchanged instead of
/// re-assembling a query from `qn` and this call's arguments. `direction` is echoed because it is the
/// caller's own choice and dropping it would answer a different question than the one asked.
#[derive(Clone, Debug, Serialize)]
struct NextCall {
    tool: &'static str,
    query: String,
    direction: &'static str,
}

/// A candidate plus its next call: the resolution contract's row, widened by the tool's own shape.
#[derive(Clone, Debug, Serialize)]
struct TraceCandidate {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    lines: [i64; 2],
    score: f64,
    why: String,
    next_call: NextCall,
}

/// A resolved query's payload. `in`/`out` are Rust keywords, so the fields are named and renamed.
#[derive(Clone, Debug, Serialize)]
struct TracePayload {
    resolution: TraceResolution,
    node: TraceNode,
    #[serde(rename = "in")]
    inbound: Vec<TraceGroup>,
    #[serde(rename = "out")]
    outbound: Vec<TraceGroup>,
    unresolved: Vec<TraceUnresolved>,
    unresolved_census: TraceCensus,
    not_indexed: Vec<NotIndexedEntry>,
}

/// An ambiguous answer is candidates only: no node, no neighbours (`DESIGN.md` §8).
#[derive(Clone, Debug, Serialize)]
struct TraceCandidates {
    candidates: Vec<TraceCandidate>,
    /// The size of the tie before the [`CANDIDATE_CAP`] slice, straight from the resolution.
    candidates_total: usize,
    /// How many candidates this payload carries; the summary line and this cannot disagree.
    candidates_shown: usize,
}

/// Nothing matched: near misses instead of a bare zero.
#[derive(Clone, Debug, Serialize)]
struct TraceNearMisses {
    near_misses: Vec<TraceCandidate>,
}

/// The four methods of the two domain seams, forwarded to the aggregate reader.
///
/// `sangkar_domain` implements both traits *for* `dyn GraphReader`, but a trait-object reference
/// cannot be coerced from one trait to another, so the tool needs this bridge. It holds the
/// aggregate — no connection, no SQL — and every method is one call to the reader's own.
struct ReaderSources<'a>(&'a dyn GraphReader);

impl ResolutionSource for ReaderSources<'_> {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        self.0.find_by_qn(qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        self.0.find_by_name(name, kinds)
    }
}

impl NeighbourSource for ReaderSources<'_> {
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

/// The effective arguments, in a fixed key order, so an implicit default hashes like an explicit one.
///
/// The order of the insertions *is* the contract: `preserve_order` keeps it, so the text this
/// function hashes is byte-for-byte the `JSON.stringify` the TypeScript hashes. `kinds: null` keeps
/// "absent" and "present" distinguishable, and `depth` here is the caller's number, not the clamp —
/// a `depth: 9` is a different call from a `depth: 4` even though both walk four hops.
fn effective_args(args: &TraceArgs) -> Map<String, Value> {
    let mut wire = Map::new();
    wire.insert("query".to_owned(), Value::String(args.query.clone()));
    wire.insert(
        "direction".to_owned(),
        Value::String(args.direction_or_all().as_str().to_owned()),
    );
    wire.insert(
        "kinds".to_owned(),
        match &args.kinds {
            Some(kinds) => Value::Array(kinds.iter().cloned().map(Value::String).collect()),
            None => Value::Null,
        },
    );
    wire.insert(
        "path".to_owned(),
        args.path.clone().map_or(Value::Null, Value::String),
    );
    wire.insert(
        "unresolved".to_owned(),
        args.unresolved.map_or(Value::Null, |bucket| {
            Value::String(bucket.as_str().to_owned())
        }),
    );
    wire.insert(
        "depth".to_owned(),
        Value::from(args.depth.unwrap_or(DEFAULT_DEPTH)),
    );
    wire.insert("budget".to_owned(), Value::from(args.budget_or_default()));
    wire
}

/// The `argsHash` a cursor binds (`DATABASE.md` §8): `sha256:<hex>` over the effective arguments —
/// after defaults, without the cursor itself — so `{query:"x"}` and `{query:"x", depth:2}` are the
/// same call and a resume cannot silently address a different query.
///
/// The digest is [`sha256_hex`], the workspace's one SHA-256: it lives in `sangkar_domain::diff`
/// because the indexer writes digests, but it is a plain content hash over any bytes, and reusing it
/// costs no dependency. `sha2` is already in the workspace through `sangkar-core::cursor`; adding it
/// here would be a second copy of the same primitive plus a hex encoder, for no gain.
#[must_use]
pub fn args_hash_of(args: &TraceArgs) -> String {
    let text = Value::Object(effective_args(args)).to_string();
    format!("sha256:{}", sha256_hex(text.as_bytes()))
}

/// The candidates of an ambiguous or missing answer, each carrying the call that resolves it. `qn` is
/// a unique qualified name, so the offered query is answered by the resolution's first step rather
/// than by the tie that produced the candidate — that is what makes "issue it unchanged" true.
fn with_next_call(candidates: &[ResolutionCandidate], direction: Direction) -> Vec<TraceCandidate> {
    candidates
        .iter()
        .map(|candidate| TraceCandidate {
            id: candidate.id,
            qn: candidate.qn.clone(),
            kind: candidate.kind.clone(),
            file: candidate.file.clone(),
            lines: [candidate.lines.start, candidate.lines.end],
            score: candidate.score,
            why: candidate.why.clone(),
            next_call: NextCall {
                tool: NAME,
                query: candidate.qn.clone(),
                direction: direction.as_str(),
            },
        })
        .collect()
}

/// Confidence of the *match*, not of an edge: `extracted` when the source names the node outright (an
/// exact qualified name or an exact bare name), `inferred` when the hit came from weaker evidence,
/// such as a dotted tail or a path (`DESIGN.md` §8's tiers). A tie never reaches this function.
fn confidence_of(step: ResolutionStep) -> &'static str {
    match step {
        ResolutionStep::ExactQn | ResolutionStep::ExactName => "extracted",
        ResolutionStep::DottedTail | ResolutionStep::Path | ResolutionStep::Decorator => "inferred",
    }
}

fn edge_rank(edge: &str) -> usize {
    EDGE_ORDER
        .iter()
        .position(|known| *known == edge)
        .unwrap_or(EDGE_ORDER.len())
}

/// The walk's groups in the contract's order. `expand` already sorts by edge name; this re-sorts by
/// the fixed list so a first page and a resumed page group identically, then by edge name (for edges
/// outside the list) and by tier, so the order is total rather than merely "usually stable".
fn ordered_groups(groups: &[NeighbourGroup]) -> Vec<&NeighbourGroup> {
    let mut ordered: Vec<&NeighbourGroup> = groups.iter().collect();
    ordered.sort_by(|left, right| {
        edge_rank(&left.kind)
            .cmp(&edge_rank(&right.kind))
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.tier.cmp(&right.tier))
    });
    ordered
}

/// One neighbour as the payload shows it.
///
/// `is_test` reads the node's kind: `expand`'s item does not carry `node.is_test`, and looking every
/// neighbour up again would be one query per item. `test` is the kind the indexer gives a test node,
/// so it is the label that is known at this seam rather than one guessed from a path.
fn to_item(item: &NeighbourItem) -> TraceItem {
    TraceItem {
        id: item.id,
        qn: item.qn.clone(),
        kind: item.kind.clone(),
        file: item.file.clone(),
        line: item.line,
        hop: item.hop,
        external: item.external.then_some(true),
        is_test: (item.kind == "test").then_some(true),
    }
}

/// Which half of the walk an entry came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    In,
    Out,
}

impl Side {
    const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }
}

/// One entry of the flattened sequence: which direction, which group, which neighbour.
struct FlatEntry {
    side: Side,
    edge: String,
    tier: String,
    item: TraceItem,
}

/// The flattened sequence's two halves, in the order the sequence is defined by `in` then `out`.
const SIDES: [Side; 2] = [Side::In, Side::Out];

/// Compact-JSON bytes of a value: the same measurement `emit` takes, taken here because sizing a page
/// needs a number *before* there is an envelope to hand it.
fn serialised_bytes<T: Serialize + ?Sized>(value: &T) -> Result<u64, AppError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len() as u64)
        .map_err(|error| {
            AppError::internal(format!("a trace payload could not be measured: {error}"))
        })
}

/// The payload as the envelope's `serde_json::Value`. The three statuses have three payload shapes and
/// one envelope type, so each branch hands over the serialised form of its own struct.
fn payload_value<T: Serialize>(payload: &T) -> Result<Value, AppError> {
    serde_json::to_value(payload).map_err(|error| {
        AppError::internal(format!("the trace payload is not serialisable: {error}"))
    })
}

/// The fixed part of [`TraceItem`]'s serialised form, measured once from a template with empty fields
/// and one-digit numbers. The template is cheaper to read than a hand-counted constant is to verify,
/// and it is the field order `to_item` writes.
struct Sizing {
    template: u64,
}

impl Sizing {
    fn new() -> Result<Self, AppError> {
        let empty = TraceItem {
            id: 0,
            qn: String::new(),
            kind: String::new(),
            file: String::new(),
            line: 0,
            hop: 0,
            external: None,
            is_test: None,
        };
        Ok(Self {
            template: serialised_bytes(&empty)?,
        })
    }

    /// What one row adds to its group: its own field sizes plus the punctuation [`to_item`] writes.
    ///
    /// Measured arithmetically rather than serialised, because sizing a dense page walks the sequence
    /// row by row and serialising every row to learn its length costs more than the walk it is sizing.
    /// [`ITEM_SLACK_BYTES`] covers what a string length cannot see, and the estimate is only a starting
    /// point: `emit` measures the page it is handed.
    fn item_bytes(&self, item: &TraceItem) -> u64 {
        self.template + digit_count(item.id) - 1 + digit_count(item.line) - 1
            + digit_count(i64::from(item.hop))
            - 1
            + item.qn.len() as u64
            + item.kind.len() as u64
            + item.file.len() as u64
            + if item.external == Some(true) {
                EXTERNAL_FLAG_BYTES
            } else {
                0
            }
            + if item.is_test == Some(true) {
                IS_TEST_FLAG_BYTES
            } else {
                0
            }
            + ITEM_SLACK_BYTES
    }
}

/// Digits of a non-negative integer, without allocating a string for the common widths.
fn digit_count(value: i64) -> u64 {
    match value {
        ..10 => 1,
        10..100 => 2,
        100..1_000 => 3,
        1_000..10_000 => 4,
        10_000..1_000_000 => 5,
        _ => value.to_string().len() as u64,
    }
}

/// A group header with no rows: `{"edge":…,"tier":…,"items":[]}`. Its `[]` is charged here, its rows
/// are charged per row, and the separator before a later group is charged with it (hence the `+ 1`).
#[derive(Serialize)]
struct GroupTemplate<'a> {
    edge: &'a str,
    tier: &'a str,
    items: [u8; 0],
}

fn group_bytes(edge: &str, tier: &str) -> Result<u64, AppError> {
    Ok(serialised_bytes(&GroupTemplate {
        edge,
        tier,
        items: [],
    })? + 1)
}

/// The longest prefix of the remaining neighbour sequence that fits `allowance` bytes, materialised in
/// the same forward pass that measures it: `in` groups in the fixed order, then `out`.
///
/// Two calls with the same arguments and the same generation walk the same sequence in the same
/// order, which is what makes `position` meaningful as an index into it (`DATABASE.md` §8); this pass
/// only picks where the page ends. It stops at the first row that does not fit, so a symbol with
/// 3 000 neighbours costs the page it serves and not the 3 000 rows it does not.
///
/// The number is an estimate, not the sizing decision: `emit` measures the envelope it is handed and
/// cuts it if the estimate was optimistic, and the caller re-sizes from what survived.
fn take_page(
    grouped: &GroupedNeighbors,
    start: u64,
    allowance: u64,
    sizing: &Sizing,
) -> Result<Vec<FlatEntry>, AppError> {
    let mut entries: Vec<FlatEntry> = Vec::new();
    let mut used: u64 = 0;
    let mut offset: u64 = 0;
    let mut run: Option<(Side, String, String)> = None;

    for side in SIDES {
        let groups_of_side = match side {
            Side::In => &grouped.inbound,
            Side::Out => &grouped.outbound,
        };
        for group in ordered_groups(groups_of_side) {
            let items = &group.items;
            // Rows of this group that belong to earlier pages are skipped, not counted.
            let first = start.saturating_sub(offset).min(items.len() as u64) as usize;
            offset += items.len() as u64;
            if first >= items.len() {
                continue;
            }
            let key = (side, group.kind.clone(), group.tier.clone());
            for neighbour in &items[first..] {
                let item = to_item(neighbour);
                let mut cost = sizing.item_bytes(&item);
                if !entries.is_empty() {
                    // The comma before this row.
                    cost += 1;
                }
                if run.as_ref() != Some(&key) {
                    // The group wrapper when the page opens a new group.
                    cost += group_bytes(&group.kind, &group.tier)?;
                }
                if used.saturating_add(cost) > allowance {
                    return Ok(entries);
                }
                used += cost;
                run = Some(key.clone());
                entries.push(FlatEntry {
                    side,
                    edge: group.kind.clone(),
                    tier: group.tier.clone(),
                    item,
                });
            }
        }
    }
    Ok(entries)
}

/// Groups rebuilt from a page, keeping the order the entries arrived in.
fn groups_of(entries: &[FlatEntry], side: Side) -> Vec<TraceGroup> {
    let mut groups: Vec<TraceGroup> = Vec::new();
    for entry in entries.iter().filter(|entry| entry.side == side) {
        match groups
            .iter_mut()
            .find(|group| group.edge == entry.edge && group.tier == entry.tier)
        {
            Some(group) => group.items.push(entry.item.clone()),
            None => groups.push(TraceGroup {
                edge: entry.edge.clone(),
                tier: entry.tier.clone(),
                items: vec![entry.item.clone()],
            }),
        }
    }
    groups
}

fn key_of(side: Side, edge: &str, tier: &str, id: i64) -> String {
    format!("{}\u{0}{edge}\u{0}{tier}\u{0}{id}", side.as_str())
}

/// How many entries of a page `emit` still carried, counted as a prefix.
///
/// `emit` pops the last row of the largest list, which can be a whole group rather than the tail of
/// this sequence, so what it delivered is not necessarily a prefix. The page is cut back to the prefix
/// it did deliver and the remainder is served by the next page; that is what keeps pages
/// duplicate-free when `emit` disagrees with the page's own estimate.
fn leading_survivors(payload: Option<&Value>, page: &[FlatEntry]) -> usize {
    let Some(payload) = payload else {
        return 0;
    };
    let mut present: HashSet<String> = HashSet::new();
    for (side, key) in [(Side::In, "in"), (Side::Out, "out")] {
        let Some(groups) = payload.get(key).and_then(Value::as_array) else {
            continue;
        };
        for group in groups {
            let edge = group.get("edge").and_then(Value::as_str).unwrap_or("");
            let tier = group.get("tier").and_then(Value::as_str).unwrap_or("");
            let Some(items) = group.get("items").and_then(Value::as_array) else {
                continue;
            };
            for item in items {
                if let Some(id) = item.get("id").and_then(Value::as_i64) {
                    present.insert(key_of(side, edge, tier, id));
                }
            }
        }
    }
    let mut survived = 0_usize;
    for entry in page {
        if !present.contains(&key_of(entry.side, &entry.edge, &entry.tier, entry.item.id)) {
            break;
        }
        survived += 1;
    }
    survived
}

fn unresolved_of(rows: &[UnresolvedRef]) -> Vec<TraceUnresolved> {
    rows.iter()
        .map(|row| TraceUnresolved {
            file: row.file.clone(),
            line: row.line,
            name: row.raw_target.clone(),
            bucket: bucket_of(row).as_str().to_owned(),
            evidence: row
                .evidence
                .clone()
                .or_else(|| row.evidence_kind.clone())
                .unwrap_or_else(|| NO_EVIDENCE.to_owned()),
            tier: UNRESOLVED_TIER,
        })
        .collect()
}

/// The census of the rows `assemble` already returned, never a second query: `total` is that scope's
/// own coverage count, so `unresolved_census.total` and `coverage.unresolved_edges` cannot disagree.
fn census_of(total: u64, rows: &[UnresolvedRef]) -> TraceCensus {
    let census = bucket_census(rows);
    let mut buckets = Map::new();
    for bucket in UnresolvedBucket::ALL {
        buckets.insert(
            bucket.as_str().to_owned(),
            Value::from(census.count(bucket)),
        );
    }
    TraceCensus {
        total,
        material: census.material as u64,
        buckets,
    }
}

/// The longest prefix of `rows` whose serialised form fits `max_bytes`, separators included. The tail
/// is what goes: these lists are read from the top, and `coverage` keeps the exact counts, so a
/// shortened list still reads as "some of N".
fn cap_rows<T: Serialize + Clone>(rows: &[T], max_bytes: u64) -> Result<Vec<T>, AppError> {
    let mut kept: Vec<T> = Vec::new();
    let mut used: u64 = 0;
    for row in rows {
        let size = serialised_bytes(row)? + if kept.is_empty() { 0 } else { 1 };
        if used.saturating_add(size) > max_bytes {
            break;
        }
        used += size;
        kept.push(row.clone());
    }
    Ok(kept)
}

/// The byte allowance a budget buys, computed the way `budget.rs` computes it so a page is sized
/// against the number `emit` will apply. A budget above the protocol ceiling is still `emit`'s
/// refusal: sizing it as if it were legal is what lets `emit` answer exactly once.
fn allowance_of(budget_tokens: u64) -> u64 {
    budget_tokens.saturating_mul(BYTES_PER_TOKEN)
}

/// How many neighbours the walk reached in each direction. Counted over the groups rather than over a
/// materialised sequence: the totals and the page arithmetic need the counts, not the rows.
#[derive(Clone, Copy, Debug)]
struct Counts {
    inbound: usize,
    outbound: usize,
}

fn counts_of(grouped: &GroupedNeighbors) -> Counts {
    Counts {
        inbound: grouped.inbound.iter().map(|group| group.items.len()).sum(),
        outbound: grouped.outbound.iter().map(|group| group.items.len()).sum(),
    }
}

/// Per-list counts plus the relation the scope supports. A direction that was not walked contributes
/// no key at all: `callers_total: 0` on a `direction: "out"` call reads as "nothing calls this", which
/// is the one claim this tool must never make by accident.
fn totals_of(counts: &Counts, direction: Direction, relation: TotalRelation) -> Totals {
    let mut totals = Totals::new(relation);
    if direction != Direction::Out {
        totals = totals.with_total("callers", Some(counts.inbound as u64));
    }
    if direction != Direction::In {
        totals = totals.with_total("callees", Some(counts.outbound as u64));
    }
    totals
}

/// The counts line every trace summary opens with. Extracted because a page that could not be served
/// still has to say what it is a page *of*.
fn head_of(node: &NodeRow, counts: &Counts, direction: Direction) -> String {
    match direction {
        Direction::In => format!("{}: {} callers", node.name, counts.inbound),
        Direction::Out => format!("{}: {} callees", node.name, counts.outbound),
        Direction::All => format!(
            "{}: {} callers, {} callees",
            node.name, counts.inbound, counts.outbound
        ),
    }
}

/// The unresolved evidence as a summary must state it: the scope's count, the bucket it was filtered
/// to, and how many rows that filter hid.
struct EvidenceFacts {
    total: u64,
    bucket: Option<UnresolvedBucket>,
    hidden: usize,
}

/// The evidence sentence every trace summary ends with. A shortened list may never read as a complete
/// one: when the `unresolved` filter is active the line names the bucket and the number of rows it
/// removed, so `3 unresolved call sites` on a filtered answer is not taken for the scope's total.
fn evidence_sentence(node: &NodeRow, facts: &EvidenceFacts) -> String {
    if facts.total == 0 {
        return ".".to_owned();
    }
    let plural = if facts.total == 1 { "" } else { "s" };
    let count = format!(
        "; {} unresolved call site{plural} in {}",
        facts.total, node.path
    );
    match facts.bucket {
        None => format!("{count}."),
        Some(bucket) => format!(
            "{count}, {} filtered out (unresolved: {}).",
            facts.hidden,
            bucket.as_str()
        ),
    }
}

fn summary_of(
    node: &NodeRow,
    counts: &Counts,
    direction: Direction,
    page: Option<(i64, i64)>,
    evidence: &EvidenceFacts,
) -> String {
    let head = head_of(node, counts, direction);
    let paged = page.map_or_else(String::new, |(shown, available)| {
        format!(" ({shown} of {available} neighbours shown)")
    });
    format!("{head}{paged}{}", evidence_sentence(node, evidence))
}

/// The generation as reported to a client, with the node's own file deciding freshness.
/// `sangkar_domain::coverage` treats a file written by another publish as a stale *answer* even when
/// the repository has not moved, so `stale` here means exactly that, and `stale_checked_at` records
/// that a check happened at all.
fn read_status_for(
    ctx: &ToolContext,
    indexed_at: &str,
    generation: &str,
    node: &NodeRow,
) -> Result<ReadStatus, AppError> {
    let file = ctx.reader.file_by_path(&node.path)?;
    let stale = file
        .as_ref()
        .is_none_or(|row| row.indexed_generation != generation);
    let mut status = ReadStatus {
        generation: generation.to_owned(),
        indexed_at: indexed_at.to_owned(),
        stale,
        stale_reason: None,
        stale_checked_at: Some((ctx.now)()),
        decisive: None,
    };
    if stale {
        status.stale_reason = Some(match &file {
            None => format!("\"{}\" has no indexed file row", node.path),
            Some(row) => format!(
                "\"{}\" was last written by generation {}",
                node.path, row.indexed_generation
            ),
        });
    }
    Ok(status)
}

/// The read status of an answer with no node behind it. There is no file scope to compare, so no
/// `stale_checked_at` is claimed: "we did not look" and "we looked and it is current" are different
/// statements, and only `coverage.complete: false` carries the caveat here.
fn unscoped_read_status(meta: &GraphMeta, indexed_at: &str) -> ReadStatus {
    ReadStatus {
        generation: meta.generation.clone().unwrap_or_default(),
        indexed_at: indexed_at.to_owned(),
        stale: false,
        stale_reason: None,
        stale_checked_at: None,
        decisive: None,
    }
}

/// No scope was checked, so nothing may be claimed: `complete: false` is the whole answer.
fn unscoped_coverage() -> Coverage {
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

/// A truncation that could not advance the cursor is terminal. The candidate and near-miss lists are
/// capped upstream, so a cut here means the budget is below the envelope's floor: minting a position
/// at the head of that list would re-serve the same candidates forever.
fn no_repeat(mut envelope: Envelope<Value>) -> Envelope<Value> {
    if envelope.truncated && envelope.next.is_some() {
        envelope.next = None;
    }
    envelope
}

/// The working set of one resolved trace: the node, the walk, the evidence and the cursor identity, so
/// the page arithmetic reads as arithmetic rather than as a parameter list.
struct Traced<'a> {
    ctx: &'a ToolContext,
    query: String,
    node: NodeRow,
    step: ResolutionStep,
    why: String,
    direction: Direction,
    budget_tokens: u64,
    generation: String,
    args_hash: String,
    start: u64,
    read_status: ReadStatus,
    coverage: Coverage,
    totals: Totals,
    census: TraceCensus,
    unresolved: Vec<TraceUnresolved>,
    not_indexed: Vec<NotIndexedEntry>,
    evidence: EvidenceFacts,
    grouped: GroupedNeighbors,
    counts: Counts,
    /// Neighbours still to serve once the rows earlier pages delivered are discounted.
    remaining: i64,
}

impl Traced<'_> {
    /// The payload of one page: the resolution block, the node, the served rows and the evidence.
    fn payload_of(&self, entries: &[FlatEntry]) -> TracePayload {
        TracePayload {
            resolution: TraceResolution {
                query: self.query.clone(),
                matched: self.node.qn.clone(),
                kind: self.node.kind.clone(),
                how: self.why.clone(),
                confidence: confidence_of(self.step),
            },
            node: TraceNode {
                id: self.node.id,
                qn: self.node.qn.clone(),
                kind: self.node.kind.clone(),
                file: self.node.path.clone(),
                lines: [self.node.start_line, self.node.end_line],
                signature: self.node.signature.clone(),
                is_test: self.node.is_test,
            },
            inbound: groups_of(entries, Side::In),
            outbound: groups_of(entries, Side::Out),
            unresolved: self.unresolved.clone(),
            unresolved_census: self.census.clone(),
            not_indexed: self.not_indexed.clone(),
        }
    }

    fn sign(&self, position: u64) -> String {
        self.ctx.cursor.sign(&CursorPayload {
            generation: self.generation.clone(),
            tool: NAME.to_owned(),
            args_hash: self.args_hash.clone(),
            position,
        })
    }

    /// The minter `emit` is handed. `dropped` is deliberately not the resume position: `emit` counts the
    /// rows it removed, and a removed row can be a whole group from the middle of the sequence rather
    /// than the tail, so only the page boundary computed here is an offset a resumed call may trust. The
    /// count is logged instead, which is what makes a surprising truncation diagnosable.
    fn mint(&self, position: u64) -> impl FnMut(usize) -> String + '_ {
        move |dropped| {
            self.ctx.log.debug(
                "trace page truncated",
                &[
                    ("dropped", LogField::Int(dropped as i64)),
                    ("resume_at", LogField::Int(position as i64)),
                ],
            );
            self.sign(position)
        }
    }

    /// What the envelope costs before a single neighbour is served. Measured, not guessed: the node, the
    /// resolution block, the evidence and the continuation token all have to fit first, and the summary
    /// is taken at its longest (every neighbour shown) so its digits cannot make the page overshoot.
    fn floor(&self) -> Result<u64, AppError> {
        let spec = EnvelopeSpec::new(
            summary_of(
                &self.node,
                &self.counts,
                self.direction,
                Some((self.remaining, self.remaining)),
                &self.evidence,
            ),
            self.read_status.clone(),
            self.coverage.clone(),
        )
        .with_totals(self.totals.clone())
        .with_next_tools(NEXT_TOOLS)
        .with_truncation(
            TruncationReason::Budget,
            Some("x".repeat(CURSOR_RESERVE_CHARS)),
        );
        serialised_bytes(&ok(payload_value(&self.payload_of(&[]))?, spec))
    }

    /// One page: the rows served, the summary that says how many of how many, and the token that
    /// continues past them.
    fn page_envelope(
        &self,
        entries: &[FlatEntry],
        truncated: bool,
    ) -> Result<Envelope<Value>, AppError> {
        let end = self.start + entries.len() as u64;
        let page = truncated.then_some((entries.len() as i64, self.remaining));
        let mut spec = EnvelopeSpec::new(
            summary_of(
                &self.node,
                &self.counts,
                self.direction,
                page,
                &self.evidence,
            ),
            self.read_status.clone(),
            self.coverage.clone(),
        )
        .with_totals(self.totals.clone())
        .with_next_tools(NEXT_TOOLS);
        if truncated {
            spec = spec.with_truncation(TruncationReason::Budget, Some(self.sign(end)));
        }
        emit(
            ok(payload_value(&self.payload_of(entries))?, spec),
            self.budget_tokens,
            self.mint(end),
        )
    }

    /// A page that fits no neighbour at all: the terminal answer to a budget below one row.
    ///
    /// It is the *only* zero-row page this tool serves. A zero-row page that carries a continuation
    /// token would hand the client the position it just asked from, and a client that follows `next`
    /// would read the same envelope forever and never reach a single caller; so the page states the
    /// floor it hit and points at the cheaper query instead of minting one.
    ///
    /// The evidence lists ride along when they fit (`emit` measures), because "there are 94 unbound
    /// call sites here" is still true and still useful even when no neighbour row can be served.
    fn terminal(&self) -> Result<Envelope<Value>, AppError> {
        let gap = if self.remaining > 0 {
            format!(
                "a single neighbour does not fit budget {} tokens",
                self.budget_tokens
            )
        } else {
            format!(
                "the remaining page does not fit budget {} tokens",
                self.budget_tokens
            )
        };
        let summary = format!(
            "{}; {gap}, re-run trace with depth: 1{}",
            head_of(&self.node, &self.counts, self.direction),
            evidence_sentence(&self.node, &self.evidence)
        );
        let spec = EnvelopeSpec::new(summary, self.read_status.clone(), self.coverage.clone())
            .with_totals(self.totals.clone())
            .with_next_tools(NEXT_TOOLS_NARROWER)
            .with_truncation(TruncationReason::Budget, None);
        let mut served = emit(
            ok(payload_value(&self.payload_of(&[]))?, spec),
            self.budget_tokens,
            // A terminal page never carries a token: the cut is already at the envelope's floor, so a
            // minted position could only repeat the one this call was given.
            |_dropped| String::new(),
        )?;
        served.next = None;
        served.next_tools = NEXT_TOOLS_NARROWER
            .iter()
            .map(|tool| (*tool).to_owned())
            .collect();
        Ok(served)
    }

    /// The page: the longest prefix of the remaining sequence that fits what is left of the allowance,
    /// then `emit`'s own verdict read back out of what survived.
    fn run(&self) -> Result<Envelope<Value>, AppError> {
        let sizing = Sizing::new()?;
        let allowance = allowance_of(self.budget_tokens);
        let page_allowance = allowance
            .saturating_sub(self.floor()?)
            .saturating_sub(SKELETON_SLACK_BYTES);
        let mut entries = take_page(&self.grouped, self.start, page_allowance, &sizing)?;
        if entries.is_empty() && self.remaining > 0 {
            return self.terminal();
        }

        let mut envelope = self.page_envelope(&entries, (entries.len() as i64) < self.remaining)?;

        // `emit` measures the page it was handed and is the only sizing decision; if it cuts the page's
        // tail (the estimate was optimistic), the page shrinks to the prefix `emit` kept. That is what
        // keeps every page a contiguous slice of the sequence: no duplicates, and no neighbour skipped
        // between one page and the next. A page that cannot advance is terminal rather than a loop.
        let mut pass = 1_usize;
        while envelope.truncated && !entries.is_empty() {
            let survived = leading_survivors(envelope.payload.as_ref(), &entries);
            if survived >= entries.len() {
                // Only the evidence lists were cut; every neighbour of this page is here.
                break;
            }
            if survived == 0 || pass >= MAX_SIZING_PASSES {
                return self.terminal();
            }
            entries.truncate(survived);
            envelope = self.page_envelope(&entries, true)?;
            pass += 1;
        }

        // The progress guarantee, stated once for every path above: a truncated page never hands back a
        // position at or before the one it was given. Zero rows served means nothing can advance.
        if envelope.truncated && envelope.next.is_some() && entries.is_empty() {
            return self.terminal();
        }
        Ok(envelope)
    }
}

/// A resolved query: the walk, the page and the envelope.
///
/// The evidence allowance is split before anything is measured: `not_indexed` gets an eighth of the
/// byte allowance (half of the evidence share, which is a quarter), the unresolved rows the remainder.
/// Both counts stay exact in `coverage`, so a shortened list is still readable as "some of N".
fn traced(
    ctx: &ToolContext,
    args: &TraceArgs,
    resolution: Resolution,
    session: &Session,
) -> Result<Envelope<Value>, AppError> {
    let Resolution::Ok {
        step, node, why, ..
    } = resolution
    else {
        // `handle` only routes an `Ok` resolution here; the other two have their own branches.
        return Err(AppError::internal(
            "a trace was assembled from a resolution that is not an answer",
        ));
    };
    let direction = args.direction_or_all();
    let budget_tokens = args.budget_or_default();
    let read_status = read_status_for(ctx, &session.indexed_at, &session.generation, &node)?;
    let assembly = assemble(
        &coverage_source(ctx.reader.as_ref()),
        &CoverageScope {
            paths: Some(vec![node.path.clone()]),
            generation: session.generation.clone(),
            stale: read_status.stale,
            file_ids: Some(vec![node.file_id]),
            include_tests: false,
        },
    )?;
    let grouped = expand(
        &ReaderSources(ctx.reader.as_ref()),
        node.id,
        direction,
        args.depth(),
        // `kinds` narrows resolution and nothing here: `expand` filters edge kinds, and a node-kind
        // filter over neighbours would drop callers of the wrong kind rather than the caller.
        None,
    )?;
    let counts = counts_of(&grouped);
    let totals = totals_of(&counts, direction, assembly.relation);
    let served = i64::try_from(session.start).unwrap_or(i64::MAX);
    let remaining = counts.inbound as i64 + counts.outbound as i64 - served;

    let evidence_allowance = allowance_of(budget_tokens) / EVIDENCE_SHARE;
    let not_indexed = cap_rows(&assembly.coverage.not_indexed, evidence_allowance / 2)?;
    let census = census_of(assembly.coverage.unresolved_edges, &assembly.unresolved);
    let unresolved_all = unresolved_of(&assembly.unresolved);
    // The filter narrows what is listed, never what was counted: the census above and `coverage` both
    // describe the scope, so a bucket answer says "some of N" and the summary says how many it hid.
    let listed: Vec<TraceUnresolved> = match args.unresolved {
        None => unresolved_all.clone(),
        Some(bucket) => unresolved_all
            .iter()
            .filter(|row| row.bucket == bucket.as_str())
            .cloned()
            .collect(),
    };
    let evidence = EvidenceFacts {
        total: census.total,
        bucket: args.unresolved,
        hidden: unresolved_all.len() - listed.len(),
    };
    let unresolved = cap_rows(
        &listed,
        evidence_allowance.saturating_sub(serialised_bytes(&not_indexed)?),
    )?;
    let coverage = Coverage {
        not_indexed: not_indexed.clone(),
        ..assembly.coverage
    };

    Traced {
        ctx,
        query: args.query.clone(),
        node,
        step,
        why,
        direction,
        budget_tokens,
        generation: session.generation.clone(),
        args_hash: session.args_hash.clone(),
        start: session.start,
        read_status,
        coverage,
        totals,
        census,
        unresolved,
        not_indexed,
        evidence,
        grouped,
        counts,
        remaining: remaining.max(0),
    }
    .run()
}

/// The narrowing `resolve` applies. Each half is omitted when absent, so a filter that was not passed
/// is not the same value as an empty one for `path` (an empty prefix is every path).
fn resolve_options_of(args: &TraceArgs) -> ResolveOptions {
    ResolveOptions {
        kinds: args.kinds.clone(),
        path: args.path.clone(),
    }
}

/// Not `not_found`: an empty database is no evidence that the symbol does not exist, and answering
/// `not_found` would let the agent tell the user it does not. `fail` with the contract's
/// `bootstrapping` code is the closed-union way to say "there is no graph to answer from yet".
fn unpublished(query: &str) -> Envelope<Value> {
    let error = AppError::new(
        ErrorCode::BOOTSTRAPPING,
        "the graph has no published generation yet",
    )
    .with_retryable(true)
    .with_retry_after_ms(UNPUBLISHED_RETRY_MS)
    .with_hint("index the repository, then call trace again");
    fail(
        error,
        EnvelopeSpec::new(
            format!("trace \"{query}\": no published generation"),
            ReadStatus {
                stale: true,
                ..ReadStatus::new(String::new(), String::new())
            },
            unscoped_coverage(),
        ),
    )
}

/// A partial page is never served: the refusal carries the contract's code and its hint, and the client
/// re-runs the original call instead of resuming into a page it cannot trust.
fn cursor_failure(
    refusal: CursorRefusal,
    meta: &GraphMeta,
    indexed_at: &str,
    query: &str,
) -> Envelope<Value> {
    let error = AppError::new(
        refusal.code,
        format!("cannot resume this trace page: {}", refusal.code),
    )
    .with_hint(refusal.hint);
    fail(
        error,
        EnvelopeSpec::new(
            format!("trace \"{query}\": the cursor was refused, re-run the original call"),
            unscoped_read_status(meta, indexed_at),
            unscoped_coverage(),
        ),
    )
}

/// The envelope of a read that failed: the error's own code and message, on the unscoped skeleton.
/// It is deliberately not `not_found` — a failed read is not an absence claim.
fn read_failure(error: AppError, query: &str) -> Envelope<Value> {
    fail(
        error,
        EnvelopeSpec::new(
            format!("trace \"{query}\": the answer could not be read"),
            ReadStatus::new(String::new(), String::new()),
            unscoped_coverage(),
        ),
    )
}

/// The identity a resume has to match: the generation being served and this call's argument digest.
struct Session {
    generation: String,
    args_hash: String,
    indexed_at: String,
    meta: GraphMeta,
    start: u64,
}

/// A page token for `position`. The one signer used by every branch: a tool that signed in two
/// places would be a tool whose resume rules could drift.
fn sign_at(ctx: &ToolContext, session: &Session, position: u64) -> String {
    ctx.cursor.sign(&CursorPayload {
        generation: session.generation.clone(),
        tool: NAME.to_owned(),
        args_hash: session.args_hash.clone(),
        position,
    })
}

fn answer(ctx: &ToolContext, args: &TraceArgs) -> Result<Envelope<Value>, AppError> {
    let meta = ctx.reader.meta_for()?;
    let Some(generation) = meta.generation.clone() else {
        return Ok(unpublished(&args.query));
    };
    // `GraphMeta` has no `indexed_at`; the served generation's `published_at` is the reader's only
    // published timestamp (see the module note).
    let indexed_at = ctx
        .reader
        .latest_generation()?
        .map_or_else(String::new, |row| row.published_at);
    let args_hash = args_hash_of(args);
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
                return Ok(cursor_failure(refusal, &meta, &indexed_at, &args.query));
            }
        }
    }
    let session = Session {
        generation,
        args_hash,
        indexed_at,
        meta,
        start,
    };

    let source = ReaderSources(ctx.reader.as_ref());
    let resolution = resolve(&source, &args.query, &resolve_options_of(args))?;
    match resolution {
        Resolution::Ambiguous {
            candidates,
            candidates_total,
            candidates_shown,
            step: _,
        } => {
            let direction = args.direction_or_all();
            // One number per name: the summary says how wide the tie was, `candidates_shown` how much of
            // it this payload carries, and the two cannot disagree because both come from the resolution.
            let totals = Totals::new(TotalRelation::Gte)
                .with_total("candidates", Some(candidates_total as u64))
                .with_candidates_shown(candidates_shown as u64);
            let spec = EnvelopeSpec::new(
                format!(
                    "{}: {} candidates matched, none preferred",
                    args.query, candidates_total
                ),
                unscoped_read_status(&session.meta, &session.indexed_at),
                unscoped_coverage(),
            )
            .with_totals(totals)
            .with_next_tools(NEXT_TOOLS_AFTER_MISS);
            // The candidate list is capped at `CANDIDATE_CAP` by the resolution contract, so `emit` has
            // nothing to cut unless the budget is below the envelope's own floor; a cut then cannot
            // advance the cursor, and re-serving the same candidates is a loop rather than a page.
            let payload = TraceCandidates {
                candidates: with_next_call(
                    &candidates[..candidates.len().min(CANDIDATE_CAP)],
                    direction,
                ),
                candidates_total,
                candidates_shown,
            };
            let envelope = ambiguous(payload_value(&payload)?, spec);
            Ok(no_repeat(emit(
                envelope,
                args.budget_or_default(),
                |_dropped| sign_at(ctx, &session, session.start),
            )?))
        }
        Resolution::NotFound { near_misses } => {
            // A miss stays a miss however narrow the filter: the near misses are the ones `resolve` kept
            // under the same prefix, so a narrowed answer never points outside it.
            let spec = EnvelopeSpec::new(
                format!(
                    "{}: no symbol matched, {} near misses",
                    args.query,
                    near_misses.len()
                ),
                unscoped_read_status(&session.meta, &session.indexed_at),
                unscoped_coverage(),
            )
            .with_totals(
                Totals::new(TotalRelation::Gte)
                    .with_total("near_misses", Some(near_misses.len() as u64)),
            )
            .with_next_tools(NEXT_TOOLS_AFTER_MISS);
            let payload = TraceNearMisses {
                near_misses: with_next_call(
                    &near_misses[..near_misses.len().min(CANDIDATE_CAP)],
                    args.direction_or_all(),
                ),
            };
            let envelope = not_found(payload_value(&payload)?, spec);
            Ok(no_repeat(emit(
                envelope,
                args.budget_or_default(),
                |_dropped| sign_at(ctx, &session, session.start),
            )?))
        }
        resolution @ Resolution::Ok { .. } => traced(ctx, args, resolution, &session),
    }
}

/// One free-text query in, one envelope out (`TECH-SPEC.md` §6: handlers return envelopes, never
/// throw, never read the environment and never open a database).
///
/// A store read that fails is an error envelope with the error's own code, never `not_found`: a failed
/// read is not an absence claim.
#[must_use]
pub fn handle(ctx: &ToolContext, args: &TraceArgs) -> Envelope<Value> {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => read_failure(error, &args.query),
    }
}
