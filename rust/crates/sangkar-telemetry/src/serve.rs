//! The machine-wide report, served: one loopback listener that renders every registered repository's
//! telemetry directory on every request.
//!
//! Ported from `src/shared/telemetry/serve.ts`. Nothing here folds or renders: [`crate::report`]
//! reads and aggregates, [`crate::html_multi`] draws the page and [`sangkar_core::registry`] owns the
//! list of trees — this module is the seam that joins the three, plus the one policy a server has to
//! hold on its own: it binds loopback and nothing else, because a report of every repository on the
//! machine is not something the network gets to read.
//!
//! The document is recomputed per request rather than at bind time: a daemon serves for days, and a
//! page rendered once would stop mentioning the records written after it started.
//!
//! # Why `tiny_http`
//!
//! `Bun.serve` has no runtime to hand this crate, and every caller in this workspace is a
//! synchronous CLI branch, so a blocking server on one thread is the shape that fits. `tiny_http` is
//! the package `RUST-MIGRATION.md` §5 names for exactly that shape; `hyper` is the other sanctioned
//! name and would bring an async runtime, a service abstraction and its own package tree for three
//! routes that each write one string.
//!
//! # One thread, on purpose
//!
//! Requests are answered one at a time by the thread that owns the listener. A report is one read of
//! a directory tree per rendered page, and the page is loopback-only: a second thread would buy
//! concurrency a local browser tab cannot use, and cost the one thing that matters here — that
//! `close` is a single, observable shutdown.
//!
//! # Closing
//!
//! [`ReportServer::close`] is the guarantee a test relies on: it stops the accept loop, drops the last
//! handle to the listener — dropping `tiny_http::Server` is what closes the socket — and then waits
//! for a connect to that port to be refused, so a caller that closes and immediately rebinds cannot
//! race a socket the kernel still holds. [`Drop`] calls it, so a path that forgets to close still
//! leaves no listener behind.

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use sangkar_core::config;
use sangkar_core::log;
use sangkar_core::registry::{self, Registry};

use crate::html_multi::{ReportSection, render_multi_page};
use crate::report::{
    ReportData, Since, TelemetryRecord, aggregate_with, parse_since, read_records,
};

/// A default, never a promise: a caller that passes `0` gets an ephemeral port instead, which is what
/// every test does so no case can collide with a real daemon or with the other cases.
pub const DEFAULT_REPORT_PORT: u16 = sangkar_core::global_config::DEFAULT_REPORT_PORT;

/// The address a caller that names no host gets, which is also the machine file's default.
pub const DEFAULT_HOST: &str = sangkar_core::global_config::DEFAULT_REPORT_HOST;

/// The whole allow-list. `localhost` is here because it is the spelling a person types; a hostname
/// that resolves to loopback is not, because the check would then have to be a DNS lookup whose
/// answer can change between the check and the bind.
pub const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];

/// The machine registry, spelled as `sangkar_core` and the daemon spell it.
const REGISTRY_FILE: &str = "sangkar.json";

/// The directory under a registry root the recorder writes, and the one directory inside it that a
/// report reads. The roots come from the machine registry, so the data directory is the per-tree
/// default rather than whatever `--data-dir` named for the process: a report of every repository is
/// not a report of one tree's relocated store.
const TELEMETRY_DIR: &str = "telemetry";

const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";
const TEXT_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

/// How long the accept loop waits for a request before it re-reads its stop flag. `close` wakes the
/// wait through `Server::unblock`, so this only bounds a wakeup that was lost.
const POLL: Duration = Duration::from_millis(50);

/// The attempts, and the timeout each one gets, of the wait in [`ReportServer::close`]: a connect to
/// a loopback port nobody listens on is refused at once, so the first attempt normally answers.
const CLOSE_ATTEMPTS: u32 = 100;
const CLOSE_TIMEOUT: Duration = Duration::from_millis(2);

/// The three ways binding a report is refused or fails.
///
/// Not an [`sangkar_core::errors::AppError`]: the first two are the argv, which the CLI answers with
/// a usage block and exit 2, and the third is the machine refusing a socket, which is exit 1. The
/// caller maps each one, because only it knows what its own exit codes mean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServeError {
    /// A host outside [`LOOPBACK_HOSTS`]: decided from the string alone, before any socket opens.
    HostNotLoopback {
        /// What the caller asked for, so the refusal quotes it back.
        host: String,
    },
    /// A `--since` that is neither `YYYY-MM-DD` nor `Nd`, refused before the bind so no page is ever
    /// answered for a window nobody can name.
    Since {
        /// The value as the caller spelled it.
        raw: String,
    },
    /// The listener could not be bound: an address already in use, a port this process may not open,
    /// a host that does not resolve.
    Bind {
        host: String,
        port: u16,
        /// The operating system's answer, which is the half of the message a caller cannot derive.
        reason: String,
    },
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HostNotLoopback { host } => write!(
                formatter,
                "the report server binds loopback only: --host must be one of {}, not \"{host}\"",
                LOOPBACK_HOSTS.join(", ")
            ),
            Self::Since { raw } => write!(
                formatter,
                "--since must be YYYY-MM-DD or <n>d, not \"{raw}\""
            ),
            Self::Bind { host, port, reason } => {
                write!(formatter, "could not bind {host}:{port}: {reason}")
            }
        }
    }
}

