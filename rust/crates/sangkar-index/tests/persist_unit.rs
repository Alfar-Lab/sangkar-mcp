//! `persist_unit` against a real migrated database: what one file's rows look like after a write,
//! what a rewrite of the same unit leaves behind, and every row the pass records when a symbol or a
//! site stops being declared.
//!
//! Every test builds its own graph under a temp directory and migrates it with the store's own
//! runner, exactly as a stage is: `<repo>/.sangkar/graph.db` belongs to the daemon that rewrites it,
//! so a fixture here is rows this suite wrote and can name. The units are hand-built rather than
//! extracted — grammar work is proved in `tests/extract_declare.rs` and binding in `tests/resolve.rs`
//! — because what is under test is what the writer does with either.
//!
//! The `file` rows are written here rather than by [`persist_unit`]: the manifest belongs to the
//! worker, and the pass only reads the id.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::paths::qn;
use sangkar_index::extract::{ExtractedNode, NodeProps, UnresolvedSite};
use sangkar_index::persist::{
    NotIndexedEntry, NotIndexedReason, PENDING_GENERATION, PersistCounts, PersistUnit, persist_unit,
};
use sangkar_index::resolve::{BindResult, ResolvedEdge, UnresolvedBinding};
use sangkar_store::db::{Conn, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::publish::begin_write;
use sangkar_store::{ToSql, Transaction, params};

const WIDGET: &str = "src/widget.ts";
const OTHER: &str = "src/other.ts";
const TEST_FILE: &str = "tests/widget.test.ts";

const WIDGET_FILE_QN: &str = "src/widget.ts::widget.ts";
const RENDER_QN: &str = "src/widget.ts::render";
const HELPER_QN: &str = "src/widget.ts::helper";
const OTHER_FILE_QN: &str = "src/other.ts::other.ts";
const LONELY_QN: &str = "src/other.ts::lonely";
const ONE_RENDER_QN: &str = "src/one.ts::render";

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
            "sangkar-persist-{label}-{}-{nanos}-{}",
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

/// The worker's half of the contract: the `file` row and its staleness manifest, written outside
/// the unit so the pass can only ever read its id.
fn add_file(conn: &Conn, path: &str, parse_status: &str) -> i64 {
    conn.connection()
        .execute(
            "INSERT INTO file (path, language, sha256, size, line_count, parse_status, indexed_generation)
             VALUES (?1, 'typescript', ?2, 120, 9, ?3, 'g-0')",
            params![path, "a".repeat(64), parse_status],
        )
        .expect("write the file row");
    conn.connection()
        .query_row("SELECT id FROM file WHERE path = ?1", [path], |row| {
            row.get::<_, i64>(0)
        })
        .expect("read the file id")
}

fn one_i64(conn: &Conn, sql: &str, params: &[&dyn ToSql]) -> i64 {
    conn.connection()
        .query_row(sql, params, |row| row.get::<_, i64>(0))
        .expect("read one integer")
}

/// Every qn the graph holds, in qn order, so a duplicate is visible as a repeated line.
fn qns(conn: &Conn) -> Vec<String> {
    let mut statement = conn
        .connection()
        .prepare("SELECT qn FROM node ORDER BY qn")
        .expect("prepare the qn read");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("read the qns");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the qns")
}

/// `(file.path, parent's qn, is_test, props, signature)`: the columns a hand-built `ExtractedNode`
/// has to reach unchanged.
type NodeColumns = (String, Option<String>, i64, String, Option<String>);

/// [`NodeColumns`] for every node, in qn order.
fn node_columns(conn: &Conn) -> Vec<NodeColumns> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT f.path, p.qn, n.is_test, n.props, n.signature FROM node n
             JOIN file f ON f.id = n.file_id LEFT JOIN node p ON p.id = n.parent_id ORDER BY n.qn",
        )
        .expect("prepare the node read");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("read the nodes");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the nodes")
}

/// `(src qn, dst qn, kind, file id of the site)` for every edge, in a stable order.
fn edge_rows(conn: &Conn) -> Vec<(String, String, String, i64)> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT s.qn, d.qn, e.kind, e.file_id FROM edge e
             JOIN node s ON s.id = e.src_id JOIN node d ON d.id = e.dst_id
             ORDER BY s.qn, d.qn, e.kind",
        )
        .expect("prepare the edge read");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("read the edges");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the edges")
}

