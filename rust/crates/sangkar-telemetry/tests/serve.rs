//! The report server over a real socket, on an ephemeral port.
//!
//! Mirrors `tests/telemetry-serve.test.ts` for the half that needs HTTP: one route, one allow-list,
//! one listener, and a page that is recomputed per request. The registry, the repositories and every
//! JSONL line live under a temporary home that removes itself, and nothing here binds a fixed port,
//! so no case can collide with a real daemon or with the other cases. Each server is closed on every
//! path — `Drop` closes it even if an assertion below panics — and the last case asserts the socket is
//! gone rather than trusting that it is.
//!
//! The fixtures are hand-written lines rather than records produced by the sink, because what is under
//! test is the server: the reader's own arithmetic is `tests/report.rs`'s subject.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::json;

use sangkar_telemetry::serve::{
    DEFAULT_HOST, DEFAULT_REPORT_PORT, LOOPBACK_HOSTS, ReportServer, ServeError, ServeOptions,
    report_sections, start,
};

/// The fixture stamp the TypeScript suite uses.
const FIXED_TS: &str = "2026-11-18T10:00:00.000Z";

/// The eight panels a body has to carry, by the ids `html.rs` gives them. Named rather than counted:
/// a page that lost a panel and gained another has the same length.
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

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// One call line, as the recorder would have written it.
fn call_line(ts: &str, seq: u32, tool: &str) -> String {
    json!({
        "ts": ts,
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

/// A temporary directory that removes itself, so the suite leaves no home and no repository behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-serve-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        // Canonical, because the registry canonicalises what it reads and the label carries the root.
        Self(fs::canonicalize(&path).expect("canonicalise the temp directory"))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    /// A directory under this one, created, named as the caller names it.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::create_dir_all(&path).expect("create a fixture directory");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The machine a case serves: its home, and the three roots its registry names.
struct Machine {
    home: TempDir,
    alpha: PathBuf,
    beta: PathBuf,
    empty: PathBuf,
}

impl Machine {
    /// Three repositories: `alpha` has two records and one torn line, `beta` has one record, `empty`
    /// has no telemetry directory at all. The torn line is deliberate — the machine total's unparsed
    /// count has to be the sum over repositories, not the count of a single directory.
    fn new(label: &str) -> Self {
        let home = TempDir::new(label);
        let alpha = home.dir("alpha");
        let beta = home.dir("beta");
        let empty = home.dir("empty");
        write_telemetry(
            &alpha,
            &[
                call_line(FIXED_TS, 1, "status"),
                call_line(FIXED_TS, 2, "search"),
                "{not json".to_owned(),
            ],
        );
        write_telemetry(&beta, &[call_line(FIXED_TS, 1, "trace")]);
        let registry = json!({
            "version": 1,
            "repos": [alpha, beta, empty],
        });
        fs::write(home.path().join("sangkar.json"), registry.to_string())
            .expect("write the registry");
        Self {
            home,
            alpha,
            beta,
            empty,
        }
    }

    fn options(&self) -> ServeOptions<'_> {
        ServeOptions {
            port: Some(0),
            ..ServeOptions::new(self.home.path())
        }
    }
}

/// One `<date>.jsonl` under the repository's telemetry directory.
fn write_telemetry(root: &Path, lines: &[String]) {
    let dir = root.join(".sangkar").join("telemetry");
    fs::create_dir_all(&dir).expect("create the telemetry directory");
    fs::write(
        dir.join("2026-11-18.jsonl"),
        format!("{}\n", lines.join("\n")),
    )
    .expect("write the daily file");
}

fn append_telemetry(root: &Path, line: &str) {
    let path = root
        .join(".sangkar")
        .join("telemetry")
        .join("2026-11-18.jsonl");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open the daily file");
    writeln!(file, "{line}").expect("append a record");
}

// -------------------------------------------------------------------------------------------
// A client the size of the contract
// -------------------------------------------------------------------------------------------

/// One answer: the status, the headers lowercased, and the body the transfer encoding describes.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(field, _value)| field == name)
            .map(|(_field, value)| value.as_str())
    }
}

