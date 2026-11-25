//! `finalize_generation` against a real migrated database: the derivations that are only cheap once
//! (degrees, `tested_by`, `tested_depth`), the FTS mirror delta and its rebuild, the tombstones a
//! publish names, and the generation row plus `meta` keys it writes.
//!
//! Every test builds its own graph under a temp directory and migrates it with the store's own
//! runner, exactly as a stage is. The rows are written with SQL rather than through
//! `persist_unit` — what the unit pass does with a file is proved in `tests/persist_unit.rs`, and
//! what is under test here is what the seal does with the graph that pass left behind.
//!
//! The fixture is the one the TypeScript suite uses: `src/widget.ts` declares `render` and `helper`
//! and `render` calls `helper`; `tests/widget.test.ts` declares a test that calls `render`;
//! `src/other.ts` is the untouched control, which nothing tests and nothing calls.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_index::persist::PENDING_GENERATION;
use sangkar_index::persist::finalize::{
    FTS_REBUILD_FRACTION, FtsMode, FtsReport, GenerationInput, GenerationResult, MetricsMode,
    ProjectIdentity, finalize_generation,
};
use sangkar_store::db::{Conn, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::publish::begin_write;
use sangkar_store::{ToSql, Transaction, params};

const WIDGET: &str = "src/widget.ts";
const OTHER: &str = "src/other.ts";
const TEST_FILE: &str = "tests/widget.test.ts";

const WIDGET_FILE_QN: &str = "src/widget.ts::widget.ts";
const RENDER: &str = "src/widget.ts::render";
const HELPER: &str = "src/widget.ts::helper";
const OTHER_FILE_QN: &str = "src/other.ts::other.ts";
const LONELY: &str = "src/other.ts::lonely";
const TEST_FILE_QN: &str = "tests/widget.test.ts::widget.test.ts";
const TEST_NODE: &str = "tests/widget.test.ts::test_render";

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
            "sangkar-finalize-{label}-{}-{nanos}-{}",
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

/// A migrated graph in a temp directory: the shipped migrations through the one runner.
fn migrated(label: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(label);
    let path = dir.join("graph.db");
    let mut writer = open_writer(&path).expect("open the writer");
    migrate(writer.connection_mut()).expect("migrate a fresh graph");
    writer.close().expect("close the writer");
    (dir, path)
}

/// One write phase: `begin_write`'s `BEGIN IMMEDIATE`, the caller's callback, then the release. The
/// scope is closed before the value comes back, so a test may read the file with a second handle.
fn in_transaction<T, F>(path: &Path, run: F) -> Result<T, AppError>
where
    F: FnOnce(&Transaction<'_>) -> Result<T, AppError>,
{
    let mut scope = begin_write(path).expect("open the write scope");
    let outcome = scope.transaction(run);
    scope.close().expect("release the write scope");
    outcome
}

/// A handle for the assertions: the write scope is closed, so nothing contends for the file.
fn handle(path: &Path) -> Conn {
    open_writer(path).expect("open the graph")
}

/// One publish written the way the worker writes it: the graph's rows, then the seal.
fn publish(path: &Path, input: &GenerationInput) -> GenerationResult {
    in_transaction(path, |tx| finalize_generation(tx, input)).expect("finalize the generation")
}

/// The input a test publishes with: an injected clock, so the row is comparable; `mode` stays the
/// caller's because it is part of what the guard checks.
fn input(generation_id: &str, mode: &str) -> GenerationInput {
    GenerationInput {
        generation_id: generation_id.to_owned(),
        mode: mode.to_owned(),
        published_at: Some("2026-01-01T00:00:00.000Z".to_owned()),
        ..GenerationInput::default()
    }
}

/// The worker's half of the contract: the `file` row and its staleness manifest. A re-persisted
/// path updates the manifest in place, so the id — and therefore every node's `file_id` — survives.
fn add_file(conn: &Conn, path: &str) -> i64 {
    conn.connection()
        .execute(
            "INSERT INTO file (path, language, sha256, size, line_count, parse_status,
             indexed_generation) VALUES (?1, 'typescript', ?2, 120, 9, 'ok', 'g-0')
             ON CONFLICT(path) DO UPDATE SET sha256 = excluded.sha256",
            params![path, "a".repeat(64)],
        )
        .expect("write the file row");
    conn.connection()
        .query_row("SELECT id FROM file WHERE path = ?1", [path], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read the file id")
}

fn add_node(conn: &Conn, file_id: i64, kind: &str, name: &str, qn: &str, is_test: bool) -> i64 {
    let is_test = if is_test { 1 } else { 0 };
    conn.connection()
        .query_row(
            "INSERT INTO node (file_id, kind, name, qn, start_line, end_line, is_test)
             VALUES (?1, ?2, ?3, ?4, 1, 3, ?5) RETURNING id",
            params![file_id, kind, name, qn, is_test],
            |row| row.get::<_, i64>(0),
        )
        .expect("write the node row")
}

fn add_edge(conn: &Conn, file_id: i64, kind: &str, src: i64, dst: i64, line: i64) {
    conn.connection()
        .execute(
            "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)
             VALUES (?1, ?2, ?3, 'extracted', ?4, ?5, '', ?3)",
            params![src, dst, kind, file_id, line],
        )
        .expect("write the edge row");
}

fn one_i64(conn: &Conn, sql: &str, bound: &[&dyn ToSql]) -> i64 {
    conn.connection()
        .query_row(sql, bound, |row| row.get::<_, i64>(0))
        .expect("read one integer")
}

/// `(in_degree, out_degree, tested_depth)` for the node at `qn`.
fn metrics_of(conn: &Conn, qualified_name: &str) -> (i64, i64, Option<i64>) {
    conn.connection()
        .query_row(
            "SELECT m.in_degree, m.out_degree, m.tested_depth FROM node_metrics m
             JOIN node n ON n.id = m.node_id WHERE n.qn = ?1",
            [qualified_name],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read one node's metrics")
}

/// `(qn, tested_depth)` for every node, in qn order, so a null is visible where it belongs.
fn depths(conn: &Conn) -> Vec<(String, Option<i64>)> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT n.qn, m.tested_depth FROM node n JOIN node_metrics m ON m.node_id = n.id
             ORDER BY n.qn",
        )
        .expect("prepare the depth read");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("read the depths");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the depths")
}

/// `(src qn, dst qn, tier)` for every derived edge, in a stable order.
fn tested_by_edges(conn: &Conn) -> Vec<(String, String, String)> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT s.qn, d.qn, e.tier FROM edge e
             JOIN node s ON s.id = e.src_id JOIN node d ON d.id = e.dst_id
             WHERE e.kind = 'tested_by' ORDER BY s.qn, d.qn",
        )
        .expect("prepare the tested_by read");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("read the tested_by edges");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the tested_by edges")
}

