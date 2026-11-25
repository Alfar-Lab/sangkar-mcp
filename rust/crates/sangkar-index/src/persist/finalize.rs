//! Sealing a generation: the whole-graph derivations, the FTS mirror, and the row that publishes the
//! result.
//!
//! Ported from `finalizeGeneration` in `src/indexer/persist.ts`. The order below is the contract and
//! each step is read by the next one:
//!
//! 1. the guards, first, because a publish is recorded once and an unnamed generation is a bug: an
//!    empty id is the pending marker, `mode` is the closed pair `full | fast`, and an id already in
//!    `generation` means a retry started from the wrong stage;
//! 2. the touched set, read from the rows themselves — a re-inserted node has a fresh id and
//!    therefore no `node_metrics` row — and the scope that set buys: a run that removed a symbol
//!    recounts every degree, because cascade took edges out of rows the neighbourhood no longer
//!    reaches;
//! 3. `node_metrics` for every node, then `tested_by` (rebuilt, never patched: the `calls` edges it
//!    reads are the authority, and a stale edge would claim a test that calls nothing), then
//!    `tested_depth` — 1 for a node a test calls, 2 for one `calls` hop further out, `NULL` else;
//! 4. the degrees, for the two-hop neighbourhood or for the whole table;
//! 5. the FTS mirror delta by rowid, or a rebuild from the mirror when the delta covers more than
//!    [`FTS_REBUILD_FRACTION`] of `node`;
//! 6. the pending tombstones stamped with this generation, returned in qn order;
//! 7. `generation`: the previously published row becomes `superseded`, this generation is inserted
//!    `published`, then the `meta` keys the read path serves — in the same transaction as the
//!    generation, so no reader can see a publish without the identity it describes.
//!
//! What this pass does **not** write: `is_entry` and `reachable_count`. Both columns exist on
//! `node_metrics`, and neither is a write-path derivation — the source reads them from the query
//! side, so inventing a pass here would fill them with values nothing asked for.
//!
//! Everything happens inside the caller's [`Transaction`]: this module opens, commits and rolls back
//! nothing, so a failure anywhere discards the whole stage instead of publishing part of a graph.

use std::collections::BTreeSet;

use sangkar_core::errors::AppError;
use sangkar_core::log;
use sangkar_store::schema::{
    META_GENERATION, META_GENERATION_SEQ, META_PROJECT_BRANCH, META_PROJECT_NAME,
    META_PROJECT_REMOTE, META_PROJECT_ROOT,
};
use sangkar_store::{OptionalExtension, ToSql, Transaction, params};

use crate::persist::PENDING_GENERATION;

/// Above this fraction of `node` rows, applying the FTS delta row by row costs more than dropping
/// the index and rebuilding it from the mirror, which is what `DATABASE.md` §5 asks for.
pub const FTS_REBUILD_FRACTION: f64 = 0.35;

/// Above this many touched nodes the two-hop expansion visits more rows than the whole table, so the
/// degree pass stops pretending it is a neighbourhood.
const NEIGHBOURHOOD_LIMIT: usize = 5_000;

/// Bound on one `IN (...)` list, well under SQLite's variable ceiling.
const ID_BATCH: usize = 400;

/// The degree of a node is a count over its own edges, so it is one correlated subquery each. An
/// edge is counted from both ends: `in_degree` counts the rows that land on the node, `out_degree`
/// the rows that leave it.
const DEGREE_UPDATE: &str = concat!(
    "UPDATE node_metrics SET",
    " in_degree = (SELECT count(*) FROM edge e WHERE e.dst_id = node_metrics.node_id),",
    " out_degree = (SELECT count(*) FROM edge e WHERE e.src_id = node_metrics.node_id)"
);

/// The five columns the FTS5 index and its mirror share, in table order.
const MIRROR_COLUMNS: &str = "rowid_, name, qn, signature, docstring, props_text";

/// `tested_by`, derived in one statement: a test node's outgoing `calls` edge into a non-test node is
/// a test of that node. The direction is what puts the edge in the queried symbol's inbound group,
/// and one row per `(test, symbol)` pair is why a test that calls a helper twice does not report two
/// tests.
const TESTED_BY_INSERT: &str = concat!(
    "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)",
    " SELECT c.src_id, c.dst_id, 'tested_by', 'inferred', min(c.file_id), min(c.line), '', 'tested_by'",
    " FROM edge c",
    " JOIN node t ON t.id = c.src_id AND t.is_test = 1",
    " JOIN node d ON d.id = c.dst_id AND d.is_test = 0",
    " WHERE c.kind = 'calls'",
    " GROUP BY c.src_id, c.dst_id",
    " ON CONFLICT DO NOTHING"
);

