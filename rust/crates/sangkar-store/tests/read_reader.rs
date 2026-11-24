//! The aggregate `GraphReader` and its seam (`TECH-SPEC.md` §6): one assertion per method over a
//! hand-built database, the four analytics queries in their copied order, the pool's one-handle-
//! per-root rule, and a `ToolContext` a handler could be handed.
//!
//! The fixture is raw `INSERT`s, not an indexer run: the repository's `.sangkar/graph.db` belongs
//! to the daemon that rewrites it, so every row here is one this suite wrote and can name.
//!
//! Ordering is behaviour — `entry_points`, `hotspots`, `test_gaps` and `read_node_fts` decide which
//! rows a page shows — so each of those tests pins the full `ORDER BY`, including the tie-break, and
//! the fixture is built so a re-sorted answer fails: two entry points with different reachable
//! counts plus one unmeasured, two hotspots with the same total and the *larger* id in first
//! position, a `NULL`-depth gap ahead of a deep one, and two FTS documents identical in every
//! indexed column so equal `bm25` is a fact of the fixture rather than a hope.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use sangkar_core::config::load_config;
use sangkar_core::cursor::{Cursor, CursorExpectation, CursorPayload, CursorResult};
use sangkar_core::log::Level;
use sangkar_store::db::{open_reader, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::pool::ReaderPool;
use sangkar_store::read::context::{CursorFacade, LogField, Logger, ToolContext};
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    GraphMeta, GraphReader, LineRange, SqliteReader, TEST_GAP_MAX_DEPTH,
};

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
            "sangkar-reader-{label}-{}-{nanos}-{}",
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

/// The fixture, written as one batch after the migration.
///
/// Three files (`typescript` twice, `rust` once), three skipped paths — two under `src/`, one under
/// `vendor/` so the prefix test has a row it must exclude, one of them unattributed so the census
/// has a `language: NULL` bucket — eleven nodes (a class with two members, a second free function,
/// two test symbols, and two `needle` functions whose only difference is their id and file), four
/// edges, one metrics row per node except the two that must read as unmeasured, two generations
/// (the meta key names the *older* one, so `generation_seq` cannot be confused with the newest
/// publish), three tombstones across two generations, two unbound call sites, and an FTS mirror
/// whose two `needle` rows are identical in every indexed column.
const FIXTURE_SQL: &str = "\
INSERT INTO file (id, path, language, sha256, size, line_count, parse_status, parse_error_ranges, indexed_generation) VALUES
  (1, 'src/a.ts', 'typescript', 'sha-a', 900, 32, 'ok', NULL, 'gen-1'),
  (2, 'src/b.ts', 'typescript', 'sha-b', 600, 20, 'partial', '12-12', 'gen-1'),
  (3, 'src/c.rs', 'rust', 'sha-c', 300, 8, 'ok', NULL, 'gen-1');

INSERT INTO not_indexed (id, path, reason, detail, language) VALUES
  (1, 'src/huge.rs', 'size_cap', '2097152 bytes over the 1048576 cap', 'rust'),
  (2, 'src/blob.bin', 'binary', 'NUL byte at offset 12', NULL),
  (3, 'vendor/lib.rs', 'ignored', NULL, 'rust');

INSERT INTO node (id, file_id, parent_id, kind, name, qn, start_line, end_line, signature, docstring, is_test, props) VALUES
  (1, 1, NULL, 'class', 'Widget', 'src/a.ts::Widget', 1, 20, 'class Widget', 'A widget.', 0, '{}'),
  (2, 1, 1, 'method', 'paint', 'src/a.ts::Widget.paint', 4, 5, 'paint(): void', NULL, 0, '{\"visibility\":\"public\"}'),
  (3, 1, 1, 'method', 'resize', 'src/a.ts::Widget.resize', 7, 9, 'resize(w: number): void', NULL, 0, '{}'),
  (4, 1, NULL, 'function', 'compute', 'src/a.ts::compute', 22, 24, NULL, NULL, 0, '{}'),
  (5, 2, NULL, 'function', 'run', 'src/b.ts::run', 2, 4, 'run(): void', NULL, 0, '{}'),
  (6, 2, NULL, 'function', 'helper', 'src/b.ts::helper', 6, 8, NULL, NULL, 0, '{}'),
  (7, 2, NULL, 'function', 'test_run', 'src/b.ts::test_run', 10, 12, NULL, NULL, 1, '{}'),
  (8, 3, NULL, 'function', 'legacy', 'src/c.rs::legacy', 1, 3, NULL, NULL, 0, '{}'),
  (9, 3, NULL, 'function', 'test_legacy', 'src/c.rs::test_legacy', 5, 7, NULL, NULL, 1, '{}'),
  (21, 1, NULL, 'function', 'needle', 'src/a.ts::needle', 30, 31, NULL, NULL, 0, '{}'),
  (22, 2, NULL, 'function', 'needle', 'src/b.ts::needle', 15, 16, NULL, NULL, 0, '{}');

