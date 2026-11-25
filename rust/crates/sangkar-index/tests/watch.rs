//! The watcher's contract is timing and process behaviour, so every test drives the real loop with
//! short intervals and a stubbed spawn: [`start_watch`] decides *when*, and a stub is the only way
//! to assert "exactly one spawn" without paying for an index run. No test may sleep for seconds, so
//! every wait is a bounded poll — [`until`] fails loudly instead of hanging — and the loop's own
//! windows are milliseconds.
//!
//! The fixtures are temp directories made by [`TempDir`], because `tempfile` is not a dependency of
//! this crate. Most are deliberately not git checkouts: that is the degraded mtime detector, which
//! is also the cheapest way to produce a change (write a file). One test builds a real checkout with
//! one `git init`, and skips itself with a printed note when git is not installed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sangkar_index::watch::{
    Clock, DEFAULT_DEBOUNCE_MS, DEFAULT_INTERVAL_MS, Detector, MAX_BACKOFF_MS, SpawnNotice,
    SpawnTarget, SpawnWorker, WatchChild, WatchHandle, WatchOptions, backoff_ms, start_watch,
};

/// A temp directory that removes itself, so a failing assertion leaves nothing behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-index-watch-{label}-{}-{nanos}-{}",
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

    /// The data dir the product would use: inside the tree, as `AppConfig` resolves it by default.
    fn data_dir(&self) -> PathBuf {
        self.0.join(".sangkar")
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A working tree with one parseable file, which is all the watcher needs to see.
const ORIGINAL: &str = "def run():\n    return 1\n";

fn fixture(label: &str) -> TempDir {
    let dir = TempDir::new(label);
    fs::write(dir.file("app.py"), ORIGINAL).expect("write the fixture source");
    dir
}

/// One spawn attempt as the stub recorded it.
#[derive(Clone, Debug)]
struct Recorded {
    target: SpawnTarget,
    /// `Date.now()` at the attempt, so a test can measure the debounce and the backoff.
    at: i64,
}

type Log = Arc<Mutex<Vec<Recorded>>>;

fn log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn calls(log: &Log) -> MutexGuard<'_, Vec<Recorded>> {
    log.lock().unwrap_or_else(PoisonError::into_inner)
}

fn count(log: &Log) -> usize {
    calls(log).len()
}

/// A child that reports `code` the first time it is asked: a run that already ended.
struct ExitedChild {
    code: i32,
}

impl WatchChild for ExitedChild {
    fn try_wait(&mut self) -> Option<i32> {
        Some(self.code)
    }

    fn kill(&mut self) {}
}

/// A child that never exits on its own, so the only way out is `stop()` killing it.
struct PendingChild {
    killed: Arc<AtomicBool>,
    code: i32,
}

impl WatchChild for PendingChild {
    fn try_wait(&mut self) -> Option<i32> {
        self.killed.load(Ordering::SeqCst).then_some(self.code)
    }

    fn kill(&mut self) {
        self.killed.store(true, Ordering::SeqCst);
    }
}

/// A spawn stub that reports `code_for(attempt)` and records every call, in order.
fn stub_spawn(code_for: impl Fn(usize) -> i32 + Send + Sync + 'static) -> (SpawnWorker, Log) {
    let log = log();
    let recorded = Arc::clone(&log);
    let spawn: SpawnWorker = Arc::new(move |target: &SpawnTarget| {
        let at = now_ms();
        let attempt = {
            let mut entries = recorded.lock().unwrap_or_else(PoisonError::into_inner);
            entries.push(Recorded {
                target: target.clone(),
                at,
            });
            entries.len()
        };
        Ok(Box::new(ExitedChild {
            code: code_for(attempt),
        }) as Box<dyn WatchChild>)
    });
    (spawn, log)
}

