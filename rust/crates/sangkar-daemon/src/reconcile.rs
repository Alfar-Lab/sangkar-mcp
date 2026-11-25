//! The reconciler: one pass over the registry that makes the live watchers match it.
//!
//! Ported from `src/daemon/reconcile.ts`. Why passive: [`crate::lifecycle`] already has one loop for
//! the whole process, so a second timer here would double the poll rate and give a stop two things to
//! race against. [`Reconciler::tick`] is one full reconciliation and [`Reconciler::stop`] ends every
//! watcher; [`RECONCILE_INTERVAL_MS`] is the period the daemon's own loop consumes, exported so the
//! two cannot drift apart.
//!
//! Why the watchers are keyed by root: the identity of a watcher is the tree it watches, so a second
//! entry for one root would mean two children indexing one graph. Keeping the interval beside the
//! handle is what makes a retune a comparison instead of a re-read: a registered repository nobody
//! touched keeps the handle it already has, so its baseline and its failure streak survive the tick.
//!
//! The decision ladder, per registered root, in order:
//! - the directory is gone: report it `missing`, stop any watcher, log one line per transition — a
//!   tree that stays deleted must not write a warning every five seconds;
//! - `watch: false`: report it `skipped`, stop any watcher, same one-line-per-transition rule;
//! - `watch: true`: exactly one watcher, started for a new root or retuned (stop, then start) when
//!   the interval changed, and left alone otherwise;
//! - no longer in `sangkar.json`: stopped and forgotten, because an unregistered tree is not ours.
//!
//! What this module never does: spawn a worker or read a repository's graph. The child, the backoff
//! and the exit-code ladder live in [`sangkar_index::watch`], and the data dir is
//! `<root>/.sangkar` so the watcher and the worker always name the same graph.
//!
//! # What differs from the TypeScript, and why
//!
//! The watcher is `sangkar-index`'s [`WatchHandle`], driven through [`Watcher`] so a test can stand in
//! a recorder and count opens, stops and stats without a thread or a child. The factory that builds
//! the real one is [`worker_watchers`], which wraps `sangkar-index`'s own spawn seam — so production
//! and the daemon's integration test go through the same worker entry point.
//!
//! `tick` is synchronous and takes `&mut self`: the loop that owns it is one thread, so the
//! TypeScript's promise queue (two overlapping passes would both see "no watcher for root" and open
//! two) has nothing to serialise. A stop is likewise a `&mut self` call, and a later `tick` reports
//! nothing and creates nothing.
//!
//! [`Watcher::stop`] cannot fail in this port, so the TypeScript's `watcher.stop_failed` line has no
//! counterpart: `WatchHandle::stop` already swallows the polling thread's panic and only promises to
//! end.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::config::{DATA_DIR_NAME, RepoDefaults};
use sangkar_core::log;
use sangkar_core::registry::read_registry;
use sangkar_core::repo_config::{WATCH_INTERVAL_MAX_MS, WATCH_INTERVAL_MIN_MS, read_repo_config};
use sangkar_index::watch::{
    SpawnWorker, WatchHandle, WatchOptions, WatchStats, start_watch, worker_spawn,
};
use serde_json::Value;

use crate::types::RepoRecord;

/// The daemon's reconciliation period, consumed by the daemon's loop. This module has no timer.
pub const RECONCILE_INTERVAL_MS: u64 = 5_000;

/// The registry `sangkar.json` beside the pid file, spelled as `sangkar_core` spells it.
const REGISTRY_FILE: &str = "sangkar.json";

/// What one [`Reconciler::tick`] changed. Every list is in registry order, so two passes compare by
/// equality.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Roots with a live watcher after this pass, which is the invariant the daemon relies on.
    pub watched: Vec<String>,
    /// Watchers created by this pass, including the second half of a retune.
    pub started: Vec<String>,
    /// Watchers ended by this pass: retunes, opt-outs, vanished roots and de-registrations.
    pub stopped: Vec<String>,
    /// Registered roots whose directory is gone, so `status` can report them instead of watching.
    pub missing: Vec<String>,
    /// Registered roots deliberately not watched: `watch: false`, or a watcher that would not open.
    pub skipped: Vec<String>,
}

/// The subset of a running watcher this module drives, so a test can substitute a recorder.
///
/// `sangkar-index`'s [`WatchHandle`] is the only production implementation; both calls take `&self`
/// because a handle is shared state, not a value.
pub trait Watcher: Send {
    /// Ends the loop, kills a running child and joins the thread. Idempotent.
    fn stop(&self);

