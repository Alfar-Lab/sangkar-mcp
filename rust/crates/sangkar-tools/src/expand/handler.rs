//! `expand`: one target in, one detail view out (`TOOLS-SPEC.md` §7, `DESIGN.md` §7).
//!
//! Ported from `src/tools/expand/handler.ts`. The handler is the tool's whole policy and does no
//! work of its own: resolution is [`sangkar_domain::resolve`], the one-hop walk is
//! [`sangkar_domain::neighbors`], the absence arithmetic is [`sangkar_domain::coverage`], sizing is
//! [`sangkar_core::budget::emit`] and the envelope comes from [`sangkar_core::envelope`]. What is
//! decided here is:
//!
//! - **the view is disclosed.** `payload.what` is the mode that answered and `payload.why` says
//!   whether the node's kind inferred it or the caller asked for it. Inference is never silent,
//!   which is the whole reason the other six tools can ship summaries instead of a `detail_level`
//!   knob: a wrong guess is visible in the answer instead of buried in a flag;
//! - **the address is restated in full.** The agent may be expanding a handle it obtained several
//!   turns ago, so `payload.target` repeats `{id, qn, kind, lines}`; a folder has no node behind it,
//!   so those three are `null` and `kind` names the address form instead;
//! - **a bare numeric id is refused, not guessed at.** "1006" is not a symbol name, and a fabricated
//!   match would be the confident answer about the wrong node that resolution exists to prevent. A
//!   folder prefix is an address too, and only `what: "coverage"` answers it;
//! - **`source` is the one mode that reads bytes, and the drift check is its honesty feature.** The
//!   file's SHA-256 is compared with the digest the index recorded *before* any text is returned: a
//!   mismatch means the graph's view of that file is out of date, so the answer is
//!   `source_drift: true` with `content: null` — never lines the agent would quote as current. A
//!   file that has since vanished is the same answer with a different `stale_reason`;
//! - **one window, one place to read it.** The node's own extent is the single effective span:
//!   `range` narrows inside it and is echoed as `payload.window`, and a clamp or an inverted pair is
//!   disclosed in `why` rather than in a second window field. Neither is an error — a line the node
//!   does not occupy is a request this answer cannot honour, not a malformed call;
//! - **`outline` is a different list from `members` on purpose.** That one is the expansion's answer
//!   body with degrees; this one is what a file or class declares, named by `qn` with its signature,
//!   read to pick the next target without opening the file. Both are the reader's own source order
//!   (`ORDER BY n.start_line, n.qn`) and are never re-sorted here;
//! - **a truncated answer is finished, not resumable.** [`PARAMETERS`](crate::expand::manifest::PARAMETERS)
//!   declares no `cursor`, so `emit` is handed a mint that yields the empty token and a cut is
//!   published as `truncated` + `truncation_reason` + `next: null` + a `gte` relation, with the
//!   summary saying "showing N of M". A minted token would be a promise the schema cannot accept
//!   back: the caller raises `budget` or narrows `what` instead;
//! - **a read error is an error envelope, never `not_found`.** `meta_for`, `resolve`, `members`,
//!   `expand`, `assemble` and the coverage queries all return `Result`; a failed read must not be
//!   flattened into "this container has no members", so [`handle`] answers it with [`fail`] and the
//!   error's own code.
//!
//! Four porting decisions are visible in the code:
//!
//! - **the payload is a `serde_json::Value`.** The TypeScript returns a union of seven payload
//!   shapes; Rust has one envelope type per payload type, so each branch serialises its own struct
//!   first ([`payload_value`]) and the envelope is `Envelope<Value>`. That is also what lets a
//!   summary written after `emit` read the lists that survived it;
//! - **the only float on the wire is a candidate's `score`,** rendered by [`Score`] so an integral
//!   value loses serde_json's `.0` and matches `JSON.stringify` byte for byte. No other field here
//!   is a float;
//! - **`read_status.indexed_at` is the served publish's `published_at`.** `sangkar_store`'s
//!   `GraphMeta` carries no timestamp (the phase-1 deviation on the record) and
//!   [`GraphReader::latest_generation`](sangkar_store::read::reader::GraphReader::latest_generation)
//!   is the newest publish;
//! - **the domain traits are bridged by [`ReaderSource`].** `sangkar_domain` implements
//!   `ResolutionSource`/`NeighbourSource` *for* `dyn GraphReader`, but Rust cannot coerce
//!   `&dyn GraphReader` into `&dyn ResolutionSource`, so the wrapper forwards. It is not a second
//!   database adapter: it holds the aggregate and calls its own methods.

use std::path::{Component, Path, PathBuf};

use sangkar_core::budget::{budget_floor_of, emit};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, TotalRelation, Totals,
    ambiguous, bootstrapping, fail, not_found, ok,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::instructions::next_tools_for;
use sangkar_core::paths::normalize_rel;
use sangkar_domain::coverage::{CoverageScope, assemble, coverage_source};
use sangkar_domain::neighbors::{Direction, NeighbourGroup, NeighbourSource, expand};
use sangkar_domain::resolve::{
    Resolution, ResolutionCandidate, ResolutionSource, ResolveOptions, resolve,
};
use sangkar_store::read::context::ToolContext;
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::GraphReader;
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::expand::manifest::{DEFAULT_BUDGET_TOKENS, EXPAND_WHATS, ExpandArgs, NAME, What};

/// Kinds whose detail is what they contain. `file` and `module` are containers in this graph, not
/// callables, so their detail is the symbols declared inside them. The type kinds a grammar may
/// emit one day are listed here because they are containers wherever they appear: inferring nothing
/// for a kind the indexer *can* produce would turn a legitimate node into a `not_found`.
const CONTAINER_KINDS: [&str; 9] = [
    "file",
    "module",
    "class",
    "interface",
    "enum",
    "struct",
    "type",
    "namespace",
    "trait",
];

/// Kinds whose detail is what they call and what calls them.
const CALLABLE_KINDS: [&str; 7] = [
    "function",
    "method",
    "constructor",
    "getter",
    "setter",
    "test",
    "endpoint",
];

/// The generation id `read_status` reports before the first publish. No generation can hold it —
/// `DATABASE.md` §3 formats them as `g-<hex>` — so it names the state instead of inventing an id.
const UNPUBLISHED: &str = "unpublished";

/// How long an unpublished graph asks to be retried in; the first index is minutes, not seconds.
const BOOTSTRAP_RETRY_MS: u64 = 5_000;

/// The summary of the answer an unpublished database gets, byte for byte the TypeScript's.
const UNPUBLISHED_SUMMARY: &str = "the graph has not published a generation yet";

/// The `error.message`/`error.hint` pair of that answer: what to do instead of retrying blindly.
const UNPUBLISHED_MESSAGE: &str = "the index is still being built";
const UNPUBLISHED_HINT: &str = "call status and retry; expand answers from a published generation";

