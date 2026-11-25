//! `sangkar daemon`: the four subcommands an operator needs to run the background process — or to
//! find out why it is not doing what they expect.
//!
//! Ported from `src/daemon/cli.ts`. The module is deliberately thin: [`crate::lifecycle`] owns the
//! pid file and the three lifecycle verbs, [`crate::main`] owns the process's composition, and the
//! caller resolves the home from the environment. This file names the subcommand, resolves the two
//! flags `run` takes, and turns the answer into the bytes a caller's stdout receives plus an exit code
//! (0 ok, 1 error or not running, 2 usage refusal).
//!
//! It also owns the one adapter the composition needs: [`crate::main::ReportServer`] states three
//! calls — bind, the address, close — and `sangkar-telemetry`'s `serve` answers with its own handle,
//! so [`TelemetryReport`] is where the two shapes meet. `run` always serves the same way
//! `src/daemon/main.ts` does: the daemon's page is the page the registry describes, bound at the
//! composition's third step and closed at its first shutdown step.
//!
//! `run` is the process `start` spawns, so it must never write anything to stdout: everything the
//! running daemon says goes through [`sangkar_core::log`] on stderr — and `start` points the child's
//! stderr at `<home>/daemon.log`, which is what makes the file and the terminal the same stream. The
//! only stdout here belongs to the command that returns.
//!
//! Why the streams are arguments: no `src/` module in this workspace may name `std::io::stdout` or
//! the print macros (`sangkar-boundary`'s stdout rule), because stdout carries the MCP protocol. The
//! binary that owns the process's streams passes them in, exactly as `sangkar_cli`'s modules receive
//! their `Write`s.
//!
//! The four global flags are accepted next to every subcommand and each is inert here: the pid file
//! lives in the machine home, so `--repo-root` and `--data-dir` describe no file this verb reads, and
//! `--json` only changes one answer — `status`'s. Every other subcommand ignores it, which is what
//! makes `daemon start --json` an acceptance rather than a stray flag.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};

use sangkar_core::config::flag_value;
use sangkar_core::errors::AppError;
use sangkar_core::global_config::{read_global_config, repo_defaults_of};
use sangkar_core::log;
use sangkar_core::pidlock::read_pid_lock;
use sangkar_core::repo_config::read_repo_config;
use sangkar_telemetry::serve::{self, ServeError, ServeOptions};
use serde_json::Value;

use crate::lifecycle::{self, DaemonOptions};
use crate::main::{BindAddress, ForegroundOptions, ReportServer, run_daemon_foreground};
use crate::types::{DaemonStatus, ReportAddress};

/// The code a caller returns when the daemon or the filesystem refused: not running, a live daemon
/// already holding the lock, a start that never wrote its pid file.
pub const EXIT_FAILURE: i32 = 1;

/// The code for the argv refusal that happens before there is anything to do.
pub const EXIT_USAGE: i32 = 2;

/// The pid file, spelled as `sangkar_core` and [`crate::lifecycle`] spell it.
const PID_FILE: &str = "daemon.pid";

/// Printed after every refusal. The same text is `VERB_USAGE.daemon` in `sangkar-cli`'s `args.rs`,
/// which answers `daemon --help` before dispatch; a test pins the two together, so a change to one is
/// a change to both.
const USAGE: &str = "usage: sangkar daemon [run|start|stop|status] [--port <n>] [--host <addr>]
  run     run the daemon in the foreground until a signal stops it
  start   start it as a detached child and return once it holds the pid file
  stop    stop the daemon this home's pid file names
  status  one header line, then one line per registered repository
  --port <n>     the report server's port (default: config.json's daemon.port, else 7317; 0 picks a free port)
  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)
  --repo-root and --data-dir are accepted by every subcommand and change nothing here; --json prints status as one JSON object line, and --port/--host still belong to \"run\"
  --port and --host belong to \"run\"; start passes neither, so it binds the same defaults";