INSERT INTO edge (id, src_id, dst_id, kind, tier, file_id, line, local_name, evidence) VALUES
  (1, 4, 5, 'call', 'resolved', 1, 23, 'run', NULL),
  (2, 5, 6, 'call', 'resolved', 2, 3, 'helper', NULL),
  (3, 2, 4, 'call', 'resolved', 1, 5, 'compute', NULL),
  (4, 6, 5, 'call', 'resolved', 2, 7, 'run', NULL);

INSERT INTO node_metrics (node_id, in_degree, out_degree, is_entry, tested_depth, reachable_count) VALUES
  (1, 0, 0, 1, 0, 9),
  (2, 0, 0, 0, 5, NULL),
  (3, 0, 0, 0, 2, NULL),
  (4, 1, 1, 1, 3, 4),
  (5, 2, 1, 0, NULL, NULL),
  (6, 0, 0, 1, 1, NULL),
  (21, 0, 0, 0, NULL, NULL),
  (22, 3, 0, 0, NULL, NULL);

INSERT INTO generation (id, seq, published_at, base_commit, worktree_dirty, files_indexed, nodes, edges, duration_ms, mode, status) VALUES
  ('gen-1', 1, '2024-02-29T12:00:00.000Z', 'abc123', 0, 3, 11, 4, 1234, 'full', 'ok'),
  ('gen-2', 2, '2024-03-01T08:30:00.000Z', 'def456', 1, 3, 11, 4, 42, 'incremental', 'ok');

INSERT INTO meta (key, value) VALUES
  ('project_name', 'sangkar-fixture'),
  ('project_root', '/tmp/fixture-root'),
  ('project_branch', ''),
  ('project_remote', 'git@example.com:sangkar/fixture.git'),
  ('generation', 'gen-1'),
  ('indexer_state', 'idle'),
  ('consecutive_failures', '0'),
  ('embed_dim', '768');

INSERT INTO tombstone (qn, kind, file_id, last_seen_generation, removed_in_generation, had_callers) VALUES
  ('src/old.ts::gone', 'method', 1, 'gen-1', 'gen-2', 1),
  ('src/old.ts::away', 'function', 1, 'gen-1', 'gen-2', 0),
  ('src/old.ts::dropped', 'class', NULL, 'gen-1', 'gen-1', 0);

INSERT INTO unresolved (id, file_id, line, raw_target, evidence_kind, evidence) VALUES
  (1, 1, 13, 'missingFn', 'unbound_import', 'import { missingFn } from \"./nope\"'),
  (2, 3, 4, 'gone', NULL, NULL);

INSERT INTO node_fts_state (rowid_, name, qn, signature, docstring, props_text) VALUES
  (21, 'needle', 'src/a.ts::needle', NULL, NULL, '{}'),
  (22, 'needle', 'src/b.ts::needle', NULL, NULL, '{}'),
  (5, 'run', 'src/b.ts::run', 'run(): void', NULL, '{}');

INSERT INTO node_fts (node_fts) VALUES ('rebuild');
";

