//! The watcher: it decides *when* an index run is due and never runs one itself.
//!
//! Ported from `src/indexer/watch.ts`. Why a poll and not a native file-event API: `TECH-SPEC.md`
//! §8 picks a portable poll of `git HEAD` + `git status` because inotify and FSEvents differ per
//! platform, and because a `git checkout`, `stash` or `rebase` rewrites the tree through a path a
//! watch event may not describe usefully. One string per observation — the "signature" — and a
//! difference is the only reason to spawn.
//!
//! Why the worker is a child and never an import: the writer is one process (§8). A parser panic in
//! a grammar has to kill a child, not the MCP session, and the write lock is only meaningful if a
//! second process can contend for it. So this module reads the tree and spawns the worker; it never
//! opens `graph.db`, never takes the lock and never imports the writer module.
//!
//! What the signature contains, and why it is not the same everywhere: in a git checkout it is HEAD,
//! the porcelain status, and the mtime plus size of every parseable path that status names. The
//! status alone is not enough — it names *which* paths differ, never how much — so a second save to
//! a file that is already dirty leaves its line identical while the graph falls behind. Stamping
//! only the paths git names keeps the cost at one `stat` per dirty file: a bare `touch` of a clean
//! file contributes nothing, so it still is not a change. A directory git cannot read degrades to
//! the mtimes of the files the graph could contain, because that is the only signal left there
//! (`sangkar_domain::diff` degrades the same way, for the same reason).
//!
//! The child's only report is its exit code, and this module treats the four codes the worker
//! defines (`WORKER_EXIT` in `src/indexer/worker.ts`, spelled out here because importing it is
//! forbidden by the process split) as follows:
//! - `0` published / `2` nothing to do: the observed state is now the baseline, and any failure
//!   streak is over;
//! - `75` lock held: another indexer is running, which is normal, so it logs and retries on the next
//!   tick without counting a failure — a contended repo must not slow its own polling down;
//! - anything else: a failure. The baseline is *not* advanced, so the change is retried, and the
//!   polling delay backs off to `min(interval * 2^failures, 300 s)` so a run that dies on a broken
//!   tree cannot be respawned every two seconds forever.
//!
//! What differs from the TypeScript, and why: the loop is one thread with an interruptible
//! [`Condvar`] sleep instead of an event loop with `setTimeout`, a child is awaited by polling
//! `try_wait` instead of `await child.exited`, and the wall clock is a [`Clock`] the caller can
//! inject. Log lines carry their event key and no structured fields, because `serde_json` is not a
//! dependency of this crate; the key is what a log query groups by, so nothing about *which* events
//! happened is lost.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sangkar_core::log;
use sangkar_core::paths::normalize_rel;
use sangkar_domain::diff::sha256_hex;

use crate::discover::ALWAYS_EXCLUDED;
use crate::lang::registry::detect_language;

/// `TECH-SPEC.md` §8: poll every 2 s — cheap enough that a change is noticed while the editor saves.
pub const DEFAULT_INTERVAL_MS: u64 = 2_000;

/// `TECH-SPEC.md` §8: coalesce a save that writes the file more than once.
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;

/// The ceiling on the failure backoff, so a repo that cannot index still gets retried.
pub const MAX_BACKOFF_MS: u64 = 300_000;

/// One poll's git call must not outlive several intervals, or the next poll starts late.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a git wait and a worker wait sleep between `try_wait` checks. `waitpid` is a syscall
/// and both waits are dominated by the child's own runtime, so this only bounds the reaction time.
const POLL_SLICE: Duration = Duration::from_millis(5);

/// `WORKER_EXIT` in `src/indexer/worker.ts`, restated: 75 is `EX_TEMPFAIL`, "another indexer holds
/// the lock".
const EXIT_PUBLISHED: i32 = 0;
const EXIT_NOTHING_TO_DO: i32 = 2;
const EXIT_LOCKED: i32 = 75;

/// The exit code reported for a child that ended without one: killed by a signal, or a `wait` error.
/// The TypeScript starts at `-1` for the same reason, and the streak treats it as a failure.
const EXIT_UNKNOWN: i32 = -1;

/// The retry delay after `failures` consecutive failures: the poll interval doubled once per
/// failure, capped at [`MAX_BACKOFF_MS`]. Public because it is a rule a caller can check without
/// waiting five minutes for the cap.
#[must_use]
pub fn backoff_ms(interval_ms: u64, failures: u32) -> u64 {
    if failures == 0 {
        return 0;
    }
    // Doubling step by step rather than shifting: `2^40` would overflow in a caller's head and, with
    // `interval * 2^failures` spelled as a shift, in the arithmetic too. The cap wins either way.
    let mut delay = interval_ms;
    for _ in 0..failures {
        if delay >= MAX_BACKOFF_MS {
            return MAX_BACKOFF_MS;
        }
        delay = delay.saturating_mul(2);
    }
    delay.min(MAX_BACKOFF_MS)
}

