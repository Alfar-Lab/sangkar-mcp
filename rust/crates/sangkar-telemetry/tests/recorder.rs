//! The recorder contract, executable.
//!
//! Mirrors `tests/telemetry.test.ts` and `tests/for-repo.test.ts`: the two flush triggers at their
//! boundaries, the per-session `seq` across a flush, one append per UTC date, the session record that
//! is buffered before any call, the per-repository gate with all three switches, the null recorder's
//! silence, and a sink that fails without taking a record — or an answer — down with it.
//!
//! Both seams are exercised through the interfaces rather than through sleeps: the clock is a dial a
//! test moves ([`sangkar_telemetry::telemetry::Now`]) and the sink records the arguments of every
//! append, so "no file" and "one file" are assertions and not timing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use sangkar_core::config::RepoConfigSource;
use sangkar_core::repo_config::RepoConfig;
use sangkar_core::runtime::{arch, os};
use sangkar_telemetry::for_repo::{RepoTelemetryInput, telemetry_for_repo};
use sangkar_telemetry::record::{
    CallSpec, Counts, IndexMode, IndexRunSpec, RecordContext, RecordStatus, SessionMode,
};
use sangkar_telemetry::sink::TelemetrySink;
use sangkar_telemetry::telemetry::{
    FLUSH_AT_MS, FLUSH_AT_RECORDS, Now, Recorder, TelemetryOptions, create, null,
};

/// `2026-11-18T10:00:00.000Z`, the instant the record suite pins, as epoch milliseconds.
const START_MS: i64 = 1_794_996_000_000;
const START_DATE: &str = "2026-11-18";
/// `2026-11-19T00:00:00.000Z`: the first instant of the next UTC day.
const NEXT_DAY_MS: i64 = 1_795_046_400_000;
const NEXT_DATE: &str = "2026-11-19";
const SERVER_VERSION: &str = "0.6.0-beta.1";

/// A temporary directory that removes itself, so the suite leaves no telemetry directory behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-recorder-{label}-{}-{nanos}-{}",
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

    /// The repository's own data dir, where a gated-on recorder writes.
    fn data_dir(&self) -> PathBuf {
        self.0.join("data")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A clock a test can move, handed over as the seam's boxed closure.
fn clock(start: i64) -> (Arc<AtomicI64>, Now) {
    let dial = Arc::new(AtomicI64::new(start));
    let shared = Arc::clone(&dial);
    let now: Now = Box::new(move || shared.load(Ordering::SeqCst));
    (dial, now)
}

/// One flush, kept whole: the date it appended to and the lines it appended.
#[derive(Clone, Debug, PartialEq)]
struct Write {
    date: String,
    lines: Vec<String>,
}

/// A sink that keeps every append, shared with the test through an `Arc` so the recorder can own the
/// boxed handle.
#[derive(Clone, Default)]
struct RecordingSink {
    writes: Arc<Mutex<Vec<Write>>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self::default()
    }

    /// The handle `TelemetryOptions` carries; this clone keeps the same log.
    fn boxed(&self) -> Box<dyn TelemetrySink + Send> {
        Box::new(self.clone())
    }

    fn writes(&self) -> Vec<Write> {
        self.writes.lock().expect("the write log lock").clone()
    }
}

impl TelemetrySink for RecordingSink {
    fn write(&mut self, date: &str, lines: &[String]) -> io::Result<()> {
        self.writes.lock().expect("the write log lock").push(Write {
            date: date.to_owned(),
            lines: lines.to_vec(),
        });
        Ok(())
    }
}

/// A sink that is always out of space (errno 28): the one failure the recorder must survive.
#[derive(Clone, Default)]
struct FailingSink {
    attempts: Arc<AtomicU64>,
}

impl FailingSink {
    fn new() -> Self {
        Self::default()
    }

    fn boxed(&self) -> Box<dyn TelemetrySink + Send> {
        Box::new(self.clone())
    }

