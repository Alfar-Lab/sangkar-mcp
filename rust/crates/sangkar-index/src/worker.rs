//! The index worker: the one process that owns the graph's writer (`TECH-SPEC.md` §10.2).
//!
//! Ported from `src/indexer/worker.ts`. Its whole reporting surface is a status byte:
//! [`run_worker`] answers [`WorkerExit::Published`] (0), [`WorkerExit::Failed`] (1),
//! [`WorkerExit::NothingToDo`] (2) or [`WorkerExit::Locked`] (75), and writes nothing to stdout —
//! stdout is the MCP protocol (§11), so a child that printed a count would corrupt the session it
//! was spawned from. Everything else goes to stderr through [`sangkar_core::log`].
//!
//! An incremental run still writes the whole discovered unit set. [`persist_unit`] deletes and
//! re-inserts a file's nodes, and `edge.dst_id` cascades, so re-persisting only the changed files
//! would drop every unchanged caller's edge into a re-written file. The hash comparison therefore
//! decides *whether* to publish, not *how much*; the price is a full write per generation, and
//! [`restore_edges`] closes the remaining hole (a caller persisted before the file it calls has its
//! edge cascaded away when that file's nodes are replaced).
//!
//! A directory that is not a git repository is normal, not an error: change detection degrades to
//! the `file.sha256` comparison (`sangkar_domain::diff` reports `degraded`), and a first run over
//! such a tree publishes instead of failing.
//!
//! What differs from the TypeScript, and why:
//!
//! - the heartbeat is a thread parked on a channel rather than a `setInterval` handle, so stopping
//!   it is one send plus one join and no timer can outlive the run;
//! - the lock is a guard's `Drop` ([`HeldLock`]) instead of a `finally` block, which is the only
//!   shape Rust gives a release that has to happen on the panic path too;
//! - a current live database is left byte-for-byte alone ([`migrate_live`]): the store's only
//!   writable handle sets `journal_mode = WAL` on the file it opens, and a run whose answer is
//!   "nothing to do" must not rewrite the graph it just declared unchanged;
//! - the run's telemetry record is a documented no-op ([`record_index_run`]): the JSONL sink and
//!   the `telemetry` verb are phase 4, so the call site and the record's shape land here and the
//!   implementation does not.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log;
use sangkar_core::paths::normalize_rel;
use sangkar_domain::diff::{
    ChangeSet, ChangedSinceOptions, DigestSource, FileDigest, changed_since,
};
use sangkar_store::db::{Conn, open_reader, open_writer};
use sangkar_store::lock::{self, LockHandle, STALE_AFTER_MS};
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::{read_rebuild_required, read_schema_version, schema_version};
use sangkar_store::publish::{SeededFrom, begin_write, with_stage};
use sangkar_store::schema::{
    META_CONSECUTIVE_FAILURES, META_GENERATION, META_INDEXER_STATE, META_REBUILD_REQUIRED,
};
use sangkar_store::{OptionalExtension, Transaction, params};

use crate::discover::{DiscoverOptions, DiscoverResult, DiscoveredFile, discover};
use crate::extract::extract_file;
use crate::lang::registry::spec_for;
use crate::parse::{ParseResult, parse_file};
use crate::persist::finalize::{
    GenerationInput, GenerationResult, ProjectIdentity, finalize_generation,
};
use crate::persist::{
    NotIndexedEntry, NotIndexedReason, PENDING_GENERATION, ParseStatus, PersistUnit, persist_unit,
};
use crate::resolve::{BindResult, FileUnit, SymbolIndex, bind, file_unit};

/// `DATABASE.md` §3.0: one database per repo, inside the data dir.
const DATABASE_FILE: &str = "graph.db";

/// `DATABASE.md` §3.0: the data dir ignores itself, so a target repo's status stays clean and no
/// tracked file is ever touched. A single star, nothing else.
const DATA_DIR_GITIGNORE: &str = "*\n";

/// `meta.indexer_state`; `indexing` is the server's claim while a child is alive, not ours.
const STATE_IDLE: &str = "idle";
const STATE_FAILED: &str = "failed";

/// Four heartbeats per staleness window, so a slow run renews the lock it holds.
const HEARTBEAT_INTERVAL_MS: u64 = STALE_AFTER_MS / 4;

/// Bound on one git invocation; the same reasoning as `sangkar_domain::diff`'s bound, with the
/// worker's shorter number because identity capture is on the publish path, not on a tool call.
const GIT_TIMEOUT_MS: u64 = 10_000;

/// The argv every git read is prefixed with, copied verbatim from `diff.ts` through
/// `sangkar_domain::diff`: `--no-pager` and `--no-optional-locks` keep the target repo's config from
/// starting a pager or taking the index lock, the two `-c` overrides leave the child no credential
/// helper or askpass program to prompt through, and `-C <root>` is what makes a relative root
/// resolve the way the caller meant.
const GIT_PREFIX: [&str; 7] = [
    "--no-pager",
    "--no-optional-locks",
    "-c",
    "credential.helper=",
    "-c",
    "core.askpass=",
    "-C",
];

/// How often the wait loop asks whether git has exited. A git read answers in milliseconds, so the
/// cost is a handful of `try_wait` calls; the poll is what gives the timeout teeth without a
/// dependency.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long a pipe reader may take to report after the child was reaped. A `git` that exited has
/// closed its own descriptors, so the collection is immediate unless a grandchild is holding the
/// pipe; the grace turns that pathological case into an empty answer instead of a hang.
const COLLECT_GRACE: Duration = Duration::from_secs(1);

/// The one `meta` upsert; every state write below goes through it.
const META_UPSERT: &str = "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value";

/// Records the live nodes of one removed path as tombstones *before* the file row is dropped: the
/// cascade would take the node rows the statement reads, and a tombstone carries no foreign key
/// precisely so it can outlive the file (`DATABASE.md` §3.3).
const TOMBSTONE_FILE: &str = concat!(
    "INSERT INTO tombstone (qn, kind, file_id, last_seen_generation, removed_in_generation,",
    " had_callers) SELECT n.qn, n.kind, n.file_id, ?1, ?2, (SELECT count(*) FROM edge e",
    " WHERE e.dst_id = n.id AND e.kind NOT IN ('contains', 'tested_by')) FROM node n",
    " JOIN file f ON f.id = n.file_id WHERE f.path = ?3",
    " ON CONFLICT(qn) DO UPDATE SET kind = excluded.kind, file_id = excluded.file_id,",
    " last_seen_generation = excluded.last_seen_generation,",
    " removed_in_generation = excluded.removed_in_generation,",
    " had_callers = excluded.had_callers",
);

/// The `file` row, including the ones whose parse failed: the row is the staleness manifest, so a
/// file that cannot be parsed is still a file we have seen.
const UPSERT_FILE: &str = concat!(
    "INSERT INTO file (path, language, sha256, size, line_count, parse_status,",
    " parse_error_ranges, indexed_generation) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    " ON CONFLICT(path) DO UPDATE SET language = excluded.language, sha256 = excluded.sha256,",
    " size = excluded.size, line_count = excluded.line_count,",
    " parse_status = excluded.parse_status,",
    " parse_error_ranges = excluded.parse_error_ranges,",
    " indexed_generation = excluded.indexed_generation",
);

