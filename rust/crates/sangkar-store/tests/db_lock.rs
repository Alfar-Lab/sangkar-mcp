//! The connection contract on real files (`DATABASE.md` §3.7) and the single-writer lock
//! (`DATABASE.md` §4), ported from the PRAGMA half of `tests/schema-parity.test.ts` and from
//! `tests/pidlock.test.ts`'s lock cases.
//!
//! Every PRAGMA is asserted by reading it back through `rusqlite` after the handle was opened: the
//! read-back is the contract, because a silently ignored PRAGMA is how a store ends up with foreign
//! keys off or a 2 MB page cache. The lock cases cover the body's exact bytes, a contended acquire,
//! the two reclaim rules (a gone pid and an old mtime), the tear a dead holder leaves behind, and
//! the release that must refuse to unlink somebody else's lock.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sangkar_core::errors::ErrorCode;
use sangkar_store::db::{Conn, ConnMode, open_reader, open_writer, statement_cache_stats};
use sangkar_store::lock::{STALE_AFTER_MS, acquire_lock, heartbeat, inspect, lock_path, release};

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
            "sangkar-store-db-lock-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
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

fn pragma_i64(conn: &Conn, name: &str) -> i64 {
    conn.connection()
        .pragma_query_value(None, name, |row| row.get::<_, i64>(0))
        .unwrap_or_else(|error| panic!("PRAGMA {name} did not read back: {error}"))
}

fn pragma_text(conn: &Conn, name: &str) -> String {
    conn.connection()
        .pragma_query_value(None, name, |row| row.get::<_, String>(0))
        .unwrap_or_else(|error| panic!("PRAGMA {name} did not read back: {error}"))
}

/// The writer's set, in the order `db.ts` applies it, each value read back from the live handle.
/// The two the task names — `journal_size_limit` and `synchronous` — are asserted here by value.
#[test]
fn a_writer_applies_its_pragmas_and_reads_every_one_back() {
    let dir = TempDir::new("writer-pragmas");
    let path = dir.join("graph.db");
    let conn = open_writer(&path).expect("open the writer");

    assert_eq!(conn.mode(), ConnMode::Writer);
    assert_eq!(conn.path(), path.as_path());
    assert!(path.exists(), "the writer creates the database file");

    assert_eq!(pragma_text(&conn, "journal_mode"), "wal");
    assert_eq!(pragma_i64(&conn, "synchronous"), 1, "PRAGMA synchronous");
    assert_eq!(
        pragma_i64(&conn, "journal_size_limit"),
        67_108_864,
        "PRAGMA journal_size_limit"
    );
    assert_eq!(pragma_i64(&conn, "foreign_keys"), 1);
    assert_eq!(pragma_i64(&conn, "busy_timeout"), 10_000);
    assert_eq!(pragma_i64(&conn, "cache_size"), -65_536);
    // The OS may refuse the mmap ceiling in full, which `db.rs` warns about rather than fails on;
    // on this target the requested window is what comes back.
    assert_eq!(pragma_i64(&conn, "mmap_size"), 268_435_456);
}

#[test]
fn a_reader_applies_its_own_pragmas_and_refuses_a_write() {
    let dir = TempDir::new("reader-pragmas");
    let path = dir.join("graph.db");

    let writer = open_writer(&path).expect("open the writer");
    writer
        .connection()
        .execute_batch("CREATE TABLE probe (id integer);")
        .expect("create a table");
    writer.close().expect("close the writer");

    let reader = open_reader(&path).expect("open the reader");
    assert_eq!(reader.mode(), ConnMode::Reader);

    assert_eq!(pragma_i64(&reader, "foreign_keys"), 1);
    assert_eq!(pragma_i64(&reader, "busy_timeout"), 10_000);
    assert_eq!(pragma_i64(&reader, "cache_size"), -16_384);
    assert_eq!(pragma_i64(&reader, "mmap_size"), 268_435_456);
    // WAL is a property of the file, so a reader sees it without asking for it.
    assert_eq!(pragma_text(&reader, "journal_mode"), "wal");

    let refused = reader
        .connection()
        .execute("INSERT INTO probe (id) VALUES (1)", []);
    assert!(
        refused.is_err(),
        "a SQLITE_OPEN_READ_ONLY handle refuses the write inside SQLite"
    );
}

