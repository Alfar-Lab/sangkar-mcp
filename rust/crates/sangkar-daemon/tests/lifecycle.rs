//! The daemon's lifecycle against real processes and real files: a detached child takes the pid
//! lock, a `status` in this process reads it, a `SIGTERM` releases it, and a second `start` refuses
//! with the holder's pid.
//!
//! Ported from `tests/daemon-lifecycle.test.ts`, with one structural difference: the TypeScript
//! drove `runDaemon` in-process through an `AbortSignal`, while the Rust contract asks for a child
//! process, so most of these cases spawn one. The child is *this test binary* re-executed with
//! `--exact child_runs_the_daemon_until_a_signal`, reached through a one-line `sh` wrapper that puts
//! the temp home in the environment — `start` inherits its environment on purpose, and a flag the
//! daemon does not read would be a second way to spell a home.
//!
//! Safety: every signal sent here goes to a process this test spawned — the pid `start` returned, or
//! the pid of a child this test created and waited on. The one lock written by hand names a pid that
//! is already reaped, so the stale case is exercised without ever signalling a stranger.
//!
//! Every home is a unique directory under `std::env::temp_dir()` removed on drop, so a panicking
//! assertion still takes its pid file with it, and a `DaemonGuard` stops a daemon an assertion left
//! running before the test ends.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use sangkar_core::errors::AppError;
use sangkar_core::pidlock::{acquire_pid_lock, read_pid_lock, release_pid_lock};
use sangkar_core::registry::{REGISTRY_VERSION, Registry, write_registry};
use sangkar_daemon::lifecycle::{DaemonOptions, run_daemon, start, status, stop};
use sangkar_daemon::types::{DaemonStatus, RepoRecord, RepoRuntime, ReportAddress};
use tokio::sync::Notify;

/// The marker the wrapper puts in the child's environment; absent means "not the child".
const CHILD_HOME_ENV: &str = "SANGKAR_DAEMON_TEST_HOME";

/// The one test the child runs: `--exact` makes libtest skip the rest of this file.
const CHILD_TEST: &str = "child_runs_the_daemon_until_a_signal";

/// How long a case waits for something a child does: spawn, tick, shutdown. Far above the observed
/// cost (a few milliseconds) and far below the daemon's own five-second waits, so a real hang fails
/// the test instead of timing out the suite.
const PATIENCE: Duration = Duration::from_secs(10);

// -------------------------------------------------------------------------------------------
// The child: the daemon `start` spawns, which is this binary with one test selected.
// -------------------------------------------------------------------------------------------
/// Runs the real loop until a signal. In the parent run the marker is absent and this returns, which
/// is why it is a test at all: it is the entry point the detached start needs, and no binary target
/// may be added to this crate (`autobins = false`).
#[test]
fn child_runs_the_daemon_until_a_signal() {
    let Some(home) = std::env::var_os(CHILD_HOME_ENV).map(PathBuf::from) else {
        return;
    };
    let ticks = home.join("test-ticks");
    let options = DaemonOptions {
        interval_ms: Some(20),
        tick: Some(Box::new(move || {
            // One line per pass, so the parent can prove the loop ran without polling a clock: a file
            // a test writes is evidence, a sleep is a guess.
            if let Ok(mut file) = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&ticks)
            {
                let _ = writeln!(file, "tick");
            }
            Ok(Vec::new())
        })),
        ..DaemonOptions::new(&home)
    };
    // The loop's own answer is the process's: `0` on a signal, `1` when it could not serve.
    std::process::exit(run_daemon(&options));
}

// -------------------------------------------------------------------------------------------
// Cases.
// -------------------------------------------------------------------------------------------
#[test]
fn status_of_an_untouched_home_reports_no_daemon() {
    let home = TempHome::new("untouched");

    let answer = status(home.path());

    assert_eq!(answer.pid, None);
    assert_eq!(answer.started_at, None);
    assert_eq!(answer.uptime_ms, None);
    assert_eq!(answer.report, None);
    assert!(answer.repositories.is_empty());
    assert!(
        !home.join("daemon.pid").exists(),
        "a status read creates nothing"
    );
}