/// `git` when the tree is a checkout, `mtime` when it is not (or git is unusable).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detector {
    Git,
    Mtime,
}

impl Detector {
    /// The lowercase spelling the TypeScript reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Mtime => "mtime",
        }
    }
}

/// What the child needs to know that the caller cannot read from its own working directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnTarget {
    /// The tree to index; also forwarded as `--repo-root`.
    pub repo_root: PathBuf,
    /// Where the graph, the lock and the data dir's `.gitignore` live; also forwarded as `--data-dir`.
    pub data_dir: PathBuf,
}

/// One spawn attempt, handed to the observer before the child's exit is awaited.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpawnNotice {
    /// [`Clock::now_ms`] at the spawn, so a caller can log an age without reading another clock.
    pub started_at: i64,
    /// Consecutive failures before this attempt, so a caller can tell a retry from a fresh run.
    pub failures: u32,
}

/// The minimum a spawned worker must offer. `std::process::Child` is wrapped rather than exposed so
/// a test can stand in a child that never runs a parser.
///
/// `try_wait` is a poll and `kill` is safe on a child that has already exited, which together let
/// `stop()` end a run without owning it.
pub trait WatchChild: Send {
    /// The worker's exit status once it has ended, `None` while it still runs.
    fn try_wait(&mut self) -> Option<i32>;

    /// Ends the worker. Safe on a worker that has already ended, and on one killed already.
    fn kill(&mut self);
}

/// The spawn seam. Tests pass a stub so no real index runs; the default runs the configured worker
/// command ([`worker_spawn`]).
///
/// A spawn that cannot start a process reports it through `Err`, which the loop treats as a failure:
/// a binary that is missing now may be built a minute later, so retrying it with the backoff is
/// right.
pub type SpawnWorker =
    Arc<dyn Fn(&SpawnTarget) -> Result<Box<dyn WatchChild>, String> + Send + Sync>;

/// Called once per spawn, before the child's exit is awaited.
pub type OnSpawn = Arc<dyn Fn(SpawnNotice) + Send + Sync>;

/// The loop's only source of wall-clock time, so a caller (or a test) can pin what a spawn reports.
pub trait Clock: Send + Sync {
    /// Milliseconds since the UNIX epoch — the unit a JavaScript date is an integer count of.
    fn now_ms(&self) -> i64;
}

/// The default [`Clock`]: the system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        epoch_millis()
    }
}

/// Everything [`start_watch`] needs. Built as a struct literal with `..WatchOptions::default()`,
/// because only the two roots are ever required.
#[derive(Clone)]
pub struct WatchOptions {
    /// The working tree whose changes are watched; made absolute once, in [`start_watch`].
    pub repo_root: PathBuf,
    /// Forwarded to the child, so the watcher and the worker can never target different graphs.
    pub data_dir: PathBuf,
    /// Poll period in milliseconds; defaults to [`DEFAULT_INTERVAL_MS`], floored at 1.
    pub interval_ms: Option<u64>,
    /// Quiet window in milliseconds after a change before spawning; defaults to
    /// [`DEFAULT_DEBOUNCE_MS`].
    pub debounce_ms: Option<u64>,
    /// Injected by tests; the default spawns this executable's worker mode.
    pub spawn: Option<SpawnWorker>,
    /// Called once per spawn, before the child's exit is awaited.
    pub on_spawn: Option<OnSpawn>,
    /// Injected by tests; the default reads the system clock.
    pub clock: Option<Arc<dyn Clock>>,
}

impl Default for WatchOptions {
    /// Both roots empty and every optional field unset. A watcher started from this observes
    /// nothing, which the loop reports as a poll failure rather than treating as "no changes".
    fn default() -> Self {
        Self {
            repo_root: PathBuf::new(),
            data_dir: PathBuf::new(),
            interval_ms: None,
            debounce_ms: None,
            spawn: None,
            on_spawn: None,
            clock: None,
        }
    }
}

/// A snapshot of the loop's memory, for `health.indexer` and for tests. Never the live object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WatchStats {
    /// Observations that produced a signature and were compared. Not the same as timer ticks.
    pub polls: u64,
    /// Spawn attempts, including the ones that exited 75 or 1.
    pub spawns: u64,
    /// Consecutive failures; any success or lock contention resets it.
    pub failures: u32,
    /// The delay currently in force, `0` on the healthy path.
    pub backoff_ms: u64,
    /// The last exit code observed, from the last child that ended.
    pub last_exit_code: Option<i32>,
    /// [`Clock::now_ms`] at the last spawn, `None` until one happened.
    pub last_spawn_at: Option<i64>,
    /// Which detector answered, so a degraded repo is visible instead of merely slower.
    pub detector: Option<Detector>,
}

