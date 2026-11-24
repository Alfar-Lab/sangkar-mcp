//! Node lookups: the shapes `resolve` and `expand` need (`DATABASE.md` §6), one prepared
//! statement per shape.
//!
//! Ported from `src/shared/store/read/nodes.ts`. Every statement comes from `Conn::prepared`, and
//! both its key and its SQL text depend only on the shape, so a warm call assembles nothing but the
//! string the driver's own cache has already parsed.
//!
//! Two porting decisions are visible in the types:
//! - `props` keeps the raw `node.props` text. This crate has no `serde_json` dependency and does not
//!   gain one here, so parsing the column belongs to the crate that serialises the row; the
//!   reference raises on malformed text rather than degrading to `{}`, and that check moves with
//!   the parser.
//! - a `kinds` filter is bound at one of [`KIND_FILTER_MAX`] + 1 arities, because the cache key is a
//!   `&'static str` and the arity is part of the query shape. A longer list reads the unfiltered
//!   shape and is matched in memory, so no caller's list can add shapes to the cache.

use rusqlite::{OptionalExtension, Row, params_from_iter};
use sangkar_core::errors::AppError;

use crate::db::Conn;

/// One node with the repo-relative path of the file that defines it.
///
/// Mirrors `NodeRow` in `src/shared/store/read/nodes.ts`. No `serde` derive: this crate does not
/// depend on `serde`, so the serialising crate adds the attributes it needs for these rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub id: i64,
    pub file_id: i64,
    /// `file.path`, joined here so no caller pays a second lookup per node.
    pub path: String,
    pub parent_id: Option<i64>,
    pub kind: String,
    pub subkind: Option<String>,
    pub name: String,
    pub qn: String,
    pub start_line: i64,
    pub end_line: i64,
    pub signature: Option<String>,
    pub docstring: Option<String>,
    pub is_test: bool,
    /// The raw `node.props` JSON text; see the module note.
    pub props: String,
}

/// The `node` columns plus `file.path`, in the order [`node_from_row`] reads them. A macro instead
/// of a `const` because Rust has no const string concatenation: `node_select!()` inside `concat!`
/// keeps one copy of the list between this module and `edges.rs`, which is why the TypeScript
/// exports `NODE_SELECT` beside `toNodeRow`.
macro_rules! node_select {
    () => {
        concat!(
            "n.id AS id, n.file_id AS file_id, f.path AS path, n.parent_id AS parent_id, ",
            "n.kind AS kind, n.subkind AS subkind, n.name AS name, n.qn AS qn, ",
            "n.start_line AS start_line, n.end_line AS end_line, n.signature AS signature, ",
            "n.docstring AS docstring, n.is_test AS is_test, n.props AS props"
        )
    };
}

/// The `FROM` clause every node select shares; `edges.rs` joins its own, so it uses [`node_select`]
/// alone.
macro_rules! node_from {
    () => {
        " FROM node n JOIN file f ON f.id = n.file_id"
    };
}

pub(crate) use node_select;

/// The prepared-cache key of [`find_by_qn`]; the shape never varies.
const BY_QN_KEY: &str = "reader.nodes.byQn";
const BY_QN_SQL: &str = concat!("SELECT ", node_select!(), node_from!(), " WHERE n.qn = ?");

const MEMBERS_KEY: &str = "reader.nodes.members";
const MEMBERS_SQL: &str = concat!(
    "SELECT ",
    node_select!(),
    node_from!(),
    " WHERE n.parent_id = ? ORDER BY n.start_line, n.qn"
);

const BY_FILE_KEY: &str = "reader.nodes.byFile";
const BY_FILE_SQL: &str = concat!(
    "SELECT ",
    node_select!(),
    node_from!(),
    " WHERE n.file_id = ? ORDER BY n.start_line, n.qn"
);

const BY_NAME_PREFIX: &str = concat!("SELECT ", node_select!(), node_from!(), " WHERE n.name = ?");
const BY_NAME_ORDER: &str = " ORDER BY n.qn";

/// The widest kind filter the cache holds a statement shape for. The suffix of every `…/N` key is
/// the number of `kind` parameters that statement binds, so one key can never name two statements.
pub(crate) const KIND_FILTER_MAX: usize = 8;

/// One key per kind-filter arity of [`find_by_name`], in `0..=KIND_FILTER_MAX` order.
const BY_NAME_KEYS: [&str; KIND_FILTER_MAX + 1] = [
    "reader.nodes.byName/0",
    "reader.nodes.byName/1",
    "reader.nodes.byName/2",
    "reader.nodes.byName/3",
    "reader.nodes.byName/4",
    "reader.nodes.byName/5",
    "reader.nodes.byName/6",
    "reader.nodes.byName/7",
    "reader.nodes.byName/8",
];