/// The summary a read that never reached a scope of its own carries.
const UNREAD_SUMMARY: &str = "expand: the answer could not be read";

/// The `read_status` verdict of a call that failed before the graph answered.
const UNREAD_ANSWER: &str = "the call failed before the graph answered";

/// The `stale_reason` of an unpublished graph: no generation, so nothing to be current against.
const NO_GENERATION: &str = "no published generation";

/// The three hints a refusal can carry, in the TypeScript's wording.
const HINT_FORMS: &str =
    "pass a qualified name (path::scope.name), a repo-relative path, or a folder prefix";
const HINT_ID: &str =
    "a numeric id is not an address on its own: pass the item's qualified name (path::scope.name)";
const HINT_NOT_INDEXED: &str = "call status to see which paths the index covers";

/// The hint of a tie: restating each candidate's `qn` as a `target` is what turns "5 of 7 shown"
/// into one call, so the tool never picks one of a tie itself.
const HINT_AMBIGUOUS: &str = "pass the qn of the item you meant; expand never picks one of a tie";

/// The hint of a folder asked for a mode only a node can answer.
const HINT_FOLDER: &str = "pass what=coverage for a folder";

/// The summary of a refusal whose floor is above the caller's allowance.
const REFUSED_SUMMARY: &str = "the requested budget is below the floor this answer needs";

/// `range ignored: pass exactly two integers, [from, to]` — the note a malformed window gets.
const RANGE_SHAPE_NOTE: &str = "range ignored: pass exactly two integers, [from, to]";

/// The coverage an answer with no scope can claim: zero files, and therefore nothing complete.
fn no_coverage() -> Coverage {
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

/// The four methods of the two domain seams, forwarded to the aggregate reader.
///
/// `sangkar_domain` implements both traits *for* `dyn GraphReader`, but a trait-object reference
/// cannot be coerced from one trait to another, so the tool needs this bridge. It holds the
/// aggregate — no connection, no SQL — and every method is one call to the reader's own.
struct ReaderSource<'a>(&'a dyn GraphReader);

impl ResolutionSource for ReaderSource<'_> {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        self.0.find_by_qn(qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        self.0.find_by_name(name, kinds)
    }
}

impl NeighbourSource for ReaderSource<'_> {
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

/// A score, serialised so an integral value loses the `.0`.
///
/// `JSON.stringify(1)` is `1`; serde_json's shortest-round-trip writer for `1.0f64` is `1.0`. A
/// candidate's score is the only float this tool puts on the wire, and its values are the
/// resolution's table (`0.9`, `0.85`, `0.8`, …) — none integral today, because the exact-`qn` step
/// produces one hit by construction and therefore never ties. The wrapper keeps that true if the
/// table grows an integral score: the wire stays the TypeScript's, and the value keeps its precision.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Score(f64);

/// `Number.MAX_SAFE_INTEGER`: past it an `f64` cannot represent every integer, so writing one would
/// claim a precision the value does not have.
const MAX_EXACT_INTEGER: f64 = 9_007_199_254_740_991.0;

impl Score {
    /// A score exactly as the resolution computed it.
    #[must_use]
    pub const fn from_f64(value: f64) -> Self {
        Self(value)
    }

    /// The value as an integer, when it renders without a fractional part; `None` for `NaN`,
    /// infinities and anything with a fraction.
    fn integral(self) -> Option<i64> {
        if self.0.is_finite() && self.0.fract() == 0.0 && self.0.abs() <= MAX_EXACT_INTEGER {
            Some(self.0 as i64)
        } else {
            None
        }
    }
}

impl Serialize for Score {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.integral() {
            Some(whole) => serializer.serialize_i64(whole),
            None => serializer.serialize_f64(self.0),
        }
    }
}

/// The address that was expanded, restated in full: `id`, `qn` and `lines` are `None` for a folder,
/// which has no node behind it.
#[derive(Clone, Debug, Serialize)]
struct TargetRef {
    id: Option<i64>,
    qn: Option<String>,
    kind: String,
    lines: Option<[i64; 2]>,
}

impl TargetRef {
    fn of(node: &NodeRow) -> Self {
        Self {
            id: Some(node.id),
            qn: Some(node.qn.clone()),
            kind: node.kind.clone(),
            lines: Some([node.start_line, node.end_line]),
        }
    }

    fn folder() -> Self {
        Self {
            id: None,
            qn: None,
            kind: "folder".to_owned(),
            lines: None,
        }
    }
}

/// The two numbers `metrics_for` publishes per node. `in` is not a Rust identifier, so the field is
/// named for the reader and renamed on the way out.
#[derive(Clone, Copy, Debug, Serialize)]
struct Degree {
    #[serde(rename = "in")]
    inbound: i64,
    out: i64,
}

/// One direct child of the expanded node, with the degrees that make it worth expanding next.
#[derive(Clone, Debug, Serialize)]
struct MemberRow {
    id: i64,
    name: String,
    kind: String,
    lines: [i64; 2],
    /// `None` is "the publish did not measure this node", which is not the same answer as degree 0.
    degree: Option<Degree>,
}

/// One member of an outline: the address, where it sits, and what it takes.
#[derive(Clone, Debug, Serialize)]
struct OutlineMember {
    qn: String,
    kind: String,
    lines: [i64; 2],
    signature: Option<String>,
}

/// One indexed file under the prefix, trimmed to what a coverage answer is read for.
#[derive(Clone, Debug, Serialize)]
struct CoverageFileRow {
    id: i64,
    path: String,
    language: String,
    sha256: String,
    line_count: i64,
    parse_status: String,
    indexed_generation: String,
}

/// One path the indexer saw and did not index, with the reason it says so.
#[derive(Clone, Debug, Serialize)]
struct CoverageSkipRow {
    path: String,
    reason: String,
    detail: Option<String>,
}

/// One numbered source line, so a quoted snippet carries its own position.
#[derive(Clone, Debug, Serialize)]
struct SourceLine {
    line: i64,
    text: String,
}

/// One neighbour as `expand` shows it. The shape is `sangkar_domain::neighbors`' own — `external`
/// is always present because the domain row always has it, unlike `trace`'s trimmed item.
#[derive(Clone, Debug, Serialize)]
struct EdgeItem {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    line: i64,
    hop: u8,
    external: bool,
}

/// All items of one `(edge, tier)` pair; `tier` is shared by construction, hence hoisted here.
#[derive(Clone, Debug, Serialize)]
struct EdgeGroup {
    edge: String,
    tier: String,
    items: Vec<EdgeItem>,
}

/// The follow-up a candidate carries, ready to send back verbatim.
#[derive(Clone, Debug, Serialize)]
struct NextCall {
    tool: &'static str,
    target: String,
}

