//! `GraphReader`: the aggregate of every read primitive, bound to one connection and handed to a
//! tool as `ToolContext::reader` (`TECH-SPEC.md` §6, `plan.constraint_invariant.md`).
//!
//! Ported from `src/shared/store/read/reader.ts`. The methods take no connection. That is the whole
//! point of the trait: a handler holds a [`GraphReader`], so "tools never see a connection" is a
//! property of the type rather than a rule someone has to remember in review. [`SqliteReader::new`]
//! closes over the handle once, at the composition root, and what a tool holds afterwards can only
//! read.
//!
//! The free functions in [`crate::read::nodes`], [`crate::read::edges`], [`crate::read::metrics`],
//! [`crate::read::coverage`] and [`crate::read::files`] keep the `conn`-first shape, because they
//! are the kernel and a caller inside this crate may hold a raw handle. This module is the seam
//! above them: it maps each of the 22 methods to one prepared statement and returns the row types
//! those primitives already define, so no handler ever names a column.
//!
//! Every query is a fixed shape assembled once by [`Conn::prepared`]; the only shape that varies is
//! an `IN (…)` list, whose arity is part of the SQL text. Two families vary that way —
//! [`GraphReader::find_by_name`]'s kind filter (an arity ladder in `nodes.rs`) and
//! [`GraphReader::test_gaps`]'s id list — and both are keyed by query *family* here, because a
//! `&'static str` key cannot carry a caller's cardinality; the arity-varying SQL stays bounded by
//! the driver's own cache, which is keyed by SQL text and capped at the same
//! [`crate::db::STATEMENT_CACHE_CAPACITY`] (the note in `coverage.rs` makes the same call for its
//! prefix list).
//!
//! `limit` on [`GraphReader::entry_points`], [`GraphReader::hotspots`] and
//! [`GraphReader::read_node_fts`] is a statement bound, not a product knob: this module decides
//! what is true and in which stable order, the tool decides what to show and how much of it.
//!
//! Two members of the TypeScript interface are deliberately absent:
//!
//! - `inboundMany`/`outboundMany` (optional in `reader.ts:201-202`, defined in `edges.ts`) are the
//!   batched frontier pair. Neither is one of the 22, and no task owns them.
//! - `statementCacheEntries` (`reader.ts:229`) reports the handle's LRU size. It is not one of the
//!   22 either; [`SqliteReader::cache_stats`] is the inherent method a status report reads, and
//!   [`crate::db::statement_cache_stats`] is the process-wide sum.
//!
//! The row types carry no `serde` derives: this crate depends on neither `serde` nor `serde_json`,
//! so the crate that serialises a payload adds the attributes it needs. Every field is a plain
//! column already read into its Rust type, and `props` stays raw text for the same reason
//! (`nodes.rs` documents that boundary).

use rusqlite::{OptionalExtension, Row, params, params_from_iter};
use sangkar_core::envelope::TestCensus;
use sangkar_core::errors::AppError;

use crate::db::{Conn, StatementCacheStats};
use crate::read::coverage::{
    IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef, collect, not_indexed_all,
    not_indexed_count, rows_for_paths, unresolved_for_files,
};
use crate::read::edges::{NeighbourEdge, inbound, outbound};
use crate::read::files::{
    LanguageCensus, all_files, census_by_language, file_by_path, tests_census,
};
use crate::read::metrics::{NodeMetrics, metrics_for};
use crate::read::nodes::{
    NodeRow, find_by_name, find_by_qn, members, nodes_by_file, placeholders, query_failed,
};

/// `test_gaps` is `tested_depth IS NULL OR tested_depth > 2` (`DATABASE.md` §3.4): a symbol with no
/// test within two hops is what a reviewer means by untested, and depth 0 (tested directly) and 1
/// (tested through a direct caller) are not gaps.
pub const TEST_GAP_MAX_DEPTH: i64 = 2;

