//! The runner's contract on a real database (`TECH-SPEC.md` §5.4.4, check 2), ported from the
//! runner half of `tests/schema-parity.test.ts`.
//!
//! One empty database file is migrated by [`migrate`] and then interrogated through `rusqlite`
//! only — the `sqlite3` CLI is not a dependency of this repository. The TypeScript suite compares
//! the live schema against Drizzle's `getTableConfig()`; with no typed model on this side, the
//! equivalent check is [`sangkar_store::schema::TABLES`] against `sqlite_master`, plus the FTS5
//! object the raw stream creates.
//!
//! The suite also pins the runner's own contract: one ledger with one row per file, a no-op
//! re-run, the `schema_too_new` refusal, the nested-transaction refusal, the `rebuild_required`
//! flag, and a migration that half-applied being re-runnable.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use sangkar_core::errors::ErrorCode;
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::{
    LEDGER_TABLE, META_REBUILD_REQUIRED, META_SCHEMA_VERSION, MIGRATIONS, read_rebuild_required,
    read_schema_version, schema_version,
};
use sangkar_store::schema::{NODE_FTS, NODE_FTS_STATE, TABLES};

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
            "sangkar-store-{label}-{}-{nanos}-{}",
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

/// The ledger names of `MIGRATIONS`, in application order: what a fresh database must record.
fn expected_applied() -> Vec<String> {
    MIGRATIONS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}

fn query_column(conn: &Connection, sql: &str) -> Vec<String> {
    let mut statement = conn.prepare(sql).expect("prepare the query");
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("run the query");
    rows.map(|row| row.expect("read a row")).collect()
}

fn ledger_names(conn: &Connection) -> Vec<String> {
    query_column(
        conn,
        &format!("SELECT name FROM {LEDGER_TABLE} ORDER BY rowid"),
    )
}

fn meta_text(conn: &Connection, key: &str) -> Option<String> {
    query_column(conn, &format!("SELECT value FROM meta WHERE key = '{key}'"))
        .into_iter()
        .next()
}

fn live_tables(conn: &Connection) -> Vec<String> {
    query_column(
        conn,
        "SELECT name FROM sqlite_master WHERE type IN ('table','view') AND name NOT LIKE 'sqlite_%'",
    )
}

fn open_migrated_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open an in-memory database");
    migrate(&mut conn).expect("migrate an empty database");
    conn
}

#[test]
fn migrates_an_empty_database_file() {
    let dir = TempDir::new("empty");
    let path = dir.join("graph.db");
    assert!(!path.exists(), "the file starts out absent, not empty");

    let mut conn = Connection::open(&path).expect("open the new database file");

    // A database that has never been migrated: no `meta` table, so both keys read as unset.
    assert_eq!(read_schema_version(&conn), 0);
    assert!(!read_rebuild_required(&conn));

    let result = migrate(&mut conn).expect("migrate the empty file");

    assert_eq!(result.from_version, 0);
    assert_eq!(result.applied, expected_applied());
    assert_eq!(result.schema_version, schema_version());
    assert_eq!(
        result.schema_version, 2,
        "one version per embedded migration"
    );
    assert!(!result.rebuild_required);
    assert_eq!(read_schema_version(&conn), 2);
    assert_eq!(meta_text(&conn, META_SCHEMA_VERSION).as_deref(), Some("2"));
    assert!(path.exists(), "the migration ran against a real file");
}

#[test]
fn records_one_ledger_row_per_file_with_an_iso_timestamp() {
    let mut conn = Connection::open_in_memory().expect("open an in-memory database");
    migrate(&mut conn).expect("migrate");

    assert_eq!(ledger_names(&conn), expected_applied());

    let applied_at = query_column(
        &conn,
        &format!("SELECT applied_at FROM {LEDGER_TABLE} ORDER BY rowid"),
    );
    assert_eq!(applied_at.len(), MIGRATIONS.len());
    for stamp in &applied_at {
        // `new Date().toISOString()`: 2026-11-24T10:37:00.123Z.
        assert_eq!(stamp.len(), 24, "not an ISO-8601 instant: {stamp}");
        assert_eq!(&stamp[4..5], "-", "not an ISO-8601 date: {stamp}");
        assert_eq!(&stamp[10..11], "T", "not an ISO-8601 instant: {stamp}");
        assert_eq!(&stamp[19..20], ".", "missing milliseconds: {stamp}");
        assert!(stamp.ends_with('Z'), "not UTC: {stamp}");
    }
}

