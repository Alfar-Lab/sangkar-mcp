//! The file census: which paths the indexer parsed, which it looked at and skipped, and the
//! per-language counts `status` reports (`DATABASE.md` §3.2).
//!
//! Ported from `src/shared/store/read/files.ts`. Two tables answer one question, and both halves
//! are mandatory. `file.parse_status` alone says "0 files of language xslt exist"; `not_indexed`
//! says "4 xslt paths were seen and none of them could be parsed". The census therefore folds a
//! skipped path into its language's `failed` count, which is the difference between an empty
//! language and a broken one — the exact confusion `TOOL-OUTPUTS.md` records in CRG, where `xslt`
//! and `liquid` are advertised as supported while both grammars fail to load.
//!
//! `parse_status` maps into the census without a third bucket: a `partial` parse produced nodes and
//! counts as parsed (its `parse_error_ranges` are still on the row for `status` to report), only
//! `failed` is a failure. `files` is always `parsed + failed`, so a caller can check the arithmetic
//! instead of trusting it.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use rusqlite::{OptionalExtension, Row};
use sangkar_core::envelope::{TestCensus, TestCensusEntry};
use sangkar_core::errors::AppError;
use sangkar_core::paths::normalize_rel;

use crate::db::Conn;
use crate::read::coverage::{FILE_COLUMNS, IndexedFileRow, collect, read_indexed_file};

/// The statement keys for [`Conn::prepared`], named after the TypeScript's.
const KEY_BY_PATH: &str = "files.byPath";
const KEY_ALL: &str = "files.all";
const KEY_CENSUS_INDEXED: &str = "files.censusIndexed";
const KEY_CENSUS_SKIPPED: &str = "files.censusSkipped";
const KEY_TESTS_CENSUS: &str = "files.testsCensus";

/// One language's census. `language` is `None` for a skipped path whose grammar the indexer could
/// not name (`not_indexed.language` is nullable); those rows are reported last rather than dropped,
/// because an unattributed failure is still a failure the caller must be told about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LanguageCensus {
    pub language: Option<String>,
    /// Paths seen for this language: indexed rows plus skipped rows. Always `parsed + failed`.
    pub files: i64,
    /// `parse_status` of `ok` or `partial`: rows that produced a graph contribution.
    pub parsed: i64,
    /// `parse_status = 'failed'` plus every `not_indexed` row for the language.
    pub failed: i64,
}

/// The `file` half of the census, as SQL groups it.
#[derive(Debug)]
struct IndexedCensusRow {
    language: String,
    files: i64,
    parsed: i64,
    failed: i64,
}

/// The `not_indexed` half: one bucket per language, including the unattributed one.
#[derive(Debug)]
struct SkippedCensusRow {
    language: Option<String>,
    skipped: i64,
}

/// One language's `node.is_test = 1` count, before the fallback for a graph written by an older
/// schema.
#[derive(Debug)]
struct TestCensusRow {
    /// Nullable because `TestCensusEntry.language` is a string and an older graph could have
    /// written a `NULL`; dropping the row instead would make the entries sum to less than `nodes`.
    language: Option<String>,
    nodes: i64,
}

/// The running totals for one language while the two halves are folded together.
#[derive(Debug, Default)]
struct CensusCounts {
    files: i64,
    parsed: i64,
    failed: i64,
}

fn read_indexed_census(row: &Row<'_>) -> rusqlite::Result<IndexedCensusRow> {
    Ok(IndexedCensusRow {
        language: row.get(0)?,
        files: row.get(1)?,
        parsed: row.get(2)?,
        failed: row.get(3)?,
    })
}

fn read_skipped_census(row: &Row<'_>) -> rusqlite::Result<SkippedCensusRow> {
    Ok(SkippedCensusRow {
        language: row.get(0)?,
        skipped: row.get(1)?,
    })
}

fn read_test_census(row: &Row<'_>) -> rusqlite::Result<TestCensusRow> {
    Ok(TestCensusRow {
        language: row.get(0)?,
        nodes: row.get(1)?,
    })
}

/// One file by its repo-relative path, over the unique `path` index (§4).
///
/// The path is normalized first, so `./src/a.ts` and `src/a.ts` are one lookup and not two rows.
/// `None` means "no `file` row", which is not the same as "not indexed": a skipped path has its
/// reason in `not_indexed` ([`crate::read::coverage::rows_for_paths`]), and `None` here is what
/// tells a caller to look there.
pub fn file_by_path(conn: &Conn, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM file f WHERE f.path = ?");
    let mut statement = conn.prepared(KEY_BY_PATH, |connection| connection.prepare_cached(&sql))?;
    statement
        .query_row([normalize_rel(path)], read_indexed_file)
        .optional()
        .map_err(|error| AppError::internal(format!("running {KEY_BY_PATH}: {error}")))
}