/// The live watcher. Dropping it stops the loop, so a caller cannot leak a polling thread.
pub struct WatchHandle {
    shared: Arc<Shared>,
    interval_ms: u64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl WatchHandle {
    /// Stops the loop: clears the pending wait, kills a running child and joins the thread. Returns
    /// once the loop has ended, and is idempotent — a second call finds the thread already joined.
    pub fn stop(&self) {
        *lock(&self.shared.stopped) = true;
        self.shared.wake.notify_all();
        if let Some(child) = lock(&self.shared.child).as_mut() {
            child.kill();
        }
        let thread = lock(&self.thread).take();
        if let Some(thread) = thread {
            // A panic in the loop is the loop's own report; `stop()` promises to end, not to raise.
            let _ = thread.join();
        }
    }

    /// The loop's memory as of this call.
    #[must_use]
    pub fn stats(&self) -> WatchStats {
        let counters = *lock(&self.shared.counters);
        WatchStats {
            polls: counters.polls,
            spawns: counters.spawns,
            failures: counters.failures,
            backoff_ms: backoff_ms(self.interval_ms, counters.failures),
            last_exit_code: counters.last_exit_code,
            last_spawn_at: counters.last_spawn_at,
            detector: counters.detector,
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One observation of the tree: the comparable key plus what produced it.
struct Observed {
    key: String,
    detector: Detector,
}

/// The loop's counters, in one lock so a snapshot is a snapshot and not a torn read.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    polls: u64,
    spawns: u64,
    failures: u32,
    last_exit_code: Option<i32>,
    last_spawn_at: Option<i64>,
    detector: Option<Detector>,
}

/// What the loop thread and [`WatchHandle`] share: the stop signal, the counters and the child slot.
struct Shared {
    /// `true` once `stop()` was called. The only state a wait re-checks, which is why it has a
    /// `Condvar` beside it: `wait_timeout` re-acquires this same lock before returning.
    stopped: Mutex<bool>,
    wake: Condvar,
    counters: Mutex<Counters>,
    /// The child a poll is waiting on, so `stop()` can end it from another thread.
    child: Mutex<Option<Box<dyn WatchChild>>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            stopped: Mutex::new(false),
            wake: Condvar::new(),
            counters: Mutex::new(Counters::default()),
            child: Mutex::new(None),
        }
    }
}

/// What the thread body needs from [`WatchOptions`], resolved once so the loop reads no environment
/// and no option twice.
struct Plan {
    repo_root: PathBuf,
    data_dir: PathBuf,
    interval_ms: u64,
    debounce_ms: u64,
    spawn: SpawnWorker,
    on_spawn: Option<OnSpawn>,
    clock: Arc<dyn Clock>,
}

/// Starts the poll loop. The first poll only records the baseline — the watcher reports *changes*,
/// so a graph that is stale at startup is the caller's index run to make, not a spawn storm here.
#[must_use]
pub fn start_watch(options: &WatchOptions) -> WatchHandle {
    let interval_ms = options.interval_ms.unwrap_or(DEFAULT_INTERVAL_MS).max(1);
    let plan = Plan {
        repo_root: absolute(&options.repo_root),
        data_dir: absolute(&options.data_dir),
        interval_ms,
        debounce_ms: options.debounce_ms.unwrap_or(DEFAULT_DEBOUNCE_MS),
        spawn: options.spawn.clone().unwrap_or_else(default_spawn),
        on_spawn: options.on_spawn.clone(),
        clock: options
            .clock
            .clone()
            .unwrap_or_else(|| Arc::new(SystemClock)),
    };

    let shared = Arc::new(Shared::new());
    let looped = Arc::clone(&shared);
    let started = thread::Builder::new()
        .name("sangkar-watch".to_owned())
        .spawn(move || {
            let mut watch = Loop {
                shared: &looped,
                plan: &plan,
                baseline: None,
                last_note: "",
            };
            watch.pump();
        })
        .ok();
    if started.is_none() {
        // A watcher that cannot start a thread is a degradation the caller must see, and the handle
        // is still valid: `stats()` reports zeros and `stop()` is a no-op.
        log::error("watch.thread_failed", &[]);
    }

    WatchHandle {
        shared,
        interval_ms,
        thread: Mutex::new(started),
    }
}

/// The production spawn: `<worker_cmd> worker --repo-root <root> --data-dir <dir>`.
///
/// The roots are arguments, not variables: one visible command line, and no ambient setting a child
/// could inherit stale. The order is the CLI's parse order (see `sangkar_core::config`). stdout is
/// discarded because it carries the MCP protocol (§11), and stdin is closed so a child cannot block
/// on a terminal that is not there. stderr is inherited: under the daemon that file *is* the log.
#[must_use]
pub fn worker_spawn(worker_cmd: Vec<OsString>) -> SpawnWorker {
    Arc::new(move |target: &SpawnTarget| {
        let Some((program, arguments)) = worker_cmd.split_first() else {
            return Err("the worker command is empty".to_owned());
        };
        Command::new(program)
            .args(arguments)
            .arg("worker")
            .arg("--repo-root")
            .arg(&target.repo_root)
            .arg("--data-dir")
            .arg(&target.data_dir)
            .current_dir(&target.repo_root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .map(|child| Box::new(ProcessChild { child }) as Box<dyn WatchChild>)
            .map_err(|error| format!("cannot spawn the worker: {error}"))
    })
}

/// The spawn used when the caller injects none: this executable in its worker mode, which is the
/// default `AppConfig::worker_cmd` resolves to.
fn default_spawn() -> SpawnWorker {
    match std::env::current_exe() {
        Ok(exe) => worker_spawn(vec![exe.into_os_string()]),
        Err(error) => {
            let message = format!("cannot resolve the current executable: {error}");
            Arc::new(move |_: &SpawnTarget| Err(message.clone()))
        }
    }
}

/// A spawned worker process: the loop only ever learns its exit status.
struct ProcessChild {
    child: Child,
}

impl WatchChild for ProcessChild {
    fn try_wait(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(EXIT_UNKNOWN)),
            Ok(None) => None,
            // A `wait` the kernel refuses must not wedge the loop, and it says nothing about the
            // repository: report an unknown status, which the streak counts as a failure.
            Err(_error) => {
                log::warn("watch.child_wait_failed", &[]);
                Some(EXIT_UNKNOWN)
            }
        }
    }

