//! The single migration runner: two embedded SQL streams, one ledger (`TECH-SPEC.md` §5.4.4).
//!
//! Ported from `src/shared/store/migrate.ts` (294 lines) statement for statement. The modeled
//! stream runs first, the raw FTS stream after it, one file = one transaction, the file's name
//! lands in `schema_migration`, and the count of files lands in `meta.schema_version`
//! (`DATABASE.md` §10). Forward-only: there are no down migrations, and a database written by a
//! newer build is refused, never downgraded.
//!
//! Deliberately not Drizzle's own migrator: it keeps a second ledger (`__drizzle_migrations` with
//! its own hash bookkeeping) and has nowhere to record a migration that cannot be expressed as
//! DDL. One graph store, one ledger.
//!
//! The SQL text is compiled in ([`crate::migrations`]), so this module needs no filesystem and no
//! migrations directory: it shapes the caller's connection and nothing else.

use std::collections::HashSet;

use rusqlite::Connection;
use sangkar_core::errors::{AppError, ErrorCode};

use crate::migrations::{
    LEDGER_TABLE, META_SCHEMA_VERSION, MIGRATIONS, read_rebuild_required, read_schema_version,
    schema_version,
};

/// The hint a newer database is refused with: the same sentence `src/shared/store/migrate.ts`
/// attaches, because a caller that reaches it must be told to upgrade rather than to delete.
const SCHEMA_TOO_NEW_HINT: &str = concat!(
    "upgrade sangkar, or point --data-dir at a fresh directory; ",
    "this build will not downgrade the graph"
);

/// `drizzle-kit generate` separates statements with this comment (journal `breakpoints: true`).
const STATEMENT_BREAKPOINT: &str = "--> statement-breakpoint";

/// The only two messages a half-applied migration may swallow, compared case-insensitively:
/// the class where SQLite refuses something that is already there — a `CREATE` collision
/// (`... already exists`) and a repeated `ALTER TABLE ... ADD COLUMN` (`duplicate column name`).
/// Everything else rolls the whole file back.
const IDEMPOTENT_ERROR: [&str; 2] = ["already exists", "duplicate column name"];

/// `meta.schema_version` upsert, the same text as `src/shared/store/migrate.ts`.
const META_UPSERT: &str = concat!(
    "INSERT INTO meta (key, value) VALUES (?, ?)\n",
    "     ON CONFLICT (key) DO UPDATE SET value = excluded.value"
);

/// What one `migrate()` call did, including the two facts the caller must act on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MigrateResult {
    /// Ledger names applied by this call, in order. Empty when the database was already current.
    pub applied: Vec<String>,
    /// `meta.schema_version` before this call; `0` when the key is unset.
    pub from_version: i64,
    /// `meta.schema_version` after this call: the number of committed migration files.
    pub schema_version: i64,
    /// `meta.rebuild_required` was `1`: the indexer must throw away the graph and index fully.
    pub rebuild_required: bool,
}

/// The `internal` failure for a file or the ledger that could not be applied: the file was rolled
/// back, nothing was recorded, and the next run retries it whole.
fn internal(message: impl Into<String>, hint: impl Into<String>) -> AppError {
    AppError::internal(message).with_hint(hint)
}

/// Brings `conn` to this build's schema and reports what changed.
///
/// Refuses with `schema_too_new` when the database was written by a newer build — a silent
/// downgrade would serve a graph the code cannot read — and with `internal` when the caller
/// already opened a transaction. Otherwise applies the pending files in order and returns
/// `rebuild_required` so the indexer can honour `meta.rebuild_required`.
pub fn migrate(conn: &mut Connection) -> Result<MigrateResult, AppError> {
    let built = schema_version();
    let from_version = read_schema_version(conn);

    if from_version > built {
        return Err(AppError::new(
            ErrorCode::SCHEMA_TOO_NEW,
            format!("database schema_version {from_version} is newer than this build ({built})"),
        )
        .with_hint(SCHEMA_TOO_NEW_HINT));
    }
    if !conn.is_autocommit() {
        return Err(internal(
            "migrate() must be called outside a transaction",
            "the runner opens one transaction per migration file",
        ));
    }

    ensure_ledger(conn)?;
    let recorded = ledger_names(conn)?;

    let mut applied = Vec::new();
    for (name, sql) in MIGRATIONS {
        if recorded.contains(name) {
            continue;
        }
        apply_file(conn, name, sql)?;
        applied.push(name.to_string());
    }

    // Only a version change touches `meta`: a re-run on a current database is a true no-op.
    if built != from_version {
        write_schema_version(conn, built)?;
    }

    Ok(MigrateResult {
        applied,
        from_version,
        schema_version: built,
        rebuild_required: read_rebuild_required(conn),
    })
}