/// A migrated, seeded database under `<root>/.sangkar/graph.db` — the path
/// [`ReaderPool::reader`] derives — plus the directory that owns it.
fn seeded_with(label: &str, tweak: impl FnOnce(&Connection)) -> (TempDir, PathBuf) {
    let dir = TempDir::new(label);
    let root = dir.join("repo");
    let data_dir = root.join(".sangkar");
    fs::create_dir_all(&data_dir).expect("create the data dir");
    {
        let mut writer = open_writer(data_dir.join("graph.db")).expect("open the writer");
        migrate(writer.connection_mut()).expect("migrate the fixture");
        writer
            .connection()
            .execute_batch(FIXTURE_SQL)
            .expect("seed the fixture");
        tweak(writer.connection());
        writer.close().expect("close the writer");
    }
    (dir, root)
}

fn seeded(label: &str) -> (TempDir, PathBuf) {
    seeded_with(label, |_| {})
}

/// [`seeded`] read back through a **reader** handle — the mode the daemon serves answers from.
fn fixture_with(label: &str, tweak: impl FnOnce(&Connection)) -> (TempDir, SqliteReader) {
    let (dir, root) = seeded_with(label, tweak);
    let conn = open_reader(root.join(".sangkar").join("graph.db")).expect("open the reader");
    (dir, SqliteReader::new(conn))
}

fn fixture(label: &str) -> (TempDir, SqliteReader) {
    fixture_with(label, |_| {})
}

fn ids(rows: &[NodeRow]) -> Vec<i64> {
    rows.iter().map(|row| row.id).collect()
}

fn kinds(list: &[&str]) -> Vec<String> {
    list.iter().map(|kind| (*kind).to_owned()).collect()
}

fn ranked_ids<T>(rows: &[T], id: impl Fn(&T) -> i64) -> Vec<i64> {
    rows.iter().map(id).collect()
}

#[test]
fn find_by_qn_returns_the_whole_row_and_none_for_an_unknown_qn() {
    let (_dir, reader) = fixture("by-qn");

    let row = reader
        .find_by_qn("src/a.ts::Widget.paint")
        .expect("read the paint row")
        .expect("the method exists");
    assert_eq!(row.id, 2, "the joined file path and the parent come along");
    assert_eq!(row.path, "src/a.ts");
    assert_eq!(row.parent_id, Some(1));
    assert_eq!(row.kind, "method");
    assert_eq!((row.start_line, row.end_line), (4, 5));
    assert_eq!(
        row.props, "{\"visibility\":\"public\"}",
        "props stays raw text"
    );
    assert!(!row.is_test);
    assert_eq!(
        reader.find_by_qn("src/a.ts::nothing").expect("read a miss"),
        None,
        "the unique index answers at most one row and nothing for an absent qn"
    );
}

#[test]
fn find_by_name_orders_by_qn_and_narrows_on_kind() {
    let (_dir, reader) = fixture("by-name");

    let both = reader.find_by_name("needle", None).expect("read both rows");
    assert_eq!(ids(&both), vec![21, 22], "ORDER BY n.qn, copied verbatim");

    let classes = reader
        .find_by_name("Widget", Some(&kinds(&["class"])))
        .expect("read the class");
    assert_eq!(ids(&classes), vec![1]);

    let none = reader
        .find_by_name("Widget", Some(&kinds(&["function"])))
        .expect("no kind matches");
    assert!(none.is_empty(), "the kind filter is all-or-nothing");
}

#[test]
fn members_returns_direct_children_in_source_order() {
    let (_dir, reader) = fixture("members");

    let children = reader.members(1).expect("read the members");
    assert_eq!(
        ids(&children),
        vec![2, 3],
        "ORDER BY start_line, qn: paint (4) before resize (7)"
    );
    assert_eq!(children[0].qn, "src/a.ts::Widget.paint");
    assert!(reader.members(5).expect("a leaf").is_empty());
}

#[test]
fn nodes_by_file_returns_every_definition_in_source_order() {
    let (_dir, reader) = fixture("by-file");

    let file_one = reader.nodes_by_file(1).expect("read file one");
    assert_eq!(
        ids(&file_one),
        vec![1, 2, 3, 4, 21],
        "ORDER BY start_line, qn"
    );
    assert_eq!(
        ids(&reader.nodes_by_file(3).expect("read file three")),
        vec![8, 9]
    );
    assert!(reader.nodes_by_file(99).expect("no such file").is_empty());
}