const DROP_FILE: &str = "DELETE FROM file WHERE path = ?1";
const DROP_NODES: &str = "DELETE FROM node WHERE file_id = (SELECT id FROM file WHERE path = ?1)";
const DROP_EDGES: &str = "DELETE FROM edge WHERE file_id = (SELECT id FROM file WHERE path = ?1)";
const DROP_UNRESOLVED: &str =
    "DELETE FROM unresolved WHERE file_id = (SELECT id FROM file WHERE path = ?1)";
const SELECT_NODE_IDS: &str = "SELECT id, qn FROM node";
const SELECT_FILE_IDS: &str = "SELECT id, path FROM file";
const INSERT_EDGE: &str = concat!(
    "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)",
    " VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT DO NOTHING",
);
const CLEAR_COVERAGE: &str = "DELETE FROM not_indexed";
const INSERT_COVERAGE: &str = concat!(
    "INSERT INTO not_indexed (path, reason, detail, language) VALUES (?1, ?2, ?3, ?4)",
    " ON CONFLICT DO NOTHING",
);
const SELECT_FILE_DIGESTS: &str = "SELECT path, sha256 FROM file ORDER BY path";
const SELECT_PREVIOUS_BASE_COMMIT: &str =
    "SELECT base_commit FROM generation ORDER BY seq DESC LIMIT 1";
const SELECT_PUBLISHED_GENERATION: &str =
    "SELECT 1 FROM generation WHERE status = 'published' LIMIT 1";
const SELECT_META: &str = "SELECT value FROM meta WHERE key = ?1";
const SELECT_META_TABLE: &str =
    "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'meta'";

/// `generation.mode`; `full` rebuilds from an empty stage, `fast` publishes incrementally.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerMode {
    Full,
    Fast,
}

impl WorkerMode {
    /// The spelling `generation.mode` stores, which is the closed set `finalize_generation` checks.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Fast => "fast",
        }
    }
}

impl std::fmt::Display for WorkerMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The four outcomes, as exit codes. 75 is `EX_TEMPFAIL`, the code `code-review-graph` uses for
/// "another indexer holds the write lock" — a contended caller retries instead of failing hard.
///
/// The discriminants are the contract, not a detail: the watcher compares the child's exit status
/// against them (`crate::watch`), and a shell reads the same numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum WorkerExit {
    Published = 0,
    Failed = 1,
    NothingToDo = 2,
    Locked = 75,
}

impl WorkerExit {
    /// The process exit code, for the verb that turns this answer into a status.
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

impl std::fmt::Display for WorkerExit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.code())
    }
}

/// Everything [`run_worker`] needs. Built as a struct literal with `..WorkerOptions::default()`,
/// because only the two roots are ever required (the same shape as `crate::watch::WatchOptions`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerOptions {
    /// The working tree to index; absolute, or resolvable against the caller's cwd.
    pub repo_root: PathBuf,
    /// Where `graph.db`, `write.lock` and the data dir's `.gitignore` live.
    pub data_dir: PathBuf,
    /// Rebuild from an empty stage and publish even when the hashes say nothing changed.
    pub force: bool,
    /// `generation.mode`; defaults to `full` on a run that is forced and `fast` otherwise.
    pub mode: Option<WorkerMode>,
}

/// What went past in one run, counted from facts the run already holds: the walk, the change plan,
/// the parse results and the bind result. Read once, on the way out, so the record describes the
/// same numbers the generation was written from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RunSummary {
    files_seen: usize,
    files_indexed: usize,
    files_skipped: usize,
    files_failed: usize,
    nodes_by_kind: BTreeMap<String, usize>,
    edges_by_kind_tier: BTreeMap<String, usize>,
    unresolved_count: usize,
    languages: BTreeMap<String, usize>,
    files_changed_in_run: usize,
}

/// The one telemetry record a run produces (`TELEMETRY.md` §`index_run`), filled from the numbers
/// [`RunSummary`] carries plus the outcomes the exit path already decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexRun {
    pub mode: WorkerMode,
    pub exit: WorkerExit,
    pub duration_ms: i64,
    pub was_forced: bool,
    pub files_seen: usize,
    pub files_indexed: usize,
    pub files_skipped: usize,
    pub files_failed: usize,
    pub nodes_by_kind: BTreeMap<String, usize>,
    pub edges_by_kind_tier: BTreeMap<String, usize>,
    pub unresolved_count: usize,
    pub languages: BTreeMap<String, usize>,
    pub db_size_bytes: u64,
    pub files_changed_in_run: usize,
}

/// One run's fixed inputs, assembled after the lock is held and the schema is current.
struct RunContext {
    repo_root: PathBuf,
    live_path: PathBuf,
    force: bool,
    mode: WorkerMode,
    rebuild_required: bool,
    started_at: Instant,
}

/// What the change comparison decided: which paths to write, and whether there is anything to do.
struct ChangePlan {
    changed: Vec<String>,
    removed: Vec<String>,
    nothing_to_do: bool,
}

/// The working tree's identity at index time, as far as git can tell it.
struct Revision {
    /// `HEAD`, or `None` when the tree is not a git repository (or git is unusable).
    head: Option<String>,
    /// `true` only when git answered and reported pending changes to tracked files.
    dirty: bool,
}

/// One discovered file, parsed or not, with the manifest facts its `file` row records.
struct ParsedFile {
    file: DiscoveredFile,
    line_count: i64,
    parse_status: ParseStatus,
    error_ranges: Vec<String>,
    /// The coverage row a failed parse contributes, or `None` when the file parsed.
    failure: Option<NotIndexedEntry>,
}

/// What one parse pass produced: the manifest facts for every discovered file, and the units the
/// bind pass and the write phase consume. The units are only the files that produced a tree, in
/// discovery order.
struct ParsePass {
    files: Vec<ParsedFile>,
    units: Vec<FileUnit>,
}

/// Everything one generation's transaction needs.
struct GenerationWrite<'a> {
    pass: ParsePass,
    removed: &'a [String],
    discovered: &'a DiscoverResult,
    resolved: &'a BindResult,
    generation_id: &'a str,
    revision: &'a Revision,
    identity: &'a ProjectIdentity,
    mode: WorkerMode,
    duration_ms: i64,
    rebuild_required: bool,
}

/// What one attempt settled on, whichever code it ended with.
struct Outcome {
    exit: WorkerExit,
    mode: WorkerMode,
    force: bool,
}

/// What [`migrate_live`] had to say: the one fact the run must honour.
struct Migration {
    rebuild_required: bool,
}