/// Every node has a metrics row once the pass has run; `INSERT OR IGNORE` leaves the existing values
/// alone, so a degree from the previous publish survives until step 4 recomputes it.
const INSERT_MISSING_METRICS: &str =
    "INSERT OR IGNORE INTO node_metrics (node_id) SELECT id FROM node";

/// The nodes written in this run: a re-inserted node has a fresh id and therefore no metrics row yet.
/// This is what makes the touched set derivable without state passed between calls.
const SELECT_TOUCHED: &str = concat!(
    "SELECT n.id AS id FROM node n WHERE NOT EXISTS",
    " (SELECT 1 FROM node_metrics m WHERE m.node_id = n.id) ORDER BY n.id"
);

const DELETE_TESTED_BY: &str = "DELETE FROM edge WHERE kind = 'tested_by'";
const CLEAR_TESTED_DEPTH: &str = "UPDATE node_metrics SET tested_depth = NULL";

/// Depth 1: the symbol a test calls, which is the target of a derived `tested_by` edge.
const SET_DEPTH_ONE: &str = concat!(
    "UPDATE node_metrics SET tested_depth = 1",
    " WHERE node_id IN (SELECT dst_id FROM edge WHERE kind = 'tested_by')"
);

/// Depth 2: one `calls` hop out from a depth-1 node — the helper of a tested function.
const SET_DEPTH_TWO: &str = concat!(
    "UPDATE node_metrics SET tested_depth = 2 WHERE tested_depth IS NULL AND node_id IN",
    " (SELECT c.dst_id FROM edge c JOIN node_metrics m ON m.node_id = c.src_id",
    " WHERE c.kind = 'calls' AND m.tested_depth = 1)"
);

const COUNT_NODES: &str = "SELECT count(*) AS rows FROM node";
const COUNT_EDGES: &str = "SELECT count(*) AS rows FROM edge";
const COUNT_FILES: &str = "SELECT count(*) AS rows FROM file";
const COUNT_PENDING_REMOVALS: &str =
    "SELECT count(*) AS rows FROM tombstone WHERE removed_in_generation = ?1";
const COUNT_GENERATION: &str = "SELECT count(*) AS rows FROM generation WHERE id = ?1";

/// Mirror rows whose node is gone. Node ids are never reused, so this is the whole stale set: a
/// rewritten file deleted its old rows rather than changing them.
const SELECT_STALE_MIRROR: &str = concat!(
    "SELECT s.rowid_ AS rowid_ FROM node_fts_state s",
    " WHERE NOT EXISTS (SELECT 1 FROM node n WHERE n.id = s.rowid_) ORDER BY s.rowid_"
);

/// Every node as a mirror row, ordered by id: the rebuild's input, because the content table of an
/// external-content FTS5 index is `node_fts_state` and it has to be complete before `rebuild` reads
/// it. The columns are [`MIRROR_COLUMNS`] aliased onto `node`, whose props column is `props`.
const SELECT_ALL_MIRROR: &str = concat!(
    "SELECT n.id AS rowid_, n.name AS name, n.qn AS qn, n.signature AS signature,",
    " n.docstring AS docstring, n.props AS props_text FROM node n ORDER BY n.id"
);

/// The nodes with no mirror row: the other half of the delta, and the only rows worth inserting.
const SELECT_MISSING_MIRROR: &str = concat!(
    "SELECT n.id AS rowid_, n.name AS name, n.qn AS qn, n.signature AS signature,",
    " n.docstring AS docstring, n.props AS props_text FROM node n",
    " WHERE NOT EXISTS (SELECT 1 FROM node_fts_state s WHERE s.rowid_ = n.id) ORDER BY n.id"
);

const DELETE_MIRROR: &str = "DELETE FROM node_fts_state";
const DELETE_MIRROR_ROW: &str = "DELETE FROM node_fts_state WHERE rowid_ = ?1";

