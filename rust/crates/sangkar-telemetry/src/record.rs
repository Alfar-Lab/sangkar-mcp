//! The one place a telemetry line's shape is decided (`plan.approach.md` "The record schema").
//!
//! Ported from `src/shared/telemetry/record.ts`. Every field is copied out of an explicit whitelist:
//! spreading an argument map or an envelope would let a payload value — a repo path, a symbol name —
//! reach the line through whatever a handler happened to leave on the object, and a later sink could
//! then invent a field.
//!
//! No IO and no clock: `ts` is injected so a record can be placed exactly in a test, and every
//! builder is a pure function of the context and the spec. The one value read here is
//! [`sangkar_core::runtime::runtime_version`], because it is a fact about the binary rather than
//! about the call site that a caller could resolve for us.

use serde::{Serialize, Serializer};
use serde_json::{Map, Value};

use sangkar_core::budget::budget_floor_of;
use sangkar_core::envelope::{Envelope, Status, TotalRelation, TruncationReason};
use sangkar_core::runtime::runtime_version;

/// The only signal a reader has that a field was removed or renamed. Adding a field keeps it at `1`
/// (a reader ignores the key it does not need); dropping one is a version bump.
pub const RECORD_VERSION: u8 = 1;

/// The three kinds, in the order the TypeScript module publishes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Call,
    IndexRun,
    Session,
}

/// `absent` is the one status no envelope can carry: the CLI answered successfully without a graph,
/// so there is nothing to read a status from. Folding it into the envelope's closed set keeps a
/// report matching one union rather than two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordStatus {
    Ok,
    Ambiguous,
    NotFound,
    Error,
    Bootstrapping,
    Absent,
}

impl From<Status> for RecordStatus {
    fn from(status: Status) -> Self {
        match status {
            Status::Ok => Self::Ok,
            Status::Ambiguous => Self::Ambiguous,
            Status::NotFound => Self::NotFound,
            Status::Error => Self::Error,
            Status::Bootstrapping => Self::Bootstrapping,
        }
    }
}

/// `other` is what the envelope's prose collapses to. The envelope types `stale_reason` as a plain
/// string (`src/shared/envelope.ts:51`) and `trace` writes a repo path into it, so a verbatim copy
/// would put a private path in every stale record. The envelope is untouched; this is a recording
/// rule, and the five `STALE_REASONS` values stay greppable in a report.
///
/// The five spellings mirror `sangkar_domain::freshness::STALE_REASONS`, which this crate cannot
/// import: a telemetry crate that depended on the domain crate would invert the DAG edge for one
/// closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedStaleReason {
    None,
    FilesChanged,
    RootMismatch,
    BaseCommitMoved,
    WatcherStalled,
    Other,
}

/// The mode the process started in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Mcp,
    Cli,
    Worker,
}

/// The graph file's size, bucketed so a report compares repositories without carrying a byte count
/// that would identify one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoSizeBucket {
    /// No graph file, or an empty one.
    None,
    Xs,
    S,
    M,
    L,
    Xl,
}

/// Whether the recorded root is a checkout, which decides what `base_commit_moved` may claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Git,
    Plain,
}

/// The index run's resolved mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexMode {
    Full,
    Fast,
}

/// A `Record<string, number>` in wire order.
///
/// The backing map keeps insertion order (the workspace enables serde_json's `preserve_order`),
/// which is what the TypeScript's object copy produces. A count is inserted instead of a free-form
/// value, so the map cannot carry a string or a nested object into a record.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Counts(Map<String, Value>);

impl Counts {
    #[must_use]
    pub fn new() -> Self {
        Self(Map::new())
    }

    pub fn insert(&mut self, key: impl Into<String>, count: u64) {
        self.0.insert(key.into(), Value::from(count));
    }

    /// The total the unresolved ratio's denominator adds to the unresolved count.
    #[must_use]
    pub fn sum(&self) -> u64 {
        self.0
            .values()
            .filter_map(Value::as_u64)
            .fold(0u64, |total, count| total.saturating_add(count))
    }
}

impl<K: Into<String>> FromIterator<(K, u64)> for Counts {
    fn from_iter<I: IntoIterator<Item = (K, u64)>>(iter: I) -> Self {
        let mut counts = Self::new();
        for (key, count) in iter {
            counts.insert(key, count);
        }
        counts
    }
}