/// The index run: lock, data dir, schema, change plan, stage, publish — or a failure code with the
/// reason on stderr. Never panics for a repository-level problem; a `Result::Err` becomes exit 1
/// with the failure recorded in `meta`, because the caller reads the code and not a struct.
#[must_use]
pub fn run_worker(options: &WorkerOptions) -> WorkerExit {
    let repo_root = absolute(&options.repo_root);
    let data_dir = absolute(&options.data_dir);
    let live_path = data_dir.join(DATABASE_FILE);
    let started_at = Instant::now();
    let mut summary = RunSummary::default();

    // What a path that leaves before {@link run_locked} resolves the mode reports: exit 1, which is
    // what a throw out of the setup means.
    let mut outcome = Outcome {
        exit: WorkerExit::Failed,
        mode: options.mode.unwrap_or(WorkerMode::Full),
        force: options.force,
    };
    if let Err(error) = run_locked(
        options,
        &repo_root,
        &data_dir,
        started_at,
        &mut summary,
        &mut outcome,
    ) {
        log::error(
            "worker.failed",
            &[
                ("repoRoot", repo_root.display().to_string().into()),
                ("dataDir", data_dir.display().to_string().into()),
                ("reason", error.message.clone().into()),
                ("code", error.code.as_str().into()),
            ],
        );
        record_failure(&live_path, &error);
        // The mode and the forced flag keep whatever the attempt had already resolved, so the record
        // describes the run that failed rather than the options it was asked with.
        outcome.exit = WorkerExit::Failed;
    }

    // Recorded only after the lock is gone: a slow sink must not hold the graph's write lock. This
    // is the run's single exit path, so exactly one record exists per run.
    record_index_run(&IndexRun {
        mode: outcome.mode,
        exit: outcome.exit,
        duration_ms: elapsed_millis(started_at),
        was_forced: outcome.force,
        files_seen: summary.files_seen,
        files_indexed: summary.files_indexed,
        files_skipped: summary.files_skipped,
        files_failed: summary.files_failed,
        nodes_by_kind: summary.nodes_by_kind,
        edges_by_kind_tier: summary.edges_by_kind_tier,
        unresolved_count: summary.unresolved_count,
        languages: summary.languages,
        db_size_bytes: db_size_of(&live_path),
        files_changed_in_run: summary.files_changed_in_run,
    });

    outcome.exit
}

/// The one seam this phase cannot fill: `TELEMETRY.md`'s `index_run` record goes here, and TASK-054
/// (phase 4) supplies the JSONL sink, its privacy filter and the verb that reads it. The call site
/// is the part that had to land now, so a later phase adds a body instead of a new call path.
///
/// Deliberately not an `Option<Telemetry>` field: the worker owns no sink, and a run with no record
/// to write is the only shape available until the sink exists.
fn record_index_run(_run: &IndexRun) {}

/// Everything that happens while the lock is held. The guard's `Drop` releases the lock on every
/// path, the panic path included, which is the TypeScript's `finally`.
///
/// `outcome` is mutated rather than returned so a failure keeps the mode and the forced flag the
/// attempt had already resolved: the record describes the run that failed, not the options it was
/// asked with.
fn run_locked(
    options: &WorkerOptions,
    repo_root: &Path,
    data_dir: &Path,
    started_at: Instant,
    summary: &mut RunSummary,
    outcome: &mut Outcome,
) -> Result<(), AppError> {
    // The lock is a file *in* the data dir, so the directory has to exist before it can be taken.
    fs::create_dir_all(data_dir).map_err(|error| {
        AppError::internal(format!(
            "cannot create the data directory {}: {error}",
            data_dir.display()
        ))
        .with_hint("point --data-dir at a writable directory")
    })?;

    let Some(lock) = HeldLock::take(data_dir)? else {
        outcome.exit = WorkerExit::Locked;
        return Ok(());
    };

    ensure_data_dir_ignore_file(data_dir)?;
    let migrated = migrate_live(&data_dir.join(DATABASE_FILE))?;
    // A store that never published has nothing to diff against: a "fast" label on a first build
    // would misdescribe both the generation row and the run's record.
    let first_build = !has_published_generation(&data_dir.join(DATABASE_FILE))?;
    let force = options.force || migrated.rebuild_required || first_build;
    let mode = options.mode.unwrap_or(if force {
        WorkerMode::Full
    } else {
        WorkerMode::Fast
    });

    outcome.mode = mode;
    outcome.force = force;
    let context = RunContext {
        repo_root: repo_root.to_path_buf(),
        live_path: data_dir.join(DATABASE_FILE),
        force,
        mode,
        rebuild_required: migrated.rebuild_required,
        started_at,
    };
    outcome.exit = index_graph(&context, summary)?;
    // The answer is settled; the lock goes before the record is written, so a contended caller is
    // not made to wait for a telemetry sink.
    drop(lock);
    Ok(())
}

/// The write lock, or `None` when another indexer holds it — the one refusal this run reports as
/// exit 75 instead of failing. Any other acquisition error is a real failure and is propagated.
///
/// The handle lives in an [`Arc`] because the heartbeat thread holds the same lock: a copy would be
/// a second handle whose `release` could remove someone else's file.
struct HeldLock {
    handle: Arc<LockHandle>,
    beat: Heartbeat,
}

