//! The artifact this migration exists for: one native binary that answers, indexes and serves a
//! protocol on a machine that has no interpreter on it.
//!
//! Every case runs the binary cargo built for the package that declares it
//! (`CARGO_BIN_EXE_sangkar` — the bytes an installed `sangkar` would run) as a real child process on
//! a machine carrying exactly two variables: `PATH` pointing at an empty directory, and `HOME` at a
//! temporary one. Nothing else is inherited, and the strip is the test — a binary that reached for
//! `bun`, `node`, `npx` or a `python` would find nothing there and fail here, while `HOME` keeps the
//! config, the registry and the telemetry a run writes out of the developer's account.
//!
//! Five claims, one per test:
//!
//! - `status` answers before anything is indexed, with no interpreter reachable;
//! - `index` publishes the `polyglot` fixture into a temporary data dir and leaves
//!   `<data dir>/graph.db` behind;
//! - the seven published tools answer that graph — six write an envelope on stdout, and the seventh
//!   (`index`) is answered by the publish in the case above, because its CLI spelling is the
//!   bootstrap verb whose whole answer is its exit code (`rust/tests/golden/polyglot/index.json`);
//! - the recorded session (`rust/tests/vectors/mcp_session.ndjson`) replays as one JSON object per
//!   line, with no `Content-Length` framing and no notification answered;
//! - the binary's own directory carries no `.wasm`, `.node` or `node_modules` beside it.
//!
//! Nothing is written where the repository lives: every run passes `--data-dir` under a temporary
//! directory that removes itself, and the `mcp` case ends the session the way the recording's client
//! did — by closing stdin — rather than by killing a process. The publish, the slowest thing here,
//! takes tens of milliseconds, so the file stays far inside the 60 s budget.
//!
//! The claim is about the artifact CI ships, a Linux x86_64 binary. A host that is not one cannot
//! state it — an interpreter could be reachable by a route this suite does not strip — so the
//! subprocess cases skip out loud there instead of passing on a machine where they prove nothing.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// The binary on trial: the one cargo built for the package that declares it, so the bytes under
/// test are the bytes an installed `sangkar` would run.
const SANGGAR: &str = env!("CARGO_BIN_EXE_sangkar");

/// The fixture these cases index: the polyglot tree every parity golden was captured on.
const FIXTURE: &str = "polyglot";

/// The six tools whose CLI spelling answers an envelope, with the arguments the recorded goldens
/// used (`rust/tests/golden/polyglot/*.json`), so the smoke case asks the questions the parity
/// replay asks instead of inventing cheaper ones. The seventh tool, `index`, is on the list above
/// and is answered by the publish itself.
const ENVELOPE_TOOLS: [(&str, &[&str]); 6] = [
    ("status", &[]),
    ("summary", &["--scope", "repo"]),
    ("trace", &["--query", "alpha", "--depth", "2"]),
    ("search", &["--query", "request"]),
    ("impact", &["--paths", "src/chain.ts", "--depth", "2"]),
    (
        "expand",
        &["--target", "src/Account.cs", "--what", "members"],
    ),
];

/// A temporary directory that removes itself, so no home, working directory or database outlives
/// the suite.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-smoke-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The repository's `rust/tests/` directory: the fixture and the recorded session this suite reads.
/// `CARGO_MANIFEST_DIR` is `rust/crates/sangkar-cli`, so two levels up is `rust`.
fn rust_tests() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
}

/// The tree every case indexes.
fn fixture() -> PathBuf {
    rust_tests().join("parity").join("fixtures").join(FIXTURE)
}

/// Whether this host is the one the claim is made about, stating the skip when it is not. A host
/// where the empty `PATH` proves nothing is not a host where the cases may pass.
fn on_the_release_target(case: &str) -> bool {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    if os == "linux" && arch == "x86_64" {
        return true;
    }
    println!(
        "binary_smoke: SKIP {case}: this suite states the Linux x86_64 artifact; this host is \
         {os}/{arch}"
    );
    false
}