/// FTS5's `delete` command: the old column values are needed, which is what the mirror is for.
const DELETE_FTS_ROW: &str = concat!(
    "INSERT INTO node_fts (node_fts, rowid, name, qn, signature, docstring, props_text)",
    " VALUES ('delete', ?1, ?2, ?3, ?4, ?5, ?6)"
);

const INSERT_FTS_ROW: &str = concat!(
    "INSERT INTO node_fts (rowid, name, qn, signature, docstring, props_text)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
);

const REBUILD_FTS: &str = "INSERT INTO node_fts (node_fts) VALUES ('rebuild')";

const SET_TOMBSTONE_GENERATION: &str =
    "UPDATE tombstone SET removed_in_generation = ?1 WHERE removed_in_generation = ?2";
const SELECT_SETTLED_TOMBSTONES: &str =
    "SELECT qn FROM tombstone WHERE removed_in_generation = ?1 ORDER BY qn";

const SELECT_NEXT_SEQ: &str = "SELECT coalesce(max(seq), 0) + 1 AS seq FROM generation";
const SUPERSEDE_PUBLISHED: &str =
    "UPDATE generation SET status = 'superseded' WHERE status = 'published' AND id <> ?1";

/// The publish row: `?1` id through `?10` mode, with the status fixed at `published`.
const INSERT_GENERATION: &str = concat!(
    "INSERT INTO generation (id, seq, published_at, base_commit, worktree_dirty, files_indexed,",
    " nodes, edges, duration_ms, mode, status)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'published')"
);

/// `meta` is a key/value table written from this side only, so one upsert and one insert-once cover
/// every key: `generation` and `generation_seq` change with each publish, the tree's name and root
/// are recorded at first index and must never be erased (`DATABASE.md` §3.1).
const UPSERT_META: &str = concat!(
    "INSERT INTO meta (key, value) VALUES (?1, ?2)",
    " ON CONFLICT(key) DO UPDATE SET value = excluded.value"
);
const INSERT_META_ONCE: &str =
    "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO NOTHING";

/// The hint every statement failure carries: a generation that cannot be written must fail rather
/// than publish the rows it managed to derive.
const FAILED_HINT: &str =
    "the generation is sealed inside the run's own write transaction; nothing was published";

/// The tree's identity (`DATABASE.md` §3.1), written with the generation it describes.
///
/// Absent only for a caller with no filesystem opinion about the tree — a unit test finalizing a
/// hand-built graph — which is why [`GenerationInput::project`] is optional rather than empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectIdentity {
    /// Repo basename at the root the run indexed.
    pub name: String,
    /// Absolute path of the root the run indexed.
    pub root: String,
    /// `git symbolic-ref --short HEAD`, empty on a detached HEAD or when git cannot answer.
    pub branch: String,
    /// `git config --get remote.origin.url`, empty when there is no origin.
    pub remote: String,
}

/// Everything [`finalize_generation`] needs besides the transaction.
#[derive(Clone, Debug, Default)]
pub struct GenerationInput {
    /// The id this publish is recorded under. Never [`PENDING_GENERATION`]: that marker names a
    /// tombstone waiting for a generation, not a generation.
    pub generation_id: String,
    /// The commit the run started from, `None` when the tree is not a repository.
    pub base_commit: Option<String>,
    /// The closed set is `full | fast`; anything else is refused before a row is written.
    pub mode: String,
    /// The tree's identity. `None` leaves the four `project_*` keys as they are.
    pub project: Option<ProjectIdentity>,
    /// Injected clock for `generation.published_at`; defaults to the current instant.
    pub published_at: Option<String>,
    /// Wall time the run took; the worker owns the timing, this module only records it.
    pub duration_ms: i64,
    /// Whether the indexed tree had uncommitted changes at publish.
    pub worktree_dirty: bool,
}

/// Which way the FTS mirror was brought up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FtsMode {
    /// Row by row: the stale mirror rows out, the missing nodes in.
    Delta,
    /// The index rebuilt from the mirror, because the delta covered more than the threshold.
    Rebuild,
}

impl FtsMode {
    /// The wire spelling, identical to the TypeScript union's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Delta => "delta",
            Self::Rebuild => "rebuild",
        }
    }
}