/// Starts a watcher on `dir` with millisecond windows and drops it at the end of the test.
fn watch(dir: &TempDir, spawn: SpawnWorker, interval_ms: u64, debounce_ms: u64) -> WatchHandle {
    start_watch(&WatchOptions {
        repo_root: dir.path().to_path_buf(),
        data_dir: dir.data_dir(),
        interval_ms: Some(interval_ms),
        debounce_ms: Some(debounce_ms),
        spawn: Some(spawn),
        ..WatchOptions::default()
    })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Polls `condition` every couple of milliseconds, and fails the test rather than hanging.
fn until(condition: impl Fn() -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "timed out after 5 s waiting for {label}"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

fn pause(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

/// Waits until the loop has taken its first observation, which is the baseline.
fn baseline_captured(handle: &WatchHandle) {
    until(|| handle.stats().polls >= 1, "the first poll (baseline)");
    assert_eq!(handle.stats().spawns, 0, "the baseline spawns nothing");
}

/// `git` as a program the test may run: found on `PATH` without executing anything outside a temp
/// directory, which is why the check is a file lookup rather than `git --version`.
fn git_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| fs::metadata(dir.join("git")).is_ok_and(|metadata| metadata.is_file()))
}

/// One `git` call inside the fixture. A refusal fails the test instead of being walked past.
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("--no-pager")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .expect("run git in the fixture");
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A commit with an identity the developer's config cannot change, so no fixture depends on it.
fn commit(root: &Path, message: &str) {
    git(
        root,
        &[
            "-c",
            "user.name=watch fixture",
            "-c",
            "user.email=watch@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// A clock the test owns, so a spawn's reported time is asserted instead of measured.
struct FixedClock(i64);

impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// `TECH-SPEC.md` §8's defaults: a 2 s poll and a 500 ms debounce.
#[test]
fn the_defaults_are_the_documented_poll_and_debounce() {
    assert_eq!(DEFAULT_INTERVAL_MS, 2_000);
    assert_eq!(DEFAULT_DEBOUNCE_MS, 500);
    assert_eq!(MAX_BACKOFF_MS, 300_000);
}

/// The retry delay after 0..6 failures, and the two ends of the cap. The Done-when asks for the two
/// values printed here: `backoff_ms(2000, 6) == 128000` and `backoff_ms(2000, 20) == 300000`.
#[test]
fn backoff_doubles_from_the_interval_to_the_cap() {
    let table: [(u32, u64); 7] = [
        (0, 0),
        (1, 4_000),
        (2, 8_000),
        (3, 16_000),
        (4, 32_000),
        (5, 64_000),
        (6, 128_000),
    ];
    for (failures, expected) in table {
        let actual = backoff_ms(2_000, failures);
        println!("backoff_ms(2000, {failures}) = {actual}");
        assert_eq!(actual, expected, "backoff after {failures} failures");
    }
    let capped = backoff_ms(2_000, 20);
    println!("backoff_ms(2000, 20) = {capped}");
    assert_eq!(capped, 300_000);
    // An interval already at the cap cannot be pushed past it, and a huge streak stays finite.
    assert_eq!(backoff_ms(MAX_BACKOFF_MS, 1), MAX_BACKOFF_MS);
    assert_eq!(backoff_ms(1, u32::MAX), MAX_BACKOFF_MS);
}

/// The watcher reports *changes*: the first poll records the baseline and spawns nothing, so a graph
/// that is stale at startup is the caller's run to make rather than a spawn storm here.
#[test]
fn the_first_poll_only_records_the_baseline() {
    let dir = fixture("baseline");
    let (spawn, log) = stub_spawn(|_| 0);
    let handle = watch(&dir, spawn, 20, 20);

    baseline_captured(&handle);
    pause(120);
    assert_eq!(count(&log), 0, "nothing changed, so nothing was spawned");
    assert!(handle.stats().polls > 1, "the loop kept polling");
    assert_eq!(handle.stats().detector, Some(Detector::Mtime));
}

/// One write spawns exactly one run, and only after the quiet window: the debounce, not the poll
/// interval, is what the spawn waits for.
#[test]
fn one_file_change_spawns_the_worker_exactly_once() {
    let dir = fixture("one-change");
    let (spawn, log) = stub_spawn(|_| 0);
    let handle = watch(&dir, spawn, 20, 60);

    baseline_captured(&handle);
    let changed_at = now_ms();
    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("change the fixture source");

    until(|| count(&log) == 1, "the spawn caused by the change");
    let first = calls(&log)[0].clone();
    let waited = first.at - changed_at;
    assert!(
        waited >= 60,
        "the quiet window elapsed before the spawn: {waited} ms"
    );
    assert_eq!(
        first.target,
        SpawnTarget {
            repo_root: dir.path().to_path_buf(),
            data_dir: dir.data_dir(),
        }
    );
    // The change was read through mtimes, because a plain temp directory is not a checkout.
    assert_eq!(handle.stats().detector, Some(Detector::Mtime));

    // The successful run adopts the observed state as the baseline, so nothing repeats.
    pause(140);
    assert_eq!(count(&log), 1);
    assert_eq!(handle.stats().last_exit_code, Some(0));
    assert_eq!(handle.stats().failures, 0);
}

/// A save often writes the file more than once. The re-check after the quiet window sees the second
/// write, so one spawn covers both — the Done-when's burst assertion.
#[test]
fn a_burst_of_writes_inside_the_debounce_window_spawns_once() {
    let dir = fixture("burst");
    let (spawn, log) = stub_spawn(|_| 0);
    let handle = watch(&dir, spawn, 20, 120);

    baseline_captured(&handle);
    let polls_before = handle.stats().polls;

    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("first write");
    // The change is detected on the next poll, which puts the loop inside the quiet window.
    until(
        || handle.stats().polls > polls_before,
        "detection of the first write",
    );
    pause(5);
    fs::write(dir.file("app.py"), "def run():\n    return 3\n").expect("second write");

    until(|| count(&log) >= 1, "the single spawn");
    pause(200);
    let spawns = count(&log);
    println!("a two-write burst inside the debounce window spawned {spawns} worker(s)");
    assert_eq!(spawns, 1);
}

/// A worker that exits 1 is a failure: the streak grows, the delay doubles, and the change is
/// retried because the baseline was not advanced.
#[test]
fn a_worker_exit_of_one_raises_the_backoff() {
    let dir = fixture("failing");
    let (spawn, log) = stub_spawn(|_| 1);
    let handle = watch(&dir, spawn, 20, 10);

    baseline_captured(&handle);
    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("change the fixture source");

    until(|| handle.stats().failures == 2, "two failed runs");
    assert_eq!(handle.stats().backoff_ms, 80, "backoff_ms(20, 2)");
    let attempts = calls(&log).clone();
    assert!(attempts.len() >= 2, "expected two attempts");
    let gap = attempts[1].at - attempts[0].at;
    assert!(
        gap >= 30,
        "the second attempt waited for the doubled delay: {gap} ms"
    );
    until(|| count(&log) >= 3, "a third attempt after the backoff");
}

/// 75 is `EX_TEMPFAIL`, "another indexer holds the lock": normal, logged, and not a failure, so a
/// contended repo does not slow its own polling down.
#[test]
fn a_worker_exit_of_seventy_five_is_not_a_failure() {
    let dir = fixture("locked");
    let (spawn, log) = stub_spawn(|_| 75);
    let handle = watch(&dir, spawn, 20, 10);

    baseline_captured(&handle);
    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("change the fixture source");

    until(
        || count(&log) >= 2,
        "a second attempt after the locked exit",
    );
    let stats = handle.stats();
    assert_eq!(stats.failures, 0);
    assert_eq!(stats.backoff_ms, 0);
    assert_eq!(stats.last_exit_code, Some(75));
}

/// A directory that is not a checkout is watched through mtimes, and a write there is still a
/// change: the fallback is slower to describe, not blind.
#[test]
fn the_mtime_detector_answers_outside_a_git_checkout() {
    let dir = fixture("no-git");
    assert!(
        !dir.file(".git").exists(),
        "the fixture must not be a git checkout"
    );
    let (spawn, log) = stub_spawn(|_| 0);
    let handle = watch(&dir, spawn, 20, 10);

    baseline_captured(&handle);
    assert_eq!(handle.stats().detector, Some(Detector::Mtime));
    fs::write(dir.file("app.py"), "def run():\n    return 7\n").expect("change the fixture source");
    until(|| count(&log) == 1, "the spawn caused by the write");
    assert_eq!(handle.stats().detector, Some(Detector::Mtime));
}

/// `stop()` ends a running child and joins the thread, so no worker outlives the watcher and no
/// zombie is left behind. A killed child is not a failure: the repository said nothing about itself.
#[test]
fn stop_kills_a_running_child_and_joins_the_loop() {
    let dir = fixture("stop");
    let killed = Arc::new(AtomicBool::new(false));
    let log = log();
    let recorded = Arc::clone(&log);
    let flag = Arc::clone(&killed);
    let spawn: SpawnWorker = Arc::new(move |target: &SpawnTarget| {
        calls(&recorded).push(Recorded {
            target: target.clone(),
            at: now_ms(),
        });
        Ok(Box::new(PendingChild {
            killed: Arc::clone(&flag),
            code: 143,
        }) as Box<dyn WatchChild>)
    });
    let handle = watch(&dir, spawn, 20, 10);

    baseline_captured(&handle);
    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("change the fixture source");
    until(|| count(&log) == 1, "the spawn whose child never exits");

    let stopping = Instant::now();
    handle.stop();
    let elapsed = stopping.elapsed();
    assert!(
        elapsed < Duration::from_secs(2),
        "stop() joined in {elapsed:?}"
    );
    assert!(killed.load(Ordering::SeqCst), "the child was killed");
    // The exit code of the child we killed was read, so the slot was drained.
    assert_eq!(handle.stats().last_exit_code, Some(143));
    assert_eq!(handle.stats().failures, 0);

    let spawns_after_stop = handle.stats().spawns;
    pause(120);
    assert_eq!(handle.stats().spawns, spawns_after_stop);
    // `stop()` is idempotent, so a second signal handler cannot double-kill.
    handle.stop();
}

/// The caller's clock is the one a spawn reports, so a caller can log an age without reading a
/// second clock — the seam these tests would otherwise have to measure.
#[test]
fn the_injected_clock_is_what_a_spawn_reports() {
    let dir = fixture("clock");
    let (spawn, log) = stub_spawn(|_| 0);
    let notices: Arc<Mutex<Vec<SpawnNotice>>> = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&notices);
    let handle = start_watch(&WatchOptions {
        repo_root: dir.path().to_path_buf(),
        data_dir: dir.data_dir(),
        interval_ms: Some(20),
        debounce_ms: Some(10),
        spawn: Some(spawn),
        on_spawn: Some(Arc::new(move |notice: SpawnNotice| {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(notice);
        })),
        clock: Some(Arc::new(FixedClock(42_000))),
    });

    baseline_captured(&handle);
    fs::write(dir.file("app.py"), "def run():\n    return 2\n").expect("change the fixture source");
    until(|| count(&log) == 1, "the spawn caused by the change");

    assert_eq!(handle.stats().last_spawn_at, Some(42_000));
    let recorded = notices
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].started_at, 42_000);
    assert_eq!(recorded[0].failures, 0);
}

/// A checkout is watched through HEAD and porcelain: a `touch` of a clean file is not a change, a
/// real edit is, and a commit moves the signature without touching a working-tree byte.
#[test]
fn a_git_checkout_is_watched_through_head_and_porcelain() {
    if !git_available() {
        eprintln!("skipping the git detector test: `git` is not on PATH");
        return;
    }
    let dir = fixture("git");
    git(dir.path(), &["init", "-q"]);
    git(dir.path(), &["add", "-A"]);
    commit(dir.path(), "fixture");

    let (spawn, log) = stub_spawn(|_| 0);
    let handle = watch(&dir, spawn, 20, 20);

    baseline_captured(&handle);
    assert_eq!(handle.stats().detector, Some(Detector::Git));

    // Same bytes, new mtime: porcelain stays clean, so the graph already covers this state.
    fs::write(dir.file("app.py"), ORIGINAL).expect("touch the fixture source");
    until(|| handle.stats().polls >= 3, "three polls after the touch");
    assert_eq!(count(&log), 0, "a touch of a clean file is not a change");

    // A real edit does change the status, and is spawned once.
    fs::write(dir.file("app.py"), "def run():\n    return 42\n").expect("edit the fixture source");
    until(|| count(&log) == 1, "the spawn caused by the dirty status");
    pause(150);
    assert_eq!(count(&log), 1);

    // Committing moves HEAD, which changes the signature without touching a working-tree byte.
    git(dir.path(), &["add", "-A"]);
    commit(dir.path(), "change");
    until(|| count(&log) == 2, "the spawn caused by the new HEAD");
    pause(150);
    assert_eq!(count(&log), 2);
}
