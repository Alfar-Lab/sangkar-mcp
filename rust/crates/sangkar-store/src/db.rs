//! Connection lifecycle: the two PRAGMA sets of `DATABASE.md` §3.7, one handle per role, and the
//! bounded prepared-statement cache that makes the measured 42.7 µs (re-built builder) → 1.7 µs
//! (prepared) difference reachable from every read path.
//!
//! Ported from `src/shared/store/db.ts`. The two traps that module closes once are closed here the
//! same way:
//! - a URI filename (`file:…?immutable=1`) is refused before SQLite can report it confusingly.
//!   `OpenFlags` carries no `SQLITE_OPEN_URI` on either handle, and a reader is
//!   `SQLITE_OPEN_READ_ONLY` alone — so a reader on a missing path fails instead of creating one;
//! - a prepared statement belongs to the connection that built it, so the cache is per handle and
//!   dies with it. A process-wide one would hand a statement bound to a closed database to the
//!   next reader.
//!
//! Readers get their own handle rather than borrowing the writer's: the publish path renames a
//! stage file over the live one, and an open handle must keep serving the previous generation
//! until it is re-opened (`DATABASE.md` §4).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, Weak};

use rusqlite::{CachedStatement, Connection, OpenFlags, OptionalExtension};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log;

/// `DATABASE.md` §3.7, reader column.
const READER_CACHE_SIZE: i64 = -16_384;
/// `DATABASE.md` §3.7, build column.
const WRITER_CACHE_SIZE: i64 = -65_536;
const BUSY_TIMEOUT_MS: i64 = 10_000;
/// 64 MiB of journal kept before SQLite truncates it; the bulk load is what makes it necessary.
const JOURNAL_SIZE_LIMIT: i64 = 67_108_864;
/// The mmap window both handles ask for; identical on purpose (`DATABASE.md` §3.7).
const MMAP_SIZE: i64 = 268_435_456;

/// `SQLite` value of `synchronous = NORMAL`; `FULL` is 2.
const SYNCHRONOUS_NORMAL: i64 = 1;

/// The hint `unsupported_filesystem` carries: a caller that reached it must be told which
/// filesystem to use, not guessed at.
const UNSUPPORTED_FILESYSTEM_HINT: &str =
    "WAL needs a writable local directory; point --data-dir at one";

/// `TECH-SPEC.md` §11 makes boundedness observable: the capacity is published, so growth is a
/// number and not a guess.
pub const STATEMENT_CACHE_CAPACITY: usize = 200;

/// The only two handles the store opens; the mode decides the PRAGMA set, nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnMode {
    Reader,
    Writer,
}

impl ConnMode {
    /// The wire spelling the TypeScript `ConnMode` uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Reader => "reader",
            Self::Writer => "writer",
        }
    }
}

impl std::fmt::Display for ConnMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What one handle's cache has done, or the sum over every live handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatementCacheStats {
    /// Entries currently retained (or the sum over all live handles).
    pub size: usize,
    /// Hard ceiling per handle: [`STATEMENT_CACHE_CAPACITY`].
    pub capacity: usize,
    pub hits: u64,
    pub misses: u64,
    /// Entries dropped because `size` would have exceeded `capacity`.
    pub evictions: u64,
}

/// A bounded LRU of the keys that name query shapes.
///
/// `VecDeque` keeps insertion order, so the front is the least recently used key — the one an
/// eviction drops. Only the *keys* live here: the statements they name are retained by the
/// driver's per-connection cache, which is capped at [`STATEMENT_CACHE_CAPACITY`] as well, so
/// memory stays bounded by the same number from both directions.
#[derive(Debug)]
struct StatementCache {
    entries: VecDeque<&'static str>,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl StatementCache {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    /// Records a lookup and returns whether it was a hit. A hit moves the key to the back and
    /// stops there; a miss only counts, so a build that fails leaves nothing inserted — the order
    /// the TypeScript `get()` uses.
    fn lookup(&mut self, key: &'static str) -> bool {
        if let Some(index) = self.entries.iter().position(|entry| *entry == key) {
            self.hits += 1;
            if let Some(entry) = self.entries.remove(index) {
                self.entries.push_back(entry);
            }
            return true;
        }
        self.misses += 1;
        false
    }

