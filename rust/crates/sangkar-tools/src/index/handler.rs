//! `index`: spawn the worker and report what it published (`TOOLS-SPEC.md` §8).
//!
//! The tool never indexes. The writer lives in exactly one process (`TECH-SPEC.md` §8), so this
//! handler starts the command `AppConfig::worker_cmd` names in its `worker` mode and reads that
//! child's stderr: a parser that runs out of memory kills the worker, not the session that answered
//! the call. Nothing here imports an indexer module and nothing here opens the database — the
//! repair path is the last place to discover a boundary leak.
//!
//! The child's whole reporting surface is JSON log lines on stderr, because stdout above it carries
//! the protocol. The lines this tool reads are the TypeScript worker's `log()` records
//! (`{"ts","level","msg",…}`):
//!
//! - `worker.published` — the run that published: `files`, `edges`, `durationMs`, `mode`, `generation`;
//! - `worker.nothing_to_do` — the tree already matched the graph, with `files` offered;
//! - `worker.locked` — another indexer holds the write lock, with `pid` and `retryAfterMs`;
//! - `worker.file_not_parsed` — one line per file the parser could not handle;
//! - `worker.failed` — the run's own reason, which an exit 1 reports instead of a stack trace.
//!
//! Anything the child does not report is not invented here: it logs no `not_indexed` count and no
//! unresolved-edge count, so `coverage.complete` is always `false` on an `index` answer and the tool
//! claims only the numbers it was told. `status` is the tool that answers coverage from the database.
//!
//! The process is the only one this crate starts, and the command it starts is a value on the
//! resolved config: during the migration a machine points `SANGKAR_WORKER_CMD` at the TypeScript
//! worker, and after Phase 3 it is this executable. The handler itself consults no process state —
//! its whole input is `&ToolContext` (which owns the resolved [`sangkar_core::config::AppConfig`])
//! and `&IndexArgs` — so "the child indexes the tree the server is bound to" is a property of the
//! type rather than of a call order.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use sangkar_core::budget::emit;
use sangkar_core::envelope::{Coverage, Envelope, EnvelopeSpec, ReadStatus, fail, ok};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log::Level;
use sangkar_core::registry::{canonical_root, register_repo};
use sangkar_store::lock::lock_path;
use sangkar_store::read::context::{LogField, ToolContext};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::index::manifest::{IndexArgs, IndexMode};

/// A child's last line is the only diagnosis there is; cap it so a stack trace cannot bloat an
/// answer. The TypeScript caps at 400 UTF-16 units; this counts characters, so a cut never splits a
/// multi-byte glyph, and the two limits differ only for text no compiler emits.
const MAX_ERROR_LINE_CHARS: usize = 400;

/// The backstop that keeps a hung child from holding the call forever: the reference corpus indexes
/// in ~40 s, so only a stuck process reaches this. The TypeScript awaits `exited` with no bound at
/// all; a repair tool whose child never exits must still answer.
pub const WORKER_TIMEOUT: Duration = Duration::from_secs(900);

/// How long a killed child gets to die and flush what it already wrote.
const KILL_GRACE: Duration = Duration::from_secs(1);

/// How often the wait loop drains the reader and polls the child.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The hint an exit 1 and an unexpected status share: the answer carries the child's last line, and
/// the full run is still on the operator's terminal.
const RUN_HINT: &str = "run `sangkar index` and read its stderr for the full run";

/// The files a run accounted for.
///
/// `scanned` is the number the worker's line reports as `files` — every path the discovery walk
/// offered to the parser; `parsed` is that number minus the paths that logged a parse failure, which
/// is exact over the set the child reported. `skipped` is always `0` because the child logs no
/// count for the paths discovery excluded.
///
/// The field is named `scanned` because `src/tools/index/handler.ts:55-63` names it `scanned`, and a
/// payload key is wire contract: a consumer reading `files.indexed` gets nothing from the TypeScript.
/// The plan's contract called it `indexed`; the source wins, and the meaning is unchanged (paths
/// offered to the parser, not paths that produced a unit — `parsed` is that narrower number).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexFileCounts {
    pub scanned: u64,
    pub parsed: u64,
    pub failed: u64,
    pub skipped: u64,
}