impl std::error::Error for ServeError {}

/// The machine total and one section per registered repository, as one request renders them.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportSections {
    /// Every section's records in one fold, with the summed unparsed count: the panel no single
    /// repository can answer.
    pub machine: ReportData,
    /// One per registry root, in registry order.
    pub sections: Vec<ReportSection>,
}

/// The clock a page is rendered against.
///
/// Injected exactly as `now?: () => Date` is on the TypeScript's options, because this crate reads no
/// clock on its own: [`system_now`] is the production reading, and a test passes its own day.
pub type Clock = fn() -> DateTime<Utc>;

/// Everything one server needs, named rather than positional because five arguments read as five
/// arguments: what the TypeScript's `ServeOptions` interface carries, minus the logger `sangkar_core`
/// owns for the whole workspace.
#[derive(Clone, Copy, Debug)]
pub struct ServeOptions<'a> {
    /// The installation's home; `<home>/sangkar.json` is the registry. Never read from the environment.
    pub home: &'a Path,
    /// `None` takes [`DEFAULT_HOST`]; anything outside [`LOOPBACK_HOSTS`] is refused.
    pub host: Option<&'a str>,
    /// `None` takes [`DEFAULT_REPORT_PORT`]; `0` asks the kernel for one.
    pub port: Option<u16>,
    /// `YYYY-MM-DD` or `Nd`, spelled as `sangkar telemetry --since` spells it.
    pub since: Option<&'a str>,
    /// The reading every rendered page is dated and every `Nd` window is measured from.
    pub now: Clock,
}

impl<'a> ServeOptions<'a> {
    /// The defaults a caller that names nothing gets: loopback, [`DEFAULT_REPORT_PORT`], no window,
    /// and the system clock.
    #[must_use]
    pub fn new(home: &'a Path) -> Self {
        Self {
            home,
            host: None,
            port: None,
            since: None,
            now: system_now,
        }
    }
}

/// A bound listener and the address it answers on.
///
/// The shape `src/shared/telemetry/serve.ts` returns, minus the promise: `url` is the origin a caller
/// reads back after `port: 0`, `repositories` is the registry's count at bind time (the routes read
/// the registry again per request), and `close` releases the port.
pub struct ReportServer {
    url: String,
    host: String,
    port: u16,
    repositories: usize,
    stop: Arc<AtomicBool>,
    /// Taken by [`ReportServer::close`]: dropping the last handle is what closes the listening
    /// socket, and the accept thread the crate spawns only notices when it wakes.
    server: Option<Arc<Server>>,
    thread: Option<JoinHandle<()>>,
    /// The address the kernel actually bound, which is how `port: 0` is read back.
    bound: Option<SocketAddr>,
}

impl ReportServer {
    /// The URL the listener answers on (`http://<host>:<port>/`), which is how `port: 0` is read back.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The host as the caller asked for it, not as the socket resolved it.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port the kernel bound, which is `0`'s answer rather than `0`.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The registry's repository count at bind time.
    #[must_use]
    pub fn repositories(&self) -> usize {
        self.repositories
    }

    /// Stops the listener and waits for the port to be released.
    ///
    /// Idempotent, because three callers can reach it: the verb after its signal, the daemon's
    /// shutdown, and [`Drop`]. A second call finds the thread already joined and no socket to wait
    /// for.
    pub fn close(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.stop.store(true, Ordering::Relaxed);
        if let Some(server) = self.server.as_ref() {
            server.unblock();
        }
        // A thread that panicked while answering one request has already dropped its own half of the
        // listener; the socket below is closed either way, so the join's own error is not a failure
        // of this close.
        let _ = thread.join();
        self.server = None;
        if let Some(address) = self.bound.take() {
            wait_until_refused(address);
        }
    }
}

impl Drop for ReportServer {
    /// The last line of defence for the one invariant a listener owes the machine: a dropped server
    /// leaves no socket behind.
    fn drop(&mut self) {
        self.close();
    }
}

