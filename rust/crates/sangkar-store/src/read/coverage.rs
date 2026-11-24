//! Coverage primitives: the two halves of "was this scope indexed?" (`DATABASE.md` §3.2, §6).
//!
//! Ported from `src/shared/store/read/coverage.ts`. A path prefix is answered from two tables and
//! both halves are mandatory: `file` names the paths that were parsed, `not_indexed` names the
//! paths that were looked at and skipped, with the reason. One half alone cannot tell "nothing
//! calls this" apart from "this was never parsed", which is the difference between a correct empty
//! answer and a wrong one (`DESIGN.md` §6).
//!
//! The prefix test is [`prefix_bounds`]'s half-open range — `path = exact` or `path > exact + "/"`
//! and `path < exact + "0"` — never `LIKE 'prefix%'` or `path >= prefix`: `-` (0x2d) sorts below
//! `/` (0x2f), so the naive lower bound also matches a sibling such as `src-old` for `src`.
//!
//! The row structs carry no `serde` derives. `sangkar-store` has no `serde` dependency, and the
//! wire shapes belong to whichever crate serialises them: `sangkar-core::envelope` owns
//! `TestCensus`, which is why [`crate::read::files::tests_census`] returns that type instead of a
//! duplicate here.

use rusqlite::{CachedStatement, OptionalExtension, Row, params_from_iter};
use sangkar_core::errors::AppError;
use sangkar_core::paths::prefix_bounds;

use crate::db::Conn;

/// The `file` columns, in the order [`read_indexed_file`] reads them. Kept beside the mapper
/// because the same row shape is selected by [`crate::read::files`]: one column list, one mapper,
/// so a column added to one query cannot be read by the other under the wrong index.
pub(crate) const FILE_COLUMNS: &str = concat!(
    "f.id, f.path, f.language, f.sha256, f.size, f.line_count,",
    " f.parse_status, f.parse_error_ranges, f.indexed_generation"
);

/// The statement keys for [`Conn::prepared`], named after the TypeScript's.
///
/// The TypeScript appends the prefix count (`coverage.indexed/2`) because its `prepared()` takes a
/// `String`; here the key is a `&'static str`, and leaking one key per distinct arity in a daemon
/// that takes `--paths` from a caller would leak with the caller's cardinality. The key therefore
/// names the query family: the arity-varying SQL is retained by the driver's own cache, which is
/// keyed by SQL text and capped at the same `STATEMENT_CACHE_CAPACITY`, so both layers stay
/// bounded and a re-issued shape is a hit either way.
const KEY_INDEXED: &str = "coverage.indexed";
const KEY_NOT_INDEXED: &str = "coverage.notIndexed";
const KEY_NOT_INDEXED_ALL: &str = "coverage.notIndexedAll";
const KEY_NOT_INDEXED_COUNT: &str = "coverage.notIndexedCount";
const KEY_UNRESOLVED: &str = "coverage.unresolved";

/// One parsed file, mapped from the `file` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexedFileRow {
    pub id: i64,
    pub path: String,
    pub language: String,
    pub sha256: String,
    pub size: i64,
    pub line_count: i64,
    /// `ok` | `partial` | `failed`; the census in `status` counts these per language.
    pub parse_status: String,
    /// `'12-12,38-38'` when `parse_status` is `partial`, otherwise `None`.
    pub parse_error_ranges: Option<String>,
    /// The publish that last wrote this file's rows; a stale value is a stale file.
    pub indexed_generation: String,
}

/// One path the indexer looked at and did not index, with the reason it says so.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedPathRow {
    pub id: i64,
    pub path: String,
    /// `grammar_unavailable` | `binary` | `ignored` | `size_cap` | `unsupported_ext` |
    /// `unreadable` | …
    pub reason: String,
    /// The human-readable cause, when the indexer had one to record.
    pub detail: Option<String>,
    /// The language the path was attributed to; `None` when the indexer could not name one.
    pub language: Option<String>,
}

/// Both halves of the answer for one prefix query, plus the two totals a caller quotes.
///
/// `indexed` and `skipped` are the lengths of the two lists, counted in the same pass that built
/// them ([`rows_for_paths`]), so the number a payload reports and the list it renders cannot
/// disagree — the drift a second `COUNT(*)` would allow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathCoverage {
    /// The indexed files under the requested prefixes, ordered by `path`.
    pub files: Vec<IndexedFileRow>,
    /// The skipped paths under the same prefixes, ordered by `path`, then `reason`.
    pub not_indexed: Vec<SkippedPathRow>,
    /// `files.len()`.
    pub indexed: usize,
    /// `not_indexed.len()`.
    pub skipped: usize,
}

