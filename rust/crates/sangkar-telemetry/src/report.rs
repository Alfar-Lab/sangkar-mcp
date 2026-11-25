//! The report's two halves: the reader that turns a telemetry directory into records, and the fold
//! that turns records into the numbers the page prints.
//!
//! Ported from `src/shared/telemetry/report.ts`. Both halves are pure in the sense a report needs:
//! [`read_records`] touches the disk and nothing else, and [`aggregate`] takes records that are
//! already in memory, with no clock, no environment and no injection point. The one clock reading
//! the whole feature takes is `now`, a parameter of [`parse_since`].
//!
//! Two failures are expected here and neither is fatal. A process killed between two lines of an
//! append leaves a torn final line, so an unparseable line costs one counter and nothing else; and
//! `record.rs` keeps `v: 1` while a field is *added* and bumps it only when one is removed or
//! renamed, so a line stamped with another version is a record this reader must not guess at.
//!
//! A record read back is not one of [`crate::record`]'s builders: those types derive `Serialize`
//! alone, and the field table below — copied verbatim from the TypeScript — is what decides whether
//! a line is a record at all. The table is the contract; the structs beside it are only how each
//! field is read once the table has accepted the line.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::Path;

use chrono::{DateTime, Days, NaiveDate, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::{Kind, RECORD_VERSION};

/// One telemetry directory read back, folded in the same call.
///
/// `records` is the vector rather than its length: the machine-wide page folds every section's
/// records into one total (`TELEMETRY.md` §6.1), and a count cannot be re-folded. The count itself is
/// `data.totals.records`, which is this vector's length after the `since` filter.
#[derive(Clone, Debug, PartialEq)]
pub struct TelemetryReadResult {
    /// Daily files listed, whether or not they could be read. A file deleted between the listing and
    /// the read is counted here and contributes no line, which is what the TypeScript does too.
    pub files: usize,
    pub records: Vec<TelemetryRecord>,
    /// Lines that were not v1 records: not JSON, not an object, another version, an unknown `kind`,
    /// or missing a field the table below requires.
    pub unparsed: usize,
    pub data: ReportData,
}

/// One record, by kind. The payloads are the wire shape this module reads, not the recorder's
/// builders: see the module documentation for why the two are separate types.
#[derive(Clone, Debug, PartialEq)]
pub enum TelemetryRecord {
    Call(ReadCall),
    IndexRun(ReadIndexRun),
    Session(ReadSession),
}

impl TelemetryRecord {
    #[must_use]
    pub fn ts(&self) -> &str {
        match self {
            Self::Call(record) => &record.ts,
            Self::IndexRun(record) => &record.ts,
            Self::Session(record) => &record.ts,
        }
    }

    #[must_use]
    pub fn install_id(&self) -> &str {
        match self {
            Self::Call(record) => &record.install_id,
            Self::IndexRun(record) => &record.install_id,
            Self::Session(record) => &record.install_id,
        }
    }

    /// The canonical three, reused from the recorder rather than declared a second time.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Call(_) => Kind::Call,
            Self::IndexRun(_) => Kind::IndexRun,
            Self::Session(_) => Kind::Session,
        }
    }
}

/// One answer, as the report reads it (`TELEMETRY.md` §4.1). Fields the report never reads are not
/// carried: `bun_version` is parsed and dropped, and `budget_tokens` only reaches a line so that the
/// ratio written beside it can be explained.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ReadCall {
    pub ts: String,
    pub install_id: String,
    pub tool: String,
    pub status: String,
    pub error_code: Option<String>,
    pub duration_ms: f64,
    /// The names the request supplied, before defaults.
    pub arg_names_set: Vec<String>,
    pub budget_used_ratio: Option<f64>,
    pub truncated: bool,
    pub truncation_reason: Option<String>,
    /// `"eq"` or `"gte"`; anything else counts in neither total, as in the TypeScript.
    pub total_relation: String,
    pub stale: bool,
    pub unresolved_edges: f64,
}

/// One index run, as the report reads it (`TELEMETRY.md` §4.2).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ReadIndexRun {
    pub ts: String,
    pub install_id: String,
    /// Absent from the required-field table, so a run without one is still a record; it is only a
    /// tie-break in the run table's order.
    pub session_id: Option<String>,
    pub mode: String,
    pub exit_code: i64,
    pub duration_ms: f64,
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub files_changed_in_run: u64,
    pub unresolved_ratio: f64,
    pub db_size_bytes: u64,
    pub generation_advanced: bool,
}