/// Identity and health from `meta`, plus the generation the cursor binds to.
///
/// Mirrors `GraphMeta` in `src/shared/store/read/reader.ts`, with `schema_version` and `embed_dim`
/// read from their own `meta` keys and `generation_seq` taken from the generation row `meta.generation`
/// names. Every optional field is `None` for a key the database never wrote — a dropped
/// `project_name` row is an absent identity, not an error — and an empty value reads as absent for
/// the identity fields, which is what the TypeScript's `metaValue` does with the empty string the
/// indexer writes for a branch or remote git could not answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphMeta {
    /// `meta.schema_version`: the migration ledger's version, `None` before the first migration.
    pub schema_version: Option<i64>,
    /// `meta.generation`: the generation a cursor binds to; `None` on an unpublished database.
    pub generation: Option<String>,
    /// `generation.seq` of [`Self::generation`]; `None` when no row carries that id. It is *not*
    /// the newest publish — [`GraphReader::latest_generation`] answers that — so a caller can tell
    /// "the generation this graph is serving" from "the generation last written".
    pub generation_seq: Option<i64>,
    /// `meta.indexer_state`: `idle` | `running` | `failed`, as the indexer last wrote it.
    pub indexer_state: Option<String>,
    /// `meta.consecutive_failures`, `0` when the indexer never recorded one.
    pub consecutive_failures: i64,
    /// `meta.project_name`: display identity, fixed at first index (`DATABASE.md` §3.1).
    pub project_name: Option<String>,
    /// `meta.project_root`: the guard against answering about a different tree.
    pub project_root: Option<String>,
    /// `meta.project_branch`: the branch `HEAD` named at publish, `None` on a detached HEAD or when
    /// the publish could not ask git.
    pub project_branch: Option<String>,
    /// `meta.project_remote`: `remote.origin.url` at publish, `None` when the clone has no origin.
    pub project_remote: Option<String>,
    /// `meta.embed_dim`: the vector width, `None` while the graph carries no embeddings.
    pub embed_dim: Option<i64>,
}

/// One publish, mapped from the `generation` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenerationRow {
    pub id: String,
    pub seq: i64,
    pub published_at: String,
    pub base_commit: Option<String>,
    pub worktree_dirty: bool,
    pub files_indexed: i64,
    pub nodes: i64,
    pub edges: i64,
    pub duration_ms: i64,
    pub mode: String,
    pub status: String,
}

/// A symbol that disappeared, kept so `summary(change)` can report it without the base commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TombstoneRow {
    pub qn: String,
    pub kind: String,
    pub file_id: Option<i64>,
    pub last_seen_generation: String,
    pub removed_in_generation: String,
    /// A removed symbol that still had callers is the highest-value review signal.
    pub had_callers: bool,
}

/// `start_line`/`end_line`, the `lines` pair every tool payload shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineRange {
    pub start: i64,
    pub end: i64,
}

/// One symbol with no inbound edges worth naming: what `summary(repo)` calls `entry_points`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryPointRow {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    /// `file.path` of the defining file.
    pub file: String,
    pub lines: LineRange,
    /// `reachable_count`, computed at publish from the entry point; `None` when unmeasured.
    pub reachable: Option<i64>,
}

/// One highly connected symbol, ordered by total degree (`DATABASE.md` §6 `summary(repo)`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotspotRow {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub lines: LineRange,
    pub in_degree: i64,
    pub out_degree: i64,
}

/// One symbol that failed the test-gap test. `tested_depth` carries the measurement — `None` means
/// no test is reachable at all, a number greater than [`TEST_GAP_MAX_DEPTH`] means one is reachable
/// but far — so a tool can word the reason without re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestGapRow {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub lines: LineRange,
    pub tested_depth: Option<i64>,
}

/// One FTS5 hit. `score` is the raw `bm25()` value, where **smaller is better** (the function
/// returns a negative number), which is why the ordering below is ascending: a tool decides how to
/// present it, but never has to re-sort to know which rows are best.
#[derive(Clone, Debug, PartialEq)]
pub struct FtsHit {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub lines: LineRange,
    pub score: f64,
}