    fn attempts(&self) -> u64 {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl TelemetrySink for FailingSink {
    fn write(&mut self, _date: &str, _lines: &[String]) -> io::Result<()> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        Err(io::Error::from_raw_os_error(28))
    }
}

/// Options for one recorder over `dir`: `<dir>/telemetry` holds the daily files.
fn options(
    dir: &Path,
    sink: Option<Box<dyn TelemetrySink + Send>>,
    now: Option<Now>,
) -> TelemetryOptions {
    TelemetryOptions {
        dir: dir.join("telemetry"),
        repo_root: dir.join("repo"),
        data_dir: dir.join("data"),
        session_mode: SessionMode::Cli,
        server_version: SERVER_VERSION.to_owned(),
        graph_schema_version: 1,
        sink,
        now,
    }
}

/// The repository half of the gate: a tree, its own data dir and its own `telemetry` flag.
fn repo_input(dir: &Path, consent: bool, registered: bool, telemetry: bool) -> RepoTelemetryInput {
    RepoTelemetryInput {
        consent,
        repo: RepoConfig {
            repo_root: dir.join("repo"),
            data_dir: dir.join("data"),
            watch: true,
            watch_interval_ms: 2_000,
            telemetry,
            source: RepoConfigSource::Project,
        },
        registered,
        session_mode: SessionMode::Mcp,
        server_version: SERVER_VERSION.to_owned(),
        graph_schema_version: 1,
        now: None,
    }
}

/// A caller's context with every recorder-owned field deliberately wrong: the recorder must replace
/// the timestamp, both identifiers and the counter, and keep only the facts of its composition.
fn context() -> RecordContext {
    RecordContext {
        ts: "1999-01-01T00:00:00.000Z".to_owned(),
        install_id: "not-the-recorders-install-id".to_owned(),
        session_id: "not-the-recorders-session-id".to_owned(),
        seq: 999,
        server_version: SERVER_VERSION.to_owned(),
        os: os().to_owned(),
        arch: arch().to_owned(),
        graph_schema_version: 1,
    }
}

fn call_spec() -> CallSpec {
    let mut args = Map::new();
    args.insert("query".to_owned(), json!("handleRequest"));
    CallSpec {
        tool: "trace".to_owned(),
        args,
        supplied_names: vec!["query".to_owned()],
        envelope: None,
        status: RecordStatus::Ok,
        error_code: None,
        duration_ms: 7,
    }
}

fn index_run_spec() -> IndexRunSpec {
    IndexRunSpec {
        mode: IndexMode::Full,
        exit_code: 0,
        duration_ms: 1_500,
        was_forced: false,
        files_seen: 10,
        files_indexed: 10,
        files_skipped: 0,
        files_failed: 0,
        nodes_by_kind: Counts::from_iter([("function", 20)]),
        edges_by_kind_tier: Counts::from_iter([("calls/extracted", 30)]),
        unresolved_count: 0,
        languages: Counts::from_iter([("rust", 10)]),
        db_size_bytes: 4_096,
        files_changed_in_run: 10,
    }
}

fn record_calls(recorder: &mut Recorder, count: usize) {
    for _ in 0..count {
        recorder.record_call(&context(), &call_spec());
    }
}

/// The daily files a live recorder wrote under `dir/telemetry`, sorted by name.
fn jsonl_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir.join("telemetry")) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect();
    files.sort();
    files
}

fn read_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .expect("the daily file reads")
        .lines()
        .map(str::to_owned)
        .collect()
}

fn parsed(lines: &[String]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| serde_json::from_str(line).expect("every line parses"))
        .collect()
}

/// The `seq` of every line that carries one, in file order; the session line has none.
fn seqs(lines: &[String]) -> Vec<u64> {
    parsed(lines)
        .iter()
        .filter_map(|record| record.get("seq"))
        .map(|seq| seq.as_u64().expect("seq is a number"))
        .collect()
}

fn flush_age() -> i64 {
    i64::try_from(FLUSH_AT_MS).expect("the flush age fits an i64")
}

#[test]
fn thirty_one_buffered_records_write_nothing_and_the_thirty_second_writes_one_file() {
    // The constant is part of the contract, not an implementation detail: the count is what caps one
    // `O_APPEND` write.
    assert_eq!(FLUSH_AT_RECORDS, 32);

    let temp = TempDir::new("count");
    let (_dial, now) = clock(START_MS);
    let mut recorder = create(options(temp.path(), None, Some(now)));

    // The session record is buffered before any call, exactly as `createTelemetry` does, so the
    // count includes it: one session line and thirty calls are a 31-record buffer.
    record_calls(&mut recorder, FLUSH_AT_RECORDS - 2);
    assert!(
        jsonl_files(temp.path()).is_empty(),
        "31 buffered records must write no file"
    );

    // The 32nd buffered record is the 31st call, and it flushes the whole buffer once.
    recorder.record_call(&context(), &call_spec());
    let files = jsonl_files(temp.path());
    assert_eq!(files.len(), 1, "the 32nd record writes exactly one file");
    assert_eq!(
        files[0].file_stem().and_then(|stem| stem.to_str()),
        Some(START_DATE),
        "the file is named by the records' own day"
    );

    let lines = read_lines(&files[0]);
    assert_eq!(
        lines.len(),
        FLUSH_AT_RECORDS,
        "one line per buffered record"
    );
    let records = parsed(&lines);
    assert_eq!(
        records[0]["kind"], "session",
        "the session line comes first"
    );
    assert!(
        records[0].get("seq").is_none(),
        "the session record carries no seq"
    );
    assert_eq!(seqs(&lines), (1..=31u64).collect::<Vec<u64>>());
}