/// What a successful run published (`TOOLS-SPEC.md` §8).
///
/// `duration_ms` is an integer because it is a duration in milliseconds; a float would render `0.0`
/// where `JSON.stringify` writes `0`, which is a byte the parity gate compares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexPayload {
    /// The mode the run published under, which can differ from the request after a forced rebuild.
    pub mode: IndexMode,
    pub duration_ms: u64,
    /// The generation that was live when the run started; `None` on a graph with no publish yet.
    pub previous_generation: Option<String>,
    pub files: IndexFileCounts,
}

/// One stderr line from the child, already classified and forwarded to the context's logger.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerEvent {
    /// The child's own message name, or `worker.stderr` for a line that is not JSON.
    pub msg: String,
    pub level: Level,
    /// The line's fields without `ts`/`level`/`msg`, which are the envelope of the log line itself.
    pub fields: Map<String, Value>,
    /// The line exactly as written, for the diagnosis an exit status cannot carry.
    pub raw: String,
}

/// How a child ended. Two cases, because a child that was killed has no status byte and pretending
/// one (`128 + signal`) would turn a kill into a code this tool would then blame on the worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerExit {
    Code(i32),
    /// Killed by a signal; the number is absent where the platform does not report one.
    Signal(Option<i32>),
}

/// What one run reported: the status it ended with, how long it ran before this tool killed it,
/// and every line it wrote. `killed_after` is set by the wait loop when the backstop fired, so the
/// envelope can say that the kill was this tool's decision rather than the child's failure.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkerReport {
    pub exit: WorkerExit,
    /// `Some(elapsed)` when this tool killed the child because it outlived the backstop.
    pub killed_after: Option<Duration>,
    /// Every stderr line, in arrival order: the child's report, not just its last line.
    pub events: Vec<WorkerEvent>,
}

/// The resolved child invocation: which program to start and with which arguments.
///
/// No environment map follows it: the child inherits this process's environment, which is not a
/// config read — it shells out to `git` and needs `PATH`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerInvocation {
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl WorkerInvocation {
    /// The whole argv, program first, for a test and for a failure message.
    #[must_use]
    pub fn cmd(&self) -> Vec<OsString> {
        let mut cmd = Vec::with_capacity(self.args.len() + 1);
        cmd.push(self.program.clone());
        cmd.extend(self.args.iter().cloned());
        cmd
    }

    /// The argv as one line for the failure that could not start it.
    #[must_use]
    pub fn command_line(&self) -> String {
        self.cmd()
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The process to start. No shell: the argv is already split, and a shell would turn a path with
    /// a space into two arguments.
    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.args);
        command
    }
}

/// The generation that is live *before* the child runs, read through the context's reader.
struct LiveSnapshot {
    generation: Option<String>,
    indexed_at: Option<String>,
}

/// The child command, and the only place the two paths it acts on are decided.
///
/// The tree and the data dir travel as `--repo-root`/`--data-dir` — the two settings the resolved
/// config already carries — so the child can only ever index the repository the server is bound to,
/// never its own working directory. The mode is always passed, as the TypeScript does: the worker's
/// own `full` default is not a second contract this tool could drift from. `--force` is a bare flag
/// and comes last, which is the order `bin/sangkar.ts` parses.
///
/// # Errors
/// `internal` when the configured command is empty: a config that names no program is a startup
/// mistake, not caller input, and `spawn` would otherwise report it as a missing file named "".
pub fn worker_invocation(
    ctx: &ToolContext,
    args: &IndexArgs,
) -> Result<WorkerInvocation, AppError> {
    let mut configured = ctx.config.worker_cmd.iter();
    let Some(program) = configured.next() else {
        return Err(
            AppError::internal("no worker command is configured").with_hint(
                "set SANGKAR_WORKER_CMD, or run the binary that carries its own worker mode",
            ),
        );
    };
    let mut argv: Vec<OsString> = configured.cloned().collect();
    argv.push(OsString::from("worker"));
    argv.push(OsString::from("--repo-root"));
    argv.push(ctx.config.repo_root.clone().into_os_string());
    argv.push(OsString::from("--data-dir"));
    argv.push(ctx.config.data_dir.clone().into_os_string());
    argv.push(OsString::from("--mode"));
    argv.push(OsString::from(args.mode().as_str()));
    if args.force() {
        argv.push(OsString::from("--force"));
    }
    Ok(WorkerInvocation {
        program: program.clone(),
        args: argv,
    })
}

