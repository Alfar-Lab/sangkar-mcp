//! The response contract: one shape, built only through this module's constructors.
//!
//! Ported from `src/shared/envelope.ts`. Every answer carries the same metadata keys in the same
//! order — `status`, `summary`, `read_status`, `coverage`, `totals`, `truncated`,
//! `truncation_reason`, `next`, `next_tools`, `payload` — plus `error`, which is present if and
//! only if `status` is [`Status::Error`] or [`Status::Bootstrapping`]. Construction goes through
//! [`ok`], [`ambiguous`], [`not_found`], [`fail`] and [`bootstrapping`], so a handler cannot omit
//! `read_status` or `coverage` by accident, and [`crate::budget::emit`] is the only place a payload
//! may be cut.
//!
//! `totals` and any payload object keep insertion order (the workspace enables serde_json's
//! `preserve_order`): cursor tests and TOON tables read those keys in the order they were written,
//! so a re-ordering map would be a silent wire change.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::errors::{AppError, ErrorCode, ErrorPayload};

/// The five statuses a response may carry. `error` and `bootstrapping` are the only two that also
/// carry an `error` object; the rest are a positive or explicitly negative answer, which is why
/// their absence can be trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Ambiguous,
    NotFound,
    Error,
    Bootstrapping,
}

impl Status {
    /// The five, in the order the TypeScript module publishes them.
    pub const ALL: [Self; 5] = [
        Self::Ok,
        Self::Ambiguous,
        Self::NotFound,
        Self::Error,
        Self::Bootstrapping,
    ];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Ambiguous => "ambiguous",
            Self::NotFound => "not_found",
            Self::Error => "error",
            Self::Bootstrapping => "bootstrapping",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a payload was cut. `budget` is set by [`crate::budget::emit`]; the other three are set by a
/// tool whose own hard limit was reached before the budget was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TruncationReason {
    Budget,
    ItemCap,
    Depth,
    Page,
}

impl TruncationReason {
    /// The four, in the order the TypeScript module publishes them.
    pub const ALL: [Self; 4] = [Self::Budget, Self::ItemCap, Self::Depth, Self::Page];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Budget => "budget",
            Self::ItemCap => "item_cap",
            Self::Depth => "depth",
            Self::Page => "page",
        }
    }
}

/// `eq` claims the total is exact. It is permitted only when coverage is complete, no unresolved
/// edges exist in scope and the generation is fresh; anything else is a lower bound and says `gte`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TotalRelation {
    Eq,
    Gte,
}

impl TotalRelation {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Gte => "gte",
        }
    }
}

/// One path the index knows it did not cover, with the reason it was skipped.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotIndexedEntry {
    pub path: String,
    pub reason: String,
}

impl NotIndexedEntry {
    #[must_use]
    pub fn new(path: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

/// Freshness of the answer itself.
///
/// `stale_checked_at` is separate from `indexed_at` because "when was this built" and "when did we
/// last look" are different questions, and only the second one lets an agent decide whether
/// re-asking is worth it. `decisive` is filled once, by the composition root: no handler knows
/// whether the answer it produced is the one that should end the agent's search.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadStatus {
    pub generation: String,
    pub indexed_at: String,
    pub stale: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_checked_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decisive: Option<bool>,
}

impl ReadStatus {
    /// A fresh generation with nothing else claimed.
    #[must_use]
    pub fn new(generation: impl Into<String>, indexed_at: impl Into<String>) -> Self {
        Self {
            generation: generation.into(),
            indexed_at: indexed_at.into(),
            stale: false,
            stale_reason: None,
            stale_checked_at: None,
            decisive: None,
        }
    }
}

/// One language's share of the indexed test corpus.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCensusEntry {
    pub language: String,
    pub nodes: u64,
}

/// The test corpus this graph indexed. `nodes: 0` with `languages: []` is "no test symbols were
/// indexed", which is a different fact from "nothing covers this change".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCensus {
    pub nodes: u64,
    pub languages: Vec<TestCensusEntry>,
}

