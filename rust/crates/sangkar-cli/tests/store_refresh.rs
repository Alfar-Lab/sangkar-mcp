//! The store lifecycle on real files (`DATABASE.md` §5), ported from the unit half of
//! `tests/store-refresh.test.ts`.
//!
//! The decision under test is a fact about a file — a `VACUUM INTO`, a seal, a `rename` — so every
//! case drives the real publish protocol over its own temporary data directory, never a stubbed
//! `stat`: a stubbed metadata call would prove the stub. Each test is one branch of that decision:
//! an untouched file (no re-open), a published second generation (a swap the reader must re-read),
//! a deleted file (`Missing`, the handle keeps its generation), a graph from a newer build (a
//! refusal that is not a swap), a missing database (`bootstrapping`, not an error), and the two
//! fingerprint cases that pin "same size" and "same path" as non-answers.
//!
//! The row payloads are `INSERT`s this suite wrote, not an indexer run: the repository's own
//! `.sangkar/graph.db` belongs to the daemon that rewrites it, so every database here lives under
//! `std::env::temp_dir()` and is removed with the directory.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sangkar_cli::store_lifecycle::{
    FileFingerprint, RefreshOutcome, Store, StoreFailure, fingerprint_of, open_store,
    refresh_store, same_file,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::schema_version;
use sangkar_store::publish::{begin_write, with_stage};
use sangkar_store::read::reader::GraphReader;

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
            "sangkar-refresh-{label}-{}-{nanos}-{}",
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

/// Publishes one generation the way the worker does: a stage, `migrate` (the only schema author a
/// fresh data dir has), one batch writing the generation row plus the `meta` keys a reader reads
/// back, then `with_stage`'s seal and rename over the live file.
///
/// `options` is for the cases that need the file to differ in a way the graph does not: padding a
/// value makes the publish demonstrably larger, and a schema offset makes it unservable. Both land
/// in the batch that already writes `meta`, so a stage is never half of either. The returned
/// fingerprint is the publication this call made, i.e. what a reader of the live file must record.
fn publish(live: &Path, generation: &str, seq: i64, options: Publish) -> FileFingerprint {
    with_stage(live, seq > 1, |staged| {
        let mut scope = begin_write(&staged.path)?;
        migrate(scope.conn_mut().connection_mut())?;
        let padding = if options.padding_bytes == 0 {
            String::new()
        } else {
            format!(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('test.padding', '{}');",
                "x".repeat(options.padding_bytes)
            )
        };
        let sql = format!(
            "INSERT OR REPLACE INTO generation \
               (id, seq, published_at, base_commit, worktree_dirty, files_indexed, nodes, edges, \
                duration_ms, mode, status) \
             VALUES ('{generation}', {seq}, '2026-11-19T09:00:00.000Z', NULL, 0, 2, 3, 1, 12, \
                'full', 'ok');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', '{generation}');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('project_name', 'refresh-fixture');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '{}');\
             {padding}",
            schema_version() + options.schema_offset,
        );
        scope.transaction(|tx| {
            tx.execute_batch(&sql)
                .map_err(|error| AppError::internal(format!("writing {generation}: {error}")))
        })?;
        scope.close()
    })
    .expect("publish a generation");

    fingerprint_of(live)
        .expect("fingerprint the published graph")
        .expect("the published graph exists")
}

/// What one publish differs in, for the cases that need the file to differ in a way the graph does
/// not.
#[derive(Clone, Copy, Default)]
struct Publish {
    /// `1` models a graph written by a build that ships one more migration than this one.
    schema_offset: i64,
    /// Bytes of one padding `meta` value, so the published file is provably a different size.
    padding_bytes: usize,
}

/// The generation a reader of `store` reports: what a refreshed handle must have read.
fn generation_of(store: &Store) -> String {
    store
        .reader()
        .meta_for()
        .expect("read the graph's meta")
        .generation
        .unwrap_or_default()
}

#[test]
fn a_publish_renames_a_new_inode_over_the_live_path_and_the_refresh_serves_it() {
    let dir = TempDir::new("newer");
    let live = dir.join("graph.db");
    let first = publish(&live, "g-b00001", 1, Publish::default());

    let mut store = open_store(dir.path()).expect("open the first generation");
    assert_eq!(generation_of(&store), "g-b00001");
    assert_eq!(store.fingerprint(), &first);
    assert_eq!(
        store.db_size_bytes(),
        first.size,
        "the size is the file opened"
    );
    assert_eq!(store.schema_version(), schema_version());

    // The publish writes a stage and renames it over the live path: same path, other file, and a
    // padded one so the two sizes provably differ. That is the event a path comparison would report
    // as "unchanged", so the new print must differ in the inode while the handle still names the
    // one path.
    let second = publish(
        &live,
        "g-b00002",
        2,
        Publish {
            schema_offset: 0,
            padding_bytes: 64 * 1024,
        },
    );
    assert_ne!(first.inode, second.inode, "a publish replaces the inode");
    assert!(
        second.size > first.size,
        "the padded generation is a bigger file"
    );
    assert_eq!(
        store.fingerprint().path,
        live,
        "the handle still names that path"
    );

    assert_eq!(refresh_store(&mut store), RefreshOutcome::Reopened);
    assert_eq!(generation_of(&store), "g-b00002");
    assert_eq!(store.fingerprint(), &second);
    assert_eq!(store.db_size_bytes(), second.size);
}