/// One machine: the home a run may write, the working directory it starts in, the empty directory
/// that is its whole `PATH`, and the data dir its graph goes in.
struct Machine {
    root: TempDir,
}

impl Machine {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        for name in ["home", "work", "empty-path", "data"] {
            fs::create_dir_all(root.join(name)).expect("create a machine directory");
        }
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The directory a run starts in: not the checkout, so autodetection and a relative argument
    /// cannot reach a tree this test did not choose.
    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn empty_path(&self) -> PathBuf {
        self.root.join("empty-path")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    /// The two globals every case passes: the tree under test, and a data dir this machine owns.
    /// Never the fixture's own `.sangkar` — a checked-in tree is not a place to write a database.
    fn globals(&self) -> Vec<String> {
        vec![
            "--repo-root".to_owned(),
            fixture().display().to_string(),
            "--data-dir".to_owned(),
            self.data_dir().display().to_string(),
        ]
    }

    /// One child on this machine's environment: `PATH` and `HOME` and nothing else.
    fn command(&self, argv: &[String]) -> Command {
        let mut command = Command::new(SANGGAR);
        command
            .args(argv)
            .current_dir(self.work())
            .env_clear()
            .env("PATH", self.empty_path())
            .env("HOME", self.home())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// One tool invocation: the name, the two globals, `--json`, then the tool's own arguments.
    fn run_tool(&self, tool: &str, args: &[&str]) -> Run {
        let mut argv = vec![tool.to_owned()];
        argv.extend(self.globals());
        argv.push("--json".to_owned());
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.run(&argv)
    }

    /// One verb invocation — the commands that own their answer instead of writing an envelope —
    /// with the same two globals.
    fn run_verb(&self, verb: &str, args: &[&str]) -> Run {
        let mut argv = vec![verb.to_owned()];
        argv.extend(self.globals());
        argv.extend(args.iter().map(|arg| (*arg).to_owned()));
        self.run(&argv)
    }

    fn run(&self, argv: &[String]) -> Run {
        let output = self
            .command(argv)
            .output()
            .unwrap_or_else(|error| panic!("cannot start {SANGGAR}: {error}"));
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }

    /// One MCP session: `frames` written to a pipe, the pipe closed, the process waited for. EOF on
    /// stdin is the shutdown signal this server documents, and it is how the recording's client
    /// ended its session — a kill would leave the exit code out of the answer.
    fn session(&self, frames: &str) -> Run {
        let mut argv = vec!["mcp".to_owned()];
        argv.extend(self.globals());
        let mut child = self
            .command(&argv)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("cannot start {SANGGAR} mcp: {error}"));
        {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            pipe.write_all(frames.as_bytes())
                .expect("write the session");
            // Dropped, not flushed: EOF is the client-gone signal this server shuts down on.
        }
        let output = child
            .wait_with_output()
            .unwrap_or_else(|error| panic!("cannot wait for {SANGGAR} mcp: {error}"));
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8(output.stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }
}

/// What one invocation answered.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Run {
    /// The run exited 0. The failure carries the child's stderr, because a stripped environment
    /// fails as a missing file rather than as a sentence.
    fn exited_zero(&self, what: &str) {
        assert_eq!(
            self.code,
            0,
            "{what} exited {} (0 expected); stderr was:\n{}",
            self.code,
            self.stderr.trim_end()
        );
    }

    /// The one JSON envelope a tool writes on stdout.
    fn envelope(&self, what: &str) -> Value {
        serde_json::from_str(&self.stdout).unwrap_or_else(|error| {
            panic!("{what} wrote no JSON envelope ({error}): {:?}", self.stdout)
        })
    }
}

/// The recorded session's client frames, and the bytes a client writes for them.
///
/// `mcp_session.ndjson` records both directions as `{"dir", "raw", …}`, so the replay is its `c2s`
/// half, one frame per line — the same extraction `tests/mcp_surface.rs` makes, so both suites
/// replay one oracle instead of two derivations of it.
fn recorded_session() -> (Vec<Value>, String) {
    let path = rust_tests().join("vectors").join("mcp_session.ndjson");
    let recording = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
    let mut sent = Vec::new();
    let mut stdin = String::new();
    for line in recording.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("a recorded line");
        if record["dir"] != "c2s" {
            continue;
        }
        let raw = record["raw"].as_str().expect("a recorded frame");
        let frame: Value = serde_json::from_str(raw).expect("a client frame");
        stdin.push_str(&serde_json::to_string(&frame).expect("a client frame serialises"));
        stdin.push('\n');
        sent.push(frame);
    }
    assert!(
        !sent.is_empty(),
        "the recording at {} carries no client frames",
        path.display()
    );
    (sent, stdin)
}