/// One process start, as the report reads it (`TELEMETRY.md` §4.3).
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct ReadSession {
    pub ts: String,
    pub install_id: String,
    pub session_mode: String,
    pub repo_size_bucket: String,
    pub root_kind: String,
}

/// The window header's half of the page: the counts that describe the whole read rather than a tool.
///
/// Field order is the serialisation order the TypeScript publishes; nothing in the page depends on
/// it, but a diff of two reports does.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Totals {
    pub records: u64,
    pub calls: u64,
    pub index_runs: u64,
    pub sessions: u64,
    pub unparsed: u64,
    pub install_ids: u64,
    pub days: u64,
}

/// The four latency numbers of one tool, over that tool's call durations.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DurationStats {
    pub p50: f64,
    pub p95: f64,
    pub mean: f64,
    pub max: f64,
}

/// One row of the per-tool table. `tools` is ordered by tool name.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ToolStats {
    pub tool: String,
    pub calls: u64,
    /// Status and error-code counts, ordered by key.
    pub statuses: BTreeMap<String, u64>,
    pub error_codes: BTreeMap<String, u64>,
    pub duration_ms: DurationStats,
    pub truncated: u64,
    /// `None` when no call of this tool carried a budget to measure against.
    pub mean_budget_used_ratio: Option<f64>,
    pub eq: u64,
    pub gte: u64,
}

/// The truncation panel. `reasons` is ordered by key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TruncationSummary {
    pub count: u64,
    pub rate: f64,
    pub reasons: BTreeMap<String, u64>,
}

/// The two total relations, and how much of the window answered exactly.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Relations {
    pub eq: u64,
    pub gte: u64,
    pub eq_ratio: f64,
}

/// What the window knew about the graph when each call ran.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Coverage {
    pub stale_calls: u64,
    pub mean_unresolved_edges: f64,
}

/// One argument name and how many calls supplied it. `arg_names` is ordered by count descending,
/// then by name ascending, so a tie does not depend on the read order.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ArgNameCount {
    pub name: String,
    pub calls: u64,
}

/// One row of the index-run table. `index_runs` is chronological.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IndexRunRow {
    pub ts: String,
    pub mode: String,
    pub exit_code: i64,
    pub duration_ms: f64,
    pub files_seen: u64,
    pub files_indexed: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub files_changed_in_run: u64,
    pub unresolved_ratio: f64,
    pub db_size_bytes: u64,
    pub generation_advanced: bool,
}

/// One day of the unresolved trend. `unresolved_trend` is ordered by date, which is the label and
/// the sort key at once.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TrendPoint {
    pub date: String,
    /// The mean of that day's runs' `unresolved_ratio`, rounded to four decimals.
    pub ratio: f64,
    pub runs: u64,
}

/// The session panel: three independent breakdowns of the same records, each ordered by key.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionBreakdown {
    pub by_mode: BTreeMap<String, u64>,
    pub by_root_kind: BTreeMap<String, u64>,
    pub by_size_bucket: BTreeMap<String, u64>,
}

/// The header's dates. `first_ts`/`last_ts` compare the timestamps as strings, exactly as the
/// TypeScript does: every record is written in one UTC spelling, so the lexicographic order is the
/// chronological one, and no parse can fail on the way to a label.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Window {
    /// The `--since` value as the caller spelled it (`"7d"`, `"2026-11-01"`), not the date it floored
    /// to: the page labels the window with what was asked for.
    pub since: Option<String>,
    pub first_ts: Option<String>,
    pub last_ts: Option<String>,
}

/// Everything the page renders, and nothing else.
///
/// Four orders are part of the contract rather than of the map implementation: `tools` by name, the
/// three mixes and the session breakdowns by key, `arg_names` by count then name, and
/// `index_runs`/`unresolved_trend` chronologically. Two folds over the same records serialise to the
/// same bytes.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ReportData {
    pub totals: Totals,
    pub tools: Vec<ToolStats>,
    pub status_mix: BTreeMap<String, u64>,
    pub error_mix: BTreeMap<String, u64>,
    pub truncation: TruncationSummary,
    pub relations: Relations,
    pub coverage: Coverage,
    pub arg_names: Vec<ArgNameCount>,
    pub index_runs: Vec<IndexRunRow>,
    pub unresolved_trend: Vec<TrendPoint>,
    pub sessions: SessionBreakdown,
    pub window: Window,
}