/// Every read a tool may perform. One method per query shape in `DATABASE.md` §6, so a handler's
/// data needs are visible in one trait and a fake is a bounded amount of work.
///
/// Object-safe on purpose: `ToolContext::reader` is a `Rc<dyn GraphReader>`, so a handler test can
/// substitute a fake without a database and without this crate.
pub trait GraphReader {
    /// Exact `qn` resolution (unique index); `None` when nothing matches.
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError>;
    /// Exact bare-name resolution, ordered by `qn`; `kinds` is all-or-nothing.
    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError>;
    /// Direct children of `parent_id` in source order (`expand(what="members")`).
    fn members(&self, parent_id: i64) -> Result<Vec<NodeRow>, AppError>;
    /// Every node one file defines, in source order.
    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError>;
    /// Edges into `node_id` with the calling node attached.
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError>;
    /// Edges out of `node_id` with the called node attached.
    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError>;
    /// Derived degrees, entry flag and `tested_depth`; `None` when the publish never measured it.
    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError>;
    /// Indexed files and skipped paths under `prefixes`: both halves of a coverage answer.
    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError>;
    /// Every skipped path in the graph, not just those under a requested prefix.
    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError>;
    /// The repo-wide number of skipped paths: the one total every tool can agree on.
    fn not_indexed_count(&self) -> Result<i64, AppError>;
    /// Unbound call sites for `file_ids`; an empty list is an absence only when coverage is complete.
    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError>;
    /// One indexed file by repo-relative path; `None` means "look in [`Self::rows_for_paths`]".
    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError>;
    /// Every indexed file in path order: the staleness manifest.
    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError>;
    /// `{language, files, parsed, failed}` per language, `not_indexed` folded in.
    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError>;
    /// Indexed test symbols and the languages behind them.
    fn tests_census(&self) -> Result<TestCensus, AppError>;
    /// Identity and health; never the environment, never the filesystem.
    fn meta_for(&self) -> Result<GraphMeta, AppError>;
    /// The newest publish by `seq`; `None` before the first publish.
    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError>;
    /// Symbols removed by the publish named `since_generation`, over `tombstone(qn)`.
    fn removed_symbols_for(&self, since_generation: &str) -> Result<Vec<TombstoneRow>, AppError>;
    /// Entry points by reachable count descending, over `idx_metrics_entry`.
    fn entry_points(&self, limit: usize) -> Result<Vec<EntryPointRow>, AppError>;
    /// Top symbols by `in_degree + out_degree` descending, ties by `id`.
    fn hotspots(&self, limit: usize) -> Result<Vec<HotspotRow>, AppError>;
    /// Gaps among `ids` where `tested_depth` is `NULL` or greater than [`TEST_GAP_MAX_DEPTH`].
    fn test_gaps(&self, ids: &[i64]) -> Result<Vec<TestGapRow>, AppError>;
    /// FTS5 `MATCH`, ordered by `bm25` ascending, then `in_degree` descending, then `id` ascending.
    fn read_node_fts(&self, match_query: &str, limit: usize) -> Result<Vec<FtsHit>, AppError>;
}

/// The one implementation: [`GraphReader`] over a [`Conn`].
///
/// The handle is owned, so dropping the reader closes the connection; the composition root builds
/// one per [`crate::pool::ReaderPool`] entry and hands `Rc<dyn GraphReader>` out.
#[derive(Debug)]
pub struct SqliteReader {
    conn: Conn,
}

impl SqliteReader {
    /// Binds `conn` once. Nothing else in the process needs to know about the handle.
    #[must_use]
    pub fn new(conn: Conn) -> Self {
        Self { conn }
    }

    /// What this handle's prepared-statement cache has done (`store/db.ts`
    /// `statementCacheStats().size`).
    ///
    /// Inherent rather than a 23rd trait method: it reports on *this* handle, which a fake has
    /// nothing to say about, and `status.health` is the only reader.
    #[must_use]
    pub fn cache_stats(&self) -> StatementCacheStats {
        self.conn.statement_cache_stats()
    }

    /// `generation.seq` of the publish named `id`; `None` when no row carries it (a generation
    /// pruned between publishes, or a `meta` row written by another build).
    fn generation_seq(&self, id: &str) -> Result<Option<i64>, AppError> {
        let mut statement = self.conn.prepared(GENERATION_SEQ_KEY, |connection| {
            connection.prepare_cached(GENERATION_SEQ_SQL)
        })?;
        statement
            .query_row([id], |row| row.get::<_, i64>("seq"))
            .optional()
            .map_err(|error| query_failed(GENERATION_SEQ_KEY, &error))
    }
}

