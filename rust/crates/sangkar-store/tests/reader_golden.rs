//! Reader goldens: the Rust [`GraphReader`] against the TypeScript reader's own answers, method by
//! method, from **one database file**.
//!
//! `scripts/parity/capture-reader-dump.ts` indexes the checked-in fixtures with the TypeScript
//! indexer, calls the TypeScript reader, and writes `rust/tests/golden/reader/<fixture>.json` —
//! one key per reader method, its arguments under `target.*`, and every value in the same
//! canonical text this test emits. This test opens the same database, replays those arguments
//! through [`SqliteReader`], renders the identical text, and reports the first structural
//! difference by method and element. Nothing here is normalized on one side only: both writers are
//! hand-rolled and agree on compact JSON, declared key order, omitted `null` fields and every
//! list's query order.
//!
//! ## The `SANGKAR_FIXTURE_DB` contract
//!
//! The *goldens* are committed; the *databases* are not (they live under
//! `target/parity-reader-db/<fixture>/graph.db`, `target/` is gitignored). The test therefore takes
//! one environment variable:
//!
//! ```text
//! SANGKAR_FIXTURE_DB=<absolute path to a graph.db>
//!   cargo test --manifest-path rust/Cargo.toml -p sangkar-store --test reader_golden -- --nocapture
//! ```
//!
//! `capture-reader-dump.ts` prints that exact command once per fixture when it finishes. Unset, the
//! test prints how to get one and passes: a reader that answers correctly is not a property of a
//! machine without the capture, and a failing suite for a missing artifact teaches nothing.
//!
//! The file is opened **read-only**. A sibling `graph.db-wal` (a capture that died mid-checkpoint)
//! would need its `-shm`, so that case is copied to a temp directory first and opened there; a
//! published database never has one.
//!
//! Which golden this database is compared against is decided by the `fixture` label **inside** the
//! golden, matched with the database's own `meta.project_name` — never by parsing the path, which
//! says where a maintainer happened to put it.
//!
//! ## What the projection leaves out, and why
//!
//! - **Timestamps**: `generation.published_at`, `generation.duration_ms` — a golden has to be
//!   reproducible, and a wall clock is not.
//! - **Generation ids**: random per publish (`g-xxxxxx`). A row that carries one is projected as
//!   `@current` (the id `latest_generation()` reports) or `@previous`, which is the fact the column
//!   actually asserts. Every other id is a rowid and is compared literally.
//! - **Machine facts**: `meta.project_root` (a checkout path), `project_branch`, `project_remote`
//!   (git's answer about wherever the archive lives). The capture indexes an archive outside git,
//!   so those are absent and `base_commit`/`worktree_dirty` carry the stable "no repository" answer.
//! - **Rust-only fields** of `GraphMeta` (`schema_version`, `generation_seq`, `embed_dim`) and its
//!   TypeScript-only `indexedAt`: the projection is the intersection of the two row types.
//!
//! Two projections are structurally empty on the checked-in fixtures and are reported as such by
//! the capture: `entry_points` (the TypeScript indexer never writes `node_metrics.is_entry = 1`, so
//! every published graph answers `[]`) and, per fixture, whatever that fixture has no rows for.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::envelope::TestCensus;
use sangkar_store::db::{Conn, open_reader};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, LineRange,
    SqliteReader, TestGapRow, TombstoneRow,
};

/// The methods the golden must cover, in the order the capture writes them.
const METHODS: [&str; 22] = [
    "find_by_name",
    "find_by_qn",
    "members",
    "nodes_by_file",
    "inbound",
    "outbound",
    "metrics_for",
    "rows_for_paths",
    "not_indexed_all",
    "not_indexed_count",
    "unresolved_for_files",
    "file_by_path",
    "all_files",
    "census_by_language",
    "tests_census",
    "meta_for",
    "latest_generation",
    "removed_symbols_for",
    "entry_points",
    "hotspots",
    "test_gaps",
    "read_node_fts",
];

/// The prefixes `rows_for_paths` is asked about; the capture derives no other list.
const PREFIXES: [&str; 2] = ["src", "lib"];

/// The rank limit both sides pass to `entry_points`, `hotspots` and `read_node_fts`.
const RANK_LIMIT: usize = 5;