/// One tied match, plus the call that expands exactly it.
#[derive(Clone, Debug, Serialize)]
struct Candidate {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    lines: [i64; 2],
    score: Score,
    why: String,
    next_call: NextCall,
}

/// `members`: a container's direct children.
#[derive(Clone, Debug, Serialize)]
struct MembersPayload {
    target: TargetRef,
    what: &'static str,
    why: String,
    members: Vec<MemberRow>,
}

/// `edges`: one node's neighbours at one hop, in the grouped shape `trace` ships.
#[derive(Clone, Debug, Serialize)]
struct EdgesPayload {
    target: TargetRef,
    what: &'static str,
    why: String,
    direction: &'static str,
    depth: u8,
    /// `in` is not a Rust identifier; the wire key is the TypeScript's.
    #[serde(rename = "in")]
    inbound: Vec<EdgeGroup>,
    out: Vec<EdgeGroup>,
}

/// `coverage`: both halves of the answer for one prefix.
#[derive(Clone, Debug, Serialize)]
struct CoveragePayload {
    target: TargetRef,
    what: &'static str,
    why: String,
    prefix: String,
    files: Vec<CoverageFileRow>,
    not_indexed: Vec<CoverageSkipRow>,
}

/// `source`: a verified line window, or the drift that stopped one being served.
#[derive(Clone, Debug, Serialize)]
struct SourcePayload {
    target: TargetRef,
    what: &'static str,
    why: String,
    path: String,
    source_drift: bool,
    window: [i64; 2],
    /// `None` only when `source_drift` is true: absence of a list means "empty", so an unreadable
    /// answer is stated as `null` rather than as zero lines.
    content: Option<Vec<SourceLine>>,
}

/// `outline`: a container's table of contents.
#[derive(Clone, Debug, Serialize)]
struct OutlinePayload {
    target: TargetRef,
    what: &'static str,
    why: String,
    path: String,
    members: Vec<OutlineMember>,
}

/// A target that matched more than one node; the envelope carries no answer body beside these.
#[derive(Clone, Debug, Serialize)]
struct AmbiguousPayload {
    target: String,
    candidates: Vec<Candidate>,
    candidates_total: usize,
    candidates_shown: usize,
    hint: &'static str,
}

/// A target nothing answered for, with the forms that would have been accepted.
#[derive(Clone, Debug, Serialize)]
struct MissingPayload {
    target: String,
    why: String,
    /// A `String` rather than a static: one of the four hints quotes the declared enum values.
    hint: String,
}

/// What one scope of one answer learns: the freshness verdict, the coverage it supports, the
/// relation those two allow and the graph's own skipped-path count.
struct Scoped {
    read_status: ReadStatus,
    coverage: Coverage,
    relation: TotalRelation,
    /// The whole graph's skip count (`assemble`'s), which a narrow scope must not understate.
    not_indexed_total: u64,
}

/// The read status of one scope. `stale_checked_at` is stamped only when this call actually
/// compared bytes with the index, so its absence means "not checked by this tool", never "checked
/// and fresh".
fn read_status_of(
    generation: &str,
    indexed_at: &str,
    drift: Option<&str>,
    now: &str,
) -> ReadStatus {
    let mut status = ReadStatus::new(generation, indexed_at);
    if let Some(reason) = drift {
        status.stale = true;
        status.stale_reason = Some(reason.to_owned());
        status.stale_checked_at = Some(now.to_owned());
    }
    status
}

/// The assembly for one scope, built by [`assemble`] so `expand` cannot disagree with the other
/// tools about whether an empty list is a fact.
fn scope_of(
    ctx: &ToolContext,
    generation: &str,
    indexed_at: &str,
    paths: &[String],
    drift: Option<&str>,
    now: &str,
) -> Result<Scoped, AppError> {
    let scope = CoverageScope {
        paths: Some(paths.to_vec()),
        generation: generation.to_owned(),
        stale: drift.is_some(),
        file_ids: None,
        include_tests: false,
    };
    let assembly = assemble(&coverage_source(ctx.reader.as_ref()), &scope)?;
    Ok(Scoped {
        read_status: read_status_of(generation, indexed_at, drift, now),
        coverage: assembly.coverage,
        relation: assembly.relation,
        not_indexed_total: assembly.not_indexed_total,
    })
}

/// The spec a `not_found`/`fail` answer carries when it has no totals of its own: the relation is
/// derived, exactly as the TypeScript's `{...base}` leaves it.
fn base_spec(summary: String, scoped: &Scoped) -> EnvelopeSpec {
    EnvelopeSpec::new(summary, scoped.read_status.clone(), scoped.coverage.clone())
        .with_next_tools(next_tools_for(NAME))
}

/// The spec a scoped `not_found` answer carries: `totals` states the relation its coverage allows.
fn scoped_spec(summary: String, scoped: &Scoped) -> EnvelopeSpec {
    base_spec(summary, scoped).with_totals(Totals::new(scoped.relation))
}

/// The spec an answer carries when it reached a scope of its own: `totals` is the caller's.
fn answer_spec(summary: String, scoped: &Scoped, totals: Totals) -> EnvelopeSpec {
    base_spec(summary, scoped).with_totals(totals)
}

/// The payload a branch serialises, or an `internal` error for a shape JSON cannot hold — no
/// contract type can trigger it, so it is a bug rather than caller input.
fn payload_value<T: Serialize>(payload: &T) -> Result<Value, AppError> {
    serde_json::to_value(payload).map_err(|error| {
        AppError::internal(format!("the expand payload is not serialisable: {error}"))
    })
}

/// A summary written after `emit`, so "showing N of M" knows what was actually delivered.
fn payload_of(envelope: &Envelope<Value>) -> &Value {
    envelope.payload.as_ref().unwrap_or(&Value::Null)
}

/// The length of one payload list; a list `emit` drained reads as zero rows shown.
fn shown_of(payload: &Value, key: &str) -> usize {
    payload[key].as_array().map_or(0, Vec::len)
}

/// One `<list>_total`, which truncation never rewrites: `calls_total: 2` with one row shown still
/// reads "showing 1 of 2".
fn total_of(envelope: &Envelope<Value>, key: &str) -> Option<u64> {
    envelope.totals.get(key).and_then(Value::as_u64)
}

/// What a sentence names the expanded address by.
fn label_of(payload: &Value) -> &str {
    payload["target"]["qn"]
        .as_str()
        .or_else(|| payload["target"]["kind"].as_str())
        .unwrap_or_default()
}

