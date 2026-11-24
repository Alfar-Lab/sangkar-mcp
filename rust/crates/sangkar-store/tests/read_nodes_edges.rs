//! The row-level read shapes of `read/{nodes,edges,metrics}.rs`, on a hand-built database.
//!
//! The fixture is raw `INSERT`s, not an indexer run: two files, a class with three methods (two of
//! them sharing a `start_line`, so the `qn` tiebreak is pinned) and a free function, one edge in
//! each direction plus a second inbound kind, a metrics row with a value in every column and one
//! that leaves `reachable_count` unmeasured.
//!
//! Every list is asserted in query order, because the order is the contract: `find_by_name` decides
//! which candidates a `trace` page shows, and a re-sorted result is a silent behaviour change.
//! `a_second_call_of_the_same_statement_is_a_cache_hit` is the reason the SQL keys are static.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use rusqlite::params;
use sangkar_store::db::{Conn, open_reader, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::read::edges::{EdgeRow, NeighbourEdge, inbound, outbound};
use sangkar_store::read::metrics::{NodeMetrics, metrics_for};
use sangkar_store::read::nodes::{NodeRow, find_by_name, find_by_qn, members, nodes_by_file};

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

/// A migrated database with the fixture in it, and a reader open on the result. The directory is
/// returned so it outlives the handle.
fn fixture(label: &str) -> (TempDir, Conn) {
    let dir = TempDir::new(label);
    let path = dir.join("graph.db");
    {
        let mut writer = open_writer(&path).expect("open the writer");
        migrate(writer.connection_mut()).expect("migrate a fresh database");
        insert_fixture(writer.connection());
    }
    let reader = open_reader(&path).expect("open the reader");
    (dir, reader)
}

/// One `node` `INSERT`: (id, file, parent, kind, subkind, name, qn, start, end, signature,
/// docstring, is_test, props).
type NodeSeed = (
    i64,
    i64,
    Option<i64>,
    &'static str,
    Option<&'static str>,
    &'static str,
    &'static str,
    i64,
    i64,
    Option<&'static str>,
    Option<&'static str>,
    i64,
    &'static str,
);

/// One `edge` `INSERT`: (id, src, dst, kind, tier, file, line, local_name, evidence).
type EdgeSeed = (
    i64,
    i64,
    i64,
    &'static str,
    &'static str,
    i64,
    i64,
    &'static str,
    Option<&'static str>,
);

/// One `node_metrics` `INSERT`: (node, in_degree, out_degree, is_entry, tested_depth,
/// reachable_count).
type MetricsSeed = (i64, i64, i64, i64, Option<i64>, Option<i64>);

/// Two files, seven nodes and the edges between them. Ids are fixed by the `INSERT`s so a test can
/// name a row, and the ids of the two `run` nodes are ordered against their `qn`s on purpose.
fn insert_fixture(connection: &Connection) {
    for (id, path, size, lines) in [(1, "src/a.ts", 120, 9), (2, "src/b.ts", 90, 8)] {
        connection
            .execute(
                "INSERT INTO file (id, path, language, sha256, size, line_count, parse_status, \
                 parse_error_ranges, indexed_generation) \
                 VALUES (?1, ?2, 'typescript', ?3, ?4, ?5, 'ok', NULL, 'g1')",
                params![id, path, format!("{:064x}", id), size, lines],
            )
            .expect("insert a file");
    }

    // (id, file, parent, kind, subkind, name, qn, start, end, signature, docstring, is_test, props)
    let nodes: [NodeSeed; 7] = [
        (
            1,
            1,
            None,
            "class",
            None,
            "Widget",
            "src/a.ts::Widget",
            1,
            9,
            Some("class Widget"),
            Some("A widget."),
            0,
            "{}",
        ),
        (
            2,
            1,
            Some(1),
            "method",
            None,
            "paint",
            "src/a.ts::Widget.paint",
            4,
            5,
            Some("paint(): void"),
            None,
            0,
            "{\"visibility\":\"public\"}",
        ),
        (
            3,
            1,
            Some(1),
            "method",
            None,
            "alpha",
            "src/a.ts::Widget.alpha",
            4,
            4,
            Some("alpha(): void"),
            None,
            0,
            "{}",
        ),
        (
            4,
            1,
            Some(1),
            "method",
            None,
            "render",
            "src/a.ts::Widget.render",
            2,
            3,
            Some("render(): string"),
            None,
            0,
            "{}",
        ),
        (
            5,
            2,
            None,
            "function",
            None,
            "compute",
            "src/b.ts::compute",
            2,
            4,
            Some("compute(x: number): number"),
            Some("Computes."),
            0,
            "{}",
        ),
        (
            6,
            2,
            None,
            "function",
            None,
            "run",
            "src/b.ts::run",
            7,
            8,
            None,
            None,
            0,
            "{}",
        ),
        (
            7,
            1,
            Some(1),
            "method",
            None,
            "run",
            "src/a.ts::Widget.run",
            8,
            9,
            None,
            None,
            1,
            "{}",
        ),
    ];
    for node in nodes {
        connection
            .execute(
                "INSERT INTO node (id, file_id, parent_id, kind, subkind, name, qn, start_line, \
                 end_line, signature, docstring, is_test, props) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    node.0, node.1, node.2, node.3, node.4, node.5, node.6, node.7, node.8, node.9,
                    node.10, node.11, node.12
                ],
            )
            .expect("insert a node");
    }

    // (id, src, dst, kind, tier, file, line, local_name, evidence)
    let edges: [EdgeSeed; 3] = [
        (1, 2, 5, "call", "resolved", 1, 5, "compute", None),
        (
            2,
            5,
            1,
            "reference",
            "resolved",
            2,
            1,
            "Widget",
            Some("import"),
        ),
        (3, 6, 5, "import", "resolved", 2, 3, "compute", None),
    ];
    for edge in edges {
        connection
            .execute(
                "INSERT INTO edge (id, src_id, dst_id, kind, tier, file_id, line, local_name, \
                 evidence) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    edge.0, edge.1, edge.2, edge.3, edge.4, edge.5, edge.6, edge.7, edge.8
                ],
            )
            .expect("insert an edge");
    }

    // (node_id, in_degree, out_degree, is_entry, tested_depth, reachable_count)
    let metrics: [MetricsSeed; 2] = [(1, 1, 0, 1, Some(0), Some(7)), (5, 2, 1, 0, Some(2), None)];
    for row in metrics {
        connection
            .execute(
                "INSERT INTO node_metrics (node_id, in_degree, out_degree, is_entry, tested_depth, \
                 reachable_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![row.0, row.1, row.2, row.3, row.4, row.5],
            )
            .expect("insert a metrics row");
    }
}

