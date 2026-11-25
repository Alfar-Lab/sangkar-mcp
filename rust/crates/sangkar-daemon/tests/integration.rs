//! The daemon as an operator runs it: the foreground composition over a temp home with registered
//! repositories, a fake worker script in place of the real indexer, and the four CLI answers.
//!
//! Ported from `tests/daemon-integration.test.ts`, `tests/daemon-main.test.ts` and
//! `tests/cli-daemon.test.ts`, restricted to what this crate owns: `sangkar-index`'s watch loop drives
//! the fake worker command, the reconciler follows `<home>/sangkar.json`, and `status --json` prints
//! what the loop recorded. The report server is a seam (`TASK-054` owns the listener), so the
//! composition case passes a stub and asserts the order instead of a socket.
//!
//! Safety: no daemon outlives a case. The loop runs on a thread this test started and is stopped
//! through its `Notify` seam — or by its guard's `Drop` when an assertion fails — every watcher joins
//! its polling thread before [`run_daemon_foreground`] returns, and every worker child is the script
//! below, which exits at once. Each home is a unique directory under the temp dir, removed on drop.
//! No pid is ever signalled: the only lock a case writes by hand is this process's own.

use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use sangkar_core::errors::AppError;
use sangkar_core::pidlock::{acquire_pid_lock, read_pid_lock, release_pid_lock};
use sangkar_core::registry::{REGISTRY_VERSION, Registry, write_registry};
use sangkar_daemon::cli::{DaemonInvocation, TelemetryReport, run_daemon_cli};
use sangkar_daemon::main::{BindAddress, ForegroundOptions, ReportServer, run_daemon_foreground};
use sangkar_daemon::reconcile::ReconcileReport;
use sangkar_daemon::types::ReportAddress;
use serde_json::Value;
use tokio::sync::Notify;

/// A repository file that asks for the shortest watch interval the contract allows, plus every key so
/// a read never warns about a setting the fixture left out.
const FAST_CONFIG: &str = r#"{"watch":true,"watch_interval":1000,"telemetry":true}"#;

/// A repository an operator paused: it must never spawn, and its own settings must reach `status`.
const PAUSED_CONFIG: &str = r#"{"watch":false,"watch_interval":5000,"telemetry":false}"#;

/// How long a case waits for something a watcher does. The watcher floors its poll at one second and
/// debounces for half of one, so three intervals is the observed cost and ten is the failing bound.
const PATIENCE: Duration = Duration::from_secs(20);