/// `members of X: 3.` / `members of X, showing 2 of 3.`
fn members_summary(envelope: &Envelope<Value>) -> String {
    let payload = payload_of(envelope);
    let shown = shown_of(payload, "members");
    let total = total_of(envelope, "members_total").unwrap_or(shown as u64);
    let head = format!("members of {}", label_of(payload));
    if envelope.truncated {
        format!("{head}, showing {shown} of {total}.")
    } else {
        format!("{head}: {total}.")
    }
}

/// `outline of X: 3.` / `outline of X, showing 2 of 3.`
fn outline_summary(envelope: &Envelope<Value>) -> String {
    let payload = payload_of(envelope);
    let shown = shown_of(payload, "members");
    let total = total_of(envelope, "members_total").unwrap_or(shown as u64);
    let head = format!("outline of {}", label_of(payload));
    if envelope.truncated {
        format!("{head}, showing {shown} of {total}.")
    } else {
        format!("{head}: {total}.")
    }
}

/// The rows of a grouped neighbour list, whatever `emit` left of them.
fn edge_rows_of(side: &Value) -> usize {
    side.as_array().map_or(0, |groups| {
        groups
            .iter()
            .map(|group| group["items"].as_array().map_or(0, Vec::len))
            .sum()
    })
}

/// `neighbours of X at depth 1: 2.` / `neighbours of X at depth 1, showing 1 of 2.`
fn edges_summary(envelope: &Envelope<Value>) -> String {
    let payload = payload_of(envelope);
    let shown = edge_rows_of(&payload["in"]) + edge_rows_of(&payload["out"]);
    let total = total_of(envelope, "edges_total").unwrap_or(shown as u64);
    let depth = payload["depth"].as_u64().unwrap_or_default();
    let head = format!("neighbours of {} at depth {depth}", label_of(payload));
    if envelope.truncated {
        format!("{head}, showing {shown} of {total}.")
    } else {
        format!("{head}: {total}.")
    }
}

/// `coverage for src/x: 1 files, 1 not indexed.`
///
/// The skip count is the *scope's*, not the graph's: the sentence prints beside the list this
/// answer holds, while the graph's own count is `totals.not_indexed_total`.
fn coverage_summary(envelope: &Envelope<Value>) -> String {
    let payload = payload_of(envelope);
    let shown = shown_of(payload, "files");
    let total = total_of(envelope, "files_total").unwrap_or(shown as u64);
    let skipped = envelope.coverage.skipped;
    let prefix = payload["prefix"].as_str().unwrap_or_default();
    let body = if envelope.truncated {
        format!("showing {shown} of {total} files, {skipped} not indexed")
    } else {
        format!("{total} files, {skipped} not indexed")
    };
    format!("coverage for {prefix}: {body}.")
}

/// `lines 5-8 of src/a.ts.`, or the two ways a window answers with no lines in it.
fn source_summary(envelope: &Envelope<Value>) -> String {
    let payload = payload_of(envelope);
    let path = payload["path"].as_str().unwrap_or_default();
    if payload["source_drift"].as_bool().unwrap_or_default() {
        return format!("{path} changed since the index: no source returned.");
    }
    let from = payload["window"][0].as_i64().unwrap_or_default();
    let to = payload["window"][1].as_i64().unwrap_or_default();
    // An inverted range or a window past the end of the file selects nothing. Saying so beats
    // "lines 8-5", and an empty list must read as an answer rather than as a missing one.
    if payload["content"].as_array().is_some_and(Vec::is_empty) {
        return format!("no lines of {path} in {from}-{to}.");
    }
    format!("lines {from}-{to} of {path}.")
}

/// The one place this tool applies a budget, and the summary is rewritten after it ran.
///
/// The mint `emit` is handed returns the empty token: `expand` declares no `cursor`, so a token
/// could never be redeemed, and publishing one would be a promise the schema cannot accept back. A
/// cut is therefore disclosed as `truncated` + `truncation_reason: budget` + `next: null` + a `gte`
/// relation, with the summary saying how much of the total was served — the same shape `trace` uses
/// for a terminal page, and the product statement that a deliberately truncated `expand` answer is
/// finished rather than resumable.
fn sized(
    budget: u64,
    envelope: Envelope<Value>,
    summarize: impl Fn(&Envelope<Value>) -> String,
) -> Envelope<Value> {
    let refused = spec_of(&envelope, REFUSED_SUMMARY.to_owned());
    let mut emitted = match emit(envelope, budget, |_: usize| String::new()) {
        Ok(emitted) => emitted,
        // `emit` refuses a budget above the protocol ceiling with `budget_too_large`; a handler
        // returns, it never throws (`TECH-SPEC.md` §6).
        Err(error) => return fail(error, refused),
    };
    // Below the floor this answer needs, `emit` returns the envelope untouched rather than
    // mislabelling a cut. Serving that would hand the caller a payload many times its allowance (a
    // dense `members` list is hundreds of kilobytes), so the answer is the floor to raise to.
    let floor = budget_floor_of(&emitted);
    if floor > budget {
        return fail(
            AppError::new(
                ErrorCode::BUDGET_TOO_LARGE,
                format!("budget {budget} is below the floor this answer needs"),
            )
            .with_hint(format!(
                "raise the budget to at least {floor} tokens; the default is {DEFAULT_BUDGET_TOKENS}"
            )),
            spec_of(&emitted, REFUSED_SUMMARY.to_owned()),
        );
    }
    if emitted.truncated {
        emitted.next = None;
    }
    emitted.summary = summarize(&emitted);
    emitted
}

/// The same answer's contract fields with a different summary and no continuation: what a refusal
/// keeps of the envelope it refused.
fn spec_of(envelope: &Envelope<Value>, summary: String) -> EnvelopeSpec {
    EnvelopeSpec {
        summary,
        read_status: envelope.read_status.clone(),
        coverage: envelope.coverage.clone(),
        totals: Some(envelope.totals.clone()),
        next_tools: envelope.next_tools.clone(),
        next: None,
        truncated: envelope.truncated,
        truncation_reason: envelope.truncation_reason,
    }
}

/// The digest a continuation would bind to: `sha256:` over the compact JSON of `{target, what}`,
/// with `what: null` for an inferred call (`handler.ts`'s `argsHashOf`).
///
/// Public because it is wire contract, not an internal detail: `DATABASE.md` §8 pins a token's
/// `argsHash` to the call it continues, and `RUST-MIGRATION.md` §8 R4 requires one test per tool
/// that reads its builder. `expand` declares no `cursor`, so nothing signs a token with it today —
/// see [`sized`] — but the identity is the same one a future continuation would have to match, and
/// the key order and the `null` are what make it byte-identical to the TypeScript's.
#[must_use]
pub fn args_hash_of(target: &str, what: Option<What>) -> String {
    let mut canonical = Map::new();
    canonical.insert("target".to_owned(), Value::String(target.to_owned()));
    canonical.insert(
        "what".to_owned(),
        what.map_or(Value::Null, |chosen| {
            Value::String(chosen.as_str().to_owned())
        }),
    );
    let text = Value::Object(canonical).to_string();
    format!("sha256:{}", sha256_hex(text.as_bytes()))
}