/// The identity columns every ranked row selects, so the four ranked queries stay comparable.
///
/// A macro for the same reason `nodes.rs` has one: `concat!` takes literals, and the alternative is
/// four copies of the column list that would drift.
macro_rules! ranked_node_select {
    () => {
        concat!(
            "n.id AS id, n.qn AS qn, n.kind AS kind, f.path AS path,",
            " n.start_line AS start_line, n.end_line AS end_line"
        )
    };
}

const META_KEY: &str = "reader.meta";
const META_SQL: &str = concat!(
    "SELECT m.key AS key, m.value AS value FROM meta m WHERE m.key IN",
    " ('project_name', 'project_root', 'project_branch', 'project_remote', 'generation',",
    " 'indexer_state', 'consecutive_failures', 'schema_version', 'embed_dim')"
);

const GENERATION_SEQ_KEY: &str = "reader.generationSeq";
const GENERATION_SEQ_SQL: &str = "SELECT g.seq AS seq FROM generation g WHERE g.id = ?";

const LATEST_GENERATION_KEY: &str = "reader.latestGeneration";
const LATEST_GENERATION_SQL: &str = concat!(
    "SELECT g.id AS id, g.seq AS seq, g.published_at AS published_at,",
    " g.base_commit AS base_commit, g.worktree_dirty AS worktree_dirty,",
    " g.files_indexed AS files_indexed, g.nodes AS nodes, g.edges AS edges,",
    " g.duration_ms AS duration_ms, g.mode AS mode, g.status AS status",
    " FROM generation g ORDER BY g.seq DESC LIMIT 1"
);

const TOMBSTONES_KEY: &str = "reader.tombstones";
const TOMBSTONES_SQL: &str = concat!(
    "SELECT t.qn AS qn, t.kind AS kind, t.file_id AS file_id,",
    " t.last_seen_generation AS last_seen_generation,",
    " t.removed_in_generation AS removed_in_generation, t.had_callers AS had_callers",
    " FROM tombstone t WHERE t.removed_in_generation = ? ORDER BY t.qn"
);

const ENTRY_POINTS_KEY: &str = "reader.entryPoints";
const ENTRY_POINTS_SQL: &str = concat!(
    "SELECT ",
    ranked_node_select!(),
    ", m.reachable_count AS reachable_count",
    " FROM node_metrics m JOIN node n ON n.id = m.node_id",
    " JOIN file f ON f.id = n.file_id WHERE m.is_entry = 1",
    " ORDER BY (m.reachable_count IS NULL) ASC, m.reachable_count DESC, n.id ASC LIMIT ?"
);

const HOTSPOTS_KEY: &str = "reader.hotspots";
const HOTSPOTS_SQL: &str = concat!(
    "SELECT ",
    ranked_node_select!(),
    ", m.in_degree AS in_degree, m.out_degree AS out_degree",
    " FROM node_metrics m JOIN node n ON n.id = m.node_id",
    " JOIN file f ON f.id = n.file_id",
    " ORDER BY (m.in_degree + m.out_degree) DESC, n.id ASC LIMIT ?"
);

/// One key for the whole `test_gaps` family: the SQL's `IN (…)` arity is the caller's, so the text
/// varies while the key cannot (see the module note on the two arity-varying shapes).
const TEST_GAPS_KEY: &str = "reader.testGaps";

const NODE_FTS_KEY: &str = "reader.nodeFts";
const NODE_FTS_SQL: &str = concat!(
    "SELECT ",
    ranked_node_select!(),
    ", bm25(node_fts) AS score FROM node_fts",
    " JOIN node n ON n.id = node_fts.rowid JOIN file f ON f.id = n.file_id",
    " LEFT JOIN node_metrics m ON m.node_id = n.id",
    " WHERE node_fts MATCH ?",
    " ORDER BY bm25(node_fts) ASC, COALESCE(m.in_degree, 0) DESC, n.id ASC LIMIT ?"
);

/// One `meta` row, before it is folded into [`GraphMeta`].
#[derive(Debug)]
struct MetaSqlRow {
    key: String,
    value: String,
}