/// A `--since` window: the value the caller typed, and the UTC midnight it floored to.
///
/// Every date in telemetry turns over at UTC midnight (`identity::utc_date_of`), so the bound is a
/// day rather than an instant, and a record stamped inside that day is in the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Since {
    raw: String,
    at: DateTime<Utc>,
}

impl Since {
    /// The value as the caller spelled it, which is what the page labels the window with.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The bound itself: UTC midnight of the day the window starts on.
    #[must_use]
    pub fn at(&self) -> DateTime<Utc> {
        self.at
    }
}

/// A `--since` value outside the two accepted forms.
///
/// Not an [`sangkar_core::errors::AppError`]: the twelve codes describe a call boundary, and this is
/// the argv — `sangkar-cli` keeps its own usage refusal separate for the same reason
/// (`crates/sangkar-cli/src/args.rs`). The CLI prints this message, then the verb's usage block, and
/// exits 2 (`TELEMETRY.md` §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinceError {
    raw: String,
}

impl SinceError {
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl std::fmt::Display for SinceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "--since must be YYYY-MM-DD or <n>d, not \"{}\"",
            self.raw
        )
    }
}

impl std::error::Error for SinceError {}

const DATE_LENGTH: usize = 10;

/// `YYYY-MM-DD` is a calendar day, `Nd` is "N days before `now`", and both floor to UTC midnight.
///
/// The two forms are the whole set: the verb's help offers `--since <YYYY-MM-DD|Nd>` and nothing
/// else, so `24h`, `7` and `1w` are refused rather than guessed at.
pub fn parse_since(raw: &str, now: DateTime<Utc>) -> Result<Since, SinceError> {
    if let Some(at) = utc_midnight_of(raw) {
        return Ok(Since {
            raw: raw.to_owned(),
            at,
        });
    }
    let refusal = || SinceError {
        raw: raw.to_owned(),
    };
    let days = days_of(raw).ok_or_else(refusal)?;
    // The offset is applied to the instant and the result floored to the UTC day, so `7d` at 13:45
    // reaches back seven days and then to that morning: the window is whole days, not a rolling one.
    let shifted = now
        .checked_sub_days(Days::new(u64::from(days)))
        .ok_or_else(refusal)?;
    Ok(Since {
        raw: raw.to_owned(),
        at: start_of_utc_day(shifted),
    })
}

/// The shape test the CLI runs before any IO, so a refusal leaves no directory touched.
#[must_use]
pub fn is_since_literal(raw: &str) -> bool {
    utc_midnight_of(raw).is_some() || days_of(raw).is_some()
}

/// The turn of the day is UTC everywhere else in telemetry, so it is here. A `--since` date is read
/// as a calendar day, not as an instant: there is no time of day to supply.
fn utc_midnight_of(raw: &str) -> Option<DateTime<Utc>> {
    if !is_date_literal(raw) {
        return None;
    }
    let year: i32 = raw.get(0..4)?.parse().ok()?;
    let month: u32 = raw.get(5..7)?.parse().ok()?;
    let day: u32 = raw.get(8..10)?.parse().ok()?;
    // JavaScript reads a year below 100 as 19xx, so `Date.UTC` there answers a different day than the
    // one written and the TypeScript's round-trip check refuses it. The same floor keeps the refusal
    // identical instead of accepting a year this calendar would have dated.
    if year < 100 {
        return None;
    }
    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    Some(date.and_time(NaiveTime::MIN).and_utc())
}