/// Lowercase hex SHA-256 of a byte slice.
fn sha256_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// Answers one `expand` call. Every branch returns an envelope built from
/// [`sangkar_core::envelope`] and sized by `emit`; nothing here opens a connection, reads the
/// environment or writes to stdout.
#[must_use]
pub fn handle(ctx: &ToolContext, args: &ExpandArgs) -> Envelope<Value> {
    match answer(ctx, args) {
        Ok(envelope) => envelope,
        Err(error) => read_failure(error, (ctx.now)()),
    }
}

/// The body of [`handle`], in the contract's order: read meta, guard the generation, resolve the
/// target, dispatch on `what`, assemble, then measure.
fn answer(ctx: &ToolContext, args: &ExpandArgs) -> Result<Envelope<Value>, AppError> {
    let target = args.target.trim().to_owned();
    let meta = ctx.reader.meta_for()?;
    let latest = ctx.reader.latest_generation()?;
    // A reader with no published generation has nothing to expand: `bootstrapping` is the honest
    // answer, and cheaper for the agent than an `ok` envelope with an invented generation id.
    let (Some(generation), Some(publish)) = (meta.generation.clone(), latest) else {
        return Ok(unpublished());
    };
    let indexed_at = publish.published_at;
    let now = (ctx.now)();
    let budget = args.budget_or_default();
    let empty = scope_of(ctx, &generation, &indexed_at, &[], None, &now)?;

    if target.is_empty() {
        return missing(
            &MissingPayload {
                target: args.target.clone(),
                why: "the target is empty".to_owned(),
                hint: HINT_FORMS.to_owned(),
            },
            base_spec("no target to expand".to_owned(), &empty),
        );
    }

    // A bare numeric id is an id, which this reader has no lookup for. Refusing it with the `qn`
    // form is honest; reinterpreting "1006" as a name would fabricate a match.
    if target.chars().all(|character| character.is_ascii_digit()) {
        let summary = format!("id {target} is not an address expand can resolve");
        return missing(
            &MissingPayload {
                target,
                why: "a bare numeric id cannot be resolved".to_owned(),
                hint: HINT_ID.to_owned(),
            },
            base_spec(summary, &empty),
        );
    }

    let options = ResolveOptions {
        kinds: None,
        path: args.path.clone(),
    };
    let resolution = resolve(&ReaderSource(ctx.reader.as_ref()), &target, &options)?;

    match resolution {
        Resolution::Ambiguous {
            candidates,
            candidates_total,
            candidates_shown,
            ..
        } => {
            let offered = candidate_rows(&candidates);
            let summary = if candidates_total == candidates_shown {
                format!(
                    "\"{target}\" matches {candidates_total} nodes; pass the qn of the one you meant"
                )
            } else {
                format!(
                    "\"{target}\" matches {candidates_total} nodes, showing {candidates_shown}; \
                     pass the qn of the one you meant"
                )
            };
            let payload = AmbiguousPayload {
                target,
                candidates: offered,
                candidates_total,
                candidates_shown,
                hint: HINT_AMBIGUOUS,
            };
            // `expand`'s ambiguous totals put `total_relation` LAST (`handler.ts:785-789`), unlike
            // the relation-first shape most tools use.
            let totals = Totals::new(TotalRelation::Gte)
                .with_total("candidates", Some(u64_of(candidates_total)))
                .with_candidates_shown(u64_of(candidates_shown))
                .with_relation_last();
            let value = payload_value(&payload)?;
            let spec = base_spec(summary, &empty).with_totals(totals);
            Ok(sized(budget, ambiguous(value, spec), |envelope| {
                envelope.summary.clone()
            }))
        }
        Resolution::Ok { node, .. } => {
            let inferred = infer_what(&node.kind);
            let Some(chosen) = args.what.or(inferred) else {
                let scope = scope_of(
                    ctx,
                    &generation,
                    &indexed_at,
                    std::slice::from_ref(&node.path),
                    None,
                    &now,
                )?;
                let why = format!("kind \"{}\" has no default detail", node.kind);
                return missing(
                    &MissingPayload {
                        target,
                        why: why.clone(),
                        hint: format!("pass what explicitly: {}", hint_what()),
                    },
                    scoped_spec(why, &scope),
                );
            };
            let why = if args.what.is_some() {
                format!("requested: {chosen}")
            } else {
                format!("inferred: {} -> {chosen}", node.kind)
            };
            answer_for(
                ctx,
                &AnswerRequest {
                    generation: &generation,
                    indexed_at: &indexed_at,
                    now: &now,
                    budget,
                    target: &target,
                    node: &node,
                    what: chosen,
                    why,
                    range: args.range.as_deref(),
                },
            )
        }
        Resolution::NotFound { .. } => {
            // No node matched. The one remaining address is a folder prefix, and only coverage
            // answers it.
            let prefix = normalize_rel(&target);
            let rows = if prefix.is_empty() {
                PathCoverage {
                    files: Vec::new(),
                    not_indexed: Vec::new(),
                    indexed: 0,
                    skipped: 0,
                }
            } else {
                ctx.reader.rows_for_paths(std::slice::from_ref(&prefix))?
            };
            let is_folder = !rows.files.is_empty() || !rows.not_indexed.is_empty();
            if is_folder && args.what.is_none_or(|asked| asked == What::Coverage) {
                let why = if args.what.is_some() {
                    "requested: coverage".to_owned()
                } else {
                    "inferred: folder -> coverage".to_owned()
                };
                return coverage_answer(
                    ctx,
                    CoverageRequest {
                        generation: &generation,
                        indexed_at: &indexed_at,
                        now: &now,
                        budget,
                        prefix,
                        why,
                        rows,
                        node: None,
                    },
                );
            }
            let scope = scope_of(
                ctx,
                &generation,
                &indexed_at,
                std::slice::from_ref(&prefix),
                None,
                &now,
            )?;
            let (why, hint, summary) = if is_folder {
                (
                    format!("\"{prefix}\" is a folder, so only coverage can be expanded from it"),
                    HINT_FOLDER.to_owned(),
                    format!("\"{prefix}\" is a folder; pass what=coverage"),
                )
            } else {
                (
                    "nothing in the index matches the target".to_owned(),
                    HINT_FORMS.to_owned(),
                    format!("nothing in the graph matches \"{target}\""),
                )
            };
            missing(
                &MissingPayload { target, why, hint },
                scoped_spec(summary, &scope),
            )
        }
    }
}