impl HeldLock {
    fn take(data_dir: &Path) -> Result<Option<Self>, AppError> {
        match lock::acquire_lock(data_dir) {
            Ok(handle) => {
                let handle = Arc::new(handle);
                let beat = Heartbeat::start(Arc::clone(&handle));
                Ok(Some(Self { handle, beat }))
            }
            Err(error) if error.code == ErrorCode::INDEX_LOCKED => {
                log::warn(
                    "worker.locked",
                    &[
                        ("dataDir", data_dir.display().to_string().into()),
                        ("pid", std::process::id().into()),
                        ("reason", error.message.into()),
                    ],
                );
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        // The beat stops first: a refresh after the release would touch a file this run no longer
        // owns, and the store's `release` only removes a lock whose recorded pid is still ours.
        self.beat.stop();
        if let Err(error) = lock::release(&self.handle) {
            // The run's own outcome is already decided; a lock that cannot be removed is a warning.
            log::error(
                "worker.lock_release_failed",
                &[
                    ("path", self.handle.path().display().to_string().into()),
                    ("reason", error.message.into()),
                ],
            );
        }
    }
}

/// Refreshes the held lock's mtime every [`HEARTBEAT_INTERVAL_MS`] while the run works.
///
/// A thread parked on a channel rather than a timer: the receive times out at the interval, so one
/// send wakes it immediately, and [`Heartbeat::stop`] joining it is what guarantees no refresh
/// outlives the run.
struct Heartbeat {
    stop: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl Heartbeat {
    fn start(handle: Arc<LockHandle>) -> Self {
        let (stop, signal) = mpsc::channel::<()>();
        let worker = thread::spawn(move || {
            loop {
                match signal.recv_timeout(Duration::from_millis(HEARTBEAT_INTERVAL_MS)) {
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(error) = lock::heartbeat(&handle) {
                            // A beat that fails is not the run's outcome: the cost of an unrefreshed
                            // lock is that another indexer may reclaim it, and that is the store's
                            // rule to enforce, not this module's to pre-empt.
                            log::warn(
                                "worker.heartbeat_failed",
                                &[
                                    ("path", handle.path().display().to_string().into()),
                                    ("reason", error.message.into()),
                                ],
                            );
                        }
                    }
                    // A send is the stop signal; a dropped sender means nobody can ask again.
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }

    /// Signals the thread and waits for it. Idempotent, so a failure path may stop a beat that has
    /// already stopped, and a thread that panicked is joined rather than waited on forever.
    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Whether this store has ever published a generation. A store that has not is a first build: there
/// is nothing to diff against, so the run is `full` and forced even though the change plan would
/// only ever say "everything changed". [`migrate_live`] has just made the file readable.
fn has_published_generation(live_path: &Path) -> Result<bool, AppError> {
    let live = open_reader(live_path)?;
    let found = live
        .connection()
        .query_row(SELECT_PUBLISHED_GENERATION, [], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(|error| internal("reading the published generation", error));
    live.close()?;
    Ok(found?.is_some())
}

/// The published graph's size, or 0 when this run never created one.
fn db_size_of(live_path: &Path) -> u64 {
    match fs::metadata(live_path) {
        Ok(metadata) => metadata.len(),
        Err(_) => 0,
    }
}

/// Writes the data dir's own `.gitignore`. A file that already holds exactly a single star is left
/// alone; anything else is replaced, because the data dir must never appear in the target repo's
/// status (`DATABASE.md` §3.0).
fn ensure_data_dir_ignore_file(data_dir: &Path) -> Result<(), AppError> {
    let path = data_dir.join(".gitignore");
    match fs::read(&path) {
        Ok(bytes) if bytes == DATA_DIR_GITIGNORE.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(
                AppError::internal(format!("cannot read {}: {error}", path.display()))
                    .with_hint("the data directory must be readable by the indexer"),
            );
        }
    }
    fs::write(&path, DATA_DIR_GITIGNORE).map_err(|error| {
        AppError::internal(format!("cannot write {}: {error}", path.display()))
            .with_hint("the data directory must be writable by the indexer")
    })
}

/// Brings the live database to the current schema before anything reads it: the change comparison
/// reads `file.sha256`, and a stage seeded from the live file inherits its tables.
///
/// A plain writer handle, not a write scope: this is DDL against the published graph, not a
/// generation, and `sangkar_store::migrate` is the one runner (`DATABASE.md` §10).
///
/// A database that is *already* current is not opened for writing at all. The store's only writable
/// handle applies `PRAGMA journal_mode = WAL` to the file it opens, and a run about to answer
/// "nothing to do" must not rewrite the published bytes it just called unchanged — a watcher polls
/// that path every two seconds. The version is therefore read through a reader, and the runner only
/// runs when the ledger is behind (on a current database its own documentation calls it a no-op).
fn migrate_live(live_path: &Path) -> Result<Migration, AppError> {
    let present = match fs::metadata(live_path) {
        Ok(metadata) => metadata.len() > 0,
        Err(_) => false,
    };
    if !present {
        return migrate_through_writer(live_path);
    }

    let current = {
        let live = open_reader(live_path)?;
        // Exactly this build's version. A *newer* file is not "current": it has to reach the
        // runner, which refuses it with `schema_too_new` instead of letting this build overwrite a
        // graph it cannot read.
        let current = read_schema_version(live.connection()) == schema_version();
        let rebuild_required = read_rebuild_required(live.connection());
        live.close()?;
        if current {
            Some(Migration { rebuild_required })
        } else {
            None
        }
    };
    match current {
        Some(migration) => Ok(migration),
        None => migrate_through_writer(live_path),
    }
}

/// The runner on the handle it needs, closed whatever the verdict was.
fn migrate_through_writer(live_path: &Path) -> Result<Migration, AppError> {
    let mut live = open_writer(live_path)?;
    let migrated = migrate(live.connection_mut());
    let closed = live.close();
    let result = migrated?;
    closed?;
    log::info(
        "worker.live_migrated",
        &[
            ("path", live_path.display().to_string().into()),
            ("fromVersion", result.from_version.into()),
            ("schemaVersion", result.schema_version.into()),
            ("applied", result.applied.join(",").into()),
            ("rebuildRequired", result.rebuild_required.into()),
        ],
    );
    Ok(Migration {
        rebuild_required: result.rebuild_required,
    })
}

/// Discovers the tree, compares it against the published graph and decides whether to run.
fn index_graph(context: &RunContext, summary: &mut RunSummary) -> Result<WorkerExit, AppError> {
    let discovered = discover(&context.repo_root, &DiscoverOptions::default());
    let plan = plan_run(
        &context.repo_root,
        &context.live_path,
        &discovered,
        context.force,
    )?;

    if plan.nothing_to_do {
        collect_seen(summary, &discovered, &plan);
        log::info(
            "worker.nothing_to_do",
            &[
                ("repoRoot", context.repo_root.display().to_string().into()),
                ("files", discovered.files.len().into()),
                ("changed", plan.changed.len().into()),
                ("removed", plan.removed.len().into()),
            ],
        );
        return Ok(WorkerExit::NothingToDo);
    }

    let pass = parse_all(&context.repo_root, &discovered)?;
    // Every unit is bound against one index: a cross-file edge needs both ends, and a per-changed-file
    // index would silently turn every cross-file call into an unresolved site.
    let resolved = {
        let index = SymbolIndex::from(pass.units.as_slice());
        bind(&pass.units, &index)
    };
    let generation_id = new_generation_id();
    let revision = revision_of(&context.repo_root);
    let identity = project_identity_of(&context.repo_root);
    collect_summary(summary, &discovered, &plan, &pass, &resolved);
    let duration_ms = elapsed_millis(context.started_at);

    with_stage(&context.live_path, !context.force, |stage| {
        let mut scope = begin_write(&stage.path)?;
        // An empty seed carries no schema (a forced rebuild has no live file to copy), so the stage
        // is migrated exactly as a fresh database would be.
        if stage.seeded_from == SeededFrom::Empty {
            migrate(scope.conn_mut().connection_mut())?;
        }
        let written = scope.transaction(|tx| {
            write_generation(
                tx,
                GenerationWrite {
                    pass,
                    removed: &plan.removed,
                    discovered: &discovered,
                    resolved: &resolved,
                    generation_id: &generation_id,
                    revision: &revision,
                    identity: &identity,
                    mode: context.mode,
                    duration_ms,
                    rebuild_required: context.rebuild_required,
                },
            )
        });
        // The scope is released on both paths: a stage the run is abandoning must not still hold a
        // handle when `with_stage` discards it, and a failing transaction must be the error the
        // caller sees rather than a `seal` complaint about an open handle.
        let closed = scope.close();
        let result = written?;
        closed?;
        Ok(result)
    })?;

    log::info(
        "worker.published",
        &[
            ("generation", generation_id.into()),
            ("repoRoot", context.repo_root.display().to_string().into()),
            ("files", discovered.files.len().into()),
            ("changed", plan.changed.len().into()),
            ("removed", plan.removed.len().into()),
            ("edges", resolved.edges.len().into()),
            ("durationMs", duration_ms.into()),
            ("mode", context.mode.as_str().into()),
        ],
    );
    Ok(WorkerExit::Published)
}

/// The change plan: the discovered files whose digest differs from their `file` row (or which have
/// no row yet), the rows whose bytes are gone from the tree, plus whatever git additionally reports
/// through [`changed_since`]. Nothing changed and not forced means exit 2 with no stage, which keeps
/// a watcher's idle polls free.
fn plan_run(
    repo_root: &Path,
    live_path: &Path,
    discovered: &DiscoverResult,
    force: bool,
) -> Result<ChangePlan, AppError> {
    let (digests, previous_base_commit) = {
        let live = open_reader(live_path)?;
        let digests = read_digests(&live)?;
        let previous = read_previous_base_commit(&live)?;
        live.close()?;
        (digests, previous)
    };
    // `changed_since` degrades to the hash comparison when git cannot answer, so a plain directory
    // is not a special case here — it is the documented fallback.
    let changes: ChangeSet = changed_since(
        repo_root,
        previous_base_commit.as_deref(),
        &KnownDigests(&digests),
        &ChangedSinceOptions::default(),
    );

    let indexed: BTreeSet<&str> = discovered
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect();
    let mut changed: BTreeSet<String> = BTreeSet::new();
    // One lookup table rather than a scan per file: the walk is the whole tree, and a comparison
    // that re-read the digest list per path would be quadratic in the corpus.
    let stored: BTreeMap<&str, &str> = digests
        .iter()
        .map(|row| (row.path.as_str(), row.sha256.as_str()))
        .collect();
    for file in &discovered.files {
        match stored.get(file.path.as_str()) {
            Some(sha256) if *sha256 == file.sha256.as_str() => {}
            _ => {
                changed.insert(file.path.clone());
            }
        }
    }
    for path in &changes.changed {
        if indexed.contains(path.as_str()) {
            changed.insert(path.clone());
        }
    }
    // A `file` row whose path discovery no longer returns is gone from the graph: deleted, newly
    // ignored, or no longer parseable. The row is the staleness manifest, so it cannot outlive the
    // bytes it describes. `digests` is already in path order.
    let removed: Vec<String> = digests
        .iter()
        .filter(|row| !indexed.contains(row.path.as_str()))
        .map(|row| row.path.clone())
        .collect();

    log::debug(
        "worker.change_plan",
        &[
            ("repoRoot", repo_root.display().to_string().into()),
            ("discovered", discovered.files.len().into()),
            ("changed", changed.len().into()),
            ("removed", removed.len().into()),
            ("degraded", changes.degraded.into()),
        ],
    );

    Ok(ChangePlan {
        nothing_to_do: !force && changed.is_empty() && removed.is_empty(),
        changed: changed.into_iter().collect(),
        removed,
    })
}

/// The digest half of the change set, already read from the live graph: `changed_since` asks for the
/// indexed digests it compares against, and the worker has them in hand, so no second query exists.
struct KnownDigests<'a>(&'a [FileDigest]);

impl DigestSource for KnownDigests<'_> {
    fn all_files(&self) -> Result<Vec<FileDigest>, AppError> {
        Ok(self.0.to_vec())
    }
}

/// Every indexed path with the digest the published graph stored for it. One query, no join.
fn read_digests(conn: &Conn) -> Result<Vec<FileDigest>, AppError> {
    let mut statement = conn
        .connection()
        .prepare(SELECT_FILE_DIGESTS)
        .map_err(|error| internal("preparing the file digests", error))?;
    let rows = statement
        .query_map([], |row| {
            Ok(FileDigest {
                path: row.get(0)?,
                sha256: row.get(1)?,
            })
        })
        .map_err(|error| internal("reading the file digests", error))?
        .collect::<Result<Vec<FileDigest>, _>>()
        .map_err(|error| internal("reading the file digests", error))?;
    Ok(rows)
}

/// The published generation's `base_commit`: the diff base the change tools default to.
fn read_previous_base_commit(conn: &Conn) -> Result<Option<String>, AppError> {
    let found = conn
        .connection()
        .query_row(SELECT_PREVIOUS_BASE_COMMIT, [], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()
        .map_err(|error| internal("reading the previous base commit", error))?;
    Ok(found.flatten())
}

/// Parses and extracts every discovered file, in discovery order. A file with no tree becomes a
/// coverage row instead of aborting the run: one unparseable file must not cost the whole graph.
fn parse_all(repo_root: &Path, discovered: &DiscoverResult) -> Result<ParsePass, AppError> {
    let mut pass = ParsePass {
        files: Vec::with_capacity(discovered.files.len()),
        units: Vec::new(),
    };
    for file in &discovered.files {
        let source = fs::read(repo_root.join(&file.path)).map_err(|error| {
            AppError::internal(format!("cannot read {}: {error}", file.path))
                .with_hint("discovery hashed this path a moment ago; check the tree is not moving")
        })?;
        let line_count = count_lines(&source);
        match parse_file(&source, file.lang) {
            ParseResult::Err { reason, detail } => {
                log::warn(
                    "worker.file_not_parsed",
                    &[
                        ("path", file.path.clone().into()),
                        ("reason", reason.as_str().into()),
                        ("detail", detail.clone().into()),
                    ],
                );
                pass.files.push(ParsedFile {
                    file: file.clone(),
                    line_count,
                    parse_status: ParseStatus::Failed,
                    error_ranges: Vec::new(),
                    failure: Some(NotIndexedEntry {
                        path: file.path.clone(),
                        reason: parse_failure_reason(reason),
                        detail: Some(detail),
                        language: Some(file.lang.as_str().to_owned()),
                    }),
                });
            }
            ParseResult::Ok { tree, error_ranges } => {
                let extracted = extract_file(&tree, &source, spec_for(file.lang), &file.path);
                // The tree is only an input to the extractor; holding every tree alive would hold
                // the whole corpus in memory.
                drop(tree);
                pass.files.push(ParsedFile {
                    file: file.clone(),
                    line_count,
                    parse_status: if error_ranges.is_empty() {
                        ParseStatus::Ok
                    } else {
                        ParseStatus::Partial
                    },
                    error_ranges,
                    failure: None,
                });
                pass.units.push(file_unit(&file.path, &extracted));
            }
        }
    }
    Ok(pass)
}

/// The coverage reason for a parser that produced no tree: the two spellings
/// [`crate::parse::ParseFailureReason`] declares, mapped onto the closed `not_indexed` set.
fn parse_failure_reason(reason: crate::parse::ParseFailureReason) -> NotIndexedReason {
    match reason {
        crate::parse::ParseFailureReason::GrammarUnavailable => {
            NotIndexedReason::GrammarUnavailable
        }
        crate::parse::ParseFailureReason::ParseFailed => NotIndexedReason::ParseFailed,
    }
}

/// `wc -l` semantics: one line per newline, and a non-empty unterminated last line counts.
fn count_lines(source: &[u8]) -> i64 {
    if source.is_empty() {
        return 0;
    }
    let newlines = source.iter().filter(|byte| **byte == b'\n').count() as i64;
    if source.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

/// `g-xxxxxx`: the short id `DATABASE.md` §3.1 shows, unique enough to name one publish.
///
/// Six hex characters from the standard library's one OS-seeded source, mixed with the clock, the
/// pid and a stack address for the same reason `sangkar_store::publish` mixes them: two runs that
/// started in the same instant must still get two names. `getrandom` and `rand` are not dependencies
/// of this crate and a generation name does not need them.
fn new_generation_id() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hash, Hasher};

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
    let suffix: String = digest[..3]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("g-{suffix}")
}

/// The tree a publish is about, for the `meta` identity keys (`DATABASE.md` §3.1): the name and root
/// come from the filesystem, so they are always known; the branch and remote come from git.
///
/// Both git reads are best-effort and cannot fail: a publish about a detached HEAD, an origin-less
/// clone or a plain directory is still a publish.
fn project_identity_of(repo_root: &Path) -> ProjectIdentity {
    let name = match repo_root.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        // A root that ends in `..` or `/` has no last component; the whole path still names it.
        None => repo_root.display().to_string(),
    };
    ProjectIdentity {
        name,
        root: repo_root.display().to_string(),
        branch: git_value(repo_root, &["symbolic-ref", "--short", "HEAD"]),
        remote: git_value(repo_root, &["config", "--get", "remote.origin.url"]),
    }
}

/// `HEAD` plus whether tracked files differ from it, or a `None` head when git cannot answer.
fn revision_of(repo_root: &Path) -> Revision {
    let head = match git_output(repo_root, &["rev-parse", "HEAD"]) {
        Some(output) if !output.trim().is_empty() => Some(output.trim().to_owned()),
        _ => None,
    };
    if head.is_none() {
        return Revision {
            head: None,
            dirty: false,
        };
    }
    let status = git_output(
        repo_root,
        &[
            "status",
            "--porcelain",
            "--untracked-files=no",
            "--no-renames",
        ],
    );
    // `None` is "git could not answer"; a tree whose state cannot be read is not called dirty.
    Revision {
        head,
        dirty: status.is_some_and(|output| !output.trim().is_empty()),
    }
}

/// One git read as a trimmed string, `""` when git cannot answer. A missing branch name must cost
/// the identity, never the generation.
fn git_value(repo_root: &Path, args: &[&str]) -> String {
    match git_output(repo_root, args) {
        Some(output) => output.trim().to_owned(),
        None => String::new(),
    }
}

/// One bounded, non-interactive git read: `Some(stdout)` only when git exited 0 within
/// [`GIT_TIMEOUT_MS`].
///
/// stderr is discarded rather than piped: nothing here reads it, and a piped-but-undrained pipe is
/// exactly the deadlock `sangkar_domain::diff` spends two reader threads to avoid.
fn git_output(repo_root: &Path, args: &[&str]) -> Option<String> {
    let mut child = match Command::new("git")
        .args(GIT_PREFIX)
        .arg(repo_root)
        .args(args)
        .current_dir(repo_root)
        // Closed stdin: the child has no terminal to prompt on.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            log::debug(
                "worker.git_unavailable",
                &[
                    ("repoRoot", repo_root.display().to_string().into()),
                    ("args", args.join(" ").into()),
                    ("reason", error.to_string().into()),
                ],
            );
            return None;
        }
    };

    let stdout = drain(child.stdout.take());
    let deadline = Instant::now() + Duration::from_millis(GIT_TIMEOUT_MS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    log::debug(
                        "worker.git_timeout",
                        &[
                            ("repoRoot", repo_root.display().to_string().into()),
                            ("args", args.join(" ").into()),
                            ("timeoutMs", GIT_TIMEOUT_MS.into()),
                        ],
                    );
                    return None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                log::debug(
                    "worker.git_unavailable",
                    &[
                        ("repoRoot", repo_root.display().to_string().into()),
                        ("args", args.join(" ").into()),
                        ("reason", error.to_string().into()),
                    ],
                );
                return None;
            }
        }
    };

    let bytes = match stdout.recv_timeout(COLLECT_GRACE) {
        Ok(Ok(bytes)) => bytes,
        _ => return None,
    };
    if !status.success() {
        log::debug(
            "worker.git_failed",
            &[
                ("repoRoot", repo_root.display().to_string().into()),
                ("args", args.join(" ").into()),
                ("exit", status_code(status).into()),
            ],
        );
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn status_code(status: std::process::ExitStatus) -> i64 {
    status.code().map_or(-1, i64::from)
}

/// Drains one child pipe on its own thread, so the wait loop never has to choose between reading and
/// reaping.
fn drain<R>(pipe: Option<R>) -> Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = pipe.read_to_end(&mut buffer).map(|_| buffer);
            // The receiver is gone only when this module already answered; dropping the result then
            // is the same "nobody is listening" a closed pipe means.
            let _ = sender.send(result);
        });
    }
    receiver
}