/// One request and its whole answer, read to EOF.
///
/// `Connection: close` is what makes the read terminate: the listener honours it and drops the socket
/// after one response, so the answer needs no parser of its own beyond the chunk framing below.
fn get(url: &str, method: &str) -> Answer {
    let authority = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(authority).expect("connect to the report server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set a read timeout");
    write!(
        stream,
        "{method} / HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .expect("write the request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the answer");
    parse(&raw)
}

/// One request to a path other than `/`.
fn get_path(url: &str, path: &str) -> Answer {
    let authority = url.trim_start_matches("http://").trim_end_matches('/');
    let mut stream = TcpStream::connect(authority).expect("connect to the report server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set a read timeout");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .expect("write the request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read the answer");
    parse(&raw)
}

fn parse(raw: &[u8]) -> Answer {
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
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(field, value)| (field.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let chunked = headers.iter().any(|(field, value)| {
        field == "transfer-encoding" && value.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        dechunk(&raw[head_end..])
    } else {
        raw[head_end..].to_vec()
    };
    Answer {
        status,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    }
}

/// `tiny_http` chunks a body of 32 KiB or more, and a page with a stylesheet is not far under it.
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

/// The address a server is listening on, for the cases that connect to it themselves.
fn address_of(server: &ReportServer) -> SocketAddr {
    format!("127.0.0.1:{}", server.port())
        .parse()
        .expect("an address from a loopback URL")
}

// -------------------------------------------------------------------------------------------
// The routes
// -------------------------------------------------------------------------------------------

#[test]
fn get_root_renders_every_repository_and_the_machine_total() {
    let machine = Machine::new("root");
    let mut server = start(machine.options()).expect("bind an ephemeral port");

    assert!(server.port() > 0, "port 0 is read back from the socket");
    assert_eq!(server.host(), DEFAULT_HOST);
    assert_eq!(
        server.repositories(),
        3,
        "the registry's count at bind time"
    );
    assert_eq!(server.url(), format!("http://127.0.0.1:{}/", server.port()));

    let answer = get(server.url(), "GET");
    assert_eq!(answer.status, 200);
    assert!(
        answer
            .header("content-type")
            .is_some_and(|value| value.starts_with("text/html")),
        "content-type: {:?}",
        answer.header("content-type")
    );

    let body = &answer.body;
    for id in PANEL_IDS {
        assert!(body.contains(&format!("id=\"{id}\"")), "the page has {id}");
    }
    assert!(body.contains("id=\"machine\""), "the machine total panel");
    // One section per registered root, in registry order, each labelled with its basename and path.
    for (index, root) in [&machine.alpha, &machine.beta, &machine.empty]
        .iter()
        .enumerate()
    {
        assert!(body.contains(&format!("id=\"repo-{}\"", index + 1)));
        assert!(body.contains(root.to_string_lossy().as_ref()));
    }
    // The machine total names the fold no single section can: 2 + 1 + 0.
    assert!(body.contains("records <span>3</span>"), "the machine total");
    // Self-contained: the page must draw itself with no network beyond this listener.
    assert!(!body.contains("http://"));
    assert!(!body.contains("<link"));

    server.close();
}

#[test]
fn wrote_records_show_up_on_the_next_request() {
    let machine = Machine::new("rerender");
    let mut server = start(machine.options()).expect("bind an ephemeral port");
    assert!(
        get(server.url(), "GET")
            .body
            .contains("records <span>3</span>")
    );

    append_telemetry(&machine.alpha, &call_line(FIXED_TS, 3, "status"));

    let body = get(server.url(), "GET").body;
    assert!(
        body.contains("records <span>4</span>"),
        "the page is rendered per request, not at bind"
    );
    // The repository count is the bind-time answer and does not move mid-request.
    assert_eq!(server.repositories(), 3);

    server.close();
}

#[test]
fn healthz_answers_ok_and_an_unknown_path_is_a_404() {
    let machine = Machine::new("routes");
    let mut server = start(machine.options()).expect("bind an ephemeral port");

    let health = get_path(server.url(), "/healthz");
    assert_eq!(health.status, 200);
    assert_eq!(
        health.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(health.body, "ok\n");

    let missing = get_path(server.url(), "/nope");
    assert_eq!(missing.status, 404);
    assert_eq!(missing.body, "not found\n");

    server.close();
}

#[test]
fn head_root_is_a_200_with_an_empty_body() {
    let machine = Machine::new("head");
    let mut server = start(machine.options()).expect("bind an ephemeral port");

    let answer = get(server.url(), "HEAD");
    assert_eq!(answer.status, 200);
    assert!(
        answer
            .header("content-type")
            .is_some_and(|value| value.starts_with("text/html"))
    );
    assert_eq!(answer.body, "", "a HEAD carries no document");

    server.close();
}

#[test]
fn since_narrows_every_section_and_labels_the_window() {
    let machine = Machine::new("since");
    append_telemetry(
        &machine.alpha,
        &call_line("2026-11-01T09:00:00.000Z", 9, "status"),
    );
    let mut server = start(ServeOptions {
        since: Some("2026-11-15"),
        ..machine.options()
    })
    .expect("bind an ephemeral port");

    let body = get(server.url(), "GET").body;
    assert!(body.contains("window <strong>2026-11-15</strong>"));
    assert!(
        body.contains("records <span>3</span>"),
        "the 2026-11-01 record is outside the window"
    );

    server.close();
}

// -------------------------------------------------------------------------------------------
// The policy
// -------------------------------------------------------------------------------------------

#[test]
fn a_non_loopback_host_is_refused_before_anything_binds() {
    let machine = Machine::new("host");

    let refused = start(ServeOptions {
        host: Some("0.0.0.0"),
        ..machine.options()
    });
    let error = refused.err().expect("0.0.0.0 is not a loopback host");
    assert_eq!(
        error,
        ServeError::HostNotLoopback {
            host: "0.0.0.0".to_owned()
        }
    );
    // The refusal names every allowed host, so the message is the fix and not just a "no".
    let message = error.to_string();
    for host in LOOPBACK_HOSTS {
        assert!(message.contains(host), "{message}");
    }
    assert!(message.contains("0.0.0.0"), "{message}");
    assert!(message.contains("loopback only"), "{message}");
}

#[test]
fn an_unreadable_since_is_refused_before_anything_binds() {
    let machine = Machine::new("since-refusal");

    let refused = start(ServeOptions {
        since: Some("24h"),
        ..machine.options()
    });
    let error = refused.err().expect("24h is not a --since literal");
    assert_eq!(
        error,
        ServeError::Since {
            raw: "24h".to_owned()
        }
    );
    assert!(error.to_string().contains("YYYY-MM-DD or <n>d"));
}

#[test]
fn the_defaults_are_the_ones_the_daemon_binds() {
    // The constants, not a bound listener: this suite never binds the default port.
    assert_eq!(DEFAULT_REPORT_PORT, 7317);
    assert_eq!(DEFAULT_HOST, "127.0.0.1");
    assert_eq!(LOOPBACK_HOSTS, ["127.0.0.1", "::1", "localhost"]);
}

#[test]
fn close_is_idempotent_and_leaves_no_listening_socket() {
    let machine = Machine::new("close");
    let mut server = start(machine.options()).expect("bind an ephemeral port");
    let address = address_of(&server);
    assert_eq!(get(server.url(), "GET").status, 200);

    server.close();
    server.close();

    let refused = TcpStream::connect_timeout(&address, Duration::from_secs(2));
    assert!(
        refused.is_err(),
        "the port is still answering after close: {refused:?}"
    );
}

#[test]
fn the_sections_are_the_registry_order_with_the_machine_fold() {
    let machine = Machine::new("sections");

    let report = report_sections(machine.home.path(), None, None);
    let labels: Vec<String> = report
        .sections
        .iter()
        .map(|section| section.label())
        .collect();
    assert_eq!(
        labels,
        [
            format!("alpha — {}", machine.alpha.display()),
            format!("beta — {}", machine.beta.display()),
            format!("empty — {}", machine.empty.display()),
        ]
    );
    assert_eq!(
        report
            .sections
            .iter()
            .map(|section| section.data.totals.records)
            .collect::<Vec<u64>>(),
        [2, 1, 0]
    );
    // The unparsed count is the sum over repositories, and the machine fold is a real aggregate.
    assert_eq!(report.machine.totals.records, 3);
    assert_eq!(report.machine.totals.unparsed, 1);
    assert_eq!(
        report
            .machine
            .tools
            .iter()
            .map(|tool| tool.tool.as_str())
            .collect::<Vec<&str>>(),
        ["search", "status", "trace"]
    );
}

/// The one thing a test cannot assert by binding: an empty registry is an empty page, not a failed
/// server.
#[test]
fn an_empty_registry_answers_the_machine_panel_alone() {
    let home = TempDir::new("empty-registry");
    fs::write(
        home.path().join("sangkar.json"),
        json!({"version": 1, "repos": []}).to_string(),
    )
    .expect("write the registry");

    // `port: 0`, like every case here: the default port belongs to whatever the machine already runs.
    let mut server = start(ServeOptions {
        port: Some(0),
        ..ServeOptions::new(home.path())
    })
    .expect("bind an ephemeral port");
    assert_eq!(server.repositories(), 0);
    let body = get(server.url(), "GET").body;
    assert!(body.contains("records <span>0</span>"));
    assert!(body.contains("no registered repository has recorded anything yet"));
    for id in PANEL_IDS {
        assert!(body.contains(&format!("id=\"{id}\"")), "the page has {id}");
    }
    server.close();
}