/// What the answer is allowed to claim. `complete` gates every absence claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub complete: bool,
    pub indexed: u64,
    pub skipped: u64,
    pub failed: u64,
    pub unresolved_edges: u64,
    pub not_indexed: Vec<NotIndexedEntry>,
    /// Indexed test symbols and the languages they come from. Optional: only the tools that render
    /// it pay the query.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestCensus>,
}

impl Coverage {
    /// A clean, complete coverage over `indexed` symbols and nothing skipped or failed.
    #[must_use]
    pub fn complete(indexed: u64) -> Self {
        Self {
            complete: true,
            indexed,
            skipped: 0,
            failed: 0,
            unresolved_edges: 0,
            not_indexed: Vec::new(),
            tests: None,
        }
    }
}

/// The key that must lead every `totals` object.
const TOTAL_RELATION: &str = "total_relation";

/// Per-list counts plus the relation they are counted under.
///
/// Every `<list>_total` key survives truncation untouched, so `calls_total: 2` with one item shown
/// still reads "showing 1 of 2". A total is `null` only when the list it counts does not apply to
/// this scope — an empty change set has no test gaps to count, which is a different answer from
/// "zero gaps".
///
/// The backing map is private and the builders are the only way in, because the key order is part
/// of the contract: `total_relation` is always written first (the TypeScript interface declares it
/// first and `emit` rewrites it in place), and the `<list>_total` keys follow in the order the
/// handler added them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Totals(Map<String, Value>);

impl Totals {
    /// Totals with only the relation set.
    #[must_use]
    pub fn new(relation: TotalRelation) -> Self {
        let mut map = Map::new();
        map.insert(
            TOTAL_RELATION.to_owned(),
            Value::String(relation.as_str().to_owned()),
        );
        Self(map)
    }

    /// The relation these totals are counted under.
    ///
    /// A map that lost its `total_relation` reads as [`TotalRelation::Gte`]: a missing relation may
    /// never be read as an exact claim.
    #[must_use]
    pub fn relation(&self) -> TotalRelation {
        match self.0.get(TOTAL_RELATION).and_then(Value::as_str) {
            Some("eq") => TotalRelation::Eq,
            _ => TotalRelation::Gte,
        }
    }

    /// Rewrites the relation in place, keeping it the first key.
    pub fn set_relation(&mut self, relation: TotalRelation) {
        let value = Value::String(relation.as_str().to_owned());
        if let Some(slot) = self.0.get_mut(TOTAL_RELATION) {
            *slot = value;
            return;
        }
        // Only reachable for a map that arrived without a relation (a hand-built payload from
        // another process); the rewritten map keeps the contract's key order.
        let mut rebuilt = Map::new();
        rebuilt.insert(TOTAL_RELATION.to_owned(), value);
        rebuilt.extend(std::mem::take(&mut self.0));
        self.0 = rebuilt;
    }

    /// The same totals under a different relation.
    #[must_use]
    pub fn with_relation(mut self, relation: TotalRelation) -> Self {
        self.set_relation(relation);
        self
    }

    /// Adds `<list>_total`. Pass the list's own name (`calls`), not the key (`calls_total`).
    ///
    /// `None` writes `null`, which means "this list does not apply to the scope" — never zero.
    #[must_use]
    pub fn with_total(mut self, list: &str, total: Option<u64>) -> Self {
        let key = format!("{list}_total");
        let value = total.map_or(Value::Null, Value::from);
        self.0.insert(key, value);
        self
    }

    /// Adds `unresolved_material`, the subset of unresolved edges a caller can act on.
    #[must_use]
    pub fn with_unresolved_material(mut self, count: u64) -> Self {
        self.0
            .insert("unresolved_material".to_owned(), Value::from(count));
        self
    }

    /// Adds `candidates_shown`, the slice of a tie an ambiguous answer served.
    #[must_use]
    pub fn with_candidates_shown(mut self, count: u64) -> Self {
        self.0
            .insert("candidates_shown".to_owned(), Value::from(count));
        self
    }