fn read_meta_row(row: &Row<'_>) -> rusqlite::Result<MetaSqlRow> {
    Ok(MetaSqlRow {
        key: row.get("key")?,
        value: row.get("value")?,
    })
}

/// One `meta` value as a tool sees it: absent and empty are the same `None`.
///
/// The indexer writes an empty string for `project_branch`/`project_remote` when git could not
/// answer, and an identity field that renders as `""` in a payload would read like a name that is
/// blank rather than a fact the graph does not have.
fn meta_text(values: &[MetaSqlRow], key: &str) -> Option<String> {
    values
        .iter()
        .find(|row| row.key == key)
        .map(|row| row.value.clone())
        .filter(|value| !value.is_empty())
}

/// One `meta` value with the empty string kept: `generation` and `indexer_state` are states, and
/// the TypeScript only nulls the identity fields.
fn meta_raw(values: &[MetaSqlRow], key: &str) -> Option<String> {
    values
        .iter()
        .find(|row| row.key == key)
        .map(|row| row.value.clone())
}

/// One `meta` value read back as an integer; a value that is not an integer reads as absent.
///
/// `schema_version` and `embed_dim` are written by this build (`migrate`, or nothing at all), so a
/// non-numeric value is a foreign writer, and degrading it to `None` matches
/// [`crate::migrations::read_schema_version`]'s own guard rather than raising on a key nothing
/// reads for correctness.
fn meta_integer(values: &[MetaSqlRow], key: &str) -> Option<i64> {
    meta_raw(values, key).and_then(|raw| raw.trim().parse::<i64>().ok())
}

/// `meta.consecutive_failures` is written by the indexer, so a non-integer is a writer bug and
/// raises instead of degrading to `0`: "no failures" and "unreadable counter" are different
/// answers to `status.health`, and only one of them is true.
fn failure_count(raw: Option<String>) -> Result<i64, AppError> {
    let Some(raw) = raw else {
        return Ok(0);
    };
    match raw.trim().parse::<i64>() {
        Ok(value) if value >= 0 => Ok(value),
        _ => Err(AppError::internal(format!(
            "meta.consecutive_failures is not a non-negative integer: {raw}"
        ))
        .with_hint("re-index the database; the counter is written by the indexer")),
    }
}

/// The six columns [`ranked_node_select!`] selects, in one reader, so the four ranked mappers
/// cannot disagree about which column is which.
#[derive(Debug)]
struct RankedColumns {
    id: i64,
    qn: String,
    kind: String,
    file: String,
    lines: LineRange,
}

impl RankedColumns {
    fn read(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            qn: row.get("qn")?,
            kind: row.get("kind")?,
            file: row.get("path")?,
            lines: LineRange {
                start: row.get("start_line")?,
                end: row.get("end_line")?,
            },
        })
    }
}

impl EntryPointRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ranked = RankedColumns::read(row)?;
        Ok(Self {
            id: ranked.id,
            qn: ranked.qn,
            kind: ranked.kind,
            file: ranked.file,
            lines: ranked.lines,
            reachable: row.get("reachable_count")?,
        })
    }
}

impl HotspotRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ranked = RankedColumns::read(row)?;
        Ok(Self {
            id: ranked.id,
            qn: ranked.qn,
            kind: ranked.kind,
            file: ranked.file,
            lines: ranked.lines,
            in_degree: row.get("in_degree")?,
            out_degree: row.get("out_degree")?,
        })
    }
}

impl TestGapRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ranked = RankedColumns::read(row)?;
        Ok(Self {
            id: ranked.id,
            qn: ranked.qn,
            kind: ranked.kind,
            file: ranked.file,
            lines: ranked.lines,
            tested_depth: row.get("tested_depth")?,
        })
    }
}

impl FtsHit {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let ranked = RankedColumns::read(row)?;
        Ok(Self {
            id: ranked.id,
            qn: ranked.qn,
            kind: ranked.kind,
            file: ranked.file,
            lines: ranked.lines,
            score: row.get("score")?,
        })
    }
}