/// One free-text repair request in, one envelope out (`TECH-SPEC.md` §6: handlers return envelopes,
/// never throw, never read the environment and never open a database).
///
/// The spawn seam is [`handle_with`]; production passes [`stream_worker`].
#[must_use]
pub fn handle(ctx: &ToolContext, args: &IndexArgs) -> Envelope<IndexPayload> {
    handle_with(ctx, args, stream_worker)
}

/// [`handle`] with the spawn injected, the way the TypeScript `handle`'s `run` parameter is: a test
/// supplies canned events so the five exit-status mappings are asserted without a child process.
///
/// The invocation a runner receives is still built by [`worker_invocation`] from the config, which
/// is what keeps "the child indexes the tree the server was bound to" testable in the same file.
#[must_use]
pub fn handle_with(
    ctx: &ToolContext,
    args: &IndexArgs,
    run: impl FnOnce(&ToolContext, &WorkerInvocation) -> Result<WorkerReport, AppError>,
) -> Envelope<IndexPayload> {
    let snapshot = live_snapshot(ctx);
    let started = Instant::now();
    let invocation = match worker_invocation(ctx, args) {
        Ok(invocation) => invocation,
        Err(error) => return failed_internal(ctx, &snapshot, &(ctx.now)(), error),
    };
    let report = match run(ctx, &invocation) {
        Ok(report) => report,
        Err(error) => return failed_internal(ctx, &snapshot, &(ctx.now)(), error),
    };
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    // One clock read per call, reused by every branch: a report and its `read_status` must not be
    // stamped two instants apart.
    let now = (ctx.now)();
    let envelope = envelope_of(ctx, &report, args, &snapshot, elapsed_ms, &now);

    // Only a run that succeeded leaves a tree worth watching, and only after the report is mapped:
    // registering an exit 1, a 75 or a rejected spawn would add a repository the next call cannot
    // read, and a refused registration must never change the answer this call already has. The
    // condition is the child's status, exactly as the TypeScript has it — an exit 0 whose report was
    // malformed still indexed the tree.
    if matches!(report.exit, WorkerExit::Code(0) | WorkerExit::Code(2)) && args.register() {
        register_tree(ctx);
    }
    envelope
}

/// The five mappings, in one place so [`handle_with`] can answer before it registers.
fn envelope_of(
    ctx: &ToolContext,
    report: &WorkerReport,
    args: &IndexArgs,
    snapshot: &LiveSnapshot,
    elapsed_ms: u64,
    now: &str,
) -> Envelope<IndexPayload> {
    // A child this tool killed did not fail: it did not finish. Saying so is the difference between
    // "the indexer is broken" and "the indexer is slow", and the last line it wrote is still quoted.
    if let Some(elapsed) = report.killed_after {
        return failed_internal(
            ctx,
            snapshot,
            now,
            AppError::internal(format!(
                "the index worker did not finish and was killed after {}: {}",
                seconds(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)),
                last_error_line(report)
            ))
            .with_hint(RUN_HINT),
        );
    }
    // Every other status is a failure. There is no branch for 70: that code is `EX_SOFTWARE`,
    // reserved for the deleted launcher's missing-platform refusal (TASK-058), and no path emits it.
    match report.exit {
        WorkerExit::Code(0) => published(ctx, report, args, snapshot, now),
        WorkerExit::Code(2) => nothing_to_do(report, args, snapshot, elapsed_ms, now),
        WorkerExit::Code(75) => locked(ctx, report, snapshot, now),
        WorkerExit::Code(code) => failed_internal(
            ctx,
            snapshot,
            now,
            AppError::internal(format!(
                "the index worker failed (exit {code}): {}",
                last_error_line(report)
            ))
            .with_hint(RUN_HINT),
        ),
        WorkerExit::Signal(signal) => failed_internal(
            ctx,
            snapshot,
            now,
            AppError::internal(format!(
                "the index worker failed ({}): {}",
                killed_by(signal),
                last_error_line(report)
            ))
            .with_hint(RUN_HINT),
        ),
    }
}