fn ids(rows: &[NodeRow]) -> Vec<i64> {
    rows.iter().map(|row| row.id).collect()
}

fn neighbour_ids(neighbours: &[NeighbourEdge]) -> Vec<i64> {
    neighbours
        .iter()
        .map(|neighbour| neighbour.edge.id)
        .collect()
}

fn kinds(list: &[&str]) -> Vec<String> {
    list.iter().map(|kind| (*kind).to_owned()).collect()
}

#[test]
fn find_by_qn_returns_the_whole_row_and_none_for_a_missing_qn() {
    let (_dir, reader) = fixture("by-qn");

    assert_eq!(
        find_by_qn(&reader, "src/a.ts::Widget.paint").expect("read the paint row"),
        Some(NodeRow {
            id: 2,
            file_id: 1,
            path: "src/a.ts".to_owned(),
            parent_id: Some(1),
            kind: "method".to_owned(),
            subkind: None,
            name: "paint".to_owned(),
            qn: "src/a.ts::Widget.paint".to_owned(),
            start_line: 4,
            end_line: 5,
            signature: Some("paint(): void".to_owned()),
            docstring: None,
            is_test: false,
            // Raw column text: this crate has no JSON parser, so a malformed value is not
            // distinguishable here from a well-formed one — see the module note in `nodes.rs`.
            props: "{\"visibility\":\"public\"}".to_owned(),
        }),
        "every column of the node row, including the joined file path and the raw props text"
    );

    let test_node = find_by_qn(&reader, "src/a.ts::Widget.run")
        .expect("read the test row")
        .expect("the test method exists");
    assert!(test_node.is_test, "is_test = 1 reads as true");
    assert_eq!(test_node.docstring, None);

    assert_eq!(
        find_by_qn(&reader, "src/b.ts::nope").expect("read a missing qn"),
        None,
        "the unique index returns at most one row and nothing for an absent qn"
    );
}