/// `(qn, kind, last_seen_generation, removed_in_generation, had_callers)` for every tombstone.
fn tombstone_rows(conn: &Conn) -> Vec<(String, String, String, String, i64)> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT qn, kind, last_seen_generation, removed_in_generation, had_callers
             FROM tombstone ORDER BY qn",
        )
        .expect("prepare the tombstone read");
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .expect("read the tombstones");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the tombstones")
}

/// `(path, reason, detail, language)` for every coverage row.
fn coverage_rows(conn: &Conn) -> Vec<(String, String, Option<String>, Option<String>)> {
    let mut statement = conn
        .connection()
        .prepare("SELECT path, reason, detail, language FROM not_indexed ORDER BY path, reason")
        .expect("prepare the coverage read");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("read the coverage rows");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the coverage rows")
}

/// `(line, raw_target, evidence_kind, evidence)` for every unresolved site.
fn unresolved_rows(conn: &Conn) -> Vec<(i64, String, Option<String>, Option<String>)> {
    let mut statement = conn
        .connection()
        .prepare(
            "SELECT line, raw_target, evidence_kind, evidence FROM unresolved ORDER BY line, raw_target",
        )
        .expect("prepare the unresolved read");
    let rows = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("read the unresolved rows");
    rows.collect::<Result<Vec<_>, _>>()
        .expect("collect the unresolved rows")
}

fn decl(kind: &str, path: &str, name: &str, start_line: u32) -> ExtractedNode {
    ExtractedNode {
        kind: kind.to_owned(),
        name: name.to_owned(),
        qn: qn(path, "", name),
        start_line,
        end_line: start_line + 2,
        signature: None,
        is_test: false,
        props: NodeProps::new(),
    }
}

fn file_node(path: &str, is_test: bool) -> ExtractedNode {
    let name = path.rsplit('/').next().unwrap_or(path);
    ExtractedNode {
        is_test,
        ..decl("file", path, name, 1)
    }
}

fn unit(rel_path: &str, nodes: Vec<ExtractedNode>) -> PersistUnit {
    PersistUnit {
        rel_path: rel_path.to_owned(),
        nodes,
        raw_edges: Vec::new(),
        unresolved: Vec::new(),
        not_indexed: Vec::new(),
    }
}

fn edge(file: &str, kind: &str, src: &str, dst: &str, line: u32) -> ResolvedEdge {
    ResolvedEdge {
        file: file.to_owned(),
        kind: kind.to_owned(),
        src: src.to_owned(),
        dst: dst.to_owned(),
        tier: "extracted".to_owned(),
        line,
        local_name: String::new(),
        evidence: kind.to_owned(),
    }
}

fn resolved_of(edges: Vec<ResolvedEdge>) -> BindResult {
    BindResult {
        edges,
        unresolved: Vec::new(),
    }
}

/// `src/widget.ts`: the file node, `render` and `helper`, and the containment that hangs them both
/// off the file node.
fn widget_nodes(include_helper: bool) -> Vec<ExtractedNode> {
    let mut nodes = vec![
        file_node(WIDGET, false),
        decl("function", WIDGET, "render", 3),
    ];
    if include_helper {
        nodes.push(decl("function", WIDGET, "helper", 8));
    }
    nodes
}

fn widget_edges(include_helper: bool) -> Vec<ResolvedEdge> {
    let mut edges = vec![edge(WIDGET, "contains", WIDGET_FILE_QN, RENDER_QN, 3)];
    if include_helper {
        edges.push(edge(WIDGET, "contains", WIDGET_FILE_QN, HELPER_QN, 8));
        edges.push(edge(WIDGET, "calls", RENDER_QN, HELPER_QN, 4));
    }
    edges
}

/// The one unit whose rewrite the suite checks: two unbound sites, one from each producer.
fn widget_unit(include_helper: bool) -> (PersistUnit, BindResult) {
    let unit = PersistUnit {
        unresolved: vec![UnresolvedSite {
            line: 5,
            raw_target: "mystery".to_owned(),
            evidence_kind: "dynamic".to_owned(),
            evidence: "call:mystery".to_owned(),
        }],
        ..unit(WIDGET, widget_nodes(include_helper))
    };
    let mut resolved = resolved_of(widget_edges(include_helper));
    resolved.unresolved.push(UnresolvedBinding {
        file: WIDGET.to_owned(),
        line: 6,
        raw_target: "missing".to_owned(),
        evidence_kind: "receiver_type_unknown".to_owned(),
        evidence: "call:missing".to_owned(),
    });
    (unit, resolved)
}

