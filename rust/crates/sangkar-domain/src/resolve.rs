//! Symbol resolution: one free-text query to at most one node, or an honest ambiguity.
//!
//! Ported from `src/shared/domain/resolve.ts` (`DESIGN.md` §8). The failure mode this module exists
//! to prevent is a confident answer about the wrong node, so it never guesses: two or more hits
//! that score the same are answered as [`Resolution::Ambiguous`], and a query that matches nothing
//! is answered as [`Resolution::NotFound`] carrying near misses rather than a bare zero.
//!
//! Detection order is fixed and observable through [`RESOLUTION_STEPS`]: exact qualified name,
//! exact name, dotted tail, path, then `@decorator` as a *property filter* (`DESIGN.md` §8:
//! decorators are properties, not a node kind, so no `decorator` kind is invented here). The first
//! step that produces a hit answers, even when a later step would also have matched; the `path`
//! filter narrows every step and the near misses, so it can empty an answer but never widen the
//! order.
//!
//! Two porting decisions are visible in the types:
//!
//! - the TypeScript throws on a store failure, so [`resolve`] returns `Result<Resolution, AppError>`
//!   rather than the contracted bare `Resolution`: a read error flattened into `not_found` would be
//!   a confident wrong answer, and that is the one thing this module exists to prevent.
//! - `node.props` is raw text here — `sangkar_store::read::nodes` keeps it that way because neither
//!   this crate nor the store depends on a JSON parser — so [`props`] reads the two keys resolution
//!   and expansion need. Malformed text reads as "no props", the same boundary the store documents;
//!   the raise for a malformed column belongs to the crate that owns the parser.

use sangkar_core::errors::AppError;
use sangkar_core::paths::{is_under_prefix, normalize_rel};
use sangkar_store::db::Conn;
use sangkar_store::read::nodes::{NodeRow, find_by_name, find_by_qn};
use sangkar_store::read::reader::{GraphReader, LineRange};

/// The contract cap on ambiguity output. Not a presentation knob: it is the width of the
/// `candidates` list `TOOLS-SPEC.md` §3 allows, so the tool never has to decide.
pub const CANDIDATE_CAP: usize = 5;

/// The detection order, as data, so a caller can report which step answered. The variant order of
/// [`ResolutionStep`] is this order.
pub const RESOLUTION_STEPS: [&str; 5] =
    ["exact_qn", "exact_name", "dotted_tail", "path", "decorator"];

/// Which of [`RESOLUTION_STEPS`] answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResolutionStep {
    ExactQn,
    ExactName,
    DottedTail,
    Path,
    Decorator,
}

impl ResolutionStep {
    /// The wire spelling: the member of [`RESOLUTION_STEPS`] this variant names.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExactQn => RESOLUTION_STEPS[0],
            Self::ExactName => RESOLUTION_STEPS[1],
            Self::DottedTail => RESOLUTION_STEPS[2],
            Self::Path => RESOLUTION_STEPS[3],
            Self::Decorator => RESOLUTION_STEPS[4],
        }
    }
}

/// Caller-side narrowing. `kinds` is all-or-nothing, like `find_by_name`; `path` is a repo-relative
/// prefix applied before any cap, so a filtered query never answers from outside it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolveOptions {
    pub kinds: Option<Vec<String>>,
    pub path: Option<String>,
}

/// One plausible match, with everything needed to judge it without a second call.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolutionCandidate {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub lines: LineRange,
    pub score: f64,
    pub why: String,
}

/// The three possible answers, and never a partial one: `Ok` carries the node, `Ambiguous` carries
/// the tied candidates (no answer body), `NotFound` carries the near misses.
///
/// `candidates_total` is the size of the tie *before* the [`CANDIDATE_CAP`] slice, so an answer that
/// shows five of twelve says so; `candidates_shown` is `candidates.len()` and the two cannot drift.
/// `Ok` keeps the winning `score` and `why` because they are part of the candidate contract the tool
/// renders, not an internal detail.
// `Ok` owns a whole `NodeRow` while the other variants own a `Vec` handle. A resolution is produced
// once per query and matched once, never collected, so the size gap costs nothing to keep: boxing the
// row would change the contract's own `node: NodeRow` shape to buy an allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum Resolution {
    Ok {
        step: ResolutionStep,
        node: NodeRow,
        score: f64,
        why: String,
    },
    Ambiguous {
        step: ResolutionStep,
        candidates: Vec<ResolutionCandidate>,
        candidates_total: usize,
        candidates_shown: usize,
    },
    NotFound {
        near_misses: Vec<ResolutionCandidate>,
    },
}