/// The ledger DDL, byte-identical to `ensureLedger()` in `src/shared/store/migrate.ts` — the
/// column alignment included, because one `graph.db` is opened by both implementations.
fn ledger_ddl() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {LEDGER_TABLE} (\n\tname       TEXT PRIMARY KEY NOT NULL,\n\tapplied_at TEXT NOT NULL\n)"
    )
}

/// One row per file, written inside that file's transaction. `applied_at` comes from SQLite's own
/// clock, in the same ISO-8601-with-milliseconds form `new Date().toISOString()` produces (the
/// injected clock the TypeScript runner accepts exists for its tests, not for the store).
fn ledger_insert() -> String {
    format!(
        "INSERT INTO {LEDGER_TABLE} (name, applied_at) VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))"
    )
}

fn ensure_ledger(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(&ledger_ddl()).map_err(|error| {
        internal(
            format!("cannot create the {LEDGER_TABLE} ledger: {error}"),
            "the database file must be writable for a migration to run",
        )
    })
}

fn ledger_names(conn: &Connection) -> Result<HashSet<String>, AppError> {
    let sql = format!("SELECT name FROM {LEDGER_TABLE}");
    let names: rusqlite::Result<Vec<String>> = (|| {
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([], |row| row.get(0))?;
        rows.collect()
    })();
    names
        .map(|names| names.into_iter().collect())
        .map_err(|error| {
            internal(
                format!("cannot read the {LEDGER_TABLE} ledger: {error}"),
                "the database file may not be a sangkar graph",
            )
        })
}

/// Applies one file in its own `BEGIN IMMEDIATE` transaction and records it under the same
/// transaction. Statement-level failures that only say "it is already there" are skipped, which
/// is what makes a partially applied migration re-runnable; any other failure rolls the whole
/// file back and is reported with its name, so nothing is recorded and the next run retries it
/// whole.
fn apply_file(conn: &Connection, name: &str, sql: &str) -> Result<(), AppError> {
    let statements = split_statements(sql);
    conn.execute_batch("BEGIN IMMEDIATE").map_err(|error| {
        internal(
            format!("cannot begin the transaction for {name}: {error}"),
            "the database file may be read-only or locked by another writer",
        )
    })?;

    let outcome: rusqlite::Result<()> = (|| {
        for statement in &statements {
            // A statement that only says it is already there is swallowed on its own, so a
            // half-applied file still completes; anything else aborts the file. The TypeScript
            // runner logs the skipped statement at debug, which arrives with `sangkar-core::log`.
            if let Err(error) = conn.execute_batch(statement)
                && !is_idempotent_failure(&error)
            {
                return Err(error);
            }
        }
        conn.execute(&ledger_insert(), [name])?;
        conn.execute_batch("COMMIT")
    })();

    match outcome {
        Ok(()) => Ok(()),
        Err(error) => {
            // The COMMIT is inside the same closure, so a failing commit lands here too.
            let _ = conn.execute_batch("ROLLBACK");
            Err(failed(name, &error))
        }
    }
}

fn failed(name: &str, error: &rusqlite::Error) -> AppError {
    internal(
        format!("migration {name} failed: {error}"),
        format!("{name} was rolled back and not recorded; fix the SQL and retry"),
    )
}

/// Writes `meta.schema_version`, but only when the migration stream created a `meta` table: a
/// stream without one records its progress in the ledger alone.
fn write_schema_version(conn: &Connection, version: i64) -> Result<(), AppError> {
    if !has_table(conn, crate::schema::META)? {
        return Ok(());
    }
    conn.execute(
        META_UPSERT,
        rusqlite::params![META_SCHEMA_VERSION, version.to_string()],
    )
    .map(|_| ())
    .map_err(|error| {
        internal(
            format!("cannot record {META_SCHEMA_VERSION}: {error}"),
            "the database file must be writable for a migration to run",
        )
    })
}

/// `sqlite_master` lookup, the guard the TypeScript runner opens with before it touches `meta`.
fn has_table(conn: &Connection, name: &str) -> Result<bool, AppError> {
    conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get::<_, i64>(0),
    )
    .map(|count| count > 0)
    .map_err(|error| {
        internal(
            format!("cannot read sqlite_master: {error}"),
            "the database file may not be a sangkar graph",
        )
    })
}