/// Which way the FTS mirror was brought up to date, and why.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FtsReport {
    pub mode: FtsMode,
    /// Mirror rows added (the nodes that had none) or, on a rebuild, every node.
    pub inserted: i64,
    /// Mirror rows dropped because their node is gone.
    pub removed: i64,
    /// `node` rows at the time of the decision: the denominator of [`Self::fraction`].
    pub rows: i64,
    /// `(inserted + removed) / rows` before the decision; above the threshold means a rebuild.
    pub fraction: f64,
}

/// Which nodes the degree pass visited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricsMode {
    /// The touched set plus two hops of `edge` around it.
    Neighbourhood,
    /// Every row of `node_metrics`.
    All,
}

impl MetricsMode {
    /// The wire spelling, identical to the TypeScript union's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Neighbourhood => "neighbourhood",
            Self::All => "all",
        }
    }
}

/// What the degree pass visited, and how many rows it recounted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetricsReport {
    pub mode: MetricsMode,
    pub rows: i64,
}

/// What one publish made true about the graph.
#[derive(Clone, Debug, PartialEq)]
pub struct GenerationResult {
    /// `count(node)` at publish.
    pub nodes: i64,
    /// `count(edge)` at publish, the derived `tested_by` rows included.
    pub edges: i64,
    /// The qns this generation removed, in qn order: `len()` is the count.
    pub tombstones: Vec<String>,
    pub fts: FtsReport,
    pub metrics: MetricsReport,
}

/// Seals a generation: derives `tested_by`, `tested_depth` and the degrees, applies the FTS delta,
/// names the tombstones and records the `generation` row plus `meta.generation`.
///
/// Called once per run, inside the same transaction as the persist calls, before the stage is
/// sealed. The result is what the worker reports; every number in it is a count of rows this
/// transaction can see.
///
/// # Errors
///
/// `internal` for every failure: an empty [`GenerationInput::generation_id`], a mode outside
/// `full | fast`, an id already in `generation`, and any statement SQLite refuses. A failed call
/// leaves the caller's transaction open and uncommitted, so the caller's own rollback is what
/// discards the rows.
pub fn finalize_generation(
    tx: &Transaction<'_>,
    input: &GenerationInput,
) -> Result<GenerationResult, AppError> {
    let generation_id = input.generation_id.as_str();
    if generation_id == PENDING_GENERATION {
        return Err(
            AppError::internal("a generation id may not be the pending marker").with_hint(format!(
                "\"{PENDING_GENERATION}\" marks an unconfirmed tombstone, not a publish"
            )),
        );
    }
    // The closed set the `generation.mode` column is read as. `metrics` and `fts` below have their
    // own two spellings; this is the one a reader filters the publish history by.
    let mode = match input.mode.as_str() {
        "full" | "fast" => input.mode.as_str(),
        unknown => {
            return Err(
                AppError::internal(format!("unknown generation mode {unknown}"))
                    .with_hint("generation.mode is the closed set full | fast"),
            );
        }
    };
    let recorded = count_matching(tx, COUNT_GENERATION, generation_id)?;
    if recorded > 0 {
        return Err(
            AppError::internal(format!("generation {generation_id} is already recorded"))
                .with_hint(
                    "a publish is recorded once; a retry starts from a new stage and a new id",
                ),
        );
    }

    let touched = ids(tx, SELECT_TOUCHED)?;
    let pending_removals = count_matching(tx, COUNT_PENDING_REMOVALS, PENDING_GENERATION)?;

    // A removal deletes its incident edges by cascade, which changes the degree of nodes that are
    // no longer within reach of any touched node and whose previous adjacency is already gone. A run
    // that removed a symbol therefore recounts every row rather than pretend otherwise.
    let scope_all = pending_removals > 0 || touched.len() > NEIGHBOURHOOD_LIMIT;

    tx.execute(INSERT_MISSING_METRICS, [])
        .map_err(|error| failed("writing the missing metrics rows", error))?;
    derive_tested(tx)?;

    let metrics = if scope_all {
        tx.execute(DEGREE_UPDATE, [])
            .map_err(|error| failed("recounting every degree", error))?;
        MetricsReport {
            mode: MetricsMode::All,
            rows: count(tx, COUNT_NODES)?,
        }
    } else {
        let dirty = neighbourhood(tx, &touched)?;
        recount_degrees(tx, &dirty)?;
        MetricsReport {
            mode: MetricsMode::Neighbourhood,
            rows: as_i64(dirty.len()),
        }
    };

    let fts = sync_fts(tx)?;

    // The tombstones this run created are stamped now, and the ordered list is what the answer
    // reports. The "qn came back" pruning is the unit pass's, not this one's: a symbol that
    // returned was never a removal.
    tx.execute(
        SET_TOMBSTONE_GENERATION,
        params![generation_id, PENDING_GENERATION],
    )
    .map_err(|error| failed("naming the generation that removed a symbol", error))?;
    let tombstones = settled_tombstones(tx, generation_id)?;

    let nodes = count(tx, COUNT_NODES)?;
    let edges = count(tx, COUNT_EDGES)?;
    // The graph's file count, not this run's: the incremental planner owns "which files changed",
    // and the generation row records the graph it published — `nodes` and `edges` are the same
    // measure, taken from the same table universe.
    let files_indexed = count(tx, COUNT_FILES)?;
    let seq = tx
        .query_row(SELECT_NEXT_SEQ, [], |row| row.get::<_, i64>(0))
        .map_err(|error| failed("reading the next generation sequence", error))?;

    tx.execute(SUPERSEDE_PUBLISHED, [generation_id])
        .map_err(|error| failed("superseding the previously published generation", error))?;
    let published_at = match input.published_at.as_deref() {
        Some(injected) => injected.to_owned(),
        None => now_iso(),
    };
    let worktree_dirty = if input.worktree_dirty { 1 } else { 0 };
    tx.execute(
        INSERT_GENERATION,
        params![
            generation_id,
            seq,
            published_at,
            input.base_commit,
            worktree_dirty,
            files_indexed,
            nodes,
            edges,
            input.duration_ms,
            mode
        ],
    )
    .map_err(|error| failed("recording the generation", error))?;
    write_meta(tx, META_GENERATION, generation_id)?;
    write_meta(tx, META_GENERATION_SEQ, &seq.to_string())?;
    if let Some(project) = &input.project {
        // The same transaction as `meta.generation`, so a reader that sees a publish sees the
        // identity it was published for.
        write_project_identity(tx, project)?;
    }

    log::debug(
        "generation finalized",
        &[
            ("generationId", generation_id.into()),
            ("seq", seq.into()),
            ("nodes", nodes.into()),
            ("edges", edges.into()),
            ("tombstones", as_i64(tombstones.len()).into()),
            ("fts", fts.mode.as_str().into()),
            ("metrics", metrics.mode.as_str().into()),
        ],
    );

    Ok(GenerationResult {
        nodes,
        edges,
        tombstones,
        fts,
        metrics,
    })
}