/// [`OTHER`]': the file node and `lonely`, which calls into `src/widget.ts`.
fn other_unit() -> (PersistUnit, BindResult) {
    (
        unit(
            OTHER,
            vec![
                file_node(OTHER, false),
                decl("function", OTHER, "lonely", 3),
            ],
        ),
        resolved_of(vec![
            edge(OTHER, "contains", OTHER_FILE_QN, LONELY_QN, 3),
            edge(OTHER, "calls", LONELY_QN, HELPER_QN, 4),
        ]),
    )
}

/// Writes one generation's worth of units inside one transaction, the way the worker does.
fn persist_all(path: &Path, writes: &[(&PersistUnit, &BindResult)]) -> Vec<PersistCounts> {
    in_transaction(path, |tx| {
        writes
            .iter()
            .map(|(unit, resolved)| persist_unit(tx, unit, resolved))
            .collect()
    })
    .expect("persist the units")
}

#[test]
fn writes_one_unit_and_rewrites_it_without_duplicating_a_qn() {
    let (_dir, path) = migrated("rewrite");
    let conn = handle(&path);
    let widget_id = add_file(&conn, WIDGET, "ok");
    conn.close().expect("close the handle");

    let (unit, resolved) = widget_unit(true);
    let first = persist_all(&path, &[(&unit, &resolved)]);
    assert_eq!(
        first,
        [PersistCounts {
            file_id: widget_id,
            nodes: 3,
            edges: 3,
            unresolved: 2,
            skipped_edges: 0,
            pending_tombstones: 0,
        }]
    );

    let conn = handle(&path);
    assert_eq!(qns(&conn), [HELPER_QN, RENDER_QN, WIDGET_FILE_QN]);
    // Containment is the parent link: both symbols hang off the file node, which has no parent.
    assert_eq!(
        node_columns(&conn),
        [
            (
                WIDGET.to_owned(),
                Some(WIDGET_FILE_QN.to_owned()),
                0,
                "{}".to_owned(),
                None
            ),
            (
                WIDGET.to_owned(),
                Some(WIDGET_FILE_QN.to_owned()),
                0,
                "{}".to_owned(),
                None
            ),
            (WIDGET.to_owned(), None, 0, "{}".to_owned(), None),
        ]
    );
    assert_eq!(
        edge_rows(&conn),
        [
            (
                RENDER_QN.to_owned(),
                HELPER_QN.to_owned(),
                "calls".to_owned(),
                widget_id
            ),
            (
                WIDGET_FILE_QN.to_owned(),
                HELPER_QN.to_owned(),
                "contains".to_owned(),
                widget_id
            ),
            (
                WIDGET_FILE_QN.to_owned(),
                RENDER_QN.to_owned(),
                "contains".to_owned(),
                widget_id
            ),
        ]
    );
    // Both producers of an unbound site are stored: the extractor's nameless call and the bind
    // pass's receiver-typed miss.
    assert_eq!(
        unresolved_rows(&conn),
        [
            (
                5,
                "mystery".to_owned(),
                Some("dynamic".to_owned()),
                Some("call:mystery".to_owned())
            ),
            (
                6,
                "missing".to_owned(),
                Some("receiver_type_unknown".to_owned()),
                Some("call:missing".to_owned())
            ),
        ]
    );
    conn.close().expect("close the handle");

    // The same unit handed over again: one row per qn, one row per edge, and nothing tombstoned.
    let again = persist_all(&path, &[(&unit, &resolved)]);
    assert_eq!(again, first);
    let conn = handle(&path);
    assert_eq!(qns(&conn), [HELPER_QN, RENDER_QN, WIDGET_FILE_QN]);
    assert_eq!(
        one_i64(&conn, "SELECT count(*) FROM node", &[]),
        one_i64(&conn, "SELECT count(DISTINCT qn) FROM node", &[])
    );
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM edge", &[]), 3);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM unresolved", &[]), 2);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM tombstone", &[]), 0);
}

