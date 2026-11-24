//! The publish protocol of `DATABASE.md` §5: `stage` → write → `seal` → `rename`.
//!
//! Ported from `src/shared/store/publish.ts` sequence for sequence. The point is that a partial
//! graph is unpublishable *by construction*, not by discipline: nothing ever writes to the live
//! database file. A run copies the live file (`VACUUM INTO`) or starts an empty one, writes into
//! that copy, and only a sealed copy is renamed over the live path. Any failure before the rename
//! ends in [`discard`], so the live file keeps serving the previous generation and two generators
//! cannot interleave into one file.
//!
//! Each seal step closes a different hole:
//! - `wal_checkpoint(TRUNCATE)` empties the WAL, so the file about to be renamed is the whole
//!   database and not a database plus a log the rename would leave behind;
//! - `journal_mode = DELETE` + `synchronous = FULL` leaves no `-wal`/`-shm` beside the stage, so
//!   the renamed file is the entire generation; a reader cannot be handed a log from the old one;
//! - `quick_check(1)` is the last chance to refuse a structurally broken file: a corrupt graph is
//!   discarded before it can replace a working one.
//!
//! Handle ownership, because three handles touch the stage:
//! - [`stage`] opens its own throwaway handle for the seed and drops it before returning;
//! - [`begin_write`]'s handle belongs to the caller and is released by [`WriteScope::close`];
//! - [`seal`] opens its own fresh handle, runs all four steps anyway and reads every value back, so
//!   it verifies that state instead of trusting `close()` — a caller that forgot `close()` gets an
//!   `AppError` `internal` (`database is locked`), not a half-sealed graph.

use std::collections::hash_map::RandomState;
use std::fs;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};
use sangkar_core::errors::AppError;
use sangkar_core::log;

use crate::db::{Conn, open_reader, open_writer};

/// The `live.stage.<rand>` infix; [`publish`] refuses any path that does not carry it.
pub const STAGE_INFIX: &str = ".stage.";

/// `SQLite` value of `synchronous = FULL`; `NORMAL` is 1.
const SYNCHRONOUS_FULL: i64 = 2;

/// What `PRAGMA journal_mode = DELETE` must answer for a stage to be publishable.
const SEALED_JOURNAL_MODE: &str = "delete";

/// Sidecars a database file may own; all of them belong to the file they are named after.
const SIDECARS: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Bytes of entropy in a stage suffix: 6, the length `randomBytes(6).toString("hex")` produced.
const STAGE_RANDOM_BYTES: usize = 6;

/// The hint a sidecar that would not go away is reported with: a `-wal` beside a published graph is
/// a file the next reader would interpret against a database that is not the one it was written
/// for, so a caller that sees this must fix the directory, not retry.
const STALE_SIDECAR_HINT: &str = concat!(
    "the data directory is not writable; a stale sidecar must never be left beside ",
    "a published graph"
);

/// What the stage was seeded from: the live graph, or nothing at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeededFrom {
    /// A `VACUUM INTO` copy of the live graph, the incremental path.
    Live,
    /// A brand-new empty file: a forced full index, or the first run with no live graph yet.
    Empty,
}

impl SeededFrom {
    /// The wire spelling the TypeScript `StageOptions`/`Stage` pair uses.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Empty => "empty",
        }
    }
}

