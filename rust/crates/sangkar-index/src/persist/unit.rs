//! One unit into the stage: the file's previous contribution goes, its nodes and their containment
//! come back, then edges, unresolved sites and coverage rows, and every qn that did not return is
//! journalled as a pending tombstone.
//!
//! Ported from `persistUnit` in `src/indexer/persist.ts`, statement for statement. The order is the
//! contract and each step depends on the one before it:
//!
//! 1. the file id, and the nodes that file contributed **before** the delete — both are only
//!    visible now, and the tombstone needs the qn, the kind and the caller count of each;
//! 2. `edge`, then `unresolved`, then `node`, all by `file_id`. Edges go first because an edge
//!    extracted from this file whose target lives elsewhere does not cascade with this file's node
//!    rows;
//! 3. nodes, with the stale row of a conflicting `qn` deleted first, because `node.qn` is unique and
//!    `qn` embeds the path: a second row carrying this qn means the same symbol was handed over
//!    twice, and adopting that id would leave the FTS mirror describing the old values;
//! 4. containment as `parent_id`, then the edges themselves — a `ResolvedEdge` whose far end has no
//!    row is counted in `skipped_edges` rather than written, because a dangling edge violates the
//!    schema's contract;
//! 5. the unresolved sites, then the coverage rows, then the tombstones — a qn that is back in the
//!    graph is not a removal, so the pending row for it is pruned.
//!
//! What this pass does **not** write: the `file` row. It carries the staleness manifest and belongs
//! to the worker, so a unit whose path has no `file` row is an internal error here, never an
//! invented id.

use std::collections::HashMap;

use sangkar_core::errors::AppError;
use sangkar_core::log;
use sangkar_core::paths::normalize_rel;
use sangkar_store::schema::META_GENERATION;
use sangkar_store::{OptionalExtension, Transaction, params};

use crate::persist::{PENDING_GENERATION, PersistCounts, PersistUnit};
use crate::resolve::BindResult;

/// The hint every statement failure carries: a run that cannot write one row must fail rather than
/// publish the rows it managed to write, and the stage is discarded by the caller.
const FAILED_HINT: &str = "the stage must be writable for a run to publish; nothing was published";

/// The file's own row: read, never written. The worker owns the staleness manifest, so a unit can
/// only be persisted for a path that already has one.
const SELECT_FILE_ID: &str = "SELECT id FROM file WHERE path = ?1";

/// A file's previous contribution, read before the delete: neither the qn nor the kind is visible
/// afterwards, and the tombstone row needs both.
const SELECT_FILE_NODES: &str = "SELECT id, qn, kind FROM node WHERE file_id = ?1 ORDER BY id";

/// The callers a symbol had. `contains` is containment and `tested_by` is derived from a `calls`
/// edge, so counting either would report a caller the symbol never had.
const COUNT_INBOUND: &str =
    "SELECT count(*) FROM edge WHERE dst_id = ?1 AND kind NOT IN ('contains', 'tested_by')";

const DELETE_EDGES: &str = "DELETE FROM edge WHERE file_id = ?1";
const DELETE_UNRESOLVED: &str = "DELETE FROM unresolved WHERE file_id = ?1";
const DELETE_NODES: &str = "DELETE FROM node WHERE file_id = ?1";

const DELETE_CONFLICTING_QN: &str = "DELETE FROM node WHERE qn = ?1 AND file_id <> ?2";

const INSERT_NODE: &str = concat!(
    "INSERT INTO node (file_id, kind, name, qn, start_line, end_line, signature, is_test, props)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id"
);

/// Containment is the only source of `parent_id`: the extractor emits it as a `contains` edge, so
/// the hierarchy is read from the edges the bind pass kept rather than re-derived from qn text.
const SET_PARENT: &str = "UPDATE node SET parent_id = ?1 WHERE id = ?2";

const SELECT_NODE_ID_BY_QN: &str = "SELECT id FROM node WHERE qn = ?1";

const INSERT_EDGE: &str = concat!(
    "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT DO NOTHING"
);

const INSERT_UNRESOLVED: &str = concat!(
    "INSERT INTO unresolved (file_id, line, raw_target, evidence_kind, evidence)",
    " VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT DO NOTHING"
);

/// A persisted path is in the graph, so any coverage row claiming otherwise is stale.
const DELETE_NOT_INDEXED: &str = "DELETE FROM not_indexed WHERE path = ?1";