#[test]
fn an_untouched_file_is_unchanged_within_a_generous_bound_and_keeps_its_reader() {
    let dir = TempDir::new("unchanged");
    let live = dir.join("graph.db");
    publish(&live, "g-aaaaaa", 1, Publish::default());

    let mut store = open_store(dir.path()).expect("open the only generation");
    let reader = std::ptr::from_ref(store.reader());
    let started = Instant::now();
    let outcome = refresh_store(&mut store);
    let elapsed = started.elapsed();

    assert_eq!(outcome, RefreshOutcome::Unchanged);
    // Identity, not equality: an unchanged file must not open a second handle. This is the
    // assertion a re-open fails on every machine, whatever the clock says.
    assert!(
        std::ptr::eq(reader, std::ptr::from_ref(store.reader())),
        "an unchanged file must not be re-opened"
    );
    assert_eq!(generation_of(&store), "g-aaaaaa");
    // A refresh is one `metadata` and one comparison. The contract's 1 ms is a DoS-shaped bound, so
    // it is asserted generously: a refresh that scanned or re-opened would blow through it, while a
    // loaded machine scheduling this thread does not.
    assert!(
        elapsed < Duration::from_millis(25),
        "an unchanged refresh took {elapsed:?}"
    );
}

#[test]
fn a_deleted_file_reports_missing_and_the_handle_keeps_its_generation() {
    let dir = TempDir::new("vanished");
    let live = dir.join("graph.db");
    publish(&live, "g-f00001", 1, Publish::default());

    let mut store = open_store(dir.path()).expect("open the only generation");
    fs::remove_file(&live).expect("delete the graph");

    // A vanished file is not a newer generation: the handle keeps serving the file it opened, which
    // the OS keeps alive for as long as this connection holds it.
    assert_eq!(refresh_store(&mut store), RefreshOutcome::Missing);
    assert_eq!(generation_of(&store), "g-f00001");
}

#[test]
fn a_graph_written_by_a_newer_build_is_refused_with_schema_too_new() {
    let dir = TempDir::new("schema");
    let live = dir.join("graph.db");
    publish(&live, "g-c00001", 1, Publish::default());
    publish(
        &live,
        "g-c00002",
        2,
        Publish {
            schema_offset: 1,
            padding_bytes: 0,
        },
    );

    let failure = open_store(dir.path()).expect_err("a newer schema is refused");
    assert_eq!(
        failure,
        StoreFailure::SchemaTooNew {
            found: schema_version() + 1,
            built: schema_version(),
        }
    );

    // The lifecycle keeps the decision; the wire keeps the closed code and the hint that names
    // which side is behind.
    assert_eq!(failure.code(), ErrorCode::SCHEMA_TOO_NEW);
    assert_eq!(failure.code(), "schema_too_new");
    let error = failure.error().expect("a refusal carries an error");
    assert_eq!(error.code, ErrorCode::SCHEMA_TOO_NEW);
    assert!(
        error
            .message
            .contains(&format!("v{}", schema_version() + 1)),
        "the message names the version found: {}",
        error.message
    );
    let hint = error.hint.expect("the refusal carries a hint");
    assert!(
        hint.contains(&format!("v{}", schema_version())),
        "the hint names the indexer version: {hint}"
    );
}

#[test]
fn a_newer_generation_that_cannot_be_served_leaves_the_previous_reader_in_place() {
    let dir = TempDir::new("schema-swap");
    let live = dir.join("graph.db");
    publish(&live, "g-c00003", 1, Publish::default());

    let mut store = open_store(dir.path()).expect("open the servable generation");
    let opened = store.fingerprint().clone();

    // A rename-over publish, so the file *is* a newer generation — and one this build must not
    // serve. The refusal is not a swap: dropping the old reader would turn a re-index into an
    // outage, so the previous handle keeps answering and keeps describing the file it opened.
    publish(
        &live,
        "g-c00004",
        2,
        Publish {
            schema_offset: 1,
            padding_bytes: 0,
        },
    );
    assert_eq!(refresh_store(&mut store), RefreshOutcome::Unchanged);
    assert_eq!(generation_of(&store), "g-c00003");
    assert_eq!(store.fingerprint(), &opened);

    // `Unchanged` alone would be a lie: a newer generation exists and this handle knows it. The
    // disclosure is `refreshStore`'s catch branch in `src/index.ts`, which returns the old store
    // carrying `staleReason`, and it is what keeps the next answer honest instead of silently old.
    let reason = store
        .stale_reason()
        .expect("a detected-but-unservable publish is disclosed");
    assert!(
        reason.starts_with("a newer generation could not be opened"),
        "the disclosure says what happened: {reason}"
    );
    assert!(
        reason.contains(&format!("found: {}", schema_version() + 1)),
        "and names the version it found: {reason}"
    );
}

