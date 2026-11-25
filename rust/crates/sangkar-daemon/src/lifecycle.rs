//! The daemon's lifecycle: one pid lock, one foreground loop, and the three verbs an operator needs
//! (`start`, `stop`, `status`). This module owns *when* a daemon runs; what it does per tick arrives
//! as [`DaemonOptions::tick`], so the reconcile loop (`TASK-056`) and the composition root can be
//! wired without this module knowing what a watcher is.
//!
//! Ported from `src/daemon/lifecycle.ts` (355 lines). Two rules make the singleton hold together:
//!
//! - **The acquirer releases.** [`run_daemon`] uses [`DaemonOptions::lock`] when it is given one and
//!   otherwise acquires the pid file itself, and it never releases a lock it did not acquire. The
//!   caller that took the lock releases it, so one handle is never released twice and the
//!   composition root can keep its `finally` as the only release site.
//! - **A daemon that cannot serve leaves no pid file.** A failing first tick releases a
//!   self-acquired lock, so a refused start never blocks the next one for [`PID_STALE_AFTER_MS`].
//!
//! The loop is a poll, not an event: [`DEFAULT_DAEMON_TICK_MS`] is short enough that a registry edit
//! is noticed while an operator is still in the shell. `tokio`'s signal streams are the one async
//! surface here — installed with the same `signal::unix` calls `sangkar_cli::mcp` uses — and the
//! public API stays synchronous by owning a current-thread runtime, because every caller of the four
//! verbs is a synchronous CLI branch.
//!
//! # `<home>/daemon.pid`, `<home>/daemon.log`, `<home>/daemon.state.json`
//!
//! The pid file is `sangkar_core::pidlock`'s, byte for byte the TypeScript's `{"pid","startedAt"}`
//! shape, so a TypeScript daemon can reclaim what this one leaves. The log is the detached child's
//! stdout *and* stderr, because nobody is left to read a pipe once `start` returns. The state file is
//! this port's own addition: `status` runs in another process and cannot read the reconciler's
//! memory, so the loop persists the per-repository record `status` reports and the report address it
//! was given. It is a record, not a claim — `status` prints the address only while the pid lock is
//! live.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sangkar_core::errors::AppError;
use sangkar_core::global_config::{read_global_config, repo_defaults_of};
use sangkar_core::log;
use sangkar_core::pidlock::{
    PidLockHandle, PidLockState, acquire_pid_lock, heartbeat_pid_lock, read_pid_lock,
    release_pid_lock,
};
use sangkar_core::registry::read_registry;
use sangkar_core::repo_config::read_repo_config;
use serde_json::Value;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::Notify;
use tokio::time::{Instant as TokioInstant, interval_at};

use crate::types::{DaemonState, DaemonStatus, RepoRecord, RepoRuntime, ReportAddress};

/// The loop's period when neither the caller nor a future flag names one: five seconds is short
/// enough that a registry edit is noticed while an operator is still in the shell. The composition
/// root passes the reconciler's own period (also five seconds), which is the value production uses.
pub const DEFAULT_DAEMON_TICK_MS: u64 = 5_000;

/// The singleton, spelled as `#shared/config.ts` spells it; both trees must agree on the file name.
const PID_FILE: &str = "daemon.pid";

/// The detached child's stdout and stderr, which is where every [`log`] line it writes lands.
const LOG_FILE: &str = "daemon.log";

/// What the running daemon records for a later `status`: the report address and one record per root.
const STATE_FILE: &str = "daemon.state.json";

/// The registry beside the pid file, spelled once so this module cannot drift from `sangkar_core`.
const REGISTRY_FILE: &str = "sangkar.json";

/// How long `start` waits for the child to prove it bound the singleton, and `stop` for it to go.
const START_TIMEOUT_MS: u64 = 5_000;
const STOP_TIMEOUT_MS: u64 = 5_000;

/// Fast enough that a healthy daemon answers in one poll, cheap enough to run for five seconds.
const POLL_MS: u64 = 50;