/// A `number` field holding a ratio, spelled the way `JSON.stringify` spells it.
///
/// A JavaScript number has no integer type, so `JSON.stringify(1)` is `1` while serde_json's `f64`
/// writer emits `1.0` for the same double. Every ratio here is `round4`-ed first, and
/// `round4` never produces a magnitude below `0.0001`, so the decimal case spells identically in
/// both writers and the integral case is the only one that has to be written out by hand.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ratio(f64);

impl Ratio {
    #[must_use]
    pub fn value(self) -> f64 {
        self.0
    }
}

impl From<f64> for Ratio {
    fn from(value: f64) -> Self {
        Self(value)
    }
}

impl Serialize for Ratio {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // 2^53: past it the integer spelling stops round-tripping, so the float form is the honest
        // one. No ratio reaches it; the guard is what keeps the branch from being a lossy cast.
        const MAX_EXACT: f64 = 9_007_199_254_740_992.0;
        let value = self.0;
        if value.fract() == 0.0 && value.abs() <= MAX_EXACT {
            return serializer.serialize_i64(value as i64);
        }
        // NaN and the infinities have no JSON spelling; serde_json writes `null` for them, which is
        // what `JSON.stringify` writes too.
        serializer.serialize_f64(value)
    }
}

/// The facts a caller has already resolved — clock, identity, versions, machine. Building them is
/// the recorder's job (it owns the clock and the process), so a builder never reads one.
///
/// `bun_version` is deliberately absent: that field is filled by the builders from
/// [`sangkar_core::runtime::runtime_version`], because this binary has no interpreter whose version
/// a caller could resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordContext {
    pub ts: String,
    pub install_id: String,
    pub session_id: String,
    pub seq: u64,
    pub server_version: String,
    pub os: String,
    pub arch: String,
    pub graph_schema_version: u64,
}

/// One answer's facts, before the record is built.
#[derive(Clone, Debug, PartialEq)]
pub struct CallSpec {
    pub tool: String,
    /// The defaulted arguments (`ToolArgs`, `src/shared/cli/args.ts`): the budget in force is the
    /// one measured, so this is the map after the manifest default, not the request's own.
    pub args: Map<String, Value>,
    /// The request's own argument names, kept beside the defaulted `args` for `arg_names_set`.
    pub supplied_names: Vec<String>,
    pub envelope: Option<Envelope<Value>>,
    pub status: RecordStatus,
    pub error_code: Option<String>,
    pub duration_ms: u64,
}

/// One index run's facts.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexRunSpec {
    pub mode: IndexMode,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub was_forced: bool,
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub nodes_by_kind: Counts,
    pub edges_by_kind_tier: Counts,
    pub unresolved_count: u64,
    pub languages: Counts,
    pub db_size_bytes: u64,
    pub files_changed_in_run: u64,
}

/// What a process looks like at start-up; the denominator every later record is read against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionFacts {
    pub session_mode: SessionMode,
    pub is_first_run: bool,
    pub repo_size_bucket: RepoSizeBucket,
    pub languages: Vec<String>,
    pub root_kind: RootKind,
}

/// One answer, as recorded. Every field is always present so a reader never distinguishes "unknown"
/// from "omitted"; the order below is the serialisation order (`TELEMETRY.md` §4.1).
///
/// Built by [`call_record_of`] and read by the report, so the fields are public rather than private
/// with getters: a record is data on its way to a line, not a value with behaviour.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CallRecord {
    pub ts: String,
    pub v: u8,
    pub kind: Kind,
    pub install_id: String,
    pub session_id: String,
    pub seq: u64,
    pub tool: String,
    pub status: RecordStatus,
    pub error_code: Option<String>,
    pub duration_ms: u64,
    /// The names the request supplied, before defaults; `[]` means "the caller sent no arguments".
    pub arg_names_set: Vec<String>,
    /// The budget in force, i.e. after the manifest default, because that is the one measured.
    pub budget_tokens: u64,
    pub payload_tokens: u64,
    pub budget_used_ratio: Option<Ratio>,
    pub truncated: bool,
    pub truncation_reason: Option<TruncationReason>,
    pub total_relation: TotalRelation,
    pub coverage_complete: bool,
    pub coverage_indexed: u64,
    pub coverage_skipped: u64,
    pub coverage_failed: u64,
    pub unresolved_edges: u64,
    pub stale: bool,
    pub stale_reason: Option<RecordedStaleReason>,
    pub generation_age_ms: Option<i64>,
    /// Top-level payload keys whose value is an array, mapped to its length; no value is copied.
    pub result_counts: Counts,
    pub server_version: String,
    /// The field name is frozen with the v1 record shape, and the value is
    /// [`sangkar_core::runtime::runtime_version`] (`rustc <version>`) — this binary has no Bun, and
    /// `sangkar_core::runtime` documents why nothing was renamed for it.
    ///
    /// Renaming would buy nothing: no aggregate in the report reads this field
    /// (`src/shared/telemetry/report.ts:277-311`), so the runtime string never reaches a report, and
    /// dropping a key from a record is a `RECORD_VERSION` bump the parity bar forbids.
    pub bun_version: String,
    pub os: String,
    pub arch: String,
    pub graph_schema_version: u64,
}

