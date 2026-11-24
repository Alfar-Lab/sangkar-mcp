//! The publish protocol's contract on real files (`DATABASE.md` §5), ported from the publish half
//! of `tests/publish.test.ts`.
//!
//! Every test runs over its own temporary directory, so "no `.stage.` file survives any path" is a
//! check on a listing this suite owns. The invariant under test is the one the product rests on:
//! a reader keeps serving the previous generation while a new one is written, sealed and renamed
//! over the live file, and a run that dies anywhere before the rename leaves the live file's bytes
//! exactly as they were.

use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_store::db::{open_reader, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::{read_schema_version, schema_version};
use sangkar_store::publish::{
    STAGE_INFIX, SeededFrom, begin_write, discard, publish, seal, stage, stage_path, with_stage,
};

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
            "sangkar-publish-{label}-{}-{nanos}-{}",
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

/// `<path><suffix>`, spelled the way the store spells a sidecar.
fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// Every `.stage.` entry in `dir`. The protocol says this is always empty once a call returns,
/// whether it returned a value, an error or an unwind.
fn stage_leftovers(dir: &Path) -> Vec<String> {
    let mut leftovers = fs::read_dir(dir)
        .expect("list the data directory")
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().to_string_lossy().into_owned();
            name.contains(STAGE_INFIX).then_some(name)
        })
        .collect::<Vec<_>>();
    leftovers.sort();
    leftovers
}

/// The names beside `path` that belong to it: its `-wal`, `-shm` and `-journal`, if any.
fn sidecars_beside(path: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm", "-journal"]
        .iter()
        .map(|suffix| sidecar(path, suffix))
        .filter(|candidate| candidate.exists())
        .collect()
}

fn one_i64(conn: &Connection, sql: &str) -> i64 {
    conn.query_row(sql, [], |row| row.get::<_, i64>(0))
        .expect("run the scalar query")
}

fn pragma_text(conn: &Connection, name: &str) -> String {
    conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|error| panic!("PRAGMA {name}: {error}"))
}

/// The previous generation: a migrated database with a marker row and one generation row, in WAL
/// as a real one is, checkpointed so the main file is the whole graph and the bytes are stable.
fn populated_live(dir: &TempDir) -> PathBuf {
    let live = dir.join("graph.db");
    let mut conn = open_writer(&live).expect("open the live writer");
    migrate(conn.connection_mut()).expect("migrate the live graph");
    conn.connection()
        .execute_batch(
            "INSERT INTO meta (key, value) VALUES ('marker', 'previous generation');\n\
             INSERT INTO generation \
             (id, seq, published_at, files_indexed, nodes, edges, duration_ms, mode, status) \
             VALUES ('gen-old', 1, '2026-01-01T00:00:00.000Z', 3, 10, 4, 12, 'full', 'ok');",
        )
        .expect("write the previous generation");
    conn.connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint the previous generation");
    conn.close().expect("close the live writer");
    live
}

/// Zeroes a whole page deep enough into the file that the 100-byte header survives: the file still
/// opens, and it is `quick_check` — not the pager — that has to refuse it. Every table the schema
/// creates has a b-tree root on one of these pages.
fn zero_page_two(path: &Path) {
    let bytes = fs::read(path).expect("read the stage");
    let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65_536,
        raw => usize::from(raw),
    };
    let mut handle = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the stage for writing");
    handle
        .seek(SeekFrom::Start(page_size as u64))
        .expect("seek to page two");
    handle
        .write_all(&vec![0u8; page_size])
        .expect("zero page two");
}

/// Zeroes the 16-byte magic string, the cheapest deterministic way to make a stage unpublishable:
/// the handle opens and every statement against it fails with `file is not a database`.
fn zero_header(path: &Path) {
    let mut handle = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the stage for writing");
    handle.write_all(&[0u8; 16]).expect("zero the header");
}

/// `PRAGMA quick_check(1)`'s rows, observed independently of the store.
fn observed_quick_check(path: &Path) -> Vec<String> {
    let conn = open_reader(path).expect("open the stage read-only");
    let mut statement = conn
        .connection()
        .prepare("PRAGMA quick_check(1)")
        .expect("prepare quick_check");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("run quick_check")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("read quick_check rows")
}