/// Splits a file into executable statements. Drizzle's `--> statement-breakpoint` wins when
/// present; a hand-written file without breakpoints is split on `;` at a line end. Splitting is
/// what makes a single already-exists error survivable: only that statement is skipped.
fn split_statements(sql: &str) -> Vec<&str> {
    let chunks: Vec<&str> = if sql.contains(STATEMENT_BREAKPOINT) {
        sql.split(STATEMENT_BREAKPOINT).collect()
    } else {
        split_on_line_end_semicolons(sql)
    };
    chunks
        .into_iter()
        .map(str::trim)
        .filter(|chunk| carries_sql(chunk))
        .collect()
}

/// The `/;\s*(?:\r?\n|$)/` split of the TypeScript runner, by hand: a `;` breaks the file when
/// everything after it up to a line end — or up to the end of the file — is whitespace. A `;`
/// with code after it on the same line stays inside its statement.
fn split_on_line_end_semicolons(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b';' {
            let mut j = i + 1;
            let mut newline_end = None;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                if bytes[j] == b'\n' && newline_end.is_none() {
                    newline_end = Some(j + 1);
                }
                j += 1;
            }
            let boundary_end = if j == bytes.len() {
                Some(bytes.len())
            } else {
                newline_end
            };
            if let Some(end) = boundary_end {
                chunks.push(&sql[start..i]);
                start = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    chunks.push(&sql[start..]);
    chunks
}

/// A chunk that is only comments executes nothing and must not reach the driver.
fn carries_sql(chunk: &str) -> bool {
    chunk.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with("--")
    })
}

/// The two errors named by [`IDEMPOTENT_ERROR`] and nothing else, case-insensitively.
fn is_idempotent_failure(error: &rusqlite::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    IDEMPOTENT_ERROR
        .iter()
        .any(|needle| message.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::{FTS_SQL, INIT_SQL};

    #[test]
    fn splits_drizzle_breakpoints_and_drops_comment_only_chunks() {
        let sql = concat!(
            "CREATE TABLE a (id integer);\n--> statement-breakpoint\n",
            "-- only a comment\n--> statement-breakpoint\n",
            "CREATE TABLE b (id integer);"
        );
        assert_eq!(
            split_statements(sql),
            [
                "CREATE TABLE a (id integer);",
                "CREATE TABLE b (id integer);"
            ]
        );
    }

    #[test]
    fn splits_a_hand_written_file_on_line_end_semicolons_only() {
        let two = "CREATE TABLE a (id integer);\nCREATE TABLE b (id integer);\n";
        assert_eq!(split_statements(two).len(), 2);
        // A `;` followed by code on the same line belongs to its statement; the separator itself
        // is consumed, as `String.prototype.split` consumes it.
        let inline = "INSERT INTO t VALUES ('a;b');\n";
        assert_eq!(split_statements(inline), ["INSERT INTO t VALUES ('a;b')"]);
        // The final `;` may be followed by nothing but whitespace.
        assert_eq!(
            split_statements("CREATE TABLE a (id integer);   \n\n").len(),
            1
        );
    }

    #[test]
    fn the_embedded_streams_split_into_their_statements() {
        // Eleven tables plus nineteen indexes.
        assert_eq!(split_statements(INIT_SQL).len(), 30);
        // A comment header and one CREATE VIRTUAL TABLE, no breakpoints in the file.
        assert_eq!(split_statements(FTS_SQL).len(), 1);
        assert!(split_statements(FTS_SQL)[0].contains("USING fts5("));
    }

    fn failing(conn: &Connection, sql: &str) -> rusqlite::Error {
        conn.execute_batch(sql)
            .expect_err("the statement is expected to fail")
    }

    #[test]
    fn only_already_exists_and_duplicate_column_name_are_swallowed() {
        let conn = Connection::open_in_memory().expect("in-memory database");
        conn.execute_batch("CREATE TABLE t (id integer)")
            .expect("first create");
        assert!(is_idempotent_failure(&failing(
            &conn,
            "CREATE TABLE t (id integer)"
        )));
        conn.execute_batch("ALTER TABLE t ADD COLUMN a integer")
            .expect("first add");
        assert!(is_idempotent_failure(&failing(
            &conn,
            "ALTER TABLE t ADD COLUMN a integer"
        )));
        // Everything else is a real failure: a syntax error, a missing table, a broken index.
        // (`CREATE TABLE t (` is *not* a syntax error here: SQLite reports "already exists"
        // before it parses, which the first assertion above already covers.)
        assert!(!is_idempotent_failure(&failing(&conn, "CREATE TABLE u (")));
        assert!(!is_idempotent_failure(&failing(
            &conn,
            "INSERT INTO missing VALUES (1)"
        )));
        assert!(!is_idempotent_failure(&failing(
            &conn,
            "CREATE UNIQUE INDEX i ON t (id) WHERE"
        )));
    }
}
