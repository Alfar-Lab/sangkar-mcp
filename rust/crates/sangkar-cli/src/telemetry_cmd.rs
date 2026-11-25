//! `sangkar telemetry`: one verb, one file, one line of stdout — or, with `--serve`, one listener.
//!
//! Ported from `src/shared/telemetry/cli.ts`. Everything but the seams lives elsewhere — [`sangkar_telemetry::report`]
//! reads and folds, [`sangkar_telemetry::html`] renders, [`sangkar_telemetry::serve`] binds and
//! answers — so this module only resolves the flags, names the output, and decides which of the exits
//! the run takes. There is no graph, no reader and no store here: a report is built from the telemetry
//! directory alone, so it answers on a tree that was never indexed.
//!
//! The four global flags are accepted here: `--repo-root` and `--data-dir` are what
//! [`sangkar_core::config::load_config`] turns into the tree and the directory this report describes,
//! and `--json` is accepted and inert because the answer of this verb is a file or a page, never a
//! machine document.
//!
//! # The answer is returned, not printed
//!
//! stdout is the two lines below and nothing else, and the workspace bans the print macros outside
//! tests, so the decision and the writing are two steps: the flag reader decides, the functions below
//! write to the handles their caller owns. The binary passes the process's streams; a test passes two
//! `Vec<u8>` and reads the bytes back, so a test cannot miss a line the binary prints.
//!
//! # Serving blocks, on purpose
//!
//! `--serve` binds, announces, and waits for `SIGINT` or `SIGTERM`: a verb that returned after the
//! announcement would take the listener with it. The wait is injected ([`run_telemetry_with`]) because
//! a suite must not wait for a signal nobody will send, and because the daemon's own shutdown is the
//! same seam spelled differently.

use std::io::{self, Write};
use std::path::Path;

use sangkar_core::config::{self, AppConfig, Env};
use sangkar_core::log;
use sangkar_telemetry::html::render_html;
use sangkar_telemetry::report::{Since, parse_since, read_records};
use sangkar_telemetry::serve::{self, ReportServer, ServeError, ServeOptions};

use crate::args::{CliUsageError, Verb, parse_verb, verb_spec};
use crate::render::{EXIT_FAILURE, EXIT_USAGE};

/// The name of the report a run with no `--out` writes, inside `<data-dir>/telemetry`.
const REPORT_FILE: &str = "report.html";

/// One `sangkar telemetry` invocation: the tokens *after* the verb name (which is the tail
/// [`crate::args::resolve_command`] answers with), the machine's environment and working directory,
/// and the two streams the caller owns. Answers the code the process exits with.
pub fn run_telemetry(
    argv: &[String],
    env: Env<'_>,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    run_telemetry_with(argv, env, cwd, out, err, &wait_for_signal)
}

/// [`run_telemetry`] with the wait injected: `hold` is called once the listener is announced, and the
/// server is closed when it returns.
///
/// That is the whole difference between the binary and a test — the production wait blocks on a
/// signal, a test's wait reads the URL it was just handed and returns — so no case has to send a
/// signal to this process to prove the verb answers.
fn run_telemetry_with(
    argv: &[String],
    env: Env<'_>,
    cwd: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
    hold: &dyn Fn(&ReportServer),
) -> i32 {
    let invocation = match parse_verb(Verb::Telemetry, argv) {
        Ok(invocation) => invocation,
        Err(error) => return usage_refusal(err, &error),
    };
    let args = &invocation.args;

    // The window first, because a `--since` this process cannot read would otherwise be discovered
    // after a directory was created or a port was bound.
    let now = serve::system_now();
    let stamp = serve::iso_of(now);
    let window = match args.text("since") {
        Some(raw) => match parse_since(raw, now) {
            Ok(window) => Some(window),
            Err(error) => return refuse(err, &error.to_string()),
        },
        None => None,
    };
    let since = window.as_ref().map(Since::raw);

    // `--port` is declared an integer, so a typo is refused by the table; the range is this verb's
    // rule, and `0` is in it — it is how every test and every supervised run asks for a free port.
    let port = match args.integer("port") {
        Some(number) => match u16::try_from(number) {
            Ok(port) => Some(port),
            Err(_) => {
                return refuse(
                    err,
                    &format!("--port must be an integer in 0..65535, not \"{number}\""),
                );
            }
        },
        None => None,
    };

    // The two answers are exclusive: one run either writes a file or answers on a socket, and a
    // caller that named both has not decided which one it wants.
    let serving = args.switch("serve");
    if serving && args.text("out").is_some() {
        return refuse(
            err,
            "--serve answers over http, so it takes no --out; drop one of the two",
        );
    }

    let config = match config::load_config(argv, env, cwd) {
        Ok(config) => config,
        Err(error) => {
            // A startup failure is argv-shaped input, reported for whichever command line it came
            // from, so `--repo-root` pointing at a tree this process cannot read is exit 2.
            log::error(&format!("startup failed: {}", error.message), &[]);
            return EXIT_USAGE;
        }
    };

    if serving {
        // Flag, then the machine file, then `serve.rs`'s own default: this verb is what an operator
        // runs by hand, so it must agree with what `daemon run` would bind.
        let host = args
            .text("host")
            .map_or_else(|| config.daemon.host.clone(), str::to_owned);
        let port = port.unwrap_or(config.daemon.port);
        return serve_foreground(&config.home, &host, port, since, out, err, hold);
    }

    // The label the page prints, owned here because the window itself moves into the read below.
    let label = window.as_ref().map(|window| window.raw().to_owned());
    write_report(
        &config,
        args.text("out"),
        window,
        label.as_deref(),
        &stamp,
        out,
        err,
    )
}