    fn kill(&mut self) {
        // A child that has already exited makes this fail, which is exactly what `stop()` racing an
        // exit produces and what the TypeScript's `kill()` on a settled child does.
        let _ = self.child.kill();
    }
}

/// The poll loop. One thread, one observation at a time, never a second writer.
struct Loop<'a> {
    shared: &'a Shared,
    plan: &'a Plan,
    /// The last state a successful run covered, or the first observation when nothing has run yet.
    baseline: Option<String>,
    /// The last message [`Self::note`] logged, so a persistently degraded repo does not flood
    /// stderr with one line per tick.
    last_note: &'static str,
}

impl Loop<'_> {
    /// Sleeps, ticks, sleeps. Nothing is awaited after `stop()`: the loop's own sleep is its only
    /// wake-up, and `stop()` cuts it short.
    fn pump(&mut self) {
        loop {
            let failures = self.with_counters(|counters| counters.failures);
            let delay = if failures == 0 {
                self.plan.interval_ms
            } else {
                backoff_ms(self.plan.interval_ms, failures)
            };
            if !self.sleep(delay) {
                return;
            }
            // An unreadable tree is a degradation, not a worker failure: log it and keep polling.
            if self.tick().is_err() {
                self.note("watch.poll_failed");
            }
        }
    }

    /// One poll: observe, compare with the baseline, debounce, re-check once, then maybe spawn.
    fn tick(&mut self) -> Result<(), String> {
        if self.child_running() {
            // One writer at a time. The running child reads the tree itself, and the next tick
            // re-reads the signature, so nothing is missed by declining to spawn here.
            return Ok(());
        }

        let seen = observe(&self.plan.repo_root, &self.plan.data_dir)?;
        if self.stopped() {
            return Ok(());
        }
        // Counted only once the observation exists: a caller that waits for `polls` is then also
        // waiting for the baseline, which is what makes "the first poll is the baseline" observable.
        self.with_counters(|counters| {
            counters.polls += 1;
            counters.detector = Some(seen.detector);
        });
        if self.baseline.is_none() {
            // The first poll only records the baseline; the watcher reports *changes*.
            self.baseline = Some(seen.key);
            return Ok(());
        }
        if self.baseline.as_deref() == Some(seen.key.as_str()) {
            return Ok(());
        }

        // Debounce: a save often writes the file several times. Re-check *once* after the quiet
        // window rather than restarting it on every write — a change that lands during the wait
        // still differs from the baseline, so it is covered by the single spawn that follows, and
        // anything newer than that shows up on the next tick.
        if !self.sleep(self.plan.debounce_ms) {
            return Ok(());
        }
        if self.child_running() {
            return Ok(());
        }
        let confirmed = observe(&self.plan.repo_root, &self.plan.data_dir)?;
        if self.stopped() {
            return Ok(());
        }
        self.with_counters(|counters| counters.detector = Some(confirmed.detector));
        if self.baseline.as_deref() == Some(confirmed.key.as_str()) {
            // The tree settled back to what the graph already has (a save that was undone).
            return Ok(());
        }
        self.run(&confirmed.key);
        Ok(())
    }

    /// Runs one worker, then folds its exit code into the baseline, the streak and the delay.
    fn run(&mut self, key: &str) {
        let started_at = self.plan.clock.now_ms();
        let failures = self.with_counters(|counters| {
            counters.spawns += 1;
            counters.last_spawn_at = Some(started_at);
            counters.failures
        });
        if let Some(on_spawn) = &self.plan.on_spawn {
            on_spawn(SpawnNotice {
                started_at,
                failures,
            });
        }

        let target = SpawnTarget {
            repo_root: self.plan.repo_root.clone(),
            data_dir: self.plan.data_dir.clone(),
        };
        let child = match (self.plan.spawn)(&target) {
            Ok(child) => child,
            // A binary that cannot be spawned is a failure like any other, and retrying it after the
            // backoff is right: the artifact may exist a minute from now.
            Err(_message) => {
                self.with_counters(|counters| counters.failures += 1);
                self.note("watch.spawn_failed");
                return;
            }
        };
        *lock(&self.shared.child) = Some(child);

        let code = self.wait_child();
        self.with_counters(|counters| counters.last_exit_code = Some(code));

        // A child killed by `stop()` says nothing about the repository, so it changes no state.
        if self.stopped() {
            return;
        }

        if code == EXIT_LOCKED {
            log::info("watch.worker_locked", &[]);
            return;
        }

        if code == EXIT_PUBLISHED || code == EXIT_NOTHING_TO_DO {
            if failures > 0 {
                log::info("watch.worker_recovered", &[]);
            }
            self.with_counters(|counters| counters.failures = 0);
            self.last_note = "";
            // The state this run saw is now covered; anything newer differs from the new baseline.
            self.baseline = Some(key.to_owned());
            log::debug("watch.worker_done", &[]);
            return;
        }

        self.with_counters(|counters| counters.failures += 1);
        log::warn("watch.worker_failed", &[]);
        // The baseline stays put: the change that failed must be retried after the backoff.
    }

    /// Waits for the child in the slot and clears it. `EXIT_UNKNOWN` when the loop is stopping.
    ///
    /// Polling `try_wait` rather than blocking in `wait()` is what keeps `stop()` able to kill the
    /// child and join the thread: the lock is held for one syscall at a time, never for the run.
    fn wait_child(&mut self) -> i32 {
        loop {
            {
                let mut slot = lock(&self.shared.child);
                match slot.as_mut() {
                    None => return EXIT_UNKNOWN,
                    Some(child) => {
                        if let Some(code) = child.try_wait() {
                            *slot = None;
                            return code;
                        }
                    }
                }
            }
            if self.stopped() {
                // `stop()` may have raced the spawn, so end the worker here too: a child nobody waits
                // for would outlive the watcher, and `stop()` has already returned by the time the
                // slot is cleared.
                if let Some(child) = lock(&self.shared.child).as_mut() {
                    child.kill();
                }
            }
            thread::sleep(POLL_SLICE);
        }
    }

    /// Sleeps for `ms`, returning `false` when `stop()` cut it short.
    ///
    /// A `Condvar` rather than `thread::sleep`, because a pending poll must not be the reason a
    /// process cannot exit: `stop()` sets the flag under the same lock and notifies, so the wait
    /// ends there rather than after the interval. One slot is enough — the debounce wait and the
    /// poll wait never overlap.
    fn sleep(&self, ms: u64) -> bool {
        let stopped = lock(&self.shared.stopped);
        let (stopped, _) = self
            .shared
            .wake
            .wait_timeout_while(stopped, Duration::from_millis(ms), |stopped| !*stopped)
            .unwrap_or_else(PoisonError::into_inner);
        !*stopped
    }

    fn stopped(&self) -> bool {
        *lock(&self.shared.stopped)
    }

    fn child_running(&self) -> bool {
        lock(&self.shared.child).is_some()
    }

    /// Runs `edit` against the counters under the lock, so a read and a write cannot be torn.
    fn with_counters<T>(&self, edit: impl FnOnce(&mut Counters) -> T) -> T {
        edit(&mut lock(&self.shared.counters))
    }

    /// Logs one message per distinct note, so a persistently degraded repo does not flood stderr.
    fn note(&mut self, msg: &'static str) {
        if self.last_note == msg {
            return;
        }
        self.last_note = msg;
        log::warn(msg, &[]);
    }
}