/// `tested_by` and `tested_depth`: the two derivations the test-gap answer is built from.
///
/// `tested_by` is derived, so it is rebuilt rather than patched. `tested_depth` is then a two-step
/// consequence of it: depth 1 is the direct target of a `tested_by` edge, depth 2 is one `calls` hop
/// beyond a depth-1 node. Everything else is `NULL`, which is what `summary`'s `test_gaps` counts.
fn derive_tested(tx: &Transaction<'_>) -> Result<(), AppError> {
    tx.execute(DELETE_TESTED_BY, [])
        .map_err(|error| failed("clearing the derived tested_by edges", error))?;
    tx.execute(TESTED_BY_INSERT, [])
        .map_err(|error| failed("deriving the tested_by edges", error))?;
    tx.execute(CLEAR_TESTED_DEPTH, [])
        .map_err(|error| failed("clearing the tested depth", error))?;
    tx.execute(SET_DEPTH_ONE, [])
        .map_err(|error| failed("marking the directly tested nodes", error))?;
    tx.execute(SET_DEPTH_TWO, [])
        .map_err(|error| failed("marking the nodes one hop behind a test", error))?;
    Ok(())
}

/// The nodes whose degree has to be recounted: the touched set plus two hops of `edge` around it.
///
/// One hop is the degree a re-written edge moves at its far end; the second is the neighbourhood a
/// `tested_by` edge and a `tested_depth = 2` value are read from. A `BTreeSet` keeps the walk
/// deterministic — two runs over the same graph prepare the same statements in the same order.
fn neighbourhood(tx: &Transaction<'_>, touched: &[i64]) -> Result<Vec<i64>, AppError> {
    let mut dirty: BTreeSet<i64> = touched.iter().copied().collect();
    for _hop in 0..2 {
        let current: Vec<i64> = dirty.iter().copied().collect();
        for chunk in current.chunks(ID_BATCH) {
            let list = placeholders(chunk.len());
            let inbound =
                format!("SELECT DISTINCT src_id AS id FROM edge WHERE dst_id IN ({list})");
            let outbound =
                format!("SELECT DISTINCT dst_id AS id FROM edge WHERE src_id IN ({list})");
            for sql in [inbound, outbound] {
                for id in ids_in(tx, &sql, chunk)? {
                    dirty.insert(id);
                }
            }
        }
    }
    Ok(dirty.into_iter().collect())
}