/// `live.stage.<12 lowercase hex>`, random per call: two indexers cannot pick the same stage.
#[test]
fn stage_path_is_the_live_path_plus_twelve_lowercase_hex() {
    let live = Path::new("/tmp/sangkar/graph.db");
    let path = stage_path(live).expect("stage path");
    let name = path
        .file_name()
        .expect("a stage path has a file name")
        .to_string_lossy()
        .into_owned();

    let expected_prefix = format!("graph.db{STAGE_INFIX}");
    assert!(
        name.starts_with(&expected_prefix),
        "{name} does not start with {expected_prefix}"
    );
    let suffix = &name[expected_prefix.len()..];
    assert_eq!(suffix.len(), 12, "6 random bytes, as hex: {suffix}");
    assert!(
        suffix
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character)),
        "{suffix} is not lowercase hex"
    );
    assert_eq!(
        path.parent(),
        live.parent(),
        "a stage is beside the live file, so the rename stays on one filesystem"
    );

    let twin = stage_path(live).expect("stage path");
    assert_ne!(path, twin, "two stages never share a name");

    let refused = stage_path(Path::new("file:graph.db?immutable=1")).expect_err("a URI is refused");
    assert_eq!(refused.code, ErrorCode::INTERNAL);
}

/// A stage is a `VACUUM INTO` copy of the live graph, or an empty file, and it says which.
#[test]
fn stage_seeds_from_the_live_graph_or_starts_empty() {
    let dir = TempDir::new("stage");
    let live = populated_live(&dir);

    let copy = stage(&live, true).expect("stage from the live graph");
    assert_eq!(copy.seeded_from, SeededFrom::Live);
    assert_eq!(copy.live_path, live);
    let reader = open_reader(&copy.path).expect("read the seeded stage");
    assert_eq!(
        one_i64(
            reader.connection(),
            "SELECT count(*) FROM meta WHERE key = 'marker'"
        ),
        1,
        "the seed carries the previous generation's rows"
    );
    assert_eq!(read_schema_version(reader.connection()), schema_version());

    let empty = stage(&live, false).expect("stage an empty file");
    assert_eq!(empty.seeded_from, SeededFrom::Empty);
    let reader = open_reader(&empty.path).expect("read the empty stage");
    assert_eq!(
        one_i64(reader.connection(), "SELECT count(*) FROM sqlite_master"),
        0,
        "an empty seed carries no schema; the caller migrates it"
    );

    // First run: there is no live graph to copy, so even a seeding caller gets an empty stage.
    let first = stage(&dir.join("nothing-here.db"), true).expect("stage with no live graph");
    assert_eq!(first.seeded_from, SeededFrom::Empty);
    assert!(first.path.exists());

    assert!(stage_leftovers(&dir.0).len() >= 3, "three stages are open");
}

/// The Done-when: publishing an empty stage over a populated database leaves the previous rows
/// gone, the new schema version present, and no `.stage.` file behind.
#[test]
fn publish_replaces_a_populated_database_and_leaves_no_stage_behind() {
    let dir = TempDir::new("publish-empty");
    let live = populated_live(&dir);
    let previous = fs::read(&live).expect("read the previous generation");

    let staged = stage(&live, false).expect("stage an empty file");
    let mut scope = begin_write(&staged.path).expect("open the stage writer");
    migrate(scope.conn_mut().connection_mut()).expect("migrate the empty stage");
    scope.close().expect("release the stage writer");
    seal(&staged.path).expect("seal the stage");
    publish(&staged.path, &live).expect("publish the stage");

    let published = open_reader(&live).expect("read the published graph");
    assert_eq!(
        read_schema_version(published.connection()),
        schema_version(),
        "the published file is the migrated stage"
    );
    assert_eq!(
        one_i64(
            published.connection(),
            "SELECT count(*) FROM meta WHERE key = 'marker'"
        ),
        0,
        "the previous generation's rows are gone"
    );
    assert_eq!(
        one_i64(published.connection(), "SELECT count(*) FROM generation"),
        0,
        "the previous generation row is gone"
    );
    assert_eq!(
        pragma_text(published.connection(), "journal_mode"),
        "delete",
        "what is live is a sealed file"
    );

    assert_eq!(
        stage_leftovers(&dir.0),
        Vec::<String>::new(),
        "a publish leaves no stage to clean up"
    );
    assert!(
        sidecars_beside(&live).is_empty(),
        "the previous generation's sidecars are gone: {:?}",
        sidecars_beside(&live)
    );
    assert_ne!(
        fs::read(&live).expect("read the published file"),
        previous,
        "the bytes on disk really changed"
    );
}