/// Every indexed file in path order. The staleness check needs the whole manifest at once
/// (`src/shared/domain/freshness.ts` compares each row's `sha256` against the bytes on disk), so
/// this one is deliberately unbounded — the caller's alternative is one query per file.
pub fn all_files(conn: &Conn) -> Result<Vec<IndexedFileRow>, AppError> {
    let sql = format!("SELECT {FILE_COLUMNS} FROM file f ORDER BY f.path");
    collect(
        conn.prepared(KEY_ALL, |connection| connection.prepare_cached(&sql))?,
        [],
        KEY_ALL,
        read_indexed_file,
    )
}

/// `{language, files, parsed, failed}` per language, with `not_indexed` folded in: every skipped
/// path adds one to `files` and one to `failed` for its language.
///
/// Languages are ordered by name with the unattributed bucket (`language: None`) last, so the
/// ordering stays stable for a caller that decides to truncate the list.
pub fn census_by_language(conn: &Conn) -> Result<Vec<LanguageCensus>, AppError> {
    const INDEXED_SQL: &str = concat!(
        "SELECT f.language AS language, COUNT(*) AS files,",
        " SUM(CASE WHEN f.parse_status = 'failed' THEN 0 ELSE 1 END) AS parsed,",
        " SUM(CASE WHEN f.parse_status = 'failed' THEN 1 ELSE 0 END) AS failed",
        " FROM file f GROUP BY f.language"
    );
    const SKIPPED_SQL: &str = concat!(
        "SELECT ni.language AS language, COUNT(*) AS skipped FROM not_indexed ni",
        " GROUP BY ni.language"
    );

    let indexed = collect(
        conn.prepared(KEY_CENSUS_INDEXED, |connection| {
            connection.prepare_cached(INDEXED_SQL)
        })?,
        [],
        KEY_CENSUS_INDEXED,
        read_indexed_census,
    )?;
    let skipped = collect(
        conn.prepared(KEY_CENSUS_SKIPPED, |connection| {
            connection.prepare_cached(SKIPPED_SQL)
        })?,
        [],
        KEY_CENSUS_SKIPPED,
        read_skipped_census,
    )?;

    // `BTreeMap` keyed by `Option<String>` puts the unattributed bucket first; the sort below is
    // what puts it last, so the intermediate order is never observable.
    let mut census: BTreeMap<Option<String>, CensusCounts> = BTreeMap::new();
    for row in indexed {
        let counts = census.entry(Some(row.language)).or_default();
        counts.files += row.files;
        counts.parsed += row.parsed;
        counts.failed += row.failed;
    }
    for row in skipped {
        let counts = census.entry(row.language).or_default();
        counts.files += row.skipped;
        counts.failed += row.skipped;
    }

    let mut out: Vec<LanguageCensus> = census
        .into_iter()
        .map(|(language, counts)| LanguageCensus {
            language,
            files: counts.files,
            parsed: counts.parsed,
            failed: counts.failed,
        })
        .collect();
    // Name ascending, the unattributed bucket last: the TypeScript comparator, spelled out.
    out.sort_by(|left, right| match (&left.language, &right.language) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    });
    Ok(out)
}

/// Indexed test symbols, and the languages they come from, ordered by size so the dominant test
/// corpus of a repo is the first entry a tool renders.
///
/// `nodes: 0` with `languages: []` is "this graph indexed no test symbol" — a property of the
/// index, not of a change, and the two must not be conflated. A clean tree therefore answers with
/// zeros, not an error: an empty `node` table is a complete, correct census.
///
/// `nodes` is the sum of the entries rather than a second `COUNT(*)`, so the total and the list it
/// summarises cannot disagree.
pub fn tests_census(conn: &Conn) -> Result<TestCensus, AppError> {
    const SQL: &str = concat!(
        "SELECT f.language AS language, COUNT(*) AS nodes FROM node n",
        " JOIN file f ON f.id = n.file_id WHERE n.is_test = 1",
        " GROUP BY f.language ORDER BY nodes DESC, f.language ASC"
    );
    let rows = collect(
        conn.prepared(KEY_TESTS_CENSUS, |connection| {
            connection.prepare_cached(SQL)
        })?,
        [],
        KEY_TESTS_CENSUS,
        read_test_census,
    )?;

    let mut languages = Vec::with_capacity(rows.len());
    let mut total: u64 = 0;
    for row in rows {
        let nodes = u64::try_from(row.nodes).map_err(|error| {
            AppError::internal(format!(
                "{KEY_TESTS_CENSUS} counted {} test nodes: {error}",
                row.nodes
            ))
        })?;
        total = total.saturating_add(nodes);
        languages.push(TestCensusEntry {
            // `file.language` is `NOT NULL`, so the fallback only covers a graph written by an
            // older schema.
            language: row.language.unwrap_or_else(|| "unknown".to_owned()),
            nodes,
        });
    }
    Ok(TestCensus {
        nodes: total,
        languages,
    })
}