#[test]
fn status_reports_one_row_per_registered_repository_with_its_own_config() {
    let home = TempHome::new("rows");
    let paused = temp_repo(
        home.path(),
        "repo-paused",
        Some(r#"{"watch":false,"watch_interval":5000,"telemetry":false}"#),
    );
    let defaults = temp_repo(home.path(), "repo-defaults", None);
    write_registry_file(home.path(), &[paused.clone(), defaults.clone()]);

    let answer = status(home.path());

    assert_eq!(
        answer.repositories,
        vec![
            RepoRuntime {
                root: paused,
                watch: false,
                last_run: None,
                last_exit: None,
                generation: None,
            },
            RepoRuntime {
                root: defaults,
                watch: true,
                last_run: None,
                last_exit: None,
                generation: None,
            },
        ],
        "the repo's own file decides `watch`, the machine file answers the rest"
    );
}

#[test]
fn start_binds_the_lock_status_reports_it_and_stop_leaves_no_pid_file() {
    let home = TempHome::new("start-stop");
    let options = lifecycle_options(home.path());

    let started = start(&options);
    assert_eq!(started.code, 0, "start refused: {:?}", started.reason);
    let pid = started.pid.expect("the child wrote its pid");
    let guard = DaemonGuard::new(home.path());

    assert!(
        wait_until(|| ticks(home.path()) >= 2, PATIENCE),
        "the child never reached a second pass"
    );
    let running = status(home.path());
    assert_eq!(running.pid, Some(pid));
    assert!(
        running
            .started_at
            .as_deref()
            .is_some_and(|started_at| started_at.ends_with('Z')),
        "startedAt is the pid lock's ISO-8601 value: {:?}",
        running.started_at
    );
    assert!(
        running.uptime_ms.is_some_and(|uptime| uptime < 60_000),
        "uptime is the startedAt against now: {:?}",
        running.uptime_ms
    );

    let stopped = stop(home.path());
    assert_eq!(stopped.code, 0, "stop reported {stopped:?}");
    assert_eq!(stopped.pid, Some(pid));
    drop(guard);
    assert!(
        !home.join("daemon.pid").exists(),
        "a clean stop removes the pid file"
    );

    let after = status(home.path());
    assert_eq!(after.pid, None);
    assert_eq!(after.started_at, None);
    assert_eq!(after.uptime_ms, None);
    assert_eq!(after.report, None);
}

#[test]
fn a_second_start_refuses_and_names_the_holder_pid() {
    let home = TempHome::new("refused");
    let options = lifecycle_options(home.path());
    let started = start(&options);
    assert_eq!(started.code, 0, "start refused: {:?}", started.reason);
    let pid = started.pid.expect("the child wrote its pid");
    let _guard = DaemonGuard::new(home.path());

    let second = start(&options);

    assert_eq!(second.code, 1, "a live daemon is not displaced");
    assert_eq!(second.pid, None, "the refusal starts nothing");
    let reason = second.reason.expect("a refusal names why");
    assert!(
        reason.contains(&format!("pid {pid}")),
        "the refusal names the holder: {reason}"
    );
    assert_eq!(
        status(home.path()).pid,
        Some(pid),
        "the first daemon still holds the lock"
    );
    assert_eq!(stop(home.path()).code, 0);
}

#[test]
fn stop_of_a_home_with_no_daemon_reports_not_running() {
    let home = TempHome::new("nothing-to-stop");

    assert_eq!(
        stop(home.path()),
        sangkar_daemon::lifecycle::StopOutcome { code: 1, pid: None }
    );
}

#[test]
fn a_lock_naming_a_dead_pid_is_reported_stale_and_reclaimed_by_a_start() {
    let home = TempHome::new("stale");
    let dead = dead_pid();
    fs::write(
        home.join("daemon.pid"),
        format!(r#"{{"pid":{dead},"startedAt":"2026-01-01T00:00:00.000Z"}}"#),
    )
    .expect("a stale pid file");

    assert_eq!(
        status(home.path()).pid,
        None,
        "a dead holder is not a daemon"
    );

    let stopped = stop(home.path());
    assert_eq!(stopped.code, 1, "there was nothing to stop");
    assert_eq!(
        stopped.pid,
        Some(dead),
        "the stale pid is reported rather than hidden"
    );

    let options = lifecycle_options(home.path());
    let started = start(&options);
    assert_eq!(
        started.code, 0,
        "a dead pid is reclaimed, not refused: {:?}",
        started.reason
    );
    assert!(
        started.pid.is_some(),
        "the reclaimed start reports the pid it wrote"
    );
    let _guard = DaemonGuard::new(home.path());
    assert_eq!(stop(home.path()).code, 0);
}

#[test]
fn a_signal_shutdown_releases_the_lock_and_logs_it() {
    let home = TempHome::new("signal");
    let options = lifecycle_options(home.path());
    let started = start(&options);
    assert_eq!(started.code, 0, "start refused: {:?}", started.reason);
    let pid = started.pid.expect("the child wrote its pid");
    let _guard = DaemonGuard::new(home.path());
    assert!(
        wait_until(|| ticks(home.path()) >= 2, PATIENCE),
        "the child never reached a second pass"
    );

    let signalled = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(signalled.success(), "kill -TERM reached pid {pid}");

    assert!(
        wait_until(|| !home.join("daemon.pid").exists(), PATIENCE),
        "the signalled daemon left its pid file behind"
    );
    // The loop logs the signal before it releases the lock, so the file being gone proves the line is
    // on disk: the daemon's stdout *and* stderr are `daemon.log` by construction.
    let log = fs::read_to_string(home.join("daemon.log")).expect("the daemon's own log");
    assert!(
        log.contains("daemon.signal"),
        "the signal reached the loop: {log}"
    );
    assert!(log.contains("SIGTERM"), "the line names the signal: {log}");
    assert_eq!(status(home.path()).pid, None);
}

#[test]
fn stop_ends_a_daemon_this_test_spawned_directly_and_it_exits_zero() {
    let home = TempHome::new("direct");
    let mut command = child_command(home.path());
    let program = command.remove(0);
    let mut child = Command::new(program)
        .args(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the daemon itself");
    let pid = child.id();

    assert!(
        // The file appears before its body is written, so the wait is for the *record*, not the
        // inode: a status that caught the empty window would report no daemon.
        wait_until(
            || read_pid_lock(&home.join("daemon.pid"))
                .is_ok_and(|state| state.held && state.pid == Some(pid)),
            PATIENCE
        ),
        "the daemon never bound the lock"
    );
    assert_eq!(status(home.path()).pid, Some(pid));

    let stopped = stop(home.path());
    assert_eq!(stopped.code, 0, "stop reported {stopped:?}");
    assert_eq!(stopped.pid, Some(pid));
    let exited = child.wait().expect("wait for the daemon");
    assert_eq!(
        exited.code(),
        Some(0),
        "a signal is a clean exit, not a kill"
    );
    assert!(!home.join("daemon.pid").exists());
}

#[test]
fn the_loop_heartbeats_its_lock() {
    let home = TempHome::new("heartbeat");
    let pid_path = home.join("daemon.pid");
    let options = lifecycle_options(home.path());
    let started = start(&options);
    assert_eq!(started.code, 0, "start refused: {:?}", started.reason);
    let _guard = DaemonGuard::new(home.path());
    assert!(
        wait_until(|| ticks(home.path()) >= 2, PATIENCE),
        "the child never reached a second pass"
    );

    // Age the mtime the way a machine that slept for two hours would. The staleness rule is
    // `mtime > PID_STALE_AFTER_MS` (60s), so a loop that never heartbeats leaves a lock that reads as
    // abandoned; a renewed mtime is the only evidence the heartbeat ran.
    let aged = Command::new("touch")
        .arg("-d")
        .arg("2 hours ago")
        .arg(&pid_path)
        .status()
        .expect("run touch");
    assert!(aged.success(), "touch aged the pid file");

    assert!(
        wait_until(
            || file_age(&pid_path).is_some_and(|age| age < Duration::from_secs(30)),
            PATIENCE
        ),
        "the loop renewed the mtime: {:?}",
        file_age(&pid_path)
    );
    assert!(status(home.path()).pid.is_some());
    assert_eq!(stop(home.path()).code, 0);
}

#[test]
fn a_failing_first_tick_leaves_no_pid_file_and_answers_one() {
    let home = TempHome::new("first-tick");
    let options = DaemonOptions {
        tick: Some(Box::new(|| Err(AppError::internal("boom")))),
        ..DaemonOptions::new(home.path())
    };

    assert_eq!(run_daemon(&options), 1);

    assert!(
        !home.join("daemon.pid").exists(),
        "a daemon that cannot serve leaves no lock behind"
    );
}

#[test]
fn run_daemon_refuses_a_lock_someone_else_holds_and_leaves_it_alone() {
    let home = TempHome::new("held-by-test");
    let pid_path = home.join("daemon.pid");
    let held = acquire_pid_lock(&pid_path).expect("the test takes the lock first");
    let ticks = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ticks);
    let options = DaemonOptions {
        tick: Some(Box::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        })),
        ..DaemonOptions::new(home.path())
    };

    assert_eq!(run_daemon(&options), 1);

    assert_eq!(
        ticks.load(Ordering::Relaxed),
        0,
        "a refused daemon never reaches its first pass"
    );
    assert_eq!(
        status(home.path()).pid,
        Some(std::process::id()),
        "the holder's own file is untouched by the refusal"
    );
    release_pid_lock(&held).expect("release the test's own lock");
    assert!(!pid_path.exists());
}