#[test]
fn inbound_and_outbound_attach_the_far_endpoint() {
    let (_dir, reader) = fixture("edges");

    // Both edges into node 5 are `call`/`resolved`; line 7 belongs to the other file, so the copy
    // of `ORDER BY e.kind, e.tier, e.line, n.qn` is what puts edge 4 before edge 1.
    let callers = reader.inbound(5, None).expect("read the callers");
    assert_eq!(
        callers.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
        vec![4, 1]
    );
    assert_eq!(
        callers.iter().map(|edge| edge.node.id).collect::<Vec<_>>(),
        vec![6, 4],
        "the far end of an inbound edge is the caller"
    );

    let callees = reader.outbound(5, None).expect("read the callees");
    assert_eq!(
        callees.iter().map(|edge| edge.edge.id).collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        callees[0].node.id, 6,
        "the far end of an outbound edge is the callee"
    );

    let none = reader
        .outbound(5, Some(&kinds(&["import"])))
        .expect("filter on a kind nothing has");
    assert!(none.is_empty());
}

#[test]
fn metrics_for_reads_the_derived_row_and_reports_an_unmeasured_node_as_none() {
    let (_dir, reader) = fixture("metrics");

    let measured = reader
        .metrics_for(5)
        .expect("read the metrics")
        .expect("node 5 was measured");
    assert_eq!((measured.in_degree, measured.out_degree), (2, 1));
    assert!(!measured.is_entry);
    assert_eq!(
        measured.tested_depth, None,
        "NULL depth is 'no test reachable'"
    );
    assert_eq!(measured.reachable_count, None);

    assert_eq!(
        reader.metrics_for(9).expect("read an unmeasured node"),
        None,
        "a node with no metrics row was never measured, which is not degree 0"
    );
}

#[test]
fn rows_for_paths_answers_both_halves_of_a_prefix() {
    let (_dir, reader) = fixture("coverage");

    let coverage = reader
        .rows_for_paths(&["src".to_owned()])
        .expect("read the prefix");
    assert_eq!(
        coverage
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.ts", "src/b.ts", "src/c.rs"]
    );
    assert_eq!(
        coverage
            .not_indexed
            .iter()
            .map(|row| row.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/blob.bin", "src/huge.rs"],
        "vendor/ is outside the prefix"
    );
    assert_eq!((coverage.indexed, coverage.skipped), (3, 2));

    let vendor = reader
        .rows_for_paths(&["vendor".to_owned()])
        .expect("read the vendor prefix");
    assert_eq!(vendor.indexed, 0);
    assert_eq!(vendor.skipped, 1);
}

#[test]
fn not_indexed_all_and_count_agree_with_the_skipped_rows() {
    let (_dir, reader) = fixture("skipped");

    let all = reader.not_indexed_all().expect("read every skipped path");
    assert_eq!(
        all.iter().map(|row| row.path.as_str()).collect::<Vec<_>>(),
        vec!["src/blob.bin", "src/huge.rs", "vendor/lib.rs"],
        "ORDER BY path, reason; the unattributed row keeps its place"
    );
    assert_eq!(all[0].reason, "binary");
    assert_eq!(all[0].language, None);

    assert_eq!(
        reader.not_indexed_count().expect("count the skipped paths"),
        3,
        "the repo-wide total, not the prefix slice"
    );
}

#[test]
fn unresolved_for_files_returns_the_unbound_call_sites_of_those_files() {
    let (_dir, reader) = fixture("unresolved");

    let one = reader.unresolved_for_files(&[1]).expect("read file one");
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].line, 13);
    assert_eq!(one[0].raw_target, "missingFn");
    assert_eq!(one[0].file, "src/a.ts");

    let both = reader
        .unresolved_for_files(&[1, 3])
        .expect("read both files");
    assert_eq!(
        both.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![1, 2],
        "ORDER BY file_id, line, raw_target"
    );

    assert!(
        reader
            .unresolved_for_files(&[])
            .expect("no files")
            .is_empty(),
        "an empty frontier is answered without a statement"
    );
}

