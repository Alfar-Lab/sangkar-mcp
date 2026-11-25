//! `index` driven against a fake worker: no index, no database, and never this repository's graph.
//!
//! Ported from `tests/tools-index.test.ts`. The tool's two hard properties are asserted here: the
//! mapping from a child's exit status onto an envelope, and the child command coming from
//! `AppConfig` rather than from anything the handler can read.
//!
//! **How "the handler never reads the environment" is proven.** The fake worker is a shell script in
//! a temp dir, and the only thing that knows its path is the [`AppConfig`] a fixture resolved —
//! through `load_config`'s *injected* lookup, so the process environment is never mutated. `handle`
//! takes `&ToolContext` (which owns that config) and `&IndexArgs` and nothing else, and
//! [`the_handler_reads_no_environment_and_writes_no_stdout`] scans the module's own source for a
//! process-state read or a stdout write. Every test below would fail at spawn time if the path had
//! to arrive any other way, because no other way exists.
//!
//! Five cases drive the real spawn — exit `0`, `2`, `75`, `1` and a command that cannot start — and
//! a sixth proves the backstop kills a child that never exits. The mapping cases that a shell script
//! cannot produce (a signal, an exit `3`, a malformed `0` report) go through [`handle_with`]'s spawn
//! seam, which is the Rust equivalent of the TypeScript `handle`'s `run` parameter.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sangkar_core::config::load_config;
use sangkar_core::cursor::Cursor;
use sangkar_core::envelope::{Status, TestCensus};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log::Level;
use sangkar_core::registry::{Registry, canonical_root, read_registry};
use sangkar_store::read::context::{CursorFacade, LogField, Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};
use serde_json::{Map, Value, json};

use sangkar_tools::index::handler::{
    IndexFileCounts, IndexPayload, WorkerEvent, WorkerExit, WorkerReport, handle, handle_with,
    stream_worker_within, worker_invocation,
};
use sangkar_tools::index::manifest::{
    DESCRIPTION, EXAMPLE, ExampleValue, IndexArgs, IndexMode, MODES, NAME, PARAMETERS,
    ParamDefault, ParamType,
};

/// The generation the fixture graph is serving, and therefore the run's `previous_generation`.
const GENERATION_LIVE: &str = "g-prev";
/// When that generation was published; `GraphMeta` has no `indexed_at`, so the reader's newest
/// generation row is the only published timestamp there is.
const INDEXED_AT: &str = "2026-01-03T00:00:00Z";
/// The one clock every call reads, so a `read_status` assertion is a constant comparison.
const NOW: &str = "2026-01-04T12:00:00.000Z";
/// Above the cursor module's 32-byte key floor; the tool never uses it.
const KEY: &str = "index-test-cursor-key-0123456789abc";

// ---------------------------------------------------------------------------------------------
// The fixture: a temp tree, a temp home, and a worker the config names.
// ---------------------------------------------------------------------------------------------

/// A directory under the system temp dir, removed when the fixture is dropped.
///
/// Hand-rolled because the crates carry no `tempfile` dependency, and the name carries the pid and a
/// nanosecond stamp so two concurrently running tests cannot share one.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sangkar-index-tool-{}-{stamp}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create the test dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn join(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// One stderr line in the child's own log format (`{ts, level, msg, …}`), built by the serializer so
/// the fixture cannot spell invalid JSON.
fn line(msg: &str, level: &str, fields: &[(&str, Value)]) -> String {
    let mut record = Map::new();
    record.insert("ts".to_owned(), json!(NOW));
    record.insert("level".to_owned(), json!(level));
    record.insert("msg".to_owned(), json!(msg));
    for (key, value) in fields {
        record.insert((*key).to_owned(), value.clone());
    }
    serde_json::to_string(&record).expect("a log line")
}