/// `(id, seq, status)` for every publish, in seq order.
fn generation_rows(conn: &Conn) -> Vec<(String, i64, String)> {
    let mut statement = conn
        .connection()
        .prepare("SELECT id, seq, status FROM generation ORDER BY seq")
        .expect("prepare the generation read");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("read the generations");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the generations")
}

/// Every `meta` row, in key order: the shape a reader serves, so an empty value stays visible.
fn meta_rows(conn: &Conn) -> Vec<(String, String)> {
    let mut statement = conn
        .connection()
        .prepare("SELECT key, value FROM meta ORDER BY key")
        .expect("prepare the meta read");
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("read the meta rows");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the meta rows")
}

fn meta_value(conn: &Conn, key: &str) -> Option<String> {
    conn.connection()
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
}

/// The node ids the FTS index answers `query` with, in rowid order.
fn fts_hits(conn: &Conn, query: &str) -> Vec<i64> {
    let mut statement = conn
        .connection()
        .prepare("SELECT rowid FROM node_fts WHERE node_fts MATCH ?1 ORDER BY rowid")
        .expect("prepare the FTS read");
    let rows = statement
        .query_map([query], |row| row.get::<_, i64>(0))
        .expect("read the FTS hits");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the FTS hits")
}

fn node_id(conn: &Conn, qualified_name: &str) -> i64 {
    one_i64(
        conn,
        "SELECT id FROM node WHERE qn = ?1",
        &[&qualified_name],
    )
}