    /// The watcher's own counters as of this call, never the live object.
    fn stats(&self) -> WatchStats;
}

impl Watcher for WatchHandle {
    fn stop(&self) {
        WatchHandle::stop(self);
    }

    fn stats(&self) -> WatchStats {
        WatchHandle::stats(self)
    }
}

/// What the factory needs to open one watcher: the two roots and the resolved poll period.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatcherOptions {
    /// The working tree to watch, in the registry's spelling.
    pub repo_root: PathBuf,
    /// `<repo_root>/.sangkar`, so the watcher and the worker target one graph.
    pub data_dir: PathBuf,
    /// The interval this repository resolved to, already clamped to the contract's range.
    pub interval_ms: u64,
}

/// The watcher factory seam: `sangkar-index`'s watcher in production ([`worker_watchers`]), a
/// recorder in a test. `Err` is a watcher that could not open, which the pass reports as `skipped`
/// and does not let abort the other repositories.
pub type OpenWatcher =
    Arc<dyn Fn(&WatcherOptions) -> Result<Box<dyn Watcher>, String> + Send + Sync>;

/// The production factory: `sangkar-index`'s poll loop, spawning `worker_cmd` for an index run.
///
/// `worker_cmd` is the command line the CLI resolves as `AppConfig::worker_cmd` (the `sangkar`
/// executable by default), and `worker_spawn` appends `worker --repo-root … --data-dir …` — so the
/// daemon and the `index` tool drive the same worker entry point, and a test can substitute a
/// script for it.
#[must_use]
pub fn worker_watchers(worker_cmd: Vec<OsString>) -> OpenWatcher {
    let spawn: SpawnWorker = worker_spawn(worker_cmd);
    Arc::new(move |options: &WatcherOptions| {
        Ok(Box::new(start_watch(&WatchOptions {
            repo_root: options.repo_root.clone(),
            data_dir: options.data_dir.clone(),
            interval_ms: Some(options.interval_ms),
            spawn: Some(Arc::clone(&spawn)),
            ..WatchOptions::default()
        })) as Box<dyn Watcher>)
    })
}

/// One live watcher and the interval it was opened with, which is what a retune compares.
struct Entry {
    handle: Box<dyn Watcher>,
    interval_ms: u64,
}

/// The reconciler's whole memory. Built once per daemon start: the machine-level settings describe
/// the installation, so a change to them is a restart's business, while each repository's own file
/// is re-read on every pass.
pub struct Reconciler {
    /// `<home>/sangkar.json`, read once per pass.
    registry_path: PathBuf,
    /// What a repository inherits before its own `<dataDir>/config.json` speaks.
    repo_defaults: RepoDefaults,
    open: OpenWatcher,
    /// Live watchers in the order they were opened — the TypeScript map's iteration order — so
    /// `watched` is the registry's order rather than a sort of it.
    live: Vec<(String, Entry)>,
    /// Roots whose *current* refusal has already been logged, so a steady state stays silent.
    refused: HashSet<String>,
    /// Every root the last pass covered, in registry order: the rows a later `status` reads.
    seen: Vec<String>,
    /// ISO-8601 of the last completed pass, stamped on every row [`Self::records`] answers.
    last_pass: Option<String>,
    /// `true` once [`Self::stop`] ran; a later pass reports nothing and creates nothing.
    stopped: bool,
}

impl Reconciler {
    /// A reconciler that has opened nothing. `home` is the installation's home, and `defaults` the
    /// machine values a repository inherits.
    #[must_use]
    pub fn new(home: &Path, repo_defaults: RepoDefaults, open: OpenWatcher) -> Self {
        Self {
            registry_path: home.join(REGISTRY_FILE),
            repo_defaults,
            open,
            live: Vec::new(),
            refused: HashSet::new(),
            seen: Vec::new(),
            last_pass: None,
            stopped: false,
        }
    }