#[test]
fn an_age_of_five_seconds_flushes_on_the_next_record_and_takes_it_along() {
    let temp = TempDir::new("age");
    let (dial, now) = clock(START_MS);
    let sink = RecordingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    // One millisecond short of the trigger: the oldest record — the session line, written at
    // `START_MS` — is not old enough yet, and the reader's own record does not age it.
    dial.store(START_MS + flush_age() - 1, Ordering::SeqCst);
    recorder.record_call(&context(), &call_spec());
    assert!(sink.writes().is_empty(), "4999 ms is not an age");

    // At exactly `FLUSH_AT_MS` the next record flushes, and it is part of that flush.
    dial.store(START_MS + flush_age(), Ordering::SeqCst);
    recorder.record_call(&context(), &call_spec());
    let writes = sink.writes();
    assert_eq!(writes.len(), 1, "the age trigger flushes once");
    assert_eq!(
        writes[0].lines.len(),
        3,
        "the session line and both calls, in order"
    );
    assert_eq!(writes[0].date, START_DATE);
}

#[test]
fn seq_is_one_per_record_and_continues_after_a_flush() {
    let temp = TempDir::new("seq");
    let (_dial, now) = clock(START_MS);
    let sink = RecordingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    record_calls(&mut recorder, FLUSH_AT_RECORDS - 1);
    let writes = sink.writes();
    assert_eq!(writes.len(), 1, "the count trigger fired once");
    assert_eq!(writes[0].lines.len(), FLUSH_AT_RECORDS);
    // The caller's context said 999; the recorder's own counter is what the record carries.
    assert_eq!(seqs(&writes[0].lines), (1..=31u64).collect::<Vec<u64>>());

    record_calls(&mut recorder, 1);
    assert_eq!(
        sink.writes().len(),
        1,
        "a single record lengthens the buffer, it does not flush it"
    );

    recorder.flush();
    let writes = sink.writes();
    assert_eq!(writes.len(), 2);
    assert_eq!(seqs(&writes[1].lines), vec![32u64], "seq continues across");
}

#[test]
fn a_flush_appends_once_per_utc_date_the_records_fall_on() {
    let temp = TempDir::new("dates");
    // Late on `START_DATE`, still inside the five seconds the age trigger waits for.
    let (dial, now) = clock(NEXT_DAY_MS - 500);
    let sink = RecordingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    dial.store(NEXT_DAY_MS - 1, Ordering::SeqCst);
    recorder.record_call(&context(), &call_spec());
    dial.store(NEXT_DAY_MS + 1, Ordering::SeqCst);
    recorder.record_call(&context(), &call_spec());
    recorder.flush();

    let writes = sink.writes();
    assert_eq!(writes.len(), 2, "one append per date, and no third");
    assert_eq!(writes[0].date, START_DATE);
    assert_eq!(writes[1].date, NEXT_DATE);
    assert_eq!(
        writes[0].lines.len(),
        2,
        "the session line and the first call"
    );
    assert_eq!(writes[1].lines.len(), 1, "the call after midnight");
}

#[test]
fn creation_buffers_the_session_record_first_and_names_its_own_day() {
    let temp = TempDir::new("session");
    fs::create_dir_all(temp.path().join("repo").join(".git")).expect("create the git marker");
    let (_dial, now) = clock(START_MS);
    let sink = RecordingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    assert!(sink.writes().is_empty(), "creation itself flushes nothing");
    recorder.flush();
    let writes = sink.writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(
        writes[0].date, START_DATE,
        "the file is named by its own ts"
    );
    assert_eq!(writes[0].lines.len(), 1);

    let record: Value = serde_json::from_str(&writes[0].lines[0]).expect("the line parses");
    assert_eq!(record["kind"], "session");
    assert_eq!(record["ts"], "2026-11-18T10:00:00.000Z");
    assert_eq!(record["session_mode"], "cli");
    assert_eq!(record["server_version"], SERVER_VERSION);
    assert_eq!(record["os"], os());
    assert_eq!(record["arch"], arch());
    assert_eq!(record["is_first_run"], Value::Bool(true));
    assert_eq!(
        record["repo_size_bucket"], "none",
        "there is no graph.db yet"
    );
    assert_eq!(record["root_kind"], "git");
    assert_eq!(record["install_id"], recorder.install_id());
    assert_eq!(record["session_id"], recorder.session_id());
    assert!(record.get("seq").is_none());

    // The identity the record carries is the one the directory persists, under today's day.
    let id_file = fs::read_to_string(temp.path().join("telemetry").join("install_id.json"))
        .expect("the install id file is written");
    let identity: Value = serde_json::from_str(&id_file).expect("the install id file parses");
    assert_eq!(identity["install_id"], record["install_id"]);
    assert_eq!(identity["date"], START_DATE);
}