#[test]
fn writes_the_signature_props_and_test_flag_the_extractor_produced() {
    let (_dir, path) = migrated("columns");
    let conn = handle(&path);
    let test_file_id = add_file(&conn, TEST_FILE, "ok");
    conn.close().expect("close the handle");

    let mut props = NodeProps::new();
    props.set_strings("decorators", &["Component".to_owned()]);
    let test_node = ExtractedNode {
        signature: Some("function test_render()".to_owned()),
        is_test: true,
        props,
        ..decl("test", TEST_FILE, "test_render", 3)
    };
    let unit = unit(TEST_FILE, vec![file_node(TEST_FILE, true), test_node]);
    let counts = persist_all(&path, &[(&unit, &resolved_of(Vec::new()))]);
    assert_eq!(counts[0].nodes, 2);

    let conn = handle(&path);
    assert_eq!(
        node_columns(&conn),
        [
            (
                TEST_FILE.to_owned(),
                None,
                1,
                r#"{"decorators":["Component"]}"#.to_owned(),
                Some("function test_render()".to_owned())
            ),
            (TEST_FILE.to_owned(), None, 1, "{}".to_owned(), None),
        ]
    );
    assert_eq!(counts[0].file_id, test_file_id);
}

#[test]
fn a_dropped_symbol_leaves_exactly_one_pending_tombstone() {
    let (_dir, path) = migrated("tombstone");
    let conn = handle(&path);
    let _widget_id = add_file(&conn, WIDGET, "ok");
    let other_id = add_file(&conn, OTHER, "ok");
    conn.close().expect("close the handle");

    // The symbol has to exist first, and so does a caller of it: `other.ts::lonely` calls
    // `widget.ts::helper`, so the tombstone's `had_callers` has a real inbound edge to count.
    let (with_helper, resolved_with_helper) = widget_unit(true);
    let (other, other_resolved) = other_unit();
    persist_all(
        &path,
        &[
            (&with_helper, &resolved_with_helper),
            (&other, &other_resolved),
        ],
    );
    // The generation the stage holds when the removal is recorded.
    in_transaction(&path, |tx| {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('generation', 'g1') ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )
        .map(|_| ())
        .map_err(|error| AppError::internal(format!("setting the generation: {error}")))
    })
    .expect("record the generation");

    let (rewritten, rewritten_resolved) = widget_unit(false);
    let second = persist_all(&path, &[(&rewritten, &rewritten_resolved)]);
    assert_eq!(
        second,
        [PersistCounts {
            file_id: _widget_id,
            nodes: 2,
            edges: 1,
            unresolved: 2,
            skipped_edges: 0,
            pending_tombstones: 1,
        }]
    );

    let conn = handle(&path);
    assert_eq!(
        qns(&conn),
        [LONELY_QN, OTHER_FILE_QN, RENDER_QN, WIDGET_FILE_QN]
    );
    // One row, and it is pending: `removed_in_generation` names no generation yet, and
    // `last_seen_generation` is the generation the stage held before this call. Two callers were
    // counted before the delete: `render` in this file and `lonely` in the other one.
    assert_eq!(
        tombstone_rows(&conn),
        [(
            HELPER_QN.to_owned(),
            "function".to_owned(),
            "g1".to_owned(),
            PENDING_GENERATION.to_owned(),
            2,
        )]
    );
    // The caller's `calls` edge went with the node it pointed at: no edge in this graph dangles,
    // and SQLite agrees the foreign keys hold.
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM edge e WHERE NOT EXISTS (SELECT 1 FROM node n WHERE n.id = e.dst_id)
             OR NOT EXISTS (SELECT 1 FROM node n WHERE n.id = e.src_id)",
            &[]
        ),
        0
    );
    assert_eq!(
        one_i64(&conn, "SELECT count(*) FROM pragma_foreign_key_check", &[]),
        0
    );
    // `other.ts` still holds its own rows; only the edge into the removed symbol is gone.
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM edge WHERE file_id = ?1",
            &[&other_id]
        ),
        1
    );
    conn.close().expect("close the handle");

    // A symbol that comes back is not a removal: the pending tombstone is pruned.
    persist_all(&path, &[(&with_helper, &resolved_with_helper)]);
    let conn = handle(&path);
    assert!(tombstone_rows(&conn).is_empty());
    assert!(qns(&conn).contains(&HELPER_QN.to_owned()));
}