/// Writes one generation into the stage. The order is fixed and load-bearing:
///
/// 1. tombstones and row drops for paths that left the tree — the tombstone reads the node rows, so
///    it comes before the delete that cascades them;
/// 2. `file` rows for every discovered path, including the ones whose parse failed;
/// 3. the previous rows of a failed parse are cleared, because that file no longer contributes;
/// 4. every unit is persisted, which replaces its own nodes, edges and unresolved sites;
/// 5. [`restore_edges`] re-inserts the resolved edges whose ends the cascades in (4) removed;
/// 6. coverage is rewritten from discovery plus parse failures;
/// 7. `finalize_generation` derives metrics/FTS/tombstone names and records the row, and the run's
///    own `meta` state is written in the same transaction so it publishes atomically.
fn write_generation(
    tx: &Transaction<'_>,
    write: GenerationWrite<'_>,
) -> Result<GenerationResult, AppError> {
    let GenerationWrite {
        pass,
        removed,
        discovered,
        resolved,
        generation_id,
        revision,
        identity,
        mode,
        duration_ms,
        rebuild_required,
    } = write;
    let ParsePass { files, units } = pass;
    let last_seen =
        read_meta(tx, META_GENERATION)?.unwrap_or_else(|| PENDING_GENERATION.to_owned());

    for path in removed {
        tx.execute(TOMBSTONE_FILE, params![last_seen, PENDING_GENERATION, path])
            .map_err(|error| failed("recording the tombstones of a removed file", error))?;
        tx.execute(DROP_FILE, params![path])
            .map_err(|error| failed("dropping a removed file's row", error))?;
    }

    for parsed in &files {
        tx.execute(
            UPSERT_FILE,
            params![
                parsed.file.path,
                parsed.file.lang.as_str(),
                parsed.file.sha256,
                file_size(parsed),
                parsed.line_count,
                parsed.parse_status.as_str(),
                joined_ranges(&parsed.error_ranges),
                generation_id,
            ],
        )
        .map_err(|error| failed("writing a file row", error))?;
    }

    for parsed in &files {
        if parsed.parse_status != ParseStatus::Failed {
            continue;
        }
        for (sql, what) in [
            (DROP_EDGES, "dropping a failed parse's edges"),
            (
                DROP_UNRESOLVED,
                "dropping a failed parse's unresolved sites",
            ),
            (DROP_NODES, "dropping a failed parse's nodes"),
        ] {
            tx.execute(sql, params![parsed.file.path])
                .map_err(|error| failed(what, error))?;
        }
    }

    // Every parsed unit, in discovery order, inside the one `BEGIN IMMEDIATE` the generation is.
    let bindings = bindings_by_file(resolved);
    let empty = BindResult::default();
    for unit in units {
        let key = normalize_rel(&unit.rel_path);
        let binding = bindings.get(&key).unwrap_or(&empty);
        let persist = PersistUnit {
            rel_path: unit.rel_path,
            nodes: unit.nodes,
            raw_edges: unit.raw_edges,
            unresolved: unit.unresolved,
            not_indexed: Vec::new(),
        };
        persist_unit(tx, &persist, binding)?;
    }

    restore_edges(tx, resolved)?;
    rewrite_coverage(tx, discovered, &files)?;

    let result = finalize_generation(
        tx,
        &GenerationInput {
            generation_id: generation_id.to_owned(),
            base_commit: revision.head.clone(),
            mode: mode.as_str().to_owned(),
            project: Some(identity.clone()),
            published_at: None,
            duration_ms,
            worktree_dirty: revision.dirty,
        },
    )?;
    write_meta(tx, META_INDEXER_STATE, STATE_IDLE)?;
    write_meta(tx, META_CONSECUTIVE_FAILURES, "0")?;
    if rebuild_required {
        // `migrate` reported it and this full rebuild just honoured it; leaving the flag set would
        // force every later run to rebuild as well.
        write_meta(tx, META_REBUILD_REQUIRED, "0")?;
    }
    Ok(result)
}