/// The offline path: fold the directory, render the document, write it, say where it went.
fn write_report(
    config: &AppConfig,
    raw_out: Option<&str>,
    window: Option<Since>,
    since: Option<&str>,
    stamp: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // Same rule as `--data-dir`: a relative path is the caller's spelling of "inside this tree", so
    // it resolves against `repo_root` rather than against the process cwd.
    let target = match raw_out {
        Some(raw) => config::resolve_path(&config.repo_root, Path::new(raw)),
        None => config.telemetry.dir.join(REPORT_FILE),
    };
    // The read folds in the same call, so the counts the line names and the numbers the page prints
    // are one pass over the directory rather than two that could disagree.
    let read = read_records(&config.telemetry.dir, window);
    let html = render_html(&read.data, stamp, since);

    if let Some(parent) = target.parent()
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return write_failure(err, &target, &error);
    }
    if let Err(error) = std::fs::write(&target, &html) {
        return write_failure(err, &target, &error);
    }

    let line = format!(
        "wrote {} ({} records, {} unparsed lines)\n",
        target.display(),
        read.data.totals.records,
        read.unparsed
    );
    match out.write_all(line.as_bytes()) {
        Ok(()) => 0,
        // The answer never reached the caller, and the code says so. `--out` is still written: the
        // file is this run's product, and a closed stdout does not unmake it.
        Err(_) => EXIT_FAILURE,
    }
}

/// The serve path: bind, announce, hold, close.
///
/// The announcement is the only stdout this path writes, because it is the only thing a caller needs
/// before it can open the page; everything the server logs on its own goes to stderr through
/// [`sangkar_core::log`].
fn serve_foreground(
    home: &Path,
    host: &str,
    port: u16,
    since: Option<&str>,
    out: &mut dyn Write,
    err: &mut dyn Write,
    hold: &dyn Fn(&ReportServer),
) -> i32 {
    let options = ServeOptions {
        home,
        host: Some(host),
        port: Some(port),
        since,
        now: serve::system_now,
    };
    let mut server = match serve::start(options) {
        Ok(server) => server,
        Err(error) => return serve_refusal(err, &error),
    };

    let line = format!(
        "serving {} ({} repositories)\n",
        server.url(),
        server.repositories()
    );
    // Flushed before the wait: stdout is block-buffered when it is a pipe, and a supervisor that
    // reads the line to learn the port would otherwise wait for a process that is waiting for it.
    if out
        .write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        // The announcement did not land, and a listener nobody can find is not a run: `server` is
        // dropped here, which closes it.
        return EXIT_FAILURE;
    }

    hold(&server);
    server.close();
    0
}