/// The three-file graph: every node and every edge of the fixture, written in one go.
fn widget_graph(conn: &Conn) {
    let widget = add_file(conn, WIDGET);
    let other = add_file(conn, OTHER);
    let test = add_file(conn, TEST_FILE);

    let widget_file = add_node(conn, widget, "file", "widget.ts", WIDGET_FILE_QN, false);
    let render = add_node(conn, widget, "function", "render", RENDER, false);
    let helper = add_node(conn, widget, "function", "helper", HELPER, false);
    let other_file = add_node(conn, other, "file", "other.ts", OTHER_FILE_QN, false);
    let lonely = add_node(conn, other, "function", "lonely", LONELY, false);
    let test_file = add_node(conn, test, "file", "widget.test.ts", TEST_FILE_QN, true);
    let test_node = add_node(conn, test, "test", "test_render", TEST_NODE, true);

    add_edge(conn, widget, "contains", widget_file, render, 3);
    add_edge(conn, widget, "contains", widget_file, helper, 8);
    add_edge(conn, widget, "calls", render, helper, 4);
    add_edge(conn, other, "contains", other_file, lonely, 3);
    add_edge(conn, test, "contains", test_file, test_node, 3);
    add_edge(conn, test, "calls", test_node, render, 4);
}

/// The four identity keys exactly as stored, so an empty value stays visible here.
fn identity_meta(conn: &Conn) -> Vec<(String, String)> {
    meta_rows(conn)
        .into_iter()
        .filter(|(key, _)| key.starts_with("project_"))
        .collect()
}

fn identity(name: &str, root: &str, branch: &str, remote: &str) -> ProjectIdentity {
    ProjectIdentity {
        name: name.to_owned(),
        root: root.to_owned(),
        branch: branch.to_owned(),
        remote: remote.to_owned(),
    }
}

#[test]
fn derives_tested_by_and_the_two_depths_behind_it() {
    let (_dir, path) = migrated("tested");
    let conn = handle(&path);
    widget_graph(&conn);
    conn.close().expect("close the handle");

    assert_eq!(FTS_REBUILD_FRACTION, 0.35);
    let first = publish(&path, &input("g1", "full"));
    assert_eq!(first.nodes, 7);
    // Six extracted edges plus the one derived `tested_by`: the derived row is part of the graph.
    assert_eq!(first.edges, 7);
    assert!(first.tombstones.is_empty());
    assert_eq!(first.metrics.mode, MetricsMode::Neighbourhood);
    // Every node of the fixture is touched, and every node is within two hops of one, so the walk
    // visits the whole table without the run having removed anything.
    assert_eq!(first.metrics.rows, 7);
    // Every node is new, so the fraction is 1 and the first publish of a graph rebuilds.
    assert_eq!(first.fts.mode, FtsMode::Rebuild);
    assert_eq!(first.fts.rows, 7);
    assert_eq!(first.fts.inserted, 7);
    assert_eq!(first.fts.removed, 0);
    assert_eq!(first.fts.fraction, 1.0);

    let conn = handle(&path);
    // The inversion is deliberate: the edge runs from the test to the symbol, which is what puts it
    // in the queried symbol's inbound group.
    assert_eq!(
        tested_by_edges(&conn),
        [(
            TEST_NODE.to_owned(),
            RENDER.to_owned(),
            "inferred".to_owned()
        )]
    );
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM edge e JOIN node s ON s.id = e.src_id
             WHERE e.kind = 'tested_by' AND s.is_test = 0",
            &[],
        ),
        0
    );

    assert_eq!(
        depths(&conn),
        [
            (LONELY.to_owned(), None),
            (OTHER_FILE_QN.to_owned(), None),
            (HELPER.to_owned(), Some(2)),
            (RENDER.to_owned(), Some(1)),
            (WIDGET_FILE_QN.to_owned(), None),
            (TEST_NODE.to_owned(), None),
            (TEST_FILE_QN.to_owned(), None),
        ]
    );
    // Degrees count every edge, the derived one included: `render` is contained in its file, called
    // by the test and tested by it; the test node is contained, calls and sources the derived edge.
    assert_eq!(metrics_of(&conn, RENDER), (3, 1, Some(1)));
    assert_eq!(metrics_of(&conn, TEST_NODE), (1, 2, None));
    assert_eq!(metrics_of(&conn, HELPER), (2, 0, Some(2)));
}