/// A published run: the counts the child printed, and the generation it says it published.
fn published(
    ctx: &ToolContext,
    report: &WorkerReport,
    args: &IndexArgs,
    snapshot: &LiveSnapshot,
    now: &str,
) -> Envelope<IndexPayload> {
    let Some(line) = last_event(report, "worker.published") else {
        return failed_internal(
            ctx,
            snapshot,
            now,
            AppError::internal("the index worker exited 0 without publishing a report").with_hint(
                "run `sangkar index` and read its stderr: the worker's last line is the report",
            ),
        );
    };

    let offered = count_in(Some(line), "files").unwrap_or(0);
    let failed = count_events(report, "worker.file_not_parsed");
    // Every offered path either produced a unit or logged a parse failure, so this subtraction is
    // exact over the set the child reported; discovery's excluded paths never reach the parser.
    let files = IndexFileCounts {
        scanned: offered,
        parsed: offered.saturating_sub(failed),
        failed,
        skipped: 0,
    };
    // The child names the mode it published under, which a forced rebuild may have changed; a mode
    // the manifest does not declare is not believed, and the request's own mode answers instead.
    let mode = text_in(Some(line), "mode")
        .and_then(|raw| IndexMode::parse(&raw))
        .unwrap_or_else(|| args.mode());
    let duration_ms = count_in(Some(line), "durationMs").unwrap_or(0);
    let edges = count_in(Some(line), "edges");
    let generation = text_in(Some(line), "generation")
        .or_else(|| snapshot.generation.clone())
        .unwrap_or_default();

    let payload = IndexPayload {
        mode,
        duration_ms,
        previous_generation: snapshot.generation.clone(),
        files: files.clone(),
    };
    // The child names the generation it published but not that generation's `published_at`, and a
    // success is fresh by definition: the run just finished, so this clock is the publish time to
    // within the run's own tail.
    let mut read_status = ReadStatus::new(generation, now.to_owned());
    read_status.stale_checked_at = Some(now.to_owned());
    let mut summary = format!(
        "{} index published: {} file(s) parsed, {} failed",
        capitalized(mode),
        files.parsed,
        files.failed
    );
    if let Some(edges) = edges {
        summary.push_str(&format!(", {edges} edge(s)"));
    }
    summary.push_str(&format!(" in {}.", seconds(duration_ms)));
    sized(
        ctx,
        ok(
            payload,
            EnvelopeSpec::new(summary, read_status, coverage_of(&files)),
        ),
    )
}

/// Exit 2: the tree already matches the graph, so nothing was published and nothing was parsed.
fn nothing_to_do(
    report: &WorkerReport,
    args: &IndexArgs,
    snapshot: &LiveSnapshot,
    elapsed_ms: u64,
    now: &str,
) -> Envelope<IndexPayload> {
    let files = IndexFileCounts {
        scanned: count_in(last_event(report, "worker.nothing_to_do"), "files").unwrap_or(0),
        ..IndexFileCounts::default()
    };
    let payload = IndexPayload {
        mode: args.mode(),
        duration_ms: elapsed_ms,
        previous_generation: snapshot.generation.clone(),
        files: files.clone(),
    };
    let summary = format!(
        "Nothing to do: the graph already matches the working tree ({} files scanned).",
        files.scanned
    );
    // Nothing published, so the live generation answered and is unchanged — but it is still fresh:
    // the run just confirmed the tree matches it.
    ok(
        payload,
        EnvelopeSpec::new(
            summary,
            read_status_of_live(snapshot, now),
            coverage_of(&files),
        ),
    )
}

/// Exit 75: another indexer holds the lock, which is a retry, not a failure of this call.
fn locked(
    ctx: &ToolContext,
    report: &WorkerReport,
    snapshot: &LiveSnapshot,
    now: &str,
) -> Envelope<IndexPayload> {
    let line = last_event(report, "worker.locked");
    let retry_after_ms = count_in(line, "retryAfterMs");
    let holder = count_in(line, "pid");
    let message = holder.map_or_else(
        || "another indexer is writing this graph".to_owned(),
        |pid| format!("another indexer (pid {pid}) is writing this graph"),
    );
    let mut error = AppError::new(ErrorCode::INDEX_LOCKED, message)
        .with_hint(format!(
            "wait for it to finish and retry; the lock file is {}",
            lock_path(&ctx.config.data_dir).display()
        ))
        .with_retryable(true);
    if let Some(retry_after_ms) = retry_after_ms {
        error = error.with_retry_after_ms(retry_after_ms);
    }
    // The summary repeats the reason rather than re-deriving it, so the two cannot say different
    // things about who holds the lock.
    let summary = error.message.clone();
    sized(
        ctx,
        fail(
            error,
            EnvelopeSpec::new(
                summary,
                read_status_of_unpublished(snapshot, now),
                coverage_of(&IndexFileCounts::default()),
            ),
        ),
    )
}