const INSERT_NOT_INDEXED: &str = concat!(
    "INSERT INTO not_indexed (path, reason, detail, language)",
    " VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING"
);

const INSERT_TOMBSTONE: &str = concat!(
    "INSERT INTO tombstone",
    " (qn, kind, file_id, last_seen_generation, removed_in_generation, had_callers)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(qn) DO UPDATE SET kind = excluded.kind,",
    " file_id = excluded.file_id, last_seen_generation = excluded.last_seen_generation,",
    " removed_in_generation = excluded.removed_in_generation, had_callers = excluded.had_callers"
);

/// A qn that exists again is not a removal, whatever an earlier unit of this run recorded: the same
/// file handed over twice, or a symbol that came back, leaves no tombstone behind.
const PRUNE_RETURNED_TOMBSTONES: &str =
    "DELETE FROM tombstone WHERE removed_in_generation = ?1 AND qn IN (SELECT qn FROM node)";

const SELECT_META: &str = "SELECT value FROM meta WHERE key = ?1";

/// One node this file contributed before the call, with the two facts the tombstone stores.
struct Gone {
    qn: String,
    kind: String,
    had_callers: i64,
}

/// Writes one file's rows into the stage, inside the caller's transaction.
///
/// `unit` is authoritative for what the file declares; `resolved.edges` is the only edge input,
/// because the bind pass already decided which raw site became an edge and at which tier. `unit`'s
/// own qns are looked up in this unit's inserts first and in the `node` table second, so an edge
/// whose far end was persisted earlier still lands — and one whose far end exists nowhere is counted
/// in [`PersistCounts::skipped_edges`] instead of being written as a dangling row.
///
/// # Errors
///
/// `internal` for every failure: a path with no `file` row, an edge whose site names a path with no
/// `file` row, a coverage row for a path other than the unit's, an insert that came back without an
/// id, and any statement SQLite refuses. A failed call leaves the caller's transaction open and
/// uncommitted, so the caller's own rollback is what discards the rows.
pub fn persist_unit(
    tx: &Transaction<'_>,
    unit: &PersistUnit,
    resolved: &BindResult,
) -> Result<PersistCounts, AppError> {
    let rel_path = normalize_rel(&unit.rel_path);
    let file_id = file_id_of(tx, &rel_path)?.ok_or_else(|| {
        AppError::internal(format!("no file row for {rel_path}")).with_hint(
            "write the file row (sha256, size, line count, parse status) before persisting its unit",
        )
    })?;

    let removed = previous_nodes(tx, file_id)?;

    tx.execute(DELETE_EDGES, [file_id])
        .map_err(|error| write_failed("deleting this file's edges", error))?;
    tx.execute(DELETE_UNRESOLVED, [file_id])
        .map_err(|error| write_failed("deleting this file's unresolved sites", error))?;
    tx.execute(DELETE_NODES, [file_id])
        .map_err(|error| write_failed("deleting this file's nodes", error))?;

    let ids = insert_nodes(tx, unit, file_id)?;

    let mut set_parent = tx
        .prepare(SET_PARENT)
        .map_err(|error| write_failed("preparing the parent link", error))?;
    for edge in &resolved.edges {
        if edge.kind != "contains" {
            continue;
        }
        let (Some(parent), Some(child)) = (ids.get(&edge.src), ids.get(&edge.dst)) else {
            continue;
        };
        set_parent
            .execute([parent, child])
            .map_err(|error| write_failed("linking a contained node to its parent", error))?;
    }
    drop(set_parent);

    let mut edges = 0;
    let mut skipped_edges = 0;
    let mut file_ids = HashMap::from([(rel_path.clone(), file_id)]);
    let mut insert_edge = tx
        .prepare(INSERT_EDGE)
        .map_err(|error| write_failed("preparing an edge insert", error))?;
    for edge in &resolved.edges {
        let src = match ids.get(&edge.src) {
            Some(id) => Some(*id),
            None => node_id_by_qn(tx, &edge.src)?,
        };
        let dst = match ids.get(&edge.dst) {
            Some(id) => Some(*id),
            None => node_id_by_qn(tx, &edge.dst)?,
        };
        let (Some(src), Some(dst)) = (src, dst) else {
            skipped_edges += 1;
            continue;
        };
        let site = edge_file(tx, &mut file_ids, &edge.file)?;
        insert_edge
            .execute(params![
                src,
                dst,
                edge.kind,
                edge.tier,
                site,
                edge.line,
                edge.local_name,
                edge.evidence
            ])
            .map_err(|error| write_failed("inserting an edge", error))?;
        edges += 1;
    }
    drop(insert_edge);

    // The extractor's own nameless sites come first, then the bind pass's failures: the same fact
    // for a reader — a site that names nothing the graph can bind — filed under this unit's file
    // because the caller hands over the unit's slice of the bind result.
    let mut insert_unresolved = tx
        .prepare(INSERT_UNRESOLVED)
        .map_err(|error| write_failed("preparing an unresolved insert", error))?;
    let mut unresolved = 0;
    for site in &unit.unresolved {
        insert_unresolved
            .execute(params![
                file_id,
                site.line,
                site.raw_target,
                site.evidence_kind,
                site.evidence
            ])
            .map_err(|error| write_failed("inserting an unresolved site", error))?;
        unresolved += 1;
    }
    for site in &resolved.unresolved {
        insert_unresolved
            .execute(params![
                file_id,
                site.line,
                site.raw_target,
                site.evidence_kind,
                site.evidence
            ])
            .map_err(|error| write_failed("inserting an unresolved site", error))?;
        unresolved += 1;
    }
    drop(insert_unresolved);

    tx.execute(DELETE_NOT_INDEXED, [&rel_path])
        .map_err(|error| write_failed("deleting this path's coverage rows", error))?;
    let mut insert_not_indexed = tx
        .prepare(INSERT_NOT_INDEXED)
        .map_err(|error| write_failed("preparing a coverage row insert", error))?;
    for entry in &unit.not_indexed {
        let path = normalize_rel(&entry.path);
        if path != rel_path {
            return Err(AppError::internal(format!(
                "not_indexed entry for {path} inside the unit for {rel_path}"
            ))
            .with_hint(
                "a unit owns the coverage rows for its own path; discovery's repo-wide list is the worker's",
            ));
        }
        insert_not_indexed
            .execute(params![
                path,
                entry.reason.as_str(),
                entry.detail,
                entry.language
            ])
            .map_err(|error| write_failed("inserting a coverage row", error))?;
    }
    drop(insert_not_indexed);

    // Tombstones: a qn that was here and is not any more. `last_seen_generation` is still the
    // previous generation at this point, which is exactly what the column means.
    let last_seen =
        read_meta(tx, META_GENERATION)?.unwrap_or_else(|| PENDING_GENERATION.to_owned());
    let mut insert_tombstone = tx
        .prepare(INSERT_TOMBSTONE)
        .map_err(|error| write_failed("preparing a tombstone insert", error))?;
    let mut pending_tombstones = 0;
    for gone in &removed {
        if ids.contains_key(&gone.qn) {
            continue;
        }
        insert_tombstone
            .execute(params![
                gone.qn,
                gone.kind,
                file_id,
                last_seen,
                PENDING_GENERATION,
                gone.had_callers
            ])
            .map_err(|error| write_failed("recording a tombstone", error))?;
        pending_tombstones += 1;
    }
    drop(insert_tombstone);
    tx.execute(PRUNE_RETURNED_TOMBSTONES, [PENDING_GENERATION])
        .map_err(|error| write_failed("pruning the tombstones of returned symbols", error))?;

    let counts = PersistCounts {
        file_id,
        nodes: ids.len(),
        edges,
        unresolved,
        skipped_edges,
        pending_tombstones,
    };
    log::debug(
        "unit persisted",
        &[
            ("file", rel_path.into()),
            ("fileId", file_id.into()),
            ("nodes", counts.nodes.into()),
            ("edges", counts.edges.into()),
            ("unresolved", counts.unresolved.into()),
            ("skippedEdges", counts.skipped_edges.into()),
            ("pendingTombstones", counts.pending_tombstones.into()),
        ],
    );
    Ok(counts)
}