/// One index run, as recorded. The server's own identity fields are left to `call`/`session`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IndexRunRecord {
    pub ts: String,
    pub v: u8,
    pub kind: Kind,
    pub install_id: String,
    pub session_id: String,
    pub mode: IndexMode,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub was_forced: bool,
    /// True only for a run that published: a generation id is minted on the write path alone.
    pub generation_advanced: bool,
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub nodes_by_kind: Counts,
    pub edges_by_kind_tier: Counts,
    pub unresolved_count: u64,
    pub unresolved_ratio: Ratio,
    pub languages: Counts,
    pub db_size_bytes: u64,
    pub files_changed_in_run: u64,
}

/// What a process looks like at start-up, as recorded (`TELEMETRY.md` §4.3).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionRecord {
    pub ts: String,
    pub v: u8,
    pub kind: Kind,
    pub install_id: String,
    pub session_id: String,
    pub session_mode: SessionMode,
    pub server_version: String,
    /// See [`CallRecord::bun_version`]: the name and the source are the same decision here.
    pub bun_version: String,
    pub os: String,
    pub arch: String,
    pub is_first_run: bool,
    pub repo_size_bucket: RepoSizeBucket,
    pub languages: Vec<String>,
    pub root_kind: RootKind,
}

/// The `call` record for one answer.
#[must_use]
pub fn call_record_of(ctx: &RecordContext, spec: &CallSpec) -> CallRecord {
    let env = spec.envelope.as_ref();
    let payload_tokens = env.map_or(0, budget_floor_of);
    let budget_tokens = budget_tokens_of(&spec.args);
    CallRecord {
        ts: ctx.ts.clone(),
        v: RECORD_VERSION,
        kind: Kind::Call,
        install_id: ctx.install_id.clone(),
        session_id: ctx.session_id.clone(),
        seq: ctx.seq,
        tool: spec.tool.clone(),
        status: spec.status,
        error_code: spec.error_code.clone(),
        duration_ms: spec.duration_ms,
        arg_names_set: supplied_names_of(&spec.supplied_names),
        budget_tokens,
        payload_tokens,
        budget_used_ratio: if budget_tokens == 0 {
            None
        } else {
            Some(ratio_of(payload_tokens, budget_tokens))
        },
        truncated: env.is_some_and(|env| env.truncated),
        truncation_reason: env.and_then(|env| env.truncation_reason),
        total_relation: env.map_or(TotalRelation::Gte, |env| env.totals.relation()),
        coverage_complete: env.is_some_and(|env| env.coverage.complete),
        coverage_indexed: env.map_or(0, |env| env.coverage.indexed),
        coverage_skipped: env.map_or(0, |env| env.coverage.skipped),
        coverage_failed: env.map_or(0, |env| env.coverage.failed),
        unresolved_edges: env.map_or(0, |env| env.coverage.unresolved_edges),
        stale: env.is_none_or(|env| env.read_status.stale),
        stale_reason: recorded_stale_reason_of(
            env.and_then(|env| env.read_status.stale_reason.as_deref()),
        ),
        generation_age_ms: generation_age_ms_of(
            &ctx.ts,
            env.map(|env| env.read_status.indexed_at.as_str()),
        ),
        result_counts: result_counts_of(env.and_then(|env| env.payload.as_ref())),
        server_version: ctx.server_version.clone(),
        bun_version: runtime_version(),
        os: ctx.os.clone(),
        arch: ctx.arch.clone(),
        graph_schema_version: ctx.graph_schema_version,
    }
}