/// Exit 1, a signal, a rejected spawn and a timeout: the run failed, and the message is the child's
/// own diagnosis. A generic "indexing failed" would throw away the only information the call
/// produced.
fn failed_internal(
    ctx: &ToolContext,
    snapshot: &LiveSnapshot,
    now: &str,
    error: AppError,
) -> Envelope<IndexPayload> {
    let summary = error.message.clone();
    sized(
        ctx,
        fail(
            error,
            EnvelopeSpec::new(
                summary,
                read_status_of_unpublished(snapshot, now),
                coverage_of(&IndexFileCounts::default()),
            ),
        ),
    )
}

/// The run's own accounting as the envelope's coverage. `complete` is always `false`: the child
/// reports offered, failed and published files and nothing about `not_indexed` or unresolved edges,
/// so this response may claim neither a complete graph nor an empty gap list.
///
/// A run that published nothing reports zeros, and `complete: false` is what makes them read as
/// "this response measured nothing" rather than "the graph is empty".
fn coverage_of(files: &IndexFileCounts) -> Coverage {
    Coverage {
        complete: false,
        indexed: files.parsed,
        skipped: files.skipped,
        failed: files.failed,
        unresolved_edges: 0,
        not_indexed: Vec::new(),
        tests: None,
    }
}

/// The generation that is live now, from the context's reader. Best effort by design: a graph broken
/// enough to need this tool is exactly when `meta_for` fails (a missing table, a refused open), and
/// the repair path must not be blocked by the thing it was called to repair.
fn live_snapshot(ctx: &ToolContext) -> LiveSnapshot {
    match live_of(ctx) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            ctx.log.warn(
                "index.live_generation_unavailable",
                &[("reason", LogField::Str(error.message))],
            );
            LiveSnapshot {
                generation: None,
                indexed_at: None,
            }
        }
    }
}

fn live_of(ctx: &ToolContext) -> Result<LiveSnapshot, AppError> {
    let meta = ctx.reader.meta_for()?;
    // `GraphMeta` carries no `indexed_at`; the served generation's `published_at` is the reader's
    // only published timestamp (the same conclusion `trace` and `status` reached).
    let indexed_at = ctx.reader.latest_generation()?.map(|row| row.published_at);
    Ok(LiveSnapshot {
        generation: meta.generation,
        indexed_at,
    })
}

/// `read_status` for a call that changed nothing: the snapshot the reader could give, marked stale
/// when the graph has no published generation at all (the one state where `status` answers
/// `bootstrapping`, and the honest reason a repair was asked for). An unpublished generation has no
/// id, so `generation` is empty rather than a made-up one.
fn read_status_of_live(snapshot: &LiveSnapshot, now: &str) -> ReadStatus {
    let mut status = ReadStatus::new(
        snapshot.generation.clone().unwrap_or_default(),
        snapshot.indexed_at.clone().unwrap_or_default(),
    );
    status.stale = snapshot.generation.is_none();
    status.stale_checked_at = Some(now.to_owned());
    if status.stale {
        status.stale_reason = Some("the graph has no published generation yet".to_owned());
    }
    status
}

/// `read_status` for a call whose run did not publish: the live generation, and why it is stale.
fn read_status_of_unpublished(snapshot: &LiveSnapshot, now: &str) -> ReadStatus {
    let mut status = read_status_of_live(snapshot, now);
    status.stale = true;
    status.stale_reason =
        Some("the index run did not publish; the live generation is unchanged".to_owned());
    status
}

