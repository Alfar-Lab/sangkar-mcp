//! Directional edge lookups: the two halves of `trace` and every hop of the `impact` BFS
//! (`DATABASE.md` §6), each served by its own covering index — `idx_edge_dst_kind` for [`inbound`],
//! `idx_edge_src_kind` for [`outbound`].
//!
//! Ported from `src/shared/store/read/edges.ts`. The far end of the edge is joined in rather than
//! left as an id: a BFS that received bare ids would pay one extra round trip per hop to name what
//! it found, which is the N+1 the prepared path exists to avoid.
//!
//! The reference's batched pair (`inboundMany`/`outboundMany`) is not ported here: nothing in this
//! phase walks a frontier, and the two single-id statements are what every caller composes.

use rusqlite::types::Value;
use rusqlite::{Row, params_from_iter};
use sangkar_core::errors::AppError;

use crate::db::Conn;
use crate::read::nodes::{
    KIND_FILTER_MAX, NodeRow, kind_shape, node_from_row, node_select, placeholders, query_failed,
};

/// The edge itself, with the columns a caller groups or reports on.
///
/// Mirrors `EdgeRow` in `src/shared/store/read/edges.ts`. No `serde` derive: this crate does not
/// depend on `serde`, so the serialising crate adds the attributes it needs for these rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub id: i64,
    pub src_id: i64,
    pub dst_id: i64,
    pub kind: String,
    pub tier: String,
    /// The file the edge was extracted from, which is not always the file of either endpoint.
    pub file_id: i64,
    pub line: i64,
    pub local_name: String,
    pub evidence: Option<String>,
}

/// An edge seen from one endpoint: `node` is the far end — the source for [`inbound`], the
/// destination for [`outbound`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeighbourEdge {
    pub edge: EdgeRow,
    pub node: NodeRow,
}

/// The edge columns, prefixed so an edge column can never collide with the `node` columns of
/// [`node_select`].
macro_rules! edge_select {
    () => {
        concat!(
            "e.id AS edge_id, e.src_id AS edge_src_id, e.dst_id AS edge_dst_id, ",
            "e.kind AS edge_kind, e.tier AS edge_tier, e.file_id AS edge_file_id, ",
            "e.line AS edge_line, e.local_name AS edge_local_name, e.evidence AS edge_evidence"
        )
    };
}

const INCIDENT_ORDER: &str = " ORDER BY e.kind, e.tier, e.line, n.qn";

/// Edges whose destination is the near side; the node joined in is `e.src_id`, the caller.
const INBOUND_PREFIX: &str = concat!(
    "SELECT ",
    edge_select!(),
    ", ",
    node_select!(),
    " FROM edge e JOIN node n ON n.id = e.src_id JOIN file f ON f.id = n.file_id",
    " WHERE e.dst_id = ?"
);

/// Edges whose source is the near side; the node joined in is `e.dst_id`, the callee.
const OUTBOUND_PREFIX: &str = concat!(
    "SELECT ",
    edge_select!(),
    ", ",
    node_select!(),
    " FROM edge e JOIN node n ON n.id = e.dst_id JOIN file f ON f.id = n.file_id",
    " WHERE e.src_id = ?"
);

/// One key per kind-filter arity of [`inbound`], in `0..=KIND_FILTER_MAX` order.
static INBOUND_KEYS: [&str; KIND_FILTER_MAX + 1] = [
    "reader.edges.inbound/0",
    "reader.edges.inbound/1",
    "reader.edges.inbound/2",
    "reader.edges.inbound/3",
    "reader.edges.inbound/4",
    "reader.edges.inbound/5",
    "reader.edges.inbound/6",
    "reader.edges.inbound/7",
    "reader.edges.inbound/8",
];

/// One key per kind-filter arity of [`outbound`], in `0..=KIND_FILTER_MAX` order.
static OUTBOUND_KEYS: [&str; KIND_FILTER_MAX + 1] = [
    "reader.edges.outbound/0",
    "reader.edges.outbound/1",
    "reader.edges.outbound/2",
    "reader.edges.outbound/3",
    "reader.edges.outbound/4",
    "reader.edges.outbound/5",
    "reader.edges.outbound/6",
    "reader.edges.outbound/7",
    "reader.edges.outbound/8",
];