#[test]
fn file_by_path_and_all_files_read_the_same_row() {
    let (_dir, reader) = fixture("files");

    let partial = reader
        .file_by_path("src/b.ts")
        .expect("read the file")
        .expect("src/b.ts is indexed");
    assert_eq!(partial.id, 2);
    assert_eq!(partial.language, "typescript");
    assert_eq!(partial.parse_status, "partial");
    assert_eq!(partial.parse_error_ranges.as_deref(), Some("12-12"));
    assert_eq!(
        reader.file_by_path("src/nope.ts").expect("read a miss"),
        None,
        "None means 'look in not_indexed', not 'an indexed empty file'"
    );

    let manifest = reader.all_files().expect("read the manifest");
    assert_eq!(
        manifest
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src/a.ts", "src/b.ts", "src/c.rs"],
        "ORDER BY path: the staleness manifest"
    );
}

#[test]
fn census_by_language_and_tests_census_fold_the_two_tables() {
    let (_dir, reader) = fixture("census");

    let census = reader.census_by_language().expect("read the census");
    let names: Vec<&str> = census
        .iter()
        .map(|row| row.language.as_deref().unwrap_or("-"))
        .collect();
    assert_eq!(
        names,
        vec!["rust", "typescript", "-"],
        "name ascending, the unattributed bucket last"
    );
    let rust = &census[0];
    assert_eq!(
        (rust.files, rust.parsed, rust.failed),
        (3, 1, 2),
        "one parsed file plus two skipped paths, one of them in vendor/"
    );

    let tests = reader.tests_census().expect("read the test census");
    assert_eq!(tests.nodes, 2);
    assert_eq!(
        tests
            .languages
            .iter()
            .map(|entry| (entry.language.as_str(), entry.nodes))
            .collect::<Vec<_>>(),
        vec![("rust", 1), ("typescript", 1)],
        "ORDER BY nodes DESC, language ASC"
    );
}

#[test]
fn meta_for_reads_the_identity_and_the_generation_it_binds_to() {
    let (_dir, reader) = fixture("meta");

    assert_eq!(
        reader.meta_for().expect("read the meta"),
        GraphMeta {
            // Written by `migrate`: one ledger row per embedded migration file.
            schema_version: Some(2),
            generation: Some("gen-1".to_owned()),
            // `gen-1` is the publish the graph is serving; `latest_generation` is `gen-2`.
            generation_seq: Some(1),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("sangkar-fixture".to_owned()),
            project_root: Some("/tmp/fixture-root".to_owned()),
            // The indexer writes an empty string when git could not answer; a blank identity field
            // must read as absent, not as a name that is empty.
            project_branch: None,
            project_remote: Some("git@example.com:sangkar/fixture.git".to_owned()),
            embed_dim: Some(768),
        }
    );
}

#[test]
fn a_dropped_meta_key_reads_as_none_rather_than_an_error() {
    let (_dir, reader) = fixture_with("meta-missing", |connection| {
        connection
            .execute("DELETE FROM meta WHERE key = 'project_name'", [])
            .expect("drop the identity row");
    });
    let meta = reader.meta_for().expect("a dropped key is not a failure");
    assert_eq!(
        meta.project_name, None,
        "an absent identity is None, and the rest of the row still answers"
    );
    assert_eq!(meta.project_root.as_deref(), Some("/tmp/fixture-root"));
}

#[test]
fn latest_generation_picks_the_newest_publish_by_seq() {
    let (_dir, reader) = fixture("generations");

    let latest = reader
        .latest_generation()
        .expect("read the newest publish")
        .expect("two generations were published");
    assert_eq!(latest.id, "gen-2");
    assert_eq!(latest.seq, 2);
    assert_eq!(latest.published_at, "2024-03-01T08:30:00.000Z");
    assert_eq!(latest.base_commit.as_deref(), Some("def456"));
    assert!(latest.worktree_dirty, "worktree_dirty = 1 reads as true");
    assert_eq!(
        (latest.files_indexed, latest.nodes, latest.edges),
        (3, 11, 4)
    );
    assert_eq!(
        (
            latest.duration_ms,
            latest.mode.as_str(),
            latest.status.as_str()
        ),
        (42, "incremental", "ok")
    );
}