// ---------------------------------------------------------------------------------------------
// The canonical writer: the same five shapes as `capture-reader-dump.ts`.
// ---------------------------------------------------------------------------------------------

/// A JSON string with the escapes both writers emit: quote, backslash and the five short controls.
fn quote(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn s(text: &str) -> String {
    quote(text)
}

fn n(value: i64) -> String {
    value.to_string()
}

fn b(value: bool) -> String {
    if value { "true" } else { "false" }.to_owned()
}

/// `[a,b]` — no spaces, so the text is a structural rendering rather than a pretty one.
fn arr(items: Vec<String>) -> String {
    format!("[{}]", items.join(","))
}

/// `{"k":v,…}` in the declared order; a `None` value drops the field entirely.
fn obj(parts: Vec<(&str, Option<String>)>) -> String {
    let kept: Vec<String> = parts
        .into_iter()
        .filter_map(|(key, value)| value.map(|value| format!("{}:{}", quote(key), value)))
        .collect();
    format!("{{{}}}", kept.join(","))
}

fn count(value: usize) -> String {
    n(i64::try_from(value).expect("a row count fits in i64"))
}

// ---------------------------------------------------------------------------------------------
// The projections, one per row type. Every field is the row type's own snake_case name.
// ---------------------------------------------------------------------------------------------

/// A generation id as a stable label, already quoted. `@current` is the id the newest publish has;
/// anything else reads as `@previous`, because two publishes are all a fixture has.
struct Labels {
    current: Option<String>,
}

impl Labels {
    fn new(current: Option<String>) -> Self {
        Self { current }
    }

    fn text(&self, id: &str) -> String {
        let label = if self.current.as_deref() == Some(id) {
            "@current"
        } else {
            "@previous"
        };
        s(label)
    }

    fn optional(&self, id: Option<&str>) -> Option<String> {
        id.map(|id| self.text(id))
    }
}

fn node_text(node: &NodeRow) -> String {
    obj(vec![
        ("id", Some(n(node.id))),
        ("file_id", Some(n(node.file_id))),
        ("path", Some(s(&node.path))),
        ("parent_id", node.parent_id.map(n)),
        ("kind", Some(s(&node.kind))),
        ("subkind", node.subkind.as_deref().map(s)),
        ("name", Some(s(&node.name))),
        ("qn", Some(s(&node.qn))),
        ("start_line", Some(n(node.start_line))),
        ("end_line", Some(n(node.end_line))),
        ("signature", node.signature.as_deref().map(s)),
        ("docstring", node.docstring.as_deref().map(s)),
        ("is_test", Some(b(node.is_test))),
        // The raw `node.props` text, which is what the TypeScript reader's parsed value round-trips
        // to; the capture proves that round trip before it projects a single row.
        ("props", Some(node.props.clone())),
    ])
}

fn edge_text(edge: &NeighbourEdge) -> String {
    obj(vec![
        (
            "edge",
            Some(obj(vec![
                ("id", Some(n(edge.edge.id))),
                ("src_id", Some(n(edge.edge.src_id))),
                ("dst_id", Some(n(edge.edge.dst_id))),
                ("kind", Some(s(&edge.edge.kind))),
                ("tier", Some(s(&edge.edge.tier))),
                ("file_id", Some(n(edge.edge.file_id))),
                ("line", Some(n(edge.edge.line))),
                ("local_name", Some(s(&edge.edge.local_name))),
                ("evidence", edge.edge.evidence.as_deref().map(s)),
            ])),
        ),
        ("node", Some(node_text(&edge.node))),
    ])
}

fn file_text(file: &IndexedFileRow, labels: &Labels) -> String {
    obj(vec![
        ("id", Some(n(file.id))),
        ("path", Some(s(&file.path))),
        ("language", Some(s(&file.language))),
        ("sha256", Some(s(&file.sha256))),
        ("size", Some(n(file.size))),
        ("line_count", Some(n(file.line_count))),
        ("parse_status", Some(s(&file.parse_status))),
        (
            "parse_error_ranges",
            file.parse_error_ranges.as_deref().map(s),
        ),
        (
            "indexed_generation",
            Some(labels.text(&file.indexed_generation)),
        ),
    ])
}

fn skipped_text(row: &SkippedPathRow) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("path", Some(s(&row.path))),
        ("reason", Some(s(&row.reason))),
        ("detail", row.detail.as_deref().map(s)),
        ("language", row.language.as_deref().map(s)),
    ])
}