/// The verb serves until it is stopped, and either of the two signals stops it: `SIGINT` is the
/// terminal, `SIGTERM` is whatever runs the verb as a service. The listener is the argument because
/// the wait starts after it was announced, not because this reading needs its address.
fn wait_for_signal(_server: &ReportServer) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        // No runtime and no signal stream: a listener that was announced and cannot be stopped is
        // worse than a run that ends, so the verb returns and `serve_foreground` closes it.
        log::error(
            "telemetry --serve could not wait for a signal",
            &[(
                "message",
                serde_json::Value::String("no async runtime for the signal stream".to_owned()),
            )],
        );
        return;
    };
    runtime.block_on(stop_on_signal());
}

/// The two termination signals, streamed by `tokio`'s process-global driver. The shape — install
/// once, warn and carry on when a stream cannot be installed — is the daemon lifecycle's, so a
/// machine that refuses one handler still stops this verb with the other.
async fn stop_on_signal() {
    let mut interrupt = install_signal(tokio::signal::unix::SignalKind::interrupt(), "SIGINT");
    let mut terminate = install_signal(tokio::signal::unix::SignalKind::terminate(), "SIGTERM");
    tokio::select! {
        () = next_signal(&mut interrupt) => {}
        () = next_signal(&mut terminate) => {}
    }
}

fn install_signal(
    kind: tokio::signal::unix::SignalKind,
    name: &str,
) -> Option<tokio::signal::unix::Signal> {
    match tokio::signal::unix::signal(kind) {
        Ok(stream) => Some(stream),
        Err(error) => {
            log::warn(
                "telemetry --serve could not trap a signal",
                &[
                    ("signal", serde_json::Value::String(name.to_owned())),
                    ("message", serde_json::Value::String(error.to_string())),
                ],
            );
            None
        }
    }
}

/// Waits for the next arrival on a stream, or forever when it could not be installed: an absent
/// signal is a branch that never wins the `select!`, not a busy loop.
async fn next_signal(stream: &mut Option<tokio::signal::unix::Signal>) {
    match stream {
        Some(stream) => {
            stream.recv().await;
        }
        None => std::future::pending().await,
    }
}

/// A refusal of the argv: the reason, then the block that says how the command is spelled.
fn refuse(err: &mut dyn Write, reason: &str) -> i32 {
    let _ = writeln!(err, "{reason}\n{}", verb_spec(Verb::Telemetry).usage);
    EXIT_USAGE
}

fn usage_refusal(err: &mut dyn Write, error: &CliUsageError) -> i32 {
    let _ = writeln!(err, "{}\n{}", error.message, error.usage);
    EXIT_USAGE
}

/// The serve path's two failures, which are not the same answer: a host or window the caller can
/// spell differently is a usage refusal (exit 2, with the block), and a socket the machine refused is
/// a run failure (exit 1, the reason alone) — no spelling of the command would have bound it.
fn serve_refusal(err: &mut dyn Write, error: &ServeError) -> i32 {
    match error {
        ServeError::HostNotLoopback { .. } | ServeError::Since { .. } => {
            refuse(err, &error.to_string())
        }
        ServeError::Bind { .. } => {
            let _ = writeln!(err, "sangkar telemetry: {error}");
            EXIT_FAILURE
        }
    }
}

fn write_failure(err: &mut dyn Write, target: &Path, error: &io::Error) -> i32 {
    let _ = writeln!(
        err,
        "sangkar telemetry: could not write {}: {error}",
        target.display()
    );
    EXIT_FAILURE
}

#[cfg(test)]
mod tests {
    //! The verb on a stated machine: a temporary home, a temporary tree and two buffers, so the suite
    //! never reads the account's own `~/.config/sangkar/` nor this repository's `.sangkar/`.
    //!
    //! What is asserted is what a caller sees: the two lines stdout may carry, the refusal stderr
    //! prints, the code the process would exit with, and the file the run left behind. The listener
    //! itself is `sangkar-telemetry/tests/serve.rs`'s subject; here it is only proved that the verb
    //! announces the URL it bound, answers on it, and closes it before returning.

    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::run_telemetry_with;

    /// The eight panel ids `html.rs` gives the report, named rather than counted.
    const PANEL_IDS: [&str; 8] = [
        "tools",
        "truncation",
        "relations",
        "coverage",
        "argument-names",
        "trend",
        "index-runs",
        "sessions",
    ];

    const FIXED_TS: &str = "2026-11-18T10:00:00.000Z";