/// The budget rule for a payload without lists.
///
/// `index` has no `budget` argument (`TOOLS-SPEC.md` §10), so the configured fallback applies;
/// [`emit`] is still the one place sizing happens, and it returns the envelope unchanged when there
/// is nothing to cut. A refusal here (`budget_too_large` for a misconfigured ceiling) must still be
/// an envelope, which is why the answer's skeleton is kept for the failure.
fn sized(ctx: &ToolContext, env: Envelope<IndexPayload>) -> Envelope<IndexPayload> {
    let summary = env.summary.clone();
    let read_status = env.read_status.clone();
    let coverage = env.coverage.clone();
    match emit(env, ctx.config.budgets.default_tokens, unreachable_mint) {
        Ok(env) => env,
        Err(error) => fail(error, EnvelopeSpec::new(summary, read_status, coverage)),
    }
}

/// `emit` mints `next` only when it dropped an item, and this payload has no list to drop: its one
/// array is `coverage.not_indexed`, empty by construction, and `files` is an object of counts.
/// Sizing therefore either returns the envelope unchanged or cannot shrink it at all — a repair
/// report is one screen.
///
/// The mint is unreachable, and it panics rather than inventing a token a client could not resume:
/// a list added to this payload later must decide about paging instead of inheriting a meaningless
/// cursor. (The TypeScript's `unreachableMint` throws an `AppError` there because its `sized`
/// catches one; a mint here returns a `String`, so the same unreachable branch is a panic.)
fn unreachable_mint(position: usize) -> String {
    unreachable!("index has no cursor, but {position} item(s) were dropped from its payload")
}

/// Publishes this tree into the machine's registry, so the daemon watches what was just indexed.
///
/// Registering is a convenience for the next call, so it must never take down this one: a refused
/// write becomes one warning and the caller returns the envelope the run already mapped. That is
/// also why the returned registry is inspected — the write is swallowed on failure, so membership
/// of the root is the only evidence the write took. A repository that was already registered reports
/// `added: false` too, and in that case its root *is* an entry.
fn register_tree(ctx: &ToolContext) {
    let root = canonical_root(&ctx.config.repo_root, None);
    let result = register_repo(&ctx.config.registry_path, &ctx.config.repo_root);
    if !result.registry.repos.contains(&root) {
        ctx.log.warn(
            "index.register_failed",
            &[(
                "reason",
                LogField::Str(format!(
                    "the registry at {} was not updated",
                    ctx.config.registry_path.display()
                )),
            )],
        );
        return;
    }
    ctx.log.info(
        "index.registered",
        &[
            (
                "repoRoot",
                LogField::Str(ctx.config.repo_root.display().to_string()),
            ),
            ("added", LogField::Bool(result.added)),
        ],
    );
}

/// Spawns the worker, forwards its stderr into the context's logger line by line, and returns the
/// whole report.
///
/// Streamed rather than buffered so an index that runs for a minute is visible while it runs: the
/// worker's progress lines are the only window into a child that takes 40 s on the reference corpus.
/// `stdout` is discarded on purpose — the child must not be able to write into the stream the MCP
/// protocol owns — and stdin is closed so nothing waits on a terminal.
///
/// # Errors
/// `internal` when the child cannot be started (the message names the argv that failed), and when
/// it cannot be waited for or reaped.
pub fn stream_worker(
    ctx: &ToolContext,
    invocation: &WorkerInvocation,
) -> Result<WorkerReport, AppError> {
    stream_worker_within(ctx, invocation, WORKER_TIMEOUT)
}