/// One poll's observation. The mtime path reports an error when the tree cannot be read at all, and
/// the loop treats that as a degradation rather than a failure (see [`Loop::pump`]).
fn observe(repo_root: &Path, data_dir: &Path) -> Result<Observed, String> {
    if let Some(git) = read_git_signature(repo_root, data_dir) {
        return Ok(git);
    }
    Ok(Observed {
        key: read_mtime_signature(repo_root, data_dir)?,
        detector: Detector::Mtime,
    })
}

/// One bounded git read: the stdout when git answered, `None` when it could not.
///
/// The flags mirror `sangkar_domain::diff`: no optional index lock (this module must not write), no
/// credential helper, no pager, and a closed stdin so a hook cannot prompt. A missing binary, a
/// missing repository or a timeout is `None`, never an error: "git is unusable here" is a
/// degradation that falls back to mtimes, and [`WatchStats::detector`] is where a caller sees it.
fn git_run(repo_root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "credential.helper=",
            "-c",
            "core.askpass=",
            "-C",
        ])
        .arg(repo_root)
        .args(args)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    // Both pipes are drained on their own threads: `status --porcelain` on a large tree outruns the
    // pipe buffer, and a child blocked on a full pipe never exits. stderr is drained and dropped —
    // the outcome is the exit code — because a log line here cannot carry a field for the reason.
    let stdout = child
        .stdout
        .take()
        .map(|pipe| thread::spawn(move || read_all(pipe)));
    let _ = child
        .stderr
        .take()
        .map(|pipe| thread::spawn(move || read_all(pipe)));

    let deadline = Instant::now() + GIT_TIMEOUT;
    let waited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_error) => break None,
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(POLL_SLICE);
    };

    let status = match waited {
        Some(status) => status,
        None => {
            // The readers are abandoned rather than joined: a killed git can leave a grandchild
            // holding the pipes open, which would make "wait for end of file" unbounded.
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
    };
    let stdout = stdout.map(join_reader).unwrap_or_default();
    status.success().then_some(stdout)
}