/// `^\d{4}-\d{2}-\d{2}$`: four digits, a dash, two digits, a dash, two digits, ASCII only.
fn is_date_literal(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() == DATE_LENGTH
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            4 | 7 => *byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

/// `None` for anything that is not `Nd` with an integer N >= 1 (`0d` would mean "today"). A count too
/// large for a calendar is a refusal rather than a silent "no filter".
fn days_of(raw: &str) -> Option<u32> {
    let digits = raw.strip_suffix('d')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse::<u32>().ok().filter(|days| *days >= 1)
}

fn start_of_utc_day(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive().and_time(NaiveTime::MIN).and_utc()
}

/// The `<dir>/<date>.jsonl` layout the sink writes, read back and folded.
///
/// Only daily files are records, and the name is the file's date rather than the line's, so the read
/// order below is a stable starting order only — nothing aggregated depends on it.
#[must_use]
pub fn read_records(dir: &Path, since: Option<Since>) -> TelemetryReadResult {
    let files = jsonl_files_of(dir);
    let window = since.as_ref();
    let mut records: Vec<TelemetryRecord> = Vec::new();
    let mut unparsed = 0;

    for file in &files {
        // A file deleted or unreadable between the listing and the read: a report must not fail
        // because the directory was rotated underneath it, and a file has no line to count. Bytes
        // rather than a `&str` read, because a torn byte must cost one line, not the whole file.
        let Ok(bytes) = fs::read(dir.join(file)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        for line in lines_of(&text) {
            let Some(record) = record_of(line) else {
                unparsed += 1;
                continue;
            };
            if window.is_some_and(|window| is_before(record.ts(), window)) {
                continue;
            }
            records.push(record);
        }
    }

    let data = aggregate_with(&records, unparsed, since.as_ref().map(Since::raw));
    TelemetryReadResult {
        files: files.len(),
        records,
        unparsed,
        data,
    }
}

/// A missing directory is "nothing recorded yet", not an error: the first run has no file.
fn jsonl_files_of(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".jsonl"))
        .collect();
    files.sort();
    files
}

/// The trailing empty element is the newline an append always ends with, and it is not a line. Every
/// other empty string is one — a zero-line append (or a killed process) leaves exactly that — and it
/// falls out as unparsed because it is not JSON.
fn lines_of(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Whether a record's own timestamp falls before the window.
///
/// An unreadable `ts` is kept: `Date.parse` answers `NaN` there, and `NaN < bound` is false, so the
/// TypeScript neither drops the record nor fails the read.
fn is_before(ts: &str, window: &Since) -> bool {
    DateTime::parse_from_rfc3339(ts).is_ok_and(|at| at.to_utc() < window.at)
}

/// The five skip clauses. A line is dropped — counted in `unparsed` and folded into no ratio and no
/// count — when it is not JSON, not an object, at another `v`, of an unknown `kind`, or missing a
/// field this module reads.
fn record_of(line: &str) -> Option<TelemetryRecord> {
    let value: Value = serde_json::from_str(line).ok()?;
    // Not an object at all: no key to read, so the two clauses below would only fail slower.
    if !value.is_object() {
        return None;
    }
    if value.get("v").and_then(Value::as_f64) != Some(f64::from(RECORD_VERSION)) {
        return None;
    }
    match value.get("kind").and_then(Value::as_str) {
        Some("call") if has_fields(&value, CALL_FIELDS) => serde_json::from_value(value)
            .ok()
            .map(TelemetryRecord::Call),
        Some("index_run") if has_fields(&value, INDEX_RUN_FIELDS) => serde_json::from_value(value)
            .ok()
            .map(TelemetryRecord::IndexRun),
        Some("session") if has_fields(&value, SESSION_FIELDS) => serde_json::from_value(value)
            .ok()
            .map(TelemetryRecord::Session),
        _ => None,
    }
}

/// The primitive a required field is checked as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldType {
    Str,
    Num,
    Bool,
    List,
    Map,
}

/// The required-field list per kind, copied verbatim from the TypeScript's `record.ts`-derived
/// table: the fields the report reads, checked as a primitive.
///
/// This is deliberately not a schema validator. A nullable field is absent because `null` is a legal
/// value there (the read structs make those `Option`), and a field no report touches is absent
/// because a reader ignores what it does not need. A line that satisfies the table and still cannot
/// be read as its kind's struct is counted unparsed rather than coerced into a count or a ratio.
const CALL_FIELDS: &[(&str, FieldType)] = &[
    ("ts", FieldType::Str),
    ("kind", FieldType::Str),
    ("install_id", FieldType::Str),
    ("tool", FieldType::Str),
    ("status", FieldType::Str),
    ("duration_ms", FieldType::Num),
    ("arg_names_set", FieldType::List),
    ("budget_tokens", FieldType::Num),
    ("payload_tokens", FieldType::Num),
    ("truncated", FieldType::Bool),
    ("total_relation", FieldType::Str),
    ("coverage_complete", FieldType::Bool),
    ("coverage_indexed", FieldType::Num),
    ("coverage_skipped", FieldType::Num),
    ("coverage_failed", FieldType::Num),
    ("unresolved_edges", FieldType::Num),
    ("stale", FieldType::Bool),
    ("result_counts", FieldType::Map),
];