/// One unit of work: the loop calls it once immediately and then once per interval. The answer is
/// what the loop persists for `status` — one record per repository the pass covered — so a
/// reconciler (`TASK-056`) reports through its return value instead of writing the state file
/// itself. `Fn`, not `FnMut`: the loop holds the options by shared reference (the contract's
/// `&DaemonOptions`), so a stateful pass keeps its state behind a `RefCell` inside the closure.
pub type DaemonTick = Box<dyn Fn() -> Result<Vec<RepoRecord>, AppError>>;

/// Everything the foreground daemon and the detached start need. Every seam is optional; production
/// passes a home, a tick, a command and, when it serves, the report address.
pub struct DaemonOptions {
    /// The machine-level home: `daemon.pid`, `daemon.log`, `daemon.state.json` and `sangkar.json`
    /// are taken inside it.
    pub home: PathBuf,
    /// Tick period; `None` takes [`DEFAULT_DAEMON_TICK_MS`].
    pub interval_ms: Option<u64>,
    /// One unit of work; required by [`run_daemon`], unused by [`start`].
    pub tick: Option<DaemonTick>,
    /// The command line that runs `daemon run` — program first — spawned detached by [`start`]. The
    /// child inherits this process's environment, so it resolves the same machine home this process
    /// did; passing the home again would be a second environment read, which the plan forbids.
    pub command: Vec<OsString>,
    /// A lock the caller already holds. Used as-is and never released here, so the handle is
    /// released exactly once — by whoever acquired it.
    pub lock: Option<PidLockHandle>,
    /// The address the composition's report server bound, recorded so `status` can print it. `None`
    /// when this daemon serves no report.
    pub report: Option<ReportAddress>,
    /// Test and caller seam: notifying this stops the loop the way `SIGINT` does.
    pub stop: Option<Arc<Notify>>,
}

impl DaemonOptions {
    /// The lifecycle with every seam empty; a caller fills what its verb needs.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            interval_ms: None,
            tick: None,
            command: Vec::new(),
            lock: None,
            report: None,
            stop: None,
        }
    }
}

/// What [`start`] did: a pid when the child bound the lock, a reason when it did not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartOutcome {
    /// `0` when the daemon bound the singleton, `1` when it did not — the exit code the CLI answers.
    pub code: i32,
    /// The pid the child wrote into the pid file, present only when it started.
    pub pid: Option<u32>,
    /// Why the start was refused or failed; absent on success. A live holder is named here with its
    /// pid, because "already running" without the pid is a message an operator has to go and verify.
    pub reason: Option<String>,
}

/// What [`stop`] did: the pid it signalled, and whether the daemon is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StopOutcome {
    /// `0` when the pid file is gone at the end, `1` otherwise — the exit code the CLI answers.
    pub code: i32,
    /// The pid the lock recorded. `Some` with [`Self::code`] `1` is the stale case: the file named a
    /// process that was already gone, which is a fact about the home, not a failed stop.
    pub pid: Option<u32>,
}

/// The foreground loop: take the singleton, run one tick and then one per interval, and answer `0`
/// when a signal stops it.
///
/// A tick after the first that throws is logged and forgotten: a reconcile pass that hit one
/// unreadable repository must not take down the daemon that serves the other four. The first tick is
/// the exception, because a daemon whose very first pass fails has nothing to serve — it releases a
/// self-acquired lock, clears the report address it advertised, and answers `1`.
///
/// # Panics
///
/// Panics only if called from inside a Tokio runtime: the public API is synchronous, so the loop
/// owns a current-thread runtime of its own. Every caller in this workspace is a synchronous CLI
/// branch.
#[must_use]
pub fn run_daemon(opts: &DaemonOptions) -> i32 {
    let Some(tick) = opts.tick.as_deref() else {
        log::error(
            "daemon run refused: no tick",
            &[("home", Value::String(opts.home.display().to_string()))],
        );
        return 1;
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error(
                "daemon run refused: no runtime",
                &[("message", Value::String(error.to_string()))],
            );
            return 1;
        }
    };
    runtime.block_on(run_loop(opts, tick))
}