// -------------------------------------------------------------------------------------------
// The reconciler against real watchers.
// -------------------------------------------------------------------------------------------
#[test]
fn a_run_watches_both_repositories_stops_one_and_status_json_reports_both() {
    let home = TempHome::new("reconciler");
    let alpha = home.repo("alpha", Some(FAST_CONFIG));
    let beta = home.repo("beta", Some(FAST_CONFIG));
    let paused = home.repo("paused", Some(PAUSED_CONFIG));
    let delta = home.repo("delta", Some(FAST_CONFIG));
    home.registry(&[alpha.clone(), beta.clone()]);
    // A file before the first poll is the baseline the watcher compares against; the bumps below are
    // the change that earns a run.
    bump(&alpha, 0);
    bump(&beta, 0);

    let reports: Arc<Mutex<Vec<ReconcileReport>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = Arc::clone(&reports);
    let stop = Arc::new(Notify::new());
    let run = DaemonRun::start(
        ForegroundOptions {
            home: home.path().to_path_buf(),
            worker_cmd: home.fake_worker(),
            interval_ms: Some(20),
            on_tick: Some(Arc::new(move |report: &ReconcileReport| {
                collector
                    .lock()
                    .expect("the report list")
                    .push(report.clone());
            })),
            stop: Some(Arc::clone(&stop)),
            ..ForegroundOptions::new(home.path().to_path_buf())
        },
        Arc::clone(&stop),
    );

    assert!(
        wait_until(
            || {
                reports
                    .lock()
                    .expect("the report list")
                    .iter()
                    .any(|report| {
                        report.started == vec![alpha.clone(), beta.clone()]
                            && report.watched == vec![alpha.clone(), beta.clone()]
                    })
            },
            PATIENCE
        ),
        "one pass starts one watcher per registered repository: {:?}",
        reports.lock().expect("the report list").clone()
    );

    // A run of both trees, driven through `sangkar-index`'s worker entry with the fake command.
    assert!(
        wait_until(
            || {
                bump(&alpha, 1);
                bump(&beta, 1);
                let runs = index_runs(&home);
                runs.iter().any(|arg| arg == &alpha) && runs.iter().any(|arg| arg == &beta)
            },
            PATIENCE
        ),
        "both watchers indexed their tree: {:?}",
        index_runs(&home)
    );

    // The recorded rows: both roots, the pass time and the worker's exit code.
    let mut reported = Value::Null;
    assert!(
        wait_until(
            || {
                let answer = run_cli(home.path(), &["status", "--json"], &[], &[]);
                if answer.code != 0 {
                    return false;
                }
                let Ok(status) = serde_json::from_str::<Value>(&answer.stdout) else {
                    return false;
                };
                let rows = status
                    .get("repositories")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let recorded = |root: &str| {
                    rows.iter().any(|row| {
                        row.get("root").and_then(Value::as_str) == Some(root)
                            && row.get("lastExit").and_then(Value::as_i64) == Some(0)
                            && row
                                .get("lastRun")
                                .and_then(Value::as_str)
                                .is_some_and(|at| at.ends_with('Z'))
                    })
                };
                if rows.len() == 2 && recorded(&alpha) && recorded(&beta) {
                    reported = status;
                    return true;
                }
                false
            },
            PATIENCE
        ),
        "status --json carries both roots with their last exit codes: {reported}"
    );
    assert_eq!(
        reported.get("pid").and_then(Value::as_u64),
        Some(u64::from(std::process::id())),
        "the daemon thread holds this process's lock"
    );
    assert_eq!(
        reported.get("report"),
        Some(&Value::Null),
        "no report server"
    );
    assert_eq!(
        reported
            .get("repositories")
            .and_then(Value::as_array)
            .map(|rows| rows
                .iter()
                .filter_map(|row| row.get("root").and_then(Value::as_str))
                .collect::<Vec<&str>>()),
        Some(vec![alpha.as_str(), beta.as_str()]),
        "one row per registered root, in the registry's order"
    );

    // Removing beta stops exactly beta's watcher, and leaves alpha's alone.
    home.registry(std::slice::from_ref(&alpha));
    assert!(
        wait_until(
            || {
                reports
                    .lock()
                    .expect("the report list")
                    .iter()
                    .any(|report| {
                        report.stopped == vec![beta.clone()]
                            && report.watched == vec![alpha.clone()]
                            && report.started.is_empty()
                    })
            },
            PATIENCE
        ),
        "a de-registered root is stopped and reported: {:?}",
        reports.lock().expect("the report list").clone()
    );
    let before = index_runs(&home).iter().filter(|arg| *arg == &beta).count();
    bump(&beta, 2);
    thread::sleep(Duration::from_millis(1_500));
    assert_eq!(
        index_runs(&home).iter().filter(|arg| *arg == &beta).count(),
        before,
        "a stopped watcher cannot spawn another run"
    );

    // Adding a watched root starts one, and the paused root is skipped and never spawns.
    home.registry(&[alpha.clone(), paused.clone(), delta.clone()]);
    assert!(
        wait_until(
            || {
                reports
                    .lock()
                    .expect("the report list")
                    .iter()
                    .any(|report| {
                        report.started == vec![delta.clone()]
                            && report.skipped == vec![paused.clone()]
                            && report.watched == vec![alpha.clone(), delta.clone()]
                            && report.stopped.is_empty()
                    })
            },
            PATIENCE
        ),
        "a new registration starts one watcher and `watch: false` opens none: {:?}",
        reports.lock().expect("the report list").clone()
    );
    bump(&paused, 1);
    bump(&delta, 1);
    thread::sleep(Duration::from_millis(1_500));
    assert!(
        !index_runs(&home).iter().any(|arg| arg == &paused),
        "a paused repository never spawned a worker: {:?}",
        index_runs(&home)
    );

    assert_eq!(run.finish(), 0, "the stop seam ends the loop cleanly");
    assert!(
        !home.join("daemon.pid").exists(),
        "a clean stop removes the pid file"
    );
}