#[test]
fn a_second_migrate_is_a_no_op() {
    let mut conn = open_migrated_in_memory();
    let version_before = meta_text(&conn, META_SCHEMA_VERSION);
    let ledger_before = ledger_names(&conn);

    let second = migrate(&mut conn).expect("a second migrate succeeds");

    assert!(second.applied.is_empty(), "applied: {:?}", second.applied);
    assert_eq!(second.from_version, second.schema_version);
    assert_eq!(second.from_version, 2);
    assert_eq!(ledger_names(&conn), ledger_before);
    assert_eq!(
        meta_text(&conn, META_SCHEMA_VERSION),
        version_before,
        "a re-run on a current database must not touch meta"
    );
}

#[test]
fn the_eleven_modeled_tables_and_the_fts_objects_exist() {
    let conn = open_migrated_in_memory();
    let tables = live_tables(&conn);

    for table in TABLES {
        assert!(
            tables.iter().any(|live| live == table),
            "{table} is missing; live: {tables:?}"
        );
    }
    assert_eq!(TABLES.len(), 11);
    assert!(
        tables.iter().any(|live| live == NODE_FTS),
        "node_fts is missing"
    );
    assert!(
        tables.iter().any(|live| live == NODE_FTS_STATE),
        "node_fts_state is missing"
    );

    // `node_fts` is FTS5 external content over the mirror, not a second copy of the corpus: an
    // external-content table has no `node_fts_content` shadow table.
    let ddl = query_column(
        &conn,
        "SELECT sql FROM sqlite_master WHERE name = 'node_fts'",
    );
    let ddl = ddl.first().expect("node_fts has DDL in sqlite_master");
    assert!(ddl.contains("USING fts5("), "{ddl}");
    assert!(ddl.contains("content = 'node_fts_state'"), "{ddl}");
    assert!(ddl.contains("remove_diacritics 2"), "{ddl}");
    assert!(
        !tables.iter().any(|live| live == "node_fts_content"),
        "node_fts stopped being external content"
    );

    // Nothing outside the modeled eleven, the runner's ledger, and the FTS5 objects: a stray
    // table would mean the embedded SQL and this list disagree.
    let unexpected: Vec<&String> = tables
        .iter()
        .filter(|live| {
            !TABLES.contains(&live.as_str())
                && live.as_str() != LEDGER_TABLE
                && !live.starts_with("node_fts")
        })
        .collect();
    assert!(unexpected.is_empty(), "unexpected tables: {unexpected:?}");
}

#[test]
fn a_partially_applied_migration_is_re_runnable() {
    let mut conn = Connection::open_in_memory().expect("open an in-memory database");
    // The modeled SQL landed without a ledger row: a hand-applied file, an older tool, or a
    // restored backup. `migrate` must skip the statements that already exist, not fail on them.
    conn.execute_batch(sangkar_store::migrations::INIT_SQL)
        .expect("pre-apply the modeled stream without recording it");
    assert_eq!(read_schema_version(&conn), 0);

    let result = migrate(&mut conn).expect("a half-applied stream is re-runnable");

    assert_eq!(result.applied, expected_applied());
    assert_eq!(read_schema_version(&conn), 2);
    assert!(
        live_tables(&conn).iter().any(|live| live == NODE_FTS),
        "the raw stream still ran after every modeled statement was skipped"
    );
}

#[test]
fn refuses_a_newer_database_with_schema_too_new() {
    let mut conn = open_migrated_in_memory();
    let newer = schema_version() + 1;
    conn.execute(
        "UPDATE meta SET value = ?1 WHERE key = ?2",
        [newer.to_string(), META_SCHEMA_VERSION.to_string()],
    )
    .expect("bump the recorded version");

    let error = migrate(&mut conn).expect_err("a newer database is refused");

    assert_eq!(error.code, ErrorCode::SCHEMA_TOO_NEW);
    assert_eq!(error.code, "schema_too_new");
    assert_eq!(
        error.message,
        format!(
            "database schema_version {newer} is newer than this build ({})",
            schema_version()
        )
    );
    // The hint is the TypeScript one, verbatim: the caller must be told to upgrade, not to delete.
    assert_eq!(
        error.hint.as_deref(),
        Some(concat!(
            "upgrade sangkar, or point --data-dir at a fresh directory; ",
            "this build will not downgrade the graph"
        ))
    );
    // Nothing ran: the refusal happens before the first file.
    assert_eq!(read_schema_version(&conn), newer);
    assert_eq!(ledger_names(&conn), expected_applied());
}