fn unresolved_text(row: &UnresolvedRef) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("file_id", Some(n(row.file_id))),
        ("path", Some(s(&row.file))),
        ("line", Some(n(row.line))),
        ("raw_target", Some(s(&row.raw_target))),
        ("evidence_kind", row.evidence_kind.as_deref().map(s)),
        ("evidence", row.evidence.as_deref().map(s)),
    ])
}

fn metrics_text(row: &NodeMetrics) -> String {
    obj(vec![
        ("node_id", Some(n(row.node_id))),
        ("in_degree", Some(n(row.in_degree))),
        ("out_degree", Some(n(row.out_degree))),
        ("is_entry", Some(b(row.is_entry))),
        ("tested_depth", row.tested_depth.map(n)),
        ("reachable_count", row.reachable_count.map(n)),
    ])
}

fn coverage_text(answer: &PathCoverage, labels: &Labels) -> String {
    obj(vec![
        (
            "files",
            Some(arr(answer
                .files
                .iter()
                .map(|file| file_text(file, labels))
                .collect())),
        ),
        (
            "not_indexed",
            Some(arr(answer.not_indexed.iter().map(skipped_text).collect())),
        ),
        ("indexed", Some(count(answer.indexed))),
        ("skipped", Some(count(answer.skipped))),
    ])
}

fn census_text(row: &LanguageCensus) -> String {
    obj(vec![
        ("language", row.language.as_deref().map(s)),
        ("files", Some(n(row.files))),
        ("parsed", Some(n(row.parsed))),
        ("failed", Some(n(row.failed))),
    ])
}

fn tests_text(census: &TestCensus) -> String {
    let languages: Vec<String> = census
        .languages
        .iter()
        .map(|entry| {
            obj(vec![
                ("language", Some(s(&entry.language))),
                (
                    "nodes",
                    Some(n(i64::try_from(entry.nodes).expect("a count fits in i64"))),
                ),
            ])
        })
        .collect();
    obj(vec![
        (
            "nodes",
            Some(n(i64::try_from(census.nodes).expect("a count fits in i64"))),
        ),
        ("languages", Some(arr(languages))),
    ])
}

fn meta_text(meta: &GraphMeta, labels: &Labels) -> String {
    obj(vec![
        ("project_name", meta.project_name.as_deref().map(s)),
        ("generation", labels.optional(meta.generation.as_deref())),
        ("indexer_state", meta.indexer_state.as_deref().map(s)),
        ("consecutive_failures", Some(n(meta.consecutive_failures))),
    ])
}

fn generation_text(row: &GenerationRow) -> String {
    obj(vec![
        ("seq", Some(n(row.seq))),
        ("worktree_dirty", Some(b(row.worktree_dirty))),
        ("files_indexed", Some(n(row.files_indexed))),
        ("nodes", Some(n(row.nodes))),
        ("edges", Some(n(row.edges))),
        ("mode", Some(s(&row.mode))),
        ("status", Some(s(&row.status))),
    ])
}

fn tombstone_text(row: &TombstoneRow, labels: &Labels) -> String {
    obj(vec![
        ("qn", Some(s(&row.qn))),
        ("kind", Some(s(&row.kind))),
        ("file_id", row.file_id.map(n)),
        (
            "last_seen_generation",
            labels.optional(Some(row.last_seen_generation.as_str())),
        ),
        (
            "removed_in_generation",
            Some(labels.text(&row.removed_in_generation)),
        ),
        ("had_callers", Some(b(row.had_callers))),
    ])
}

fn lines_text(lines: LineRange) -> String {
    arr(vec![n(lines.start), n(lines.end)])
}

fn entry_point_text(row: &EntryPointRow) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("qn", Some(s(&row.qn))),
        ("kind", Some(s(&row.kind))),
        ("file", Some(s(&row.file))),
        ("lines", Some(lines_text(row.lines))),
        ("reachable", row.reachable.map(n)),
    ])
}