#[test]
fn the_status_answer_survives_a_path_with_no_interpreter_on_it() {
    if !on_the_release_target("the empty-PATH cases") {
        return;
    }
    // The record this suite exists to produce: how large the artifact is that answers, indexes and
    // serves with no interpreter beside it.
    let size = fs::metadata(SANGGAR)
        .unwrap_or_else(|error| panic!("cannot stat {SANGGAR}: {error}"))
        .len();
    println!("binary_smoke: {SANGGAR} is {size} bytes");

    let machine = Machine::new("status");
    let ran = machine.run_tool("status", &[]);
    ran.exited_zero("sangkar status on a data dir with no graph");
    // Before the first publish `status` answers the bare line an operator reads rather than an
    // envelope, and the line naming the file it looked for is what makes exit 0 mean "answered"
    // instead of "did nothing" — it also proves `--data-dir` reached the store.
    let graph = machine.data_dir().join("graph.db");
    assert!(
        ran.stdout.contains(&graph.display().to_string()),
        "sangkar status did not name the graph it looked for at {}; stdout was {:?}",
        graph.display(),
        ran.stdout
    );
}

#[test]
fn the_index_verb_publishes_a_graph_with_nothing_on_the_path_but_an_empty_directory() {
    if !on_the_release_target("the empty-PATH cases") {
        return;
    }
    let machine = Machine::new("index");
    let ran = machine.run_verb("index", &[]);
    ran.exited_zero("sangkar index on the polyglot fixture");
    // The bootstrap verb's contract is its exit code: the recorded golden
    // (`rust/tests/golden/polyglot/index.json`) carries no stdout, and a worker that printed there
    // would corrupt the session that spawned it.
    assert!(
        ran.stdout.is_empty(),
        "sangkar index wrote {:?} on stdout",
        ran.stdout
    );
    let graph = machine.data_dir().join("graph.db");
    let published = fs::metadata(&graph)
        .unwrap_or_else(|error| panic!("the publish left no {}: {error}", graph.display()));
    assert!(
        published.is_file() && published.len() > 0,
        "{} is not a database with bytes in it",
        graph.display()
    );
}

#[test]
fn every_tool_answers_the_published_graph_with_no_interpreter_on_the_path() {
    if !on_the_release_target("the empty-PATH cases") {
        return;
    }
    let machine = Machine::new("tools");
    machine
        .run_verb("index", &[])
        .exited_zero("the publish that makes the graph the tools read");

    for (tool, args) in ENVELOPE_TOOLS {
        let ran = machine.run_tool(tool, args);
        ran.exited_zero(&format!("sangkar {tool} {}", args.join(" ")));
        let envelope = ran.envelope(tool);
        let status = envelope["status"].as_str().unwrap_or_else(|| {
            panic!("sangkar {tool} answered no string status: {:?}", ran.stdout)
        });
        // A miss is a legitimate answer (`not_found`); a failure is not, because these arguments are
        // the goldens' own and the graph they read is the one this machine just published.
        assert_ne!(
            status,
            "error",
            "sangkar {tool} answered an error envelope: {}",
            ran.stdout.trim_end()
        );
        let generation = envelope["read_status"]["generation"]
            .as_str()
            .unwrap_or_default();
        assert!(
            !generation.is_empty(),
            "sangkar {tool} answered without a generation, so it read no published graph: {}",
            ran.stdout.trim_end()
        );
    }
}