/// The `file.id` of `path`, `None` when the worker has not written the row yet.
fn file_id_of(tx: &Transaction<'_>, path: &str) -> Result<Option<i64>, AppError> {
    tx.query_row(SELECT_FILE_ID, [path], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|error| write_failed("reading the file row", error))
}

/// The nodes `file_id` contributed before this call, in the id order they were written, each with
/// its inbound caller count taken while its edges still exist.
fn previous_nodes(tx: &Transaction<'_>, file_id: i64) -> Result<Vec<Gone>, AppError> {
    let previous: Vec<(i64, String, String)> = {
        let mut select = tx
            .prepare(SELECT_FILE_NODES)
            .map_err(|error| write_failed("preparing the previous-node read", error))?;
        let rows = select
            .query_map([file_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(|error| write_failed("reading the previous nodes", error))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| write_failed("reading the previous nodes", error))?
    };

    let mut inbound = tx
        .prepare(COUNT_INBOUND)
        .map_err(|error| write_failed("preparing the inbound count", error))?;
    let mut removed = Vec::with_capacity(previous.len());
    for (id, qn, kind) in previous {
        let had_callers = inbound
            .query_row([id], |row| row.get::<_, i64>(0))
            .map_err(|error| write_failed("counting a symbol's callers", error))?;
        removed.push(Gone {
            qn,
            kind,
            had_callers,
        });
    }
    Ok(removed)
}

/// Inserts the unit's nodes and returns `qn → id` for the ones that landed, dropping a qn the unit
/// declared twice because the first declaration is the one the edges refer to.
fn insert_nodes(
    tx: &Transaction<'_>,
    unit: &PersistUnit,
    file_id: i64,
) -> Result<HashMap<String, i64>, AppError> {
    let mut ids: HashMap<String, i64> = HashMap::with_capacity(unit.nodes.len());
    let mut delete_conflict = tx
        .prepare(DELETE_CONFLICTING_QN)
        .map_err(|error| write_failed("preparing the qn-conflict delete", error))?;
    let mut insert_node = tx
        .prepare(INSERT_NODE)
        .map_err(|error| write_failed("preparing the node insert", error))?;
    for node in &unit.nodes {
        if ids.contains_key(&node.qn) {
            continue;
        }
        delete_conflict
            .execute(params![node.qn, file_id])
            .map_err(|error| write_failed("deleting a stale row for a conflicting qn", error))?;
        let id = insert_node
            .query_row(
                params![
                    file_id,
                    node.kind,
                    node.name,
                    node.qn,
                    node.start_line,
                    node.end_line,
                    node.signature,
                    if node.is_test { 1i64 } else { 0i64 },
                    node.props.to_json()
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(|error| write_failed("inserting a node", error))?
            .ok_or_else(|| AppError::internal(format!("inserting {} returned no id", node.qn)))?;
        ids.insert(node.qn.clone(), id);
    }
    Ok(ids)
}

/// The id a qn already has in the stage, for an edge end this unit did not just insert.
fn node_id_by_qn(tx: &Transaction<'_>, qn: &str) -> Result<Option<i64>, AppError> {
    let mut statement = tx
        .prepare_cached(SELECT_NODE_ID_BY_QN)
        .map_err(|error| write_failed("preparing the node-id lookup", error))?;
    statement
        .query_row([qn], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|error| write_failed("looking up an edge end", error))
}

/// The `file.id` of the file an edge's site sits in, resolved once per path.
///
/// An edge carries the file its site sits in rather than re-deriving it from `src`: a call edge's
/// source can be a symbol outside the site's file once binding jumps to a superclass.
fn edge_file(
    tx: &Transaction<'_>,
    cache: &mut HashMap<String, i64>,
    path: &str,
) -> Result<i64, AppError> {
    let path = normalize_rel(path);
    if let Some(id) = cache.get(&path) {
        return Ok(*id);
    }
    let id = file_id_of(tx, &path)?.ok_or_else(|| {
        AppError::internal(format!(
            "edge site names {path}, which has no file row"
        ))
        .with_hint(
            "every edge carries the file its site sits in; persist that file before the unit that references it",
        )
    })?;
    cache.insert(path, id);
    Ok(id)
}

/// One `meta` value, `None` when the key is unset. A read that fails is a failure, not an absent
/// key: the tombstone's `last_seen_generation` is a fact about the graph, so guessing it is worse
/// than stopping the run.
fn read_meta(tx: &Transaction<'_>, key: &str) -> Result<Option<String>, AppError> {
    tx.query_row(SELECT_META, [key], |row| row.get::<_, String>(0))
        .optional()
        .map_err(|error| write_failed("reading a meta key", error))
}

/// A statement-level failure: `internal`, naming the write that did not happen. A persist that
/// cannot write one row fails the run; it never skips the row and publishes the rest.
fn write_failed(what: impl std::fmt::Display, error: impl std::fmt::Display) -> AppError {
    AppError::internal(format!("{what}: {error}")).with_hint(FAILED_HINT)
}