#[test]
fn removed_symbols_for_reads_the_tombstones_of_one_generation() {
    let (_dir, reader) = fixture("tombstones");

    let removed = reader
        .removed_symbols_for("gen-2")
        .expect("read the removals");
    assert_eq!(
        removed
            .iter()
            .map(|row| row.qn.as_str())
            .collect::<Vec<_>>(),
        vec!["src/old.ts::away", "src/old.ts::gone"],
        "ORDER BY qn, and only the rows removed in this generation"
    );
    assert_eq!(
        removed
            .iter()
            .map(|row| row.had_callers)
            .collect::<Vec<_>>(),
        vec![false, true],
        "a removed symbol that still had callers is the review signal"
    );
    assert_eq!(removed[1].file_id, Some(1));

    let older = reader
        .removed_symbols_for("gen-1")
        .expect("read the older removals");
    assert_eq!(older.len(), 1);
    assert_eq!(older[0].qn, "src/old.ts::dropped");
    assert_eq!(older[0].file_id, None);

    assert!(
        reader
            .removed_symbols_for("gen-9")
            .expect("a generation nothing was removed in")
            .is_empty(),
        "the predicate is an equality on removed_in_generation"
    );
}

#[test]
fn entry_points_order_by_reachable_count_descending_with_unmeasured_last() {
    let (_dir, reader) = fixture("entry-points");

    let entries = reader.entry_points(3).expect("read the entry points");
    assert_eq!(
        ranked_ids(&entries, |row| row.id),
        vec![1, 4, 6],
        "ORDER BY (reachable_count IS NULL) ASC, reachable_count DESC, n.id ASC"
    );
    assert_eq!(
        entries.iter().map(|row| row.reachable).collect::<Vec<_>>(),
        vec![Some(9), Some(4), None]
    );
    assert_eq!(
        entries[0].lines,
        LineRange { start: 1, end: 20 },
        "the identity columns every ranked query shares"
    );
    assert_eq!(entries[0].file, "src/a.ts");
    assert_eq!(entries[0].qn, "src/a.ts::Widget");
    assert_eq!(entries[0].kind, "class");

    let bounded = reader.entry_points(1).expect("read one entry point");
    assert_eq!(
        ranked_ids(&bounded, |row| row.id),
        vec![1],
        "limit is a bound"
    );
}

#[test]
fn hotspots_order_by_total_degree_descending_then_id_ascending() {
    let (_dir, reader) = fixture("hotspots");

    let hotspots = reader.hotspots(5).expect("read the hotspots");
    assert_eq!(
        ranked_ids(&hotspots, |row| row.id),
        vec![5, 22, 4, 1, 2],
        "ORDER BY (in_degree + out_degree) DESC, n.id ASC: 5 and 22 tie at 3, then 4, then the \
         zero-total rows by id"
    );
    assert_eq!(
        (hotspots[0].in_degree, hotspots[0].out_degree),
        (2, 1),
        "both degrees are read back, not just their sum"
    );
    assert_eq!(hotspots[1].in_degree, 3);
    assert_eq!(
        hotspots[4].id, 2,
        "the id tiebreak, not the insertion order"
    );
    assert_eq!(
        ranked_ids(&reader.hotspots(1).expect("read one hotspot"), |row| {
            row.id
        }),
        vec![5]
    );
}

#[test]
fn test_gaps_report_null_and_deep_depths_in_the_copied_order() {
    let (_dir, reader) = fixture("test-gaps");

    let gaps = reader.test_gaps(&[1, 2, 3, 5, 6]).expect("read the gaps");
    assert_eq!(
        ranked_ids(&gaps, |row| row.id),
        vec![5, 2],
        "ORDER BY (tested_depth IS NULL) DESC, tested_depth DESC, n.id ASC: the unmeasured gap \
         first, then the deep one; depth 0, 1 and 2 are not gaps"
    );
    assert_eq!(
        gaps.iter().map(|row| row.tested_depth).collect::<Vec<_>>(),
        vec![None, Some(5)]
    );
    assert_eq!(gaps[1].qn, "src/a.ts::Widget.paint");
    assert_eq!(TEST_GAP_MAX_DEPTH, 2);
}