#[test]
fn the_recorded_session_replays_as_newline_delimited_json_with_nothing_else_on_stdout() {
    if !on_the_release_target("the empty-PATH cases") {
        return;
    }
    let machine = Machine::new("mcp");
    let (sent, stdin) = recorded_session();
    let ran = machine.session(&stdin);
    ran.exited_zero("a recorded MCP session that ends on stdin EOF");

    assert!(
        !ran.stdout.contains('\r'),
        "stdout carried a carriage return, which no MCP frame may"
    );
    assert!(
        !ran.stdout.contains("Content-Length"),
        "the transport framed with a Content-Length header instead of a newline"
    );
    assert!(
        ran.stdout.ends_with('\n'),
        "stdout did not end on a frame terminator: {:?}",
        &ran.stdout[ran.stdout.len().saturating_sub(80)..]
    );
    let body = &ran.stdout[..ran.stdout.len() - 1];
    assert!(
        !body.contains("\n\n"),
        "stdout carried a blank line between frames"
    );

    let mut answered = Vec::new();
    for line in body.lines() {
        let frame: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("a stdout line is not JSON ({error}): {line}"));
        assert!(
            frame.is_object(),
            "every response is one JSON object, not {frame}"
        );
        assert_eq!(
            frame["jsonrpc"], "2.0",
            "every frame is a JSON-RPC 2.0 message"
        );
        answered.push(
            frame["id"]
                .as_u64()
                .unwrap_or_else(|| panic!("a response carries no numeric id: {line}")),
        );
    }

    // Keyed by id, never by arrival order: the transport dispatches concurrently, which the
    // protocol allows and the recording measured. Equality of the two sets states both halves —
    // every request with an id answered once, and the notification answered not at all.
    let mut requested: Vec<u64> = sent
        .iter()
        .filter_map(|frame| frame["id"].as_u64())
        .collect();
    assert_eq!(
        answered.len(),
        requested.len(),
        "the session answered {} frames for {} id-bearing requests; stdout was:\n{}",
        answered.len(),
        requested.len(),
        ran.stdout
    );
    answered.sort_unstable();
    requested.sort_unstable();
    assert_eq!(
        answered, requested,
        "the session answered a different set of ids than the recording asked"
    );
    println!(
        "binary_smoke: replayed {} client frames, answered {}",
        sent.len(),
        answered.len()
    );
}

#[test]
fn the_binary_ships_with_no_wasm_node_or_node_modules_beside_it() {
    let binary = fs::canonicalize(SANGGAR)
        .unwrap_or_else(|error| panic!("cannot resolve {SANGGAR}: {error}"));
    let metadata = fs::metadata(&binary)
        .unwrap_or_else(|error| panic!("cannot stat {}: {error}", binary.display()));
    assert!(
        metadata.is_file() && metadata.len() > 0,
        "{} is not a file with bytes in it",
        binary.display()
    );

    // The directory an install would ship: anything the binary had to load from beside itself would
    // have to sit here, and none of these three is something a native-only artifact needs.
    let directory = binary
        .parent()
        .unwrap_or_else(|| panic!("{} has no parent directory", binary.display()));
    let mut assets = Vec::new();
    for entry in fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
    {
        let entry = entry.expect("a readable directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "node_modules" || name.ends_with(".wasm") || name.ends_with(".node") {
            assets.push(name);
        }
    }
    assets.sort();
    assert!(
        assets.is_empty(),
        "{} carries assets a native-only binary would not need: {}",
        directory.display(),
        assets.join(", ")
    );

    // The file itself is a native executable, not a script with an interpreter line: on the release
    // target that is the ELF magic, and a shebang would fail it.
    if std::env::consts::OS == "linux" {
        let mut magic = [0u8; 4];
        fs::File::open(&binary)
            .and_then(|mut file| file.read_exact(&mut magic))
            .unwrap_or_else(|error| {
                panic!("cannot read the head of {}: {error}", binary.display())
            });
        assert_eq!(
            &magic,
            b"\x7fELF",
            "{} does not start with the ELF magic a Linux executable carries",
            binary.display()
        );
    }
}