/// A refusal with the missing-target payload, serialised into the envelope's `Value`: every answer
/// this tool returns is one type, so a `not_found` is as much a payload as an `ok` is.
fn missing(payload: &MissingPayload, spec: EnvelopeSpec) -> Result<Envelope<Value>, AppError> {
    Ok(not_found(payload_value(payload)?, spec))
}

/// The kind-to-`what` table (`TOOLS-SPEC.md` §7) plus the kinds this indexer declares. `None` means
/// the kind has no defensible default, which the caller reports as `not_found` with a hint rather
/// than guessing a mode the agent did not want.
fn infer_what(kind: &str) -> Option<What> {
    if CONTAINER_KINDS.contains(&kind) {
        return Some(What::Members);
    }
    if CALLABLE_KINDS.contains(&kind) {
        return Some(What::Edges);
    }
    None
}

/// `pass one of: members, edges, source, coverage, outline`, from the manifest's own enum so the
/// hint and the schema cannot drift apart.
fn hint_what() -> String {
    format!("pass one of: {}", EXPAND_WHATS.join(", "))
}

/// A list length as the wire's `u64`: saturating rather than a bare cast, because a length that
/// cannot be represented is a broken count and not a reason to take the server down.
fn u64_of(count: usize) -> u64 {
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// The tie's candidates, each carrying the call that resolves it. `qn` is unique, so the offered
/// target is answered by resolution's first step rather than by the tie that produced it — that is
/// what makes "issue it unchanged" true.
fn candidate_rows(candidates: &[ResolutionCandidate]) -> Vec<Candidate> {
    candidates
        .iter()
        .map(|candidate| Candidate {
            id: candidate.id,
            qn: candidate.qn.clone(),
            kind: candidate.kind.clone(),
            file: candidate.file.clone(),
            lines: [candidate.lines.start, candidate.lines.end],
            score: Score::from_f64(candidate.score),
            why: candidate.why.clone(),
            next_call: NextCall {
                tool: NAME,
                target: candidate.qn.clone(),
            },
        })
        .collect()
}

/// One resolved node and the mode chosen for it.
struct AnswerRequest<'a> {
    generation: &'a str,
    indexed_at: &'a str,
    now: &'a str,
    budget: u64,
    target: &'a str,
    node: &'a NodeRow,
    what: What,
    why: String,
    /// Only `what: "source"` reads it; every other mode is answerable from the graph alone.
    range: Option<&'a [i64]>,
}

/// Dispatches one resolved node to its detail mode. Each mode is its own answer.
fn answer_for(ctx: &ToolContext, request: &AnswerRequest<'_>) -> Result<Envelope<Value>, AppError> {
    match request.what {
        What::Members => members_answer(ctx, request),
        What::Edges => edges_answer(ctx, request),
        What::Coverage => {
            let rows = ctx
                .reader
                .rows_for_paths(std::slice::from_ref(&request.node.path))?;
            coverage_answer(
                ctx,
                CoverageRequest {
                    generation: request.generation,
                    indexed_at: request.indexed_at,
                    now: request.now,
                    budget: request.budget,
                    prefix: request.node.path.clone(),
                    why: request.why.clone(),
                    rows,
                    node: Some(request.node),
                },
            )
        }
        What::Outline => outline_answer(ctx, request),
        What::Source => source_answer(ctx, request),
    }
}

/// A container's detail: its direct children, in source order.
fn members_answer(
    ctx: &ToolContext,
    request: &AnswerRequest<'_>,
) -> Result<Envelope<Value>, AppError> {
    let scope = scope_of(
        ctx,
        request.generation,
        request.indexed_at,
        std::slice::from_ref(&request.node.path),
        None,
        request.now,
    )?;
    let payload = MembersPayload {
        target: TargetRef::of(request.node),
        what: What::Members.as_str(),
        why: request.why.clone(),
        members: member_rows(ctx.reader.as_ref(), request.node)?,
    };
    let totals = Totals::new(scope.relation)
        .with_total("members", Some(u64_of(payload.members.len())))
        .with_relation_last();
    let value = payload_value(&payload)?;
    Ok(sized(
        request.budget,
        ok(value, answer_spec(String::new(), &scope, totals)),
        members_summary,
    ))
}

/// One node's neighbours: the grouped shape `trace` ships, at one hop.
fn edges_answer(
    ctx: &ToolContext,
    request: &AnswerRequest<'_>,
) -> Result<Envelope<Value>, AppError> {
    let scope = scope_of(
        ctx,
        request.generation,
        request.indexed_at,
        std::slice::from_ref(&request.node.path),
        None,
        request.now,
    )?;
    // Depth 1 is "the edges of this node": a deeper walk is `trace`'s question, not an expansion of
    // one item.
    let grouped = expand(
        &ReaderSource(ctx.reader.as_ref()),
        request.node.id,
        Direction::All,
        1,
        None,
    )?;
    let inbound = edge_groups(&grouped.inbound);
    let outbound = edge_groups(&grouped.outbound);
    let edges_total = count_items(&inbound) + count_items(&outbound);
    let payload = EdgesPayload {
        target: TargetRef::of(request.node),
        what: What::Edges.as_str(),
        why: request.why.clone(),
        direction: grouped.direction.as_str(),
        depth: grouped.depth,
        inbound,
        out: outbound,
    };
    let totals = Totals::new(scope.relation)
        .with_total("edges", Some(edges_total))
        .with_relation_last();
    let value = payload_value(&payload)?;
    Ok(sized(
        request.budget,
        ok(value, answer_spec(String::new(), &scope, totals)),
        edges_summary,
    ))
}

/// A container's outline: what it declares, in source order.
fn outline_answer(
    ctx: &ToolContext,
    request: &AnswerRequest<'_>,
) -> Result<Envelope<Value>, AppError> {
    let scope = scope_of(
        ctx,
        request.generation,
        request.indexed_at,
        std::slice::from_ref(&request.node.path),
        None,
        request.now,
    )?;
    let members = outline_rows(ctx.reader.as_ref(), request.node)?;
    let totals = Totals::new(scope.relation)
        .with_total("members", Some(u64_of(members.len())))
        .with_relation_last();
    let payload = OutlinePayload {
        target: TargetRef::of(request.node),
        what: What::Outline.as_str(),
        why: request.why.clone(),
        path: request.node.path.clone(),
        members,
    };
    let value = payload_value(&payload)?;
    Ok(sized(
        request.budget,
        ok(value, answer_spec(String::new(), &scope, totals)),
        outline_summary,
    ))
}