/// One call site whose target did not bind, kept for the absence contract (`DATABASE.md` §7).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedRef {
    pub id: i64,
    pub file_id: i64,
    /// `file.path` of the file the call site lives in, joined from the `file` row.
    pub file: String,
    pub line: i64,
    pub raw_target: String,
    pub evidence_kind: Option<String>,
    pub evidence: Option<String>,
}

/// One `not_indexed` row, mapped in one place so every reader of that table agrees on the shape.
fn read_skipped_path(row: &Row<'_>) -> rusqlite::Result<SkippedPathRow> {
    Ok(SkippedPathRow {
        id: row.get(0)?,
        path: row.get(1)?,
        reason: row.get(2)?,
        detail: row.get(3)?,
        language: row.get(4)?,
    })
}

/// One `file` row through [`FILE_COLUMNS`]. Shared with [`crate::read::files`], which selects the
/// same columns, so the two readers of that table cannot drift apart.
pub(crate) fn read_indexed_file(row: &Row<'_>) -> rusqlite::Result<IndexedFileRow> {
    Ok(IndexedFileRow {
        id: row.get(0)?,
        path: row.get(1)?,
        language: row.get(2)?,
        sha256: row.get(3)?,
        size: row.get(4)?,
        line_count: row.get(5)?,
        parse_status: row.get(6)?,
        parse_error_ranges: row.get(7)?,
        indexed_generation: row.get(8)?,
    })
}

fn read_unresolved(row: &Row<'_>) -> rusqlite::Result<UnresolvedRef> {
    Ok(UnresolvedRef {
        id: row.get(0)?,
        file_id: row.get(1)?,
        file: row.get(2)?,
        line: row.get(3)?,
        raw_target: row.get(4)?,
        evidence_kind: row.get(5)?,
        evidence: row.get(6)?,
    })
}