#[test]
fn find_by_name_orders_by_qn_and_narrows_on_kind() {
    let (_dir, reader) = fixture("by-name");

    // The bare name exists twice. Insertion order is 6 then 7; `qn` order is the other way round,
    // so the ids prove the ORDER BY and not the rowid.
    let both = find_by_name(&reader, "run", None).expect("read both candidates");
    assert_eq!(ids(&both), vec![7, 6], "ORDER BY n.qn, copied verbatim");

    let functions = find_by_name(&reader, "run", Some(&kinds(&["function"]))).expect("filter");
    assert_eq!(ids(&functions), vec![6]);

    let both_kinds =
        find_by_name(&reader, "run", Some(&kinds(&["method", "function"]))).expect("two kinds");
    assert_eq!(
        ids(&both_kinds),
        vec![7, 6],
        "the kind filter keeps the qn order"
    );

    let none = find_by_name(&reader, "run", Some(&kinds(&["class"]))).expect("no kind matches");
    assert!(
        none.is_empty(),
        "the filter is all-or-nothing, not a fallback"
    );

    let empty = find_by_name(&reader, "run", Some(&[])).expect("an empty list means every kind");
    assert_eq!(ids(&empty), vec![7, 6]);

    // More kinds than the cache holds a shape for: the unfiltered rows are narrowed in memory, and
    // the copied order survives that path too.
    let wide = kinds(&["a", "b", "c", "d", "e", "f", "g", "h", "method"]);
    let wide_hit =
        find_by_name(&reader, "run", Some(&wide)).expect("a filter wider than the ladder");
    assert_eq!(ids(&wide_hit), vec![7]);
    let wide_miss = kinds(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]);
    assert!(
        find_by_name(&reader, "run", Some(&wide_miss))
            .expect("nine kinds, none matching")
            .is_empty(),
        "the wide filter still excludes every kind it does not list"
    );

    let unique = find_by_name(&reader, "Widget", None).expect("read the class");
    assert_eq!(
        unique,
        vec![NodeRow {
            id: 1,
            file_id: 1,
            path: "src/a.ts".to_owned(),
            parent_id: None,
            kind: "class".to_owned(),
            subkind: None,
            name: "Widget".to_owned(),
            qn: "src/a.ts::Widget".to_owned(),
            start_line: 1,
            end_line: 9,
            signature: Some("class Widget".to_owned()),
            docstring: Some("A widget.".to_owned()),
            is_test: false,
            props: "{}".to_owned(),
        }]
    );
}

#[test]
fn members_returns_source_order_with_the_qn_tiebreak() {
    let (_dir, reader) = fixture("members");

    let children = members(&reader, 1).expect("read the class members");
    // `render` starts at line 2; `alpha` and `paint` share line 4, and `alpha` sorts before `paint`
    // although its id is the higher one — so the tiebreak is `qn`, as the reference orders it, not
    // the id.
    assert_eq!(ids(&children), vec![4, 3, 2, 7]);
    assert_eq!(
        children
            .iter()
            .map(|row| row.start_line)
            .collect::<Vec<_>>(),
        vec![2, 4, 4, 8]
    );
    assert_eq!(
        children
            .first()
            .expect("the first member")
            .signature
            .as_deref(),
        Some("render(): string"),
        "the whole row is read, not only its id"
    );

    assert!(
        members(&reader, 5)
            .expect("read a childless function")
            .is_empty(),
        "no children means no rows, not an error"
    );
}