fn hotspot_text(row: &HotspotRow) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("qn", Some(s(&row.qn))),
        ("kind", Some(s(&row.kind))),
        ("file", Some(s(&row.file))),
        ("lines", Some(lines_text(row.lines))),
        ("in_degree", Some(n(row.in_degree))),
        ("out_degree", Some(n(row.out_degree))),
    ])
}

fn test_gap_text(row: &TestGapRow) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("qn", Some(s(&row.qn))),
        ("kind", Some(s(&row.kind))),
        ("file", Some(s(&row.file))),
        ("lines", Some(lines_text(row.lines))),
        ("tested_depth", row.tested_depth.map(n)),
    ])
}

/// The exact `f64` the database returned, as its IEEE-754 bit pattern in decimal: the one text both
/// languages produce for a real without agreeing on a decimal format.
fn fts_text(row: &FtsHit) -> String {
    obj(vec![
        ("id", Some(n(row.id))),
        ("qn", Some(s(&row.qn))),
        ("kind", Some(s(&row.kind))),
        ("file", Some(s(&row.file))),
        ("lines", Some(lines_text(row.lines))),
        ("score_bits", Some(s(&row.score.to_bits().to_string()))),
    ])
}

// ---------------------------------------------------------------------------------------------
// Reading the golden: one key per line, so neither side needs a JSON library.
// ---------------------------------------------------------------------------------------------

/// A parsed golden: the fixture label plus every `key → canonical value text` line.
struct Golden {
    path: PathBuf,
    fixture: String,
    entries: Vec<(String, String)>,
}

impl Golden {
    fn text(&self, key: &str) -> &str {
        self.entries
            .iter()
            .find(|(entry, _)| entry == key)
            .map(|(_, value)| value.as_str())
            .unwrap_or_else(|| panic!("{}: no {key} line", self.path.display()))
    }
}

/// The backslash escapes {@link quote} writes, undone; a golden key never contains one.
fn unquote(text: &str) -> String {
    let body = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or_else(|| panic!("{text} is not a quoted string"));
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some(other) => panic!("unsupported escape \\{other} in {text}"),
            None => panic!("a dangling escape in {text}"),
        }
    }
    out
}

fn read_golden(path: &Path) -> Golden {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let mut fixture: Option<String> = None;
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed == "{" || trimmed == "}" {
            continue;
        }
        let rest = trimmed.strip_prefix('"').unwrap_or_else(|| {
            panic!(
                "{}:{}: a key must start with a quote",
                path.display(),
                index + 1
            )
        });
        let end = rest.find('"').unwrap_or_else(|| {
            panic!(
                "{}:{}: a key must end with a quote",
                path.display(),
                index + 1
            )
        });
        let key = &rest[..end];
        let after = rest[end + 1..].strip_prefix(": ").unwrap_or_else(|| {
            panic!(
                "{}:{}: {key} must be followed by `: `",
                path.display(),
                index + 1
            )
        });
        let value = after.strip_suffix(',').unwrap_or(after);
        if key == "fixture" {
            fixture = Some(unquote(value));
        }
        entries.push((key.to_owned(), value.to_owned()));
    }
    let fixture = fixture.unwrap_or_else(|| panic!("{}: no fixture label", path.display()));
    Golden {
        path: path.to_path_buf(),
        fixture,
        entries,
    }
}

/// One `target.*` value: a string, an integer, or a list of integers.
enum TargetValue {
    Text(String),
    Number(i64),
    List(Vec<i64>),
}

fn decode_target(key: &str, text: &str) -> TargetValue {
    if text.starts_with('"') {
        return TargetValue::Text(unquote(text));
    }
    if let Some(inner) = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        let items = if inner.is_empty() {
            Vec::new()
        } else {
            inner
                .split(',')
                .map(|item| {
                    item.parse::<i64>()
                        .unwrap_or_else(|_| panic!("{key}: {item} is not an id"))
                })
                .collect()
        };
        return TargetValue::List(items);
    }
    TargetValue::Number(
        text.parse::<i64>()
            .unwrap_or_else(|_| panic!("{key}: {text} is not an integer")),
    )
}

/// The arguments the capture used, replayed here so the two sides cannot drift on a query.
struct Targets {
    duplicate_name: String,
    qn: String,
    members_parent_id: i64,
    file_id: i64,
    node_id: i64,
    file_path: String,
    unresolved_file_ids: Vec<i64>,
    test_gap_ids: Vec<i64>,
    fts_query: String,
    removed_generation: String,
}