#[test]
fn a_reader_refuses_a_missing_database_instead_of_creating_one() {
    let dir = TempDir::new("reader-missing");
    let path = dir.join("graph.db");

    let error = open_reader(&path).expect_err("a read-only open must not create the file");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        !path.exists(),
        "the failed open left no database behind, so a missing graph is an error, not empty"
    );
}

#[test]
fn a_uri_filename_is_refused_before_sqlite_sees_it() {
    let dir = TempDir::new("uri-filename");
    let database = dir.join("graph.db");
    let uri = format!("file:{}?immutable=1", database.display());

    for error in [
        open_reader(&uri).expect_err("a URI is not a path"),
        open_writer(&uri).expect_err("a URI is not a path"),
    ] {
        assert_eq!(error.code, ErrorCode::INTERNAL);
        assert!(
            error.message.contains("URI filename"),
            "the refusal names the URI: {}",
            error.message
        );
        assert_eq!(error.hint.as_deref(), Some("pass a plain filesystem path"));
    }
    assert!(!database.exists());
}

/// `journal_mode = WAL` on an in-memory database answers `"memory"`, which is the one refusal the
/// read-back exists for: the filesystem cannot hold a write-ahead log.
#[test]
fn a_filesystem_that_cannot_hold_a_wal_is_unsupported() {
    let error =
        open_writer(":memory:").expect_err("an in-memory database reports journal_mode memory");

    assert_eq!(
        error.code,
        ErrorCode::UNSUPPORTED_FILESYSTEM,
        "the failure is the filesystem's, not a generic one: {}",
        error.message
    );
    assert!(
        error.message.contains("journal_mode is memory, not wal"),
        "the refusal reports what SQLite answered: {}",
        error.message
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("WAL needs a writable local directory; point --data-dir at one"),
        "the hint names a writable filesystem rather than guessing"
    );
}

#[test]
fn the_cache_counts_hits_misses_and_evicts_exactly_the_oldest_key() {
    let dir = TempDir::new("statement-cache");
    let path = dir.join("graph.db");
    open_writer(&path)
        .expect("open the writer")
        .close()
        .expect("close it");
    let conn = open_reader(&path).expect("open the reader");

    // The key is `&'static str` so the cache never owns a second copy of a query's name; leaking
    // the probe keys is how a test names 201 of them without a 201-line table.
    let keys: Vec<&'static str> = (0..201)
        .map(|index| Box::leak(format!("reader.probe/{index}").into_boxed_str()) as &'static str)
        .collect();

    for (index, key) in keys.iter().enumerate() {
        // A distinct statement per key, so the 200 retained entries are 200 real statements and not
        // one `SELECT 1` reached through 200 names.
        let sql = format!("SELECT {index}");
        let mut statement = conn
            .prepared(key, |connection| connection.prepare_cached(&sql))
            .expect("prepare a probe");
        let value: i64 = statement
            .query_row([], |row| row.get(0))
            .expect("run a probe");
        assert_eq!(value, i64::try_from(index).expect("index fits"));
    }

    let stats = conn.statement_cache_stats();
    assert_eq!(stats.capacity, 200);
    assert_eq!(stats.misses, 201, "every distinct key is a miss");
    assert_eq!(stats.hits, 0);
    assert_eq!(
        stats.evictions, 1,
        "the 201st distinct key evicts exactly one"
    );
    assert_eq!(stats.size, 200);

    // The evicted entry is the FIRST key inserted: re-using it misses again and, by evicting the
    // next oldest, is the only observable form of "exactly the oldest key".
    let mut statement = conn
        .prepared(keys[0], |connection| connection.prepare_cached("SELECT 0"))
        .expect("prepare the evicted key");
    let _ = statement.query_row([], |row| row.get::<_, i64>(0));
    let stats = conn.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.evictions, stats.size),
        (0, 202, 2, 200),
        "the first key was the one evicted at the 201st insert"
    );

    // A key that survived is a hit, does not evict, and becomes the most recently used one.
    for key in [keys[2], keys[3]] {
        let sql = format!("SELECT {}", key.rsplit('/').next().expect("the index"));
        let mut statement = conn
            .prepared(key, |connection| connection.prepare_cached(&sql))
            .expect("prepare a retained key");
        let _ = statement.query_row([], |row| row.get::<_, i64>(0));
    }
    let stats = conn.statement_cache_stats();
    assert_eq!(
        (stats.hits, stats.misses, stats.evictions, stats.size),
        (2, 202, 2, 200),
        "a hit moves the key to the most-recently-used end and evicts nothing"
    );

    // The free function sums every live cache; the suite runs in parallel threads, so it is a lower
    // bound, which still proves this handle is in the registry.
    let total = statement_cache_stats();
    assert_eq!(total.capacity, 200);
    assert!(total.size >= 200, "this handle is counted: {total:?}");
    assert!(
        total.misses >= 202,
        "this handle's misses are counted: {total:?}"
    );
}