/// [`stream_worker`] with the backstop injected, so a test can prove that a child that never exits
/// is killed instead of held forever.
pub fn stream_worker_within(
    ctx: &ToolContext,
    invocation: &WorkerInvocation,
    timeout: Duration,
) -> Result<WorkerReport, AppError> {
    let mut command = invocation.command();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child: Child = command.spawn().map_err(|error| {
        AppError::internal(format!(
            "could not start the index worker {}: {error}",
            invocation.command_line()
        ))
        .with_hint(
            "the command comes from worker_cmd (SANGKAR_WORKER_CMD, or this executable); \
             check that it exists and is executable",
        )
    })?;
    let Some(stderr) = child.stderr.take() else {
        // Unreachable: stderr was piped two lines above. Reap rather than leave a child behind.
        let _ = child.kill();
        let _ = child.wait();
        return Err(AppError::internal(
            "the index worker's stderr was not piped",
        ));
    };

    // The reader owns the pipe so the wait below can be a deadline rather than a blocking read: a
    // child that never closes stderr would otherwise hold this call forever.
    let (sender, receiver) = mpsc::channel::<WorkerEvent>();
    let reader = thread::spawn(move || read_events(stderr, &sender));

    let mut events: Vec<WorkerEvent> = Vec::new();
    let began = Instant::now();
    let deadline = began + timeout;
    let mut status: Option<ExitStatus> = None;
    let mut killed_after: Option<Duration> = None;
    let mut kill_deadline: Option<Instant> = None;
    let mut closed = false;
    loop {
        loop {
            match receiver.try_recv() {
                Ok(event) => {
                    forward(ctx, &event);
                    events.push(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    closed = true;
                    break;
                }
            }
        }
        if status.is_none() {
            match child.try_wait() {
                Ok(exited) => status = exited,
                Err(error) => {
                    // A wait that failed leaves a child this tool can no longer observe; reap it here
                    // rather than returning with a process still running behind the answer.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AppError::internal(format!(
                        "cannot wait for the index worker: {error}"
                    )));
                }
            }
        }
        // Both halves must settle before the loop ends: the exit status arrives before the pipe's
        // remaining lines are drained, and dropping the channel early would lose them.
        if status.is_some() && closed {
            break;
        }
        let now = Instant::now();
        if kill_deadline.is_none() && now >= deadline {
            // One kill, then a bounded grace to drain what it wrote before dying. SIGKILL cannot be
            // ignored, so the child is reaped on the next turn of this loop.
            if child.kill().is_ok() {
                killed_after = Some(began.elapsed());
            }
            kill_deadline = Some(now + KILL_GRACE);
        }
        if kill_deadline.is_some_and(|grace| now >= grace) {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    let status = match status {
        Some(status) => status,
        None => child.wait().map_err(|error| {
            AppError::internal(format!("cannot reap the index worker: {error}"))
        })?,
    };
    // A reader still blocked on a pipe a grandchild inherited is left detached: joining it would
    // turn this tool's answer into a wait for a process it does not own.
    if closed {
        let _ = reader.join();
    }
    Ok(WorkerReport {
        exit: exit_of(&status),
        killed_after,
        events,
    })
}

/// Reads stderr to the end, one line per event, until the pipe closes.
///
/// A last line without its newline still arrives — `read_until` returns the bytes it read at EOF —
/// so a child killed mid-line is reported rather than silently dropped.
fn read_events(stderr: impl Read, sender: &mpsc::Sender<WorkerEvent>) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // A non-UTF-8 byte sequence becomes the replacement character, as the TypeScript's decoding
        // reader does: a broken stderr must not cost the whole report.
        let line = String::from_utf8_lossy(&buffer);
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        if sender.send(event_of(line)).is_err() {
            // The answer already returned; nothing is left to report to.
            break;
        }
    }
}

/// One stderr line as an event. A line that is not a JSON object is kept whole: an uncaught panic or
/// a runtime banner is exactly the text an exit 1 has to report, and dropping it would leave the
/// failure with a status and no reason.
fn event_of(line: &str) -> WorkerEvent {
    let Ok(Value::Object(record)) = serde_json::from_str::<Value>(line) else {
        return WorkerEvent {
            msg: "worker.stderr".to_owned(),
            level: Level::Warn,
            fields: Map::from_iter([("line".to_owned(), Value::String(line.to_owned()))]),
            raw: line.to_owned(),
        };
    };
    let msg = record
        .get("msg")
        .and_then(Value::as_str)
        .filter(|msg| !msg.is_empty())
        .unwrap_or("worker.log")
        .to_owned();
    let level = match record.get("level").and_then(Value::as_str) {
        Some("debug") => Level::Debug,
        Some("warn") => Level::Warn,
        Some("error") => Level::Error,
        _ => Level::Info,
    };
    let fields = record
        .into_iter()
        .filter(|(key, _)| key != "ts" && key != "level" && key != "msg")
        .collect();
    WorkerEvent {
        msg,
        level,
        fields,
        raw: line.to_owned(),
    }
}