// -------------------------------------------------------------------------------------------
// The composition order.
// -------------------------------------------------------------------------------------------
/// A report server that records when the composition reached it, and proves the lock was already
/// held: the third step must come after the first, and the close must come last.
struct RecordingServer {
    order: Arc<Mutex<Vec<&'static str>>>,
    pid_path: PathBuf,
    held_at_start: Arc<AtomicUsize>,
}

impl ReportServer for RecordingServer {
    fn start(&mut self, host: &str, port: u16) -> Result<ReportAddress, AppError> {
        if read_pid_lock(&self.pid_path).is_ok_and(|state| state.held) {
            self.held_at_start.fetch_add(1, Ordering::Relaxed);
        }
        self.order
            .lock()
            .expect("the order list")
            .push("report.start");
        Ok(ReportAddress {
            url: format!("http://{host}:{port}"),
            host: host.to_owned(),
            port,
        })
    }

    fn close(&mut self) {
        self.order
            .lock()
            .expect("the order list")
            .push("report.close");
    }
}

#[test]
fn the_composition_takes_the_lock_then_binds_and_closes_in_reverse_order() {
    let home = TempHome::new("composition");
    let alpha = home.repo("alpha", Some(FAST_CONFIG));
    home.registry(std::slice::from_ref(&alpha));
    let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
    let held_at_start = Arc::new(AtomicUsize::new(0));
    let server = RecordingServer {
        order: Arc::clone(&order),
        pid_path: home.join("daemon.pid"),
        held_at_start: Arc::clone(&held_at_start),
    };
    let stop = Arc::new(Notify::new());
    let order_for_tick = Arc::clone(&order);
    let stop_for_tick = Arc::clone(&stop);

    let options = ForegroundOptions {
        home: home.path().to_path_buf(),
        worker_cmd: home.fake_worker(),
        interval_ms: Some(20),
        report: Some(Box::new(server)),
        on_tick: Some(Arc::new(move |_report: &ReconcileReport| {
            order_for_tick.lock().expect("the order list").push("tick");
            // One pass is enough to prove the loop; the seam stops it the way a signal would.
            stop_for_tick.notify_one();
        })),
        stop: Some(Arc::clone(&stop)),
        ..ForegroundOptions::new(home.path().to_path_buf())
    };

    assert_eq!(run_daemon_foreground(options), 0);

    assert_eq!(
        order.lock().expect("the order list").clone(),
        vec!["report.start", "tick", "report.close"],
        "lock, reconciler, report, loop — and the reverse on shutdown"
    );
    assert_eq!(
        held_at_start.load(Ordering::Relaxed),
        1,
        "the pid file was held before the report bound"
    );
    let after = sangkar_daemon::lifecycle::status(home.path());
    assert_eq!(after.pid, None, "the seam is a clean stop");
    assert_eq!(after.report, None, "a stopped daemon advertises no address");
    assert!(
        !home.join("daemon.pid").exists(),
        "a clean stop removes the pid file"
    );
}