/// `source`: the only mode that reads bytes, and therefore the only one that can report drift.
fn source_answer(
    ctx: &ToolContext,
    request: &AnswerRequest<'_>,
) -> Result<Envelope<Value>, AppError> {
    let node = request.node;
    let Some(file) = ctx.reader.file_by_path(&node.path)? else {
        let scope = scope_of(
            ctx,
            request.generation,
            request.indexed_at,
            std::slice::from_ref(&node.path),
            None,
            request.now,
        )?;
        let why = format!("\"{}\" is not an indexed file", node.path);
        return missing(
            &MissingPayload {
                target: request.target.to_owned(),
                why: why.clone(),
                hint: HINT_NOT_INDEXED.to_owned(),
            },
            scoped_spec(why, &scope),
        );
    };

    // The index stores repo-relative paths, so an escaping one is a writer bug; refusing it here is
    // what keeps `what: "source"` from becoming an arbitrary-file reader (`DESIGN.md` §10.4).
    let Some(absolute) = source_path_of(&ctx.config.repo_root, &node.path) else {
        let summary = format!("refusing to read \"{}\"", node.path);
        return Ok(fail(
            AppError::internal(format!(
                "refusing to read \"{}\" outside the repository root",
                node.path
            ))
            .with_hint("re-index; the stored path is repo-relative by construction"),
            EnvelopeSpec::new(
                summary,
                read_status_of(request.generation, request.indexed_at, None, request.now),
                no_coverage(),
            )
            .with_next_tools(next_tools_for(NAME)),
        ));
    };

    let bytes = std::fs::read(&absolute).ok();
    let drift = drift_of(&node.path, bytes.as_deref(), &file.sha256);
    let scope = scope_of(
        ctx,
        request.generation,
        request.indexed_at,
        std::slice::from_ref(&node.path),
        drift.as_deref(),
        request.now,
    )?;
    // The node's extent is the single effective span, `range` narrows inside it, and the clamp is
    // disclosed in `why` rather than in a second window field: one window, one place to read it.
    let plan = source_window_of(node, request.range);
    let content = match (bytes.as_deref(), &drift) {
        (Some(bytes), None) => Some(window_lines(
            &String::from_utf8_lossy(bytes),
            plan.window[0],
            plan.window[1],
        )),
        _ => None,
    };
    let totals = Totals::new(scope.relation)
        .with_total("lines", Some(u64_of(content.as_ref().map_or(0, Vec::len))))
        .with_relation_last();
    let payload = SourcePayload {
        target: TargetRef::of(node),
        what: What::Source.as_str(),
        why: match plan.note {
            Some(note) => format!("{}; {note}", request.why),
            None => request.why.clone(),
        },
        path: node.path.clone(),
        source_drift: drift.is_some(),
        window: plan.window,
        content,
    };
    let value = payload_value(&payload)?;
    Ok(sized(
        request.budget,
        ok(value, answer_spec(String::new(), &scope, totals)),
        source_summary,
    ))
}

/// A folder answer, either inferred from a prefix that matched no node or asked for on a node.
struct CoverageRequest<'a> {
    generation: &'a str,
    indexed_at: &'a str,
    now: &'a str,
    budget: u64,
    prefix: String,
    why: String,
    rows: PathCoverage,
    node: Option<&'a NodeRow>,
}

fn coverage_answer(
    ctx: &ToolContext,
    request: CoverageRequest<'_>,
) -> Result<Envelope<Value>, AppError> {
    let scope = scope_of(
        ctx,
        request.generation,
        request.indexed_at,
        std::slice::from_ref(&request.prefix),
        None,
        request.now,
    )?;
    let files = file_rows(&request.rows.files);
    let not_indexed = skip_rows(&request.rows.not_indexed);
    let totals = Totals::new(scope.relation)
        .with_total("files", Some(u64_of(files.len())))
        .with_total("not_indexed", Some(scope.not_indexed_total))
        .with_relation_last();
    let payload = CoveragePayload {
        target: request.node.map_or_else(TargetRef::folder, TargetRef::of),
        what: What::Coverage.as_str(),
        why: request.why,
        prefix: request.prefix,
        files,
        not_indexed,
    };
    let value = payload_value(&payload)?;
    Ok(sized(
        request.budget,
        ok(value, answer_spec(String::new(), &scope, totals)),
        coverage_summary,
    ))
}

/// The `bootstrapping` answer of an unpublished graph: no generation, no coverage, and a retry
/// interval instead of a promise.
fn unpublished() -> Envelope<Value> {
    bootstrapping(
        BootstrappingSpec::new(
            EnvelopeSpec::new(
                UNPUBLISHED_SUMMARY.to_owned(),
                ReadStatus {
                    generation: UNPUBLISHED.to_owned(),
                    indexed_at: String::new(),
                    stale: true,
                    stale_reason: Some(NO_GENERATION.to_owned()),
                    stale_checked_at: None,
                    decisive: None,
                },
                no_coverage(),
            )
            .with_next_tools(next_tools_for(NAME)),
            BOOTSTRAP_RETRY_MS,
        )
        .with_message(UNPUBLISHED_MESSAGE)
        .with_hint(UNPUBLISHED_HINT),
    )
}

/// A store read that fails is an error envelope with the error's own code, never `not_found`: a
/// failed read is not an absence claim.
fn read_failure(error: AppError, now: String) -> Envelope<Value> {
    fail(
        error,
        EnvelopeSpec::new(
            UNREAD_SUMMARY.to_owned(),
            ReadStatus {
                generation: String::new(),
                indexed_at: String::new(),
                stale: true,
                stale_reason: Some(UNREAD_ANSWER.to_owned()),
                stale_checked_at: Some(now),
                decisive: None,
            },
            no_coverage(),
        )
        .with_next_tools(next_tools_for(NAME)),
    )
}

/// A container's children: the reader's `members` query, which is already `ORDER BY start_line,
/// qn`. One level only, so a file's children are its top-level symbols and a class's are its
/// methods: the agent can pick what to expand next instead of reading the file.
fn member_rows(reader: &dyn GraphReader, node: &NodeRow) -> Result<Vec<MemberRow>, AppError> {
    let mut rows = Vec::new();
    for child in reader.members(node.id)? {
        rows.push(MemberRow {
            id: child.id,
            name: child.name,
            kind: child.kind,
            lines: [child.start_line, child.end_line],
            degree: degree_of(reader, child.id)?,
        });
    }
    Ok(rows)
}

/// `members` for a class-like node and `nodes_by_file` for a file, in the store's own source order.
///
/// Both queries sort by `start_line, qn`, so this maps and does not re-sort: one source order, not
/// two that could disagree. The node's own row is dropped from a file's list — `target` already
/// states the address and the whole file's extent, and repeating it as its own member would read as
/// a child.
fn outline_rows(reader: &dyn GraphReader, node: &NodeRow) -> Result<Vec<OutlineMember>, AppError> {
    let rows = if node.kind == "file" {
        reader
            .nodes_by_file(node.file_id)?
            .into_iter()
            .filter(|row| row.id != node.id)
            .collect()
    } else {
        reader.members(node.id)?
    };
    Ok(rows
        .into_iter()
        .map(|row| OutlineMember {
            qn: row.qn,
            kind: row.kind,
            lines: [row.start_line, row.end_line],
            signature: row.signature,
        })
        .collect())
}