/// Reads a pipe to end of file as text. A read error is reported as empty output: the exit code and
/// stderr already say what went wrong.
fn read_all(mut pipe: impl Read) -> String {
    let mut buffer = Vec::new();
    let _ = pipe.read_to_end(&mut buffer);
    String::from_utf8_lossy(&buffer).into_owned()
}

fn join_reader(handle: JoinHandle<String>) -> String {
    handle.join().unwrap_or_default()
}

/// HEAD, the porcelain status, and a stamp of the paths that status names, or `None` when this
/// directory is not a checkout git can read.
///
/// `rev-parse` answers two questions in one process: a failure means "not a repository", and its
/// second line is the sub-root prefix `status` needs, since `HEAD` is a separate call on purpose —
/// `rev-parse HEAD` fails on a branch with no commits yet, and that is a real repository whose
/// working tree still deserves watching.
///
/// `-z` and `-uall` are what make the status usable as an input rather than a display:
/// NUL-separated records mean no quoted path has to be decoded, and naming every untracked *file*
/// (instead of collapsing an untracked directory to one `?? dir/` line) is the same blind spot one
/// level down — a second edit inside a new directory would otherwise leave the signature unchanged.
fn read_git_signature(repo_root: &Path, data_dir: &Path) -> Option<Observed> {
    let check = git_run(repo_root, &["rev-parse", "--git-dir", "--show-prefix"])?;
    // Empty at the toplevel, `sub/` when the watched root is a subdirectory; always with its slash.
    let prefix = check.split('\n').nth(1).unwrap_or("").trim();

    let head = git_run(repo_root, &["rev-parse", "HEAD"]);
    let status = git_run(repo_root, &["status", "--porcelain", "-z", "-uall"])?;

    let stamp = dirty_stamp(repo_root, data_dir, &git_status_paths(&status), prefix);
    // An unborn HEAD contributes the empty string — the second call answers `None` — which is still
    // a stable key component, and that repository's working tree still deserves watching.
    Some(Observed {
        key: format!(
            "git\n{}\n{status}\n{stamp}",
            head.as_deref().unwrap_or("").trim()
        ),
        detector: Detector::Git,
    })
}

/// The paths `git status --porcelain -z` reports. A rename or copy is two records — the new path
/// with its status, then the original with no prefix — so the original is consumed here: it is
/// either gone (a rename) or unchanged (a copy), and stamping it would describe nothing.
fn git_status_paths(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut records = stdout.split('\0');
    while let Some(record) = records.next() {
        // Two status columns and the separating space precede the path, and the empty record at the
        // end of the stream has no path at all.
        if record.len() < 3 {
            continue;
        }
        if let Some(path) = record.get(3..) {
            paths.push(path.to_owned());
        }
        let code = record.as_bytes().first().copied();
        if code == Some(b'R') || code == Some(b'C') {
            records.next();
        }
    }
    paths
}