/// Writes an executable shell script named `name` in `dir`.
fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).expect("write the fake worker");
    let mut permissions = fs::metadata(&path)
        .expect("stat the fake worker")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("make the fake worker executable");
    path
}

/// A fake worker: it prints each line on stderr and exits with `code`.
///
/// A here-document rather than `printf`, so the child reports exactly the bytes the test built — no
/// quoting, no format specifiers, no shell expansion.
fn worker_script(dir: &TempDir, name: &str, lines: &[String], code: i32) -> PathBuf {
    let mut body = String::from("#!/bin/sh\ncat >&2 <<'WORKER_STDERR'\n");
    for line in lines {
        body.push_str(line);
        body.push('\n');
    }
    body.push_str("WORKER_STDERR\n");
    body.push_str(&format!("exit {code}\n"));
    write_script(dir, name, &body)
}

/// A fake worker plus the context a handler receives.
struct Fixture {
    temp: TempDir,
    ctx: ToolContext,
}

impl Fixture {
    /// A fixture whose worker prints `lines` on stderr and exits with `code`.
    fn with_script(name: &str, lines: &[String], code: i32) -> Self {
        let temp = TempDir::new();
        let script = worker_script(&temp, name, lines, code);
        Self {
            ctx: context(&temp, &script),
            temp,
        }
    }

    /// A fixture whose worker is a script written verbatim, for a child that does not end on its own.
    fn with_body(name: &str, body: &str) -> Self {
        let temp = TempDir::new();
        let script = write_script(&temp, name, body);
        Self {
            ctx: context(&temp, &script),
            temp,
        }
    }

    /// A fixture whose worker path nothing exists at: the spawn itself fails.
    fn with_missing_worker() -> Self {
        let temp = TempDir::new();
        let missing = temp.join("missing-worker.sh");
        Self {
            ctx: context(&temp, &missing),
            temp,
        }
    }

    fn registry(&self) -> Registry {
        read_registry(&self.ctx.config.registry_path)
    }

    fn registered(&self) -> bool {
        self.ctx.config.registry_path.exists()
    }
}

/// A context over a fake reader, a real cursor (unused), a recording logger and a temp config.
fn context(dir: &TempDir, worker: &Path) -> ToolContext {
    let repo_root = dir.join("repo");
    let data_dir = dir.join("data");
    let home = dir.join("home");
    fs::create_dir_all(&repo_root).expect("create the test tree");
    fs::create_dir_all(&home).expect("create the test home");

    // The tree and the data dir are named as flags, and the worker arrives as `SANGKAR_WORKER_CMD`
    // through the *injected* lookup: this test states a machine, it does not mutate the one it runs
    // on. `worker_cmd` splits on whitespace, so the path must not contain a space — no temp dir name
    // this fixture makes does.
    let argv = vec![
        "--repo-root".to_owned(),
        repo_root.display().to_string(),
        "--data-dir".to_owned(),
        data_dir.display().to_string(),
    ];
    let worker_cmd = worker.display().to_string();
    let sangkar_home = home.display().to_string();
    let config = load_config(
        &argv,
        &move |key: &str| match key {
            "SANGKAR_HOME" => Some(sangkar_home.clone()),
            "SANGKAR_WORKER_CMD" => Some(worker_cmd.clone()),
            _ => None,
        },
        &repo_root,
    )
    .expect("resolve the fixture config");
    // No test may reach the machine's real registry, whatever `$HOME` says.
    assert!(config.registry_path.starts_with(dir.path()));

    let reader: Rc<dyn GraphReader> = Rc::new(FakeReader);
    let cursor: Rc<dyn CursorFacade> =
        Rc::new(Cursor::new(KEY.as_bytes().to_vec(), None).expect("bind the test cursor key"));
    ToolContext {
        reader,
        cursor,
        log: Logger::with_sink(NAME, record_line),
        now: clock,
        config: Rc::new(config),
        db_size_bytes: 0,
    }
}

fn clock() -> String {
    NOW.to_owned()
}