impl Targets {
    fn read(golden: &Golden) -> Self {
        let text = |key: &str| match decode_target(key, golden.text(key)) {
            TargetValue::Text(value) => value,
            other => panic!(
                "{key} must be a string, not {other:?}",
                other = variant(&other)
            ),
        };
        let number = |key: &str| match decode_target(key, golden.text(key)) {
            TargetValue::Number(value) => value,
            other => panic!(
                "{key} must be an id, not {other:?}",
                other = variant(&other)
            ),
        };
        let list = |key: &str| match decode_target(key, golden.text(key)) {
            TargetValue::List(value) => value,
            other => panic!(
                "{key} must be an id list, not {other:?}",
                other = variant(&other)
            ),
        };
        Self {
            duplicate_name: text("target.duplicate_name"),
            qn: text("target.qn"),
            members_parent_id: number("target.members_parent_id"),
            file_id: number("target.file_id"),
            node_id: number("target.node_id"),
            file_path: text("target.file_path"),
            unresolved_file_ids: list("target.unresolved_file_ids"),
            test_gap_ids: list("target.test_gap_ids"),
            fts_query: text("target.fts_query"),
            removed_generation: text("target.removed_generation"),
        }
    }
}

fn variant(value: &TargetValue) -> &'static str {
    match value {
        TargetValue::Text(_) => "a string",
        TargetValue::Number(_) => "an integer",
        TargetValue::List(_) => "a list",
    }
}

// ---------------------------------------------------------------------------------------------
// The structural diff: the method, then the first element that differs and what both sides say.
// ---------------------------------------------------------------------------------------------

/// Split the inside of a JSON object or array at its top-level commas: brackets and quoted strings
/// (with escapes) are skipped, so a nested value stays one part.
fn split_top_level(body: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0usize;
    for (index, ch) in body.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&body[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < body.len() {
        parts.push(&body[start..]);
    }
    parts
}

/// `[member, …]` of an object, each `("key", value text)`; the key keeps its quotes.
fn object_members(text: &str) -> Vec<(&str, &str)> {
    let body = text
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
        .unwrap_or_else(|| panic!("{text} is not an object"));
    split_top_level(body)
        .into_iter()
        .map(split_member)
        .collect()
}

fn split_member(member: &str) -> (&str, &str) {
    let rest = member
        .strip_prefix('"')
        .unwrap_or_else(|| panic!("{member} does not start with a key"));
    let mut escaped = false;
    for (index, ch) in rest.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '"' {
            let key = &member[..index + 2];
            let value = rest[index + 1..]
                .strip_prefix(':')
                .unwrap_or_else(|| panic!("{member} has no value"));
            return (key, value);
        }
    }
    panic!("{member} has no closing quote")
}

fn array_elements(text: &str) -> Vec<&str> {
    let body = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or_else(|| panic!("{text} is not an array"));
    split_top_level(body)
}

fn shorten(text: &str) -> String {
    const LIMIT: usize = 220;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let cut: String = text.chars().take(LIMIT).collect();
    format!("{cut}…")
}

fn location(path: &[String]) -> String {
    let mut out = String::new();
    for segment in path {
        if segment.starts_with('[') {
            out.push_str(segment);
        } else {
            if !out.is_empty() {
                out.push('.');
            }
            out.push_str(segment);
        }
    }
    out
}