/// The read surface resolution needs. A `Conn`-backed implementation comes from
/// [`resolution_source`], and `dyn GraphReader` implements this trait directly, so a tool holding a
/// reader needs no adapter; a test passes a hand-written object over an in-memory fixture, which is
/// how the ambiguity contract is provable without a database.
///
/// `find_by_decorator` has a default because the store has no props query: without an override the
/// decorator step degrades to "name matches *and* carries the property", the honest subset of the
/// filter rather than an invented node kind.
pub trait ResolutionSource {
    /// Exact `qn` resolution; `None` when nothing matches.
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError>;

    /// Exact bare-name resolution, ordered by `qn`; `kinds` is all-or-nothing.
    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError>;

    /// Every node carrying `@name`, narrowed by `kinds`.
    fn find_by_decorator(
        &self,
        name: &str,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        let nodes = self.find_by_name(name, kinds)?;
        Ok(nodes
            .into_iter()
            .filter(|node| has_decorator(node, name))
            .collect())
    }
}

/// The store's two prepared lookups, bound to a connection.
#[derive(Clone, Copy, Debug)]
pub struct ConnResolutionSource<'a> {
    conn: &'a Conn,
}

/// Binds [`ResolutionSource`] to a connection; the only database-aware function here.
#[must_use]
pub fn resolution_source(conn: &Conn) -> ConnResolutionSource<'_> {
    ConnResolutionSource { conn }
}

impl ResolutionSource for ConnResolutionSource<'_> {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        find_by_qn(self.conn, qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        find_by_name(self.conn, name, kinds)
    }
}

/// A tool holds `Rc<dyn GraphReader>` and must not build a second adapter, so the aggregate is a
/// resolution source by itself. No batched member exists to forward.
impl ResolutionSource for dyn GraphReader + '_ {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        GraphReader::find_by_qn(self, qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        GraphReader::find_by_name(self, name, kinds)
    }
}

/// One scored hit, kept with its row so `Ok` can return the node without a second lookup.
struct Hit {
    node: NodeRow,
    score: f64,
    why: String,
}

/// The hits of one detection step; only the first non-empty step is consulted.
struct StepHit {
    step: ResolutionStep,
    hits: Vec<Hit>,
}

/// One step of the table. A type alias keeps the table's type readable.
type Step = fn(&dyn ResolutionSource, &str, &Filter) -> Result<Option<StepHit>, AppError>;

/// The steps in order. `exact_qn` produces one hit by construction, so a single winner there is
/// always an `Ok` answer; every later step can tie and therefore can be `Ambiguous`.
const STEPS: [Step; 5] = [exact_qn, exact_name, dotted_tail, path_step, decorator_step];

/// Step scores. Their absolute values matter only for ordering; what matters for the ambiguity
/// contract is that every candidate produced by the same step with the same evidence scores
/// *identically*, which is what makes a tie a genuine tie.
const fn step_score(step: ResolutionStep) -> f64 {
    match step {
        ResolutionStep::ExactQn => 1.0,
        ResolutionStep::ExactName => 0.9,
        ResolutionStep::DottedTail => 0.8,
        ResolutionStep::Path => 0.7,
        ResolutionStep::Decorator => 0.6,
    }
}

/// A dotted query whose receiver matches the node's own scope is the better of two tail hits.
const DOTTED_SCOPE_BONUS: f64 = 0.05;
/// A path query names the file node itself, not one of its symbols.
const PATH_FILE_BONUS: f64 = 0.05;
/// Everything below the weakest step: plausible, never a match.
const NEAR_MISS_SCORE: f64 = 0.3;

/// `0.8 + 0.05` is `0.8500000000000001`; two decimals keeps the tie comparison exact.
fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn hit(node: NodeRow, score: f64, why: String) -> Hit {
    Hit {
        node,
        score: round2(score),
        why,
    }
}