    /// One reconciliation: the registry against the live watchers, root by root.
    ///
    /// A pass is atomic with respect to [`Self::stop`]: both take `&mut self`, and the daemon's loop
    /// is one thread, so a stop cannot land between a decision and the open that follows it — the
    /// reason the TypeScript needs a queue and a mid-tick flag, and this port needs neither.
    pub fn tick(&mut self) -> ReconcileReport {
        let mut report = ReconcileReport::default();
        if self.stopped {
            return report;
        }
        let at = iso8601_millis(now_millis());
        let registry = read_registry(&self.registry_path);
        let mut seen: Vec<String> = Vec::with_capacity(registry.repos.len());

        for path in &registry.repos {
            let root = path.display().to_string();
            seen.push(root.clone());
            let data_dir = path.join(DATA_DIR_NAME);

            if !path.is_dir() {
                report.missing.push(root.clone());
                self.note_once(&root, "reconcile.repo_missing", &[("at", text(&at))]);
                self.end(&root, &mut report.stopped);
                continue;
            }

            // The repository's own file is re-read every pass, which is what makes a `watch: false`
            // or a retune visible without a restart.
            let config = read_repo_config(path, Some(&data_dir), self.repo_defaults);
            if !config.watch {
                report.skipped.push(root.clone());
                self.note_once(
                    &root,
                    "reconcile.watch_off",
                    &[
                        ("at", text(&at)),
                        ("watchIntervalMs", Value::from(config.watch_interval_ms)),
                    ],
                );
                self.end(&root, &mut report.stopped);
                continue;
            }

            self.refused.remove(&root);
            let interval_ms = config
                .watch_interval_ms
                .clamp(WATCH_INTERVAL_MIN_MS, WATCH_INTERVAL_MAX_MS);
            if let Some(index) = self.index_of(&root) {
                let opened_with = self.live[index].1.interval_ms;
                if opened_with == interval_ms {
                    // Unchanged: the running watcher keeps its baseline and its failure streak.
                    continue;
                }
                log::info(
                    "reconcile.watcher_retuned",
                    &[
                        ("repoRoot", text(&root)),
                        ("fromMs", Value::from(opened_with)),
                        ("toMs", Value::from(interval_ms)),
                    ],
                );
                self.end(&root, &mut report.stopped);
            }

            match (self.open)(&WatcherOptions {
                repo_root: path.clone(),
                data_dir,
                interval_ms,
            }) {
                Ok(handle) => {
                    self.live.push((
                        root.clone(),
                        Entry {
                            handle,
                            interval_ms,
                        },
                    ));
                    report.started.push(root);
                }
                Err(message) => {
                    // One tree that cannot be watched is not a reason to stop serving the others.
                    report.skipped.push(root.clone());
                    log::error(
                        "reconcile.open_failed",
                        &[("repoRoot", text(&root)), ("message", text(&message))],
                    );
                }
            }
        }

        // De-registered roots: stopped and forgotten, in the order they were opened.
        let dropped: Vec<String> = self
            .live
            .iter()
            .map(|(root, _)| root.clone())
            .filter(|root| !seen.contains(root))
            .collect();
        for root in dropped {
            log::info(
                "reconcile.unregistered",
                &[("repoRoot", text(&root)), ("at", text(&at))],
            );
            self.end(&root, &mut report.stopped);
        }

        report.watched = self.live.iter().map(|(root, _)| root.clone()).collect();
        self.seen = seen;
        self.last_pass = Some(at);
        report
    }

    /// Ends every watcher and clears the refusal memory. A later [`Self::tick`] answers an empty
    /// report and opens nothing.
    pub fn stop(&mut self) {
        self.stopped = true;
        let live = std::mem::take(&mut self.live);
        self.refused.clear();
        self.seen.clear();
        for (_, entry) in live {
            entry.handle.stop();
        }
    }

    /// The rows a `status` reads after this pass: one per root the pass covered, in registry order.
    ///
    /// `last_run` is the pass's own timestamp — that is what the field means — and `last_exit` is the
    /// last index run the watcher reported, absent for a root with no live watcher (`watch: false`, a
    /// vanished tree, one that would not open). `generation` is left absent on purpose: it lives in
    /// the published graph's `meta`, and the daemon does not open `graph.db` — the process split keeps
    /// the writer in one process, and a per-pass read of every registered graph would contend with it.
    #[must_use]
    pub fn records(&self) -> Vec<RepoRecord> {
        let Some(at) = self.last_pass.clone() else {
            return Vec::new();
        };
        self.seen
            .iter()
            .map(|root| RepoRecord {
                root: root.clone(),
                last_run: Some(at.clone()),
                last_exit: self
                    .index_of(root)
                    .and_then(|index| self.live[index].1.handle.stats().last_exit_code),
                generation: None,
            })
            .collect()
    }