/// Recounts the degrees of `ids` in batches, so one statement never carries an unbounded parameter
/// list. A statement per arity is the cached shape, not a query built per node.
fn recount_degrees(tx: &Transaction<'_>, ids: &[i64]) -> Result<(), AppError> {
    for chunk in ids.chunks(ID_BATCH) {
        let sql = format!(
            "{DEGREE_UPDATE} WHERE node_id IN ({})",
            placeholders(chunk.len())
        );
        let pointers: Vec<&dyn ToSql> = chunk.iter().map(|id| id as &dyn ToSql).collect();
        tx.execute(&sql, &pointers[..])
            .map_err(|error| failed("recounting a node's degree", error))?;
    }
    Ok(())
}

/// Brings `node_fts` in line with `node`, by rowid.
///
/// The delta is derivable because node ids are never reused: a rewritten file deletes its old rows
/// and inserts new ones, so a node that changed has a new id. That leaves exactly two sets — mirror
/// rows whose node is gone, and nodes with no mirror row — and no bookkeeping between calls. A
/// rewritten row therefore counts twice, once on each side, which errs towards rebuilding rather
/// than towards a stale index. Above [`FTS_REBUILD_FRACTION`] the row-by-row cost exceeds a rebuild
/// from the mirror, which is what `DATABASE.md` §5 asks for.
fn sync_fts(tx: &Transaction<'_>) -> Result<FtsReport, AppError> {
    let rows = count(tx, COUNT_NODES)?;
    let stale = ids(tx, SELECT_STALE_MIRROR)?;
    let missing = mirror_rows(tx, SELECT_MISSING_MIRROR)?;
    let moved = stale.len() + missing.len();
    let fraction = if rows == 0 {
        0.0
    } else {
        as_i64(moved) as f64 / rows as f64
    };
    let insert_mirror =
        format!("INSERT INTO node_fts_state ({MIRROR_COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)");

    if fraction > FTS_REBUILD_FRACTION {
        // Rebuild from the mirror, not from `node`: the content table of an external-content FTS5
        // index is `node_fts_state`, so it has to be complete before `rebuild` reads it.
        tx.execute(DELETE_MIRROR, [])
            .map_err(|error| failed("clearing the FTS mirror", error))?;
        let all = mirror_rows(tx, SELECT_ALL_MIRROR)?;
        let mut insert = tx
            .prepare(&insert_mirror)
            .map_err(|error| failed("preparing a mirror insert", error))?;
        for row in &all {
            insert
                .execute(params![
                    row.rowid_,
                    row.name,
                    row.qn,
                    row.signature,
                    row.docstring,
                    row.props_text
                ])
                .map_err(|error| failed("inserting a mirror row", error))?;
        }
        drop(insert);
        tx.execute(REBUILD_FTS, [])
            .map_err(|error| failed("rebuilding the FTS index", error))?;
        log::debug(
            "fts rebuilt",
            &[
                ("rows", rows.into()),
                ("stale", as_i64(stale.len()).into()),
                ("fraction", fraction.into()),
            ],
        );
        return Ok(FtsReport {
            mode: FtsMode::Rebuild,
            inserted: as_i64(all.len()),
            removed: as_i64(stale.len()),
            rows,
            fraction,
        });
    }

    let select_mirror = format!("SELECT {MIRROR_COLUMNS} FROM node_fts_state WHERE rowid_ = ?1");
    let mut mirror_of = tx
        .prepare(&select_mirror)
        .map_err(|error| failed("preparing the mirror read", error))?;
    let mut delete_fts = tx
        .prepare(DELETE_FTS_ROW)
        .map_err(|error| failed("preparing the FTS delete", error))?;
    let mut delete_mirror = tx
        .prepare(DELETE_MIRROR_ROW)
        .map_err(|error| failed("preparing the mirror delete", error))?;
    for rowid in &stale {
        let found = mirror_of
            .query_row([*rowid], |row| {
                Ok(MirrorRow {
                    rowid_: row.get(0)?,
                    name: row.get(1)?,
                    qn: row.get(2)?,
                    signature: row.get(3)?,
                    docstring: row.get(4)?,
                    props_text: row.get(5)?,
                })
            })
            .optional()
            .map_err(|error| failed("reading a mirror row", error))?;
        let Some(row) = found else {
            continue;
        };
        delete_fts
            .execute(params![
                row.rowid_,
                row.name,
                row.qn,
                row.signature,
                row.docstring,
                row.props_text
            ])
            .map_err(|error| failed("deleting an FTS row", error))?;
        delete_mirror
            .execute([row.rowid_])
            .map_err(|error| failed("deleting a mirror row", error))?;
    }
    drop(delete_mirror);
    drop(delete_fts);
    drop(mirror_of);

    let mut insert_mirror = tx
        .prepare(&insert_mirror)
        .map_err(|error| failed("preparing a mirror insert", error))?;
    let mut insert_fts = tx
        .prepare(INSERT_FTS_ROW)
        .map_err(|error| failed("preparing an FTS insert", error))?;
    for row in &missing {
        insert_mirror
            .execute(params![
                row.rowid_,
                row.name,
                row.qn,
                row.signature,
                row.docstring,
                row.props_text
            ])
            .map_err(|error| failed("inserting a mirror row", error))?;
        insert_fts
            .execute(params![
                row.rowid_,
                row.name,
                row.qn,
                row.signature,
                row.docstring,
                row.props_text
            ])
            .map_err(|error| failed("inserting an FTS row", error))?;
    }

    log::debug(
        "fts delta applied",
        &[
            ("rows", rows.into()),
            ("inserted", as_i64(missing.len()).into()),
            ("removed", as_i64(stale.len()).into()),
            ("fraction", fraction.into()),
        ],
    );
    Ok(FtsReport {
        mode: FtsMode::Delta,
        inserted: as_i64(missing.len()),
        removed: as_i64(stale.len()),
        rows,
        fraction,
    })
}