#[test]
fn an_index_run_is_buffered_and_stamped_like_a_call() {
    let temp = TempDir::new("index-run");
    let (_dial, now) = clock(START_MS);
    let sink = RecordingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    recorder.record_index_run(&context(), &index_run_spec());
    recorder.flush();

    let writes = sink.writes();
    assert_eq!(writes[0].lines.len(), 2, "the session line and the run");
    let record: Value = serde_json::from_str(&writes[0].lines[1]).expect("the line parses");
    assert_eq!(record["kind"], "index_run");
    assert_eq!(record["mode"], "full");
    assert_eq!(record["languages"], json!({"rust": 10}));
    // The recorder's clock and identifiers, not the caller's guesses.
    assert_eq!(record["ts"], "2026-11-18T10:00:00.000Z");
    assert_eq!(record["install_id"], recorder.install_id());
    assert_eq!(record["session_id"], recorder.session_id());
    // The run's record shape has no `seq` in either implementation, so the counter it consumed is
    // only visible on the next call.
    assert!(record.get("seq").is_none());

    recorder.record_call(&context(), &call_spec());
    recorder.flush();
    let writes = sink.writes();
    let line: Value = serde_json::from_str(&writes[1].lines[0]).expect("the line parses");
    assert_eq!(
        line["seq"], 2,
        "the run incremented seq even though it cannot carry it"
    );
}

#[test]
fn the_gate_needs_consent_registration_and_the_repos_own_flag() {
    // Open: all three agree, and the recorder writes under the repo's data dir.
    let open = TempDir::new("gate-open");
    let (_dial, now) = clock(START_MS);
    let mut input = repo_input(open.path(), true, true, true);
    input.now = Some(now);
    let mut recorder = telemetry_for_repo(input);
    assert_eq!(recorder.install_id().len(), 32, "an open gate mints an id");
    recorder.record_call(&context(), &call_spec());
    recorder.flush();
    let files = jsonl_files(&open.data_dir());
    assert_eq!(files.len(), 1, "a recording repository writes its own file");
    assert_eq!(
        files[0].file_stem().and_then(|stem| stem.to_str()),
        Some(START_DATE)
    );
    assert_eq!(read_lines(&files[0]).len(), 2);

    // Closed: any one of the three is enough to keep the tree out of the record, and the recorder a
    // closed gate hands out has no destination at all.
    for (label, consent, registered, flag) in [
        ("no consent", false, true, true),
        ("unregistered", true, false, true),
        ("the repo's flag off", true, true, false),
        ("all three off", false, false, false),
    ] {
        let temp = TempDir::new("gate-closed");
        let mut recorder = telemetry_for_repo(repo_input(temp.path(), consent, registered, flag));
        assert_eq!(recorder.install_id(), "", "{label} must hand out null()");
        assert_eq!(recorder.session_id(), "", "{label} must hand out null()");
        record_calls(&mut recorder, FLUSH_AT_RECORDS * 2);
        recorder.flush();
        recorder.close();
        assert!(
            !temp.data_dir().join("telemetry").exists(),
            "{label} must write nothing to the data dir"
        );
    }
}

#[test]
fn null_records_nothing_and_touches_no_file() {
    let temp = TempDir::new("null");
    let mut recorder = null();
    assert_eq!(recorder.install_id(), "");
    assert_eq!(recorder.session_id(), "");

    record_calls(&mut recorder, FLUSH_AT_RECORDS * 2);
    recorder.record_index_run(&context(), &index_run_spec());
    recorder.flush();
    recorder.close();

    assert!(
        fs::read_dir(temp.path())
            .expect("the temp dir lists")
            .next()
            .is_none(),
        "the null recorder wrote a file"
    );
}

#[test]
fn a_failing_sink_costs_records_never_an_answer() {
    let temp = TempDir::new("failing");
    let (_dial, now) = clock(START_MS);
    let sink = FailingSink::new();
    let mut recorder = create(options(temp.path(), Some(sink.boxed()), Some(now)));

    // The count trigger attempts one append, which fails: the records are lost, nothing panics.
    record_calls(&mut recorder, FLUSH_AT_RECORDS - 1);
    assert_eq!(sink.attempts(), 1);

    // Degraded from then on: further records are dropped in memory and never retried, so a full disk
    // does not become one log line per call.
    record_calls(&mut recorder, FLUSH_AT_RECORDS * 2);
    recorder.flush();
    recorder.close();
    assert_eq!(sink.attempts(), 1, "a degraded recorder stops writing");
    assert!(jsonl_files(temp.path()).is_empty());
}