#[test]
fn recounts_a_node_written_after_the_last_publish() {
    let (_dir, path) = migrated("touched");
    let conn = handle(&path);
    let file = add_file(&conn, WIDGET);
    let widget_file = add_node(&conn, file, "file", "widget.ts", WIDGET_FILE_QN, false);
    let render = add_node(&conn, file, "function", "render", RENDER, false);
    let helper = add_node(&conn, file, "function", "helper", HELPER, false);
    add_edge(&conn, file, "contains", widget_file, render, 3);
    add_edge(&conn, file, "calls", render, helper, 4);
    conn.close().expect("close the handle");

    publish(&path, &input("g1", "full"));

    // A node with no metrics row is exactly what "written in this run" looks like, so a second
    // publish derives the touched set from that absence instead of being told which files changed.
    let conn = handle(&path);
    let extra = add_node(
        &conn,
        file,
        "function",
        "extra",
        "src/widget.ts::extra",
        false,
    );
    add_edge(&conn, file, "calls", helper, extra, 12);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM node", &[]), 4);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM node_metrics", &[]), 3);
    conn.close().expect("close the handle");

    let second = publish(&path, &input("g2", "fast"));
    assert_eq!(second.metrics.mode, MetricsMode::Neighbourhood);
    // The new node and the two hops behind it: `helper`, then `render`. The file node is one hop
    // further out and nothing incident to it changed, so it keeps the degree the previous publish
    // gave it.
    assert_eq!(second.metrics.rows, 3);
    assert_eq!(second.nodes, 4);

    let conn = handle(&path);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM node_metrics", &[]), 4);
    assert_eq!(metrics_of(&conn, "src/widget.ts::extra"), (1, 0, None));
    // `helper` gained an outgoing edge, which is the degree a re-written edge moves at its far end.
    assert_eq!(metrics_of(&conn, HELPER), (1, 1, None));
}

#[test]
fn settles_the_tombstones_it_names() {
    let (_dir, path) = migrated("tombstone");
    let conn = handle(&path);
    let file = add_file(&conn, WIDGET);
    add_node(&conn, file, "file", "widget.ts", WIDGET_FILE_QN, false);
    // Two removals a run recorded, in an order that is not qn order: the writer stages them pending
    // and only a publish may name the generation that removed them.
    for gone in [
        "src/widget.ts::away",
        "src/widget.ts::gone",
        "src/widget.ts::also_gone",
    ] {
        conn.connection()
            .execute(
                "INSERT INTO tombstone
                 (qn, kind, file_id, last_seen_generation, removed_in_generation, had_callers)
                 VALUES (?1, 'function', NULL, 'g1', ?2, 1)",
                params![gone, PENDING_GENERATION],
            )
            .expect("record a pending tombstone");
    }
    conn.close().expect("close the handle");

    let result = publish(&path, &input("g2", "fast"));
    assert_eq!(
        result.tombstones,
        [
            "src/widget.ts::also_gone".to_owned(),
            "src/widget.ts::away".to_owned(),
            "src/widget.ts::gone".to_owned(),
        ]
    );
    // A removal deleted its incident edges by cascade, so the degree pass recounts the whole table
    // rather than trust a neighbourhood whose adjacency is already gone.
    assert_eq!(result.metrics.mode, MetricsMode::All);
    assert_eq!(result.metrics.rows, 1);

    let conn = handle(&path);
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM tombstone WHERE removed_in_generation = ?1",
            &[&PENDING_GENERATION],
        ),
        0
    );
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM tombstone WHERE removed_in_generation = 'g2'",
            &[],
        ),
        3
    );
}

/// One file of `count` declarations named `<prefix>1..<prefix>N`, all contained in the file node.
/// The ids are returned in insertion order, so a re-write can be spelled as "delete these, insert
/// those".
fn add_declarations(conn: &Conn, path: &str, prefix: &str, count: i64) -> Vec<i64> {
    let file = add_file(conn, path);
    let name = path.rsplit('/').next().unwrap_or(path);
    let file_node = add_node(conn, file, "file", name, &format!("{path}::{name}"), false);
    let mut ids = vec![file_node];
    for index in 1..=count {
        let symbol = format!("{path}::{prefix}{index}");
        let node = add_node(
            conn,
            file,
            "function",
            &format!("{prefix}{index}"),
            &symbol,
            false,
        );
        add_edge(conn, file, "contains", file_node, node, index + 2);
        ids.push(node);
    }
    ids
}

/// Deletes every row of one file's nodes: what a re-persisted file does, minus the bookkeeping. Node
/// ids are never reused, so the delete leaves the FTS mirror holding rows no node owns.
fn delete_file_nodes(conn: &Conn, file_path: &str) {
    conn.connection()
        .execute(
            "DELETE FROM node WHERE file_id = (SELECT id FROM file WHERE path = ?1)",
            [file_path],
        )
        .expect("delete the file's nodes");
}