#[test]
fn a_missing_database_is_a_bootstrapping_answer_not_an_error() {
    let dir = TempDir::new("absent");
    let live = dir.join("graph.db");

    let failure = open_store(dir.path()).expect_err("nothing has been published");
    assert_eq!(
        fingerprint_of(&live).expect("stat the absent path"),
        None,
        "an absent file has no fingerprint"
    );
    assert_eq!(failure, StoreFailure::Missing);
    assert_eq!(failure.code(), ErrorCode::BOOTSTRAPPING);
    assert_eq!(failure.code(), "bootstrapping");
    assert_eq!(
        failure.error(),
        None,
        "a first session before the first publish is an answer, not a failure"
    );
    assert!(!live.exists(), "opening must not make a database appear");

    // A zero-byte file is the same answer: a stage that never got renamed into place.
    fs::write(&live, b"").expect("write an empty graph.db");
    assert_eq!(
        open_store(dir.path()).expect_err("an empty graph is not servable"),
        StoreFailure::Missing
    );

    // And the flow a server started before the first index goes through: the next open is ready.
    publish(&live, "g-e00001", 1, Publish::default());
    let store = open_store(dir.path()).expect("a publish makes the file servable");
    assert_eq!(generation_of(&store), "g-e00001");
}

#[test]
fn two_files_of_the_same_size_are_not_the_same_file() {
    let dir = TempDir::new("twins");
    let one = dir.join("one.db");
    let two = dir.join("two.db");
    let bytes = vec![0u8; 4096];
    fs::write(&one, &bytes).expect("write the first file");
    fs::write(&two, &bytes).expect("write the second file");

    let a = fingerprint_of(&one)
        .expect("fingerprint the first file")
        .expect("the first file exists");
    let b = fingerprint_of(&two)
        .expect("fingerprint the second file")
        .expect("the second file exists");

    assert_eq!(a.size, b.size, "the fixture must not differ in size");
    assert_ne!(a.inode, b.inode);
    assert!(
        !same_file(&a, &b),
        "a copy of the same size is a different publication"
    );
}

#[test]
fn same_file_compares_the_inode_and_never_the_path() {
    let first = FileFingerprint {
        path: PathBuf::from("/tmp/sangkar-one/graph.db"),
        inode: 41,
        size: 8192,
        mtime_ns: 1_700_000_000_000_000_000,
    };

    // The publish: same path, a new inode. Reading this as "unchanged" is the stale-payload bug.
    let replaced = FileFingerprint {
        inode: 42,
        ..first.clone()
    };
    assert!(!same_file(&first, &replaced));

    // The other direction: another spelling of one path is still one publication, because the path
    // is not part of the comparison at all.
    let respelled = FileFingerprint {
        path: PathBuf::from("/tmp/sangkar-one/./graph.db"),
        ..first.clone()
    };
    assert!(same_file(&first, &respelled));
}

#[test]
fn a_disclosure_is_cleared_once_a_later_publish_is_actually_served() {
    let dir = TempDir::new("disclosure-clears");
    let live = dir.join("graph.db");
    publish(&live, "g-d00001", 1, Publish::default());
    // Keep the servable bytes, so a good generation can be put back without republishing through
    // the too-new file (which `migrate` would refuse as a downgrade).
    let servable = dir.join("servable.db");
    fs::copy(&live, &servable).expect("keep a copy of the servable graph");

    let mut store = open_store(dir.path()).expect("open the servable generation");
    assert_eq!(store.stale_reason(), None, "a fresh handle is current");

    publish(
        &live,
        "g-d00002",
        2,
        Publish {
            schema_offset: 1,
            padding_bytes: 0,
        },
    );
    assert_eq!(refresh_store(&mut store), RefreshOutcome::Unchanged);
    assert!(
        store.stale_reason().is_some(),
        "the failed reopen is disclosed"
    );

    // The next publish that IS served replaces the handle wholesale, so the disclosure goes with
    // the generation it described: a marker that outlived its cause would be its own lie.
    fs::copy(&servable, &live).expect("put a servable generation back");
    assert_eq!(refresh_store(&mut store), RefreshOutcome::Reopened);
    assert_eq!(store.stale_reason(), None);
    assert_eq!(generation_of(&store), "g-d00001");
}