/// `BEGIN IMMEDIATE` commits on `Ok`, rolls back on `Err`, and `close()` hands over a file that is
/// no longer in WAL.
#[test]
fn begin_write_commits_rolls_back_and_releases_the_wal() {
    let dir = TempDir::new("write-scope");
    let staged = stage(&dir.join("graph.db"), false).expect("stage an empty file");
    let mut scope = begin_write(&staged.path).expect("open the stage writer");

    scope
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE probe (value TEXT)")
                .map_err(|error| AppError::internal(format!("create probe: {error}")))
        })
        .expect("commit the schema");
    scope
        .transaction(|tx| {
            tx.execute("INSERT INTO probe VALUES ('committed')", [])
                .map_err(|error| AppError::internal(format!("insert: {error}")))?;
            Ok(())
        })
        .expect("commit a row");

    let failed = scope
        .transaction(|tx| -> Result<(), AppError> {
            tx.execute("INSERT INTO probe VALUES ('rolled back')", [])
                .map_err(|error| AppError::internal(format!("insert: {error}")))?;
            Err(AppError::internal("the load failed"))
        })
        .expect_err("a failing callback fails its transaction");
    assert_eq!(failed.message, "the load failed");

    scope.close().expect("release the stage writer");

    let reader = open_reader(&staged.path).expect("read the released stage");
    let values = {
        let mut statement = reader
            .connection()
            .prepare("SELECT value FROM probe ORDER BY value")
            .expect("prepare the probe query");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("run the probe query")
            .collect::<rusqlite::Result<Vec<String>>>()
            .expect("read the probe rows")
    };
    assert_eq!(
        values,
        vec!["committed".to_owned()],
        "the committed row survived and the rolled-back one did not"
    );
    assert_eq!(
        pragma_text(reader.connection(), "journal_mode"),
        "delete",
        "close() checkpoints the WAL and drops the handbook to DELETE"
    );
    assert!(
        sidecars_beside(&staged.path).is_empty(),
        "close() leaves no -wal/-shm: {:?}",
        sidecars_beside(&staged.path)
    );
}

/// `seal` on a stage a live handle still owns: the checkpoint passes, the journal-mode switch is
/// refused (`database is locked`), and releasing the handle makes the same file sealable.
#[test]
fn seal_refuses_a_stage_whose_journal_mode_cannot_switch() {
    let dir = TempDir::new("seal-journal-mode");
    let staged = stage(&dir.join("graph.db"), false).expect("stage an empty file");
    // Still open, so the file is in WAL and a second connection cannot switch it to DELETE. The
    // stage has to hold at least one committed page: with no `-wal` file at all there is nothing
    // for the mode switch to be refused over, and SQLite answers `delete`.
    let mut scope = begin_write(&staged.path).expect("open the stage writer");
    scope
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE probe (value TEXT); INSERT INTO probe VALUES ('x')")
                .map_err(|error| AppError::internal(format!("write the stage: {error}")))
        })
        .expect("commit a page into the WAL");
    assert!(
        sidecar(&staged.path, "-wal").exists(),
        "the WAL is what makes the switch refusable"
    );

    let error = seal(&staged.path).expect_err("an owned stage cannot be sealed");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains("journal_mode = DELETE"),
        "the refusal comes from the journal-mode step: {}",
        error.message
    );
    assert!(
        error.message.contains("database is locked"),
        "the observed value is in the message: {}",
        error.message
    );
    assert!(error.message.contains(&staged.path.display().to_string()));
    assert!(
        !error.message.contains("is busy"),
        "the checkpoint itself succeeded: {}",
        error.message
    );

    scope.close().expect("release the stage writer");
    let report = seal(&staged.path).expect("the released stage seals");
    assert_eq!(report.journal_mode, "delete");
    assert_eq!(report.synchronous, 2);
    assert_eq!(report.quick_check, vec!["ok".to_owned()]);
    assert_eq!(report.checkpoint.busy, 0);
}