/// Maps one SQL row to the domain row; `pub(crate)` because `edges.rs` returns whole nodes too, so
/// the two modules cannot drift on a column.
pub(crate) fn node_from_row(row: &Row<'_>) -> rusqlite::Result<NodeRow> {
    Ok(NodeRow {
        id: row.get("id")?,
        file_id: row.get("file_id")?,
        path: row.get("path")?,
        parent_id: row.get("parent_id")?,
        kind: row.get("kind")?,
        subkind: row.get("subkind")?,
        name: row.get("name")?,
        qn: row.get("qn")?,
        start_line: row.get("start_line")?,
        end_line: row.get("end_line")?,
        signature: row.get("signature")?,
        docstring: row.get("docstring")?,
        // `is_test` is an `INTEGER` column; the `1/0` view belongs at this boundary, not in the row.
        is_test: row.get::<_, i64>("is_test")? != 0,
        props: row.get("props")?,
    })
}

/// The error every statement in the `read` modules raises: the key names the shape, so a failure
/// points at one statement rather than at the whole read path.
pub(crate) fn query_failed(sql_key: &'static str, error: &rusqlite::Error) -> AppError {
    AppError::internal(format!("{sql_key}: {error}"))
}

/// The `findByName` SQL for one kind-filter arity; called only when a shape is prepared, so the only
/// variable text is the `?, ?, …` list.
fn by_name_sql(kind_arity: usize) -> String {
    let mut sql = String::from(BY_NAME_PREFIX);
    if kind_arity > 0 {
        sql.push_str(" AND n.kind IN (");
        sql.push_str(&placeholders(kind_arity));
        sql.push(')');
    }
    sql.push_str(BY_NAME_ORDER);
    sql
}

/// `?, ?, ?` for an `IN (...)` list; the arity is part of the cache key, never a per-call loop.
pub(crate) fn placeholders(count: usize) -> String {
    vec!["?"; count].join(", ")
}

/// The key of the statement that answers `requested` kinds, and how many kinds its SQL binds.
///
/// A filter wider than [`KIND_FILTER_MAX`] gets the unfiltered shape and arity `0`; the caller then
/// narrows the rows it got, which is the same answer without a statement of its own.
pub(crate) fn kind_shape(keys: &[&'static str], requested: usize) -> (&'static str, usize) {
    if requested <= KIND_FILTER_MAX {
        (keys[requested], requested)
    } else {
        (keys[0], 0)
    }
}

/// Exact resolution on the unique `qn`, the first step of every `trace` (§6).
pub fn find_by_qn(conn: &Conn, qn: &str) -> Result<Option<NodeRow>, AppError> {
    let mut statement =
        conn.prepared(BY_QN_KEY, |connection| connection.prepare_cached(BY_QN_SQL))?;
    statement
        .query_row([qn], node_from_row)
        .optional()
        .map_err(|error| query_failed(BY_QN_KEY, &error))
}

/// Bare-name resolution over `idx_node_name_kind` (§6). `kinds` narrows the candidate set and is
/// all-or-nothing: an empty or omitted list means every kind. Results are ordered by `qn`, which is
/// unique, so the order is stable for a caller's own cursor and identical to the reference.
pub fn find_by_name(
    conn: &Conn,
    name: &str,
    kinds: Option<&[String]>,
) -> Result<Vec<NodeRow>, AppError> {
    let filter = kinds.unwrap_or_default();
    let (key, bound) = kind_shape(&BY_NAME_KEYS, filter.len());
    let mut statement = conn.prepared(key, |connection| {
        connection.prepare_cached(&by_name_sql(bound))
    })?;

    let mut params: Vec<&str> = Vec::with_capacity(1 + bound);
    params.push(name);
    params.extend(filter[..bound].iter().map(String::as_str));

    let mapped = statement
        .query_map(params_from_iter(params), node_from_row)
        .map_err(|error| query_failed(key, &error))?;
    let mut nodes = mapped
        .collect::<rusqlite::Result<Vec<NodeRow>>>()
        .map_err(|error| query_failed(key, &error))?;
    if filter.len() > bound {
        nodes.retain(|node| filter.contains(&node.kind));
    }
    Ok(nodes)
}

/// `expand(what="members")`: the direct children of `parent_id`, over `idx_node_parent` (§6), in
/// source order. `qn` breaks a `start_line` tie and is unique, exactly as the reference orders it.
pub fn members(conn: &Conn, parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
    let mut statement = conn.prepared(MEMBERS_KEY, |connection| {
        connection.prepare_cached(MEMBERS_SQL)
    })?;
    statement
        .query_map([parent_id], node_from_row)
        .map_err(|error| query_failed(MEMBERS_KEY, &error))?
        .collect::<rusqlite::Result<Vec<NodeRow>>>()
        .map_err(|error| query_failed(MEMBERS_KEY, &error))
}

/// Every node a file defines, over `idx_node_file_line` (§6), in source order.
pub fn nodes_by_file(conn: &Conn, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
    let mut statement = conn.prepared(BY_FILE_KEY, |connection| {
        connection.prepare_cached(BY_FILE_SQL)
    })?;
    statement
        .query_map([file_id], node_from_row)
        .map_err(|error| query_failed(BY_FILE_KEY, &error))?
        .collect::<rusqlite::Result<Vec<NodeRow>>>()
        .map_err(|error| query_failed(BY_FILE_KEY, &error))
}