/// Everything one `sangkar daemon …` call needs, with no environment read of its own.
///
/// `argv` is the tokens *after* the `daemon` verb, so a dispatcher that already resolved the global
/// flags can hand them over unchanged and this module still validates what it reads.
pub struct DaemonInvocation<'a> {
    /// The tokens after `daemon`, global flags included: this module strips and checks them.
    pub argv: &'a [String],
    /// The machine home, resolved by the one module allowed to read the environment.
    pub home: &'a Path,
    /// The command line `start` spawns, program first — this executable and `daemon run`.
    pub command: Vec<OsString>,
    /// The index worker command line, program first: `AppConfig::worker_cmd`.
    pub worker_cmd: Vec<OsString>,
    /// The report server `run` binds. `None` takes this crate's own — [`TelemetryReport`], which is
    /// `sangkar-telemetry`'s `serve` over [`Self::home`], the page a real daemon serves — and `Some`
    /// substitutes one, which is how a test watches a bind without opening a socket. Every other
    /// subcommand ignores it.
    pub report: Option<Box<dyn ReportServer>>,
    /// Where the answers go. `status` is the only subcommand that writes here.
    pub stdout: &'a mut dyn Write,
    /// Where the refusals go: the reason first, then the block.
    pub stderr: &'a mut dyn Write,
}

/// One `sangkar daemon …` call: the answer, and the exit code the process should leave with.
pub fn run_daemon_cli(invocation: DaemonInvocation<'_>) -> i32 {
    let DaemonInvocation {
        argv,
        home,
        command,
        worker_cmd,
        report,
        stdout,
        stderr,
    } = invocation;

    // Answered before any subcommand is resolved, so `daemon --help` never reaches a lock or a bind.
    if argv.iter().any(|arg| arg == "--help" || arg == "-h") {
        let _ = writeln!(stdout, "{USAGE}");
        return 0;
    }

    // The global flags leave the argv before the subcommand is resolved, so `daemon status --json` is
    // an acceptance rather than a stray flag. A value flag with nothing to consume is refused here
    // with this verb's own block rather than the tool one the dispatcher was written for.
    let Globals { rest, json } = match strip_globals(argv) {
        Ok(globals) => globals,
        Err(reason) => return refuse(stderr, &reason),
    };
    let Some((verb, rest)) = rest.split_first() else {
        return refuse(stderr, "daemon needs a subcommand");
    };
    match verb.as_str() {
        "run" => run(rest, home, worker_cmd, report, stdout, stderr),
        "start" => start(rest, home, command, stdout, stderr),
        "stop" => stop(rest, home, stdout, stderr),
        "status" => status(rest, home, json, stdout, stderr),
        other => refuse(stderr, &format!("unknown subcommand \"{other}\"")),
    }
}

/// The foreground process. `home` comes from the caller's config and nowhere else, so the pid file
/// this acquires is the one `start`, `stop` and `status` read.
///
/// Two refusals are handled here because the process is long-lived and a stack trace would be the
/// whole answer: a bad `--port`/`--host` is a usage refusal (exit 2), and a pid file another daemon
/// holds is the composition's refusal (exit 1, logged by [`crate::main`]). Both leave the home as
/// they found it — the composition takes the lock before it does anything else.
fn run(
    rest: &[String],
    home: &Path,
    worker_cmd: Vec<OsString>,
    report: Option<Box<dyn ReportServer>>,
    _stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if let Some(flag) = unknown_flag(rest, &["port", "host"]) {
        return refuse(stderr, &format!("\"daemon run\" takes no {flag}"));
    }

    let raw_port = flag_value(rest, "port");
    let port = match raw_port {
        Some(raw) => match parse_port(&raw) {
            Some(port) => Some(port),
            None => {
                return refuse(
                    stderr,
                    &format!("--port must be an integer in 0..65535, not \"{raw}\""),
                );
            }
        },
        None if rest.iter().any(|token| token == "--port") => {
            return refuse(stderr, "--port needs a value");
        }
        None => None,
    };

    let daemon = read_global_config(home).daemon;
    let host = match flag_value(rest, "host") {
        Some(host) if host.is_empty() => return refuse(stderr, "--host needs a value"),
        Some(host) => host,
        None if rest.iter().any(|token| token == "--host") => {
            return refuse(stderr, "--host needs a value");
        }
        None => daemon.host,
    };

    // Flag, then the machine file, then the report server's built-in loopback default. `start` passes
    // neither flag, so the child resolves the same file and the two cannot disagree.
    let bind = BindAddress {
        host,
        port: port.unwrap_or(daemon.port),
    };
    // A caller that named a server substitutes it; otherwise `run` serves, exactly as the TypeScript
    // does. The adapter is built over `home` and nowhere else, so the page lists the registry the lock
    // beside it protects.
    let report: Box<dyn ReportServer> =
        report.unwrap_or_else(|| Box::new(TelemetryReport::new(home.to_path_buf())));
    let options = ForegroundOptions {
        home: home.to_path_buf(),
        worker_cmd,
        interval_ms: None,
        report: Some(report),
        bind,
        watchers: None,
        on_tick: None,
        stop: None,
    };
    // Nothing is written to stdout here: the daemon's own lines are log lines on stderr, and the
    // process ends with the code the loop answered.
    run_daemon_foreground(options)
}