#[test]
fn applies_an_fts_delta_below_the_threshold_and_rebuilds_above_it() {
    let (_dir, path) = migrated("fts");
    let conn = handle(&path);
    let alpha = add_declarations(&conn, "src/a.ts", "alpha", 1);
    let bee = add_declarations(&conn, "src/b.ts", "b", 20);
    conn.close().expect("close the handle");

    let first = publish(&path, &input("g1", "full"));
    assert_eq!(first.nodes, 23);
    assert_eq!(
        first.fts,
        FtsReport {
            mode: FtsMode::Rebuild,
            inserted: 23,
            removed: 0,
            rows: 23,
            fraction: 1.0,
        }
    );

    // Two rows of 23 is 0.087: a rewritten file is patched row by row, not rebuilt.
    let conn = handle(&path);
    delete_file_nodes(&conn, "src/a.ts");
    add_declarations(&conn, "src/a.ts", "beta", 1);
    let beta = node_id(&conn, "src/a.ts::beta1");
    conn.close().expect("close the handle");

    let second = publish(&path, &input("g2", "fast"));
    assert_eq!(second.fts.mode, FtsMode::Delta);
    assert_eq!(second.fts.rows, 23);
    assert_eq!(second.fts.inserted, 2);
    assert_eq!(second.fts.removed, 2);
    assert_eq!(second.fts.fraction, 4.0 / 23.0);

    let conn = handle(&path);
    // FTS5's `delete` command needs the old column values, so the removed name is only gone if the
    // mirror was read before its row was dropped.
    assert!(fts_hits(&conn, "alpha1").is_empty());
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM node_fts_state WHERE rowid_ = ?1",
            &[&alpha[1]],
        ),
        0
    );
    // The delta inserted the new nodes into both the mirror and the index, so the new name is
    // searchable without a rebuild, and the untouched file is still searchable as well.
    assert_eq!(fts_hits(&conn, "beta1"), [beta]);
    assert_eq!(fts_hits(&conn, "b7"), [bee[7]]);
    conn.close().expect("close the handle");

    // Re-persisting all 21 nodes of `src/b.ts` costs 21 stale mirror rows and 21 missing ones. 42 of
    // 23 is above the threshold, so the index is rebuilt from the mirror instead of patched.
    let conn = handle(&path);
    delete_file_nodes(&conn, "src/b.ts");
    let bee = add_declarations(&conn, "src/b.ts", "b", 20);
    conn.close().expect("close the handle");

    let third = publish(&path, &input("g3", "fast"));
    assert_eq!(third.nodes, 23);
    assert_eq!(third.fts.mode, FtsMode::Rebuild);
    assert_eq!(third.fts.rows, 23);
    assert_eq!(third.fts.inserted, 23);
    assert_eq!(third.fts.removed, 21);
    assert_eq!(third.fts.fraction, 42.0 / 23.0);

    let conn = handle(&path);
    assert_eq!(fts_hits(&conn, "b13"), [bee[13]]);
    // The file `src/a.ts` was untouched by this run, and a rebuild keeps what it did not rewrite.
    assert_eq!(fts_hits(&conn, "beta1"), [beta]);
    assert_eq!(
        one_i64(&conn, "SELECT count(*) FROM node_fts_state", &[]),
        23
    );
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM node", &[]), 23);
}

#[test]
fn rebuilds_when_half_the_mirror_is_stale() {
    let (_dir, path) = migrated("threshold");
    let conn = handle(&path);
    let declarations = add_declarations(&conn, "src/a.ts", "a", 5);
    conn.close().expect("close the handle");

    let first = publish(&path, &input("g1", "full"));
    assert_eq!(first.nodes, 6);

    // Two of the six mirror rows lose their node, and nothing replaces them: `rows` after the
    // delete is four, so the fraction is 2/4 = 0.5. That is the region the threshold exists for —
    // half the mirror is stale, and patching row by row costs more than rebuilding.
    let conn = handle(&path);
    for id in &declarations[1..3] {
        conn.connection()
            .execute("DELETE FROM node WHERE id = ?1", [id])
            .expect("delete a node");
    }
    conn.close().expect("close the handle");

    let second = publish(&path, &input("g2", "fast"));
    assert_eq!(second.nodes, 4);
    assert_eq!(
        second.fts,
        FtsReport {
            mode: FtsMode::Rebuild,
            inserted: 4,
            removed: 2,
            rows: 4,
            fraction: 0.5,
        }
    );

    let conn = handle(&path);
    assert!(fts_hits(&conn, "a1").is_empty());
    assert_eq!(
        one_i64(&conn, "SELECT count(*) FROM node_fts_state", &[]),
        4
    );
}