fn candidate_of(hit: &Hit) -> ResolutionCandidate {
    ResolutionCandidate {
        id: hit.node.id,
        qn: hit.node.qn.clone(),
        kind: hit.node.kind.clone(),
        file: hit.node.path.clone(),
        lines: LineRange {
            start: hit.node.start_line,
            end: hit.node.end_line,
        },
        score: hit.score,
        why: hit.why.clone(),
    }
}

/// The options after normalization: a `kinds` list and a normalized prefix or none.
struct Filter {
    kinds: Vec<String>,
    under: Option<String>,
}

/// Absent, empty and whitespace-only all mean "no filter": `path: ""` answers as it did before the
/// option existed. [`normalize_rel`] would turn `""` into a prefix that matches nothing, which is
/// the one outcome this must not produce.
fn filter_of(options: &ResolveOptions) -> Filter {
    let path = normalize_rel(options.path.as_deref().unwrap_or_default().trim());
    Filter {
        kinds: options.kinds.clone().unwrap_or_default(),
        under: if path.is_empty() { None } else { Some(path) },
    }
}

/// An empty `kinds` list means every kind, matching `find_by_name`'s all-or-nothing filter; `None`
/// says the same thing to the store without a redundant statement shape.
fn kinds_of(filter: &Filter) -> Option<&[String]> {
    (!filter.kinds.is_empty()).then_some(filter.kinds.as_slice())
}

/// An empty `kinds` list means every kind, matching `find_by_name`'s all-or-nothing filter.
fn accepts_kind(node: &NodeRow, kinds: &[String]) -> bool {
    kinds.is_empty() || kinds.contains(&node.kind)
}

/// The path half of the filter. It can never be pushed into a store lookup, so every step applies it
/// to the rows it got back — that is what keeps a near miss from outside the prefix out.
fn accepts_path(node: &NodeRow, filter: &Filter) -> bool {
    match &filter.under {
        None => true,
        Some(under) => is_under_prefix(&node.path, under),
    }
}