#[test]
fn nodes_by_file_returns_every_node_in_source_order() {
    let (_dir, reader) = fixture("by-file");

    assert_eq!(
        ids(&nodes_by_file(&reader, 1).expect("read file 1")),
        vec![1, 4, 3, 2, 7]
    );

    let second = nodes_by_file(&reader, 2).expect("read file 2");
    assert_eq!(ids(&second), vec![5, 6]);
    assert_eq!(
        second.first().expect("the first node of file 2"),
        &NodeRow {
            id: 5,
            file_id: 2,
            path: "src/b.ts".to_owned(),
            parent_id: None,
            kind: "function".to_owned(),
            subkind: None,
            name: "compute".to_owned(),
            qn: "src/b.ts::compute".to_owned(),
            start_line: 2,
            end_line: 4,
            signature: Some("compute(x: number): number".to_owned()),
            docstring: Some("Computes.".to_owned()),
            is_test: false,
            props: "{}".to_owned(),
        },
        "the joined path is the file the node belongs to"
    );

    assert!(
        nodes_by_file(&reader, 99)
            .expect("read an empty file")
            .is_empty(),
        "a file with no nodes yields no rows"
    );
}

#[test]
fn inbound_joins_the_calling_node_and_orders_by_kind() {
    let (_dir, reader) = fixture("inbound");

    // Edges 1 (`call`, line 5) and 3 (`import`, line 3) both end at node 5. Sorting by line would
    // put edge 3 first; the copied ORDER BY sorts by kind first.
    let callers = inbound(&reader, 5, None).expect("read the callers of compute");
    assert_eq!(
        neighbour_ids(&callers),
        vec![1, 3],
        "ORDER BY e.kind, e.tier, e.line, n.qn"
    );
    assert_eq!(
        callers.first().expect("the call edge"),
        &NeighbourEdge {
            edge: EdgeRow {
                id: 1,
                src_id: 2,
                dst_id: 5,
                kind: "call".to_owned(),
                tier: "resolved".to_owned(),
                file_id: 1,
                line: 5,
                local_name: "compute".to_owned(),
                evidence: None,
            },
            node: NodeRow {
                id: 2,
                file_id: 1,
                path: "src/a.ts".to_owned(),
                parent_id: Some(1),
                kind: "method".to_owned(),
                subkind: None,
                name: "paint".to_owned(),
                qn: "src/a.ts::Widget.paint".to_owned(),
                start_line: 4,
                end_line: 5,
                signature: Some("paint(): void".to_owned()),
                docstring: None,
                is_test: false,
                props: "{\"visibility\":\"public\"}".to_owned(),
            },
        },
        "the far end is the joined node, with its file path and every other column"
    );

    assert_eq!(
        neighbour_ids(&inbound(&reader, 5, Some(&kinds(&["import"]))).expect("filter on import")),
        vec![3]
    );
    assert_eq!(
        neighbour_ids(&inbound(&reader, 5, Some(&[])).expect("an empty list means every kind")),
        vec![1, 3]
    );
    assert!(
        inbound(&reader, 5, Some(&kinds(&["inherit"])))
            .expect("a kind with no edges")
            .is_empty(),
        "the filter is all-or-nothing"
    );

    let inherited = inbound(&reader, 1, None).expect("read the users of Widget");
    assert_eq!(neighbour_ids(&inherited), vec![2]);
    assert_eq!(
        inherited.first().expect("the reference edge").node.qn,
        "src/b.ts::compute",
        "inbound joins the source of the edge"
    );
}