/// The mtime and size of every path git just named that the graph could contain. This is the half
/// of the signature that answers "did the bytes move", and it stays cheap because git did the
/// selecting: one `stat` per dirty or untracked path, none for the rest of the tree. `prefix` is the
/// sub-root `status` reports under, so a path outside it belongs to the rest of the checkout and is
/// not this graph's tree. Paths the data dir owns are pruned here as they are in
/// [`read_mtime_signature`], so the stamp itself can never describe a worker's own write; the status
/// text beside it is git's own view and is not pruned, which stays sound only because the worker
/// writes `<data_dir>/.gitignore` and git then leaves the data dir out of that text entirely.
fn dirty_stamp(repo_root: &Path, data_dir: &Path, paths: &[String], prefix: &str) -> String {
    let skip_dir = is_inside(repo_root, data_dir).then_some(data_dir);
    let mut rows = Vec::new();
    for reported in paths {
        let Some(rest) = reported.strip_prefix(prefix) else {
            continue;
        };
        let absolute = repo_root.join(rest);
        if skip_dir.is_some_and(|skip| strict_within(skip, &absolute)) {
            continue;
        }
        let rel = normalize_rel(&relative_to(repo_root, &absolute));
        if detect_language(&rel).is_none() {
            // Not a file any shipped grammar could parse, so its bytes cannot change the graph.
            continue;
        }
        // Gone between the status and the stat: the next poll sees the settled tree, and the status
        // text still carries the deletion itself.
        let Ok(metadata) = fs::metadata(&absolute) else {
            continue;
        };
        if !metadata.is_file() {
            // A directory or a device file cannot change the graph's bytes.
            continue;
        }
        rows.push(format!(
            "{rel}\0{}\0{}",
            mtime_millis(&metadata),
            metadata.len()
        ));
    }
    rows.sort();
    rows.join("\n")
}

/// The mtime-only signature: a digest over the path, mtime and size of every file the graph could
/// contain. It is deliberately not a hash of contents — that is the worker's job, and re-reading
/// every file on every poll would cost more than the run it would save.
fn read_mtime_signature(repo_root: &Path, data_dir: &Path) -> Result<String, String> {
    // A data dir inside the tree would otherwise look like a source change on every run: the worker
    // writes `graph.db` there, so the watcher would spawn forever. Outside the tree there is
    // nothing to prune.
    let skip_dir = is_inside(repo_root, data_dir).then_some(data_dir);
    let mut rows: Vec<String> = Vec::new();
    let mut pending: Vec<PathBuf> = vec![repo_root.to_path_buf()];

    while let Some(dir) = pending.pop() {
        let reader = fs::read_dir(&dir)
            .map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let mut entries: Vec<(OsString, fs::FileType)> = Vec::new();
        for entry in reader {
            let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
            let file_type = entry
                .file_type()
                .map_err(|error| format!("cannot stat {}: {error}", entry.path().display()))?;
            entries.push((entry.file_name(), file_type));
        }
        // Sorted so the digest is a function of the tree, not of `readdir` order.
        entries.sort_by(|left, right| left.0.cmp(&right.0));

        for (name, file_type) in entries {
            let absolute = dir.join(&name);
            // A symlink can point outside the tree or form a cycle; discovery skips them for the
            // same reason, and the signature must describe the same file set the graph can.
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                let excluded = name
                    .to_str()
                    .is_some_and(|name| ALWAYS_EXCLUDED.contains(&name));
                if excluded || skip_dir.is_some_and(|skip| skip == absolute.as_path()) {
                    continue;
                }
                pending.push(absolute);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let rel = normalize_rel(&relative_to(repo_root, &absolute));
            if detect_language(&rel).is_none() {
                // Not a file any shipped grammar could parse, so its mtime cannot change the graph.
                continue;
            }
            // Vanished between the readdir and the stat: the next poll sees the settled tree.
            let Ok(metadata) = fs::metadata(&absolute) else {
                continue;
            };
            rows.push(format!(
                "{rel}\0{}\0{}",
                mtime_millis(&metadata),
                metadata.len()
            ));
        }
    }

    rows.sort();
    let digest = sha256_hex(rows.join("\n").as_bytes());
    Ok(format!("mtime\n{}\n{digest}", rows.len()))
}

/// `true` when `child` is strictly inside `parent`; the data dir must be pruned only when inside.
fn is_inside(parent: &Path, child: &Path) -> bool {
    match child.strip_prefix(parent) {
        Ok(rest) => !rest.as_os_str().is_empty(),
        Err(_) => false,
    }
}