    /// The position of `root`'s watcher, or `None` when it has none.
    fn index_of(&self, root: &str) -> Option<usize> {
        self.live.iter().position(|(key, _)| key == root)
    }

    /// Ends `root`'s watcher, if it has one, and records the stop.
    fn end(&mut self, root: &str, stopped: &mut Vec<String>) {
        let Some(index) = self.index_of(root) else {
            return;
        };
        let (_, entry) = self.live.remove(index);
        stopped.push(root.to_owned());
        entry.handle.stop();
    }

    /// Logs `msg` for `root` once per refusal, so a root that stays refused is one line, not one per
    /// pass. A root that recovers clears its entry and can warn again when it refuses anew.
    fn note_once(&mut self, root: &str, msg: &str, fields: &[(&str, Value)]) {
        if !self.refused.insert(root.to_owned()) {
            return;
        }
        let mut owned: Vec<(&str, Value)> = Vec::with_capacity(fields.len() + 1);
        owned.push(("repoRoot", text(root)));
        owned.extend(fields.iter().cloned());
        log::warn(msg, &owned);
    }
}

/// A JSON string field, spelled once so every log line in this module builds it the same way.
fn text(value: &str) -> Value {
    Value::String(value.to_owned())
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

/// `new Date(millis).toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// The same bytes `sangkar_core::log` writes for a line's `ts` and `pidlock` writes for
/// `startedAt`; both keep their formatter private, so this module states the one format it needs
/// rather than reaching into another module's internals.
fn iso8601_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1_000);
    let subsecond = millis.rem_euclid(1_000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let second_of_day = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:03}Z")
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date for a day count since
/// 1970-01-01.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use sangkar_core::config::{DEFAULT_REPO_DEFAULTS, RepoDefaults};
    use sangkar_core::registry::{REGISTRY_VERSION, Registry, write_registry};
    use sangkar_index::watch::{Detector, WatchStats};

    use super::{OpenWatcher, Reconciler, Watcher, WatcherOptions, iso8601_millis};

    /// A watcher that records how often it was stopped, so "unchanged means untouched" is a count and
    /// not an inference from a log line.
    struct Stub {
        stops: Arc<AtomicUsize>,
        stats: WatchStats,
    }

    impl Watcher for Stub {
        fn stop(&self) {
            self.stops.fetch_add(1, Ordering::Relaxed);
        }

        fn stats(&self) -> WatchStats {
            self.stats
        }
    }

    /// The factory seam a test needs: what was asked for, and a handle per open.
    #[derive(Clone, Default)]
    struct Spy {
        opened: Arc<Mutex<Vec<WatcherOptions>>>,
        stops: Arc<Mutex<Vec<Arc<AtomicUsize>>>>,
    }

    impl Spy {
        fn factory(&self) -> OpenWatcher {
            let opened = Arc::clone(&self.opened);
            let stops = Arc::clone(&self.stops);
            Arc::new(move |options: &WatcherOptions| {
                let counter = Arc::new(AtomicUsize::new(0));
                opened.lock().expect("the open list").push(options.clone());
                stops
                    .lock()
                    .expect("the stop list")
                    .push(Arc::clone(&counter));
                Ok(Box::new(Stub {
                    stops: counter,
                    stats: WatchStats {
                        last_exit_code: Some(0),
                        detector: Some(Detector::Mtime),
                        ..WatchStats::default()
                    },
                }) as Box<dyn Watcher>)
            })
        }

        fn opened(&self) -> Vec<WatcherOptions> {
            self.opened.lock().expect("the open list").clone()
        }

        /// How often each opened watcher was stopped, in open order.
        fn stop_counts(&self) -> Vec<usize> {
            self.stops
                .lock()
                .expect("the stop list")
                .iter()
                .map(|counter| counter.load(Ordering::Relaxed))
                .collect()
        }
    }

    /// Complete repository files, so a read never warns about a key the fixture left out.
    const FULL_CONFIG_4000: &str = r#"{"watch":true,"watch_interval":4000,"telemetry":true}"#;
    const FULL_CONFIG_5000: &str = r#"{"watch":true,"watch_interval":5000,"telemetry":true}"#;
    const PAUSED_CONFIG: &str = r#"{"watch":false,"watch_interval":5000,"telemetry":false}"#;

    /// A temp home removed on drop, named uniquely so cases and runs never share a registry.
    struct Home(PathBuf);

    impl Home {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicUsize = AtomicUsize::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "sangkar-reconcile-{label}-{}-{unique}",
                std::process::id()
            ));
            fs::create_dir_all(&path).expect("a temp home");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// A repository directory with an optional on-disk config, in its absolute spelling.
        fn repo(&self, name: &str, config: Option<&str>) -> PathBuf {
            let root = self.0.join(name);
            fs::create_dir_all(&root).expect("a repo dir");
            if let Some(config) = config {
                let data_dir = root.join(".sangkar");
                fs::create_dir_all(&data_dir).expect("a data dir");
                fs::write(data_dir.join("config.json"), config).expect("a repo config");
            }
            root
        }

        fn registry(&self, roots: &[PathBuf]) {
            let registry = Registry {
                version: REGISTRY_VERSION,
                repos: roots.to_vec(),
            };
            write_registry(&self.0.join("sangkar.json"), &registry).expect("a registry");
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn reconciler(home: &Home, spy: &Spy) -> Reconciler {
        let defaults: RepoDefaults = DEFAULT_REPO_DEFAULTS;
        Reconciler::new(home.path(), defaults, spy.factory())
    }

    fn roots(paths: &[&PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.display().to_string())
            .collect()
    }

    /// One pass opens exactly one watcher per watched root at that repository's own interval, and a
    /// second pass leaves every handle alone — the no-restart rule, proven by a zero stop count.
    #[test]
    fn one_pass_opens_one_watcher_per_root_and_a_quiet_pass_touches_nothing() {
        let home = Home::new("open");
        let alpha = home.repo("alpha", None);
        let beta = home.repo("beta", Some(FULL_CONFIG_4000));
        home.registry(&[alpha.clone(), beta.clone()]);
        let spy = Spy::default();
        let mut reconciler = reconciler(&home, &spy);

        let first = reconciler.tick();

        assert_eq!(first.started, roots(&[&alpha, &beta]));
        assert_eq!(first.watched, roots(&[&alpha, &beta]));
        assert!(first.stopped.is_empty());
        assert_eq!(
            spy.opened()
                .iter()
                .map(|options| options.interval_ms)
                .collect::<Vec<u64>>(),
            vec![2_000, 4_000],
            "the machine default and the repository's own file, each as read"
        );
        assert_eq!(
            spy.opened()[0].data_dir,
            alpha.join(".sangkar"),
            "the watcher and the worker name one graph"
        );

        let second = reconciler.tick();
        assert!(second.started.is_empty(), "a quiet pass restarts nothing");
        assert!(second.stopped.is_empty());
        assert_eq!(second.watched, first.watched);
        assert_eq!(spy.opened().len(), 2, "two watchers, never a third");
        assert_eq!(spy.stop_counts(), vec![0, 0], "unchanged means untouched");
    }

    /// A raised interval restarts only that repository, and the pass reports it as both stopped and
    /// started because both happened.
    #[test]
    fn a_retuned_interval_restarts_only_that_root() {
        let home = Home::new("retune");
        let alpha = home.repo("alpha", None);
        let beta = home.repo("beta", Some(FULL_CONFIG_4000));
        home.registry(&[alpha.clone(), beta.clone()]);
        let spy = Spy::default();
        let mut reconciler = reconciler(&home, &spy);
        reconciler.tick();

        fs::write(beta.join(".sangkar").join("config.json"), FULL_CONFIG_5000)
            .expect("a retuned repo config");
        let retuned = reconciler.tick();

        assert_eq!(retuned.stopped, roots(&[&beta]));
        assert_eq!(retuned.started, roots(&[&beta]));
        assert_eq!(retuned.watched, roots(&[&alpha, &beta]));
        assert_eq!(
            spy.stop_counts(),
            vec![0, 1, 0],
            "only beta's first handle ended; the retuned one is its own"
        );
        assert_eq!(spy.opened().len(), 3);
        assert_eq!(spy.opened()[2].interval_ms, 5_000);
    }

    /// A root the operator turned off, one whose directory is gone and one dropped from the registry
    /// are each stopped and reported, and a steady refusal is logged once rather than every pass.
    #[test]
    fn watch_off_missing_and_unregistered_roots_are_reported_and_stopped() {
        let home = Home::new("ladder");
        let quiet = home.repo("quiet", Some(PAUSED_CONFIG));
        let gone = home.path().join("gone");
        let loud = home.repo("loud", None);
        home.registry(&[quiet.clone(), gone.clone(), loud.clone()]);
        let spy = Spy::default();
        let mut reconciler = reconciler(&home, &spy);

        let first = reconciler.tick();
        assert_eq!(first.skipped, roots(&[&quiet]));
        assert_eq!(first.missing, roots(&[&gone]));
        assert_eq!(first.started, roots(&[&loud]));
        assert_eq!(first.watched, roots(&[&loud]));
        assert_eq!(spy.opened().len(), 1, "an opt-out never opens a watcher");

        let second = reconciler.tick();
        assert_eq!(second.skipped, first.skipped);
        assert_eq!(second.missing, first.missing);
        assert!(second.started.is_empty());

        home.registry(&[quiet]);
        let dropped = reconciler.tick();
        assert_eq!(dropped.stopped, roots(&[&loud]));
        assert!(dropped.watched.is_empty());
        assert_eq!(spy.stop_counts(), vec![1]);
    }

    /// A watcher that will not open is skipped and logged, and the rest of the pass still runs.
    #[test]
    fn a_watcher_that_will_not_open_does_not_abort_the_pass() {
        let home = Home::new("refused-open");
        let broken = home.repo("broken", None);
        let fine = home.repo("fine", None);
        home.registry(&[broken.clone(), fine.clone()]);
        let open: OpenWatcher = Arc::new(move |options: &WatcherOptions| {
            if options.repo_root.ends_with("broken") {
                return Err("no thread".to_owned());
            }
            Ok(Box::new(Stub {
                stops: Arc::new(AtomicUsize::new(0)),
                stats: WatchStats::default(),
            }) as Box<dyn Watcher>)
        });
        let mut reconciler = Reconciler::new(home.path(), DEFAULT_REPO_DEFAULTS, open);

        let report = reconciler.tick();

        assert_eq!(report.skipped, roots(&[&broken]));
        assert_eq!(report.started, roots(&[&fine]));
        assert_eq!(report.watched, roots(&[&fine]));
    }

    /// What a stopped reconciler answers: nothing watched, nothing opened, nothing stamped.
    #[test]
    fn a_stopped_reconciler_opens_nothing_and_records_nothing() {
        let home = Home::new("stopped");
        let alpha = home.repo("alpha", None);
        home.registry(&[alpha]);
        let spy = Spy::default();
        let mut reconciler = reconciler(&home, &spy);
        reconciler.tick();

        reconciler.stop();
        let after = reconciler.tick();

        assert_eq!(after, super::ReconcileReport::default());
        assert_eq!(spy.opened().len(), 1, "stop opens nothing new");
        assert_eq!(spy.stop_counts(), vec![1], "stop ends the watcher once");
        assert!(reconciler.records().is_empty());
    }

    /// The rows a `status` reads: one per covered root, the pass timestamp, and the watcher's last
    /// exit code where one exists.
    #[test]
    fn the_records_carry_the_pass_time_and_the_last_exit_code() {
        let home = Home::new("records");
        let quiet = home.repo("quiet", Some(PAUSED_CONFIG));
        let loud = home.repo("loud", None);
        home.registry(&[quiet.clone(), loud.clone()]);
        let spy = Spy::default();
        let mut reconciler = reconciler(&home, &spy);
        reconciler.tick();

        let records = reconciler.records();

        assert_eq!(
            records
                .iter()
                .map(|record| record.root.clone())
                .collect::<Vec<String>>(),
            roots(&[&quiet, &loud])
        );
        assert!(
            records[0]
                .last_run
                .as_deref()
                .is_some_and(|at| at.ends_with('Z')),
            "the pass stamps an ISO-8601 instant: {:?}",
            records[0].last_run
        );
        assert_eq!(records[0].last_exit, None, "an opt-out has no run");
        assert_eq!(records[1].last_exit, Some(0), "the watcher's last run");
        assert_eq!(records[1].generation, None, "the daemon reads no graph");
    }

    /// The formatter this module borrowed for `last_run`, checked against the values
    /// `sangkar_core::log` is tested with — the same `toISOString()` bytes.
    #[test]
    fn the_pass_timestamp_is_the_iso_string_the_pid_lock_writes() {
        let cases: [(i64, &str); 4] = [
            (0, "1970-01-01T00:00:00.000Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
            (1_709_210_096_789, "2024-02-29T12:34:56.789Z"),
            (1_767_225_600_000, "2026-01-01T00:00:00.000Z"),
        ];
        for (millis, expected) in cases {
            assert_eq!(iso8601_millis(millis), expected, "millis {millis}");
        }
    }
}