    /// One call line, as the recorder would have written it.
    fn call_line(seq: u32, tool: &str) -> String {
        serde_json::json!({
            "ts": FIXED_TS,
            "v": 1,
            "kind": "call",
            "install_id": "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f",
            "session_id": "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a",
            "seq": seq,
            "tool": tool,
            "status": "ok",
            "error_code": null,
            "duration_ms": 10,
            "arg_names_set": ["budget"],
            "budget_tokens": 1_000,
            "payload_tokens": 100,
            "budget_used_ratio": 0.1,
            "truncated": false,
            "truncation_reason": null,
            "total_relation": "gte",
            "coverage_complete": true,
            "coverage_indexed": 1,
            "coverage_skipped": 0,
            "coverage_failed": 0,
            "unresolved_edges": 0,
            "stale": false,
            "stale_reason": null,
            "generation_age_ms": null,
            "result_counts": {},
            "server_version": "0.6.0-beta.1",
            "bun_version": "1.4.4",
            "os": "linux",
            "arch": "x64",
            "graph_schema_version": 1,
        })
        .to_string()
    }

    /// A temporary directory that removes itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let unique = format!(
                "sangkar-telemetry-cmd-{label}-{}-{nanos}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed)
            );
            let path = std::env::temp_dir().join(unique);
            fs::create_dir_all(&path).expect("create the temp directory");
            Self(fs::canonicalize(&path).expect("canonicalise the temp directory"))
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn dir(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::create_dir_all(&path).expect("create a directory");
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// One machine: a temporary home (`SANGKAR_HOME` and `$HOME` at once) and one tree, so nothing a
    /// run resolves can reach the account's own installation.
    struct Machine {
        root: TempDir,
        tree: PathBuf,
    }

    impl Machine {
        fn new(label: &str) -> Self {
            let root = TempDir::new(label);
            let tree = root.dir("tree");
            // A `.git` marker stops autodetection here instead of walking out of the temporary root.
            fs::create_dir_all(tree.join(".git")).expect("mark the tree");
            Self { root, tree }
        }

        fn home(&self) -> &Path {
            self.root.path()
        }

        fn tree(&self) -> &Path {
            &self.tree
        }

        /// The tree's data directory, holding `lines` in one daily file.
        fn telemetry(&self, lines: &[String]) -> PathBuf {
            let data = self.root.dir("data");
            let dir = data.join("telemetry");
            fs::create_dir_all(&dir).expect("create the telemetry directory");
            fs::write(
                dir.join("2026-11-18.jsonl"),
                format!("{}\n", lines.join("\n")),
            )
            .expect("write the daily file");
            data
        }

        /// One invocation in `elsewhere`, with `--repo-root` and `--data-dir` stated so no path is
        /// guessed, and a hold that answers immediately.
        fn run(&self, argv: &[&str]) -> Run {
            self.run_holding(argv, &|_server| {})
        }

        /// One invocation with the serve wait injected: `hold` is called once the listener is
        /// announced, and the server is closed when it returns.
        fn run_holding(&self, argv: &[&str], hold: &dyn Fn(&super::ReportServer)) -> Run {
            let vars: HashMap<&'static str, String> = HashMap::from([
                ("SANGKAR_HOME", self.home().display().to_string()),
                ("HOME", self.home().display().to_string()),
            ]);
            let env = |name: &str| vars.get(name).cloned();
            let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            let code = run_telemetry_with(&argv, &env, self.tree(), &mut stdout, &mut stderr, hold);
            Run {
                code,
                stdout: String::from_utf8(stdout).expect("stdout is UTF-8"),
                stderr: String::from_utf8(stderr).expect("stderr is UTF-8"),
            }
        }
    }

    /// What one invocation answered.
    struct Run {
        code: i32,
        stdout: String,
        stderr: String,
    }

    // ---------------------------------------------------------------------------------------
    // The report
    // ---------------------------------------------------------------------------------------

    #[test]
    fn the_report_is_written_and_the_line_names_it() {
        let machine = Machine::new("write");
        let data = machine.telemetry(&[
            call_line(1, "status"),
            call_line(2, "search"),
            "{not json".to_owned(),
        ]);
        let tree = machine.tree().display().to_string();
        let dir = data.display().to_string();

        // `--json` is a global this verb accepts and ignores: the answer is a file, not a document.
        let run = machine.run(&["--json", "--repo-root", &tree, "--data-dir", &dir]);

        let report = data.join("telemetry").join("report.html");
        assert_eq!(run.code, 0);
        assert_eq!(
            run.stdout,
            format!("wrote {} (2 records, 1 unparsed lines)\n", report.display())
        );
        assert_eq!(
            run.stderr, "",
            "a run that answered writes nothing to stderr"
        );

        let written = fs::read_to_string(&report).expect("read the report");
        for id in PANEL_IDS {
            assert!(
                written.contains(&format!("id=\"{id}\"")),
                "the report has the {id} panel"
            );
        }
        // The header states the window this run folded, which no `--since` narrowed.
        assert!(written.contains("window <strong>all records</strong>"));
    }

    #[test]
    fn out_names_the_file_and_is_resolved_against_the_tree() {
        let machine = Machine::new("out");
        let data = machine.telemetry(&[call_line(1, "status")]);
        let tree = machine.tree().display().to_string();
        let dir = data.display().to_string();

        let run = machine.run(&[
            "--repo-root",
            &tree,
            "--data-dir",
            &dir,
            "--out",
            "report.html",
        ]);

        let report = machine.tree().join("report.html");
        assert_eq!(run.code, 0);
        assert_eq!(
            run.stdout,
            format!("wrote {} (1 records, 0 unparsed lines)\n", report.display())
        );
        assert!(report.exists());
    }

    #[test]
    fn an_unreadable_since_is_refused_and_writes_nothing() {
        let machine = Machine::new("since");
        // A path nobody created yet: `--data-dir` names where the report would be written, and a
        // refusal must not be the thing that brings the directory into being.
        let data_dir = machine.root.path().join("data");
        let tree = machine.tree().display().to_string();
        let dir = data_dir.display().to_string();

        let run = machine.run(&[
            "--since",
            "nonsense",
            "--repo-root",
            &tree,
            "--data-dir",
            &dir,
        ]);

        assert_eq!(run.code, 2);
        assert_eq!(run.stdout, "");
        assert!(
            run.stderr
                .contains("--since must be YYYY-MM-DD or <n>d, not \"nonsense\""),
            "{}",
            run.stderr
        );
        assert!(run.stderr.contains("usage: sangkar telemetry"));
        assert!(!data_dir.exists(), "a refusal creates no directory");
    }

    // ---------------------------------------------------------------------------------------
    // The refusals
    // ---------------------------------------------------------------------------------------

    #[test]
    fn serve_and_out_are_mutually_exclusive() {
        let machine = Machine::new("exclusive");
        let tree = machine.tree().display().to_string();
        let out = machine.root.path().join("report.html");
        let out_text = out.display().to_string();

        let run = machine.run(&["--serve", "--out", &out_text, "--repo-root", &tree]);

        assert_eq!(run.code, 2);
        assert_eq!(run.stdout, "");
        assert!(run.stderr.contains("--serve"), "{}", run.stderr);
        assert!(run.stderr.contains("--out"), "{}", run.stderr);
        assert!(run.stderr.contains("usage: sangkar telemetry"));
        // The block is where an operator learns the shape of the option it collided with.
        assert!(run.stderr.contains("--port <n>"));
        assert!(run.stderr.contains("--host <addr>"));
        assert!(!out.exists(), "neither answer was attempted");
    }

    #[test]
    fn a_non_loopback_host_is_a_usage_refusal() {
        let machine = Machine::new("host");
        let tree = machine.tree().display().to_string();

        let run = machine.run(&[
            "--serve",
            "--port",
            "0",
            "--host",
            "0.0.0.0",
            "--repo-root",
            &tree,
        ]);

        assert_eq!(run.code, 2);
        assert_eq!(run.stdout, "");
        assert!(run.stderr.contains("loopback only"), "{}", run.stderr);
        assert!(run.stderr.contains("0.0.0.0"), "{}", run.stderr);
        assert!(run.stderr.contains("usage: sangkar telemetry"));
        // A usage refusal, not a crash: a bind attempt that panicked would leave a frame behind.
        assert!(!run.stderr.contains("panicked"), "{}", run.stderr);
    }

    #[test]
    fn a_port_outside_the_range_is_refused_with_its_value() {
        let machine = Machine::new("port");
        let tree = machine.tree().display().to_string();

        let run = machine.run(&["--serve", "--port", "70000", "--repo-root", &tree]);

        assert_eq!(run.code, 2);
        assert_eq!(run.stdout, "");
        assert!(
            run.stderr
                .contains("--port must be an integer in 0..65535, not \"70000\""),
            "{}",
            run.stderr
        );
        assert!(run.stderr.contains("usage: sangkar telemetry"));
    }

    // ---------------------------------------------------------------------------------------
    // The listener
    // ---------------------------------------------------------------------------------------

    #[test]
    fn serve_announces_the_url_it_answers_on_and_closes_it() {
        let machine = Machine::new("serve");
        let tree = machine.tree().display().to_string();
        let answer = RefCell::new(None);
        let hold = |server: &super::ReportServer| {
            *answer.borrow_mut() = Some(get(server.url()));
        };

        let run = machine.run_holding(&["--serve", "--port", "0", "--repo-root", &tree], &hold);

        // One line, matching `serving http://127.0.0.1:<port>/ (<n> repositories)`.
        let line = run.stdout.strip_suffix('\n').expect("one line, ended");
        assert!(!line.contains('\n'), "stdout is one line: {:?}", run.stdout);
        let rest = line
            .strip_prefix("serving http://127.0.0.1:")
            .expect("the announced URL");
        let (port, repositories) = rest.split_once('/').expect("the URL's path");
        let port: u16 = port.parse().expect("the bound port");
        assert!(port > 0, "port 0 is read back from the socket");
        assert_eq!(repositories, " (0 repositories)");

        assert_eq!(run.code, 0);
        assert_eq!(run.stderr, "", "the announcement is stdout's only line");

        let (status, content_type, body) = answer
            .into_inner()
            .expect("the listener answered while it was held");
        assert_eq!(status, 200);
        assert!(
            content_type.starts_with("text/html"),
            "content-type: {content_type}"
        );
        // An empty registry is still a page with every panel, which is what makes the marker check
        // meaningful on a served body rather than on a fixture's copy.
        for id in PANEL_IDS {
            assert!(body.contains(&format!("id=\"{id}\"")), "the page has {id}");
        }

        let address: SocketAddr = format!("127.0.0.1:{port}")
            .parse()
            .expect("an address from the announcement");
        let refused = TcpStream::connect_timeout(&address, Duration::from_secs(2));
        assert!(
            refused.is_err(),
            "the port is still answering after the verb returned: {refused:?}"
        );
    }

    /// One request and its whole answer: `Connection: close` is what makes the read terminate.
    fn get(url: &str) -> (u16, String, String) {
        let authority = url.trim_start_matches("http://").trim_end_matches('/');
        let mut stream = TcpStream::connect(authority).expect("connect to the verb's listener");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set a read timeout");
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
        )
        .expect("write the request");
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).expect("read the answer");

        let head_end = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|at| at + 4)
            .expect("the answer has headers");
        let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|code| code.parse().ok())
            .expect("a status line");
        let fields: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(field, value)| (field.to_ascii_lowercase(), value.trim().to_owned()))
            .collect();
        let content_type = fields
            .iter()
            .find(|(field, _value)| field == "content-type")
            .map(|(_field, value)| value.clone())
            .unwrap_or_default();
        let chunked = fields
            .iter()
            .any(|(field, value)| field == "transfer-encoding" && value.contains("chunked"));
        let body = if chunked {
            dechunk(&raw[head_end..])
        } else {
            raw[head_end..].to_vec()
        };
        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    /// `tiny_http` chunks a body of 32 KiB or more, and this page is a stylesheet away from it.
    fn dechunk(raw: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        let mut rest = raw;
        while let Some(at) = rest.windows(2).position(|window| window == b"\r\n") {
            let size = String::from_utf8_lossy(&rest[..at]);
            let Ok(size) = usize::from_str_radix(size.split(';').next().unwrap_or("").trim(), 16)
            else {
                break;
            };
            if size == 0 || rest.len() < at + 2 + size {
                break;
            }
            body.extend_from_slice(&rest[at + 2..at + 2 + size]);
            rest = &rest[(at + 2 + size + 2).min(rest.len())..];
        }
        body
    }
}