#[test]
fn the_loop_records_what_status_reports_and_stops_advertising_its_report() {
    let home = TempHome::new("record");
    let repo = temp_repo(home.path(), "repo", None);
    write_registry_file(home.path(), std::slice::from_ref(&repo));
    let address = ReportAddress {
        url: "http://127.0.0.1:7317".to_owned(),
        host: "127.0.0.1".to_owned(),
        port: 7317,
    };
    let stop_seam = Arc::new(Notify::new());
    let observed: Arc<Mutex<Vec<DaemonStatus>>> = Arc::new(Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let (home_for_tick, repo_for_tick) = (home.path().to_path_buf(), repo.clone());
    let (stop_for_tick, observed_for_tick, calls_for_tick) = (
        Arc::clone(&stop_seam),
        Arc::clone(&observed),
        Arc::clone(&calls),
    );
    let options = DaemonOptions {
        interval_ms: Some(20),
        report: Some(address.clone()),
        stop: Some(Arc::clone(&stop_seam)),
        tick: Some(Box::new(move || {
            // What a status *beside* the daemon sees, captured from inside the loop so it is not a
            // race with the process exiting.
            observed_for_tick
                .lock()
                .expect("the observation list")
                .push(status(&home_for_tick));
            // Two passes: the first records rows, the second proves `status` read them back, and its
            // notification is what ends the loop without a signal.
            if calls_for_tick.fetch_add(1, Ordering::Relaxed) + 1 >= 2 {
                stop_for_tick.notify_one();
            }
            Ok(vec![RepoRecord {
                root: repo_for_tick.clone(),
                last_run: Some("2026-01-01T00:00:00.000Z".to_owned()),
                last_exit: Some(0),
                generation: Some("g-1".to_owned()),
            }])
        })),
        ..DaemonOptions::new(home.path())
    };

    assert_eq!(run_daemon(&options), 0, "the seam stops the loop cleanly");

    let seen = observed.lock().expect("the observation list").clone();
    assert_eq!(seen.len(), 2, "one observation per pass");
    assert_eq!(
        seen[1].pid,
        Some(std::process::id()),
        "the loop's own pid is the recorded holder"
    );
    assert_eq!(
        seen[1].report,
        Some(address),
        "the bound address is recorded"
    );
    assert_eq!(seen[1].repositories.len(), 1);
    assert_eq!(
        seen[1].repositories[0].last_exit,
        Some(0),
        "the second pass reads the first pass's record"
    );
    assert_eq!(seen[1].repositories[0].generation.as_deref(), Some("g-1"));

    let after = status(home.path());
    assert_eq!(after.pid, None, "the seam is a clean stop");
    assert_eq!(after.report, None, "a stopped daemon advertises no address");
    assert_eq!(
        after.repositories[0].last_run.as_deref(),
        Some("2026-01-01T00:00:00.000Z"),
        "the record survives the process"
    );
    assert!(!home.join("daemon.pid").exists());
}

// -------------------------------------------------------------------------------------------
// The scaffolding: a temp home, a repo, a registry, the child command and a cleanup guard.
// -------------------------------------------------------------------------------------------
/// A `mkdtemp`-style home removed on drop. The name carries the pid and a counter, so two cases — or
/// two runs of this binary — never share a lock.
struct TempHome(PathBuf);

impl TempHome {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sangkar-daemon-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("a temp home");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Stops a daemon an assertion left running, using only the pid file in a home this test made. The
/// guard never signals anything it did not create: the home is the test's, so the daemon is too.
struct DaemonGuard<'a>(&'a Path);

impl<'a> DaemonGuard<'a> {
    fn new(home: &'a Path) -> Self {
        Self(home)
    }
}

impl Drop for DaemonGuard<'_> {
    fn drop(&mut self) {
        let _ = stop(self.0);
    }
}