    /// Inserts a freshly built key and drops the oldest one. `insert` grows the set by exactly one,
    /// so the excess can never be more than one and the single `pop_front` is the whole rule.
    fn insert(&mut self, key: &'static str) {
        self.entries.push_back(key);
        if self.entries.len() > STATEMENT_CACHE_CAPACITY {
            self.entries.pop_front();
            self.evictions += 1;
        }
    }

    fn stats(&self) -> StatementCacheStats {
        StatementCacheStats {
            size: self.entries.len(),
            capacity: STATEMENT_CACHE_CAPACITY,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }
}

/// Every live handle's cache, so `statement_cache_stats()` can sum them without holding a
/// connection: `status.payload.health.statement_cache_entries` is read by a caller that may own
/// none. `Weak`, so a closed handle stops counting even though its entry is still in the list.
static LIVE_CACHES: LazyLock<Mutex<Vec<Weak<Mutex<StatementCache>>>>> =
    LazyLock::new(|| Mutex::new(Vec::new()));

/// The cache holds keys and counters only, so a poisoned lock has no torn invariant to protect;
/// recovering keeps a bookkeeping mutex from taking a session down with it.
fn guard<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn register(cache: &Arc<Mutex<StatementCache>>) {
    let mut live = guard(&LIVE_CACHES);
    // Forget the handles that closed since the last registration or stats call, so the list cannot
    // grow with the process's history of readers.
    live.retain(|entry| entry.strong_count() > 0);
    live.push(Arc::downgrade(cache));
}

/// The sum over every live handle: what `status.payload.health.statement_cache_entries` reports
/// without holding a connection. [`Conn::statement_cache_stats`] is the single-handle form.
#[must_use]
pub fn statement_cache_stats() -> StatementCacheStats {
    let live = guard(&LIVE_CACHES);
    let mut total = StatementCacheStats {
        size: 0,
        capacity: STATEMENT_CACHE_CAPACITY,
        hits: 0,
        misses: 0,
        evictions: 0,
    };
    for cache in live.iter().filter_map(Weak::upgrade) {
        let stats = guard(&cache).stats();
        total.size += stats.size;
        total.hits += stats.hits;
        total.misses += stats.misses;
        total.evictions += stats.evictions;
    }
    total
}

/// One open handle: the connection, the LRU that names its statements, and the path it came from.
#[derive(Debug)]
pub struct Conn {
    path: PathBuf,
    mode: ConnMode,
    conn: Connection,
    cache: Arc<Mutex<StatementCache>>,
}

impl Conn {
    /// The filesystem path this handle was opened from, never a URI.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn mode(&self) -> ConnMode {
        self.mode
    }

    /// The raw connection, for the paths (migrations, FTS5, PRAGMAs) the ported SQL needs.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    /// The writer's connection, for the calls that need ownership of a transaction — `migrate`
    /// and the publish path both take `&mut Connection`.
    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Returns the statement cached under `sql_key`, building it once on a miss.
    ///
    /// `sql_key` must name the query shape, including any parameter arity that changes the
    /// generated SQL — the rule the TypeScript `prepared()` states, so a caller cannot key two
    /// different statements under one name.
    ///
    /// A hit moves the key to the most-recently-used end and evicts nothing. A miss prepares
    /// first, then inserts and evicts exactly one key, the oldest. A prepare that fails counts the
    /// miss and inserts nothing.
    ///
    /// `build` runs on a hit too — the statement itself lives in the driver's per-connection cache,
    /// which this wrapper cannot hand out from its own bookkeeping — and that second layer is
    /// capped at the same [`STATEMENT_CACHE_CAPACITY`], so a hit is a lookup, not a parse.
    ///
    /// The `&self` borrow is also what keeps a statement out of a transaction: opening one needs
    /// `&mut self`, so a live [`CachedStatement`] cannot cross that boundary.
    pub fn prepared<'a, F>(
        &'a self,
        sql_key: &'static str,
        build: F,
    ) -> Result<CachedStatement<'a>, AppError>
    where
        F: FnOnce(&'a Connection) -> rusqlite::Result<CachedStatement<'a>>,
    {
        let hit = guard(&self.cache).lookup(sql_key);
        let statement = build(&self.conn)
            .map_err(|error| AppError::internal(format!("preparing {sql_key}: {error}")))?;
        if !hit {
            guard(&self.cache).insert(sql_key);
        }
        Ok(statement)
    }

    /// Cache statistics for this handle; [`statement_cache_stats`] is the sum over all handles.
    #[must_use]
    pub fn statement_cache_stats(&self) -> StatementCacheStats {
        guard(&self.cache).stats()
    }

    /// Closes the handle. The TypeScript's `close()` is idempotent at runtime; here the handle is
    /// consumed, so "unusable afterwards" is a compile-time property instead of a flag.
    pub fn close(self) -> Result<(), AppError> {
        let path = self.path;
        self.conn.close().map_err(|(_, error)| {
            AppError::internal(format!("closing {}: {error}", path.display()))
        })
    }
}