#[test]
fn test_gaps_with_no_ids_runs_no_statement() {
    let (_dir, reader) = fixture("test-gaps-empty");

    let before = reader.cache_stats();
    let gaps = reader.test_gaps(&[]).expect("an empty frontier");
    let after = reader.cache_stats();

    assert!(gaps.is_empty(), "no candidate ids means no gaps");
    assert_eq!(
        before, after,
        "the counters are untouched: `IN ()` is not SQL, so the answer is known before the query"
    );
    assert_eq!(before.hits + before.misses, after.hits + after.misses);
}

#[test]
fn read_node_fts_orders_by_bm25_then_in_degree_then_id() {
    let (_dir, reader) = fixture("fts");

    let hits = reader
        .read_node_fts("needle", 10)
        .expect("search the index");
    assert_eq!(
        ranked_ids(&hits, |row| row.id),
        vec![22, 21],
        "both documents are identical in every indexed column, so bm25 ties and in_degree (3 > 0) \
         decides before the id"
    );
    assert_eq!(hits[0].score, hits[1].score, "the fixture's tie is real");
    assert!(
        hits[0].score < 0.0,
        "bm25 is the raw value: smaller is better, and a match is negative"
    );
    assert_eq!(hits[0].qn, "src/b.ts::needle");
    assert_eq!(hits[0].file, "src/b.ts");
    assert_eq!(hits[1].lines, LineRange { start: 30, end: 31 });
    assert!(
        hits.iter().all(|hit| hit.qn.contains("needle")),
        "the non-matching row in the index is not returned"
    );

    let first = reader.read_node_fts("needle", 1).expect("read one hit");
    assert_eq!(
        ranked_ids(&first, |row| row.id),
        vec![22],
        "limit is a bound"
    );
    assert!(
        reader
            .read_node_fts("absentterm", 10)
            .expect("search for nothing")
            .is_empty(),
        "a term the index does not hold answers empty, not an error"
    );
}

#[test]
fn the_pool_returns_one_handle_per_root() {
    let (_dir_a, root_a) = seeded("pool-a");
    let (_dir_b, root_b) = seeded("pool-b");
    let mut pool = ReaderPool::new();

    let first = pool.reader(&root_a).expect("open the first root");
    let again = pool
        .reader(&root_a.join("."))
        .expect("open the same root, spelled differently");
    assert!(
        Rc::ptr_eq(&first, &again),
        "the key is the canonical path, so one root is one handle"
    );
    assert_eq!(pool.len(), 1);
    assert!(pool.contains(&root_a), "the canonical root is the key");
    assert!(!pool.contains(&root_b));

    let other = pool.reader(&root_b).expect("open the second root");
    assert!(
        !Rc::ptr_eq(&first, &other),
        "a second root gets a handle of its own"
    );
    assert_eq!(pool.len(), 2);

    // The handle is a reader over the seeded graph, which is what a handler receives.
    let root_meta = first.meta_for().expect("read through the pooled handle");
    assert_eq!(root_meta.generation.as_deref(), Some("gen-1"));
}

#[test]
fn the_pool_evicts_on_the_callers_tick_and_reopens_afterwards() {
    let (_dir, root) = seeded("pool-idle");
    let mut pool = ReaderPool::new();
    let first = pool.reader(&root).expect("open the root");

    let now = Instant::now();
    assert_eq!(
        pool.evict_idle(now, Duration::from_secs(60)),
        0,
        "a handle just handed out is not idle"
    );
    assert_eq!(
        pool.evict_idle(now + Duration::from_secs(120), Duration::from_secs(60)),
        1,
        "the caller ticks; the pool owns no timer"
    );
    assert!(pool.is_empty());

    let reopened = pool.reader(&root).expect("reopen after eviction");
    assert!(
        !Rc::ptr_eq(&first, &reopened),
        "an evicted handle is gone; the next call opens a fresh one"
    );
    assert_eq!(
        reopened
            .latest_generation()
            .ok()
            .flatten()
            .map(|row| row.id),
        Some("gen-2".to_owned()),
        "the fresh handle reads the same file"
    );
}