#[test]
fn a_tombstone_reads_as_pending_when_the_generation_is_unset() {
    let (_dir, path) = migrated("tombstone-pending");
    let conn = handle(&path);
    let _widget_id = add_file(&conn, WIDGET, "ok");
    // The runner records `schema_version`; the generation is the publish's to write, and it has not
    // written one yet.
    assert_eq!(
        one_i64(
            &conn,
            "SELECT count(*) FROM meta WHERE key = 'generation'",
            &[]
        ),
        0
    );
    conn.close().expect("close the handle");

    // Containment only: no `calls` edge into `helper` exists, so a non-zero count would mean the
    // `contains` edge was counted as a caller.
    let (with_helper, _) = widget_unit(true);
    let contains_only = resolved_of(vec![
        edge(WIDGET, "contains", WIDGET_FILE_QN, RENDER_QN, 3),
        edge(WIDGET, "contains", WIDGET_FILE_QN, HELPER_QN, 8),
    ]);
    let (without, without_resolved) = widget_unit(false);
    persist_all(&path, &[(&with_helper, &contains_only)]);
    persist_all(&path, &[(&without, &without_resolved)]);

    let conn = handle(&path);
    // `helper`'s only inbound edge is the `contains` from the file node, which is containment and
    // not a caller, so `had_callers` is zero.
    assert_eq!(
        tombstone_rows(&conn),
        [(
            HELPER_QN.to_owned(),
            "function".to_owned(),
            PENDING_GENERATION.to_owned(),
            PENDING_GENERATION.to_owned(),
            0,
        )]
    );
}

#[test]
fn a_unit_owns_the_coverage_rows_of_its_own_path() {
    let (_dir, path) = migrated("coverage");
    let conn = handle(&path);
    let _broken_id = add_file(&conn, "src/broken.ts", "failed");
    conn.close().expect("close the handle");

    let broken = "src/broken.ts";
    let mut unit_with_coverage = unit(broken, vec![file_node(broken, false)]);
    unit_with_coverage.not_indexed = vec![NotIndexedEntry {
        path: broken.to_owned(),
        reason: NotIndexedReason::GrammarUnavailable,
        detail: Some("no grammar for typescript".to_owned()),
        language: Some("typescript".to_owned()),
    }];
    persist_all(&path, &[(&unit_with_coverage, &resolved_of(Vec::new()))]);

    let conn = handle(&path);
    assert_eq!(
        coverage_rows(&conn),
        [(
            broken.to_owned(),
            "grammar_unavailable".to_owned(),
            Some("no grammar for typescript".to_owned()),
            Some("typescript".to_owned()),
        )]
    );
    conn.close().expect("close the handle");

    // A unit may not claim another path's coverage: the call fails, and the rollback is what leaves
    // the previous rows in place.
    let mut foreign = unit(broken, vec![file_node(broken, false)]);
    foreign.not_indexed = vec![
        NotIndexedEntry::new(broken, NotIndexedReason::ParseFailed),
        NotIndexedEntry::new(OTHER, NotIndexedReason::Binary),
    ];
    let error = in_transaction(&path, |tx| {
        persist_unit(tx, &foreign, &resolved_of(Vec::new()))
    })
    .expect_err("a coverage row for another path is refused");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert_eq!(
        error.message,
        "not_indexed entry for src/other.ts inside the unit for src/broken.ts"
    );
    assert_eq!(
        error.hint.as_deref(),
        Some(
            "a unit owns the coverage rows for its own path; discovery's repo-wide list is the worker's"
        )
    );
    let conn = handle(&path);
    assert_eq!(
        coverage_rows(&conn),
        [(
            broken.to_owned(),
            "grammar_unavailable".to_owned(),
            Some("no grammar for typescript".to_owned()),
            Some("typescript".to_owned()),
        )]
    );
    conn.close().expect("close the handle");

    // The path is indexed now, so the coverage row claiming otherwise goes.
    persist_all(
        &path,
        &[(
            &unit(broken, vec![file_node(broken, false)]),
            &resolved_of(Vec::new()),
        )],
    );
    let conn = handle(&path);
    assert!(coverage_rows(&conn).is_empty());
}