async fn run_loop(
    opts: &DaemonOptions,
    tick: &dyn Fn() -> Result<Vec<RepoRecord>, AppError>,
) -> i32 {
    let pid_path = opts.home.join(PID_FILE);
    let acquired;
    let handle: &PidLockHandle = match &opts.lock {
        Some(handle) => handle,
        None => match acquire_pid_lock(&pid_path) {
            Ok(handle) => {
                acquired = handle;
                &acquired
            }
            Err(error) => {
                log::error(
                    "daemon refused the pid lock",
                    &[
                        ("path", Value::String(pid_path.display().to_string())),
                        ("code", Value::String(error.code.to_string())),
                        ("message", Value::String(error.message)),
                    ],
                );
                return 1;
            }
        },
    };
    let owns = opts.lock.is_none();

    // Installed before the first pass: a SIGTERM that arrives while the first reconcile runs would
    // otherwise take the default action and leave the pid file behind for the stale window.
    let mut interrupt = install(SignalKind::interrupt(), "SIGINT");
    let mut terminate = install(SignalKind::terminate(), "SIGTERM");
    let mut stop = opts.stop.clone();

    // The report address first and the previous rows kept: a daemon that fails its first pass must
    // not leave the *previous* daemon's address looking current for as long as its lock is held.
    let previous = read_daemon_state(&opts.home).repositories;
    record_state(&opts.home, opts.report.as_ref(), &previous);

    match tick() {
        Ok(records) => record_state(&opts.home, opts.report.as_ref(), &records),
        Err(error) => {
            log::error(
                "daemon first tick failed",
                &[("message", Value::String(error.message))],
            );
            record_state(&opts.home, None, &previous);
            release(handle, owns);
            return 1;
        }
    }

    let period = Duration::from_millis(opts.interval_ms.unwrap_or(DEFAULT_DAEMON_TICK_MS).max(1));
    // `interval_at`, not `interval`: the first pass already ran, so the next one is one period away.
    let mut ticker = interval_at(TokioInstant::now() + period, period);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // The heartbeat is what keeps this lock from looking abandoned after
                // `PID_STALE_AFTER_MS`; a heartbeat that fails skips the pass and says so, exactly as
                // the TypeScript's single try-block does.
                if let Err(error) = heartbeat_pid_lock(handle) {
                    log::error(
                        "daemon tick failed",
                        &[("message", Value::String(error.message))],
                    );
                    continue;
                }
                match tick() {
                    Ok(records) => record_state(&opts.home, opts.report.as_ref(), &records),
                    Err(error) => {
                        log::error(
                            "daemon tick failed",
                            &[("message", Value::String(error.message))],
                        );
                    }
                }
            }
            name = next_signal(&mut interrupt, "SIGINT") => {
                log::info("daemon.signal", &[("signal", Value::String(name))]);
                break;
            }
            name = next_signal(&mut terminate, "SIGTERM") => {
                log::info("daemon.signal", &[("signal", Value::String(name))]);
                break;
            }
            () = requested_stop(&mut stop) => {
                log::info("daemon.stopped", &[("reason", Value::String("stop seam".to_owned()))]);
                break;
            }
        }
    }

    // One shutdown, in the order a stop has to happen: stop advertising, then drop the lock, so
    // nobody reads an address this process no longer serves. A release that fails is logged and
    // still exits 0: the loop is over either way, and a stop is not a crash.
    record_state(
        &opts.home,
        None,
        &read_daemon_state(&opts.home).repositories,
    );
    release(handle, owns);
    0
}

fn release(handle: &PidLockHandle, owns: bool) {
    if !owns {
        return;
    }
    if let Err(error) = release_pid_lock(handle) {
        log::error(
            "daemon shutdown failed",
            &[("message", Value::String(error.message))],
        );
    }
}

/// The two termination signals, streamed by `tokio`'s process-global driver. The shape — install once,
/// warn and carry on when a stream cannot be installed — is `sangkar_cli::mcp`'s, so a daemon that
/// cannot trap one signal is still a daemon that serves.
fn install(kind: SignalKind, name: &str) -> Option<Signal> {
    match signal(kind) {
        Ok(stream) => Some(stream),
        Err(error) => {
            log::warn(
                "daemon.signal_failed",
                &[
                    ("signal", Value::String(name.to_owned())),
                    ("message", Value::String(error.to_string())),
                ],
            );
            None
        }
    }
}