/// `file.size` as the column's integer. Discovery caps a candidate at 512 MiB, so the saturation is
/// unreachable; it exists because the column is `integer` and the fact is `u64`.
fn file_size(parsed: &ParsedFile) -> i64 {
    i64::try_from(parsed.file.size).unwrap_or(i64::MAX)
}

/// The `parse_error_ranges` value: `None` when the list is empty, else the ranges comma-joined.
fn joined_ranges(ranges: &[String]) -> Option<String> {
    if ranges.is_empty() {
        None
    } else {
        Some(ranges.join(","))
    }
}

/// Splits one whole-graph bind result into the per-unit shape `persist_unit` takes.
fn bindings_by_file(resolved: &BindResult) -> BTreeMap<String, BindResult> {
    let mut by_file: BTreeMap<String, BindResult> = BTreeMap::new();
    for edge in &resolved.edges {
        entry_for(&mut by_file, &edge.file).edges.push(edge.clone());
    }
    for site in &resolved.unresolved {
        entry_for(&mut by_file, &site.file)
            .unresolved
            .push(site.clone());
    }
    by_file
}

fn entry_for<'a>(by_file: &'a mut BTreeMap<String, BindResult>, file: &str) -> &'a mut BindResult {
    by_file.entry(normalize_rel(file)).or_default()
}

