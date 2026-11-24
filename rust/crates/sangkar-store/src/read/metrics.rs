//! The derived per-node numbers `DATABASE.md` §3.4 computes once at publish, read back per node.
//!
//! Ported from `src/shared/store/read/metrics.ts`. `tested_depth` is the reason the table exists:
//! `test_gaps` and `impact` answer "is this tested, and through which caller" from one `INTEGER`
//! instead of a graph walk, so the read must not lose the distinction between `0` (tested directly),
//! a positive depth and `NULL` (no test reachable). A missing metrics row is reported as `None`, not
//! as zeroes: "degree 0" is a measurement, "not measured" is not.

use rusqlite::{OptionalExtension, Row};
use sangkar_core::errors::AppError;

use crate::db::Conn;
use crate::read::nodes::query_failed;

/// `node_metrics` for one node.
///
/// Mirrors `NodeMetrics` in `src/shared/store/read/metrics.ts`. No `serde` derive: this crate does
/// not depend on `serde`, so the serialising crate adds the attributes it needs for these rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeMetrics {
    pub node_id: i64,
    pub in_degree: i64,
    pub out_degree: i64,
    /// `is_entry` as a boolean; the `1/0` view belongs at the read boundary, not in the column.
    pub is_entry: bool,
    /// Minimum hops to a test node; `None` when no test is reachable.
    pub tested_depth: Option<i64>,
    /// `summary(repo)` only: nodes reachable from this entry point; `None` elsewhere.
    pub reachable_count: Option<i64>,
}

const FOR_NODE_KEY: &str = "reader.metrics.forNode";
const FOR_NODE_SQL: &str = concat!(
    "SELECT node_id, in_degree, out_degree, is_entry, tested_depth, reachable_count",
    " FROM node_metrics WHERE node_id = ?"
);

/// Metrics for one node, by primary key. `None` means the publish never measured this node.
pub fn metrics_for(conn: &Conn, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
    let mut statement = conn.prepared(FOR_NODE_KEY, |connection| {
        connection.prepare_cached(FOR_NODE_SQL)
    })?;
    statement
        .query_row([node_id], metrics_from_row)
        .optional()
        .map_err(|error| query_failed(FOR_NODE_KEY, &error))
}

fn metrics_from_row(row: &Row<'_>) -> rusqlite::Result<NodeMetrics> {
    Ok(NodeMetrics {
        node_id: row.get("node_id")?,
        in_degree: row.get("in_degree")?,
        out_degree: row.get("out_degree")?,
        is_entry: row.get::<_, i64>("is_entry")? != 0,
        tested_depth: row.get("tested_depth")?,
        reachable_count: row.get("reachable_count")?,
    })
}