thread_local! {
    /// The lines this test thread logged. A thread-local rather than a global because libtest runs
    /// each test on its own thread and a shared collector would interleave two tests' lines.
    static LOG_LINES: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

fn record_line(level: Level, msg: &str, _fields: &[(&str, LogField)]) {
    LOG_LINES.with(|lines| {
        lines
            .borrow_mut()
            .push((level.as_str().to_owned(), msg.to_owned()));
    });
}

fn logged_since(mark: usize) -> Vec<(String, String)> {
    LOG_LINES.with(|lines| lines.borrow()[mark..].to_vec())
}

fn log_mark() -> usize {
    LOG_LINES.with(|lines| lines.borrow().len())
}

// ---------------------------------------------------------------------------------------------
// A reader that answers the two questions an index call asks and refuses every other one.
// ---------------------------------------------------------------------------------------------

struct FakeReader;

impl GraphReader for FakeReader {
    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: Some(GENERATION_LIVE.to_owned()),
            generation_seq: Some(1),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("fake".to_owned()),
            project_root: Some("/tmp/fake".to_owned()),
            project_branch: None,
            project_remote: None,
            embed_dim: None,
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        Ok(Some(GenerationRow {
            id: GENERATION_LIVE.to_owned(),
            seq: 1,
            published_at: INDEXED_AT.to_owned(),
            base_commit: None,
            worktree_dirty: false,
            files_indexed: 1,
            nodes: 0,
            edges: 0,
            duration_ms: 0,
            mode: "fast".to_owned(),
            status: "ok".to_owned(),
        }))
    }

    fn find_by_qn(&self, _qn: &str) -> Result<Option<NodeRow>, AppError> {
        unreachable!("index never reads a node")
    }

    fn find_by_name(
        &self,
        _name: &str,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("index never resolves a name")
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("index never expands a container")
    }

    fn nodes_by_file(&self, _file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("index never reads a file's nodes")
    }

    fn inbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        unreachable!("index never walks edges")
    }

    fn outbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        unreachable!("index never walks edges")
    }

    fn metrics_for(&self, _node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        unreachable!("index never reads metrics")
    }

    fn rows_for_paths(&self, _prefixes: &[String]) -> Result<PathCoverage, AppError> {
        unreachable!("index reports the run's own coverage, not the graph's")
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError> {
        unreachable!("index reports the run's own coverage, not the graph's")
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        unreachable!("index reports the run's own coverage, not the graph's")
    }

    fn unresolved_for_files(&self, _file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        unreachable!("index never reads unresolved call sites")
    }

    fn file_by_path(&self, _path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        unreachable!("index never reads an indexed file")
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        unreachable!("index never lists files")
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        unreachable!("index never reads a language census")
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        unreachable!("index never reads the test census")
    }

    fn removed_symbols_for(&self, _since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        unreachable!("index never reads tombstones")
    }

    fn entry_points(&self, _limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        unreachable!("index never ranks entry points")
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        unreachable!("index never ranks hotspots")
    }

    fn test_gaps(&self, _ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        unreachable!("index is not a coverage report")
    }

    fn read_node_fts(&self, _match_query: &str, _limit: usize) -> Result<Vec<FtsHit>, AppError> {
        unreachable!("index does not search")
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers for the seam-driven cases.
// ---------------------------------------------------------------------------------------------

/// An event whose line is not JSON: the raw text is the whole diagnosis.
fn raw_event(msg: &str, level: Level, raw: &str) -> WorkerEvent {
    WorkerEvent {
        msg: msg.to_owned(),
        level,
        fields: Map::new(),
        raw: raw.to_owned(),
    }
}

fn report(exit: WorkerExit, events: Vec<WorkerEvent>) -> WorkerReport {
    WorkerReport {
        exit,
        killed_after: None,
        events,
    }
}

fn payload_of(env: &sangkar_core::envelope::Envelope<IndexPayload>) -> &IndexPayload {
    assert_eq!(env.status, Status::Ok, "{:?}", env.error);
    env.payload
        .as_ref()
        .expect("an ok envelope carries a payload")
}

// ---------------------------------------------------------------------------------------------
// The manifest.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_manifest_keeps_the_description_verbatim_and_the_documented_defaults() {
    assert_eq!(NAME, "index");
    // `TOOLS-SPEC.md` §8 byte for byte: an admin client reads this before the one tool that
    // rewrites the whole graph, so a paraphrase would be a behaviour change.
    assert_eq!(
        DESCRIPTION,
        concat!(
            "Admin: force a full or fast index, or repair a broken one. ",
            "Not for normal use — the server keeps the graph current automatically."
        )
    );

    let names: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(names, ["force", "mode", "repo", "register"]);

    let force = &PARAMETERS[0];
    assert_eq!(force.ty, ParamType::Bool);
    assert!(!force.required);
    assert_eq!(force.default, Some(ParamDefault::Bool(false)));

    let mode = &PARAMETERS[1];
    assert_eq!(mode.ty, ParamType::Enum(&MODES));
    assert_eq!(mode.default, Some(ParamDefault::Text("full")));

    // A repository named by nobody is the one the root is bound to, so `repo` carries no default.
    let repo = &PARAMETERS[2];
    assert_eq!(repo.ty, ParamType::Str);
    assert!(!repo.required);
    assert_eq!(repo.default, None);

    let register = &PARAMETERS[3];
    assert_eq!(register.ty, ParamType::Bool);
    assert_eq!(register.default, Some(ParamDefault::Bool(true)));

    assert_eq!(EXAMPLE, [("mode", ExampleValue::Text("fast"))]);

    // The handler applies the same constants the table declares, so the two cannot drift.
    let defaults = IndexArgs::default();
    assert!(!defaults.force());
    assert_eq!(defaults.mode(), IndexMode::Full);
    assert!(defaults.register());
}

// ---------------------------------------------------------------------------------------------
// The invocation: the migration knob, and the tree the child acts on.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_invocation_runs_the_worker_named_by_config_against_the_bound_tree() {
    let fixture = Fixture::with_script("published.sh", &[], 0);
    let invocation = worker_invocation(&fixture.ctx, &IndexArgs::default()).expect("an invocation");
    let argv: Vec<String> = invocation
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();

    // The program is the one `AppConfig::worker_cmd` names — the fake script — not this test
    // process and not a shell.
    assert_eq!(
        PathBuf::from(&argv[0]),
        PathBuf::from(fixture.ctx.config.worker_cmd[0].clone())
    );
    assert_eq!(&argv[1..3], ["worker", "--repo-root"]);
    assert_eq!(argv[3], fixture.ctx.config.repo_root.display().to_string());
    assert_eq!(argv[4], "--data-dir");
    assert_eq!(argv[5], fixture.ctx.config.data_dir.display().to_string());
    assert_eq!(&argv[6..8], ["--mode", "full"]);
    assert_eq!(argv.len(), 8);
    assert!(!argv.contains(&"--force".to_owned()));
    // The two paths travel as argv flags, never in the environment: the invocation carries no
    // environment map at all, so the child inherits this process's and nothing else.
    assert!(!invocation.command_line().contains("SANGKAR_REPO_ROOT"));
    assert!(!invocation.command_line().contains("SANGKAR_DATA_DIR"));

    // `--force` is a bare flag and comes last, which is the order `bin/sangkar.ts` parses.
    let forced = worker_invocation(
        &fixture.ctx,
        &IndexArgs {
            force: Some(true),
            mode: Some(IndexMode::Fast),
            ..IndexArgs::default()
        },
    )
    .expect("an invocation");
    let argv: Vec<String> = forced
        .cmd()
        .iter()
        .map(|part| part.to_string_lossy().into_owned())
        .collect();
    assert_eq!(argv.last().map(String::as_str), Some("--force"));
    assert_eq!(&argv[6..8], ["--mode", "fast"]);
}

