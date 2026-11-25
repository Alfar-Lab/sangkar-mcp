//! The worker's contract is a process contract: four exit codes, an untouched stdout, and `meta`
//! rows that report a failure without publishing a graph. So the tests drive [`run_worker`] the way
//! the server does — one call, one code — over a temp directory that is deliberately not a git
//! repository, because "a plain directory still indexes" is the degradation the design promises.
//!
//! The four codes and the test that observes each one:
//!
//! | code | meaning | test |
//! |---|---|---|
//! | 0 | published | [`publishes_a_plain_directory_once_then_reports_nothing_to_do`] |
//! | 1 | failed | [`a_graph_from_a_newer_build_is_failed_and_recorded`], [`an_unusable_data_dir_is_failed`] |
//! | 2 | nothing to do | [`publishes_a_plain_directory_once_then_reports_nothing_to_do`] |
//! | 75 | another indexer holds the lock | [`a_held_lock_is_locked_and_writes_nothing`] |
//!
//! The fixture is two Python files where the caller sorts *before* the file it calls
//! (`src/caller.py` calls `src/widget.py`): that is the order in which `persist_unit`'s per-file
//! `DELETE FROM node` would cascade the caller's edge away, so the cross-file edge assertion is the
//! evidence that the generation's edges survive a full re-persist.
//!
//! What is asserted about bytes: an idle run must leave the published graph byte for byte as it
//! found it, because a watcher polls that path every two seconds. The digest is
//! [`sangkar_domain::diff::sha256_hex`] over the bytes of `graph.db` itself (not over a source
//! file's `file.sha256`), computed by [`live_digest`].

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_domain::diff::sha256_hex;
use sangkar_index::worker::{WorkerExit, WorkerMode, WorkerOptions, run_worker};
use sangkar_store::db::{Conn, open_reader, open_writer};
use sangkar_store::lock::{acquire_lock, inspect, lock_path, release};
use sangkar_store::migrate::migrate;
use sangkar_store::{OptionalExtension, params};

const CALLER: &str = "src/caller.py";
const WIDGET: &str = "src/widget.py";

const WIDGET_SOURCE: &str = "def render(title):\n    return title\n";