const INDEX_RUN_FIELDS: &[(&str, FieldType)] = &[
    ("ts", FieldType::Str),
    ("kind", FieldType::Str),
    ("install_id", FieldType::Str),
    ("mode", FieldType::Str),
    ("exit_code", FieldType::Num),
    ("duration_ms", FieldType::Num),
    ("generation_advanced", FieldType::Bool),
    ("files_seen", FieldType::Num),
    ("files_indexed", FieldType::Num),
    ("files_skipped", FieldType::Num),
    ("files_failed", FieldType::Num),
    ("files_changed_in_run", FieldType::Num),
    ("unresolved_ratio", FieldType::Num),
    ("languages", FieldType::Map),
    ("db_size_bytes", FieldType::Num),
];

const SESSION_FIELDS: &[(&str, FieldType)] = &[
    ("ts", FieldType::Str),
    ("kind", FieldType::Str),
    ("install_id", FieldType::Str),
    ("session_mode", FieldType::Str),
    ("is_first_run", FieldType::Bool),
    ("repo_size_bucket", FieldType::Str),
    ("root_kind", FieldType::Str),
];

fn has_fields(value: &Value, spec: &[(&str, FieldType)]) -> bool {
    spec.iter()
        .all(|(name, kind)| has_type(value.get(*name), *kind))
}

fn has_type(value: Option<&Value>, kind: FieldType) -> bool {
    match kind {
        FieldType::Str => value.is_some_and(Value::is_string),
        FieldType::Num => value.is_some_and(Value::is_number),
        FieldType::Bool => value.is_some_and(Value::is_boolean),
        FieldType::List => value.is_some_and(Value::is_array),
        FieldType::Map => value.is_some_and(Value::is_object),
    }
}

/// The fold, for a caller holding records and nothing else: no unparsed line, no window label.
///
/// The reader's own result carries the other two facts, so it calls [`aggregate_with`] instead; both
/// produce the same numbers for the same records.
#[must_use]
pub fn aggregate(records: &[TelemetryRecord]) -> ReportData {
    aggregate_with(records, 0, None)
}