// ---------------------------------------------------------------------------------------------
// The five exit-status mappings, each through a real spawn of the fake worker.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_exit_0_publishes_the_counts_the_worker_printed() {
    let fixture = Fixture::with_script(
        "published.sh",
        &[
            line(
                "worker.file_not_parsed",
                "warn",
                &[
                    ("path", json!("docs/broken.ts")),
                    ("reason", json!("grammar_unavailable")),
                ],
            ),
            line(
                "worker.published",
                "info",
                &[
                    ("generation", json!("g-new")),
                    ("repoRoot", json!("/repo")),
                    ("files", json!(12)),
                    ("changed", json!(12)),
                    ("removed", json!(0)),
                    ("edges", json!(340)),
                    ("durationMs", json!(1450)),
                    ("mode", json!("full")),
                ],
            ),
        ],
        0,
    );

    let env = handle(&fixture.ctx, &IndexArgs::default());
    assert_eq!(env.status, Status::Ok, "{:?}", env.error);
    let payload = payload_of(&env);
    assert_eq!(payload.mode, IndexMode::Full);
    assert_eq!(payload.duration_ms, 1450);
    assert_eq!(
        payload.previous_generation.as_deref(),
        Some(GENERATION_LIVE)
    );
    // 12 offered, 1 of them logged a parse failure, so 11 produced a unit.
    assert_eq!(
        payload.files,
        IndexFileCounts {
            scanned: 12,
            parsed: 11,
            failed: 1,
            skipped: 0,
        }
    );
    assert_eq!(env.read_status.generation, "g-new");
    assert!(!env.read_status.stale);
    assert_eq!(env.read_status.indexed_at, NOW);
    assert_eq!(env.coverage.indexed, 11);
    assert_eq!(env.coverage.failed, 1);
    // The child never reports the gap list, so coverage may not claim completeness.
    assert!(!env.coverage.complete);
    assert!(
        env.summary
            .starts_with("Full index published: 11 file(s) parsed, 1 failed")
    );
    assert!(env.summary.contains("340 edge(s)"));
    // 1450 ms is `(1.45).toFixed(1)`: `1.4s`, because 1.45 is 1.4499… in binary — the same
    // number the TypeScript renders, not a rounding policy of this port's own.
    assert!(env.summary.ends_with("in 1.4s."), "{}", env.summary);
    assert!(!env.truncated);
    assert_eq!(env.next, None);

    // The counts are integers on the wire: an integral `f64` would render `1450.0` where
    // `JSON.stringify` writes `1450`, which is a byte the parity gate compares.
    let text = serde_json::to_string(&env).expect("serialise the answer");
    assert!(text.contains(r#""duration_ms":1450"#), "{text}");
    assert!(
        text.contains(r#""files":{"scanned":12,"parsed":11,"failed":1,"skipped":0}"#),
        "the payload key is `scanned`, the name handler.ts:57 gives it: {text}"
    );
    assert!(!text.contains("1450.0"));

    // An exit 0 registers the tree it indexed, and the answer is the run's report untouched by it.
    assert_eq!(
        fixture.registry().repos,
        vec![canonical_root(&fixture.ctx.config.repo_root, None)]
    );
    assert!(
        fixture
            .ctx
            .config
            .registry_path
            .starts_with(fixture.temp.path())
    );
    let registered = logged_since(0);
    let names: Vec<&str> = registered
        .iter()
        .filter(|(_, msg)| msg == "index.registered")
        .map(|(_, msg)| msg.as_str())
        .collect();
    assert_eq!(names, ["index.registered"]);
    assert!(
        registered
            .iter()
            .any(|(level, msg)| level == "info" && msg == "index.registered")
    );
}

#[test]
fn an_exit_2_reports_nothing_to_do_and_still_registers() {
    let fixture = Fixture::with_script(
        "nothing.sh",
        &[line("worker.nothing_to_do", "info", &[("files", json!(7))])],
        2,
    );

    let env = handle(
        &fixture.ctx,
        &IndexArgs {
            mode: Some(IndexMode::Fast),
            ..IndexArgs::default()
        },
    );
    assert_eq!(env.status, Status::Ok, "{:?}", env.error);
    let payload = payload_of(&env);
    assert_eq!(payload.mode, IndexMode::Fast);
    // The offered count is the child's; nothing was parsed or failed, and discovery's skipped paths
    // are not counted by the child at all.
    assert_eq!(
        payload.files,
        IndexFileCounts {
            scanned: 7,
            parsed: 0,
            failed: 0,
            skipped: 0,
        }
    );
    assert_eq!(
        payload.previous_generation.as_deref(),
        Some(GENERATION_LIVE)
    );
    assert!(
        env.summary
            .starts_with("Nothing to do: the graph already matches")
    );
    assert!(env.summary.contains("(7 files scanned)"));
    // Nothing published, so the live generation answered — and it is the tree the run just
    // confirmed, so it is not stale.
    assert_eq!(env.read_status.generation, GENERATION_LIVE);
    assert_eq!(env.read_status.indexed_at, INDEXED_AT);
    assert!(!env.read_status.stale);
    // The tree matched the graph, but it is still the tree: an exit 2 registers too.
    assert_eq!(
        fixture.registry().repos,
        vec![canonical_root(&fixture.ctx.config.repo_root, None)]
    );
}

#[test]
fn an_exit_75_is_a_retryable_lock_with_a_hint_naming_the_lock_file() {
    let fixture = Fixture::with_script(
        "locked.sh",
        &[line(
            "worker.locked",
            "warn",
            &[("pid", json!(4242)), ("retryAfterMs", json!(1000))],
        )],
        75,
    );

    let env = handle(&fixture.ctx, &IndexArgs::default());
    assert_eq!(env.status, Status::Error);
    let error = env.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INDEX_LOCKED);
    assert_eq!(error.retryable, Some(true));
    assert_eq!(error.retry_after_ms, Some(1000));
    // The holder is named, and the hint points at the file the operator would delete.
    assert!(error.message.contains("4242"), "{}", error.message);
    let hint = error.hint.as_deref().expect("a hint");
    assert!(hint.contains("write.lock"), "{hint}");
    assert!(
        hint.contains(&fixture.ctx.config.data_dir.display().to_string()),
        "{hint}"
    );
    assert!(env.payload.is_none());
    // The live generation did not move, and it is stale because this call published nothing.
    assert_eq!(env.read_status.generation, GENERATION_LIVE);
    assert!(env.read_status.stale);
    // A lock is not a successful run: nothing was registered.
    assert!(!fixture.registered());
}

#[test]
fn an_exit_1_is_internal_and_carries_the_workers_own_diagnosis() {
    let fixture = Fixture::with_script(
        "failed.sh",
        &[line(
            "worker.failed",
            "error",
            &[("reason", json!("SQLiteError: no such table: file"))],
        )],
        1,
    );

    let env = handle(&fixture.ctx, &IndexArgs::default());
    assert_eq!(env.status, Status::Error);
    let error = env.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains("SQLiteError: no such table: file"),
        "{}",
        error.message
    );
    assert!(error.message.contains("exit 1"));
    assert!(env.payload.is_none());
    assert!(!fixture.registered());
}