const CALLER_SOURCE: &str = "from widget import render\n\n\ndef run():\n    return render(\"x\")\n";

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
            "sangkar-worker-{label}-{}-{nanos}-{}",
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

    /// Writes one fixture file, creating the directories it needs.
    fn write(&self, relative: &str, content: &str) {
        let absolute = self.0.join(relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).expect("create the fixture's directory");
        }
        fs::write(&absolute, content).expect("write the fixture file");
    }

    fn remove(&self, relative: &str) {
        fs::remove_file(self.0.join(relative)).expect("remove the fixture file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A temp working tree plus the data dir the product would use, which is inside it.
struct Repo {
    dir: TempDir,
}

impl Repo {
    /// The two-file fixture; `extra` adds or overrides paths.
    fn new(label: &str, extra: &[(&str, &str)]) -> Self {
        let dir = TempDir::new(label);
        dir.write(CALLER, CALLER_SOURCE);
        dir.write(WIDGET, WIDGET_SOURCE);
        for (path, content) in extra {
            dir.write(path, content);
        }
        Self { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn data_dir(&self) -> PathBuf {
        self.root().join(".sangkar")
    }

    fn live(&self) -> PathBuf {
        self.data_dir().join("graph.db")
    }

    /// The options the server's own spawn would produce: an explicit `fast` mode, not forced.
    fn options(&self) -> WorkerOptions {
        WorkerOptions {
            repo_root: self.root().to_path_buf(),
            data_dir: self.data_dir(),
            force: false,
            mode: Some(WorkerMode::Fast),
        }
    }
}

/// One publish, as the tests read it back.
struct Generation {
    id: String,
    mode: String,
    status: String,
    base_commit: Option<String>,
    worktree_dirty: i64,
    files_indexed: i64,
    nodes: i64,
    edges: i64,
}

/// One `file` row's manifest facts.
struct FileRow {
    path: String,
    parse_status: String,
    error_ranges: Option<String>,
    line_count: i64,
    indexed_generation: String,
}

fn reader(live: &Path) -> Conn {
    open_reader(live).expect("open the published graph read-only")
}

fn count(live: &Path, table: &str) -> i64 {
    let conn = reader(live);
    let sql = format!("SELECT count(*) FROM {table}");
    let rows = conn
        .connection()
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .expect("count the table");
    conn.close().expect("close the reader");
    rows
}

fn meta(live: &Path, key: &str) -> Option<String> {
    let conn = reader(live);
    let value = conn
        .connection()
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .expect("read a meta value");
    conn.close().expect("close the reader");
    value
}

/// Writes one `meta` row, the way a migration would before a run.
fn set_meta(live: &Path, key: &str, value: &str) {
    let writer = open_writer(live).expect("open the published graph for writing");
    writer
        .connection()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .expect("write a meta value");
    writer.close().expect("close the writer");
}

/// The newest `generation` row, which is the one a reader is served.
fn latest_generation(live: &Path) -> Generation {
    let conn = reader(live);
    let row = conn
        .connection()
        .query_row(
            concat!(
                "SELECT id, mode, status, base_commit, worktree_dirty, files_indexed, nodes, edges",
                " FROM generation ORDER BY seq DESC LIMIT 1",
            ),
            [],
            |row| {
                Ok(Generation {
                    id: row.get(0)?,
                    mode: row.get(1)?,
                    status: row.get(2)?,
                    base_commit: row.get(3)?,
                    worktree_dirty: row.get(4)?,
                    files_indexed: row.get(5)?,
                    nodes: row.get(6)?,
                    edges: row.get(7)?,
                })
            },
        )
        .expect("read the newest generation");
    conn.close().expect("close the reader");
    row
}

fn generation_count(live: &Path) -> i64 {
    count(live, "generation")
}

fn file_rows(live: &Path) -> Vec<FileRow> {
    let conn = reader(live);
    let rows = {
        let mut statement = conn
            .connection()
            .prepare(
                "SELECT path, parse_status, parse_error_ranges, line_count, indexed_generation \
                 FROM file ORDER BY path",
            )
            .expect("prepare the file rows");
        statement
            .query_map([], |row| {
                Ok(FileRow {
                    path: row.get(0)?,
                    parse_status: row.get(1)?,
                    error_ranges: row.get(2)?,
                    line_count: row.get(3)?,
                    indexed_generation: row.get(4)?,
                })
            })
            .expect("query the file rows")
            .collect::<Result<Vec<FileRow>, _>>()
            .expect("collect the file rows")
    };
    conn.close().expect("close the reader");
    rows
}

/// One count over `edge` joined to both ends, so an assertion names the symbols it means.
fn edge_count(live: &Path, where_clause: &str) -> i64 {
    let conn = reader(live);
    let sql = format!(
        "SELECT count(*) FROM edge e JOIN node s ON s.id = e.src_id JOIN node d ON d.id = e.dst_id \
         WHERE {where_clause}"
    );
    let rows = conn
        .connection()
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .expect("count the edges");
    conn.close().expect("close the reader");
    rows
}

/// The published graph's own digest: the bytes of `graph.db`, hashed with the function the change
/// tools use, so "unchanged" means the same bytes and not merely the same row counts.
fn live_digest(live: &Path) -> String {
    let bytes = fs::read(live).expect("read the published graph");
    sha256_hex(&bytes)
}

/// Stage leftovers: a discarded stage must leave nothing behind, published or not.
fn stage_files(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(data_dir)
        .expect("list the data directory")
        .map(|entry| {
            entry
                .expect("read a directory entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.contains(".stage."))
        .collect();
    names.sort();
    names
}

fn lock_is_held(data_dir: &Path) -> bool {
    inspect(data_dir).expect("inspect the write lock").held
}

#[test]
fn publishes_a_plain_directory_once_then_reports_nothing_to_do() {
    let repo = Repo::new("publish", &[]);

    let first = run_worker(&repo.options());
    assert_eq!(first, WorkerExit::Published);

    // One generation, and the state a reader's `status.health` reports.
    assert_eq!(generation_count(&repo.live()), 1);
    assert_eq!(meta(&repo.live(), "indexer_state").as_deref(), Some("idle"));
    assert_eq!(
        meta(&repo.live(), "consecutive_failures").as_deref(),
        Some("0")
    );
    let published = latest_generation(&repo.live());
    // The mode the caller asked for, even though a first build is forced internally.
    assert_eq!(published.mode, "fast");
    assert_eq!(published.status, "published");
    // Not a git repository: no commit is claimed, and the run still published.
    assert_eq!(published.base_commit, None);
    assert_eq!(published.worktree_dirty, 0);
    assert_eq!(published.files_indexed, 2);
    assert!(
        published.nodes > 0,
        "{published_nodes}",
        published_nodes = published.nodes
    );

    // Both files are in the graph, with the manifest facts the incremental planner keys on.
    let files = file_rows(&repo.live());
    assert_eq!(
        files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![CALLER, WIDGET]
    );
    assert_eq!(files[0].line_count, 5);
    assert_eq!(files[0].parse_status, "ok");
    assert_eq!(files[0].error_ranges, None);
    assert_eq!(files[1].line_count, 2);
    assert_eq!(files[1].parse_status, "ok");
    assert_eq!(files[0].indexed_generation, published.id);
    assert_eq!(files[1].indexed_generation, published.id);

    // The cross-file edge: caller sorts first, so its edge into `widget.py` is the one a per-file
    // delete would have cascaded away before the callee's nodes were re-inserted.
    assert_eq!(
        edge_count(
            &repo.live(),
            "e.kind = 'calls' AND s.name = 'run' AND d.name = 'render'"
        ),
        1
    );

    // The data dir ignores itself with exactly one star, and the lock is gone after the run.
    assert_eq!(
        fs::read_to_string(repo.data_dir().join(".gitignore")).expect("read the data dir's ignore"),
        "*\n"
    );
    assert!(!lock_is_held(&repo.data_dir()));
    assert_eq!(stage_files(&repo.data_dir()), Vec::<String>::new());

    // Nothing changed and the run was not forced: 2, no second generation, and the published bytes
    // are exactly the bytes the first publish left — an idle poll must not rewrite the graph.
    let before = live_digest(&repo.live());
    let second = run_worker(&repo.options());
    assert_eq!(second, WorkerExit::NothingToDo);
    assert_eq!(live_digest(&repo.live()), before);
    assert_eq!(generation_count(&repo.live()), 1);
    assert_eq!(stage_files(&repo.data_dir()), Vec::<String>::new());
}

#[test]
fn a_fresh_store_is_a_first_build_so_a_run_with_no_mode_publishes_full() {
    let repo = Repo::new("first-build", &[]);
    let options = WorkerOptions {
        repo_root: repo.root().to_path_buf(),
        data_dir: repo.data_dir(),
        force: false,
        mode: None,
    };

    // No `mode` and no `force`: this is `run_worker`'s own resolution. A store that has never
    // published a generation has nothing to diff against, so the run is `full` by default — a
    // `fast` label there would misdescribe the generation it wrote.
    assert_eq!(run_worker(&options), WorkerExit::Published);
    assert_eq!(latest_generation(&repo.live()).mode, "full");

    // The same tree, still with no `mode`: now there is a published generation to compare against,
    // so the run resolves to `fast`, finds nothing changed and writes no second generation.
    assert_eq!(run_worker(&options), WorkerExit::NothingToDo);
    assert_eq!(generation_count(&repo.live()), 1);
}

#[test]
fn force_publishes_a_fresh_generation_from_an_empty_stage() {
    let repo = Repo::new("force", &[]);
    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
    let nodes = count(&repo.live(), "node");

    // No `mode` override: a forced run defaults to `full`.
    let forced = WorkerOptions {
        repo_root: repo.root().to_path_buf(),
        data_dir: repo.data_dir(),
        force: true,
        mode: None,
    };
    assert_eq!(run_worker(&forced), WorkerExit::Published);
    // A forced run starts from an empty file, so the graph's history starts over with it: one row,
    // `full`, and the same graph content the incremental run had published.
    assert_eq!(generation_count(&repo.live()), 1);
    assert_eq!(latest_generation(&repo.live()).mode, "full");
    assert_eq!(count(&repo.live(), "node"), nodes);
    assert_eq!(meta(&repo.live(), "indexer_state").as_deref(), Some("idle"));
    assert_eq!(stage_files(&repo.data_dir()), Vec::<String>::new());
}

#[test]
fn a_pending_rebuild_flag_forces_a_full_run_and_is_cleared_by_it() {
    let repo = Repo::new("rebuild-flag", &[]);
    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);

    // What a migration that cannot be applied in place records: the indexer must throw the graph
    // away and index fully. No `force` on the options — the flag alone has to do it.
    set_meta(&repo.live(), "rebuild_required", "1");
    let unforced = WorkerOptions {
        repo_root: repo.root().to_path_buf(),
        data_dir: repo.data_dir(),
        force: false,
        mode: None,
    };

    assert_eq!(run_worker(&unforced), WorkerExit::Published);
    assert_eq!(latest_generation(&repo.live()).mode, "full");
    // The flag is honoured, so it is cleared: leaving it set would force every later run to rebuild.
    assert_eq!(meta(&repo.live(), "rebuild_required").as_deref(), Some("0"));
    // And the run after that finds nothing to do, which is the proof the flag is really gone.
    assert_eq!(run_worker(&unforced), WorkerExit::NothingToDo);
}

#[test]
fn a_changed_file_publishes_and_restores_every_edge() {
    let repo = Repo::new("fast", &[]);
    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
    let first = latest_generation(&repo.live());
    let edges_before = first.edges;
    assert!(edges_before > 0, "the fixture must have an edge to restore");

    // One file gains a declaration, so one digest differs from its `file` row.
    repo.dir.write(
        WIDGET,
        &format!("{WIDGET_SOURCE}\n\ndef helper(value):\n    return value\n"),
    );

    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
    let second = latest_generation(&repo.live());
    assert_eq!(generation_count(&repo.live()), 2);
    assert_ne!(second.id, first.id);

    // The Fast run rewrites the *whole* discovered unit set: both `file` rows name the new
    // generation, not only the one whose digest moved. That is the price the module documentation
    // states — the hash comparison decides *whether* to publish, not how much.
    let files = file_rows(&repo.live());
    for file in &files {
        assert_eq!(file.indexed_generation, second.id, "{}", file.path);
    }
    // The changed unit's new symbol landed.
    assert_eq!(
        count(&repo.live(), "node"),
        first.nodes + 1,
        "widget.py gained exactly one declaration"
    );

    // And the unchanged caller's edge into the changed file survived the cascade: `persist_unit`
    // replaces widget.py's nodes, which takes `run → render` with it unless `restore_edges` puts the
    // full resolved set back.
    assert_eq!(
        edge_count(
            &repo.live(),
            "e.kind = 'calls' AND s.name = 'run' AND d.name = 'render'"
        ),
        1
    );
    assert_eq!(second.edges, edges_before + 1);

    // And the tree is idle again, with the bytes from this publish untouched.
    let before = live_digest(&repo.live());
    assert_eq!(run_worker(&repo.options()), WorkerExit::NothingToDo);
    assert_eq!(live_digest(&repo.live()), before);
}

#[test]
fn a_held_lock_is_locked_and_writes_nothing() {
    let repo = Repo::new("locked", &[]);
    fs::create_dir_all(repo.data_dir()).expect("create the data directory");
    // The server holds the lock while a worker runs; a second worker must not queue or block. Taken
    // in-process through the store: the same exclusive create a second process would contend for.
    let held = acquire_lock(&repo.data_dir()).expect("hold the write lock");
    assert!(lock_path(&repo.data_dir()).exists());
    let outcome = run_worker(&repo.options());
    assert_eq!(outcome, WorkerExit::Locked);
    // 75 is a refusal, not a run: no graph was created and the holder's lock is untouched.
    assert!(!repo.live().exists());
    assert!(lock_is_held(&repo.data_dir()));
    assert_eq!(
        inspect(&repo.data_dir()).expect("inspect").pid,
        Some(std::process::id())
    );

    release(&held).expect("release the write lock");
    // The next run is not blocked by the released lock.
    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
}

#[test]
fn a_tree_with_no_parseable_files_still_publishes() {
    // Nothing discovery can index: the one file's extension has no grammar, so the walk reports it
    // as coverage and the plan has zero changed and zero removed paths.
    let repo = Repo::new("empty", &[("notes.txt", "not source\n")]);
    repo.dir.remove(CALLER);
    repo.dir.remove(WIDGET);

    let outcome = run_worker(&repo.options());
    assert_eq!(outcome, WorkerExit::Published);

    let published = latest_generation(&repo.live());
    assert_eq!(published.files_indexed, 0);
    assert_eq!(published.nodes, 0);
    assert_eq!(published.edges, 0);
    assert_eq!(count(&repo.live(), "file"), 0);
    assert_eq!(count(&repo.live(), "node"), 0);
    assert_eq!(count(&repo.live(), "edge"), 0);
    // The path is still accounted for, which is what makes "no callers in notes.txt" answerable.
    assert_eq!(count(&repo.live(), "not_indexed"), 1);
    assert_eq!(meta(&repo.live(), "indexer_state").as_deref(), Some("idle"));
}

#[test]
fn a_deleted_file_leaves_a_tombstone_and_drops_its_row() {
    let repo = Repo::new("removed", &[]);
    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
    let first = latest_generation(&repo.live());
    assert_eq!(
        file_rows(&repo.live())
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![CALLER, WIDGET]
    );

    // The caller's bytes leave the tree and a path no grammar covers appears: the first must stop
    // being in the graph, the second must be in coverage with the reason discovery gave it.
    repo.dir.remove(CALLER);
    repo.dir.write("notes.txt", "not source\n");

    assert_eq!(run_worker(&repo.options()), WorkerExit::Published);
    assert_eq!(
        file_rows(&repo.live())
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![WIDGET]
    );

    // The removed symbols are tombstones named by the generation that removed them, so
    // `summary(change)` can report them after the rows are gone.
    let removed_by = latest_generation(&repo.live()).id;
    assert_ne!(removed_by, first.id);
    let conn = reader(&repo.live());
    let tombstones = {
        let mut statement = conn
            .connection()
            .prepare("SELECT removed_in_generation, last_seen_generation FROM tombstone")
            .expect("prepare the tombstones");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query the tombstones")
            .collect::<Result<Vec<(String, String)>, _>>()
            .expect("collect the tombstones")
    };
    conn.close().expect("close the reader");
    assert!(
        !tombstones.is_empty(),
        "the removed file's symbols are gone"
    );
    for (removed, last_seen) in tombstones {
        assert_eq!(removed, removed_by);
        assert_eq!(last_seen, first.id);
    }

    assert_eq!(
        coverage(&repo.live()),
        vec![("notes.txt".to_owned(), "unsupported_ext".to_owned())]
    );
}

/// Every coverage row, in path order, so an assertion reads as the report a payload would render.
fn coverage(live: &Path) -> Vec<(String, String)> {
    let conn = reader(live);
    let rows = {
        let mut statement = conn
            .connection()
            .prepare("SELECT path, reason FROM not_indexed ORDER BY path")
            .expect("prepare the coverage rows");
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query the coverage rows")
            .collect::<Result<Vec<(String, String)>, _>>()
            .expect("collect the coverage rows")
    };
    conn.close().expect("close the reader");
    rows
}

#[test]
fn a_graph_from_a_newer_build_is_failed_and_recorded() {
    let repo = Repo::new("newer", &[]);
    fs::create_dir_all(repo.data_dir()).expect("create the data directory");
    // A graph written by a newer build: current schema, but the ledger says a higher version. The
    // runner must refuse it rather than let this build overwrite a graph it cannot read.
    {
        let mut writer = open_writer(repo.live()).expect("open the live graph");
        migrate(writer.connection_mut()).expect("migrate the fixture graph");
        writer.close().expect("close the writer");
    }
    {
        let writer = open_writer(repo.live()).expect("reopen the live graph");
        writer
            .connection()
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = ?2",
                params!["99", "schema_version"],
            )
            .expect("bump the recorded schema version");
        writer.close().expect("close the writer");
    }
    let nodes = count(&repo.live(), "node");

    assert_eq!(run_worker(&repo.options()), WorkerExit::Failed);

    // No generation was published, and the failure is the fact `status.health` reports.
    assert_eq!(generation_count(&repo.live()), 0);
    assert_eq!(count(&repo.live(), "node"), nodes);
    assert_eq!(
        meta(&repo.live(), "indexer_state").as_deref(),
        Some("failed")
    );
    assert_eq!(
        meta(&repo.live(), "consecutive_failures").as_deref(),
        Some("1")
    );
    // The lock is released on the failure path too, and no stage survives it.
    assert!(!lock_is_held(&repo.data_dir()));
    assert_eq!(stage_files(&repo.data_dir()), Vec::<String>::new());

    // A second failure counts as a second consecutive one, not a reset.
    assert_eq!(run_worker(&repo.options()), WorkerExit::Failed);
    assert_eq!(
        meta(&repo.live(), "consecutive_failures").as_deref(),
        Some("2")
    );
}

#[test]
fn an_unusable_data_dir_is_failed() {
    let repo = Repo::new("no-data-dir", &[]);
    // A regular file where the data directory has to be: the lock cannot even be created, so this is
    // exit 1 and not 75 — the lock was never contended, the directory is unusable.
    let blocked = repo.root().join("not-a-directory");
    fs::write(&blocked, "occupied\n").expect("write a file where the data dir would be");

    let outcome = run_worker(&WorkerOptions {
        repo_root: repo.root().to_path_buf(),
        data_dir: blocked,
        force: false,
        mode: None,
    });
    assert_eq!(outcome, WorkerExit::Failed);
}
