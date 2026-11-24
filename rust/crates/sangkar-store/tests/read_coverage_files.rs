//! The absence side of the graph (`DATABASE.md` §3.2, §6, §7): what was skipped and why, which
//! files cover a prefix, which unbound call sites live in them, and the per-language census.
//!
//! Ported from the coverage and file-census halves of the TypeScript read suite. Every test builds
//! its own migrated database and writes its own rows: the repository's `.sangkar/graph.db` belongs
//! to the daemon that rewrites it, so a fixture here is rows this suite wrote and can name.
//!
//! The fixture is deliberately about the *edges* of the prefix test — a `src-old/` sibling of `src`
//! on both tables, one skipped path per reason, one language with no `not_indexed.language` — plus
//! two languages with test nodes and three unresolved rows with three different `evidence_kind`
//! values.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::envelope::TestCensus;
use sangkar_store::db::{Conn, open_reader, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::read::coverage::{
    IndexedFileRow, SkippedPathRow, not_indexed_all, not_indexed_count, rows_for_paths,
    unresolved_for_files,
};
use sangkar_store::read::files::{all_files, census_by_language, file_by_path, tests_census};

/// A temporary directory that removes itself, so the suite leaves no database behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-read-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The rows every fixture carries, written as one batch after the migration.
///
/// `file` holds three files under `src/`, a `src-old/x.rs` sibling, a `lib/d.ts` and a
/// `docs/readme.md`; `not_indexed` holds one path per reason, including a `src-old/` sibling so the
/// prefix test is exercised on both tables. `unresolved` keeps four rows across two files with
/// three distinct `evidence_kind` values, numbered so the order the query must produce is visible.
const FIXTURE_SQL: &str = "\
INSERT INTO file (id, path, language, sha256, size, line_count, parse_status, parse_error_ranges, indexed_generation) VALUES
  (1, 'src/a.rs', 'rust', 'sha-rust-a', 120, 12, 'ok', NULL, 'gen-1'),
  (2, 'src/b.go', 'go', 'sha-go-b', 240, 24, 'partial', '12-12', 'gen-1'),
  (3, 'src/c.rs', 'rust', 'sha-rust-c', 60, 6, 'failed', NULL, 'gen-1'),
  (4, 'src-old/x.rs', 'rust', 'sha-rust-old', 90, 9, 'ok', NULL, 'gen-1'),
  (5, 'lib/d.ts', 'typescript', 'sha-ts-d', 300, 30, 'ok', NULL, 'gen-1'),
  (6, 'docs/readme.md', 'markdown', 'sha-md', 30, 3, 'ok', NULL, 'gen-1');

INSERT INTO not_indexed (id, path, reason, detail, language) VALUES
  (1, 'src/skip.ignored', 'ignored', NULL, 'rust'),
  (2, 'src/binary.png', 'binary', 'NUL byte at offset 12', NULL),
  (3, 'src/huge.rs', 'size_cap', '2097152 bytes over the 1048576 cap', 'rust'),
  (4, 'src/blob.xyz', 'unsupported_ext', 'no grammar for .xyz', NULL),
  (5, 'lib/locked.ts', 'unreadable', 'EACCES: permission denied', 'typescript'),
  (6, 'src-old/legacy.rs', 'ignored', NULL, 'rust');

INSERT INTO node (id, file_id, kind, name, qn, start_line, end_line, is_test) VALUES
  (1, 1, 'function', 'main', 'src/a.rs::main', 1, 3, 0),
  (2, 1, 'function', 'parses', 'src/a.rs::tests.parses', 8, 10, 1),
  (3, 2, 'function', 'TestThing', 'src/b.go::TestThing', 5, 7, 1),
  (4, 5, 'function', 'spec', 'lib/d.ts::spec', 2, 4, 1),
  (5, 4, 'function', 'legacy', 'src-old/x.rs::legacy', 1, 2, 0);

INSERT INTO unresolved (id, file_id, line, raw_target, evidence_kind, evidence) VALUES
  (1, 1, 10, 'missing_fn', 'unbound_import', 'import { missingFn } from \"./nope\"'),
  (2, 1, 4, 'pkg.deep', 'external_module', NULL),
  (3, 1, 20, 'alias.fn', 'dynamic_require', 'require(name)'),
  (4, 4, 1, 'other_fn', 'unbound_import', NULL);
";

/// A migrated database with no rows at all: the clean tree the census must answer zeros for.
fn migrated(label: &str) -> (TempDir, Conn) {
    let dir = TempDir::new(label);
    let path = dir.join("graph.db");
    {
        let mut writer = open_writer(&path).expect("open the writer");
        migrate(writer.connection_mut()).expect("migrate the fixture");
        writer.close().expect("close the writer");
    }
    let reader = open_reader(&path).expect("open the reader");
    (dir, reader)
}

/// [`migrated`] plus [`FIXTURE_SQL`], read back through a **reader** handle — the mode the daemon
/// serves answers from, so the prefix test is checked on the read-only path it runs on.
fn fixture(label: &str) -> (TempDir, Conn) {
    let dir = TempDir::new(label);
    let path = dir.join("graph.db");
    {
        let mut writer = open_writer(&path).expect("open the writer");
        migrate(writer.connection_mut()).expect("migrate the fixture");
        writer
            .connection()
            .execute_batch(FIXTURE_SQL)
            .expect("insert the fixture rows");
        writer.close().expect("close the writer");
    }
    let reader = open_reader(&path).expect("open the reader");
    (dir, reader)
}

fn prefixes(list: &[&str]) -> Vec<String> {
    list.iter().map(|prefix| (*prefix).to_owned()).collect()
}

fn paths(rows: &[IndexedFileRow]) -> Vec<&str> {
    rows.iter().map(|row| row.path.as_str()).collect()
}

fn skipped_projection(rows: &[SkippedPathRow]) -> Vec<(&str, &str, Option<&str>, Option<&str>)> {
    rows.iter()
        .map(|row| {
            (
                row.path.as_str(),
                row.reason.as_str(),
                row.detail.as_deref(),
                row.language.as_deref(),
            )
        })
        .collect()
}

/// Both `prefix_bounds` failure modes in one assertion set: the subdirectory is matched by the
/// strict range, and the sibling that sorts on the *other side* of `/` is not. A `LIKE 'src%'` or a
/// `path >= 'src'` predicate would return `src-old/x.rs` here.
#[test]
fn a_prefix_matches_its_subtree_and_never_its_sibling() {
    let (_dir, conn) = fixture("prefix-sibling");
    let coverage = rows_for_paths(&conn, &prefixes(&["src"])).expect("answer the src prefix");

    assert_eq!(paths(&coverage.files), ["src/a.rs", "src/b.go", "src/c.rs"]);
    assert!(
        !coverage
            .files
            .iter()
            .any(|row| row.path.starts_with("src-old")),
        "src-old is a sibling of src, not a child: {:?}",
        paths(&coverage.files)
    );
    assert_eq!(
        coverage
            .not_indexed
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        [
            "src/binary.png",
            "src/blob.xyz",
            "src/huge.rs",
            "src/skip.ignored"
        ],
        "the same prefix test answers the not_indexed half, in `path, reason` order"
    );

    // The number and the list come from the same pass, so they cannot disagree.
    assert_eq!(coverage.indexed, coverage.files.len());
    assert_eq!(coverage.skipped, coverage.not_indexed.len());
    assert_eq!((coverage.indexed, coverage.skipped), (3, 4));

    // The sibling's files are reachable, from its own prefix: the exclusion above is the bounds,
    // not a missing row.
    let sibling =
        rows_for_paths(&conn, &prefixes(&["src-old"])).expect("answer the src-old prefix");
    assert_eq!(paths(&sibling.files), ["src-old/x.rs"]);
    assert_eq!(sibling.skipped, 1);
}

/// The equality half of the bounds, and two prefixes merged into one `path`-ordered answer.
#[test]
fn a_prefix_is_matched_by_equality_or_by_a_merged_range() {
    let (_dir, conn) = fixture("prefix-merge");

    let single = rows_for_paths(&conn, &prefixes(&["docs/readme.md"])).expect("answer one path");
    assert_eq!(single.files.len(), 1);
    assert_eq!(single.files[0].id, 6);
    assert_eq!(single.indexed, 1);
    assert!(single.not_indexed.is_empty());

    // `lib` sorts before `src`, and the answer is one list in SQL order, not two concatenated
    // per-prefix lists.
    let merged = rows_for_paths(&conn, &prefixes(&["src", "lib"])).expect("answer two prefixes");
    assert_eq!(
        paths(&merged.files),
        ["lib/d.ts", "src/a.rs", "src/b.go", "src/c.rs"]
    );
    assert_eq!(
        merged
            .not_indexed
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        [
            "lib/locked.ts",
            "src/binary.png",
            "src/blob.xyz",
            "src/huge.rs",
            "src/skip.ignored"
        ]
    );
    assert_eq!((merged.indexed, merged.skipped), (4, 5));
}

/// One row per skip reason, with the `detail` and nullable `language` columns intact, in the
/// `ORDER BY ni.path, ni.reason` order a `status` answer renders.
#[test]
fn every_skip_reason_is_preserved_in_path_and_reason_order() {
    let (_dir, conn) = fixture("skip-reasons");
    let rows = not_indexed_all(&conn).expect("every skipped path");

    assert_eq!(
        skipped_projection(&rows),
        [
            (
                "lib/locked.ts",
                "unreadable",
                Some("EACCES: permission denied"),
                Some("typescript")
            ),
            ("src-old/legacy.rs", "ignored", None, Some("rust")),
            (
                "src/binary.png",
                "binary",
                Some("NUL byte at offset 12"),
                None
            ),
            (
                "src/blob.xyz",
                "unsupported_ext",
                Some("no grammar for .xyz"),
                None
            ),
            (
                "src/huge.rs",
                "size_cap",
                Some("2097152 bytes over the 1048576 cap"),
                Some("rust")
            ),
            ("src/skip.ignored", "ignored", None, Some("rust")),
        ],
        "the five reasons this fixture writes are ignored, binary, size_cap, \
         unsupported_ext and unreadable"
    );

    let mut reasons: Vec<&str> = rows.iter().map(|row| row.reason.as_str()).collect();
    reasons.sort_unstable();
    reasons.dedup();
    assert_eq!(
        reasons,
        [
            "binary",
            "ignored",
            "size_cap",
            "unreadable",
            "unsupported_ext"
        ]
    );
}

/// The headline total: one `COUNT(*)`, the same number as the list it counts.
#[test]
fn the_repo_wide_skip_total_equals_the_list_it_counts() {
    let (_dir, conn) = fixture("skip-total");
    let count = not_indexed_count(&conn).expect("count the skipped paths");
    let rows = not_indexed_all(&conn).expect("list the skipped paths");
    assert_eq!(count, 6);
    assert_eq!(
        count,
        i64::try_from(rows.len()).expect("a row count fits in i64")
    );
}

/// A clean prefix list is answered without a statement: the counters of that handle's cache do not
/// move, and the same handle's earlier *non-empty* call did move them, so the probe is not vacuous.
#[test]
fn an_empty_prefix_list_returns_two_empty_lists_and_issues_no_query() {
    let (_dir, conn) = fixture("empty-prefix");

    let warm = rows_for_paths(&conn, &prefixes(&["src"])).expect("warm both prefix statements");
    assert_eq!(warm.indexed, 3);
    let before = conn.statement_cache_stats();
    assert!(
        before.misses >= 2,
        "the two prefix queries registered as misses: {before:?}"
    );

    let empty = rows_for_paths(&conn, &[]).expect("an empty prefix list is not an error");
    assert!(empty.files.is_empty());
    assert!(empty.not_indexed.is_empty());
    assert_eq!((empty.indexed, empty.skipped), (0, 0));

    let after = conn.statement_cache_stats();
    assert_eq!(
        (after.size, after.hits, after.misses, after.evictions),
        (before.size, before.hits, before.misses, before.evictions),
        "no statement may be prepared for an empty prefix list"
    );
}

/// Unbound call sites: `file.path` joined in, one row per site, in `file_id, line, raw_target`
/// order regardless of the id order the caller passed.
#[test]
fn unresolved_rows_carry_their_file_path_and_their_query_order() {
    let (_dir, conn) = fixture("unresolved");

    let rows = unresolved_for_files(&conn, &[1]).expect("the call sites of one file");
    assert_eq!(
        rows.iter()
            .map(|row| (
                row.line,
                row.raw_target.as_str(),
                row.evidence_kind.as_deref(),
            ))
            .collect::<Vec<_>>(),
        [
            (4, "pkg.deep", Some("external_module")),
            (10, "missing_fn", Some("unbound_import")),
            (20, "alias.fn", Some("dynamic_require")),
        ],
        "ordered by line, and three distinct evidence_kind values survive"
    );
    assert!(
        rows.iter()
            .all(|row| row.file_id == 1 && row.file == "src/a.rs"),
        "every row carries the path of the file it lives in: {rows:?}"
    );
    assert_eq!(
        rows[1].evidence.as_deref(),
        Some("import { missingFn } from \"./nope\"")
    );

    // The filter is the id, not the path: `src-old/x.rs` has its own row and is only returned when
    // its id is asked for.
    assert!(
        unresolved_for_files(&conn, &[1])
            .expect("re-ask one file")
            .iter()
            .all(|row| row.file != "src-old/x.rs")
    );
    let both = unresolved_for_files(&conn, &[4, 1]).expect("two files");
    assert_eq!(
        both.iter().map(|row| row.file_id).collect::<Vec<_>>(),
        [1, 1, 1, 4],
        "rows are ordered by file_id whatever order the ids arrived in"
    );
    assert_eq!(both[3].file, "src-old/x.rs");
    assert_eq!(both[3].raw_target, "other_fn");

    // A file with no unbound site is an empty answer, not an error.
    assert!(
        unresolved_for_files(&conn, &[2])
            .expect("a covered file")
            .is_empty()
    );
}

/// A file list that selects nothing is answered without a statement, by the same probe as the
/// empty prefix list.
#[test]
fn an_empty_file_list_returns_no_rows_and_issues_no_query() {
    let (_dir, conn) = fixture("empty-files");

    let warm = unresolved_for_files(&conn, &[1]).expect("warm the unresolved statement");
    assert_eq!(warm.len(), 3);
    let before = conn.statement_cache_stats();
    assert!(
        before.misses >= 1,
        "the unresolved query registered as a miss"
    );

    let empty = unresolved_for_files(&conn, &[]).expect("no files is not an error");
    assert!(empty.is_empty());

    let after = conn.statement_cache_stats();
    assert_eq!(
        (after.size, after.hits, after.misses, after.evictions),
        (before.size, before.hits, before.misses, before.evictions),
        "no statement may be prepared for an empty file list"
    );
}

/// One lookup by path, normalized first, with `None` reserved for "no `file` row".
#[test]
fn a_file_lookup_normalizes_the_path_and_reports_absence_as_none() {
    let (_dir, conn) = fixture("file-by-path");

    let found = file_by_path(&conn, "./src/b.go")
        .expect("read one file")
        .expect("`./src/b.go` and `src/b.go` are one row");
    assert_eq!(found.id, 2);
    assert_eq!(found.language, "go");
    assert_eq!(found.parse_status, "partial");
    assert_eq!(found.parse_error_ranges.as_deref(), Some("12-12"));
    assert_eq!(
        (found.size, found.line_count),
        (240, 24),
        "the columns the staleness manifest reads survive"
    );
    assert_eq!(found.indexed_generation, "gen-1");

    assert!(
        file_by_path(&conn, "src/missing.rs")
            .expect("read an absent file")
            .is_none()
    );
    // A skipped path has a `not_indexed` row and no `file` row: `None` here is what tells a caller
    // to look there.
    assert!(
        file_by_path(&conn, "src/huge.rs")
            .expect("read a skipped path")
            .is_none()
    );
}

/// The whole manifest, in `path` order, with `src-old/` sorted before `src/` because `-` < `/`.
#[test]
fn every_indexed_file_is_listed_in_path_order() {
    let (_dir, conn) = fixture("all-files");
    let rows = all_files(&conn).expect("the whole manifest");
    assert_eq!(
        paths(&rows),
        [
            "docs/readme.md",
            "lib/d.ts",
            "src-old/x.rs",
            "src/a.rs",
            "src/b.go",
            "src/c.rs"
        ]
    );
    assert_eq!(rows.len(), 6);
}

/// The census folds `not_indexed` into its language's `failed`, keeps `files = parsed + failed`, and
/// puts the unattributed bucket last.
#[test]
fn the_census_folds_skipped_paths_into_failed_and_puts_the_unattributed_last() {
    let (_dir, conn) = fixture("census");
    let census = census_by_language(&conn).expect("the language census");

    assert_eq!(
        census
            .iter()
            .map(|row| (row.language.as_deref(), row.files, row.parsed, row.failed))
            .collect::<Vec<_>>(),
        [
            // 1 `ok` file; nothing skipped.
            (Some("go"), 1, 1, 0),
            (Some("markdown"), 1, 1, 0),
            // 3 `file` rows (1 failed) + 3 skipped paths = 6, and `parsed` stays 2.
            (Some("rust"), 6, 2, 4),
            // 1 `ok` file + 1 unreadable path.
            (Some("typescript"), 2, 1, 1),
            // binary + unsupported_ext, which named no language.
            (None, 2, 0, 2),
        ]
    );
    for row in &census {
        assert_eq!(
            row.files,
            row.parsed + row.failed,
            "`files` is always `parsed + failed`: {row:?}"
        );
    }
}

/// `nodes` counts only `is_test = 1`, grouped by `file.language`, largest corpus first and ties
/// broken by language name.
#[test]
fn the_test_census_counts_per_language_and_sums_its_own_entries() {
    let (_dir, conn) = fixture("tests-census");
    let census = tests_census(&conn).expect("the test census");

    assert_eq!(
        census.nodes, 3,
        "the three `is_test = 1` nodes, not the five"
    );
    assert_eq!(
        census
            .languages
            .iter()
            .map(|entry| (entry.language.as_str(), entry.nodes))
            .collect::<Vec<_>>(),
        [("go", 1), ("rust", 1), ("typescript", 1)],
        "one test node per language, ties ordered by language name"
    );
    assert_eq!(
        census.nodes,
        census
            .languages
            .iter()
            .map(|entry| entry.nodes)
            .sum::<u64>(),
        "the total is the sum of the entries it publishes"
    );
}

/// A migrated database with no rows is the clean tree: the census answers zeros rather than an
/// error, because "no test symbol was indexed" is a property of the index and not a failure.
#[test]
fn a_clean_tree_answers_an_empty_census_and_an_empty_manifest() {
    let (_dir, conn) = migrated("clean-tree");

    assert_eq!(
        tests_census(&conn).expect("a clean tree is not an error"),
        TestCensus {
            nodes: 0,
            languages: Vec::new()
        }
    );
    assert!(
        census_by_language(&conn)
            .expect("a clean census")
            .is_empty()
    );
    assert!(all_files(&conn).expect("an empty manifest").is_empty());
    assert_eq!(
        not_indexed_count(&conn).expect("no skipped paths"),
        0,
        "a graph that skipped nothing counts zero, not a missing row"
    );
    assert!(not_indexed_all(&conn).expect("no skipped paths").is_empty());
    assert!(
        rows_for_paths(&conn, &prefixes(&["src"]))
            .expect("a prefix over an empty graph")
            .indexed
            == 0
    );
}