/// Runs one prepared statement and maps every row through `map`.
///
/// `what` names the query in the error: a `busy` or a type mismatch from the wrong statement is
/// undebuggable, and the store's only exit is an `AppError` that has to say which query failed.
pub(crate) fn collect<T>(
    mut statement: CachedStatement<'_>,
    params: impl rusqlite::Params,
    what: &str,
    map: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, AppError> {
    let mut rows = statement
        .query(params)
        .map_err(|error| AppError::internal(format!("running {what}: {error}")))?;
    let mut out = Vec::new();
    while let Some(row) = rows
        .next()
        .map_err(|error| AppError::internal(format!("reading {what}: {error}")))?
    {
        out.push(map(row).map_err(|error| AppError::internal(format!("mapping {what}: {error}")))?);
    }
    Ok(out)
}

/// `(t.path = ? OR (t.path > ? AND t.path < ?))`, once per requested prefix and joined by `OR`.
///
/// Built from `prefix_count` alone — a number, never a prefix string — so no caller value reaches
/// the SQL text; the three bounds of each prefix are bound as parameters by [`path_params`].
fn path_conditions(alias: &str, prefix_count: usize) -> String {
    let block = format!("({alias}.path = ? OR ({alias}.path > ? AND {alias}.path < ?))");
    vec![block; prefix_count].join(" OR ")
}

/// The three bounds of every prefix, flattened to [`path_conditions`]' placeholder order.
fn path_params(prefixes: &[String]) -> Vec<String> {
    let mut params = Vec::with_capacity(prefixes.len() * 3);
    for prefix in prefixes {
        let bounds = prefix_bounds(prefix);
        params.push(bounds.exact);
        params.push(bounds.low);
        params.push(bounds.high);
    }
    params
}

/// Every indexed file under `prefixes`, plus every skipped path under the same prefixes.
///
/// A path matches a prefix by equality or by being strictly inside it, and the two lists are
/// disjoint because a path has a `file` row or a `not_indexed` row, not both as the same fact.
/// No limit and no ordering beyond `path`, which keeps a caller's pagination stable; ranking and
/// formatting belong to the tool.
///
/// An empty `prefixes` slice returns the empty [`PathCoverage`] without touching the connection: a
/// statement whose `WHERE` is empty would either match everything or fail to parse, and neither is
/// what "no prefix was asked about" means.
pub fn rows_for_paths(conn: &Conn, prefixes: &[String]) -> Result<PathCoverage, AppError> {
    if prefixes.is_empty() {
        return Ok(PathCoverage {
            files: Vec::new(),
            not_indexed: Vec::new(),
            indexed: 0,
            skipped: 0,
        });
    }

    let params = path_params(prefixes);

    let where_clause = path_conditions("f", prefixes.len());
    let indexed_sql =
        format!("SELECT {FILE_COLUMNS} FROM file f WHERE {where_clause} ORDER BY f.path");
    let files = collect(
        conn.prepared(KEY_INDEXED, |connection| {
            connection.prepare_cached(&indexed_sql)
        })?,
        params_from_iter(params.iter().map(String::as_str)),
        KEY_INDEXED,
        read_indexed_file,
    )?;

    let where_clause = path_conditions("ni", prefixes.len());
    let skipped_sql = format!(
        "SELECT ni.id, ni.path, ni.reason, ni.detail, ni.language FROM not_indexed ni \
         WHERE {where_clause} ORDER BY ni.path, ni.reason"
    );
    let not_indexed = collect(
        conn.prepared(KEY_NOT_INDEXED, |connection| {
            connection.prepare_cached(&skipped_sql)
        })?,
        params_from_iter(params.iter().map(String::as_str)),
        KEY_NOT_INDEXED,
        read_skipped_path,
    )?;

    // The counts are the lists: read off the vectors just built, in this pass, so the number and
    // the rows it counts cannot come from two different queries.
    let indexed = files.len();
    let skipped = not_indexed.len();
    Ok(PathCoverage {
        files,
        not_indexed,
        indexed,
        skipped,
    })
}

/// Every skipped path in the graph, with no prefix filter, ordered by `path`, then `reason`.
///
/// [`rows_for_paths`] answers "is this scope covered", which is a slice by construction, so a
/// caller that sums slices reports a smaller number than the graph holds whenever a skip sits
/// outside every scope it asked about. This is the list that total is counted from.
pub fn not_indexed_all(conn: &Conn) -> Result<Vec<SkippedPathRow>, AppError> {
    const SQL: &str = concat!(
        "SELECT ni.id, ni.path, ni.reason, ni.detail, ni.language FROM not_indexed ni",
        " ORDER BY ni.path, ni.reason"
    );
    collect(
        conn.prepared(KEY_NOT_INDEXED_ALL, |connection| {
            connection.prepare_cached(SQL)
        })?,
        [],
        KEY_NOT_INDEXED_ALL,
        read_skipped_path,
    )
}

/// The repo-wide number of skipped paths: the one total every tool can quote.
///
/// A `COUNT(*)` rather than `not_indexed_all().len()` because the total is a headline number read
/// on every `status`, and materialising every row to count it costs the whole table.
pub fn not_indexed_count(conn: &Conn) -> Result<i64, AppError> {
    const SQL: &str = "SELECT COUNT(*) AS n FROM not_indexed";
    let mut statement = conn.prepared(KEY_NOT_INDEXED_COUNT, |connection| {
        connection.prepare_cached(SQL)
    })?;
    statement
        .query_row([], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|error| AppError::internal(format!("running {KEY_NOT_INDEXED_COUNT}: {error}")))
        .map(Option::unwrap_or_default)
}

/// Unbound call sites for the given files, over `idx_unresolved_file` (§6).
///
/// An empty result is only an absence when coverage for the same scope is complete
/// (`DATABASE.md` §7), which is why this returns rows and never a verdict. An empty `file_ids`
/// slice returns no rows without touching the connection: `IN ()` is not valid SQL, and the
/// honest answer to "which unbound sites live in no file" is the empty list.
pub fn unresolved_for_files(conn: &Conn, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
    if file_ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = vec!["?"; file_ids.len()].join(", ");
    let sql = format!(
        "SELECT u.id, u.file_id, f.path, u.line, u.raw_target, u.evidence_kind, u.evidence \
         FROM unresolved u JOIN file f ON f.id = u.file_id \
         WHERE u.file_id IN ({list}) ORDER BY u.file_id, u.line, u.raw_target"
    );
    collect(
        conn.prepared(KEY_UNRESOLVED, |connection| connection.prepare_cached(&sql))?,
        params_from_iter(file_ids),
        KEY_UNRESOLVED,
        read_unresolved,
    )
}