/// Which endpoint was asked about, and therefore which endpoint is joined in.
#[derive(Clone, Copy)]
enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    fn prefix(self) -> &'static str {
        match self {
            Self::Inbound => INBOUND_PREFIX,
            Self::Outbound => OUTBOUND_PREFIX,
        }
    }

    fn keys(self) -> &'static [&'static str] {
        match self {
            Self::Inbound => &INBOUND_KEYS,
            Self::Outbound => &OUTBOUND_KEYS,
        }
    }
}

/// One statement shape: the near-side predicate, the optional `IN (...)` list and the order. Called
/// only when a shape is prepared, so the only variable text is the `?, ?, …` list.
fn incident_sql(direction: Direction, kind_arity: usize) -> String {
    let mut sql = String::from(direction.prefix());
    if kind_arity > 0 {
        sql.push_str(" AND e.kind IN (");
        sql.push_str(&placeholders(kind_arity));
        sql.push(')');
    }
    sql.push_str(INCIDENT_ORDER);
    sql
}

/// Maps one joined row to the domain pair; the node half is [`node_from_row`], so the node columns
/// have exactly one reader.
fn neighbour_from_row(row: &Row<'_>) -> rusqlite::Result<NeighbourEdge> {
    Ok(NeighbourEdge {
        edge: EdgeRow {
            id: row.get("edge_id")?,
            src_id: row.get("edge_src_id")?,
            dst_id: row.get("edge_dst_id")?,
            kind: row.get("edge_kind")?,
            tier: row.get("edge_tier")?,
            file_id: row.get("edge_file_id")?,
            line: row.get("edge_line")?,
            local_name: row.get("edge_local_name")?,
            evidence: row.get("edge_evidence")?,
        },
        node: node_from_row(row)?,
    })
}

/// Both directions share the fetch and the mapping; only the SQL differs by [`Direction`].
fn incident(
    conn: &Conn,
    direction: Direction,
    node_id: i64,
    kinds: Option<&[String]>,
) -> Result<Vec<NeighbourEdge>, AppError> {
    let filter = kinds.unwrap_or_default();
    let (key, bound) = kind_shape(direction.keys(), filter.len());
    let mut statement = conn.prepared(key, |connection| {
        connection.prepare_cached(&incident_sql(direction, bound))
    })?;

    // The id is an `INTEGER` and the kinds are `TEXT`, so the bound list is one `Value` type rather
    // than a `&dyn ToSql` slice: `params_from_iter` binds values, never SQL text.
    let mut params: Vec<Value> = Vec::with_capacity(1 + bound);
    params.push(Value::Integer(node_id));
    params.extend(filter[..bound].iter().map(|kind| Value::Text(kind.clone())));

    let mapped = statement
        .query_map(params_from_iter(params), neighbour_from_row)
        .map_err(|error| query_failed(key, &error))?;
    let mut neighbours = mapped
        .collect::<rusqlite::Result<Vec<NeighbourEdge>>>()
        .map_err(|error| query_failed(key, &error))?;
    if filter.len() > bound {
        neighbours.retain(|neighbour| filter.contains(&neighbour.edge.kind));
    }
    Ok(neighbours)
}

/// Edges whose destination is `node_id`, with the calling node attached. `kinds` filters on
/// `edge.kind` and is all-or-nothing: an empty or omitted list means every kind. Ordered by
/// `(kind, tier, line, qn)` so a caller grouping by kind sees a stable order inside each group.
pub fn inbound(
    conn: &Conn,
    node_id: i64,
    kinds: Option<&[String]>,
) -> Result<Vec<NeighbourEdge>, AppError> {
    incident(conn, Direction::Inbound, node_id, kinds)
}

/// Edges whose source is `node_id`, with the called node attached. Same filter and ordering
/// contract as [`inbound`].
pub fn outbound(
    conn: &Conn,
    node_id: i64,
    kinds: Option<&[String]>,
) -> Result<Vec<NeighbourEdge>, AppError> {
    incident(conn, Direction::Outbound, node_id, kinds)
}