    /// Moves `total_relation` to the END of the object, keeping every other key in its order.
    ///
    /// Most tools write the relation first, and [`Totals::new`] does that. Two do not:
    /// `src/tools/impact/handler.ts:289-295` and `expand`'s members/edges/ambiguous totals declare
    /// their `<list>_total` keys first and `total_relation` last, so the emitted bytes differ from
    /// the relation-first shape. A caller cannot observe the difference — a JSON object is unordered
    /// — but the `--json` captures are compared byte for byte, so a tool that needs the other order
    /// has to say so. Call this LAST, after every other key is in place.
    #[must_use]
    pub fn with_relation_last(mut self) -> Self {
        if let Some(relation) = self.0.shift_remove(TOTAL_RELATION) {
            self.0.insert(TOTAL_RELATION.to_owned(), relation);
        }
        self
    }

    /// One entry of the wire object, for a reader that has to print it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// The wire object itself.
    #[must_use]
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.0
    }
}

/// The one response shape every tool returns.
///
/// Fields are declared in wire order, and `truncation_reason` / `next` are written as `null` rather
/// than omitted: the TypeScript side types them `TruncationReason | null` and `string | null`, and a
/// key that is present is a different byte string from one that is missing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope<T> {
    pub status: Status,
    pub summary: String,
    pub read_status: ReadStatus,
    pub coverage: Coverage,
    pub totals: Totals,
    pub truncated: bool,
    pub truncation_reason: Option<TruncationReason>,
    pub next: Option<String>,
    pub next_tools: Vec<String>,
    pub payload: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorPayload>,
}

/// Everything a constructor needs that is not the payload.
///
/// `totals` defaults to the relation the `read_status`/`coverage` pair can honestly support;
/// `next`, `truncated` and `truncation_reason` are overridable through [`EnvelopeSpec::with_truncation`]
/// for the tools that hit `depth`, `page` or `item_cap` before `budget` — that tool is the only
/// decider besides [`crate::budget::emit`], which is the only place a truncation is *derived* from
/// size. A caller-supplied truncation is copied as it stands and `emit` never erases it (a
/// zero-row page depends on that); it only downgrades its relation to `gte`.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvelopeSpec {
    pub summary: String,
    pub read_status: ReadStatus,
    pub coverage: Coverage,
    pub totals: Option<Totals>,
    pub next_tools: Vec<String>,
    pub next: Option<String>,
    pub truncated: bool,
    pub truncation_reason: Option<TruncationReason>,
}

impl EnvelopeSpec {
    #[must_use]
    pub fn new(summary: impl Into<String>, read_status: ReadStatus, coverage: Coverage) -> Self {
        Self {
            summary: summary.into(),
            read_status,
            coverage,
            totals: None,
            next_tools: Vec::new(),
            next: None,
            truncated: false,
            truncation_reason: None,
        }
    }

    /// Replaces the derived relation and every `<list>_total` with the caller's own totals.
    #[must_use]
    pub fn with_totals(mut self, totals: Totals) -> Self {
        self.totals = Some(totals);
        self
    }

    #[must_use]
    pub fn with_next_tools(mut self, tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.next_tools = tools.into_iter().map(Into::into).collect();
        self
    }

    /// A truncation the tool itself decided on (`depth`, `page`, `item_cap`) and the token that
    /// continues it. `emit` downgrades the relation to `gte` even when the payload fits.
    #[must_use]
    pub fn with_truncation(mut self, reason: TruncationReason, next: Option<String>) -> Self {
        self.truncated = true;
        self.truncation_reason = Some(reason);
        self.next = next;
        self
    }
}

/// The default `error.message` of a bootstrapping answer.
pub const DEFAULT_BOOTSTRAP_MESSAGE: &str = "the index is still being built";

/// An [`EnvelopeSpec`] plus the retry contract a bootstrapping answer carries.
///
/// The TypeScript side declares `BootstrappingSpec extends EnvelopeSpec`; Rust has no struct
/// inheritance, so the envelope spec is nested rather than flattened. The wire shape is unaffected:
/// `retry_after_ms`, `message` and `hint` all live inside the `error` object.
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrappingSpec {
    pub envelope: EnvelopeSpec,
    pub retry_after_ms: u64,
    pub message: Option<String>,
    pub hint: Option<String>,
}