#[test]
fn a_worker_that_cannot_be_started_names_the_command() {
    let fixture = Fixture::with_missing_worker();

    let env = handle(&fixture.ctx, &IndexArgs::default());
    assert_eq!(env.status, Status::Error);
    let error = env.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains("could not start the index worker"),
        "{}",
        error.message
    );
    // The failed command is the sentence's subject: a caller of a repair tool needs to know what
    // was not there, not just that something was not.
    assert!(
        error.message.contains("missing-worker.sh"),
        "{}",
        error.message
    );
    assert!(env.payload.is_none());
    assert!(!fixture.registered());
}

// ---------------------------------------------------------------------------------------------
// The mappings a shell script cannot produce: a malformed report, another status, a signal.
// ---------------------------------------------------------------------------------------------

#[test]
fn an_exit_0_without_a_published_line_is_a_broken_contract_not_an_empty_success() {
    let fixture = Fixture::with_script("empty.sh", &[], 0);

    let env = handle_with(&fixture.ctx, &IndexArgs::default(), |_, _| {
        Ok(report(WorkerExit::Code(0), Vec::new()))
    });
    assert_eq!(env.status, Status::Error);
    let error = env.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains("without publishing a report"),
        "{}",
        error.message
    );
    assert!(env.payload.is_none());
    // The status promised a successful run, so the tree was indexed even though the report was
    // malformed; the TypeScript registers on the status, not on the envelope it managed to build.
    assert!(fixture.registered());
}