/// The fold. Everything that leaves here is ordered by its own keys — `tools` by name, every mix by
/// key, `arg_names` by count then name, the runs and the trend chronologically — and every derived
/// ratio passes through [`round4`], so two folds over the same records serialise to the same bytes.
/// A run row's `unresolved_ratio` is the one ratio copied as recorded, not re-rounded.
///
/// `unparsed` and `since` are the two facts a record list does not carry: a section of the
/// machine-wide page sums the first over its directories and labels itself with the second.
#[must_use]
pub fn aggregate_with(
    records: &[TelemetryRecord],
    unparsed: usize,
    since: Option<&str>,
) -> ReportData {
    let mut by_tool: BTreeMap<String, ToolAccumulator> = BTreeMap::new();
    let mut status_mix: Tally = BTreeMap::new();
    let mut error_mix: Tally = BTreeMap::new();
    let mut truncation_reasons: Tally = BTreeMap::new();
    let mut arg_counts: Tally = BTreeMap::new();
    let mut truncated_total = 0;
    let mut eq_total = 0;
    let mut gte_total = 0;
    let mut stale_calls = 0;
    let mut unresolved_total = 0.0;
    let mut calls_count = 0;

    for record in records {
        let TelemetryRecord::Call(call) = record else {
            continue;
        };
        calls_count += 1;
        let tool = by_tool.entry(call.tool.clone()).or_default();
        tool.durations.push(call.duration_ms);
        bump(&mut tool.statuses, &call.status);
        bump(&mut status_mix, &call.status);
        if let Some(code) = call.error_code.as_deref() {
            bump(&mut tool.error_codes, code);
            bump(&mut error_mix, code);
        }
        if let Some(ratio) = call.budget_used_ratio {
            tool.ratios.push(ratio);
        }
        if call.truncated {
            truncated_total += 1;
            tool.truncated += 1;
            if let Some(reason) = call.truncation_reason.as_deref() {
                bump(&mut truncation_reasons, reason);
            }
        }
        match call.total_relation.as_str() {
            "eq" => {
                eq_total += 1;
                tool.eq += 1;
            }
            "gte" => {
                gte_total += 1;
                tool.gte += 1;
            }
            _ => {}
        }
        if call.stale {
            stale_calls += 1;
        }
        unresolved_total += call.unresolved_edges;
        for name in &call.arg_names_set {
            bump(&mut arg_counts, name);
        }
    }

    let tools: Vec<ToolStats> = by_tool
        .into_iter()
        .map(|(tool, accumulator)| tool_stats_of(tool, accumulator))
        .collect();

    let mut runs: Vec<&ReadIndexRun> = records
        .iter()
        .filter_map(|record| match record {
            TelemetryRecord::IndexRun(run) => Some(run),
            _ => None,
        })
        .collect();
    runs.sort_by(|left, right| compare_runs(left, right));

    let mut trend_by_day: BTreeMap<&str, TrendAccumulator> = BTreeMap::new();
    for run in &runs {
        let entry = trend_by_day.entry(day_of(&run.ts)).or_default();
        entry.total += run.unresolved_ratio;
        entry.runs += 1;
    }

    let mut by_mode: Tally = BTreeMap::new();
    let mut by_root_kind: Tally = BTreeMap::new();
    let mut by_size_bucket: Tally = BTreeMap::new();
    let mut sessions_count = 0;
    for record in records {
        let TelemetryRecord::Session(session) = record else {
            continue;
        };
        sessions_count += 1;
        bump(&mut by_mode, &session.session_mode);
        bump(&mut by_root_kind, &session.root_kind);
        bump(&mut by_size_bucket, &session.repo_size_bucket);
    }

    // `install_id` groups records from one host-day without being kept: only the count leaves here.
    let mut install_ids: HashSet<&str> = HashSet::new();
    let mut record_days: HashSet<&str> = HashSet::new();
    let mut first_ts: Option<&str> = None;
    let mut last_ts: Option<&str> = None;
    for record in records {
        install_ids.insert(record.install_id());
        record_days.insert(day_of(record.ts()));
        let ts = record.ts();
        if first_ts.is_none_or(|first| ts < first) {
            first_ts = Some(ts);
        }
        if last_ts.is_none_or(|last| ts > last) {
            last_ts = Some(ts);
        }
    }

    let mut arg_names: Vec<ArgNameCount> = arg_counts
        .into_iter()
        .map(|(name, calls)| ArgNameCount { name, calls })
        .collect();
    arg_names.sort_by(|left, right| {
        right
            .calls
            .cmp(&left.calls)
            .then_with(|| left.name.cmp(&right.name))
    });

    let unresolved_trend: Vec<TrendPoint> = trend_by_day
        .into_iter()
        .map(|(date, entry)| TrendPoint {
            date: date.to_owned(),
            ratio: round4(entry.total / entry.runs as f64),
            runs: entry.runs,
        })
        .collect();

    ReportData {
        totals: Totals {
            records: records.len() as u64,
            calls: calls_count,
            index_runs: runs.len() as u64,
            sessions: sessions_count,
            unparsed: unparsed as u64,
            install_ids: install_ids.len() as u64,
            days: record_days.len() as u64,
        },
        tools,
        status_mix,
        error_mix,
        truncation: TruncationSummary {
            count: truncated_total,
            rate: share_or_zero(truncated_total, calls_count),
            reasons: truncation_reasons,
        },
        relations: Relations {
            eq: eq_total,
            gte: gte_total,
            eq_ratio: share_or_zero(eq_total, calls_count),
        },
        coverage: Coverage {
            stale_calls,
            mean_unresolved_edges: if calls_count == 0 {
                0.0
            } else {
                round4(unresolved_total / calls_count as f64)
            },
        },
        arg_names,
        index_runs: runs.iter().map(|run| index_run_row_of(run)).collect(),
        unresolved_trend,
        sessions: SessionBreakdown {
            by_mode,
            by_root_kind,
            by_size_bucket,
        },
        window: Window {
            since: since.map(str::to_owned),
            first_ts: first_ts.map(str::to_owned),
            last_ts: last_ts.map(str::to_owned),
        },
    }
}