/// `None` is "the publish did not measure this node", which is not the same answer as degree 0.
fn degree_of(reader: &dyn GraphReader, node_id: i64) -> Result<Option<Degree>, AppError> {
    let metrics: Option<NodeMetrics> = reader.metrics_for(node_id)?;
    Ok(metrics.map(|metrics| Degree {
        inbound: metrics.in_degree,
        out: metrics.out_degree,
    }))
}

/// The domain's groups as the payload shows them.
fn edge_groups(groups: &[NeighbourGroup]) -> Vec<EdgeGroup> {
    groups
        .iter()
        .map(|group| EdgeGroup {
            edge: group.kind.clone(),
            tier: group.tier.clone(),
            items: group
                .items
                .iter()
                .map(|item| EdgeItem {
                    id: item.id,
                    qn: item.qn.clone(),
                    kind: item.kind.clone(),
                    file: item.file.clone(),
                    line: item.line,
                    hop: item.hop,
                    external: item.external,
                })
                .collect(),
        })
        .collect()
}

/// How many rows a grouped neighbour list holds.
fn count_items(groups: &[EdgeGroup]) -> u64 {
    groups
        .iter()
        .map(|group| u64_of(group.items.len()))
        .fold(0, u64::saturating_add)
}

fn file_rows(rows: &[IndexedFileRow]) -> Vec<CoverageFileRow> {
    rows.iter()
        .map(|row| CoverageFileRow {
            id: row.id,
            path: row.path.clone(),
            language: row.language.clone(),
            sha256: row.sha256.clone(),
            line_count: row.line_count,
            parse_status: row.parse_status.clone(),
            indexed_generation: row.indexed_generation.clone(),
        })
        .collect()
}

fn skip_rows(rows: &[SkippedPathRow]) -> Vec<CoverageSkipRow> {
    rows.iter()
        .map(|row| CoverageSkipRow {
            path: row.path.clone(),
            reason: row.reason.clone(),
            detail: row.detail.clone(),
        })
        .collect()
}

/// The absolute path of an indexed file, or `None` when the stored path would leave the tree.
///
/// [`normalize_rel`] keeps `..` (collapsing it would make an escaping path look contained), and
/// every component that is not a plain name — `RootDir`, `Prefix`, `ParentDir`, `CurDir` — is a
/// path this answer must not read. Accepting only `Component::Normal` is therefore the whole check:
/// a relative path of ordinary names joined onto the repository root cannot escape it.
fn source_path_of(repo_root: &Path, stored_path: &str) -> Option<PathBuf> {
    let rel = normalize_rel(stored_path);
    if rel.is_empty() {
        return None;
    }
    let mut inside = PathBuf::new();
    for component in Path::new(&rel).components() {
        match component {
            Component::Normal(part) => inside.push(part),
            _ => return None,
        }
    }
    Some(repo_root.join(inside))
}

/// What this call learned about the working tree: `None` is "the bytes match the index", `Some` is
/// the reason they do not. Both a changed file and a vanished one are drift — neither is an error.
fn drift_of(path: &str, bytes: Option<&[u8]>, indexed_sha256: &str) -> Option<String> {
    let Some(bytes) = bytes else {
        return Some(format!("\"{path}\" is no longer readable on disk"));
    };
    if sha256_hex(bytes) == indexed_sha256.to_lowercase() {
        return None;
    }
    Some(format!("\"{path}\" changed since the index"))
}

/// A verified line window and the sentence `why` gains when the caller's `range` was not honoured
/// as written.
struct WindowPlan {
    window: [i64; 2],
    note: Option<String>,
}

/// The window a `what: "source"` answer reads.
///
/// Both ends clamp into the node's own extent: a line outside the node is not part of the answer
/// this node can give, so the request narrows instead of failing. A pair that arrives inverted
/// selects nothing rather than being silently reordered — the caller asked for a span that cannot
/// exist, and an empty `content` says so where a reordered window would look like an answer.
/// Neither case throws: a caller's bad input is answered and disclosed (`DESIGN.md` §10).
fn source_window_of(node: &NodeRow, range: Option<&[i64]>) -> WindowPlan {
    let [start, end] = extent_of(node);
    let Some(range) = range else {
        return WindowPlan {
            window: [start, end],
            note: None,
        };
    };
    let [from, to] = match <[i64; 2]>::try_from(range) {
        Ok(pair) => pair,
        Err(_) => {
            return WindowPlan {
                window: [start, end],
                note: Some(RANGE_SHAPE_NOTE.to_owned()),
            };
        }
    };
    let window = [from.clamp(start, end), to.clamp(start, end)];
    let asked = format!("range {from}-{to}");
    if to < from {
        return WindowPlan {
            window,
            note: Some(format!("{asked} is inverted, so no lines were selected")),
        };
    }
    if window == [from, to] {
        return WindowPlan { window, note: None };
    }
    WindowPlan {
        window,
        note: Some(format!(
            "{asked} clamped to the node's extent {start}-{end}"
        )),
    }
}

/// The node's own line span, `[start, end]`, never inverted: a caller's `range` is read inside it.
fn extent_of(node: &NodeRow) -> [i64; 2] {
    let start = node.start_line.max(1);
    [start, node.end_line.max(start)]
}

/// The numbered lines of `[from, to]` inside `text`.
///
/// A window past the end of the file clamps and a window that starts past it is empty rather than an
/// error, because the index's line numbers for a verified file are facts about that file.
fn window_lines(text: &str, from: i64, to: i64) -> Vec<SourceLine> {
    let all = lines_of(text);
    let start = from.max(1);
    let end = to.min(i64::try_from(all.len()).unwrap_or(i64::MAX));
    let mut lines = Vec::new();
    let mut number = start;
    while number <= end {
        let index = usize::try_from(number - 1).unwrap_or(usize::MAX);
        lines.push(SourceLine {
            line: number,
            text: all.get(index).copied().unwrap_or_default().to_owned(),
        });
        number += 1;
    }
    lines
}

/// `text.split(/\r?\n/)`: a `\r` is dropped only when a `\n` follows it, so a lone trailing
/// carriage return stays part of its line, exactly as the TypeScript's split leaves it.
fn lines_of(text: &str) -> Vec<&str> {
    let pieces: Vec<&str> = text.split('\n').collect();
    let last = pieces.len().saturating_sub(1);
    pieces
        .into_iter()
        .enumerate()
        .map(|(index, piece)| {
            if index == last {
                piece
            } else {
                piece.strip_suffix('\r').unwrap_or(piece)
            }
        })
        .collect()
}