/// Waits for the next arrival on a stream, or forever when it could not be installed: an absent
/// signal is a branch that never wins the `select!`, not a busy loop.
async fn next_signal(stream: &mut Option<Signal>, name: &str) -> String {
    match stream {
        Some(stream) => {
            stream.recv().await;
            name.to_owned()
        }
        None => std::future::pending().await,
    }
}

/// The caller's stop seam: resolves once `Notify::notify_one` was called, including a notification
/// that arrived before the loop reached this branch (`Notify` keeps one permit).
async fn requested_stop(stop: &mut Option<Arc<Notify>>) {
    match stop {
        Some(notify) => notify.notified().await,
        None => std::future::pending().await,
    }
}

/// "Is the daemon up, and what is it serving": the pid file, the registry and the daemon's own state
/// file, each answer built from files so a stopped home reports `pid: None` rather than an error.
///
/// A pid file whose process is gone or whose heartbeat is aged is reported as no daemon, because that
/// is what an operator means by "running". A lock that cannot be read is reported the same way plus
/// one error line: the contract gives `status` no error channel, and an operator reading it still
/// needs the registry rows.
#[must_use]
pub fn status(home: &Path) -> DaemonStatus {
    let pid_path = home.join(PID_FILE);
    let lock = match read_pid_lock(&pid_path) {
        Ok(state) => state,
        Err(error) => {
            log::error(
                "daemon status could not read the pid lock",
                &[
                    ("path", Value::String(pid_path.display().to_string())),
                    ("message", Value::String(error.message)),
                ],
            );
            PidLockState {
                held: false,
                pid: None,
                stale: false,
                started_at: None,
            }
        }
    };
    let live = lock.held && !lock.stale;
    let DaemonState {
        report,
        repositories: records,
    } = read_daemon_state(home);
    let defaults = repo_defaults_of(&read_global_config(home));
    let repositories = read_registry(&home.join(REGISTRY_FILE))
        .repos
        .iter()
        .map(|root| {
            let key = root.display().to_string();
            // The repo's own file is re-read on every status, which is what makes an edit visible
            // without a daemon: `watch` is the file's answer, never the last pass's.
            let config = read_repo_config(root, None, defaults);
            let recorded = records.iter().find(|record| record.root == key);
            RepoRuntime {
                root: key,
                watch: config.watch,
                last_run: recorded.and_then(|record| record.last_run.clone()),
                last_exit: recorded.and_then(|record| record.last_exit),
                generation: recorded.and_then(|record| record.generation.clone()),
            }
        })
        .collect();

    let started_at = if live { lock.started_at } else { None };
    DaemonStatus {
        pid: if live { lock.pid } else { None },
        uptime_ms: started_at
            .as_deref()
            .and_then(parse_iso8601_millis)
            .and_then(|started| u64::try_from(now_millis() - started).ok()),
        started_at,
        repositories,
        report: if live { report } else { None },
    }
}