/// A WAL another connection still holds is a `busy` checkpoint, reported with its number, and a
/// stage that is only busy for that reason seals once the reader lets go.
#[test]
fn seal_reports_a_busy_checkpoint() {
    let dir = TempDir::new("seal-busy");
    let staged = stage(&dir.join("graph.db"), false).expect("stage an empty file");
    let mut scope = begin_write(&staged.path).expect("open the stage writer");
    // Keep the frames in the log: the checkpoint must find something to refuse, and the
    // auto-checkpoint would otherwise empty it on commit.
    scope
        .conn()
        .connection()
        .execute_batch("PRAGMA wal_autocheckpoint = 0")
        .expect("disable the auto-checkpoint");
    scope
        .transaction(|tx| {
            tx.execute_batch("CREATE TABLE probe (value INTEGER)")
                .map_err(|error| AppError::internal(format!("create probe: {error}")))?;
            for value in 0..64 {
                tx.execute("INSERT INTO probe VALUES (?1)", [value])
                    .map_err(|error| AppError::internal(format!("insert: {error}")))?;
            }
            Ok(())
        })
        .expect("commit the rows");

    let reader = open_reader(&staged.path).expect("open a reader on the stage");
    reader
        .connection()
        .execute_batch("BEGIN")
        .expect("hold a read transaction");
    assert_eq!(
        one_i64(reader.connection(), "SELECT count(*) FROM probe"),
        64
    );

    let error = seal(&staged.path).expect_err("a reader holding the WAL makes the checkpoint busy");
    assert!(
        error
            .message
            .starts_with("wal_checkpoint(TRUNCATE) is busy ("),
        "{}",
        error.message
    );
    assert!(error.message.ends_with(&staged.path.display().to_string()));
    assert!(
        !error.message.contains("is busy (0)"),
        "busy 0 is the success case: {}",
        error.message
    );

    reader
        .connection()
        .execute_batch("ROLLBACK")
        .expect("release the read transaction");
    drop(reader);
    scope.close().expect("release the stage writer");
    seal(&staged.path).expect("the stage seals once it is uncontended");
}

/// `quick_check` refuses a structurally broken stage and reports the text SQLite observed, so the
/// corruption is diagnosable instead of a bare "seal failed".
#[test]
fn seal_reports_the_quick_check_text() {
    let dir = TempDir::new("seal-quick-check");
    let staged = stage(&dir.join("graph.db"), false).expect("stage an empty file");
    let mut scope = begin_write(&staged.path).expect("open the stage writer");
    migrate(scope.conn_mut().connection_mut()).expect("migrate the stage");
    scope
        .transaction(|tx| {
            tx.execute_batch(
                "INSERT INTO generation \
                 (id, seq, published_at, files_indexed, nodes, edges, duration_ms, mode, status) \
                 VALUES ('gen-new', 1, '2026-01-01T00:00:00.000Z', 1, 2, 1, 3, 'full', 'ok')",
            )
            .map_err(|error| AppError::internal(format!("write a generation: {error}")))
        })
        .expect("commit the generation");
    scope.close().expect("release the stage writer");
    assert_eq!(
        observed_quick_check(&staged.path),
        vec!["ok".to_owned()],
        "the stage is sound before the corruption"
    );

    zero_page_two(&staged.path);
    let observed = observed_quick_check(&staged.path);
    assert_ne!(observed, vec!["ok".to_owned()], "the corruption is visible");

    let error = seal(&staged.path).expect_err("a corrupt stage does not seal");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert_eq!(
        error.message,
        format!(
            "quick_check failed on {}: {}",
            staged.path.display(),
            observed.join("; ")
        ),
        "the message carries the observed text"
    );
    assert!(error.hint.is_some());
}