/// The report server `run` binds: `sangkar-telemetry`'s `serve` over this home, which is the same
/// page `telemetry --serve` renders — one loopback listener over the machine registry, recomputed per
/// request.
///
/// The adapter exists because the composition states three calls and `serve` answers with its own
/// handle; this is the only place the two shapes meet. `home` is the home the daemon holds the lock
/// in, never a second resolution of it: the registry the page lists and the pid file the lock protects
/// have to describe one machine.
///
/// `serve` owns the policy this adapter must not repeat: loopback-only hosts, `port: 0` asking the
/// kernel, and the wait in `close` that makes a closed port refuse before this returns.
pub struct TelemetryReport {
    home: PathBuf,
    /// The bound server, once [`ReportServer::start`] answered. It closes on `Drop`, so a start that
    /// succeeds and a `close` a caller never makes still leave no socket behind.
    server: Option<serve::ReportServer>,
}

impl TelemetryReport {
    /// The report over `home`, bound when the composition reaches its third step.
    #[must_use]
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self {
            home: home.into(),
            server: None,
        }
    }
}

impl ReportServer for TelemetryReport {
    fn start(&mut self, host: &str, port: u16) -> Result<ReportAddress, AppError> {
        // Everything `daemon run` does not name keeps `serve`'s own default: no window (`--since`
        // belongs to `telemetry --serve`) and the system clock.
        let options = ServeOptions {
            host: Some(host),
            port: Some(port),
            ..ServeOptions::new(&self.home)
        };
        let server = serve::start(options).map_err(|error| report_refusal(&error))?;
        // Read before the handle moves into the field: `port: 0` is only an address once the kernel
        // answered, and `serve` has already read that answer back.
        let address = ReportAddress {
            url: server.url().to_owned(),
            host: server.host().to_owned(),
            port: server.port(),
        };
        self.server = Some(server);
        Ok(address)
    }

    fn close(&mut self) {
        // Safe after a refused start: there is nothing to close, and the shutdown runs either way.
        if let Some(mut server) = self.server.take() {
            server.close();
        }
    }
}

/// A refused bind as the composition's [`AppError`].
///
/// `internal` is the closed union's honest answer here: none of the twelve codes names a socket the
/// machine refused or a host outside the loopback allow-list, and inventing one would be a contract
/// change. The message is `serve`'s own, because the half an operator acts on — the host, the port and
/// the operating system's reason — is exactly what it already spells. The composition logs it and
/// answers exit 1, the same code the TypeScript's `serveReport` rejection reaches `main.ts` with.
fn report_refusal(error: &ServeError) -> AppError {
    AppError::internal(error.to_string())
}

/// A detached start: the child's pid is read back from the pid file it wrote, not from the spawn.
fn start(
    rest: &[String],
    home: &Path,
    command: Vec<OsString>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if let Some(flag) = unknown_flag(rest, &[]) {
        return refuse(stderr, &format!("\"daemon start\" takes no {flag}"));
    }

    let options = DaemonOptions {
        command,
        ..DaemonOptions::new(home.to_path_buf())
    };
    let outcome = lifecycle::start(&options);
    if outcome.code != 0 {
        // The lifecycle logged the reason; a second line on stdout would be a claim it did not make.
        return EXIT_FAILURE;
    }
    match outcome.pid {
        Some(pid) => {
            let _ = writeln!(stdout, "started (pid {pid})");
            0
        }
        None => fail(stderr, "the daemon reported a start but wrote no pid file"),
    }
}

/// A stop is only announced once the pid file is gone — [`lifecycle::stop`] waits for exactly that —
/// and the pid printed is the one read before the signal, because the file that held it is gone.
fn stop(rest: &[String], home: &Path, stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    if let Some(flag) = unknown_flag(rest, &[]) {
        return refuse(stderr, &format!("\"daemon stop\" takes no {flag}"));
    }

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
            return EXIT_FAILURE;
        }
    };
    let Some(pid) = state.pid.filter(|_| state.held) else {
        let _ = writeln!(stdout, "not running");
        return EXIT_FAILURE;
    };
    if lifecycle::stop(home).code != 0 {
        return EXIT_FAILURE;
    }
    let _ = writeln!(stdout, "stopped (pid {pid})");
    0
}