#[test]
fn three_publishes_produce_seq_one_two_three_with_only_the_last_published() {
    let (_dir, path) = migrated("sequence");
    let conn = handle(&path);
    let file = add_file(&conn, WIDGET);
    add_node(&conn, file, "file", "widget.ts", WIDGET_FILE_QN, false);
    conn.close().expect("close the handle");

    let mut first = input("g1", "full");
    first.published_at = Some("2026-01-01T00:00:00.000Z".to_owned());
    first.duration_ms = 12;
    first.worktree_dirty = true;
    first.base_commit = Some("abc1234".to_owned());
    publish(&path, &first);

    // A second file arrives between the publishes: `files_indexed` is the graph's file count, not
    // the number of files this run wrote.
    let conn = handle(&path);
    let other = add_file(&conn, OTHER);
    add_node(&conn, other, "file", "other.ts", OTHER_FILE_QN, false);
    conn.close().expect("close the handle");

    let mut second = input("g2", "fast");
    second.published_at = Some("2026-01-02T00:00:00.000Z".to_owned());
    second.duration_ms = 4;
    publish(&path, &second);

    // The third publish injects no clock, so the row is stamped by the module's own.
    let mut third = input("g3", "fast");
    third.published_at = None;
    let result = publish(&path, &third);

    let conn = handle(&path);
    assert_eq!(
        generation_rows(&conn),
        [
            ("g1".to_owned(), 1, "superseded".to_owned()),
            ("g2".to_owned(), 2, "superseded".to_owned()),
            ("g3".to_owned(), 3, "published".to_owned()),
        ]
    );
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM generation WHERE status = 'published'",
            &[],
        ),
        1
    );
    assert_eq!(meta_value(&conn, "generation").as_deref(), Some("g3"));
    assert_eq!(meta_value(&conn, "generation_seq").as_deref(), Some("3"));

    let (published_at, base_commit, worktree_dirty, files_indexed, nodes, edges, duration, mode) =
        conn.connection()
            .query_row(
                "SELECT published_at, base_commit, worktree_dirty, files_indexed, nodes, edges,
                 duration_ms, mode FROM generation WHERE id = 'g1'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .expect("read the first generation row");
    assert_eq!(published_at, "2026-01-01T00:00:00.000Z");
    assert_eq!(base_commit.as_deref(), Some("abc1234"));
    assert_eq!(worktree_dirty, 1);
    assert_eq!(files_indexed, 1);
    assert_eq!(nodes, 1);
    assert_eq!(edges, 0);
    assert_eq!(duration, 12);
    assert_eq!(mode, "full");

    // The second publish saw two files and two nodes, although this run wrote one of each.
    let (files_indexed, nodes) = conn
        .connection()
        .query_row(
            "SELECT files_indexed, nodes FROM generation WHERE id = 'g2'",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("read the second generation row");
    assert_eq!((files_indexed, nodes), (2, 2));

    let stamped = conn
        .connection()
        .query_row(
            "SELECT published_at FROM generation WHERE id = 'g3'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read the third generation row");
    assert_eq!(
        stamped.len(),
        24,
        "an ISO instant with milliseconds: {stamped}"
    );
    assert!(stamped.ends_with('Z') && stamped.contains('T'), "{stamped}");

    // Nothing was touched before the third publish, so the walk visited nothing and the delta had
    // nothing to move: the zero-fraction branch, which is the one a rebuild would be wrong for.
    assert_eq!(result.metrics.mode, MetricsMode::Neighbourhood);
    assert_eq!(result.metrics.rows, 0);
    assert_eq!(result.fts.mode, FtsMode::Delta);
    assert_eq!(result.fts.fraction, 0.0);
    assert_eq!(result.fts.inserted, 0);
    assert_eq!(result.fts.removed, 0);
}

#[test]
fn refuses_an_unnamed_unknown_or_repeated_generation() {
    let (_dir, path) = migrated("guards");
    let conn = handle(&path);
    let file = add_file(&conn, WIDGET);
    add_node(&conn, file, "file", "widget.ts", WIDGET_FILE_QN, false);
    conn.close().expect("close the handle");

    // The empty id is the marker an unconfirmed tombstone carries: publishing under it would make a
    // removal indistinguishable from a generation name.
    let unnamed = in_transaction(&path, |tx| {
        finalize_generation(tx, &input(PENDING_GENERATION, "full"))
    })
    .expect_err("the pending marker is not a generation id");
    assert_eq!(unnamed.code, ErrorCode::INTERNAL);
    assert_eq!(
        unnamed.hint.as_deref(),
        Some("\"\" marks an unconfirmed tombstone, not a publish")
    );

    // `mode` is the closed pair a reader filters the publish history by.
    let unknown = in_transaction(&path, |tx| {
        finalize_generation(tx, &input("g1", "incremental"))
    })
    .expect_err("a mode outside the closed set is refused");
    assert_eq!(unknown.code, ErrorCode::INTERNAL);
    assert_eq!(unknown.message, "unknown generation mode incremental");
    assert_eq!(
        unknown.hint.as_deref(),
        Some("generation.mode is the closed set full | fast")
    );

    publish(&path, &input("g1", "full"));
    let repeated = in_transaction(&path, |tx| finalize_generation(tx, &input("g1", "fast")))
        .expect_err("a publish is recorded once");
    assert_eq!(repeated.code, ErrorCode::INTERNAL);
    assert_eq!(repeated.message, "generation g1 is already recorded");
    assert!(
        repeated
            .hint
            .as_deref()
            .unwrap_or_default()
            .starts_with("a publish is recorded once")
    );

    // Neither refusal wrote anything: the one successful publish is the only generation, and it is
    // still the one the reader is served.
    let conn = handle(&path);
    assert_eq!(
        generation_rows(&conn),
        [("g1".to_owned(), 1, "published".to_owned())]
    );
    assert_eq!(meta_value(&conn, "generation").as_deref(), Some("g1"));
}

#[test]
fn records_the_tree_identity_once_and_refreshes_its_branch() {
    let (_dir, path) = migrated("identity");
    let conn = handle(&path);
    let file = add_file(&conn, OTHER);
    add_node(&conn, file, "file", "other.ts", OTHER_FILE_QN, false);
    conn.close().expect("close the handle");

    let mut first = input("g1", "full");
    first.project = Some(identity(
        "widgets",
        "/checkout/widgets",
        "main",
        "git@example.invalid:acme/widgets.git",
    ));
    publish(&path, &first);

    let conn = handle(&path);
    assert_eq!(
        identity_meta(&conn),
        [
            ("project_branch".to_owned(), "main".to_owned()),
            ("project_name".to_owned(), "widgets".to_owned()),
            (
                "project_remote".to_owned(),
                "git@example.invalid:acme/widgets.git".to_owned()
            ),
            ("project_root".to_owned(), "/checkout/widgets".to_owned()),
        ]
    );
    conn.close().expect("close the handle");

    // A second publish on another branch: name and root are recorded once (the root is the
    // `root_mismatch` guard), while branch and remote describe the tree as it is now.
    let mut second = input("g2", "fast");
    second.project = Some(identity("renamed", "/checkout/elsewhere", "feature", ""));
    publish(&path, &second);

    let conn = handle(&path);
    assert_eq!(
        identity_meta(&conn),
        [
            ("project_branch".to_owned(), "feature".to_owned()),
            ("project_name".to_owned(), "widgets".to_owned()),
            ("project_remote".to_owned(), String::new()),
            ("project_root".to_owned(), "/checkout/widgets".to_owned()),
        ]
    );
    conn.close().expect("close the handle");

    // A caller with no filesystem opinion leaves the four keys alone rather than blanking them.
    publish(&path, &input("g3", "fast"));
    let conn = handle(&path);
    assert_eq!(
        identity_meta(&conn),
        [
            ("project_branch".to_owned(), "feature".to_owned()),
            ("project_name".to_owned(), "widgets".to_owned()),
            ("project_remote".to_owned(), String::new()),
            ("project_root".to_owned(), "/checkout/widgets".to_owned()),
        ]
    );
}