/// Re-inserts every resolved edge whose two ends exist, once the whole unit set is written.
///
/// `persist_unit` deletes a file's nodes before it inserts the replacements, and `edge.dst_id`
/// cascades: an edge written by an earlier unit into a file persisted later is gone by the time that
/// file's nodes exist again. Re-inserting the full resolved set here — after every node row is final,
/// with `ON CONFLICT DO NOTHING` against the edge's unique key — makes the generation's edge set
/// independent of the order the units happened to be persisted in. An edge whose end is missing (the
/// file left the tree, or its parse failed) is dropped, never written dangling.
fn restore_edges(tx: &Transaction<'_>, resolved: &BindResult) -> Result<(), AppError> {
    if resolved.edges.is_empty() {
        return Ok(());
    }
    let node_ids = id_map(tx, SELECT_NODE_IDS, "reading the node ids")?;
    let file_ids: BTreeMap<String, i64> = {
        let mut statement = tx
            .prepare(SELECT_FILE_IDS)
            .map_err(|error| internal("preparing the file ids", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| internal("reading the file ids", error))?
            .collect::<Result<Vec<(i64, String)>, _>>()
            .map_err(|error| internal("reading the file ids", error))?;
        rows.into_iter().map(|(id, path)| (path, id)).collect()
    };

    for edge in &resolved.edges {
        let (Some(src), Some(dst)) = (node_ids.get(&edge.src), node_ids.get(&edge.dst)) else {
            continue;
        };
        let Some(file_id) = file_ids.get(&normalize_rel(&edge.file)) else {
            continue;
        };
        tx.execute(
            INSERT_EDGE,
            params![
                src,
                dst,
                edge.kind,
                edge.tier,
                file_id,
                edge.line,
                edge.local_name,
                edge.evidence,
            ],
        )
        .map_err(|error| failed("restoring a resolved edge", error))?;
    }
    Ok(())
}

/// Rewrites `not_indexed` from this run's discovery walk and parse failures. Discovery always walks
/// the whole tree, so the replacement is exact: a path that became indexable loses its old reason
/// instead of keeping a coverage row that contradicts the graph.
fn rewrite_coverage(
    tx: &Transaction<'_>,
    discovered: &DiscoverResult,
    files: &[ParsedFile],
) -> Result<(), AppError> {
    tx.execute(CLEAR_COVERAGE, [])
        .map_err(|error| failed("clearing the coverage rows", error))?;
    for entry in &discovered.not_indexed {
        tx.execute(
            INSERT_COVERAGE,
            params![
                entry.path,
                entry.reason.as_str(),
                None::<String>,
                entry.language.map(|lang| lang.as_str()),
            ],
        )
        .map_err(|error| failed("writing a coverage row", error))?;
    }
    for parsed in files {
        let Some(failure) = &parsed.failure else {
            continue;
        };
        tx.execute(
            INSERT_COVERAGE,
            params![
                failure.path,
                failure.reason.as_str(),
                failure.detail,
                failure.language,
            ],
        )
        .map_err(|error| failed("writing a failed parse's coverage row", error))?;
    }
    Ok(())
}

fn read_meta(tx: &Transaction<'_>, key: &str) -> Result<Option<String>, AppError> {
    tx.query_row(SELECT_META, params![key], |row| row.get::<_, String>(0))
        .optional()
        .map_err(|error| internal("reading a meta value", error))
}

fn write_meta(tx: &Transaction<'_>, key: &str, value: &str) -> Result<(), AppError> {
    tx.execute(META_UPSERT, params![key, value])
        .map(|_rows| ())
        .map_err(|error| failed("writing a meta value", error))
}

fn id_map(tx: &Transaction<'_>, sql: &str, what: &str) -> Result<BTreeMap<String, i64>, AppError> {
    let mut statement = tx.prepare(sql).map_err(|error| internal(what, error))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| internal(what, error))?
        .collect::<Result<Vec<(i64, String)>, _>>()
        .map_err(|error| internal(what, error))?;
    Ok(rows.into_iter().map(|(id, key)| (key, id)).collect())
}

/// The facts a run that publishes nothing still knows: the walk and the change plan. A
/// `NothingToDo` run writes no nodes, so every other counter stays zero.
fn collect_seen(summary: &mut RunSummary, discovered: &DiscoverResult, plan: &ChangePlan) {
    summary.files_seen = discovered.files.len();
    summary.files_skipped = discovered.skipped;
    summary.files_changed_in_run = plan.changed.len();
    let mut languages: BTreeMap<String, usize> = BTreeMap::new();
    for file in &discovered.files {
        *languages.entry(file.lang.as_str().to_owned()).or_insert(0) += 1;
    }
    summary.languages = languages;
}