#[test]
fn an_unexpected_status_and_a_signal_both_report_the_childs_last_line() {
    let fixture = Fixture::with_script("unexpected.sh", &[], 0);

    let unexpected = handle_with(&fixture.ctx, &IndexArgs::default(), |_, _| {
        Ok(report(
            WorkerExit::Code(3),
            vec![raw_event("worker.stderr", Level::Warn, "boom")],
        ))
    });
    assert_eq!(unexpected.status, Status::Error);
    let error = unexpected.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(error.message.contains("exit 3"), "{}", error.message);
    assert!(error.message.contains("boom"), "{}", error.message);

    let killed = handle_with(&fixture.ctx, &IndexArgs::default(), |_, _| {
        Ok(report(
            WorkerExit::Signal(Some(11)),
            vec![raw_event(
                "worker.stderr",
                Level::Warn,
                "segmentation fault",
            )],
        ))
    });
    assert_eq!(killed.status, Status::Error);
    let error = killed.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error.message.contains("killed by signal 11"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("segmentation fault"),
        "{}",
        error.message
    );
}

// ---------------------------------------------------------------------------------------------
// Registration: gated on the status, never able to change the answer.
// ---------------------------------------------------------------------------------------------

#[test]
fn register_false_indexes_without_publishing_the_tree_to_the_registry() {
    let fixture = Fixture::with_script(
        "published.sh",
        &[line(
            "worker.published",
            "info",
            &[("files", json!(3)), ("mode", json!("full"))],
        )],
        0,
    );

    let env = handle(
        &fixture.ctx,
        &IndexArgs {
            register: Some(false),
            ..IndexArgs::default()
        },
    );
    assert_eq!(payload_of(&env).files.scanned, 3);
    assert!(!fixture.registered());
    assert!(
        logged_since(0)
            .iter()
            .all(|(_, msg)| msg != "index.registered")
    );
}