/// A `Record<string, number>` in the order the report publishes it: by key.
type Tally = BTreeMap<String, u64>;

#[derive(Debug, Default)]
struct ToolAccumulator {
    durations: Vec<f64>,
    ratios: Vec<f64>,
    statuses: Tally,
    error_codes: Tally,
    truncated: u64,
    eq: u64,
    gte: u64,
}

#[derive(Debug, Default)]
struct TrendAccumulator {
    total: f64,
    runs: u64,
}

fn tool_stats_of(tool: String, mut accumulator: ToolAccumulator) -> ToolStats {
    let mut durations = std::mem::take(&mut accumulator.durations);
    durations.sort_by(f64::total_cmp);
    ToolStats {
        tool,
        calls: durations.len() as u64,
        statuses: accumulator.statuses,
        error_codes: accumulator.error_codes,
        duration_ms: DurationStats {
            p50: percentile(&durations, 0.5),
            p95: percentile(&durations, 0.95),
            mean: round4(mean_of(&durations)),
            max: max_of(&durations),
        },
        truncated: accumulator.truncated,
        mean_budget_used_ratio: if accumulator.ratios.is_empty() {
            None
        } else {
            Some(round4(mean_of(&accumulator.ratios)))
        },
        eq: accumulator.eq,
        gte: accumulator.gte,
    }
}

/// Chronological, with a tie-break that never leans on the read order: two runs recorded inside the
/// same millisecond would otherwise swap places between two reads. A run without a `session_id`
/// sorts as the empty string, which keeps the order total where the TypeScript's comparator
/// answered 1 for `undefined`.
fn compare_runs(left: &ReadIndexRun, right: &ReadIndexRun) -> std::cmp::Ordering {
    left.ts
        .cmp(&right.ts)
        .then_with(|| {
            left.session_id
                .as_deref()
                .unwrap_or_default()
                .cmp(right.session_id.as_deref().unwrap_or_default())
        })
        .then_with(|| left.exit_code.cmp(&right.exit_code))
}

fn index_run_row_of(run: &ReadIndexRun) -> IndexRunRow {
    IndexRunRow {
        ts: run.ts.clone(),
        mode: run.mode.clone(),
        exit_code: run.exit_code,
        duration_ms: run.duration_ms,
        files_seen: run.files_seen,
        files_indexed: run.files_indexed,
        files_skipped: run.files_skipped,
        files_failed: run.files_failed,
        files_changed_in_run: run.files_changed_in_run,
        unresolved_ratio: run.unresolved_ratio,
        db_size_bytes: run.db_size_bytes,
        generation_advanced: run.generation_advanced,
    }
}

/// Nearest-rank over an ascending array: no interpolation, so a report reproduces by hand. The rank
/// is `ceil(q * n)`, clamped into the array.
fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    let Some(last) = sorted.len().checked_sub(1) else {
        return 0.0;
    };
    let rank = (quantile * sorted.len() as f64).ceil().max(1.0) as usize - 1;
    sorted.get(rank.min(last)).copied().unwrap_or(0.0)
}

fn mean_of(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut total = 0.0;
    for value in values {
        total += value;
    }
    total / values.len() as f64
}

/// The last element of an ascending array; `0` for an empty one.
fn max_of(values: &[f64]) -> f64 {
    values.last().copied().unwrap_or(0.0)
}

fn bump(tally: &mut Tally, key: &str) {
    *tally.entry(key.to_owned()).or_insert(0) += 1;
}

/// ISO-8601 in UTC puts the UTC date in the first ten bytes, which is the whole group key.
fn day_of(ts: &str) -> &str {
    ts.get(..10).unwrap_or(ts)
}

/// Four decimals is the report's precision; more would only describe float noise.
///
/// `f64::round` breaks a tie away from zero and the JavaScript `Math.round` breaks it upwards; every
/// value rounded here is a duration, a ratio or a mean of those, and none of them is negative, so the
/// two agree.
fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

fn share_or_zero(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        round4(part as f64 / whole as f64)
    }
}