#[test]
fn outbound_joins_the_called_node_and_orders_by_kind() {
    let (_dir, reader) = fixture("outbound");

    let callees = outbound(&reader, 5, None).expect("read what compute calls");
    assert_eq!(neighbour_ids(&callees), vec![2]);
    assert_eq!(
        callees.first().expect("the reference edge"),
        &NeighbourEdge {
            edge: EdgeRow {
                id: 2,
                src_id: 5,
                dst_id: 1,
                kind: "reference".to_owned(),
                tier: "resolved".to_owned(),
                file_id: 2,
                line: 1,
                local_name: "Widget".to_owned(),
                evidence: Some("import".to_owned()),
            },
            node: NodeRow {
                id: 1,
                file_id: 1,
                path: "src/a.ts".to_owned(),
                parent_id: None,
                kind: "class".to_owned(),
                subkind: None,
                name: "Widget".to_owned(),
                qn: "src/a.ts::Widget".to_owned(),
                start_line: 1,
                end_line: 9,
                signature: Some("class Widget".to_owned()),
                docstring: Some("A widget.".to_owned()),
                is_test: false,
                props: "{}".to_owned(),
            },
        },
        "outbound joins the destination of the edge"
    );

    assert_eq!(
        neighbour_ids(&outbound(&reader, 2, None).expect("read paint")),
        vec![1]
    );
    assert_eq!(
        neighbour_ids(&outbound(&reader, 6, None).expect("read run")),
        vec![3]
    );
    assert!(
        outbound(&reader, 1, None)
            .expect("read a node with no callers of its own")
            .is_empty()
    );
}

#[test]
fn metrics_for_reads_every_column_and_reports_a_missing_row_as_none() {
    let (_dir, reader) = fixture("metrics");

    assert_eq!(
        metrics_for(&reader, 1).expect("read the entry point"),
        Some(NodeMetrics {
            node_id: 1,
            in_degree: 1,
            out_degree: 0,
            is_entry: true,
            // 0 is "tested directly"; it must not collapse into `None`.
            tested_depth: Some(0),
            reachable_count: Some(7),
        })
    );
    assert_eq!(
        metrics_for(&reader, 5).expect("read compute"),
        Some(NodeMetrics {
            node_id: 5,
            in_degree: 2,
            out_degree: 1,
            is_entry: false,
            tested_depth: Some(2),
            reachable_count: None,
        }),
        "an unmeasured reachable_count stays None"
    );
    assert_eq!(
        metrics_for(&reader, 2).expect("read a node the publish never measured"),
        None,
        "a missing row is not zeroes"
    );
}

#[test]
fn a_second_call_of_the_same_statement_is_a_cache_hit() {
    let (_dir, reader) = fixture("cache");

    let _ = find_by_qn(&reader, "src/a.ts::Widget").expect("first by-qn call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (0, 1, 1),
        "{stats:?}"
    );

    let _ = find_by_qn(&reader, "src/a.ts::Widget").expect("second by-qn call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (1, 1, 1),
        "{stats:?}"
    );

    // A different kind-filter arity is a different SQL text, so it is a miss of its own; the same
    // arity asked twice is a hit on the same entry.
    let _ = find_by_name(&reader, "run", None).expect("arity 0, first call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (1, 2, 2),
        "{stats:?}"
    );

    let _ = find_by_name(&reader, "run", None).expect("arity 0, second call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (2, 2, 2),
        "{stats:?}"
    );

    let one_kind = kinds(&["function"]);
    let _ = find_by_name(&reader, "run", Some(&one_kind)).expect("arity 1, first call");
    let _ = find_by_name(&reader, "run", Some(&one_kind)).expect("arity 1, second call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (3, 3, 3),
        "the arity is part of the key: a new shape misses once, then hits"
    );

    let _ = inbound(&reader, 5, None).expect("inbound, first call");
    let _ = inbound(&reader, 5, None).expect("inbound, second call");
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (4, 4, 4),
        "{stats:?}"
    );

    // A filter wider than the statement ladder reuses the unfiltered shape and filters in memory,
    // so a caller's long list cannot grow the cache.
    let wide = kinds(&["a", "b", "c", "d", "e", "f", "g", "h", "call"]);
    let filtered = inbound(&reader, 5, Some(&wide)).expect("a filter wider than the ladder");
    assert_eq!(
        neighbour_ids(&filtered),
        vec![1],
        "the in-memory filter matched `call` only"
    );
    let stats = reader.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.size),
        (5, 4, 4),
        "the wide filter hit the arity-0 shape: no new entry, no new miss"
    );
}