#[test]
fn reports_meta_rebuild_required() {
    let mut conn = open_migrated_in_memory();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, '1')\n     ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        [META_REBUILD_REQUIRED],
    )
    .expect("raise the flag");

    let flagged = migrate(&mut conn).expect("migrate with the flag raised");
    assert!(flagged.rebuild_required);
    assert!(flagged.applied.is_empty());

    conn.execute(
        "UPDATE meta SET value = '0' WHERE key = ?1",
        [META_REBUILD_REQUIRED],
    )
    .expect("clear the flag");
    assert!(
        !migrate(&mut conn)
            .expect("migrate with the flag cleared")
            .rebuild_required
    );
}

#[test]
fn refuses_to_run_inside_a_transaction() {
    let mut conn = open_migrated_in_memory();
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("open a transaction");

    let error = migrate(&mut conn).expect_err("a nested transaction is refused");

    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert_eq!(
        error.message,
        "migrate() must be called outside a transaction"
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("the runner opens one transaction per migration file")
    );
    conn.execute_batch("ROLLBACK")
        .expect("close the transaction");
}

/// The root of this repository, four levels above the crate's manifest.
fn repo_root() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("the repository root above rust/crates/sangkar-store")
        .to_path_buf();
    assert!(
        root.join("AGENTS.md").is_file(),
        "expected the repository root, got {}",
        root.display()
    );
    root
}

fn with_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// One explicit notice for a case this machine cannot test, printed rather than swallowed: a
/// silently passing test would claim parity it never checked.
fn skip(reason: &str) {
    println!("SKIP migrates_a_copy_of_a_typescript_built_database: {reason}");
}

#[test]
fn migrates_a_copy_of_a_typescript_built_database() {
    let live = repo_root().join(".sangkar").join("graph.db");
    if !live.is_file() {
        skip(&format!(
            "{} does not exist; no TypeScript index was built on this machine, so \
             cross-implementation compatibility is unverified here",
            live.display()
        ));
        return;
    }

    // The live database is never opened or written: the runner gets a copy, sidecars included.
    let dir = TempDir::new("typescript-graph");
    let copy = dir.join("graph.db");
    fs::copy(&live, &copy).expect("copy the TypeScript-built graph.db");
    for sidecar in ["-wal", "-shm"] {
        let from = with_sidecar(&live, sidecar);
        if from.is_file() {
            fs::copy(&from, with_sidecar(&copy, sidecar)).expect("copy a journal sidecar");
        }
    }

    // The live file is shared with whoever is indexing this repository, so a copy can be torn
    // mid-write. That is a snapshot problem, not a runner defect, and it is reported as such.
    let conn = match Connection::open(&copy) {
        Ok(conn) => conn,
        Err(error) => {
            skip(&format!("the copied database would not open ({error})"));
            return;
        }
    };
    let integrity: String = match conn.query_row("PRAGMA quick_check(1)", [], |row| row.get(0)) {
        Ok(check) => check,
        Err(error) => {
            skip(&format!("the copied database failed quick_check ({error})"));
            return;
        }
    };
    if integrity != "ok" {
        skip(&format!(
            "the copy is a torn snapshot ({integrity}); the live graph.db was being written while \
             it was copied"
        ));
        return;
    }
    let mut conn = conn;
    let from_version = read_schema_version(&conn);
    assert_eq!(
        from_version,
        schema_version(),
        "the TypeScript database records a different schema version"
    );

    let result = migrate(&mut conn).expect("migrate the TypeScript-built database");

    assert!(
        result.applied.is_empty(),
        "the copy is already current, so nothing may run: {:?}",
        result.applied
    );
    assert_eq!(result.from_version, from_version);
    assert_eq!(result.schema_version, 2);
    assert_eq!(read_schema_version(&conn), 2);

    // The graph itself is untouched by the runner, and the FTS table it will query is there.
    let nodes: i64 = conn
        .query_row("SELECT count(*) FROM node", [], |row| row.get(0))
        .expect("count the nodes");
    assert!(nodes > 0, "the copied graph should hold nodes");
    let tables = live_tables(&conn);
    assert!(
        tables.iter().any(|live| live == NODE_FTS),
        "node_fts is missing"
    );
    for table in TABLES {
        assert!(
            tables.iter().any(|live| live == table),
            "{table} is missing"
        );
    }
}