/// The first structural difference, as `location: golden … reader …`; `None` when the two texts are
/// equal (the caller already knows they are not, so a `None` here is a bug in this walker).
fn first_difference(expected: &str, actual: &str, path: &mut Vec<String>) -> Option<String> {
    if expected == actual {
        return None;
    }
    if expected.starts_with('{') && actual.starts_with('{') {
        let expected_members = object_members(expected);
        let actual_members = object_members(actual);
        for (index, (key, value)) in expected_members.iter().enumerate() {
            match actual_members.get(index) {
                None => {
                    return Some(format!(
                        "{}: the golden has member {key} and the reader answered none",
                        location(path)
                    ));
                }
                Some((actual_key, actual_value)) => {
                    if key != actual_key {
                        return Some(format!(
                            "{}: the golden has member {key} where the reader answered {actual_key}",
                            location(path)
                        ));
                    }
                    path.push(key.trim_matches('"').to_owned());
                    if let Some(difference) = first_difference(value, actual_value, path) {
                        return Some(difference);
                    }
                    path.pop();
                }
            }
        }
        if expected_members.len() != actual_members.len() {
            return Some(format!(
                "{}: the golden has {} members and the reader answered {}",
                location(path),
                expected_members.len(),
                actual_members.len()
            ));
        }
        return None;
    }
    if expected.starts_with('[') && actual.starts_with('[') {
        let expected_elements = array_elements(expected);
        let actual_elements = array_elements(actual);
        for (index, element) in expected_elements.iter().enumerate() {
            path.push(format!("[{index}]"));
            match actual_elements.get(index) {
                None => {
                    return Some(format!(
                        "{}: the golden has an element and the reader answered none",
                        location(path)
                    ));
                }
                Some(actual_element) => {
                    if let Some(difference) = first_difference(element, actual_element, path) {
                        return Some(difference);
                    }
                }
            }
            path.pop();
        }
        if expected_elements.len() != actual_elements.len() {
            return Some(format!(
                "{}: the golden has {} elements and the reader answered {}",
                location(path),
                expected_elements.len(),
                actual_elements.len()
            ));
        }
        return None;
    }
    Some(format!(
        "{}: the golden has {} and the reader answered {}",
        location(path),
        shorten(expected),
        shorten(actual)
    ))
}

// ---------------------------------------------------------------------------------------------
// The fixture database.
// ---------------------------------------------------------------------------------------------

/// A temporary directory that removes itself, for the `-wal` copy.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "sangkar-reader-golden-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Open the fixture database read-only. A published database is one file; a `-wal` sibling (a
/// capture interrupted before its checkpoint) needs its `-shm` beside it, so that case is copied to
/// a temp directory and opened there — the capture's file is never written by this test.
fn open_fixture(path: &Path, temp: &mut Option<TempDir>) -> Conn {
    let mut wal = OsString::from(path.as_os_str());
    wal.push("-wal");
    let wal = PathBuf::from(wal);
    if !wal.exists() {
        return open_reader(path)
            .unwrap_or_else(|error| panic!("cannot open {}: {error}", path.display()));
    }

    let directory = TempDir::new("wal");
    let copy = directory.0.join("graph.db");
    for (from, to) in [
        (path.to_path_buf(), copy.clone()),
        (
            wal.clone(),
            PathBuf::from(format!("{}-wal", copy.display())),
        ),
        (
            PathBuf::from(format!("{}-shm", path.display())),
            PathBuf::from(format!("{}-shm", copy.display())),
        ),
    ] {
        if from.exists() {
            fs::copy(&from, &to)
                .unwrap_or_else(|error| panic!("cannot copy {}: {error}", from.display()));
        }
    }
    let conn = open_reader(&copy)
        .unwrap_or_else(|error| panic!("cannot open {}: {error}", copy.display()));
    *temp = Some(directory);
    conn
}

fn golden_directory() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/golden/reader")
}