/// Step 1: the unique `qn`. Nothing can tie with it, so it is answered immediately.
fn exact_qn(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Option<StepHit>, AppError> {
    let Some(node) = source.find_by_qn(query)? else {
        return Ok(None);
    };
    if !accepts_kind(&node, &filter.kinds) || !accepts_path(&node, filter) {
        return Ok(None);
    }
    let step = ResolutionStep::ExactQn;
    Ok(Some(StepHit {
        step,
        hits: vec![hit(
            node,
            step_score(step),
            "exact qualified name".to_owned(),
        )],
    }))
}

/// Step 2: bare name, narrowed by `kinds` when the caller supplied them.
fn exact_name(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Option<StepHit>, AppError> {
    let nodes: Vec<NodeRow> = source
        .find_by_name(query, kinds_of(filter))?
        .into_iter()
        .filter(|node| accepts_path(node, filter))
        .collect();
    if nodes.is_empty() {
        return Ok(None);
    }
    let step = ResolutionStep::ExactName;
    let total = nodes.len();
    let hits = nodes
        .into_iter()
        .enumerate()
        .map(|(index, node)| {
            let why = if total == 1 {
                "exact name, 1 of 1 candidate".to_owned()
            } else {
                format!("exact name, {} of {total} candidates", index + 1)
            };
            hit(node, step_score(step), why)
        })
        .collect();
    Ok(Some(StepHit { step, hits }))
}

/// Step 3: `Receiver.name` resolved through its tail, preferring the tail in its own scope.
fn dotted_tail(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Option<StepHit>, AppError> {
    let Some(dot) = query.rfind('.') else {
        return Ok(None);
    };
    if dot == 0 || dot + 1 == query.len() {
        return Ok(None);
    }
    let tail = &query[dot + 1..];
    let nodes: Vec<NodeRow> = source
        .find_by_name(tail, kinds_of(filter))?
        .into_iter()
        .filter(|node| accepts_path(node, filter))
        .collect();
    if nodes.is_empty() {
        return Ok(None);
    }
    let step = ResolutionStep::DottedTail;
    let scoped_suffix_dot = format!(".{query}");
    let scoped_suffix_colons = format!("::{query}");
    let hits = nodes
        .into_iter()
        .map(|node| {
            let scoped =
                node.qn.ends_with(&scoped_suffix_dot) || node.qn.ends_with(&scoped_suffix_colons);
            let why = if scoped {
                format!("dotted name \"{query}\" matched in the node's own scope")
            } else {
                format!("dotted tail \"{tail}\" of \"{query}\"")
            };
            let score = step_score(step) + if scoped { DOTTED_SCOPE_BONUS } else { 0.0 };
            hit(node, score, why)
        })
        .collect();
    Ok(Some(StepHit { step, hits }))
}

/// A query shaped like a path rather than a symbol: it has a separator, or its last segment ends in
/// a known source extension. The extension list is what keeps `Receiver.name` out of this step. The
/// TypeScript spells it as one regular expression; this crate has no regex dependency, so the same
/// language is spelled out below.
fn is_path_like(query: &str) -> bool {
    let basename = match query.rfind('/') {
        Some(slash) => &query[slash + 1..],
        None => query,
    };
    let name = basename.strip_prefix('.').unwrap_or(basename);
    let Some(dot) = name.rfind('.') else {
        return false;
    };
    let stem = &name[..dot];
    // `[\w.+-]+`: `\w` is the ASCII word class in a JavaScript regex without the `u` flag.
    let stem_is_name = !stem.is_empty()
        && stem
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '+' | '-'));
    stem_is_name && is_source_extension(&name[dot + 1..])
}

/// `[cm]?[jt]sx?`, spelled out: the optional module prefix, the `j`/`t`, the `s` and the optional `x`.
fn is_script_extension(extension: &str) -> bool {
    let rest = extension.strip_prefix(['c', 'm']).unwrap_or(extension);
    let mut chars = rest.chars();
    matches!(chars.next(), Some('j' | 't'))
        && chars.next() == Some('s')
        && matches!(chars.next(), Some('x') | None)
        && chars.next().is_none()
}

fn is_source_extension(extension: &str) -> bool {
    is_script_extension(extension)
        || matches!(
            extension,
            "py" | "go"
                | "rs"
                | "java"
                | "kt"
                | "rb"
                | "cs"
                | "cpp"
                | "cc"
                | "cxx"
                | "c"
                | "h"
                | "hpp"
                | "php"
                | "swift"
                | "scala"
                | "sql"
                | "sh"
                | "md"
                | "json"
                | "yml"
                | "yaml"
                | "toml"
                | "xml"
                | "html"
        )
}

/// Step 4: a path resolves to the file node that owns it, or to symbols declared inside it.
fn path_step(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Option<StepHit>, AppError> {
    if !query.contains('/') && !is_path_like(query) {
        return Ok(None);
    }
    let normalized = normalize_rel(query);
    if normalized.is_empty() {
        return Ok(None);
    }
    let basename = match normalized.rfind('/') {
        Some(slash) => &normalized[slash + 1..],
        None => normalized.as_str(),
    };
    let mut names = vec![basename];
    if let Some(extension_dot) = basename.rfind('.')
        && extension_dot > 0
    {
        names.push(&basename[..extension_dot]);
    }

    let step = ResolutionStep::Path;
    let mut seen: Vec<i64> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    for name in names {
        for node in source.find_by_name(name, kinds_of(filter))? {
            if seen.contains(&node.id) {
                continue;
            }
            if node.path != normalized && !is_under_prefix(&node.path, &normalized) {
                continue;
            }
            if !accepts_path(&node, filter) {
                continue;
            }
            seen.push(node.id);
            let is_file = node.kind == "file";
            let why = if is_file {
                format!("file \"{normalized}\"")
            } else {
                format!("symbol declared in \"{normalized}\"")
            };
            let score = step_score(step) + if is_file { PATH_FILE_BONUS } else { 0.0 };
            hits.push(hit(node, score, why));
        }
    }
    if hits.is_empty() {
        return Ok(None);
    }
    Ok(Some(StepHit { step, hits }))
}

/// Annotated and namespaced decorators both compare by their bare name.
fn decorator_name(text: &str) -> String {
    let without_at = text.strip_prefix('@').unwrap_or(text);
    let call = without_at.find('(').unwrap_or(without_at.len());
    without_at[..call].trim().to_owned()
}

/// `props.decorators` is `string[]` or `{name}[]` depending on the grammar, so both shapes are read
/// here instead of forcing one on the indexer.
fn has_decorator(node: &NodeRow, name: &str) -> bool {
    props::strings(&node.props, "decorators")
        .iter()
        .any(|entry| decorator_name(entry) == name)
}

/// Step 5: `@name` is a property filter, never a kind.
fn decorator_step(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Option<StepHit>, AppError> {
    if !query.starts_with('@') {
        return Ok(None);
    }
    let name = decorator_name(query);
    if name.is_empty() {
        return Ok(None);
    }
    let nodes: Vec<NodeRow> = source
        .find_by_decorator(&name, kinds_of(filter))?
        .into_iter()
        .filter(|node| accepts_path(node, filter))
        .collect();
    if nodes.is_empty() {
        return Ok(None);
    }
    let step = ResolutionStep::Decorator;
    let hits = nodes
        .into_iter()
        .map(|node| {
            let why = format!("property filter @{name} on \"{}\"", node.name);
            hit(node, step_score(step), why)
        })
        .collect();
    Ok(Some(StepHit { step, hits }))
}

/// Nothing matched: another symbol that shares a segment of the query is the useful answer.
fn near_miss_names(query: &str) -> Vec<String> {
    if query.starts_with('@') {
        let name = decorator_name(query);
        return if name.is_empty() {
            Vec::new()
        } else {
            vec![name]
        };
    }
    if query.contains('/') || is_path_like(query) {
        let normalized = normalize_rel(query);
        let basename = match normalized.rfind('/') {
            Some(slash) => &normalized[slash + 1..],
            None => normalized.as_str(),
        };
        return match basename.rfind('.') {
            Some(extension_dot) if extension_dot > 0 => {
                vec![basename.to_owned(), basename[..extension_dot].to_owned()]
            }
            _ => vec![basename.to_owned()],
        };
    }
    query
        .split('.')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect()
}

fn near_misses(
    source: &dyn ResolutionSource,
    query: &str,
    filter: &Filter,
) -> Result<Vec<ResolutionCandidate>, AppError> {
    let mut candidates: Vec<ResolutionCandidate> = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    for name in near_miss_names(query) {
        if candidates.len() >= CANDIDATE_CAP {
            break;
        }
        for node in source.find_by_name(&name, kinds_of(filter))? {
            if !accepts_path(&node, filter) {
                continue;
            }
            if seen.contains(&node.id) || candidates.len() >= CANDIDATE_CAP {
                continue;
            }
            seen.push(node.id);
            let why = format!("segment \"{name}\" of \"{query}\" matched a symbol");
            candidates.push(candidate_of(&hit(node, NEAR_MISS_SCORE, why)));
        }
    }
    Ok(candidates)
}

/// Turns one step's hits into an answer: a strictly highest score wins, an equal highest score is a
/// tie and becomes `Ambiguous` with all of them. Never picks on a tie.
fn decide(step_hit: StepHit) -> Resolution {
    let StepHit { step, hits } = step_hit;
    let Some(first) = hits.first() else {
        // Unreachable: `decide` is only called with at least one hit.
        return Resolution::NotFound {
            near_misses: Vec::new(),
        };
    };
    let top = hits
        .iter()
        .skip(1)
        .fold(first.score, |highest, entry| highest.max(entry.score));
    let mut winners: Vec<Hit> = hits
        .into_iter()
        .filter(|entry| entry.score == top)
        .collect();
    let candidates_total = winners.len();
    if candidates_total == 1 {
        let only = winners.remove(0);
        return Resolution::Ok {
            step,
            node: only.node,
            score: only.score,
            why: only.why,
        };
    }
    // The cap is applied after the tie is known, so the payload can say "five of twelve" instead of
    // implying the tie was five wide.
    let candidates: Vec<ResolutionCandidate> = winners
        .iter()
        .take(CANDIDATE_CAP)
        .map(candidate_of)
        .collect();
    Resolution::Ambiguous {
        step,
        candidates_shown: candidates.len(),
        candidates,
        candidates_total,
    }
}

/// Resolves one free-text query against `source`. `kinds` narrows every step and is all-or-nothing,
/// exactly like `find_by_name`: an empty or omitted list means every kind. `path` restricts every
/// step — and the near misses — to nodes under that repo-relative prefix.
///
/// Evaluation stops at the first step that produces a hit, so an exact name always wins over a
/// dotted tail and a dotted tail always wins over a path, even though a later step might also have
/// matched. A step whose only hits fall outside `path` produces no hit at all, so the filter can
/// narrow an answer and can empty it, but can never widen the detection order.
pub fn resolve(
    source: &dyn ResolutionSource,
    query: &str,
    options: &ResolveOptions,
) -> Result<Resolution, AppError> {
    let text = query.trim();
    if text.is_empty() {
        return Ok(Resolution::NotFound {
            near_misses: Vec::new(),
        });
    }
    let filter = filter_of(options);
    for step in STEPS {
        let Some(step_hit) = step(source, text, &filter)? else {
            continue;
        };
        if step_hit.hits.is_empty() {
            continue;
        }
        return Ok(decide(step_hit));
    }
    Ok(Resolution::NotFound {
        near_misses: near_misses(source, text, &filter)?,
    })
}

/// The `node.props` members resolution and expansion read, without a JSON dependency.
///
/// `props` is JSON written by the indexer, and the TypeScript parses it once per row. This crate has
/// no parser to give it in that shape, so the two members that change an answer are read here:
/// `decorators` (the step-5 property filter) and the `external`/`builtin` flags (`expand`'s
/// `external` column). Only *top-level* members are visible, which is where the indexer writes them;
/// a nested key of the same name is not a node property.
///
/// Malformed text reads as "no props" rather than raising: the raise lives with the parser the store
/// does not own (`nodes.rs` documents that boundary), and an unreadable column must not turn a
/// decorator miss into a resolution answer.
pub(crate) mod props {
    /// The raw text of one top-level member's value.
    fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
        let bytes = text.as_bytes();
        let mut at = skip_ws(text, 0);
        if bytes.get(at) != Some(&b'{') {
            return None;
        }
        at += 1;
        loop {
            at = skip_ws(text, at);
            if *bytes.get(at)? != b'"' {
                return None;
            }
            let (member, after_key) = read_string(text, at)?;
            at = skip_ws(text, after_key);
            if bytes.get(at) != Some(&b':') {
                return None;
            }
            let start = skip_ws(text, at + 1);
            let end = skip_value(text, start)?;
            if member == key {
                return Some(&text[start..end]);
            }
            at = skip_ws(text, end);
            match *bytes.get(at)? {
                b',' => at += 1,
                b'}' => return None,
                _ => return None,
            }
        }
    }

    /// A `true` member, e.g. `{"external": true}`.
    pub(crate) fn boolean(text: &str, key: &str) -> bool {
        value(text, key).map(str::trim) == Some("true")
    }

    /// The strings of an array member, whether it holds plain strings or `{name}` objects.
    pub(crate) fn strings(text: &str, key: &str) -> Vec<String> {
        let Some(raw) = value(text, key) else {
            return Vec::new();
        };
        let bytes = raw.as_bytes();
        let mut at = skip_ws(raw, 0);
        if bytes.get(at) != Some(&b'[') {
            return Vec::new();
        }
        at += 1;
        let mut found = Vec::new();
        loop {
            at = skip_ws(raw, at);
            match bytes.get(at) {
                Some(b']') | None => return found,
                Some(b',') => at += 1,
                Some(b'"') => {
                    let Some((entry, next)) = read_string(raw, at) else {
                        return found;
                    };
                    found.push(entry);
                    at = next;
                }
                Some(b'{') => {
                    let Some(end) = skip_value(raw, at) else {
                        return found;
                    };
                    if let Some(name) = value(&raw[at..end], "name").and_then(read_quoted) {
                        found.push(name);
                    }
                    at = end;
                }
                Some(_) => {
                    let Some(end) = skip_value(raw, at) else {
                        return found;
                    };
                    at = end;
                }
            }
        }
    }

    /// One standalone JSON string, e.g. the value half of `"name": "…"`.
    fn read_quoted(raw: &str) -> Option<String> {
        let at = skip_ws(raw, 0);
        read_string(raw, at).map(|(text, _)| text)
    }

    fn skip_ws(text: &str, mut at: usize) -> usize {
        let bytes = text.as_bytes();
        while matches!(bytes.get(at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            at += 1;
        }
        at
    }

    /// The index just past one JSON value, so a caller can slice it or step over it.
    fn skip_value(text: &str, at: usize) -> Option<usize> {
        let bytes = text.as_bytes();
        match *bytes.get(at)? {
            b'"' => read_string(text, at).map(|(_, next)| next),
            b'{' | b'[' => {
                let mut depth = 0_usize;
                let mut scanned = at;
                loop {
                    match *bytes.get(scanned)? {
                        b'"' => scanned = read_string(text, scanned)?.1,
                        b'{' | b'[' => {
                            depth += 1;
                            scanned += 1;
                        }
                        b'}' | b']' => {
                            depth -= 1;
                            scanned += 1;
                            if depth == 0 {
                                return Some(scanned);
                            }
                        }
                        _ => scanned += 1,
                    }
                }
            }
            _ => {
                let mut scanned = at;
                while let Some(byte) = bytes.get(scanned) {
                    if matches!(byte, b',' | b'}' | b']') || byte.is_ascii_whitespace() {
                        break;
                    }
                    scanned += 1;
                }
                Some(scanned)
            }
        }
    }

    /// One JSON string at `at`, decoded; the index just past its closing quote.
    fn read_string(text: &str, at: usize) -> Option<(String, usize)> {
        let bytes = text.as_bytes();
        if bytes.get(at) != Some(&b'"') {
            return None;
        }
        let mut decoded = String::new();
        let mut scanned = at + 1;
        loop {
            match *bytes.get(scanned)? {
                b'"' => return Some((decoded, scanned + 1)),
                b'\\' => {
                    let (ch, next) = read_escape(text, scanned)?;
                    decoded.push(ch);
                    scanned = next;
                }
                _ => {
                    let ch = text[scanned..].chars().next()?;
                    decoded.push(ch);
                    scanned += ch.len_utf8();
                }
            }
        }
    }

    fn read_escape(text: &str, at: usize) -> Option<(char, usize)> {
        let escape = *text.as_bytes().get(at + 1)?;
        let simple = |ch: char| Some((ch, at + 2));
        match escape {
            b'"' => simple('"'),
            b'\\' => simple('\\'),
            b'/' => simple('/'),
            b'b' => simple('\u{8}'),
            b'f' => simple('\u{c}'),
            b'n' => simple('\n'),
            b'r' => simple('\r'),
            b't' => simple('\t'),
            b'u' => {
                let mut code = hex4(text.as_bytes(), at + 2)?;
                let mut next = at + 6;
                // A high surrogate is half a scalar; the pair is combined when the writer emitted
                // one, and a lone half degrades to U+FFFD rather than dropping the whole string.
                if (0xd800..0xdc00).contains(&code)
                    && text.as_bytes().get(next..next + 2) == Some(b"\\u".as_slice())
                    && let Some(low) = hex4(text.as_bytes(), next + 2)
                    && (0xdc00..0xe000).contains(&low)
                {
                    code = 0x1_0000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                    next += 6;
                }
                Some((char::from_u32(code).unwrap_or('\u{fffd}'), next))
            }
            _ => None,
        }
    }

    fn hex4(bytes: &[u8], at: usize) -> Option<u32> {
        let digits = bytes.get(at..at + 4)?;
        let mut value = 0_u32;
        for digit in digits {
            value = value * 16 + (*digit as char).to_digit(16)?;
        }
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::props;

    #[test]
    fn a_decorator_array_reads_plain_and_named_entries() {
        let text = r#"{"decorators":["Injectable",{"name":"@Component({})"}]}"#;
        assert_eq!(
            props::strings(text, "decorators"),
            ["Injectable".to_owned(), "@Component({})".to_owned()]
        );
    }

    #[test]
    fn an_escaped_decorator_name_is_decoded() {
        let text = r#"{"decorators":["@Api\u0050roperty()"]}"#;
        assert_eq!(props::strings(text, "decorators"), ["@ApiProperty()"]);
    }

    #[test]
    fn absent_malformed_and_non_array_props_read_as_no_decorators() {
        for text in [
            "",
            "{}",
            "{not json",
            r#"{"decorators":"Injectable"}"#,
            "[]",
        ] {
            assert_eq!(
                props::strings(text, "decorators"),
                Vec::<String>::new(),
                "{text:?} carries no decorator array"
            );
        }
    }

    #[test]
    fn a_flag_is_read_only_at_the_top_level() {
        assert!(props::boolean(r#"{"external":true}"#, "external"));
        assert!(props::boolean(r#"{"external": true, "x": 1}"#, "external"));
        assert!(!props::boolean(r#"{"external":false}"#, "external"));
        assert!(!props::boolean(
            r#"{"nested":{"external":true}}"#,
            "external"
        ));
        assert!(!props::boolean("{}", "external"));
    }
}