/// A repo directory with an optional on-disk config, returned in the canonical spelling the registry
/// stores: a symlinked temp directory must compare equal to what `status` reports.
fn temp_repo(home: &Path, name: &str, config: Option<&str>) -> String {
    let root = home.join(name);
    fs::create_dir_all(&root).expect("a repo dir");
    if let Some(config) = config {
        let data_dir = root.join(".sangkar");
        fs::create_dir_all(&data_dir).expect("a data dir");
        fs::write(data_dir.join("config.json"), config).expect("a repo config");
    }
    fs::canonicalize(&root)
        .unwrap_or(root)
        .display()
        .to_string()
}

/// The registry through `sangkar_core`'s writer, so the test cannot drift from the file `status` reads.
fn write_registry_file(home: &Path, roots: &[String]) {
    let registry = Registry {
        version: REGISTRY_VERSION,
        repos: roots.iter().map(PathBuf::from).collect(),
    };
    write_registry(&home.join("sangkar.json"), &registry).expect("a registry");
}

/// The command line `start` spawns: this binary, one test selected, with the temp home handed over by
/// a one-line shell wrapper. `exec` keeps the pid the lock records equal to the daemon's own.
fn child_command(home: &Path) -> Vec<OsString> {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let script =
        format!("SANGKAR_DAEMON_TEST_HOME=\"$1\" exec \"$2\" --exact {CHILD_TEST} --nocapture");
    vec![
        OsString::from("/bin/sh"),
        OsString::from("-c"),
        OsString::from(script),
        OsString::from("sangkar-daemon-test"),
        home.as_os_str().to_owned(),
        exe.into_os_string(),
    ]
}

/// The options a lifecycle case needs: a fast tick and the command that runs the child.
fn lifecycle_options(home: &Path) -> DaemonOptions {
    DaemonOptions {
        interval_ms: Some(20),
        command: child_command(home),
        ..DaemonOptions::new(home)
    }
}

/// The passes the child has recorded so far.
fn ticks(home: &Path) -> usize {
    fs::read_to_string(home.join("test-ticks")).map_or(0, |raw| raw.lines().count())
}

fn file_age(path: &Path) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// A pid that belonged to a process this test spawned and waited on, so it is reaped and free.
fn dead_pid() -> u32 {
    let mut child = Command::new("true")
        .spawn()
        .expect("spawn a short-lived process");
    let pid = child.id();
    child.wait().expect("reap the short-lived process");
    pid
}

/// Polls `probe` until it answers `true` or the deadline passes; the answer is the return value.
fn wait_until(probe: impl Fn() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
