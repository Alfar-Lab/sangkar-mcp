//! What the typed Drizzle model used to provide, as constants: the eleven table names and the ten
//! `meta` keys (`DATABASE.md` §3).
//!
//! [`TABLES`] is the list `tests/migrate.rs` compares against `sqlite_master` after a migration —
//! the Rust replacement for the table half of `tests/schema-parity.test.ts`. It is a list of
//! names, not a second copy of the DDL: the DDL stays `drizzle-kit` output and is only ever
//! embedded from `src/shared/store/migrations/`.

/// Identity, schema version and runtime state; the only place a schema version lives (§3.1).
pub const META: &str = "meta";
/// Publish history; the current generation is `meta.generation` (equivalently `MAX(seq)`).
pub const GENERATION: &str = "generation";
/// Every file the indexer parsed; also the staleness manifest via `sha256`.
pub const FILE: &str = "file";
/// Every path the indexer looked at and did not index, so "no callers in X" is answerable.
pub const NOT_INDEXED: &str = "not_indexed";
/// Call sites whose target did not bind; stored rather than dropped, for absence semantics.
pub const UNRESOLVED: &str = "unresolved";
/// Nodes of the symbol graph. `qn` is repo-relative and machine independent.
pub const NODE: &str = "node";
/// Resolved edges only, id-keyed.
pub const EDGE: &str = "edge";
/// Derived per-node values, computed once at publish so the query path never recomputes them.
pub const NODE_METRICS: &str = "node_metrics";
/// Removed symbols, so `summary(change)` can report them without re-parsing the base commit.
pub const TOMBSTONE: &str = "tombstone";
/// Mirror for the external-content FTS5 table: the indexed values, kept under the node id.
pub const NODE_FTS_STATE: &str = "node_fts_state";
/// Optional embeddings. Created but empty in the MVP.
pub const NODE_VECTOR: &str = "node_vector";

/// The FTS5 virtual table of the raw stream. Not in [`TABLES`]: `drizzle-kit` cannot model a
/// virtual table, so it is created by `raw/0001_fts.sql` and is external content over
/// [`NODE_FTS_STATE`].
pub const NODE_FTS: &str = "node_fts";

/// Every table `schema.ts` models, in the order that file declares them.
pub const TABLES: [&str; 11] = [
    META,
    GENERATION,
    FILE,
    NOT_INDEXED,
    UNRESOLVED,
    NODE,
    EDGE,
    NODE_METRICS,
    TOMBSTONE,
    NODE_FTS_STATE,
    NODE_VECTOR,
];

/// The two keys the runner owns are defined in [`crate::migrations`], which reads and writes
/// them; they are re-exported so every `meta` key is reachable from this module.
pub use crate::migrations::{META_REBUILD_REQUIRED, META_SCHEMA_VERSION};

/// `meta.generation`: the id of the generation the read path serves.
pub const META_GENERATION: &str = "generation";
/// `meta.generation_seq`: the publish counter, fixed at publish.
pub const META_GENERATION_SEQ: &str = "generation_seq";
/// `meta.project_name`: display identity, recorded once at first index.
pub const META_PROJECT_NAME: &str = "project_name";
/// `meta.project_root`: the guard against answering about a different tree.
pub const META_PROJECT_ROOT: &str = "project_root";
/// `meta.project_branch`: the branch `HEAD` named at publish.
pub const META_PROJECT_BRANCH: &str = "project_branch";
/// `meta.project_remote`: `remote.origin.url` at publish.
pub const META_PROJECT_REMOTE: &str = "project_remote";
/// `meta.indexer_state`: `idle` | `running` | `failed`, as the indexer last wrote it.
pub const META_INDEXER_STATE: &str = "indexer_state";
/// `meta.consecutive_failures`: failed runs since the last publish.
pub const META_CONSECUTIVE_FAILURES: &str = "consecutive_failures";

/// Every `meta` key the store reads or writes, in the order `DATABASE.md` §3.1 lists them.
pub const META_KEYS: [&str; 10] = [
    META_SCHEMA_VERSION,
    META_GENERATION,
    META_GENERATION_SEQ,
    META_INDEXER_STATE,
    META_REBUILD_REQUIRED,
    META_CONSECUTIVE_FAILURES,
    META_PROJECT_NAME,
    META_PROJECT_ROOT,
    META_PROJECT_BRANCH,
    META_PROJECT_REMOTE,
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn the_eleven_modeled_table_names_are_distinct_and_counted() {
        assert_eq!(TABLES.len(), 11);
        assert_eq!(TABLES.iter().copied().collect::<HashSet<_>>().len(), 11);
        // The virtual table is deliberately outside the list `schema.ts` models.
        assert!(!TABLES.contains(&NODE_FTS));
    }

    #[test]
    fn the_ten_meta_keys_are_distinct_and_include_the_runner_pair() {
        assert_eq!(META_KEYS.len(), 10);
        assert_eq!(META_KEYS.iter().copied().collect::<HashSet<_>>().len(), 10);
        assert!(META_KEYS.contains(&META_SCHEMA_VERSION));
        assert!(META_KEYS.contains(&META_REBUILD_REQUIRED));
        assert_eq!(META_SCHEMA_VERSION, "schema_version");
        assert_eq!(META_REBUILD_REQUIRED, "rebuild_required");
    }
}