/// Starts the daemon as a detached child and waits for its pid file, which is the only proof that the
/// singleton was bound.
///
/// The child's stdin is `/dev/null` and its stdout *and* stderr append to `<home>/daemon.log`: the
/// daemon is detached, so nobody is left to read a pipe, and stdout stays free for the MCP protocol
/// discipline the rest of the workspace follows. It is put in a process group of its own
/// (`Command::process_group`, the one part of `detached: true` std can express without `libc`), so a
/// Ctrl-C in the operator's terminal does not reach it.
#[must_use]
pub fn start(opts: &DaemonOptions) -> StartOutcome {
    let pid_path = opts.home.join(PID_FILE);
    let existing = match read_pid_lock(&pid_path) {
        Ok(state) => state,
        Err(error) => {
            log::error(
                "daemon start failed: the pid file cannot be read",
                &[
                    ("path", Value::String(pid_path.display().to_string())),
                    ("message", Value::String(error.message)),
                ],
            );
            return StartOutcome {
                code: 1,
                pid: None,
                reason: Some(format!("cannot read {}", pid_path.display())),
            };
        }
    };
    if existing.held && !existing.stale {
        log::warn(
            "daemon start refused: a daemon already holds the pid file",
            &[
                ("path", Value::String(pid_path.display().to_string())),
                ("pid", existing.pid.map_or(Value::Null, Value::from)),
            ],
        );
        let holder = match existing.pid {
            Some(pid) => format!("a daemon already holds {} (pid {pid})", pid_path.display()),
            None => format!("a daemon already holds {}", pid_path.display()),
        };
        return StartOutcome {
            code: 1,
            pid: None,
            reason: Some(holder),
        };
    }

    if let Err(error) = fs::create_dir_all(&opts.home) {
        log::error(
            "daemon start failed: the home cannot be created",
            &[
                ("home", Value::String(opts.home.display().to_string())),
                ("message", Value::String(error.to_string())),
            ],
        );
        return StartOutcome {
            code: 1,
            pid: None,
            reason: Some(format!("cannot create {}", opts.home.display())),
        };
    }
    let Some((program, args)) = opts.command.split_first() else {
        log::error(
            "daemon start failed: no command to run",
            &[("home", Value::String(opts.home.display().to_string()))],
        );
        return StartOutcome {
            code: 1,
            pid: None,
            reason: Some("no daemon command was given".to_owned()),
        };
    };
    let log_path = opts.home.join(LOG_FILE);
    let log_file = match OpenOptions::new().append(true).create(true).open(&log_path) {
        Ok(file) => file,
        Err(error) => {
            log::error(
                "daemon start failed: the log cannot be opened",
                &[
                    ("path", Value::String(log_path.display().to_string())),
                    ("message", Value::String(error.to_string())),
                ],
            );
            return StartOutcome {
                code: 1,
                pid: None,
                reason: Some(format!("cannot open {}", log_path.display())),
            };
        }
    };
    // Two handles on one file: the child writes both streams, and this process keeps neither open.
    let (stdout, stderr) = match (log_file.try_clone(), log_file) {
        (Ok(stdout), stderr) => (Stdio::from(stdout), Stdio::from(stderr)),
        (Err(error), _) => {
            log::error(
                "daemon start failed: the log cannot be duplicated",
                &[
                    ("path", Value::String(log_path.display().to_string())),
                    ("message", Value::String(error.to_string())),
                ],
            );
            return StartOutcome {
                code: 1,
                pid: None,
                reason: Some(format!("cannot open {}", log_path.display())),
            };
        }
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .process_group(0);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            log::error(
                "daemon start failed: the child could not be spawned",
                &[
                    ("home", Value::String(opts.home.display().to_string())),
                    ("message", Value::String(error.to_string())),
                ],
            );
            return StartOutcome {
                code: 1,
                pid: None,
                reason: Some(format!("cannot spawn {}", program.to_string_lossy())),
            };
        }
    };
    // Dropping the handle neither kills nor reaps the child in Rust; the daemon is its own process
    // from here, exactly as `child.unref()` leaves it in the TypeScript.
    drop(child);

    // "Held" is not enough: a stale file left by a killed daemon is held too, and a start that
    // accepted it would report the dead pid as its own. The lock has to be held by a live process,
    // which is what the child's create-and-write produces in one step.
    if !wait_until(&|| is_live(&pid_path), START_TIMEOUT_MS) {
        log::error(
            "daemon did not write its pid file in time",
            &[
                ("path", Value::String(pid_path.display().to_string())),
                ("timeoutMs", Value::from(START_TIMEOUT_MS)),
                ("log", Value::String(log_path.display().to_string())),
            ],
        );
        return StartOutcome {
            code: 1,
            pid: None,
            reason: Some(format!(
                "the daemon wrote no pid file within {START_TIMEOUT_MS}ms"
            )),
        };
    }

    let pid = read_pid_lock(&pid_path).ok().and_then(|state| state.pid);
    log::info(
        "daemon started",
        &[
            ("pid", pid.map_or(Value::Null, Value::from)),
            ("home", Value::String(opts.home.display().to_string())),
        ],
    );
    StartOutcome {
        code: 0,
        pid,
        reason: None,
    }
}