/// Fills the run's counters from what the run already holds: no second walk, no query, no re-parse.
/// `unresolved_count` is both lists `persist_unit` inserts — the sites the extractor could not type
/// and the ones binding failed to resolve — so it is the row count the graph will show.
fn collect_summary(
    summary: &mut RunSummary,
    discovered: &DiscoverResult,
    plan: &ChangePlan,
    pass: &ParsePass,
    resolved: &BindResult,
) {
    collect_seen(summary, discovered, plan);
    summary.files_indexed = pass
        .files
        .iter()
        .filter(|file| file.parse_status != ParseStatus::Failed)
        .count();
    summary.files_failed = pass
        .files
        .iter()
        .filter(|file| file.parse_status == ParseStatus::Failed)
        .count();
    let mut nodes_by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for unit in &pass.units {
        for node in &unit.nodes {
            *nodes_by_kind.entry(node.kind.clone()).or_insert(0) += 1;
        }
    }
    summary.nodes_by_kind = nodes_by_kind;
    let mut edges_by_kind_tier: BTreeMap<String, usize> = BTreeMap::new();
    for edge in &resolved.edges {
        *edges_by_kind_tier
            .entry(format!("{}:{}", edge.kind, edge.tier))
            .or_insert(0) += 1;
    }
    summary.edges_by_kind_tier = edges_by_kind_tier;
    summary.unresolved_count = pass
        .units
        .iter()
        .map(|unit| unit.unresolved.len())
        .sum::<usize>()
        + resolved.unresolved.len();
}

/// Records a failed run in the live graph's `meta`: `indexer_state = failed` and one more
/// `consecutive_failures`, which is what `status.health` reports even though no generation was
/// published (`DATABASE.md` §5). Best-effort by design — the run's exit code is already decided, and
/// a database that cannot be opened must not turn a clean 1 into a crash.
fn record_failure(live_path: &Path, error: &AppError) {
    if !live_path.exists() {
        // Nothing was ever migrated here, so there is no `meta` to write and no graph to mislead.
        return;
    }
    let noted = (|| -> Result<(), AppError> {
        // A plain writer handle, not a write scope: this is a two-row `meta` note about the
        // published file, not a generation. `sangkar_store::db` hands out no other writable handle.
        let live = open_writer(live_path)?;
        let written = note_failure(&live);
        let closed = live.close();
        written?;
        closed
    })();
    match noted {
        Ok(()) => {}
        Err(meta_error) => log::error(
            "worker.failure_not_recorded",
            &[
                ("livePath", live_path.display().to_string().into()),
                ("reason", meta_error.message.into()),
            ],
        ),
    }
    log::warn(
        "worker.failure_recorded",
        &[
            ("livePath", live_path.display().to_string().into()),
            ("reason", error.message.clone().into()),
        ],
    );
}

/// The two-row note itself, on an open handle.
fn note_failure(live: &Conn) -> Result<(), AppError> {
    let table = live
        .connection()
        .query_row(SELECT_META_TABLE, [], |row| row.get::<_, String>(0))
        .optional()
        .map_err(|error| internal("looking for the meta table", error))?;
    if table.is_none() {
        return Ok(());
    }
    let previous = live
        .connection()
        .query_row(SELECT_META, params![META_CONSECUTIVE_FAILURES], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(|error| internal("reading the failure streak", error))?;
    let parsed = previous
        .as_deref()
        .and_then(|raw| raw.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let failures = (parsed + 1).to_string();
    for (key, value) in [
        (META_CONSECUTIVE_FAILURES, failures.as_str()),
        (META_INDEXER_STATE, STATE_FAILED),
    ] {
        live.connection()
            .execute(META_UPSERT, params![key, value])
            .map_err(|error| failed("recording a failed run", error))?;
    }
    Ok(())
}

/// The `internal` failure for a write the caller cannot act on beyond retrying the run.
fn failed(what: &str, error: impl std::fmt::Display) -> AppError {
    AppError::internal(format!("{what}: {error}"))
        .with_hint("the stage is discarded; the published graph is unchanged")
}

/// The `internal` failure for a read that should have succeeded on a migrated stage.
fn internal(what: &str, error: impl std::fmt::Display) -> AppError {
    AppError::internal(format!("{what}: {error}"))
}

/// Milliseconds one run took, as the `generation.duration_ms` and record field both expect.
fn elapsed_millis(started_at: Instant) -> i64 {
    i64::try_from(started_at.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// `path` as an absolute path, because the identity keys and the git child both need one.
fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(error) => {
            log::warn(
                "worker.cwd_unreadable",
                &[
                    ("path", path.display().to_string().into()),
                    ("reason", error.to_string().into()),
                ],
            );
            path.to_path_buf()
        }
    }
}

#[cfg(test)]
mod tests {
    use sangkar_domain::diff::sha256_hex;

    use super::*;

    /// The digest helper the live-file assertion in `tests/worker.rs` uses is the same one the change
    /// tools do, so a typo in one is a failure in both.
    #[test]
    fn the_change_digest_is_the_domain_digest() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// `wc -l` semantics on the boundary cases the manifest's `line_count` records.
    #[test]
    fn line_count_matches_wc() {
        assert_eq!(count_lines(b""), 0);
        assert_eq!(count_lines(b"one\n"), 1);
        assert_eq!(count_lines(b"one"), 1);
        assert_eq!(count_lines(b"one\ntwo"), 2);
        assert_eq!(count_lines(b"one\ntwo\n"), 2);
        assert_eq!(count_lines(b"\n\n"), 2);
    }

    /// `g-` plus six hex characters: the id `DATABASE.md` §3.1 shows, and never the pending marker.
    #[test]
    fn generation_ids_are_short_hex() {
        let id = new_generation_id();
        assert_eq!(id.len(), 8, "{id}");
        assert!(id.starts_with("g-"), "{id}");
        assert!(
            id[2..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{id}"
        );
        assert_ne!(id, PENDING_GENERATION);
        assert_ne!(new_generation_id(), id);
    }

    /// The four codes are the process contract; a renumbering would silently break the watcher and
    /// every shell that reads the status.
    #[test]
    fn exit_codes_are_the_contract() {
        assert_eq!(WorkerExit::Published.code(), 0);
        assert_eq!(WorkerExit::Failed.code(), 1);
        assert_eq!(WorkerExit::NothingToDo.code(), 2);
        assert_eq!(WorkerExit::Locked.code(), 75);
        assert_eq!(WorkerMode::Full.as_str(), "full");
        assert_eq!(WorkerMode::Fast.as_str(), "fast");
    }

    /// The ranges column is `None` rather than an empty string when nothing went wrong: the two are
    /// different facts, and only one of them means "parsed clean".
    #[test]
    fn empty_error_ranges_are_null() {
        assert_eq!(joined_ranges(&[]), None);
        assert_eq!(
            joined_ranges(&["3".to_owned(), "9-11".to_owned()]),
            Some("3,9-11".to_owned())
        );
    }
}
