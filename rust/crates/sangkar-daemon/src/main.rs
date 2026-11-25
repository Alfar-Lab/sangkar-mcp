//! The daemon's composition root: the one process an operator starts.
//!
//! Ported from `src/daemon/main.ts`. Three slices, each already correct on its own, are joined here
//! and nowhere else — the singleton ([`sangkar_core::pidlock`]), the reconciler
//! ([`crate::reconcile`]) and the report server (`sangkar-telemetry`'s `serve`, arriving through
//! [`ReportServer`]). Keeping the join in one small module is what makes the order of the three
//! operations reviewable: an operator gets one process that holds the lock, keeps one watcher per
//! registered repository and serves the report, or gets an error and no leftovers.
//!
//! The order is the contract:
//!
//! 1. **The lock first, before anything else.** An `index_locked` refusal therefore has nothing to
//!    undo: no watcher was started, no port was bound, and the pid file belongs to the daemon that is
//!    still running, so this call must not remove it.
//! 2. **The reconciler next.** It is passive — it owns no timer and opens nothing until a pass — so
//!    constructing it starts nothing.
//! 3. **The server third**, so a bind failure still reaches the shutdown below with no watcher and no
//!    tick behind it, and releases the lock.
//! 4. **The loop last**, through [`crate::lifecycle::run_daemon`]. The loop adopts the lock this
//!    module holds and never releases it, which is why the release lives here, exactly once, after
//!    the shutdown. The loop calls [`crate::reconcile::Reconciler::tick`] once per interval: one
//!    scheduler for the whole process, never a second one inside the reconciler.
//!
//! On shutdown the order is the reverse — watchers, then the server, then the lock — so nobody takes
//! the home while this process still holds a watcher or a socket.
//!
//! What this module is not: it holds no MCP surface, opens no database, imports nothing from
//! `sangkar-tools`, and reads no environment variable. `home` arrives as an argument from the module
//! allowed to resolve it (`sangkar_core::config`, through [`crate::cli`]'s caller), and every
//! diagnostic goes to `sangkar_core::log` on stderr, because stdout is the protocol.
//!
//! # The report server is a seam, not an implementation
//!
//! `sangkar-telemetry`'s `serve` (`TASK-054`) owns the listener, and this module opens no socket of
//! its own. The composition needs three calls from it — start at the third step, one address, close
//! at the first shutdown step — so [`ReportServer`] states exactly those, and
//! [`ForegroundOptions::report`] is where a caller plugs its adapter in. The address it answers is
//! what the loop records in `DaemonOptions::report`, which is the only way `port: 0` is ever read
//! back.

use std::cell::RefCell;
use std::ffi::OsString;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use sangkar_core::errors::AppError;
use sangkar_core::global_config::{
    DEFAULT_REPORT_HOST, DEFAULT_REPORT_PORT, read_global_config, repo_defaults_of,
};
use sangkar_core::log;
use sangkar_core::pidlock::{PidLockHandle, acquire_pid_lock, release_pid_lock};
use serde_json::Value;
use tokio::sync::Notify;

use crate::lifecycle::{DaemonOptions, run_daemon};
use crate::reconcile::{
    OpenWatcher, RECONCILE_INTERVAL_MS, ReconcileReport, Reconciler, worker_watchers,
};
use crate::types::ReportAddress;

/// The pid file, spelled as `sangkar_core` and [`crate::lifecycle`] spell it.
const PID_FILE: &str = "daemon.pid";

/// The report server as the composition drives it: bind, answer the address, close.
///
/// `start` is called with the host and port the caller resolved (the `run` flags over the machine
/// file), so the policy — loopback only, `port: 0` picking a free port — stays with the server that
/// owns it. `close` must be safe on a server that never bound, because a failed `start` still runs
/// the shutdown.
pub trait ReportServer: Send {
    /// Bind the report and answer where it answers. Called once, at the composition's third step.
    ///
    /// # Errors
    ///
    /// The bind's own refusal — an address in use, a non-loopback host — as an [`AppError`], which
    /// the composition logs and turns into exit code 1 with the lock released.
    fn start(&mut self, host: &str, port: u16) -> Result<ReportAddress, AppError>;

    /// Close what [`Self::start`] bound. Called once, before the lock is released.
    fn close(&mut self);
}

/// The per-pass notice a caller (or a test) receives: called after every pass, once per tick.
pub type OnReconcile = Arc<dyn Fn(&ReconcileReport) + Send + Sync>;

/// Everything the foreground daemon needs. Only `home` is required; every other field is a seam or a
/// choice, so a test can drive the composition without a socket or a signal.
pub struct ForegroundOptions {
    /// The machine-level home: `daemon.pid`, `sangkar.json`, `daemon.state.json` and the log live in
    /// it.
    pub home: PathBuf,
    /// The index worker command line, program first — `AppConfig::worker_cmd`, which the watcher
    /// extends with `worker --repo-root … --data-dir …`. Not the command `start` spawns for
    /// `daemon run`: that one is [`DaemonOptions::command`], and the two would be different programs
    /// if one field carried both.
    pub worker_cmd: Vec<OsString>,
    /// The daemon loop's own period; `None` takes [`RECONCILE_INTERVAL_MS`], the reconciler's period.
    pub interval_ms: Option<u64>,
    /// The report server to bind, or `None` to serve none. `TASK-054`'s `serve` arrives here as an
    /// adapter; when the machine configuration enables no report, this is `None` and no port is bound.
    pub report: Option<Box<dyn ReportServer>>,
    /// The host and port `--port`/`--host` resolved to, defaulted from the machine file. Unused when
    /// [`Self::report`] is `None`.
    pub bind: BindAddress,
    /// A watcher factory a test substitutes; production builds one from [`Self::worker_cmd`].
    pub watchers: Option<OpenWatcher>,
    /// Called after each pass, so a test can read the reports without polling a clock.
    pub on_tick: Option<OnReconcile>,
    /// Test and caller seam: notifying this stops the loop the way `SIGINT` does.
    pub stop: Option<Arc<Notify>>,
}