/// Stops the daemon by the pid it recorded, and waits for the pid file to go — a "stopped" printed
/// before the process has exited is the message an operator acts on too early.
///
/// `SIGTERM` is sent with the `kill` utility rather than a `libc` call: no dependency may be added
/// for one signal, and `/bin/kill -TERM <pid>` is exactly what `tests/mcp_surface.rs` already does.
/// A pid that is already gone is reported as such and answers `1`, the same answer `status` gives
/// for the same home; nothing is ever refused.
#[must_use]
pub fn stop(home: &Path) -> StopOutcome {
    let pid_path = home.join(PID_FILE);
    let state = match read_pid_lock(&pid_path) {
        Ok(state) => state,
        Err(error) => {
            log::error(
                "daemon stop failed: the pid file cannot be read",
                &[
                    ("path", Value::String(pid_path.display().to_string())),
                    ("message", Value::String(error.message)),
                ],
            );
            return StopOutcome { code: 1, pid: None };
        }
    };
    let Some(pid) = state.pid.filter(|_| state.held) else {
        log::info(
            "daemon stop: not running",
            &[("path", Value::String(pid_path.display().to_string()))],
        );
        return StopOutcome { code: 1, pid: None };
    };

    match Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            // `ESRCH` is the stale case: the recorded process is gone, so the pid file is the
            // remains of a crash, not a daemon. Any other refusal is logged as what it is.
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            if message.to_ascii_lowercase().contains("no such process") {
                log::info(
                    "daemon stop: the recorded pid is gone",
                    &[
                        ("path", Value::String(pid_path.display().to_string())),
                        ("pid", Value::from(pid)),
                    ],
                );
            } else {
                log::error(
                    "daemon stop failed: kill refused the pid",
                    &[
                        ("pid", Value::from(pid)),
                        ("message", Value::String(message)),
                    ],
                );
            }
            return StopOutcome {
                code: 1,
                pid: Some(pid),
            };
        }
        Err(error) => {
            log::error(
                "daemon stop failed: kill could not run",
                &[("message", Value::String(error.to_string()))],
            );
            return StopOutcome {
                code: 1,
                pid: Some(pid),
            };
        }
    }

    if !wait_until(
        &|| read_pid_lock(&pid_path).is_ok_and(|state| !state.held),
        STOP_TIMEOUT_MS,
    ) {
        log::warn(
            "daemon did not remove its pid file in time",
            &[
                ("path", Value::String(pid_path.display().to_string())),
                ("pid", Value::from(pid)),
                ("timeoutMs", Value::from(STOP_TIMEOUT_MS)),
                (
                    "staleAfterMs",
                    Value::from(sangkar_core::pidlock::PID_STALE_AFTER_MS),
                ),
            ],
        );
        return StopOutcome {
            code: 1,
            pid: Some(pid),
        };
    }

    log::info(
        "daemon stopped",
        &[
            ("pid", Value::from(pid)),
            ("home", Value::String(home.display().to_string())),
        ],
    );
    StopOutcome {
        code: 0,
        pid: Some(pid),
    }
}

/// Whether the pid file names a live daemon: held, and not the remains of one that is gone.
fn is_live(pid_path: &Path) -> bool {
    read_pid_lock(pid_path).is_ok_and(|state| state.held && !state.stale)
}

/// Polls `probe` until it answers `true` or the deadline passes; the answer is the return value.
fn wait_until(probe: &dyn Fn() -> bool, timeout_ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if probe() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
}

/// Persists the report address and the rows the last pass produced, with one warning on failure: a
/// status file that cannot be written costs the answer's detail, never the daemon.
fn record_state(home: &Path, report: Option<&ReportAddress>, repositories: &[RepoRecord]) {
    let state = DaemonState {
        report: report.cloned(),
        repositories: repositories.to_vec(),
    };
    if let Err(error) = write_daemon_state(home, &state) {
        log::warn(
            "daemon state not written",
            &[
                (
                    "path",
                    Value::String(home.join(STATE_FILE).display().to_string()),
                ),
                ("message", Value::String(error.message)),
            ],
        );
    }
}