/// A `usize` limit bound as SQLite's `LIMIT`; a count wider than `i64` is unrepresentable as a row
/// count, so it saturates rather than wrapping to a negative bound that would mean "no limit".
fn limit_param(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

impl GraphReader for SqliteReader {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        find_by_qn(&self.conn, qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        find_by_name(&self.conn, name, kinds)
    }

    fn members(&self, parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        members(&self.conn, parent_id)
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        nodes_by_file(&self.conn, file_id)
    }

    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        inbound(&self.conn, node_id, kinds)
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        outbound(&self.conn, node_id, kinds)
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        metrics_for(&self.conn, node_id)
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        rows_for_paths(&self.conn, prefixes)
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError> {
        not_indexed_all(&self.conn)
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        not_indexed_count(&self.conn)
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        unresolved_for_files(&self.conn, file_ids)
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        file_by_path(&self.conn, path)
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        all_files(&self.conn)
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        census_by_language(&self.conn)
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        tests_census(&self.conn)
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        // `meta` is one row per key; the identity, health and generation keys are read in one lookup.
        let values = collect(
            self.conn
                .prepared(META_KEY, |connection| connection.prepare_cached(META_SQL))?,
            [],
            META_KEY,
            read_meta_row,
        )?;
        let generation = meta_raw(&values, "generation");
        let generation_seq = match &generation {
            Some(id) => self.generation_seq(id)?,
            None => None,
        };
        Ok(GraphMeta {
            schema_version: meta_integer(&values, "schema_version"),
            generation,
            generation_seq,
            indexer_state: meta_raw(&values, "indexer_state"),
            consecutive_failures: failure_count(meta_raw(&values, "consecutive_failures"))?,
            project_name: meta_text(&values, "project_name"),
            project_root: meta_text(&values, "project_root"),
            project_branch: meta_text(&values, "project_branch"),
            project_remote: meta_text(&values, "project_remote"),
            embed_dim: meta_integer(&values, "embed_dim"),
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        let mut statement = self.conn.prepared(LATEST_GENERATION_KEY, |connection| {
            connection.prepare_cached(LATEST_GENERATION_SQL)
        })?;
        statement
            .query_row([], |row| {
                Ok(GenerationRow {
                    id: row.get("id")?,
                    seq: row.get("seq")?,
                    published_at: row.get("published_at")?,
                    base_commit: row.get("base_commit")?,
                    // `worktree_dirty` is an `INTEGER` column; the boolean view belongs here.
                    worktree_dirty: row.get::<_, i64>("worktree_dirty")? != 0,
                    files_indexed: row.get("files_indexed")?,
                    nodes: row.get("nodes")?,
                    edges: row.get("edges")?,
                    duration_ms: row.get("duration_ms")?,
                    mode: row.get("mode")?,
                    status: row.get("status")?,
                })
            })
            .optional()
            .map_err(|error| query_failed(LATEST_GENERATION_KEY, &error))
    }

    fn removed_symbols_for(&self, since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        collect(
            self.conn.prepared(TOMBSTONES_KEY, |connection| {
                connection.prepare_cached(TOMBSTONES_SQL)
            })?,
            [since_generation],
            TOMBSTONES_KEY,
            |row| {
                Ok(TombstoneRow {
                    qn: row.get("qn")?,
                    kind: row.get("kind")?,
                    file_id: row.get("file_id")?,
                    last_seen_generation: row.get("last_seen_generation")?,
                    removed_in_generation: row.get("removed_in_generation")?,
                    had_callers: row.get::<_, i64>("had_callers")? != 0,
                })
            },
        )
    }

    fn entry_points(&self, limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        collect(
            self.conn.prepared(ENTRY_POINTS_KEY, |connection| {
                connection.prepare_cached(ENTRY_POINTS_SQL)
            })?,
            params![limit_param(limit)],
            ENTRY_POINTS_KEY,
            EntryPointRow::from_row,
        )
    }

    fn hotspots(&self, limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        collect(
            self.conn.prepared(HOTSPOTS_KEY, |connection| {
                connection.prepare_cached(HOTSPOTS_SQL)
            })?,
            params![limit_param(limit)],
            HOTSPOTS_KEY,
            HotspotRow::from_row,
        )
    }

    fn test_gaps(&self, ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        // An empty frontier is answered without a statement: `IN ()` is not SQL, and the answer is
        // knowable before the query — which is also what keeps the cache counters still.
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            concat!(
                "SELECT {}, m.tested_depth AS tested_depth FROM node_metrics m",
                " JOIN node n ON n.id = m.node_id JOIN file f ON f.id = n.file_id",
                " WHERE n.id IN ({})",
                " AND (m.tested_depth IS NULL OR m.tested_depth > ?)",
                " ORDER BY (m.tested_depth IS NULL) DESC, m.tested_depth DESC, n.id ASC"
            ),
            ranked_node_select!(),
            placeholders(ids.len())
        );
        let mut bound: Vec<i64> = Vec::with_capacity(ids.len() + 1);
        bound.extend_from_slice(ids);
        bound.push(TEST_GAP_MAX_DEPTH);
        collect(
            self.conn
                .prepared(TEST_GAPS_KEY, |connection| connection.prepare_cached(&sql))?,
            params_from_iter(bound),
            TEST_GAPS_KEY,
            TestGapRow::from_row,
        )
    }

    fn read_node_fts(&self, match_query: &str, limit: usize) -> Result<Vec<FtsHit>, AppError> {
        // `node_fts` is a virtual table, so no typed query builder can model it and the statement
        // stays raw. `bm25()` and `MATCH` are the two things only this module may run: the ranking is
        // part of the contract, and a caller that re-sorted would break the cursor's stable ordering.
        collect(
            self.conn.prepared(NODE_FTS_KEY, |connection| {
                connection.prepare_cached(NODE_FTS_SQL)
            })?,
            params![match_query, limit_param(limit)],
            NODE_FTS_KEY,
            FtsHit::from_row,
        )
    }
}

impl SqliteReader {}

#[cfg(test)]
mod tests {
    use super::{TEST_GAP_MAX_DEPTH, failure_count, meta_integer, read_meta_row};

    fn rows(pairs: &[(&str, &str)]) -> Vec<super::MetaSqlRow> {
        pairs
            .iter()
            .map(|(key, value)| super::MetaSqlRow {
                key: (*key).to_owned(),
                value: (*value).to_owned(),
            })
            .collect()
    }

    #[test]
    fn an_absent_failure_counter_is_zero_and_a_written_one_is_its_value() {
        assert_eq!(failure_count(None).expect("absent"), 0);
        assert_eq!(failure_count(Some("3".to_owned())).expect("three"), 3);
        assert_eq!(failure_count(Some(" 3 ".to_owned())).expect("trimmed"), 3);
    }

    #[test]
    fn an_unreadable_failure_counter_raises_instead_of_reading_as_zero() {
        for raw in ["three", "-1", "", "1.5"] {
            assert!(
                failure_count(Some(raw.to_owned())).is_err(),
                "{raw:?} is a writer bug, not a measurement of zero"
            );
        }
    }

    #[test]
    fn a_missing_or_empty_meta_value_reads_as_absent() {
        let values = rows(&[("project_name", "sangkar"), ("project_branch", "")]);
        assert_eq!(
            super::meta_text(&values, "project_name").as_deref(),
            Some("sangkar")
        );
        assert_eq!(super::meta_text(&values, "project_branch"), None);
        assert_eq!(super::meta_text(&values, "project_remote"), None);
        assert_eq!(
            super::meta_raw(&values, "project_branch").as_deref(),
            Some("")
        );
        assert_eq!(meta_integer(&values, "embed_dim"), None);
    }

    #[test]
    fn the_gap_threshold_is_the_documented_two_hops() {
        assert_eq!(TEST_GAP_MAX_DEPTH, 2);
    }

    #[test]
    fn a_meta_row_is_read_by_column_name() {
        let connection = rusqlite::Connection::open_in_memory().expect("open a memory database");
        connection
            .execute_batch("CREATE TABLE meta (key text PRIMARY KEY NOT NULL, value text NOT NULL)")
            .expect("create meta");
        connection
            .execute("INSERT INTO meta (key, value) VALUES ('k', 'v')", [])
            .expect("insert a row");
        let row = connection
            .query_row(
                "SELECT m.key AS key, m.value AS value FROM meta m",
                [],
                read_meta_row,
            )
            .expect("read the row");
        assert_eq!((row.key.as_str(), row.value.as_str()), ("k", "v"));
    }
}