/// A held lock refuses the composition before the reconciler or the report exists, and a report that
/// cannot bind leaves no pid file and no pass behind it.
#[test]
fn a_refused_composition_leaves_nothing_behind() {
    let home = TempHome::new("held");
    let alpha = home.repo("alpha", Some(FAST_CONFIG));
    home.registry(std::slice::from_ref(&alpha));
    let pid_path = home.join("daemon.pid");
    let held = acquire_pid_lock(&pid_path).expect("the test takes the lock first");
    let ticks = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(AtomicUsize::new(0));
    let ticks_for_tick = Arc::clone(&ticks);
    let opened_for_server = Arc::clone(&opened);

    let options = ForegroundOptions {
        home: home.path().to_path_buf(),
        worker_cmd: home.fake_worker(),
        interval_ms: Some(20),
        report: Some(Box::new(CountingServer {
            starts: opened_for_server,
        })),
        on_tick: Some(Arc::new(move |_report: &ReconcileReport| {
            ticks_for_tick.fetch_add(1, Ordering::Relaxed);
        })),
        ..ForegroundOptions::new(home.path().to_path_buf())
    };

    assert_eq!(
        run_daemon_foreground(options),
        1,
        "a live daemon is not displaced"
    );
    assert_eq!(ticks.load(Ordering::Relaxed), 0, "a refusal runs no pass");
    assert_eq!(
        opened.load(Ordering::Relaxed),
        0,
        "a refusal binds no report"
    );
    assert!(
        read_pid_lock(&pid_path)
            .is_ok_and(|state| state.held && state.pid == Some(std::process::id())),
        "the holder's own file is untouched by the refusal"
    );
    release_pid_lock(&held).expect("release the test's own lock");
    assert!(!pid_path.exists());

    let failing = TempHome::new("bind-failure");
    let repo = failing.repo("alpha", Some(FAST_CONFIG));
    failing.registry(&[repo]);
    let ticks = Arc::new(AtomicUsize::new(0));
    let ticks_for_tick = Arc::clone(&ticks);
    let options = ForegroundOptions {
        home: failing.path().to_path_buf(),
        worker_cmd: failing.fake_worker(),
        interval_ms: Some(20),
        report: Some(Box::new(FailingServer)),
        on_tick: Some(Arc::new(move |_report: &ReconcileReport| {
            ticks_for_tick.fetch_add(1, Ordering::Relaxed);
        })),
        ..ForegroundOptions::new(failing.path().to_path_buf())
    };

    assert_eq!(
        run_daemon_foreground(options),
        1,
        "a bind failure fails run"
    );
    assert_eq!(
        ticks.load(Ordering::Relaxed),
        0,
        "a bind failure runs no pass"
    );
    assert!(
        !failing.join("daemon.pid").exists(),
        "a bind failure leaves no pid file"
    );
    assert!(
        !failing.join("daemon.state.json").exists(),
        "no pass ran, so nothing was recorded"
    );
}

/// A server that must never be reached: any call is a composition that skipped the lock.
struct CountingServer {
    starts: Arc<AtomicUsize>,
}

impl ReportServer for CountingServer {
    fn start(&mut self, _host: &str, _port: u16) -> Result<ReportAddress, AppError> {
        self.starts.fetch_add(1, Ordering::Relaxed);
        Err(AppError::internal("this server must not be started"))
    }

    fn close(&mut self) {}
}

/// A server whose bind refuses, the `EADDRINUSE` case: the composition must undo the lock only.
struct FailingServer;

impl ReportServer for FailingServer {
    fn start(&mut self, _host: &str, _port: u16) -> Result<ReportAddress, AppError> {
        Err(AppError::internal("address already in use"))
    }

    fn close(&mut self) {}
}