/// The answer an operator reads first, and the exit code `stop` would give: 0 only while running.
///
/// The plain form keeps the TypeScript's lines byte for byte — the repository's own settings come
/// from its own file again here, because `DaemonStatus` carries only what the daemon recorded — and
/// `--json` prints the `DaemonStatus` object as one line plus a newline.
fn status(
    rest: &[String],
    home: &Path,
    json: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> i32 {
    if let Some(flag) = unknown_flag(rest, &[]) {
        return refuse(stderr, &format!("\"daemon status\" takes no {flag}"));
    }

    let answer: DaemonStatus = lifecycle::status(home);
    let running = answer.pid.is_some();
    if json {
        match serde_json::to_string(&answer) {
            Ok(line) => {
                let _ = writeln!(stdout, "{line}");
            }
            Err(error) => {
                return fail(
                    stderr,
                    &format!("the status could not be rendered as JSON: {error}"),
                );
            }
        }
    } else {
        let defaults = repo_defaults_of(&read_global_config(home));
        let mut lines = vec![match answer.pid {
            Some(pid) => format!("running, pid {pid}"),
            None => "not running".to_owned(),
        }];
        for row in &answer.repositories {
            let config = read_repo_config(Path::new(&row.root), None, defaults);
            lines.push(format!(
                "  {}  watch={} interval={}ms telemetry={}",
                row.root,
                on_off(row.watch),
                config.watch_interval_ms,
                on_off(config.telemetry)
            ));
        }
        let _ = writeln!(stdout, "{}", lines.join("\n"));
    }
    if running { 0 } else { EXIT_FAILURE }
}

/// The global flags, removed: what the subcommand still reads, and whether `--json` was asked for.
struct Globals {
    rest: Vec<String>,
    json: bool,
}

/// Strips `--json` and the two value flags whose values this verb never reads.
///
/// A value flag consumes the token after it, exactly as the TypeScript's `splitGlobals` does, and a
/// dangling one is the refusal the caller prints with this verb's block.
fn strip_globals(argv: &[String]) -> Result<Globals, String> {
    let mut rest = Vec::with_capacity(argv.len());
    let mut json = false;
    let mut index = 0;
    while let Some(token) = argv.get(index) {
        index += 1;
        if token == "--json" {
            json = true;
            continue;
        }
        let name = token.strip_prefix("--").unwrap_or_default();
        let (flag, inline) = match name.split_once('=') {
            Some((flag, value)) => (flag, Some(value)),
            None => (name, None),
        };
        if flag != "repo-root" && flag != "data-dir" {
            rest.push(token.clone());
            continue;
        }
        if inline.is_some() {
            continue;
        }
        if argv.get(index).is_none() {
            return Err(format!("--{flag} needs a value"));
        }
        index += 1;
    }
    Ok(Globals { rest, json })
}

/// The first flag a subcommand does not read, or `None`. `run` reads `--port` and `--host`; the other
/// three read none, and a silently ignored `--port` on `start` would look like it had been applied. A
/// flag's value is a bare token and is never mistaken for a flag name.
fn unknown_flag(argv: &[String], allowed: &[&str]) -> Option<String> {
    argv.iter()
        .find(|token| {
            let Some(name) = token.strip_prefix("--") else {
                return false;
            };
            !allowed.contains(&name.split('=').next().unwrap_or_default())
        })
        .cloned()
}

/// A `--port` value: a non-empty run of digits that fits a `u16`. Everything else is the refusal.
fn parse_port(raw: &str) -> Option<u16> {
    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// The spelling both human answers use for a settings switch.
fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

/// The reason first, then the block, so the last thing a caller sees is how to call it right.
fn refuse(stderr: &mut dyn Write, reason: &str) -> i32 {
    let _ = writeln!(stderr, "{reason}\n{USAGE}");
    EXIT_USAGE
}

/// A failure with no usage to offer: the verb was called correctly and the daemon or fs refused.
fn fail(stderr: &mut dyn Write, reason: &str) -> i32 {
    log::error(
        "daemon.cli_refused",
        &[("message", Value::String(reason.to_owned()))],
    );
    let _ = writeln!(stderr, "sangkar daemon: {reason}");
    EXIT_FAILURE
}