fn read_goldens() -> Vec<Golden> {
    let directory = golden_directory();
    let mut paths: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
        .map(|entry| entry.expect("a directory entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    paths.sort();
    assert!(
        !paths.is_empty(),
        "{} holds no golden: run `~/.bun/bin/bun run scripts/parity/capture-reader-dump.ts`",
        directory.display()
    );
    paths.iter().map(|path| read_golden(path)).collect()
}

// ---------------------------------------------------------------------------------------------
// The test.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_rust_reader_answers_every_golden_row() {
    let Some(db) = std::env::var_os("SANGKAR_FIXTURE_DB") else {
        println!(
            "reader_golden: SANGKAR_FIXTURE_DB is unset — skipped. Capture a database with\n  \
             ~/.bun/bin/bun run scripts/parity/capture-reader-dump.ts\n\
             and pass the printed path. The goldens under rust/tests/golden/reader are committed; \
             the databases are not."
        );
        return;
    };
    let db_path = PathBuf::from(db);
    assert!(
        db_path.is_file(),
        "SANGKAR_FIXTURE_DB={} is not a file",
        db_path.display()
    );

    let goldens = read_goldens();
    let mut temp: Option<TempDir> = None;
    let reader = SqliteReader::new(open_fixture(&db_path, &mut temp));

    let meta = reader.meta_for().expect("meta_for");
    let project_name = meta
        .project_name
        .clone()
        .unwrap_or_else(|| panic!("{}: meta.project_name is absent", db_path.display()));
    let golden = goldens
        .iter()
        .find(|golden| golden.fixture == project_name)
        .unwrap_or_else(|| {
            let labels: Vec<&str> = goldens
                .iter()
                .map(|golden| golden.fixture.as_str())
                .collect();
            panic!(
                "{} answers project_name {project_name:?} and no golden says so: {}",
                db_path.display(),
                labels.join(", ")
            )
        });

    for (key, _) in &golden.entries {
        if key == "fixture" || key.starts_with("target.") || key.starts_with("note.") {
            continue;
        }
        assert!(
            METHODS.contains(&key.as_str()),
            "{}: {key} is not one of the {count} reader methods",
            golden.path.display(),
            count = METHODS.len()
        );
    }

    let targets = Targets::read(golden);
    let labels = Labels::new(
        reader
            .latest_generation()
            .expect("latest_generation")
            .map(|row| row.id),
    );

    // `@current` is the only resolvable generation label: it names the newest publish, which is the
    // one a tombstone from the second capture carries.
    let removed_generation = labels.current.clone().unwrap_or_default();
    assert!(
        !removed_generation.is_empty() && targets.removed_generation == "@current",
        "{}: target.removed_generation is {:?} and this database has no newest publish",
        golden.path.display(),
        targets.removed_generation
    );

    let prefixes: Vec<String> = PREFIXES.iter().map(|prefix| (*prefix).to_owned()).collect();
    let values: Vec<(&str, String)> = vec![
        (
            "find_by_name",
            arr(reader
                .find_by_name(&targets.duplicate_name, None)
                .expect("find_by_name")
                .iter()
                .map(node_text)
                .collect()),
        ),
        (
            "find_by_qn",
            obj(vec![
                (
                    "hit",
                    Some(match reader.find_by_qn(&targets.qn).expect("find_by_qn") {
                        Some(node) => node_text(&node),
                        None => "null".to_owned(),
                    }),
                ),
                (
                    "miss",
                    Some(
                        match reader
                            .find_by_qn("no/such/qn.ts::__absent__")
                            .expect("find_by_qn")
                        {
                            Some(node) => node_text(&node),
                            None => "null".to_owned(),
                        },
                    ),
                ),
            ]),
        ),
        (
            "members",
            arr(reader
                .members(targets.members_parent_id)
                .expect("members")
                .iter()
                .map(node_text)
                .collect()),
        ),
        (
            "nodes_by_file",
            arr(reader
                .nodes_by_file(targets.file_id)
                .expect("nodes_by_file")
                .iter()
                .map(node_text)
                .collect()),
        ),
        (
            "inbound",
            arr(reader
                .inbound(targets.node_id, None)
                .expect("inbound")
                .iter()
                .map(edge_text)
                .collect()),
        ),
        (
            "outbound",
            arr(reader
                .outbound(targets.node_id, None)
                .expect("outbound")
                .iter()
                .map(edge_text)
                .collect()),
        ),
        (
            "metrics_for",
            obj(vec![
                (
                    "hit",
                    Some(
                        match reader.metrics_for(targets.node_id).expect("metrics_for") {
                            Some(metrics) => metrics_text(&metrics),
                            None => "null".to_owned(),
                        },
                    ),
                ),
                (
                    "miss",
                    Some(match reader.metrics_for(-1).expect("metrics_for") {
                        Some(metrics) => metrics_text(&metrics),
                        None => "null".to_owned(),
                    }),
                ),
            ]),
        ),
        (
            "rows_for_paths",
            obj(vec![
                (
                    "src",
                    Some(coverage_text(
                        &reader
                            .rows_for_paths(&prefixes[..1])
                            .expect("rows_for_paths"),
                        &labels,
                    )),
                ),
                (
                    "lib",
                    Some(coverage_text(
                        &reader
                            .rows_for_paths(&prefixes[1..])
                            .expect("rows_for_paths"),
                        &labels,
                    )),
                ),
            ]),
        ),
        (
            "not_indexed_all",
            arr(reader
                .not_indexed_all()
                .expect("not_indexed_all")
                .iter()
                .map(skipped_text)
                .collect()),
        ),
        (
            "not_indexed_count",
            n(reader.not_indexed_count().expect("not_indexed_count")),
        ),
        (
            "unresolved_for_files",
            arr(reader
                .unresolved_for_files(&targets.unresolved_file_ids)
                .expect("unresolved_for_files")
                .iter()
                .map(unresolved_text)
                .collect()),
        ),
        (
            "file_by_path",
            obj(vec![
                (
                    "hit",
                    Some(
                        match reader
                            .file_by_path(&targets.file_path)
                            .expect("file_by_path")
                        {
                            Some(file) => file_text(&file, &labels),
                            None => "null".to_owned(),
                        },
                    ),
                ),
                (
                    "miss",
                    Some(
                        match reader
                            .file_by_path("no/such/path.ts")
                            .expect("file_by_path")
                        {
                            Some(file) => file_text(&file, &labels),
                            None => "null".to_owned(),
                        },
                    ),
                ),
            ]),
        ),
        (
            "all_files",
            arr(reader
                .all_files()
                .expect("all_files")
                .iter()
                .map(|file| file_text(file, &labels))
                .collect()),
        ),
        (
            "census_by_language",
            arr(reader
                .census_by_language()
                .expect("census_by_language")
                .iter()
                .map(census_text)
                .collect()),
        ),
        (
            "tests_census",
            tests_text(&reader.tests_census().expect("tests_census")),
        ),
        ("meta_for", meta_text(&meta, &labels)),
        (
            "latest_generation",
            match reader.latest_generation().expect("latest_generation") {
                Some(row) => generation_text(&row),
                None => "null".to_owned(),
            },
        ),
        (
            "removed_symbols_for",
            arr(reader
                .removed_symbols_for(&removed_generation)
                .expect("removed_symbols_for")
                .iter()
                .map(|row| tombstone_text(row, &labels))
                .collect()),
        ),
        (
            "entry_points",
            arr(reader
                .entry_points(RANK_LIMIT)
                .expect("entry_points")
                .iter()
                .map(entry_point_text)
                .collect()),
        ),
        (
            "hotspots",
            arr(reader
                .hotspots(RANK_LIMIT)
                .expect("hotspots")
                .iter()
                .map(hotspot_text)
                .collect()),
        ),
        (
            "test_gaps",
            arr(reader
                .test_gaps(&targets.test_gap_ids)
                .expect("test_gaps")
                .iter()
                .map(test_gap_text)
                .collect()),
        ),
        (
            "read_node_fts",
            arr(reader
                .read_node_fts(&targets.fts_query, RANK_LIMIT)
                .expect("read_node_fts")
                .iter()
                .map(fts_text)
                .collect()),
        ),
    ];

    println!(
        "reader golden: {} ({})",
        golden.fixture,
        golden.path.display()
    );
    let mut compared = 0usize;
    for (method, rust) in &values {
        let expected = golden.text(method);
        println!(
            "  {method:<22} {:>4} element(s)",
            array_elements_or_members(expected)
        );
        if rust != expected {
            let mut path = vec![(*method).to_owned()];
            let difference = first_difference(expected, rust, &mut path)
                .unwrap_or_else(|| format!("{method}: texts differ"));
            panic!(
                "{method}: the reader and the golden disagree\n  first difference at {difference}\n\
                 \x20 golden: {}\n  rust:   {}\n",
                shorten(expected),
                shorten(rust)
            );
        }
        compared += 1;
    }
    assert_eq!(
        compared,
        METHODS.len(),
        "the comparison covered {compared} of {} methods",
        METHODS.len()
    );
    println!(
        "{compared} compared methods against the {} golden",
        golden.fixture
    );
}

/// How many rows (or halves) one projection holds, for the run's own report.
fn array_elements_or_members(text: &str) -> usize {
    if text.starts_with('[') {
        array_elements(text).len()
    } else if text.starts_with('{') {
        object_members(text).len()
    } else {
        1
    }
}