/// A read-only handle: `SQLITE_OPEN_READ_ONLY`, so a write fails inside SQLite rather than in a
/// review, and a missing database is an error instead of an empty graph.
pub fn open_reader(path: impl AsRef<Path>) -> Result<Conn, AppError> {
    open(path.as_ref(), ConnMode::Reader)
}

/// The single writer's handle, in WAL with `synchronous = NORMAL` (`DATABASE.md` §3.7).
pub fn open_writer(path: impl AsRef<Path>) -> Result<Conn, AppError> {
    open(path.as_ref(), ConnMode::Writer)
}

fn open(path: &Path, mode: ConnMode) -> Result<Conn, AppError> {
    assert_filesystem_path(path)?;

    let flags = match mode {
        // `SQLITE_OPEN_URI` is absent on both handles: a URI filename never reaches SQLite's URI
        // parser. `SQLITE_OPEN_CREATE` is absent from the reader for the same reason the
        // TypeScript passes `readonly: true` — reading must not make a database appear.
        ConnMode::Reader => OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ConnMode::Writer => {
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
        }
    };

    let conn = Connection::open_with_flags(path, flags)
        .map_err(|error| AppError::internal(format!("cannot open {}: {error}", path.display())))?;

    // A half-configured handle must not survive: dropping `conn` closes it, so the caller sees the
    // PRAGMA that failed and no descriptor stays open on the data directory.
    apply_pragmas(&conn, mode)?;

    // The contract's ceiling is the retention of the driver's per-connection cache too, so the LRU
    // above and the statements it names stay bounded by the same number.
    conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);

    let cache = Arc::new(Mutex::new(StatementCache::new()));
    register(&cache);

    Ok(Conn {
        path: path.to_path_buf(),
        mode,
        conn,
        cache,
    })
}

/// The PRAGMA statements in application order.
///
/// The order is the contract, not a detail: `journal_mode = WAL` is applied first, as the reference
/// does, because it is the one setting whose result refuses the handle — a refused filesystem must
/// not first pay for six more statements. `busy_timeout` is set before anything that can block on
/// another writer, and `foreign_keys` last, after every statement that could be affected by it.
fn pragmas(mode: ConnMode) -> Vec<String> {
    let busy_timeout = format!("busy_timeout = {BUSY_TIMEOUT_MS}");
    let mmap_size = format!("mmap_size = {MMAP_SIZE}");
    match mode {
        ConnMode::Reader => vec![
            "foreign_keys = ON".to_owned(),
            busy_timeout,
            format!("cache_size = {READER_CACHE_SIZE}"),
            mmap_size,
        ],
        ConnMode::Writer => vec![
            "journal_mode = WAL".to_owned(),
            "synchronous = NORMAL".to_owned(),
            format!("cache_size = {WRITER_CACHE_SIZE}"),
            mmap_size,
            busy_timeout,
            format!("journal_size_limit = {JOURNAL_SIZE_LIMIT}"),
            "foreign_keys = ON".to_owned(),
        ],
    }
}