/// The `index_run` record for one run.
#[must_use]
pub fn index_run_record_of(ctx: &RecordContext, spec: &IndexRunSpec) -> IndexRunRecord {
    let unresolved_total = spec
        .unresolved_count
        .saturating_add(spec.edges_by_kind_tier.sum());
    IndexRunRecord {
        ts: ctx.ts.clone(),
        v: RECORD_VERSION,
        kind: Kind::IndexRun,
        install_id: ctx.install_id.clone(),
        session_id: ctx.session_id.clone(),
        mode: spec.mode,
        exit_code: spec.exit_code,
        duration_ms: spec.duration_ms,
        was_forced: spec.was_forced,
        generation_advanced: spec.exit_code == 0,
        files_seen: spec.files_seen,
        files_indexed: spec.files_indexed,
        files_skipped: spec.files_skipped,
        files_failed: spec.files_failed,
        nodes_by_kind: spec.nodes_by_kind.clone(),
        edges_by_kind_tier: spec.edges_by_kind_tier.clone(),
        unresolved_count: spec.unresolved_count,
        unresolved_ratio: if unresolved_total == 0 {
            0.0.into()
        } else {
            ratio_of(spec.unresolved_count, unresolved_total)
        },
        languages: spec.languages.clone(),
        db_size_bytes: spec.db_size_bytes,
        files_changed_in_run: spec.files_changed_in_run,
    }
}

/// The `session` record for one process.
#[must_use]
pub fn session_record_of(ctx: &RecordContext, facts: &SessionFacts) -> SessionRecord {
    SessionRecord {
        ts: ctx.ts.clone(),
        v: RECORD_VERSION,
        kind: Kind::Session,
        install_id: ctx.install_id.clone(),
        session_id: ctx.session_id.clone(),
        session_mode: facts.session_mode,
        server_version: ctx.server_version.clone(),
        bun_version: runtime_version(),
        os: ctx.os.clone(),
        arch: ctx.arch.clone(),
        is_first_run: facts.is_first_run,
        repo_size_bucket: facts.repo_size_bucket,
        languages: facts.languages.clone(),
        root_kind: facts.root_kind,
    }
}

/// The graph file's size, bucketed so a report compares repositories without carrying a byte count
/// that would identify one. Boundaries are inclusive-low: 1 MiB is [`RepoSizeBucket::S`], not `xs`.
///
/// `0` is "no graph", the one size this crate can see for an absent file; the TypeScript also folds
/// a negative size into it, which a `u64` size cannot be.
#[must_use]
pub fn repo_size_bucket_of(bytes: u64) -> RepoSizeBucket {
    const MIB: u64 = 1024 * 1024;
    if bytes == 0 {
        RepoSizeBucket::None
    } else if bytes < MIB {
        RepoSizeBucket::Xs
    } else if bytes < 10 * MIB {
        RepoSizeBucket::S
    } else if bytes < 100 * MIB {
        RepoSizeBucket::M
    } else if bytes < 1024 * MIB {
        RepoSizeBucket::L
    } else {
        RepoSizeBucket::Xl
    }
}

/// The recorded root kind: `git` when `<repoRoot>/.git` exists, else `plain`.
#[must_use]
pub fn root_kind_of(has_git_marker: bool) -> RootKind {
    if has_git_marker {
        RootKind::Git
    } else {
        RootKind::Plain
    }
}

/// `parseArgs` fills every declared default (`src/shared/cli/args.ts:200-211`), so the argument map's
/// keys here would report the schema rather than the request — a bare `status` would record
/// `["budget","paths"]` and the field would stop answering the one question it exists for.
fn supplied_names_of(names: &[String]) -> Vec<String> {
    let mut kept = names.to_vec();
    kept.sort();
    kept.dedup();
    kept
}

/// Deliberately the defaulted value, not the supplied one: the budget in force is what a ratio has
/// to be measured against, and a tool without a `budget` argument records `0`.
///
/// A JSON `3200.0` is the same budget as `3200`; the TypeScript's `typeof budget === "number"` does
/// not distinguish the two either.
fn budget_tokens_of(args: &Map<String, Value>) -> u64 {
    let Some(Value::Number(number)) = args.get("budget") else {
        return 0;
    };
    if let Some(budget) = number.as_u64() {
        return budget;
    }
    match number.as_f64() {
        Some(value) if value >= 0.0 => value as u64,
        _ => 0,
    }
}