/// Forwards one line under the child's own message name, so its report is greppable server-side.
fn forward(ctx: &ToolContext, event: &WorkerEvent) {
    let fields: Vec<(&str, LogField)> = event
        .fields
        .iter()
        .map(|(key, value)| (key.as_str(), log_field_of(value)))
        .collect();
    match event.level {
        Level::Debug => ctx.log.debug(&event.msg, &fields),
        Level::Info => ctx.log.info(&event.msg, &fields),
        Level::Warn => ctx.log.warn(&event.msg, &fields),
        Level::Error => ctx.log.error(&event.msg, &fields),
    }
}

/// A field as the logger's own scalar. A value the logger cannot express as a scalar is passed as
/// its JSON text rather than dropped, so a nested field is still visible in the line.
fn log_field_of(value: &Value) -> LogField {
    match value {
        Value::String(text) => LogField::Str(text.clone()),
        Value::Bool(flag) => LogField::Bool(*flag),
        Value::Number(number) => number
            .as_i64()
            .map_or_else(|| LogField::Str(number.to_string()), LogField::Int),
        other => LogField::Str(other.to_string()),
    }
}

/// The newest event with this `msg`; the child logs one per outcome, and the last one wins.
fn last_event<'a>(report: &'a WorkerReport, msg: &str) -> Option<&'a WorkerEvent> {
    report.events.iter().rev().find(|event| event.msg == msg)
}

fn count_events(report: &WorkerReport, msg: &str) -> u64 {
    let count = report
        .events
        .iter()
        .filter(|event| event.msg == msg)
        .count();
    u64::try_from(count).unwrap_or(u64::MAX)
}

/// An integer field of this event, or `None`.
///
/// A count is an unsigned integer: a float or a negative value is not one, and is treated as absent
/// rather than truncated, because a wrong count is worse than a missing one. A missing count stays
/// missing — the payload reports `0` only where the contract says a zero is the fact.
fn count_in(event: Option<&WorkerEvent>, key: &str) -> Option<u64> {
    event?.fields.get(key)?.as_u64()
}

/// A non-empty string field of this event, or `None`.
fn text_in(event: Option<&WorkerEvent>, key: &str) -> Option<String> {
    event?
        .fields
        .get(key)?
        .as_str()
        .filter(|text| !text.is_empty())
        .map(ToOwned::to_owned)
}

/// The line an exit 1 reports. The child's own `worker.failed` line carries the reason; failing
/// that, the last error-level line is the crash text, and failing that the last line it wrote (a
/// `worker.stderr` event holds a non-JSON line whole); with nothing at all, the message says so.
fn last_error_line(report: &WorkerReport) -> String {
    let failed = last_event(report, "worker.failed");
    if let Some(reason) = text_in(failed, "reason") {
        return truncated(&reason);
    }
    let source = failed
        .or_else(|| {
            report
                .events
                .iter()
                .rev()
                .find(|event| event.level == Level::Error)
        })
        .or_else(|| report.events.last());
    source.map_or_else(
        || "the worker exited without a line on stderr".to_owned(),
        |event| truncated(&event.raw),
    )
}

fn truncated(text: &str) -> String {
    text.chars().take(MAX_ERROR_LINE_CHARS).collect()
}

/// How a status byte becomes the report's exit: a killed child has no code, and inventing
/// `128 + signal` would report a shell convention as the worker's own number.
fn exit_of(status: &ExitStatus) -> WorkerExit {
    status
        .code()
        .map_or_else(|| WorkerExit::Signal(signal_of(status)), WorkerExit::Code)
}

#[cfg(unix)]
fn signal_of(status: &ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn signal_of(_status: &ExitStatus) -> Option<i32> {
    None
}

/// `"killed by signal 9"`, or a plain phrase where the platform reports no number.
fn killed_by(signal: Option<i32>) -> String {
    signal.map_or_else(
        || "killed by a signal".to_owned(),
        |number| format!("killed by signal {number}"),
    )
}

/// The mode as the first word of a summary: `Full`/`Fast`, as `TOOLS-SPEC.md` §8's example spells it.
fn capitalized(mode: IndexMode) -> &'static str {
    match mode {
        IndexMode::Full => "Full",
        IndexMode::Fast => "Fast",
    }
}

/// Milliseconds as the seconds `toFixed(1)` renders: `41203` is `41.2s`, and `0` is `0.0s`.
fn seconds(milliseconds: u64) -> String {
    format!("{:.1}s", milliseconds as f64 / 1000.0)
}
