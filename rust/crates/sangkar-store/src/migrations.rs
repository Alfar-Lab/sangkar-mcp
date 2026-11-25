//! The two SQL streams, embedded from this crate's own `migrations/` directory
//! (`TECH-SPEC.md` §5.4.4).
//!
//! The DDL is `drizzle-kit generate` output from `src/shared/store/schema.ts`, and the FTS5
//! objects beside it are hand-written there. Both `.sql` files live at the crate root
//! (`migrations/`, moved out of the TypeScript tree in TASK-061) and are embedded by relative
//! path instead of being re-written in Rust, so one `graph.db` stays readable by either
//! implementation for the whole migration and `bun run db:drift` keeps testing the same bytes.
//! Nothing here edits them.
//!
//! The runner itself is in [`crate::migrate`]; this module only names the streams, the ledger and
//! the two `meta` keys, and reads the version facts back out of a database.

use rusqlite::{Connection, OptionalExtension};

/// The modeled stream: the eleven tables of `schema.ts` and their index plan.
pub const INIT_SQL: &str = include_str!("../migrations/0000_init.sql");

/// The raw stream: the FTS5 table Drizzle cannot model.
pub const FTS_SQL: &str = include_str!("../migrations/raw/0001_fts.sql");

/// Every migration in application order: the modeled stream first, the raw stream after it,
/// because raw SQL may depend on a modeled table (`node_fts` needs `node_fts_state`).
pub const MIGRATIONS: [(&str, &str); 2] = [("0000_init", INIT_SQL), ("raw/0001_fts", FTS_SQL)];

/// Ledger table. Not modeled in `schema.ts` on purpose: it belongs to the runner, not the graph.
pub const LEDGER_TABLE: &str = "schema_migration";

/// `meta` keys the runner reads and writes (`DATABASE.md` §3.1, §10). The other eight live in
/// [`crate::schema`], which re-exports these two so that module names every key.
pub const META_SCHEMA_VERSION: &str = "schema_version";
pub const META_REBUILD_REQUIRED: &str = "rebuild_required";

/// The schema version this build produces: one ledger row per migration file.
pub const fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

/// What this build ships, pinned at compile time: a third `.sql` file that reaches the ledger
/// without a second entry in [`MIGRATIONS`] is a version every existing `graph.db` would have to
/// be rebuilt for, so it fails the build here instead of a database at runtime.
const _: () = assert!(
    schema_version() == 2,
    "two migrations ship: 0000_init and raw/0001_fts, so schema_version() must be 2"
);

/// One `meta` value, `None` when there is no `meta` table, no such key, or the value is not text.
///
/// The TypeScript side guards the missing table with `hasTable()` and then queries; both checks
/// collapse into the same `None` here, so every "unset" reads as absent rather than as an error.
fn read_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
        row.get::<_, String>(0)
    })
    .optional()
    .ok()
    .flatten()
}

/// `meta.schema_version` as an integer; `0` when the database has not been migrated.
///
/// A value that is not an integer is treated as `0` so the ledger, not a stray string, decides
/// what ran. Only the runner writes this key, and it writes integers.
pub fn read_schema_version(conn: &Connection) -> i64 {
    read_meta(conn, META_SCHEMA_VERSION)
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(0)
}

/// True when a migration asked for a full index instead of an incremental one.
pub fn read_rebuild_required(conn: &Connection) -> bool {
    read_meta(conn, META_REBUILD_REQUIRED).as_deref() == Some("1")
}