#[test]
fn acquire_writes_the_typescript_lock_body_and_inspect_reads_it_back() {
    let dir = TempDir::new("lock-acquire");
    let handle = acquire_lock(dir.path()).expect("take the lock");

    assert_eq!(handle.path(), lock_path(dir.path()).as_path());
    assert_eq!(handle.pid(), std::process::id());
    assert_iso8601(handle.started_at());

    // `JSON.stringify(handle)`: same keys, same order, no whitespace.
    let body = fs::read_to_string(handle.path()).expect("read the lock body");
    assert_eq!(
        body,
        format!(
            r#"{{"path":"{}","pid":{},"startedAt":"{}"}}"#,
            handle.path().display(),
            handle.pid(),
            handle.started_at()
        )
    );

    let state = inspect(dir.path()).expect("inspect the lock");
    assert!(state.held);
    assert_eq!(state.pid, Some(std::process::id()));
    assert!(!state.stale, "a fresh lock with a live pid is not stale");

    release(&handle).expect("release the lock");
    assert!(!handle.path().exists());
    assert!(
        !inspect(dir.path()).expect("inspect the released lock").held,
        "the released lock is gone, not held"
    );
}

#[test]
fn a_second_acquire_while_the_lock_is_held_is_index_locked() {
    let dir = TempDir::new("lock-contended");
    let handle = acquire_lock(dir.path()).expect("take the lock");

    let error = acquire_lock(dir.path()).expect_err("the lock is held by this live process");
    assert_eq!(error.code, ErrorCode::INDEX_LOCKED);
    assert!(error.retryable, "contention is retryable");
    assert_eq!(error.retry_after_ms, Some(1_000));
    assert!(
        error
            .message
            .contains(&format!("pid {}", std::process::id())),
        "the refusal names the holder: {}",
        error.message
    );
    assert_eq!(
        error.hint.as_deref(),
        Some("wait for the running indexer to finish, or delete the lock if you know it is dead")
    );

    release(&handle).expect("release the lock");
}

#[test]
fn a_lock_whose_recorded_pid_is_gone_is_reclaimed() {
    let dir = TempDir::new("lock-stale-pid");
    let dead = dead_pid();
    // A fresh body, so only the liveness rule can make it stale.
    write_lock_body(
        dir.path(),
        &format!(
            r#"{{"path":"{}","pid":{dead},"startedAt":"1970-01-01T00:00:00.000Z"}}"#,
            lock_path(dir.path()).display()
        ),
    );

    let state = inspect(dir.path()).expect("inspect the lock");
    assert!(state.held);
    assert_eq!(state.pid, Some(dead));
    assert!(
        state.stale,
        "a pid with no /proc entry is stale whatever the mtime says"
    );

    let handle = acquire_lock(dir.path()).expect("reclaim the stale lock");
    assert_eq!(
        inspect(dir.path()).expect("inspect the lock").pid,
        Some(std::process::id())
    );
    release(&handle).expect("release the lock");
}

#[test]
fn a_lock_older_than_the_window_is_reclaimed_even_for_a_live_pid() {
    let dir = TempDir::new("lock-stale-age");
    write_lock_body(
        dir.path(),
        &format!(
            r#"{{"path":"{}","pid":{},"startedAt":"1970-01-01T00:00:00.000Z"}}"#,
            lock_path(dir.path()).display(),
            std::process::id()
        ),
    );
    age_lock_file(dir.path(), STALE_AFTER_MS / 1_000 + 60);

    let state = inspect(dir.path()).expect("inspect the lock");
    assert!(
        state.stale,
        "the heartbeat is the clock, so an old mtime expires a live pid"
    );
    assert_eq!(state.pid, Some(std::process::id()));

    let handle = acquire_lock(dir.path()).expect("reclaim the expired lock");
    release(&handle).expect("release the lock");
}