#[test]
fn an_edge_whose_far_end_has_no_row_is_counted_and_not_written() {
    let (_dir, path) = migrated("skipped-edge");
    let conn = handle(&path);
    let widget_id = add_file(&conn, WIDGET, "ok");
    let other_id = add_file(&conn, OTHER, "ok");
    conn.close().expect("close the handle");

    // `lonely` is written first, so the fallback lookup into the `node` table has a row to find.
    let (other, other_resolved) = other_unit();
    persist_all(&path, &[(&other, &other_resolved)]);

    let ghost = "src/ghost.ts::ghost";
    let unit = unit(
        WIDGET,
        vec![
            file_node(WIDGET, false),
            decl("function", WIDGET, "render", 3),
        ],
    );
    let resolved = resolved_of(vec![
        edge(WIDGET, "contains", WIDGET_FILE_QN, RENDER_QN, 3),
        // The far end belongs to a unit persisted earlier: found in the table, not skipped.
        edge(WIDGET, "calls", RENDER_QN, LONELY_QN, 4),
        // A site in another file pointing into this one: the source is found the same way.
        edge(OTHER, "calls", LONELY_QN, RENDER_QN, 5),
        // Neither end of this one has a row, so it is counted and never written.
        edge(WIDGET, "calls", RENDER_QN, ghost, 6),
    ]);
    let counts = persist_all(&path, &[(&unit, &resolved)]);
    assert_eq!(
        counts,
        [PersistCounts {
            file_id: widget_id,
            nodes: 2,
            edges: 3,
            unresolved: 0,
            skipped_edges: 1,
            pending_tombstones: 0,
        }]
    );

    let conn = handle(&path);
    assert_eq!(
        edge_rows(&conn),
        [
            (
                LONELY_QN.to_owned(),
                RENDER_QN.to_owned(),
                "calls".to_owned(),
                other_id
            ),
            (
                OTHER_FILE_QN.to_owned(),
                LONELY_QN.to_owned(),
                "contains".to_owned(),
                other_id
            ),
            (
                RENDER_QN.to_owned(),
                LONELY_QN.to_owned(),
                "calls".to_owned(),
                widget_id
            ),
            (
                WIDGET_FILE_QN.to_owned(),
                RENDER_QN.to_owned(),
                "contains".to_owned(),
                widget_id
            ),
        ]
    );
    assert_eq!(
        one_i64(&conn, "SELECT count(*) FROM node WHERE qn = ?1", &[&ghost]),
        0
    );
}

#[test]
fn a_qn_declared_under_two_files_leaves_one_row() {
    let (_dir, path) = migrated("qn-conflict");
    let conn = handle(&path);
    let one_id = add_file(&conn, "src/one.ts", "ok");
    let two_id = add_file(&conn, "src/two.ts", "ok");
    conn.close().expect("close the handle");
    // A stale row carrying this unit's qn under another file: the same symbol handed over twice.
    in_transaction(&path, |tx| {
        tx.execute(
            "INSERT INTO node (file_id, kind, name, qn, start_line, end_line, is_test, props)
             VALUES (?1, 'function', 'render', ?2, 1, 3, 0, '{}')",
            params![two_id, ONE_RENDER_QN],
        )
        .map(|_| ())
        .map_err(|error| AppError::internal(format!("writing the stale row: {error}")))
    })
    .expect("write the stale row");

    let unit = unit(
        "src/one.ts",
        vec![decl("function", "src/one.ts", "render", 1)],
    );
    let counts = persist_all(&path, &[(&unit, &resolved_of(Vec::new()))]);
    assert_eq!(counts[0].file_id, one_id);

    let conn = handle(&path);
    assert_eq!(qns(&conn), [ONE_RENDER_QN]);
    assert_eq!(
        one_i64(
            &conn,
            "SELECT file_id FROM node WHERE qn = ?1",
            &[&ONE_RENDER_QN]
        ),
        one_id
    );
    // Nothing was tombstoned: the row belonged to another file, so this unit never declared it.
    assert!(tombstone_rows(&conn).is_empty());
}

#[test]
fn refuses_a_unit_whose_file_row_is_missing() {
    let (_dir, path) = migrated("no-file-row");
    let ghost = "src/ghost.ts";
    let unit = unit(ghost, vec![file_node(ghost, false)]);
    let error = in_transaction(&path, |tx| {
        persist_unit(tx, &unit, &resolved_of(Vec::new()))
    })
    .expect_err("a unit with no file row is refused");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert_eq!(error.message, "no file row for src/ghost.ts");
    assert_eq!(
        error.hint.as_deref(),
        Some(
            "write the file row (sha256, size, line count, parse status) before persisting its unit"
        )
    );

    let conn = handle(&path);
    assert_eq!(one_i64(&conn, "SELECT count(*) FROM node", &[]), 0);
}