#[test]
fn an_unwritable_registry_warns_once_and_leaves_the_published_answer_alone() {
    let fixture = Fixture::with_script(
        "published.sh",
        &[line(
            "worker.published",
            "info",
            &[("generation", json!("g-new")), ("files", json!(3))],
        )],
        0,
    );
    // A file where the home directory was: the registry's own path cannot be written, so the write
    // is refused rather than thrown — registering is a convenience for the next call.
    let home = fixture.ctx.config.registry_path.parent().expect("a home");
    fs::remove_dir_all(home).expect("remove the empty home");
    fs::write(home, "not a directory").expect("block the registry");
    let mark = log_mark();

    let env = handle(&fixture.ctx, &IndexArgs::default());

    assert_eq!(env.status, Status::Ok, "{:?}", env.error);
    assert_eq!(env.read_status.generation, "g-new");
    assert_eq!(payload_of(&env).files.scanned, 3);
    let lines = logged_since(mark);
    let warned: Vec<&(String, String)> = lines
        .iter()
        .filter(|(_, msg)| msg == "index.register_failed")
        .collect();
    assert_eq!(warned.len(), 1, "{lines:?}");
    assert_eq!(warned[0].0, "warn");
    assert!(
        lines.iter().all(|(_, msg)| msg != "index.registered"),
        "{lines:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The backstop, and the one property the type signature is supposed to make structural.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_worker_that_never_exits_is_killed_at_the_deadline() {
    let fixture = Fixture::with_body(
        "hang.sh",
        &format!(
            "#!/bin/sh\ncat >&2 <<'WORKER_STDERR'\n{}\nWORKER_STDERR\nexec sleep 30\n",
            line("worker.change_plan", "debug", &[("discovered", json!(2))])
        ),
    );
    let timeout = Duration::from_millis(200);
    let invocation = worker_invocation(&fixture.ctx, &IndexArgs::default()).expect("an invocation");

    let report = stream_worker_within(&fixture.ctx, &invocation, timeout).expect("a run");
    assert!(report.killed_after.is_some(), "the backstop fired");
    assert!(
        matches!(report.exit, WorkerExit::Signal(_)),
        "a killed child has no status byte: {:?}",
        report.exit
    );
    // What it managed to write before the kill is still the report.
    let messages: Vec<&str> = report
        .events
        .iter()
        .map(|event| event.msg.as_str())
        .collect();
    assert_eq!(messages, ["worker.change_plan"]);

    // The same run, mapped: a kill this tool decided on is not a child failure, and the message says
    // which it was.
    let env = handle_with(&fixture.ctx, &IndexArgs::default(), |ctx, invocation| {
        stream_worker_within(ctx, invocation, timeout)
    });
    assert_eq!(env.status, Status::Error);
    let error = env.error.as_ref().expect("an error payload");
    assert_eq!(error.code, ErrorCode::INTERNAL);
    assert!(
        error
            .message
            .contains("did not finish and was killed after"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("worker.change_plan"),
        "{}",
        error.message
    );
}

/// The Done-when's second clause, asserted against the module's own bytes rather than by review:
/// no handler reads process state, and nothing in the module writes to stdout.
///
/// The two are structural, not incidental. `handle` takes `&ToolContext` and `&IndexArgs`; the only
/// thing that names the child program is `AppConfig::worker_cmd`, and every test above supplies it
/// through an injected lookup. A `std::env::var` here would be the first one in a tool module, and
/// `println!` would corrupt the MCP stream — so both are refused at the source, not at review time.
#[test]
fn the_handler_reads_no_environment_and_writes_no_stdout() {
    let sources = [
        ("handler.rs", include_str!("../src/index/handler.rs")),
        ("manifest.rs", include_str!("../src/index/manifest.rs")),
        ("mod.rs", include_str!("../src/index/mod.rs")),
    ];
    for (name, source) in sources {
        assert!(
            !source.contains("std::env"),
            "{name} reads process state; the config is the only seam"
        );
        // `eprintln!` contains `println!`, so this pair bans all three writers.
        assert!(
            !source.contains("println!"),
            "{name} writes a diagnostic to stdout or stderr instead of the logger"
        );
        assert!(!source.contains("print!("), "{name} writes to stdout");
    }
    // The one process the tool starts is spawned through `std::process`, with an argv that is
    // already split — never a shell, and never an environment the handler supplied.
    assert!(sources[0].1.contains("use std::process::"));
    assert!(sources[0].1.contains("Command::new("));
    assert!(!sources[0].1.contains("sh -c"));
    assert!(!sources[0].1.contains(".env("));
    // The child program is the configured vector, not a path this module knows.
    assert!(sources[0].1.contains("worker_cmd"));
}