/// `child == prune` or `child` is under `prune`: the one test the stamp pruning needs.
fn strict_within(prune: &Path, child: &Path) -> bool {
    child == prune || is_inside(prune, child)
}

/// `path` as the root-relative form [`normalize_rel`] takes, or the path itself when it escapes.
fn relative_to(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rest) => rest.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

/// `Math.floor(mtimeMs)`: milliseconds since the epoch, floored, and negative before 1970.
fn mtime_millis(metadata: &fs::Metadata) -> i64 {
    match metadata.modified() {
        Ok(time) => match time.duration_since(UNIX_EPOCH) {
            Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
            Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
        },
        // A filesystem that reports no modification time still has a stable size, and a `0` stamp
        // keeps the row comparable instead of dropping the path.
        Err(_) => 0,
    }
}

/// `Date.now()` for the default clock; the epoch on a host clock set before 1970, as `log` does.
fn epoch_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// `path.resolve(p)` without touching the filesystem: `.` is dropped and a relative path is rooted at
/// the working directory. `..` is kept, because resolving it would describe a different directory
/// when a symlink is on the way and the only caller that cares passes absolute roots anyway.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// A mutex lock whose poison is not an error: a panic in one thread must not wedge the loop, and
/// every field under these locks is a plain counter, a flag or an owned handle.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::{Detector, MAX_BACKOFF_MS, backoff_ms, git_status_paths, is_inside, strict_within};
    use std::path::Path;

    /// `TECH-SPEC.md` §8's rule, checked without waiting five minutes for the cap: the interval
    /// doubled once per failure, and the cap once doubling would pass it.
    #[test]
    fn backoff_doubles_from_the_interval_to_the_cap() {
        let cases: [(u32, u64); 9] = [
            (0, 0),
            (1, 4_000),
            (2, 8_000),
            (3, 16_000),
            (4, 32_000),
            (5, 64_000),
            (6, 128_000),
            (9, MAX_BACKOFF_MS),
            (20, MAX_BACKOFF_MS),
        ];
        for (failures, expected) in cases {
            assert_eq!(
                backoff_ms(2_000, failures),
                expected,
                "backoff after {failures} failures"
            );
        }
        // An interval already at or past the cap cannot push past it.
        assert_eq!(backoff_ms(MAX_BACKOFF_MS, 1), MAX_BACKOFF_MS);
        // The doubling is saturating rather than shifting, so a huge streak stays finite.
        assert_eq!(backoff_ms(1, u32::MAX), MAX_BACKOFF_MS);
    }

    /// The detector's spellings are the ones a log query and `health.indexer` expect.
    #[test]
    fn detector_spellings_are_the_typescript_strings() {
        assert_eq!(Detector::Git.as_str(), "git");
        assert_eq!(Detector::Mtime.as_str(), "mtime");
    }

    /// `git status --porcelain -z` records: an ordinary change, an untracked file, and a rename,
    /// whose original path is a second record with no status of its own.
    #[test]
    fn porcelain_records_name_their_paths_and_skip_a_rename_original() {
        let stdout = " M app.py\u{0}?? pkg/one.py\u{0}R  new.py\u{0}old.py\u{0}";
        assert_eq!(
            git_status_paths(stdout),
            vec![
                "app.py".to_owned(),
                "pkg/one.py".to_owned(),
                "new.py".to_owned()
            ]
        );
        // A copy names a source that still exists, so it is skipped for the same reason.
        assert_eq!(
            git_status_paths("C  copy.py\u{0}origin.py\u{0}"),
            vec!["copy.py".to_owned()]
        );
        // A clean tree is one empty record, and an empty record has no path.
        assert!(git_status_paths("\u{0}").is_empty());
    }

    /// The data dir is pruned only when it is strictly inside the tree; a sibling whose name starts
    /// with the tree's name is not inside it.
    #[test]
    fn pruning_compares_path_components_not_string_prefixes() {
        let root = Path::new("/tmp/tree");
        assert!(is_inside(root, Path::new("/tmp/tree/.sangkar")));
        assert!(!is_inside(root, root));
        assert!(!is_inside(root, Path::new("/tmp/tree-old/.sangkar")));
        assert!(!is_inside(root, Path::new("/tmp/other")));
        assert!(strict_within(
            Path::new("/tmp/tree/.sangkar"),
            Path::new("/tmp/tree/.sangkar")
        ));
        assert!(strict_within(
            Path::new("/tmp/tree/.sangkar"),
            Path::new("/tmp/tree/.sangkar/graph.db")
        ));
        assert!(!strict_within(
            Path::new("/tmp/tree/.sangkar"),
            Path::new("/tmp/tree/app.py")
        ));
    }
}