/// The identity keys (`DATABASE.md` §3.1), written with the generation they describe.
///
/// `project_name` and `project_root` are recorded **once**: §3.1 fixes them at first index and makes
/// `project_root` the `root_mismatch` guard, which a re-index of a moved checkout must report rather
/// than erase. Branch and remote are what the tree is on *now*, so each publish refreshes them.
fn write_project_identity(
    tx: &Transaction<'_>,
    identity: &ProjectIdentity,
) -> Result<(), AppError> {
    write_meta_once(tx, META_PROJECT_NAME, &identity.name)?;
    write_meta_once(tx, META_PROJECT_ROOT, &identity.root)?;
    write_meta(tx, META_PROJECT_BRANCH, &identity.branch)?;
    write_meta(tx, META_PROJECT_REMOTE, &identity.remote)
}

/// The tombstones this generation names, in qn order.
fn settled_tombstones(tx: &Transaction<'_>, generation_id: &str) -> Result<Vec<String>, AppError> {
    let mut select = tx
        .prepare(SELECT_SETTLED_TOMBSTONES)
        .map_err(|error| failed("preparing the tombstone read", error))?;
    let rows = select
        .query_map([generation_id], |row| row.get::<_, String>(0))
        .map_err(|error| failed("reading the settled tombstones", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("reading the settled tombstones", error))
}

/// `INSERT ... ON CONFLICT(key) DO UPDATE`: the key that describes the current state.
fn write_meta(tx: &Transaction<'_>, key: &str, value: &str) -> Result<(), AppError> {
    tx.execute(UPSERT_META, params![key, value])
        .map_err(|error| failed(format!("writing meta.{key}"), error))?;
    Ok(())
}

/// `INSERT ... ON CONFLICT(key) DO NOTHING`: the key that is recorded once, forever.
fn write_meta_once(tx: &Transaction<'_>, key: &str, value: &str) -> Result<(), AppError> {
    tx.execute(INSERT_META_ONCE, params![key, value])
        .map_err(|error| failed(format!("recording meta.{key}"), error))?;
    Ok(())
}

/// One `count(*)` read. The statement is a constant at every call site, never assembled from input.
fn count(tx: &Transaction<'_>, sql: &str) -> Result<i64, AppError> {
    tx.query_row(sql, [], |row| row.get::<_, i64>(0))
        .map_err(|error| failed("counting rows", error))
}

/// The same read for a statement with one bound value: both the pending-removal and the
/// already-recorded checks are one text comparison.
fn count_matching(tx: &Transaction<'_>, sql: &str, value: &str) -> Result<i64, AppError> {
    tx.query_row(sql, [value], |row| row.get::<_, i64>(0))
        .map_err(|error| failed("counting rows", error))
}

/// One integer column, every row: what the touched set and the stale-mirror read return.
fn ids(tx: &Transaction<'_>, sql: &str) -> Result<Vec<i64>, AppError> {
    let mut select = tx
        .prepare(sql)
        .map_err(|error| failed("preparing a node id read", error))?;
    let rows = select
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|error| failed("reading node ids", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("reading node ids", error))
}

/// The same read for a batched `IN (...)` list: the arity is in the statement, the ids are the
/// statement's parameters.
fn ids_in(tx: &Transaction<'_>, sql: &str, values: &[i64]) -> Result<Vec<i64>, AppError> {
    let pointers: Vec<&dyn ToSql> = values.iter().map(|value| value as &dyn ToSql).collect();
    let mut select = tx
        .prepare(sql)
        .map_err(|error| failed("preparing a neighbourhood read", error))?;
    let rows = select
        .query_map(&pointers[..], |row| row.get::<_, i64>(0))
        .map_err(|error| failed("reading the neighbourhood", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("reading the neighbourhood", error))
}

/// Every row the statement returns as a [`MirrorRow`], in the statement's own order.
fn mirror_rows(tx: &Transaction<'_>, sql: &str) -> Result<Vec<MirrorRow>, AppError> {
    let mut select = tx
        .prepare(sql)
        .map_err(|error| failed("preparing a mirror read", error))?;
    let rows = select
        .query_map([], |row| {
            Ok(MirrorRow {
                rowid_: row.get(0)?,
                name: row.get(1)?,
                qn: row.get(2)?,
                signature: row.get(3)?,
                docstring: row.get(4)?,
                props_text: row.get(5)?,
            })
        })
        .map_err(|error| failed("reading mirror rows", error))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| failed("reading mirror rows", error))
}

/// The mirror row an FTS5 `delete` command needs: FTS5 reads the old values back from the content
/// table, so the mirror is what makes a delete possible at all.
struct MirrorRow {
    rowid_: i64,
    name: String,
    qn: String,
    signature: Option<String>,
    docstring: Option<String>,
    props_text: Option<String>,
}

/// `SELECT ?1, ?2, ...` for a batched `IN` list: the arity is part of the statement, never a
/// per-node loop.
fn placeholders(count: usize) -> String {
    let mut list = String::with_capacity(count * 4);
    for index in 1..=count {
        if index > 1 {
            list.push_str(", ");
        }
        list.push('?');
        list.push_str(&index.to_string());
    }
    list
}

/// `count` as the `i64` a column stores. A usize that does not fit is a graph SQLite could not have
/// written, so the clamp is a placeholder rather than a lie worth an error path.
fn as_i64(count: usize) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn failed(what: impl std::fmt::Display, error: impl std::fmt::Display) -> AppError {
    AppError::internal(format!("{what}: {error}")).with_hint(FAILED_HINT)
}

/// `new Date().toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// The kernel formats the same string for a log line, where it is private to that module, and
/// `sangkar-cli` keeps its own copy for the clock its handlers read. This is the third caller and
/// therefore the third copy of the same twenty lines, rather than a new dependency or a widened
/// kernel API from a module this pass does not own.
fn now_iso() -> String {
    let millis = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // A host clock set before 1970 is not a state this tool supports; the epoch keeps the row
        // well formed instead of storing a negative timestamp the TypeScript never produced.
        Err(_) => 0,
    };
    let seconds = millis.div_euclid(1_000);
    let subsecond = millis.rem_euclid(1_000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let second_of_day = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:03}Z")
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date for a day count since
/// 1970-01-01, with the leap-year rule baked into the era arithmetic rather than branchy.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}