// -------------------------------------------------------------------------------------------
// The four subcommands.
// -------------------------------------------------------------------------------------------
#[test]
fn the_cli_answers_the_four_subcommands_with_their_lines_and_codes() {
    let home = TempHome::new("cli");
    let paused = home.repo("paused", Some(PAUSED_CONFIG));
    let plain = home.repo("plain", None);
    home.registry(&[paused.clone(), plain.clone()]);
    let command = home.fake_worker();

    // `status` on an untouched home: the header line and exit 1.
    let empty = TempHome::new("cli-empty");
    let answer = run_cli(empty.path(), &["status"], &command, &[]);
    assert_eq!(answer.code, 1);
    assert_eq!(answer.stdout, "not running\n");
    assert!(!answer.stderr.contains("unknown tool"), "{answer:?}");

    // A row per registered repository, with the settings its own file (or the machine defaults) name.
    let answer = run_cli(home.path(), &["status"], &command, &[]);
    assert_eq!(
        answer.stdout,
        format!(
            "not running\n  {paused}  watch=off interval=5000ms telemetry=off\n  {plain}  watch=on interval=2000ms telemetry=on\n"
        ),
        "{answer:?}"
    );

    // The pid file *is* what makes `status` say running, and it exits 0 only then.
    let pid_path = home.join("daemon.pid");
    fs::write(
        &pid_path,
        format!(
            r#"{{"pid":{},"startedAt":"1970-01-01T00:00:00.000Z"}}"#,
            std::process::id()
        ),
    )
    .expect("a live pid file");
    let answer = run_cli(home.path(), &["status"], &command, &[]);
    assert_eq!(answer.code, 0);
    assert_eq!(
        answer.stdout,
        format!(
            "running, pid {}\n  {paused}  watch=off interval=5000ms telemetry=off\n  {plain}  watch=on interval=2000ms telemetry=on\n",
            std::process::id()
        )
    );

    let answer = run_cli(home.path(), &["status", "--json"], &command, &[]);
    assert_eq!(answer.code, 0, "a live daemon is running: {answer:?}");
    assert!(
        answer.stdout.ends_with("\n") && answer.stdout.lines().count() == 1,
        "the object plus one newline, nothing else: {:?}",
        answer.stdout
    );
    let status: Value = serde_json::from_str(&answer.stdout).expect("the DaemonStatus object");
    assert_eq!(
        status.get("pid").and_then(Value::as_u64),
        Some(u64::from(std::process::id()))
    );
    assert_eq!(
        status
            .get("repositories")
            .and_then(Value::as_array)
            .map(|rows| rows
                .iter()
                .filter_map(|row| row.get("root").and_then(Value::as_str))
                .collect::<Vec<&str>>()),
        Some(vec![paused.as_str(), plain.as_str()]),
        "both roots, in the registry's order"
    );

    // The global flags are accepted next to every subcommand and change nothing but `status --json`.
    let answer = run_cli(
        home.path(),
        &["--repo-root", "/elsewhere", "status", "--json"],
        &command,
        &[],
    );
    assert_eq!(answer.code, 0, "{answer:?}");
    assert!(answer.stdout.starts_with(r#"{"pid":"#) || answer.stdout.starts_with("{\"pid\":"));

    // `stop` with no daemon: one line and exit 1.
    fs::remove_file(&pid_path).expect("drop the hand-written pid file");
    let answer = run_cli(home.path(), &["stop"], &command, &[]);
    assert_eq!(answer.code, 1);
    assert_eq!(answer.stdout, "not running\n");

    // Usage refusals: exit 2, nothing on stdout, the reason and the block on stderr.
    let cases: [(&[&str], &str); 6] = [
        (&[], "daemon needs a subcommand"),
        (&["bogus"], "unknown subcommand \"bogus\""),
        (
            &["status", "--port", "20"],
            "\"daemon status\" takes no --port",
        ),
        (
            &["start", "--port", "20"],
            "\"daemon start\" takes no --port",
        ),
        (&["run", "--port", "abc"], "--port must be an integer"),
        (&["run", "--port"], "--port needs a value"),
    ];
    for (argv, reason) in cases {
        let answer = run_cli(home.path(), argv, &command, &[]);
        assert_eq!(answer.code, 2, "{argv:?} is a usage refusal: {answer:?}");
        assert_eq!(answer.stdout, "", "{argv:?} writes no stdout");
        assert!(
            answer.stderr.contains(reason),
            "{argv:?} names why: {answer:?}"
        );
        assert!(
            answer.stderr.contains("usage: sangkar daemon"),
            "{argv:?} ends with the block: {answer:?}"
        );
    }
    let answer = run_cli(home.path(), &["run", "--host", ""], &command, &[]);
    assert_eq!(answer.code, 2, "an empty host is a usage refusal");
    assert!(answer.stderr.contains("--host needs a value"));
    let answer = run_cli(home.path(), &["status", "--data-dir"], &command, &[]);
    assert_eq!(answer.code, 2, "a dangling global is a usage refusal");
    assert!(answer.stderr.contains("--data-dir needs a value"));

    // `start` against a live daemon: exit 1, nothing on stdout, nothing spawned.
    let held = acquire_pid_lock(&pid_path).expect("the test takes the lock first");
    let answer = run_cli(home.path(), &["start"], &command, &[]);
    assert_eq!(answer.code, 1, "a live daemon is not displaced: {answer:?}");
    assert_eq!(answer.stdout, "");
    assert!(
        !home.join("index-runs.log").exists(),
        "a refused start spawns nothing: {:?}",
        index_runs(&home)
    );
    release_pid_lock(&held).expect("release the test's own lock");
    assert!(!pid_path.exists());
}

// -------------------------------------------------------------------------------------------
// The report server `run` binds: `serve` behind the composition's seam.
// -------------------------------------------------------------------------------------------
/// The adapter `run` builds, bound through the composition: a request answers the page this home's
/// registry describes, and the port refuses once the shutdown closed it.
#[test]
fn a_run_serves_the_report_and_the_port_refuses_after_the_stop() {
    let home = TempHome::new("serving");
    let alpha = home.repo("alpha", Some(FAST_CONFIG));
    home.registry(std::slice::from_ref(&alpha));
    let stop = Arc::new(Notify::new());
    let options = ForegroundOptions {
        home: home.path().to_path_buf(),
        worker_cmd: home.fake_worker(),
        interval_ms: Some(20),
        report: Some(Box::new(TelemetryReport::new(home.path()))),
        // `--port 0`: the kernel answers, so the case cannot collide with a real daemon or with the
        // other cases in this file.
        bind: BindAddress {
            host: "127.0.0.1".to_owned(),
            port: 0,
        },
        stop: Some(Arc::clone(&stop)),
        ..ForegroundOptions::new(home.path().to_path_buf())
    };
    let run = DaemonRun::start(options, Arc::clone(&stop));

    // The address `port: 0` read back is the one the loop recorded, which is how a caller observes
    // an ephemeral bind: `status` answers it only while this process holds the lock.
    let mut url = String::new();
    assert!(
        wait_until(
            || {
                let address = sangkar_daemon::lifecycle::status(home.path()).report;
                match address {
                    Some(address) => {
                        url = address.url;
                        true
                    }
                    None => false,
                }
            },
            PATIENCE
        ),
        "the serving run recorded its bound address"
    );
    assert!(
        url.starts_with("http://127.0.0.1:"),
        "the recorded address is the loopback origin: {url}"
    );
    assert!(
        home.join("daemon.pid").exists(),
        "the lock is held while the report is served"
    );

    // One read of the page: a status line, the content type, and this home's registry in the body.
    let answer = http_get(&url);
    assert!(
        answer.status_line.starts_with("HTTP/1.1 200"),
        "{url} answered {} rather than 200",
        answer.status_line
    );
    assert!(
        answer.content_type.starts_with("text/html"),
        "the report is an HTML page, not {:?}",
        answer.content_type
    );
    assert!(
        answer
            .body
            .contains("<title>sangkar telemetry report</title>"),
        "the body is the report document: {:?}",
        &answer.body[..answer.body.len().min(200)]
    );
    assert!(
        answer.body.contains(&alpha),
        "one section per registered root, so the page is this home's: {alpha}"
    );

    assert_eq!(run.finish(), 0, "the stop seam ends the loop cleanly");

    // The same call that bound the server closed it: the port refuses, and the record above is gone.
    let address = origin_of(&url);
    assert!(
        TcpStream::connect_timeout(&address, Duration::from_millis(250)).is_err(),
        "{url} still accepts after the shutdown"
    );
    assert!(
        !home.join("daemon.pid").exists(),
        "a clean stop removes the pid file"
    );
    assert_eq!(
        sangkar_daemon::lifecycle::status(home.path()).report,
        None,
        "a stopped daemon advertises no address"
    );
}

/// A port this test already holds: `run` exits 1, leaves no pid file and reaches no pass, so nothing
/// was watched and the home is usable by the next start.
#[test]
fn a_run_whose_port_is_taken_fails_and_watches_nothing() {
    let home = TempHome::new("port-taken");
    let alpha = home.repo("alpha", Some(FAST_CONFIG));
    home.registry(std::slice::from_ref(&alpha));

    // The other listener: a real socket on an ephemeral port, released when the case ends.
    let holder = TcpListener::bind(("127.0.0.1", 0)).expect("the test's own listener");
    let port = holder.local_addr().expect("its address").port();

    let ticks = Arc::new(AtomicUsize::new(0));
    let ticks_for_tick = Arc::clone(&ticks);
    // A guard rather than a seam this case uses: the bind refusal returns before the loop exists, and
    // if it ever did not, the stored permit ends the loop at its first wait instead of hanging the
    // suite. A run that took this branch answers 0, so the exit-code assertion below still states
    // which path ran.
    let stop = Arc::new(Notify::new());
    stop.notify_one();
    let options = ForegroundOptions {
        home: home.path().to_path_buf(),
        worker_cmd: home.fake_worker(),
        interval_ms: Some(20),
        report: Some(Box::new(TelemetryReport::new(home.path()))),
        bind: BindAddress {
            host: "127.0.0.1".to_owned(),
            port,
        },
        on_tick: Some(Arc::new(move |_report: &ReconcileReport| {
            ticks_for_tick.fetch_add(1, Ordering::Relaxed);
        })),
        stop: Some(Arc::clone(&stop)),
        ..ForegroundOptions::new(home.path().to_path_buf())
    };

    assert_eq!(
        run_daemon_foreground(options),
        1,
        "a port already in use fails run"
    );
    assert_eq!(
        ticks.load(Ordering::Relaxed),
        0,
        "a bind failure runs no pass"
    );
    assert!(
        !home.join("daemon.pid").exists(),
        "a bind failure leaves no pid file, so the next start is not blocked by a lock nobody holds"
    );
    assert!(
        !home.join("daemon.state.json").exists(),
        "no pass ran, so nothing was recorded"
    );
    assert!(
        index_runs(&home).is_empty(),
        "no watcher ran, so no worker was spawned: {:?}",
        index_runs(&home)
    );

    // The home is usable again: the lock the refused run took was released, not left behind.
    let lock = acquire_pid_lock(&home.join("daemon.pid")).expect("the released lock is free");
    release_pid_lock(&lock).expect("release the test's own lock");
    drop(holder);
}

// -------------------------------------------------------------------------------------------
// The scaffolding: a temp home, fixtures, the fake worker, the CLI call and the daemon thread.
// -------------------------------------------------------------------------------------------
/// A `mkdtemp`-style home removed on drop. The name carries the pid and a counter, so two cases — or
/// two runs of this binary — never share a lock.
struct TempHome(PathBuf);

impl TempHome {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "sangkar-daemon-run-{label}-{}-{unique}",
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

    /// A repository directory with an optional on-disk config, returned in the canonical spelling the
    /// registry stores.
    fn repo(&self, name: &str, config: Option<&str>) -> String {
        let root = self.0.join(name);
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

    /// The registry through `sangkar_core`'s writer, so the test cannot drift from the file the
    /// reconciler reads.
    fn registry(&self, roots: &[String]) {
        let registry = Registry {
            version: REGISTRY_VERSION,
            repos: roots.iter().map(PathBuf::from).collect(),
        };
        write_registry(&self.0.join("sangkar.json"), &registry).expect("a registry");
    }

    /// The index worker command line: a script that records the arguments `sangkar-index` extends it
    /// with and answers "published", so no real parser runs in a case.
    fn fake_worker(&self) -> Vec<OsString> {
        let script = self.0.join("worker.sh");
        let log = self.0.join("index-runs.log");
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nexit 0\n",
                log.display()
            ),
        )
        .expect("a fake worker");
        let mut permissions = fs::metadata(&script).expect("its metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).expect("an executable fake worker");
        vec![script.into_os_string()]
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The daemon loop on a thread of this test, stopped through its `Notify` seam — and by `Drop` when
/// an assertion fails, so no watcher outlives the case.
struct DaemonRun {
    stop: Arc<Notify>,
    handle: Option<JoinHandle<i32>>,
}

impl DaemonRun {
    fn start(options: ForegroundOptions, stop: Arc<Notify>) -> Self {
        let handle = thread::spawn(move || run_daemon_foreground(options));
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stops the loop the way a signal would and answers its exit code.
    fn finish(mut self) -> i32 {
        self.stop.notify_one();
        self.handle
            .take()
            .expect("a started run")
            .join()
            .expect("the daemon thread")
    }
}

impl Drop for DaemonRun {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// One CLI answer, with the bytes a caller's streams would have received.
#[derive(Debug)]
struct CliAnswer {
    code: i32,
    stdout: String,
    stderr: String,
}

/// Runs `sangkar daemon <argv>` against `home`, with the streams captured.
fn run_cli(home: &Path, argv: &[&str], command: &[OsString], worker_cmd: &[OsString]) -> CliAnswer {
    let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_daemon_cli(DaemonInvocation {
        argv: &argv,
        home,
        command: command.to_vec(),
        worker_cmd: worker_cmd.to_vec(),
        report: None,
        stdout: &mut stdout,
        stderr: &mut stderr,
    });
    CliAnswer {
        code,
        stdout: String::from_utf8(stdout).expect("utf-8 stdout"),
        stderr: String::from_utf8(stderr).expect("utf-8 stderr"),
    }
}

/// The worker arguments recorded so far: five per run (`worker --repo-root <root> --data-dir <dir>`).
fn index_runs(home: &TempHome) -> Vec<String> {
    fs::read_to_string(home.join("index-runs.log")).map_or_else(
        |_| Vec::new(),
        |raw| raw.lines().map(str::to_owned).collect(),
    )
}

/// Writes a parseable file into `root`, so the watcher's signature differs from its baseline. The
/// counter makes two writes in the same millisecond distinct.
fn bump(root: &str, generation: usize) {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    fs::write(
        Path::new(root).join("main.ts"),
        format!("export const value = {generation}_{unique};\n"),
    )
    .expect("a changed source file");
}

/// One HTTP answer, split into the pieces a case asserts on.
struct HttpAnswer {
    status_line: String,
    content_type: String,
    body: String,
}

/// A `GET /` over a socket this test opens. The connect, the write and the read all have bounds, so a
/// listener that never answers fails the case instead of hanging it, and `Connection: close` is what
/// lets the read end at the last byte.
fn http_get(url: &str) -> HttpAnswer {
    let address = origin_of(url);
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("{url} did not accept: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("a read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("a write timeout");
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n"
    )
    .expect("the request");
    stream.flush().expect("the request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("the response");

    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("{url} answered no complete response: {raw:?}"));
    let mut lines = head.lines();
    let status_line = lines.next().expect("a status line").to_owned();
    let content_type = lines
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_else(|| panic!("{url} answered no content-type: {head:?}"));
    HttpAnswer {
        status_line,
        content_type,
        body: body.to_owned(),
    }
}

/// The socket `http://127.0.0.1:<port>/` names, which is the origin the report server answered with.
fn origin_of(url: &str) -> SocketAddr {
    let authority = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("{url} is not an http origin"))
        .trim_end_matches('/');
    authority
        .parse()
        .unwrap_or_else(|error| panic!("{url} is not a socket address: {error}"))
}

/// Polls `probe` until it answers `true` or the deadline passes; the answer is the return value.
fn wait_until(mut probe: impl FnMut() -> bool, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}