impl BootstrappingSpec {
    #[must_use]
    pub fn new(envelope: EnvelopeSpec, retry_after_ms: u64) -> Self {
        Self {
            envelope,
            retry_after_ms,
            message: None,
            hint: None,
        }
    }

    #[must_use]
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    #[must_use]
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// The relation an untruncated payload may claim, derived from the three conditions this module can
/// see: complete coverage, no unresolved edges in scope, fresh generation. [`crate::budget::emit`]
/// owns the fourth (truncation) and downgrades when it cuts.
#[must_use]
pub fn relation_for(read_status: &ReadStatus, coverage: &Coverage) -> TotalRelation {
    let exact = coverage.complete && coverage.unresolved_edges == 0 && !read_status.stale;
    if exact {
        TotalRelation::Eq
    } else {
        TotalRelation::Gte
    }
}

/// The body every constructor shares, so the envelope literal exists exactly once.
///
/// The TypeScript version copies `read_status`, `coverage.not_indexed` and `next_tools` to stop a
/// caller mutating a shared fixture; here the parameters are owned, so the copy is the move and
/// there is nothing left to alias.
fn build_envelope<T>(status: Status, payload: Option<T>, spec: EnvelopeSpec) -> Envelope<T> {
    let totals = spec
        .totals
        .unwrap_or_else(|| Totals::new(relation_for(&spec.read_status, &spec.coverage)));
    Envelope {
        status,
        summary: spec.summary,
        read_status: spec.read_status,
        coverage: spec.coverage,
        totals,
        truncated: spec.truncated,
        truncation_reason: spec.truncation_reason,
        next: spec.next,
        next_tools: spec.next_tools,
        payload,
        error: None,
    }
}

/// A copy of `env` carrying a different payload.
#[must_use]
pub fn with_payload<T>(mut env: Envelope<T>, payload: Option<T>) -> Envelope<T> {
    env.payload = payload;
    env
}

/// A copy of `env` with new totals; every `<list>_total` in `totals` is the caller's to keep.
#[must_use]
pub fn with_totals<T>(mut env: Envelope<T>, totals: Totals) -> Envelope<T> {
    env.totals = totals;
    env
}

/// A query that answered. `payload` is the tool's own shape, unbounded here.
#[must_use]
pub fn ok<T>(payload: T, spec: EnvelopeSpec) -> Envelope<T> {
    build_envelope(Status::Ok, Some(payload), spec)
}

/// A query whose target matched more than one node. `payload` carries the candidates.
#[must_use]
pub fn ambiguous<T>(payload: T, spec: EnvelopeSpec) -> Envelope<T> {
    build_envelope(Status::Ambiguous, Some(payload), spec)
}

/// A query that found nothing. `payload` carries near misses, never a bare zero.
#[must_use]
pub fn not_found<T>(payload: T, spec: EnvelopeSpec) -> Envelope<T> {
    build_envelope(Status::NotFound, Some(payload), spec)
}

/// A failure: `status: error` plus the contract error payload derived from the error.
///
/// The TypeScript `fail` takes `unknown` and rebuilds a payload from the contract fields, turning a
/// code outside the closed union into `internal`. That branch has no Rust equivalent: an
/// [`AppError`] already carries an [`ErrorCode`] and nothing else can be passed here.
#[must_use]
pub fn fail<T>(error: AppError, spec: EnvelopeSpec) -> Envelope<T> {
    let mut env = build_envelope(Status::Error, None, spec);
    env.error = Some(error.payload());
    env
}

/// The index is still being built. Returns `retry_after_ms` instead of blocking the client past its
/// deadline.
#[must_use]
pub fn bootstrapping<T>(spec: BootstrappingSpec) -> Envelope<T> {
    let mut env = build_envelope(Status::Bootstrapping, None, spec.envelope);
    env.error = Some(ErrorPayload {
        code: ErrorCode::BOOTSTRAPPING,
        message: spec
            .message
            .unwrap_or_else(|| DEFAULT_BOOTSTRAP_MESSAGE.to_owned()),
        hint: spec.hint,
        retryable: None,
        retry_after_ms: Some(spec.retry_after_ms),
    });
    env
}