#[test]
fn a_torn_lock_body_is_reclaimed_only_by_age() {
    let dir = TempDir::new("lock-torn");
    // What a holder that died between the create and the write leaves behind.
    write_lock_body(dir.path(), r#"{"path":"/x","pid":"#);

    let state = inspect(dir.path()).expect("inspect a torn body");
    assert!(state.held);
    assert_eq!(state.pid, None, "an unparsable body records no pid");
    assert!(
        !state.stale,
        "a fresh torn body is the microseconds-long window of a healthy acquire"
    );

    age_lock_file(dir.path(), STALE_AFTER_MS / 1_000 + 60);
    assert!(inspect(dir.path()).expect("inspect the torn body").stale);

    let handle = acquire_lock(dir.path()).expect("reclaim the torn body");
    release(&handle).expect("release the lock");
}

#[test]
fn release_refuses_a_lock_recorded_by_another_pid() {
    let dir = TempDir::new("lock-release");
    let handle = acquire_lock(dir.path()).expect("take the lock");

    write_lock_body(
        dir.path(),
        &format!(
            r#"{{"path":"{}","pid":1,"startedAt":"1970-01-01T00:00:00.000Z"}}"#,
            lock_path(dir.path()).display()
        ),
    );
    release(&handle).expect("releasing a lock this handle does not own is a no-op");
    assert!(
        handle.path().exists(),
        "removing it would give two writers the same repository"
    );

    write_lock_body(
        dir.path(),
        &format!(
            r#"{{"path":"{}","pid":{},"startedAt":"{}"}}"#,
            lock_path(dir.path()).display(),
            handle.pid(),
            handle.started_at()
        ),
    );
    release(&handle).expect("release our own lock");
    assert!(!handle.path().exists());
}

#[test]
fn heartbeat_touches_the_mtime_that_is_the_liveness_clock() {
    let dir = TempDir::new("lock-heartbeat");
    let handle = acquire_lock(dir.path()).expect("take the lock");

    age_lock_file(dir.path(), 300);
    let aged = fs::metadata(handle.path())
        .expect("stat the lock")
        .modified()
        .expect("read the mtime");

    heartbeat(&handle).expect("heartbeat");
    let touched = fs::metadata(handle.path())
        .expect("stat the lock")
        .modified()
        .expect("read the mtime");
    assert!(touched > aged, "the heartbeat moved the mtime forward");
    assert!(!inspect(dir.path()).expect("inspect the lock").stale);

    fs::remove_file(handle.path()).expect("remove the lock behind the handle");
    heartbeat(&handle).expect("a lock that vanished is a warning, not an error");
}

fn write_lock_body(data_dir: &Path, body: &str) {
    fs::write(lock_path(data_dir), body).expect("write a lock body");
}

fn age_lock_file(data_dir: &Path, seconds: u64) {
    let path = lock_path(data_dir);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("open the lock to age it");
    let when = SystemTime::now() - Duration::from_secs(seconds);
    file.set_times(fs::FileTimes::new().set_modified(when))
        .expect("set the mtime");
}

/// A pid that has certainly exited: a child this test started and reaped. Measured, not guessed —
/// `1` would be wrong, because init is alive and `/proc/1` exists.
fn dead_pid() -> u32 {
    let mut child = Command::new("true").spawn().expect("spawn a child");
    let pid = child.id();
    child.wait().expect("reap the child");
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the reaped pid {pid} still has a /proc entry"
    );
    pid
}

/// `new Date().toISOString()` is 24 characters, and the shape is what the other implementation
/// parses: `YYYY-MM-DDTHH:MM:SS.sssZ`.
fn assert_iso8601(value: &str) {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 24, "startedAt is {value:?}");
    for index in [4, 7] {
        assert_eq!(bytes[index], b'-', "startedAt is {value:?}");
    }
    assert_eq!(bytes[10], b'T', "startedAt is {value:?}");
    for index in [13, 16] {
        assert_eq!(bytes[index], b':', "startedAt is {value:?}");
    }
    assert_eq!(bytes[19], b'.', "startedAt is {value:?}");
    assert_eq!(bytes[23], b'Z', "startedAt is {value:?}");
    for index in [0, 1, 2, 3, 5, 6, 8, 9, 11, 12, 14, 15, 17, 18, 20, 21, 22] {
        assert!(
            bytes[index].is_ascii_digit(),
            "startedAt byte {index} is not a digit in {value:?}"
        );
    }
}