/// Replaces `<home>/daemon.state.json` atomically: the temp file carries this process's pid and the
/// rename is the publish, so a `status` running beside a tick reads either the previous record or
/// the new one, never half of either.
fn write_daemon_state(home: &Path, state: &DaemonState) -> Result<(), AppError> {
    let path = home.join(STATE_FILE);
    let temp = home.join(format!("{STATE_FILE}.{}.tmp", std::process::id()));
    let body = serde_json::to_string(state).map_err(|error| {
        AppError::internal(format!("cannot serialise the daemon state: {error}"))
    })?;
    fs::write(&temp, body)
        .map_err(|error| AppError::internal(format!("cannot write {}: {error}", temp.display())))?;
    fs::rename(&temp, &path).map_err(|error| {
        // A rename that left its temp behind must not leave the home littered for the next status.
        let _ = fs::remove_file(&temp);
        AppError::internal(format!("cannot publish {}: {error}", path.display()))
    })
}

/// The daemon's record, or an empty one. An absent file is the normal state of a home no daemon has
/// served since the port; an unreadable or malformed one is one warning and the empty record,
/// because `status` must still answer from the files it can read.
fn read_daemon_state(home: &Path) -> DaemonState {
    let path = home.join(STATE_FILE);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return DaemonState::default(),
        Err(error) => {
            log::warn(
                "daemon state ignored",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(format!("unreadable: {error}"))),
                ],
            );
            return DaemonState::default();
        }
    };
    match serde_json::from_str(&raw) {
        Ok(state) => state,
        Err(error) => {
            log::warn(
                "daemon state ignored",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(format!("invalid JSON: {error}"))),
                ],
            );
            DaemonState::default()
        }
    }
}

/// Milliseconds since the UNIX epoch — the unit a JavaScript date is an integer count of.
fn now_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // A host clock set before 1970 is not a state this tool supports; the epoch keeps the
        // arithmetic well defined instead of wrapping around it.
        Err(_) => 0,
    }
}

/// `Date.parse` for the one timestamp this module reads: the pid lock's `startedAt`, written by
/// `sangkar_core::pidlock` as `toISOString()`. Anything else is `None`, because an uptime computed
/// from a timestamp that is not this shape would be a number with no meaning.
fn parse_iso8601_millis(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() != 24
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'.'
        || bytes[23] != b'Z'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = raw.get(range)?;
        if !slice.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        slice.parse().ok()
    };
    let year = number(0..4)?;
    let month = number(5..7)?;
    let day = number(8..10)?;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let millis = number(20..23)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

/// Howard Hinnant's `days_from_civil`: the day count since 1970-01-01 for a proleptic Gregorian
/// date, the exact inverse of the `civil_from_days` `sangkar_core::log` and `pidlock` format from.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::{days_from_civil, parse_iso8601_millis};

    /// Every expectation is `Date.parse(...)` under Bun 1.4.4 for the bytes `toISOString()` writes,
    /// including the two dates either side of the epoch and a leap day. The formatter lives in
    /// `sangkar_core::pidlock`; this is the reader for it, and the pair must agree.
    #[test]
    fn the_pid_locks_started_at_parses_to_its_milliseconds() {
        let cases: [(i64, &str); 7] = [
            (0, "1970-01-01T00:00:00.000Z"),
            (1, "1970-01-01T00:00:00.001Z"),
            (-1_000, "1969-12-31T23:59:59.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_000_000_000_000, "2001-09-09T01:46:40.000Z"),
            (1_709_210_096_789, "2024-02-29T12:34:56.789Z"),
            (1_767_225_600_000, "2026-01-01T00:00:00.000Z"),
        ];
        for (millis, raw) in cases {
            assert_eq!(parse_iso8601_millis(raw), Some(millis), "{raw}");
        }
    }

    #[test]
    fn a_timestamp_that_is_not_the_written_shape_is_unreadable() {
        for raw in [
            "",
            "not a date",
            "2026-01-01T00:00:00Z",
            "2026-01-01 00:00:00.000Z",
            "2026-13-01T00:00:00.000Z",
            "2026-01-32T00:00:00.000Z",
            "2026-01-01T24:00:00.000Z",
            "2026-01-01T00:60:00.000Z",
            "2026-01-01T00:00:60.000Z",
            "2026-0a-01T00:00:00.000Z",
            "2026-01-01T00:00:00.000+00:00",
        ] {
            assert_eq!(parse_iso8601_millis(raw), None, "{raw}");
        }
    }

    #[test]
    fn the_day_count_is_the_one_the_formatter_counts_from() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
    }
}