/// Binds the listener and answers with its origin.
///
/// The host is validated before anything binds, so a refusal leaves no socket behind and no
/// half-started server to close. `since` is validated here for the same reason: a window that cannot
/// be read is a refusal, not a page that quietly reports everything.
///
/// # Errors
///
/// [`ServeError::HostNotLoopback`], [`ServeError::Since`] or [`ServeError::Bind`].
pub fn start(options: ServeOptions<'_>) -> Result<ReportServer, ServeError> {
    let ServeOptions {
        home,
        host,
        port,
        since,
        now,
    } = options;
    let host = host.unwrap_or(DEFAULT_HOST);
    if !is_loopback_host(host) {
        return Err(ServeError::HostNotLoopback {
            host: host.to_owned(),
        });
    }
    if let Some(raw) = since
        && parse_since(raw, now()).is_err()
    {
        return Err(ServeError::Since {
            raw: raw.to_owned(),
        });
    }

    let repositories = read_registry(home).repos.len();
    let requested = port.unwrap_or(DEFAULT_REPORT_PORT);
    let listener = Server::http((host, requested)).map_err(|error| ServeError::Bind {
        host: host.to_owned(),
        port: requested,
        reason: error.to_string(),
    })?;
    // Read back before the handle is shared: `port: 0` is only an address once the kernel answers.
    let bound = listener.server_addr().to_ip();
    let bound_port = bound.map_or(requested, |address| address.port());

    let listener = Arc::new(listener);
    let stop = Arc::new(AtomicBool::new(false));
    let thread = spawn_accept_loop(
        Arc::clone(&listener),
        home.to_path_buf(),
        since.map(str::to_owned),
        now,
        Arc::clone(&stop),
    );
    let url = format!("http://{}:{bound_port}/", url_host(host));
    log::info(
        "report server listening",
        &[
            ("url", Value::String(url.clone())),
            ("host", Value::String(host.to_owned())),
            ("port", Value::from(bound_port)),
            ("repositories", Value::from(repositories)),
        ],
    );

    Ok(ReportServer {
        url,
        host: host.to_owned(),
        port: bound_port,
        repositories,
        stop,
        server: Some(listener),
        thread: Some(thread),
        bound,
    })
}

/// One section per registered root, in registry order.
///
/// A root with no telemetry directory is still a section: "this repository recorded nothing" is an
/// answer, and dropping it would hide a repo that was registered but never used. `window` is the
/// bound the records are filtered against and `since` is the label the page prints — the same pair
/// the TypeScript passes as `{since: window}` and `since`.
pub fn report_sections(home: &Path, window: Option<&Since>, since: Option<&str>) -> ReportSections {
    let registry = read_registry(home);
    let mut sections: Vec<ReportSection> = Vec::with_capacity(registry.repos.len());
    let mut records: Vec<TelemetryRecord> = Vec::new();
    let mut unparsed = 0;

    for root in &registry.repos {
        let source_dir = root.join(config::DATA_DIR_NAME).join(TELEMETRY_DIR);
        let read = read_records(&source_dir, window.cloned());
        unparsed += read.unparsed;
        let (root_text, source_text) = (root.to_string_lossy(), source_dir.to_string_lossy());
        sections.push(ReportSection::new(
            &root_text,
            &source_text,
            aggregate_with(&read.records, read.unparsed, since),
        ));
        records.extend(read.records);
    }

    ReportSections {
        machine: aggregate_with(&records, unparsed, since),
        sections,
    }
}

/// Membership in the literal allow-list, so the refusal is decided from the string alone.
fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host)
}

/// `::1` in a URL has to be bracketed: `http://::1:7317/` is not a URL a client can read.
fn url_host(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_owned()
    }
}

fn read_registry(home: &Path) -> Registry {
    registry::read_registry(&home.join(REGISTRY_FILE))
}

fn spawn_accept_loop(
    listener: Arc<Server>,
    home: PathBuf,
    since: Option<String>,
    now: Clock,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || accept_loop(&listener, &home, since.as_deref(), now, &stop))
}

/// The accept loop: one request at a time, until the stop flag is set.
fn accept_loop(listener: &Server, home: &Path, since: Option<&str>, now: Clock, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match listener.recv_timeout(POLL) {
            Ok(Some(request)) => answer(request, home, since, now),
            // Nothing arrived inside the window, which is why the wait has a bound: the flag is read
            // again instead of blocking until a request that a closed server would never answer.
            Ok(None) => {}
            Err(error) => {
                // `close` unblocks this wait and sets the flag first, so an error with the flag clear
                // is the listener failing under the loop — a server that stopped accepting is not one
                // to answer from.
                if !stop.load(Ordering::Relaxed) {
                    log::warn(
                        "report server stopped accepting",
                        &[("reason", Value::String(error.to_string()))],
                    );
                }
                return;
            }
        }
    }
}