/// Where the report server should bind: the `run` flags over the machine file, else the report
/// server's own built-in defaults.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindAddress {
    pub host: String,
    pub port: u16,
}

impl Default for BindAddress {
    /// `serve`'s built-in answer, which a machine file that names neither host nor port keeps.
    fn default() -> Self {
        Self {
            host: DEFAULT_REPORT_HOST.to_owned(),
            port: DEFAULT_REPORT_PORT,
        }
    }
}

impl ForegroundOptions {
    /// The foreground daemon with every seam empty; a caller fills what its verb needs.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            bind: BindAddress::default(),
            worker_cmd: Vec::new(),
            interval_ms: None,
            report: None,
            watchers: None,
            on_tick: None,
            stop: None,
        }
    }
}

/// Runs the whole daemon in the foreground and answers `0` when the loop stops cleanly.
///
/// The lock is taken here and released here, exactly once; the loop is handed the handle and never
/// releases it. Every failure path below leaves the home as it found it: no watcher running, no port
/// bound, no pid file this process wrote still on disk.
#[must_use]
pub fn run_daemon_foreground(options: ForegroundOptions) -> i32 {
    let ForegroundOptions {
        home,
        worker_cmd,
        interval_ms,
        mut report,
        bind,
        watchers,
        on_tick,
        stop,
    } = options;

    // First, and before the reconciler exists: a refused start must leave this call with nothing to
    // undo.
    let pid_path = home.join(PID_FILE);
    let lock = match acquire_pid_lock(&pid_path) {
        Ok(lock) => lock,
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
    };

    // Second: passive, so this opens nothing and a later failure has nothing to end.
    let open = watchers.unwrap_or_else(|| worker_watchers(worker_cmd));
    let defaults = repo_defaults_of(&read_global_config(&home));
    let reconciler = Rc::new(RefCell::new(Reconciler::new(&home, defaults, open)));

    // Third: the server. Its address is the only way `port: 0` is read back, and it is recorded in
    // `DaemonOptions::report` so `status` prints the port this process actually bound.
    let address = match report.as_mut() {
        None => None,
        Some(server) => match server.start(&bind.host, bind.port) {
            Ok(address) => {
                log::info(
                    "daemon.ready",
                    &[
                        ("home", Value::String(home.display().to_string())),
                        ("url", Value::String(address.url.clone())),
                        ("port", Value::from(address.port)),
                    ],
                );
                Some(address)
            }
            Err(error) => {
                // A bind failure is the composition's third step: the reconciler has opened nothing,
                // so the shutdown is the reverse order that never had a loop to run.
                log::error(
                    "daemon report server refused",
                    &[
                        ("home", Value::String(home.display().to_string())),
                        ("host", Value::String(bind.host.clone())),
                        ("port", Value::from(bind.port)),
                        ("message", Value::String(error.message)),
                    ],
                );
                reconciler.borrow_mut().stop();
                release(Some(&lock));
                return 1;
            }
        },
    };

    // Fourth, and last: the loop. One scheduler for the whole process, and one tick per interval;
    // the closure is the only place a pass is asked for.
    let tick_reconciler = Rc::clone(&reconciler);
    let tick = Box::new(move || {
        let passed = tick_reconciler.borrow_mut().tick();
        log::info(
            "daemon.tick",
            &[
                ("watched", Value::from(passed.watched.len())),
                ("missing", Value::from(passed.missing.len())),
            ],
        );
        if let Some(on_tick) = &on_tick {
            on_tick(&passed);
        }
        Ok(tick_reconciler.borrow().records())
    });
    let opts = DaemonOptions {
        interval_ms: Some(interval_ms.unwrap_or(RECONCILE_INTERVAL_MS)),
        tick: Some(tick),
        // The loop adopts the lock this module already holds and never releases it, which is why the
        // single release below is the composition's and not the loop's.
        lock: Some(lock),
        report: address.clone(),
        stop,
        ..DaemonOptions::new(home.clone())
    };

    let code = run_daemon(&opts);

    // The reverse order, unconditionally: stop opening work, close what is serving, and only then
    // drop the lock — so nobody can take the home while this process still holds a watcher or a
    // socket. A close that fails is logged and does not change the loop's answer: the loop is over
    // either way, and a shutdown path is not a crash.
    reconciler.borrow_mut().stop();
    if let Some(server) = report.as_mut() {
        server.close();
    }
    release(opts.lock.as_ref());
    code
}

/// Releases the lock this process acquired, with one error line on failure: the loop's answer is
/// already decided, and a pid file the filesystem refused to remove is a fact to report, not an
/// exception to raise.
fn release(lock: Option<&PidLockHandle>) {
    let Some(lock) = lock else {
        return;
    };
    if let Err(error) = release_pid_lock(lock) {
        log::error(
            "daemon shutdown failed",
            &[
                ("path", Value::String(lock.path().display().to_string())),
                ("message", Value::String(error.message)),
            ],
        );
    }
}