/// What the fake sink recorded, formatted so the assertion can name the scope and the field types.
static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn capture(level: Level, msg: &str, fields: &[(&str, LogField)]) {
    let line = format!("{} {msg} {fields:?}", level.as_str());
    CAPTURED.lock().expect("capture lock").push(line);
}

fn fixed_clock() -> String {
    "2024-03-01T09:00:00.000Z".to_owned()
}

fn cursor_key() -> Vec<u8> {
    b"a-test-key-of-at-least-32-bytes!!".to_vec()
}

#[test]
fn a_tool_context_is_six_values_and_no_environment() {
    let (dir, root) = seeded("context");
    let mut pool = ReaderPool::new();
    let reader: Rc<dyn GraphReader> = pool.reader(&root).expect("open the reader");
    let cursor: Rc<dyn CursorFacade> = Rc::new(
        Cursor::new(cursor_key(), Some("fixture".to_owned())).expect("bind the cursor key"),
    );
    // The environment is injected: this config describes a machine that exists only inside the
    // fixture, which is the property the seam exists for.
    let home = dir.join("home");
    fs::create_dir_all(&home).expect("create the fake home");
    let home_text = home.to_string_lossy().into_owned();
    let lookup = move |key: &str| match key {
        "SANGKAR_HOME" | "HOME" => Some(home_text.clone()),
        _ => None,
    };
    let config = Rc::new(load_config(&[], &lookup, &root).expect("resolve the config"));

    let ctx = ToolContext {
        reader,
        cursor,
        log: Logger::with_sink("summary", capture),
        now: fixed_clock,
        config,
        db_size_bytes: 4242,
    };

    assert_eq!(
        (ctx.now)(),
        "2024-03-01T09:00:00.000Z",
        "the clock is injected"
    );
    assert_eq!(ctx.db_size_bytes, 4242);
    assert_eq!(
        ctx.config.repo_root, root,
        "the config names the fixture tree"
    );
    assert_eq!(ctx.log.tool(), "summary", "the logger is pre-scoped");
    ctx.log.warn(
        "status.db_size_unavailable",
        &[
            ("size", LogField::Int(0)),
            ("empty", LogField::Bool(true)),
            ("path", LogField::from("src/a.ts")),
        ],
    );
    let captured = CAPTURED.lock().expect("capture lock");
    assert_eq!(captured.len(), 1);
    let line = &captured[0];
    assert!(
        line.starts_with("warn status.db_size_unavailable"),
        "{line}"
    );
    assert!(line.contains(r#"("tool", Str("summary"))"#), "{line}");
    assert!(line.contains(r#"("size", Int(0))"#), "{line}");
    assert!(line.contains(r#"("empty", Bool(true))"#), "{line}");
    drop(captured);

    let payload = CursorPayload {
        generation: "gen-1".to_owned(),
        tool: "summary".to_owned(),
        args_hash: "sha256:abc".to_owned(),
        position: 2,
    };
    let token = ctx.cursor.sign(&payload);
    let expected = CursorExpectation {
        generation: "gen-1".to_owned(),
        tool: "summary".to_owned(),
        args_hash: "sha256:abc".to_owned(),
    };
    assert_eq!(
        ctx.cursor.verify(&token, &expected),
        CursorResult::Accepted { position: 2 },
        "a handler can mint and verify, and cannot reach the key"
    );

    assert_eq!(
        ctx.reader
            .meta_for()
            .expect("read through the trait object")
            .generation
            .as_deref(),
        Some("gen-1"),
        "the reader field is the aggregate, not a connection"
    );
}

/// The pool and the context agree on the file they open: `<root>/.sangkar/graph.db`.
#[test]
fn the_pool_derives_the_data_dir_path() {
    let (_dir, root) = seeded("pool-path");
    assert!(Path::new(&root.join(".sangkar").join("graph.db")).exists());
    let mut pool = ReaderPool::new();
    assert!(pool.reader(&root).is_ok());

    let (empty_dir, unindexed) = seeded("pool-empty");
    fs::remove_file(unindexed.join(".sangkar").join("graph.db")).expect("remove the index");
    assert!(
        pool.reader(&unindexed).is_err(),
        "a root with no index fails instead of opening an empty database"
    );
    drop(empty_dir);
}