/// `discard` removes the stage and its sidecars and never the live file, which is the whole reason
/// the two paths are one infix apart.
#[test]
fn discard_removes_the_stage_and_its_sidecars_but_never_the_live_file() {
    let dir = TempDir::new("discard");
    let live = populated_live(&dir);
    let before = fs::read(&live).expect("read the live generation");

    let staged = stage(&live, true).expect("stage from the live graph");
    for suffix in ["-wal", "-shm", "-journal"] {
        fs::write(sidecar(&staged.path, suffix), b"stale").expect("write a stale sidecar");
    }

    discard(&staged.path).expect("discard the stage");

    assert!(!staged.path.exists(), "the stage is gone");
    assert!(
        sidecars_beside(&staged.path).is_empty(),
        "no sidecar survives"
    );
    assert_eq!(
        fs::read(&live).expect("re-read the live generation"),
        before,
        "discard only deletes the path it was given"
    );
    assert_eq!(stage_leftovers(&dir.0), Vec::<String>::new());
}

/// A publisher only accepts a stage: a path without the infix, or a stage that is gone, is refused
/// before anything is renamed.
#[test]
fn publish_refuses_a_path_that_is_not_a_stage() {
    let dir = TempDir::new("publish-refuses");
    let plain = dir.join("graph.db");
    fs::write(&plain, b"not a stage").expect("write a plain file");

    let error = publish(&plain, &dir.join("other.db")).expect_err("a plain path is not a stage");
    assert!(
        error.message.contains("not a stage file"),
        "{}",
        error.message
    );
    assert!(error.hint.is_some());

    let staged = stage_path(&dir.join("graph.db")).expect("stage path");
    let error = publish(&staged, &dir.join("graph.db")).expect_err("a missing stage is refused");
    assert!(
        error.message.contains("does not exist"),
        "{}",
        error.message
    );
}

/// A callback that fails leaves the live file untouched and no stage behind — the guarantee
/// `with_stage` exists for.
#[test]
fn a_failing_callback_leaves_the_live_file_untouched() {
    let dir = TempDir::new("with-stage-error");
    let live = populated_live(&dir);
    let before = fs::read(&live).expect("read the live generation");

    let error = with_stage::<(), _>(&live, true, |staged| {
        let mut scope = begin_write(&staged.path).expect("open the stage writer");
        migrate(scope.conn_mut().connection_mut()).expect("migrate the stage");
        scope
            .transaction(|tx| {
                tx.execute_batch("DELETE FROM generation")
                    .map_err(|error| AppError::internal(format!("delete: {error}")))
            })
            .expect("commit a half-built graph");
        scope.close().expect("release the stage writer");
        Err(AppError::internal("the load failed"))
    })
    .expect_err("a failing callback does not publish");

    assert_eq!(error.message, "the load failed");
    assert_eq!(
        fs::read(&live).expect("re-read the live generation"),
        before,
        "the live file's bytes are unchanged"
    );
    assert_eq!(
        stage_leftovers(&dir.0),
        Vec::<String>::new(),
        "the stage is discarded"
    );

    // And the graph a reader would open is still the previous one.
    let reader = open_reader(&live).expect("read the live graph");
    assert_eq!(
        one_i64(reader.connection(), "SELECT count(*) FROM generation"),
        1,
        "the previous generation is still the one being served"
    );
}

/// A callback that panics is cleaned up too: the `Drop` guard is the only mechanism that runs on
/// an unwind, which is why the discard path is a `Drop` and not a completion of the happy path.
#[test]
fn a_panicking_callback_leaves_the_live_file_untouched() {
    let dir = TempDir::new("with-stage-panic");
    let live = populated_live(&dir);
    let before = fs::read(&live).expect("read the live generation");

    // The panic this test expects would otherwise print a backtrace and be mistaken for a failing
    // suite, so the hook is replaced for the duration of the catch and restored immediately. The
    // hook is process-wide; the window is one call, and a panic elsewhere in the suite is still a
    // failure, only a quieter one.
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(|_| {}));
    let caught = panic::catch_unwind(AssertUnwindSafe(|| {
        with_stage::<(), _>(&live, true, |staged| {
            let mut scope = begin_write(&staged.path).expect("open the stage writer");
            migrate(scope.conn_mut().connection_mut()).expect("migrate the stage");
            scope
                .transaction(|tx| {
                    tx.execute_batch("DELETE FROM generation")
                        .map_err(|error| AppError::internal(format!("delete: {error}")))
                })
                .expect("commit a half-built graph");
            panic!("the load died mid-write");
        })
    }));
    panic::set_hook(previous_hook);

    let payload = caught.expect_err("the panic reaches the caller, it is not swallowed");
    let message = payload
        .downcast_ref::<&str>()
        .copied()
        .map(str::to_owned)
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    assert_eq!(message, "the load died mid-write");

    assert_eq!(
        fs::read(&live).expect("re-read the live generation"),
        before,
        "the live file's bytes are unchanged"
    );
    assert_eq!(
        stage_leftovers(&dir.0),
        Vec::<String>::new(),
        "the drop guard discarded the stage during the unwind"
    );
}