/// `round4(payload / budget)`: a 4-decimal ratio, the report's precision.
fn ratio_of(part: u64, whole: u64) -> Ratio {
    round4(part as f64 / whole as f64).into()
}

/// Four decimals is the report's precision; more would only describe float noise.
///
/// `f64::round` rounds half away from zero where `Math.round` rounds half towards positive
/// infinity; every ratio rounded here is non-negative, which is the range where the two agree.
fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// The envelope's string is not a value we may keep. A `STALE_REASONS` member passes through;
/// anything else — prose, and `trace` writes a repo path into it — becomes `other`.
fn recorded_stale_reason_of(reason: Option<&str>) -> Option<RecordedStaleReason> {
    match reason {
        None => None,
        Some("none") => Some(RecordedStaleReason::None),
        Some("files_changed") => Some(RecordedStaleReason::FilesChanged),
        Some("root_mismatch") => Some(RecordedStaleReason::RootMismatch),
        Some("base_commit_moved") => Some(RecordedStaleReason::BaseCommitMoved),
        Some("watcher_stalled") => Some(RecordedStaleReason::WatcherStalled),
        Some(_) => Some(RecordedStaleReason::Other),
    }
}

/// `Date.parse(ts) - Date.parse(indexed_at)`, and `null` when either is not the form this workspace
/// writes.
fn generation_age_ms_of(ts: &str, indexed_at: Option<&str>) -> Option<i64> {
    let indexed_at = indexed_at?;
    let recorded = parse_iso8601_millis(ts)?;
    let indexed = parse_iso8601_millis(indexed_at)?;
    Some(recorded - indexed)
}

/// Lengths survive, values do not: a payload string never reaches the line from here.
fn result_counts_of(payload: Option<&Value>) -> Counts {
    let mut counts = Counts::new();
    match payload {
        Some(Value::Object(map)) => {
            for (key, value) in map {
                if let Some(items) = value.as_array() {
                    counts.insert(key.as_str(), items.len() as u64);
                }
            }
        }
        // `Object.entries` of an array is its indices, so an array payload counts the array-valued
        // members of its own elements. No tool returns one; the port copies the rule rather than
        // assuming a payload shape the TypeScript never checked.
        Some(Value::Array(items)) => {
            for (index, value) in items.iter().enumerate() {
                if let Some(members) = value.as_array() {
                    counts.insert(index.to_string(), members.len() as u64);
                }
            }
        }
        _ => {}
    }
    counts
}

/// The epoch milliseconds an ISO-8601 UTC timestamp names, or `None` when it is not the form this
/// workspace writes (`YYYY-MM-DDTHH:MM:SS[.fff]Z`).
///
/// Only the `Z` form is accepted. The TypeScript uses `Date.parse`, which also accepts offset forms
/// and a zone-less string as *local* time — the second is a host-dependent answer no stored value
/// should be interpreted through, so it is rejected instead of guessed at.
///
/// Written by hand for the same reason `sangkar_core::log` and `sangkar_domain::freshness` write
/// their own copies: the crate set carries no date library, and the shared parser lives in a module
/// of another crate that this crate may not depend on and does not own. A `sangkar_core` time
/// helper would retire every copy of this arithmetic.
fn parse_iso8601_millis(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[10] != b'T'
    {
        return None;
    }
    let year = parse_digits(bytes.get(0..4)?)?;
    let month = parse_digits(bytes.get(5..7)?)?;
    let day = parse_digits(bytes.get(8..10)?)?;
    let hour = parse_digits(bytes.get(11..13)?)?;
    let minute = parse_digits(bytes.get(14..16)?)?;
    let second = parse_digits(bytes.get(17..19)?)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut rest = bytes.get(19..)?;
    let mut millis = 0i64;
    if rest.first() == Some(&b'.') {
        rest = rest.get(1..)?;
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let kept = digits.min(3);
        let value = parse_digits(rest.get(..kept)?)?;
        millis = value * 10i64.pow(u32::try_from(3 - kept).unwrap_or(0));
        rest = rest.get(digits..)?;
    }
    if rest != b"Z" && rest != b"z" {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

/// A run of ASCII digits as an integer; any other byte, or an empty slice, is `None`.
fn parse_digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(i64::from(byte.saturating_sub(b'0')))?;
    }
    Some(value)
}

/// Howard Hinnant's `days_from_civil`: the inverse of the formatter every crate in this workspace
/// carries a private copy of.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}