/// The three routes, matched literally on method and path.
///
/// `HEAD /healthz` is a 404 on purpose: the contract lists the method of every route, and a probe
/// that wants to know whether the page renders has to read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Route {
    Health,
    HeadReport,
    Report,
    Missing,
}

impl Route {
    fn of(method: &Method, url: &str) -> Self {
        // `request.url()` is the request target, so anything from the `?` on is the client's business.
        let path = url.split_once('?').map_or(url, |(path, _query)| path);
        match (method, path) {
            (Method::Get, "/healthz") => Self::Health,
            (Method::Head, "/") => Self::HeadReport,
            (Method::Get, "/") => Self::Report,
            _ => Self::Missing,
        }
    }
}

fn answer(request: Request, home: &Path, since: Option<&str>, now: Clock) {
    let route = Route::of(request.method(), request.url());
    let response = match route {
        Route::Health => Response::from_string("ok\n").with_header(content_type(TEXT_CONTENT_TYPE)),
        // Status and headers are the whole answer to a HEAD; rendering the page would be work no
        // client reads, so the document is not built here.
        Route::HeadReport => {
            Response::from_string(String::new()).with_header(content_type(HTML_CONTENT_TYPE))
        }
        Route::Report => {
            let page = page_at(home, since, now());
            Response::from_string(page).with_header(content_type(HTML_CONTENT_TYPE))
        }
        Route::Missing => Response::from_string("not found\n")
            .with_status_code(StatusCode(404))
            .with_header(content_type(TEXT_CONTENT_TYPE)),
    };
    // A client that hung up while the page was being rendered is not the listener's failure: the next
    // request arrives on the same socket, so the answer is logged and the loop continues.
    if let Err(error) = request.respond(response) {
        log::debug(
            "report response not delivered",
            &[("reason", Value::String(error.to_string()))],
        );
    }
}

/// One rendered page, from this request's clock.
fn page_at(home: &Path, since: Option<&str>, now: DateTime<Utc>) -> String {
    // `Nd` is one window read against one clock reading per request, so a server that runs for days
    // slides its window with the calendar instead of freezing it at the bind. `start` refuses a
    // `--since` it cannot parse, so the only value this can drop is one that stopped parsing since —
    // and the page then states the whole history rather than answering nothing.
    let window = since.and_then(|raw| parse_since(raw, now).ok());
    let sections = report_sections(home, window.as_ref(), since);
    render_multi_page(&sections.machine, &iso_of(now), since, &sections.sections)
}

/// The one clock reading a page or a report file carries, in the spelling the TypeScript's
/// `toISOString` writes: three fractional digits and a trailing `Z`.
///
/// Public because the CLI's file write takes the same reading: one directory rendered to a page and
/// to a file has to carry the same stamp, and a second spelling here would be a second format.
#[must_use]
pub fn iso_of(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The system clock, as the one reading a page carries.
///
/// This crate reads no clock of its own — `report.rs` takes `now` as an argument, and `serve` takes
/// this function as one — so the reading is named in exactly one place and a test can state the day
/// instead of waiting for it.
#[must_use]
pub fn system_now() -> DateTime<Utc> {
    DateTime::<Utc>::from(std::time::SystemTime::now())
}

/// The header each route sets. Both arguments are ASCII literals written here, so the only way
/// `from_bytes` can report anything is a literal edited into non-ASCII — a mistake in this file, not
/// in a request.
fn content_type(value: &'static str) -> Header {
    Header::from_bytes("content-type", value).expect("a content-type built from ASCII literals")
}

/// Waits for the listening socket to be gone.
///
/// The accept thread the crate spawns closes the listener a moment after the last handle drops, so
/// "the port is free" is a fact to wait for rather than to assume: a caller that closes and rebinds
/// would otherwise race the kernel, and a suite would leave a socket the machine still lists. A
/// connect to a closed loopback port is refused immediately, so the first attempt normally answers.
fn wait_until_refused(address: SocketAddr) {
    for _ in 0..CLOSE_ATTEMPTS {
        if TcpStream::connect_timeout(&address, CLOSE_TIMEOUT).is_err() {
            return;
        }
        thread::sleep(CLOSE_TIMEOUT);
    }
    log::warn(
        "report server socket still open after close",
        &[("address", Value::String(address.to_string()))],
    );
}