/// The other Done-when: a seal that fails leaves the live file byte-identical.
///
/// The Done-when asks for a `sha256` comparison. `sha2` is not a dependency of this crate and a
/// hashing crate added for one test would be the wrong dependency, so the bytes are compared
/// directly — equal length plus equal content, which is a strictly stronger claim than two equal
/// digests.
#[test]
fn a_seal_failure_leaves_the_live_file_byte_identical() {
    let dir = TempDir::new("seal-failure");
    let live = populated_live(&dir);
    let before = fs::read(&live).expect("read the live generation");

    let error = with_stage(&live, true, |staged| {
        let mut scope = begin_write(&staged.path).expect("open the stage writer");
        migrate(scope.conn_mut().connection_mut()).expect("migrate the stage");
        scope.close().expect("release the stage writer");
        // Unpublishable by construction: SQLite will not even read it.
        zero_header(&staged.path);
        Ok(())
    })
    .expect_err("a broken stage does not publish");

    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains(&"file is not a database".to_owned())
            || error.message.contains("quick_check"),
        "the failure names what SQLite observed: {}",
        error.message
    );

    let after = fs::read(&live).expect("re-read the live generation");
    assert_eq!(after.len(), before.len(), "same length");
    assert_eq!(after, before, "same bytes");
    assert_eq!(
        stage_leftovers(&dir.0),
        Vec::<String>::new(),
        "nothing was left beside the live file"
    );
}

/// The happy path end to end: the callback's value comes through, the stage becomes the live file,
/// and nothing about the stage survives it.
#[test]
fn with_stage_publishes_when_the_callback_succeeds() {
    let dir = TempDir::new("with-stage-ok");
    let live = populated_live(&dir);

    let written = with_stage(&live, true, |staged| {
        assert_eq!(staged.seeded_from, SeededFrom::Live);
        let mut scope = begin_write(&staged.path).expect("open the stage writer");
        migrate(scope.conn_mut().connection_mut()).expect("migrate the stage");
        let rows = scope
            .transaction(|tx| {
                tx.execute_batch(
                    "DELETE FROM generation;\n\
                     INSERT INTO generation \
                     (id, seq, published_at, files_indexed, nodes, edges, duration_ms, mode, status) \
                     VALUES ('gen-new', 2, '2026-02-02T00:00:00.000Z', 5, 20, 9, 21, 'full', 'ok')",
                )
                .map_err(|error| AppError::internal(format!("write the next generation: {error}")))?;
                Ok(1)
            })
            .expect("commit the next generation");
        scope.close().expect("release the stage writer");
        Ok(rows)
    })
    .expect("a successful callback publishes");

    assert_eq!(written, 1, "the callback's value is passed through");

    let published = open_reader(&live).expect("read the published graph");
    assert_eq!(
        one_i64(published.connection(), "SELECT count(*) FROM generation"),
        1
    );
    let id = published
        .connection()
        .query_row("SELECT id FROM generation", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("read the published generation id");
    assert_eq!(id, "gen-new", "the new generation is the one being served");
    assert_eq!(
        pragma_text(published.connection(), "journal_mode"),
        "delete",
        "what was published is a sealed file"
    );
    assert_eq!(stage_leftovers(&dir.0), Vec::<String>::new());
    assert!(sidecars_beside(&live).is_empty());
}