impl std::fmt::Display for SeededFrom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A stage file beside the live one, plus what it was seeded from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stage {
    /// The live database this stage will be renamed over.
    pub live_path: PathBuf,
    /// `live.stage.<rand>`; the only kind of path [`publish`] accepts as its source.
    pub path: PathBuf,
    /// [`SeededFrom::Live`] after a `VACUUM INTO` seed, [`SeededFrom::Empty`] on a forced full index.
    pub seeded_from: SeededFrom,
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`'s report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointReport {
    /// `1` when another connection still holds the WAL; a sealed stage needs `0`.
    pub busy: i64,
    /// WAL frames in the log and frames moved into the database; `-1` on a non-WAL file.
    pub log: i64,
    pub checkpointed: i64,
}

/// What [`seal`] observed, read back rather than assumed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealReport {
    pub checkpoint: CheckpointReport,
    /// `PRAGMA journal_mode` read back: `"delete"`.
    pub journal_mode: String,
    /// `PRAGMA synchronous` read back: `2`, i.e. `FULL`.
    pub synchronous: i64,
    /// `PRAGMA quick_check(1)` rows; a sound database reports exactly `["ok"]`.
    pub quick_check: Vec<String>,
}

/// The open writer on a stage file. The handle is the caller's to release: [`begin_write`] opened
/// it, and [`seal`] fails while it is still open — which is how a forgotten `close()` is caught
/// instead of a stage whose WAL still has a handle attached being sealed.
#[derive(Debug)]
pub struct WriteScope {
    path: PathBuf,
    conn: Conn,
}

impl WriteScope {
    /// The stage this writer is attached to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The underlying handle, for the raw-SQL paths the store has no wrapper for.
    #[must_use]
    pub fn conn(&self) -> &Conn {
        &self.conn
    }

    /// The same handle, mutably, for the calls that need ownership — the migration runner is one:
    /// an empty seed carries no schema, so the caller runs [`crate::migrate::migrate`] on the stage,
    /// and that takes `&mut Connection`.
    ///
    /// Two borrows cannot be live at once, so a mutable borrow here is also what keeps a raw
    /// statement and a transaction from interleaving.
    pub fn conn_mut(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// Runs `callback` inside one `BEGIN IMMEDIATE` transaction and returns its value.
    ///
    /// `immediate` takes the write lock up front, so a contended writer fails before the first
    /// statement instead of midway through the load, where a half-written stage would already
    /// exist. The `Transaction` rolls back when it is dropped, so an `Err` from the callback — or
    /// a panic inside it — leaves no partial rows behind; `commit` is the only path that keeps
    /// them.
    ///
    /// `&mut self` is what keeps two transactions from being open at once: opening one borrows the
    /// handle exclusively for as long as the transaction lives.
    pub fn transaction<T, F>(&mut self, callback: F) -> Result<T, AppError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<T, AppError>,
    {
        let path = self.path.clone();
        let transaction = self
            .conn
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| {
                AppError::internal(format!("BEGIN IMMEDIATE on {}: {error}", path.display()))
            })?;
        let value = callback(&transaction)?;
        transaction.commit().map_err(|error| {
            AppError::internal(format!("COMMIT on {}: {error}", path.display()))
        })?;
        Ok(value)
    }

    /// Ends the write phase: `wal_checkpoint(TRUNCATE)`, then `journal_mode = DELETE`, then the
    /// handle closes. Consuming, so "the scope is unusable afterwards" is a compile error instead
    /// of a flag — the same shape [`Conn::close`] has.
    ///
    /// The ordering is not cosmetic. SQLite defers the real `sqlite3_close` while a statement is
    /// still referenced, so a connection closed behind a live statement keeps the file open and
    /// leaves `-wal`/`-shm` beside the stage; the journal-mode change from another connection then
    /// fails with `SQLITE_BUSY` and the stage can never be sealed. Doing both PRAGMAs on the
    /// connection that legitimately owns the file, while it is uncontended, is what makes [`seal`]
    /// reachable at all — `rusqlite` handing a still-open connection back from a failed `close`
    /// (`SQLITE_BUSY`, statement alive) is the same deferral, one layer up.
    ///
    /// A failure here is `internal`: a run that cannot hand over a clean file must fail, not
    /// publish.
    pub fn close(self) -> Result<(), AppError> {
        let Self { path, conn } = self;
        let released = release_wal(conn.connection());
        let closed = conn.close();
        if let Err(error) = released {
            return Err(AppError::internal(format!(
                "cannot release the WAL on {}: {error}",
                path.display()
            ))
            .with_hint("close every other handle on the stage; the graph stays unpublished"));
        }
        closed
    }
}

/// Discards a stage on every way out of [`with_stage`], a panic included — the `Drop` body is the
/// only thing Rust runs on all of them.
struct StageGuard {
    path: PathBuf,
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        discard_quietly(&self.path);
    }
}

/// `bun:sqlite` cannot open a URI filename, and `rename` cannot rename one either, so both
/// spellings are refused before SQLite can report it confusingly.
///
/// The same guard exists in [`crate::db`] for handles; it is repeated here because a publish path is
/// not always opened by the store — `rename` and `remove_file` touch it directly — so the check has
/// to happen before this module's own filesystem call, not only in front of a connection.
fn assert_filesystem_path(path: &Path) -> Result<(), AppError> {
    if path.to_string_lossy().starts_with("file:") {
        return Err(AppError::internal(format!(
            "the publish path cannot be a URI filename: {}",
            path.display()
        ))
        .with_hint("pass a plain filesystem path"));
    }
    Ok(())
}

/// `path` with `suffix` appended as bytes, so a non-UTF-8 data directory keeps its spelling.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 6 random bytes as lowercase hex — the 12 characters of a stage suffix.
///
/// `rand` and `getrandom` are not dependencies of this crate and are not needed for a name: the
/// entropy comes from the standard library's one per-instance OS-seeded source. `RandomState::new()`
/// seeds each hasher from the operating system and is documented to make two instances produce
/// different keys, which is what keeps two indexers that ignore the write lock from writing the
/// same stage. The clock, the pid and a stack address are mixed in because the fallback must not
/// rest on a single property of one hasher: a process that forked would repeat the pid space, two
/// threads a same-instant clock, and ASLR a same address. Same technique as
/// `sangkar_core::config`'s cursor-key fallback, shortened to the bytes a filename needs.
fn random_suffix() -> String {
    let state = RandomState::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let probe = 0u8;
    let address = std::ptr::addr_of!(probe) as usize;

    let mut hasher = state.build_hasher();
    nanos.hash(&mut hasher);
    pid.hash(&mut hasher);
    address.hash(&mut hasher);
    let digest = hasher.finish().to_le_bytes();

    digest[..STAGE_RANDOM_BYTES]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The path a stage for `live_path` would get: `live.stage.<rand>`.
///
/// The suffix is random, not a pid, so two indexers that ignore the write lock still cannot write
/// the same stage.
pub fn stage_path(live_path: &Path) -> Result<PathBuf, AppError> {
    assert_filesystem_path(live_path)?;
    Ok(with_suffix(
        live_path,
        &format!("{STAGE_INFIX}{}", random_suffix()),
    ))
}

/// Removes one file if it is there; a missing file is a no-op, nothing else is.
fn remove_if_present(path: &Path) -> Result<(), AppError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(
            AppError::internal(format!("cannot remove {}: {error}", path.display()))
                .with_hint(STALE_SIDECAR_HINT),
        ),
    }
}

/// `wal_checkpoint(TRUNCATE)` plus `journal_mode = DELETE`, on the connection that owns the file.
///
/// Shared by [`WriteScope::close`] and [`seal`]: `seal` runs it on a fresh handle, where it succeeds
/// when the stage was released and fails (`database is locked`) when it was not. The verdict here is
/// the statements' own; reading the values back is `seal`'s job, so this only reports a failure.
fn release_wal(conn: &Connection) -> Result<(), AppError> {
    checkpoint(conn)?;
    conn.execute_batch("PRAGMA journal_mode = DELETE")
        .map_err(|error| AppError::internal(format!("PRAGMA journal_mode = DELETE: {error}")))
}

/// `PRAGMA wal_checkpoint(TRUNCATE)`, as the single row it returns.
fn checkpoint(conn: &Connection) -> Result<CheckpointReport, AppError> {
    let mut statement = conn
        .prepare("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| AppError::internal(format!("PRAGMA wal_checkpoint: {error}")))?;
    let reports = statement
        .query_map([], |row| {
            Ok(CheckpointReport {
                busy: row.get(0)?,
                log: row.get(1)?,
                checkpointed: row.get(2)?,
            })
        })
        .map_err(|error| AppError::internal(format!("PRAGMA wal_checkpoint: {error}")))?
        .collect::<rusqlite::Result<Vec<CheckpointReport>>>()
        .map_err(|error| AppError::internal(format!("PRAGMA wal_checkpoint: {error}")))?;

    match reports.as_slice() {
        [report] => Ok(*report),
        _ => Err(AppError::internal(
            "PRAGMA wal_checkpoint(TRUNCATE) did not return exactly one row",
        )),
    }
}

/// `PRAGMA quick_check(1)`'s rows, in the order SQLite reported them.
fn quick_check(conn: &Connection) -> Result<Vec<String>, AppError> {
    let mut statement = conn
        .prepare("PRAGMA quick_check(1)")
        .map_err(|error| AppError::internal(format!("PRAGMA quick_check: {error}")))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| AppError::internal(format!("PRAGMA quick_check: {error}")))?
        .collect::<rusqlite::Result<Vec<String>>>()
        .map_err(|error| AppError::internal(format!("PRAGMA quick_check: {error}")))?;
    Ok(rows)
}

/// The `journal_mode = DELETE` read-back: exactly `delete`, or the observed value is the message.
fn expect_journal_mode(stage_file: &Path, observed: &str) -> Result<(), AppError> {
    if observed != SEALED_JOURNAL_MODE {
        return Err(AppError::internal(format!(
            "journal_mode is {observed}, expected {SEALED_JOURNAL_MODE}, on {}",
            stage_file.display()
        ))
        .with_hint("another connection is still open on the stage; close it before sealing"));
    }
    Ok(())
}

/// The `synchronous` read-back: exactly `2` (`FULL`), or the observed value is the message.
fn expect_synchronous(observed: i64) -> Result<(), AppError> {
    if observed != SYNCHRONOUS_FULL {
        return Err(AppError::internal(format!(
            "synchronous is {observed}, expected {SYNCHRONOUS_FULL}"
        )));
    }
    Ok(())
}

/// The `quick_check(1)` read-back: exactly the single row `ok`, or the observed text is the message.
fn expect_quick_check(stage_file: &Path, observed: &[String]) -> Result<(), AppError> {
    if observed.len() != 1 || observed[0] != "ok" {
        return Err(AppError::internal(format!(
            "quick_check failed on {}: {}",
            stage_file.display(),
            observed.join("; ")
        ))
        .with_hint("the stage is unusable; it is discarded rather than published"));
    }
    Ok(())
}

/// Cleanup that must run on the failure path without replacing the failure: the reason the run
/// died is what the caller can act on, so a cleanup error is reported (stderr, error level) and the
/// original error keeps its place.
fn discard_quietly(path: &Path) {
    if let Err(error) = discard(path) {
        log::error(
            "cannot discard stage",
            &[
                ("path", path.display().to_string().into()),
                ("reason", error.message.clone().into()),
            ],
        );
    }
}

/// A read-write handle without a PRAGMA set and *without* `SQLITE_OPEN_CREATE`: what [`seal`]
/// needs, where the writer's `journal_mode = WAL` would undo the file state the seal is about to
/// establish, and where a missing stage must be an error instead of a fresh empty file.
///
/// `SQLITE_OPEN_URI` is absent, as on every other handle: a URI filename is never parsed as one.
fn open_existing(path: &Path) -> Result<Connection, AppError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| AppError::internal(format!("cannot open {}: {error}", path.display())))
}

/// `VACUUM INTO` a stage from the live graph.
fn seed_from_live_graph(live_path: &Path, path: &Path) -> Result<(), AppError> {
    // Read-only: the source of a publish is never written by the publisher.
    let source = open_reader(live_path)?;
    let target = path.to_str().ok_or_else(|| {
        AppError::internal(format!(
            "the stage path {} is not valid UTF-8",
            path.display()
        ))
        .with_hint("VACUUM INTO takes its destination as text; use a UTF-8 data directory")
    })?;
    // A bound parameter, not string interpolation: the path is data, and a quote in a data
    // directory name must not become SQL.
    source
        .connection()
        .execute("VACUUM INTO ?1", [target])
        .map_err(|error| {
            AppError::internal(format!(
                "VACUUM INTO {} from {}: {error}",
                path.display(),
                live_path.display()
            ))
        })?;
    Ok(())
}

/// An empty stage: a file with no schema and no rows, which the caller migrates like a fresh
/// database. This is the one handle that creates — the stage does not exist until it does.
fn seed_empty(path: &Path) -> Result<(), AppError> {
    let handle = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| AppError::internal(format!("cannot create {}: {error}", path.display())))?;
    handle.close().map_err(|(_, error)| {
        AppError::internal(format!("cannot close {}: {error}", path.display()))
    })
}

/// Creates the stage file a run writes into: `VACUUM INTO` from the live graph, or an empty file
/// when `seed_from_live` is false and when there is no live graph yet, which is first run.
///
/// `seed_from_live` is the negation of the TypeScript's `forceFull`: a forced full index wants no
/// previous rows, so it passes `false`. The seed is packed by `VACUUM INTO` rather than byte-copied,
/// so free pages and the previous `-wal` do not travel with it.
pub fn stage(live_path: &Path, seed_from_live: bool) -> Result<Stage, AppError> {
    assert_filesystem_path(live_path)?;
    let path = stage_path(live_path)?;
    let has_graph = fs::metadata(live_path)
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false);
    let seeded_from = if seed_from_live && has_graph {
        SeededFrom::Live
    } else {
        SeededFrom::Empty
    };

    let seeded = (|| -> Result<(), AppError> {
        // A stage beside a relative live path has an empty parent, which already exists.
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|error| {
                AppError::internal(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        match seeded_from {
            SeededFrom::Live => seed_from_live_graph(live_path, &path),
            SeededFrom::Empty => seed_empty(&path),
        }
    })();

    if let Err(error) = seeded {
        // A half-seeded stage must not survive: it would be picked up by the next run as a graph.
        discard_quietly(&path);
        return Err(AppError::internal(format!(
            "cannot stage {} from {seeded_from}: {error}",
            live_path.display()
        ))
        .with_hint(
            "check that the data directory exists, is writable and holds a readable graph.db",
        ));
    }

    log::debug(
        "stage created",
        &[
            ("path", path.display().to_string().into()),
            ("livePath", live_path.display().to_string().into()),
            ("seededFrom", seeded_from.as_str().into()),
        ],
    );
    Ok(Stage {
        live_path: live_path.to_path_buf(),
        path,
        seeded_from,
    })
}

/// Opens a writer on a stage file. The scope's handle is released by [`WriteScope::close`], and must
/// be before [`seal`] runs.
pub fn begin_write(stage_file: &Path) -> Result<WriteScope, AppError> {
    let conn = open_writer(stage_file)?;
    Ok(WriteScope {
        path: conn.path().to_path_buf(),
        conn,
    })
}

/// Makes the stage publishable: `DATABASE.md` §5's four steps, every value read back.
///
/// Fails `internal` when any step does not reach its expected value — including a write scope that
/// was never closed, where SQLite refuses the journal-mode change. The checkpoint and the
/// journal-mode change are cheap when the stage was released by [`WriteScope::close`] (then the
/// read-back is the assertion) and are the work when a stage arrives still in WAL mode with no
/// connection holding it, e.g. after a previous run died.
pub fn seal(stage_file: &Path) -> Result<SealReport, AppError> {
    assert_filesystem_path(stage_file)?;
    // A fresh handle, not the writing one: the read-backs have to describe the file, not a
    // connection that could be remembering its own settings.
    let conn = open_existing(stage_file)?;
    let verdict = seal_on(&conn, stage_file);
    let closed = conn.close();
    // The handle is the sealer's own, and nothing it set is lost by dropping it: `journal_mode` is
    // a property of the file (the read/write version bytes of its header), which is why the
    // read-back above means anything after this handle is gone.
    match (verdict, closed) {
        (Ok(report), Ok(())) => Ok(report),
        (Ok(_), Err((_, error))) => Err(AppError::internal(format!(
            "cannot close {}: {error}",
            stage_file.display()
        ))),
        // A failing seal keeps its own verdict: it is the reason the run died.
        (Err(error), _) => Err(error),
    }
}

/// The four steps on an open handle, in the order that makes the file publishable.
fn seal_on(conn: &Connection, stage_file: &Path) -> Result<SealReport, AppError> {
    let checkpoint = checkpoint(conn)?;
    if checkpoint.busy != 0 {
        return Err(AppError::internal(format!(
            "wal_checkpoint(TRUNCATE) is busy ({}) on {}",
            checkpoint.busy,
            stage_file.display()
        ))
        .with_hint("another handle still holds the stage; close the write scope before sealing"));
    }

    let journal_mode = conn
        .query_row("PRAGMA journal_mode = DELETE", [], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|error| {
            AppError::internal(format!(
                "cannot seal {}: PRAGMA journal_mode = DELETE: {error}",
                stage_file.display()
            ))
            .with_hint("another connection is still open on the stage; close it before sealing")
        })?;
    expect_journal_mode(stage_file, &journal_mode)?;

    conn.execute_batch("PRAGMA synchronous = FULL")
        .map_err(|error| AppError::internal(format!("PRAGMA synchronous = FULL: {error}")))?;
    let synchronous = conn
        .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
        .map_err(|error| {
            AppError::internal(format!("PRAGMA synchronous returned no integer: {error}"))
        })?;
    expect_synchronous(synchronous)?;

    let quick_check = quick_check(conn)?;
    expect_quick_check(stage_file, &quick_check)?;

    let report = SealReport {
        checkpoint,
        journal_mode,
        synchronous,
        quick_check,
    };
    log::debug(
        "stage sealed",
        &[
            ("path", stage_file.display().to_string().into()),
            ("journal_mode", report.journal_mode.clone().into()),
            ("synchronous", report.synchronous.into()),
            ("checkpoint_busy", report.checkpoint.busy.into()),
            ("quick_check", report.quick_check.join("; ").into()),
        ],
    );
    Ok(report)
}

/// Renames a sealed stage over the live file and drops the previous generation's sidecars.
///
/// Order matters: the rename comes first, because deleting the old `-wal` before the swap would
/// throw away committed data if the swap then failed. The sidecars are removed after it because
/// they describe the file that is no longer there — the newly published file is in
/// `journal_mode = DELETE`, so any `-wal`/`-shm`/`-journal` beside it is a leftover and must not be
/// interpreted against it.
pub fn publish(stage_file: &Path, live_path: &Path) -> Result<(), AppError> {
    assert_filesystem_path(live_path)?;
    assert_filesystem_path(stage_file)?;
    if !stage_file.to_string_lossy().contains(STAGE_INFIX) {
        return Err(AppError::internal(format!(
            "refusing to publish {}: not a stage file",
            stage_file.display()
        ))
        .with_hint(format!(
            "a publishable file is named {}{STAGE_INFIX}<rand>",
            live_path.display()
        )));
    }
    if !stage_file.exists() {
        return Err(AppError::internal(format!(
            "stage file {} does not exist",
            stage_file.display()
        ))
        .with_hint("stage, write and seal before publishing"));
    }

    fs::rename(stage_file, live_path).map_err(|error| {
        AppError::internal(format!(
            "cannot rename {} over {}: {error}",
            stage_file.display(),
            live_path.display()
        ))
        .with_hint("the stage and the live file must be in the same directory (same filesystem)")
    })?;

    for suffix in SIDECARS {
        remove_if_present(&with_suffix(live_path, suffix))?;
    }

    let bytes = fs::metadata(live_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    log::info(
        "graph published",
        &[
            ("path", live_path.display().to_string().into()),
            ("bytes", bytes.into()),
        ],
    );
    Ok(())
}

/// Removes a stage and its sidecars. Never touches the live file: the paths only differ by
/// [`STAGE_INFIX`], and this only deletes the path it was given.
pub fn discard(stage_file: &Path) -> Result<(), AppError> {
    assert_filesystem_path(stage_file)?;
    remove_if_present(stage_file)?;
    for suffix in SIDECARS {
        remove_if_present(&with_suffix(stage_file, suffix))?;
    }
    log::debug(
        "stage discarded",
        &[("path", stage_file.display().to_string().into())],
    );
    Ok(())
}

/// Runs `callback` against a fresh stage of `live_path` and publishes only if it returns `Ok`:
/// `stage` → `callback` → `seal` → `publish`, with `discard` on every other path.
///
/// The [`StageGuard`] is the guarantee §5 asks for: an `Err` anywhere in the callback, in `seal` or
/// in `publish`, and a panic anywhere in the callback, reaches `discard` and never a second
/// `publish`, so the live file (and its bytes) is unchanged and no `-wal`/`-shm`/`.stage.` file is
/// left in the data directory. After a successful publish the same `discard` is a no-op on the stage
/// path, which is why it runs unconditionally here just as it does in the TypeScript's `finally`.
///
/// The callback owns its write scope and must close it; `seal` fails loudly otherwise. The
/// callback's return value is passed through.
pub fn with_stage<T, F>(live_path: &Path, seed_from_live: bool, callback: F) -> Result<T, AppError>
where
    F: FnOnce(&Stage) -> Result<T, AppError>,
{
    let staged = stage(live_path, seed_from_live)?;
    // Dropped on every exit from here on, including the unwind of a panicking callback.
    let _guard = StageGuard {
        path: staged.path.clone(),
    };
    let value = callback(&staged)?;
    // Sealing here rather than trusting the callback to have done it is what makes the rename
    // itself the only path to the live file. `seal` is idempotent, so a callback that sealed to
    // verify is not penalised.
    seal(&staged.path)?;
    publish(&staged.path, live_path)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{STAGE_INFIX, expect_journal_mode, expect_quick_check, expect_synchronous};

    /// A stage that answers any journal mode but `delete` is refused *with the observed value*, so
    /// an operator reads what the file actually is instead of a bare "seal failed".
    ///
    /// Driven directly because a modern SQLite returns `delete` for a file it can switch at all: the
    /// reachable real-world failure is `SQLITE_BUSY` (another handle, `tests/publish.rs`), and the
    /// mismatch branch is only reachable through the read-back that guards it.
    #[test]
    fn journal_mode_read_back_names_the_observed_value() {
        let stage = Path::new("/tmp/graph.db.stage.0a1b2c3d4e5f");
        let error = expect_journal_mode(stage, "wal").expect_err("wal is not a sealed stage");
        assert_eq!(
            error.message,
            "journal_mode is wal, expected delete, on /tmp/graph.db.stage.0a1b2c3d4e5f"
        );
        assert!(error.hint.is_some());
        assert!(expect_journal_mode(stage, "delete").is_ok());
    }

    /// `synchronous` is read back, not assumed: anything but `FULL` (2) refuses the stage.
    #[test]
    fn synchronous_read_back_refuses_anything_but_full() {
        let error = expect_synchronous(1).expect_err("NORMAL is not a sealed stage");
        assert_eq!(error.message, "synchronous is 1, expected 2");
        assert!(expect_synchronous(2).is_ok());
    }

    /// The quick-check read-back is exact: the single row `ok`, never a prefix match, and the
    /// observed text travels in the message.
    #[test]
    fn quick_check_read_back_carries_the_observed_text() {
        let stage = Path::new("/tmp/graph.db.stage.0a1b2c3d4e5f");
        assert!(expect_quick_check(stage, &["ok".to_owned()]).is_ok());

        let corrupt = vec![
            "*** in database main ***\nTree 2 page 2: btreeInitPage() returns error code 11"
                .to_owned(),
        ];
        let error = expect_quick_check(stage, &corrupt).expect_err("corruption is not a seal");
        assert!(error.message.starts_with(
            "quick_check failed on /tmp/graph.db.stage.0a1b2c3d4e5f: *** in database main ***"
        ));
        assert!(
            error
                .message
                .contains("btreeInitPage() returns error code 11")
        );

        let empty: Vec<String> = Vec::new();
        assert!(
            expect_quick_check(stage, &empty).is_err(),
            "no rows is a failure, not a pass"
        );
        let partial = vec!["ok".to_owned(), "ok".to_owned()];
        assert!(
            expect_quick_check(stage, &partial).is_err(),
            "exactly one row, not two"
        );
    }

    /// The infix is a contract with `publish.ts` and with `publish`'s own source check: a stage
    /// this build writes is a stage that build accepts.
    #[test]
    fn stage_infix_is_pinned() {
        assert_eq!(STAGE_INFIX, ".stage.");
    }
}