/// Applies a PRAGMA set and reads every value back. The read-back is the point: a silently ignored
/// PRAGMA is how a store ends up with foreign keys off or a 2 MB page cache, and neither shows up
/// until the data is already wrong or the query is already slow.
fn apply_pragmas(conn: &Connection, mode: ConnMode) -> Result<(), AppError> {
    for pragma in pragmas(mode) {
        // The TypeScript runs `.exec("PRAGMA …")`, which discards the row a setting PRAGMA
        // returns; `execute_batch` is the rusqlite call with that same contract.
        conn.execute_batch(&format!("PRAGMA {pragma}"))
            .map_err(|error| AppError::internal(format!("PRAGMA {pragma} failed: {error}")))?;
    }

    expect_pragma(conn, "foreign_keys", 1)?;
    expect_pragma(conn, "busy_timeout", BUSY_TIMEOUT_MS)?;
    expect_pragma(
        conn,
        "cache_size",
        match mode {
            ConnMode::Reader => READER_CACHE_SIZE,
            ConnMode::Writer => WRITER_CACHE_SIZE,
        },
    )?;

    // mmap_size is a ceiling the OS may refuse to grant in full, and a smaller window is slower
    // rather than wrong, so this one is reported instead of thrown. A pager with no file behind it
    // — an in-memory database — answers the pragma with no row at all, which is the same verdict as
    // a window of zero: no mapping. Reporting it rather than failing is also what keeps the
    // diagnostic from masking the `journal_mode` refusal just below it.
    let granted = pragma_i64_present(conn, "mmap_size")?.unwrap_or(0);
    if granted < MMAP_SIZE {
        log::warn(
            "mmap_size below the requested window",
            &[("requested", MMAP_SIZE.into()), ("granted", granted.into())],
        );
    }

    if mode == ConnMode::Writer {
        let journal_mode = pragma_text(conn, "journal_mode")?;
        if journal_mode != "wal" {
            return Err(AppError::new(
                ErrorCode::UNSUPPORTED_FILESYSTEM,
                format!("journal_mode is {journal_mode}, not wal"),
            )
            .with_hint(UNSUPPORTED_FILESYSTEM_HINT));
        }
        expect_pragma(conn, "synchronous", SYNCHRONOUS_NORMAL)?;
        expect_pragma(conn, "journal_size_limit", JOURNAL_SIZE_LIMIT)?;
    }

    Ok(())
}

fn pragma_i64(conn: &Connection, name: &str) -> Result<i64, AppError> {
    conn.pragma_query_value(None, name, |row| row.get::<_, i64>(0))
        .map_err(|error| AppError::internal(format!("PRAGMA {name} returned no integer: {error}")))
}

/// A settings PRAGMA that some pagers answer with no row at all (`mmap_size` without a file).
fn pragma_i64_present(conn: &Connection, name: &str) -> Result<Option<i64>, AppError> {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|error| AppError::internal(format!("PRAGMA {name} failed: {error}")))
}

fn pragma_text(conn: &Connection, name: &str) -> Result<String, AppError> {
    conn.pragma_query_value(None, name, |row| row.get::<_, String>(0))
        .map_err(|error| AppError::internal(format!("PRAGMA {name} returned no text: {error}")))
}

fn expect_pragma(conn: &Connection, name: &str, expected: i64) -> Result<(), AppError> {
    let actual = pragma_i64(conn, name)?;
    if actual != expected {
        return Err(AppError::internal(format!(
            "PRAGMA {name} is {actual}, expected {expected}"
        )));
    }
    Ok(())
}

/// `bun:sqlite` cannot open a URI filename, so the TypeScript rejects one before SQLite reports it
/// confusingly. The refusal is kept even though rusqlite could parse it: one filename form, both
/// implementations, and a `file:` path that silently means something else is exactly the trap.
fn assert_filesystem_path(path: &Path) -> Result<(), AppError> {
    if path.to_string_lossy().starts_with("file:") {
        return Err(AppError::internal(format!(
            "the graph store cannot open the URI filename {}",
            path.display()
        ))
        .with_hint("pass a plain filesystem path"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ConnMode, pragmas};

    /// The two sets are a contract with `src/shared/store/db.ts`: the order decides whether
    /// `journal_mode` can be refused before `synchronous` is set, so it is pinned byte for byte.
    #[test]
    fn pragma_sets_are_the_typescript_sequences() {
        assert_eq!(
            pragmas(ConnMode::Reader),
            [
                "foreign_keys = ON",
                "busy_timeout = 10000",
                "cache_size = -16384",
                "mmap_size = 268435456",
            ]
        );
        assert_eq!(
            pragmas(ConnMode::Writer),
            [
                "journal_mode = WAL",
                "synchronous = NORMAL",
                "cache_size = -65536",
                "mmap_size = 268435456",
                "busy_timeout = 10000",
                "journal_size_limit = 67108864",
                "foreign_keys = ON",
            ]
        );
    }
}
