//! The MCP surface against the recorded session: `tools/list` field for field, every failure in
//! band, and stdout carrying nothing but one JSON object per response.
//!
//! The oracle is `rust/tests/vectors/` — the frames `@modelcontextprotocol/sdk` really sent and the
//! answers this repository's TypeScript server really gave (`mcp_session.ndjson`, recorded by
//! TASK-011; `mcp_session_ts_only.json`, its answer half). Neither file is edited by this test: a
//! replay that rewrote its own oracle would prove nothing.
//!
//! Two things shape the assertions:
//!
//! - **`rmcp` dispatches concurrently**, so arrival order is not request order (the decision in
//!   `rust/spikes/mcp/DECISION.md` measured ids `1,5,3,2,4`). Every comparison here is keyed by the
//!   JSON-RPC `id`, which is the only thing a conforming client may rely on.
//! - **the recording was taken on a temp data dir**, so the one answer that embeds a path
//!   (`status` before the first publish) is compared with the recording's path substituted for this
//!   run's. Everything else is byte-identical.
//!
//! The server runs as a real child process with real pipes, because that is the claim: not "the
//! handler returned this string" but "the process put exactly these bytes on stdout".

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_cli::mcp::Surface;
use sangkar_core::config;
use sangkar_core::envelope::Envelope;
use sangkar_core::errors::AppError;
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::schema_version;
use sangkar_store::publish::{begin_write, with_stage};
use sangkar_store::read::context::ToolContext;
use sangkar_tools::registry::{Args, Descriptor, RegisteredTool, TOOLS};
use serde_json::{Map, Value};

/// The binary this suite drives. Cargo builds it for an integration test of the package that
/// declares it, so the bytes on trial are the ones an installed `sangkar` would run.
const SANGGAR: &str = env!("CARGO_BIN_EXE_sangkar");

/// The path the recording was taken on, as it appears in its `status` answer.
const RECORDED_DATA_DIR: &str = "/tmp/sangkar-parity-mcp-session";

/// A temporary directory that removes itself, so the suite leaves no home, tree or database behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-mcp-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        let path = fs::canonicalize(&path).expect("canonicalise the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One machine and the tree it serves: a home, a repository that looks indexed, and a data dir.
struct Machine {
    root: TempDir,
    tree: PathBuf,
    data_dir: PathBuf,
}

impl Machine {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        let tree = root.path().join("tree");
        fs::create_dir_all(tree.join(".git")).expect("create the tree");
        let data_dir = root.path().join("data");
        fs::create_dir_all(&data_dir).expect("create the data dir");
        Self {
            root,
            tree,
            data_dir,
        }
    }

    fn home(&self) -> &Path {
        self.root.path()
    }

    /// The environment the server resolves its home and key from: the machine's own, so no test
    /// touches the developer's registry or config.
    fn env(&self, name: &str) -> Option<String> {
        match name {
            "SANGKAR_HOME" | "HOME" => Some(self.home().display().to_string()),
            _ => None,
        }
    }

    /// The two globals every session in this file passes, so no session ever opens the almost-real
    /// `.sangkar/graph.db` a checkout of this repository may be carrying.
    fn globals(&self) -> Vec<String> {
        vec![
            "--repo-root".to_owned(),
            self.tree.display().to_string(),
            "--data-dir".to_owned(),
            self.data_dir.display().to_string(),
        ]
    }

    /// The data dir with the path the recording was taken on, for the one answer that embeds it.
    fn recorded_text(&self, text: &str) -> String {
        text.replace(
            &format!("{RECORDED_DATA_DIR}/graph.db"),
            &self.data_dir.join("graph.db").display().to_string(),
        )
    }

    /// One full session: every byte written, stdin closed, the process waited for.
    fn session(&self, extra_globals: &[&str], stdin: &str) -> Session {
        let mut argv = vec!["mcp".to_owned()];
        argv.extend(self.globals());
        argv.extend(extra_globals.iter().map(|arg| (*arg).to_owned()));
        let mut child = Command::new(SANGGAR)
            .args(&argv)
            .env("SANGKAR_HOME", self.home())
            .env("HOME", self.home())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn the server");
        {
            let mut pipe = child.stdin.take().expect("stdin is piped");
            pipe.write_all(stdin.as_bytes()).expect("write the session");
            // Closed, not flushed: EOF is the client-gone signal this server shuts down on.
        }
        let output = child.wait_with_output().expect("wait for the server");
        Session {
            code: output.status.code(),
            stdout: output.stdout,
            stderr: String::from_utf8(output.stderr).expect("stderr is UTF-8"),
        }
    }
}

/// What one session left behind.
struct Session {
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: String,
}

impl Session {
    /// The responses, keyed by JSON-RPC `id` — never by arrival order (`rmcp` dispatches
    /// concurrently, which the decision recorded).
    fn answers(&self) -> Map<String, Value> {
        let mut answers = Map::new();
        for frame in self.frames() {
            let id = frame["id"]
                .as_u64()
                .expect("every recorded answer has a numeric id");
            assert!(
                answers.insert(id.to_string(), frame).is_none(),
                "id {id} answered twice"
            );
        }
        answers
    }

    /// The response frames, with the framing rules this suite exists to enforce.
    fn frames(&self) -> Vec<Value> {
        assert!(
            !self.stdout.is_empty(),
            "the session wrote nothing to stdout; stderr was: {}",
            self.stderr
        );
        assert!(
            !self.stdout.contains(&b'\r'),
            "stdout carried a carriage return, which no MCP frame may"
        );
        let text = std::str::from_utf8(&self.stdout).expect("stdout is UTF-8");
        assert!(
            !text.contains("Content-Length"),
            "the transport framed with Content-Length instead of a newline"
        );
        assert!(
            text.ends_with('\n'),
            "stdout did not end on a frame terminator: {:?}",
            &text[text.len().saturating_sub(80)..]
        );
        let body = &text[..text.len() - 1];
        assert!(
            !body.contains("\n\n"),
            "stdout carried a blank line between frames"
        );
        let mut frames = Vec::new();
        for line in text.lines() {
            let frame: Value = serde_json::from_str(line).unwrap_or_else(|error| {
                panic!("a stdout line is not a JSON object ({error}): {line}")
            });
            assert!(
                frame.is_object(),
                "every response is one JSON object, not {frame}"
            );
            assert_eq!(
                frame["jsonrpc"], "2.0",
                "every frame is a JSON-RPC 2.0 message"
            );
            frames.push(frame);
        }
        frames
    }

    fn answer(&self, id: u64) -> Value {
        self.answers()
            .get(&id.to_string())
            .cloned()
            .unwrap_or_else(|| panic!("no answer for id {id}; stderr was: {}", self.stderr))
    }
}

/// The recorded session, split into the frames the client sent and the answers the TypeScript server
/// gave, keyed by id. Both halves come from `rust/tests/vectors/`.
fn recorded() -> (Vec<Value>, Map<String, Value>) {
    let vectors = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("vectors");
    let session =
        fs::read_to_string(vectors.join("mcp_session.ndjson")).expect("read the recorded session");
    let mut sent = Vec::new();
    let mut tc_only = Map::new();
    for line in session.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("a recorded line");
        let raw = record["raw"].as_str().expect("a recorded frame");
        if record["dir"] == "c2s" {
            sent.push(serde_json::from_str(raw).expect("a client frame"));
        }
    }
    // The answer half, read from the file that holds nothing but the TypeScript server's frames.
    let answers = fs::read_to_string(vectors.join("mcp_session_ts_only.json"))
        .expect("read the TypeScript answers");
    let answers: Value = serde_json::from_str(&answers).expect("a JSON document");
    for response in answers["responses"]
        .as_array()
        .expect("the recorded answers")
    {
        tc_only.insert(
            response["id"].as_u64().expect("a numeric id").to_string(),
            serde_json::from_str(response["raw"].as_str().expect("a frame"))
                .expect("a JSON-RPC frame"),
        );
    }
    (sent, tc_only)
}

/// The recorded session as the bytes a client writes: one frame per line, in the recorded order.
fn recorded_stdin(sent: &[Value]) -> String {
    let mut stdin = String::new();
    for frame in sent {
        stdin.push_str(&serde_json::to_string(frame).expect("a client frame serialises"));
        stdin.push('\n');
    }
    stdin
}

/// Publishes one generation the way the worker does — a stage, `migrate`, one batch writing the
/// generation row plus the `meta` keys a reader reads back, then `with_stage`'s seal and rename over
/// the live file. `project_root` is the tree the answer will claim to serve, so the identity guard
/// has something to compare.
fn publish(live: &Path, generation: &str, project_root: &Path) {
    with_stage(live, false, |staged| {
        let mut scope = begin_write(&staged.path)?;
        migrate(scope.conn_mut().connection_mut())?;
        let sql = format!(
            "INSERT OR REPLACE INTO generation \
               (id, seq, published_at, base_commit, worktree_dirty, files_indexed, nodes, edges, \
                duration_ms, mode, status) \
             VALUES ('{generation}', 1, '2026-11-19T09:00:00.000Z', NULL, 0, 0, 0, 0, 12, \
                'full', 'ok');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', '{generation}');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('project_name', 'mcp-fixture');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('project_root', '{}');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '{}');",
            project_root.display(),
            schema_version(),
        );
        scope.transaction(|tx| {
            tx.execute_batch(&sql)
                .map_err(|error| AppError::internal(format!("writing {generation}: {error}")))
        })?;
        scope.close()
    })
    .expect("publish a generation");
}

#[test]
fn the_replayed_tools_list_equals_the_typescript_capture_field_by_field() {
    let machine = Machine::new("tools-list");
    let (sent, recorded) = recorded();
    let session = machine.session(&[], &recorded_stdin(&sent));
    assert_eq!(
        session.code,
        Some(0),
        "a session that ends on stdin EOF exits 0; stderr was: {}",
        session.stderr
    );

    let ours = session.answer(2);
    let theirs = recorded["2"].clone();
    let (ours, theirs) = (&ours["result"], &theirs["result"]);

    // Field for field, as parsed JSON: the members, their values and their nesting.
    assert_eq!(
        ours["tools"], theirs["tools"],
        "the published tools differ from the recorded surface"
    );
    // And as one serialisation, so a re-ordered key inside a schema is a failure too: `serde_json`'s
    // `preserve_order` feature is what keeps the manifest's declaration order on the wire.
    assert_eq!(
        serde_json::to_string(&ours["tools"]).expect("a serialisation"),
        serde_json::to_string(&theirs["tools"]).expect("a serialisation"),
        "the tools parse equal but do not serialise identically"
    );
    assert_eq!(
        ours["tools"].as_array().map(Vec::len),
        Some(TOOLS.len()),
        "the surface publishes every registered tool"
    );
}

#[test]
fn the_handshake_reports_this_process_and_the_published_guide() {
    let machine = Machine::new("handshake");
    let (sent, recorded) = recorded();
    let session = machine.session(&[], &recorded_stdin(&sent));
    let ours = session.answer(1);
    let ours = &ours["result"];
    let theirs = recorded["1"].clone();
    let theirs = &theirs["result"];

    assert_eq!(ours["protocolVersion"], theirs["protocolVersion"]);
    assert_eq!(ours["capabilities"], theirs["capabilities"]);
    assert_eq!(ours["capabilities"]["tools"], Value::Object(Map::new()));
    assert_eq!(ours["serverInfo"], theirs["serverInfo"]);
    assert_eq!(ours["serverInfo"]["name"], "sangkar");
    // The guide, byte for byte: the only long string this surface publishes.
    assert_eq!(
        ours["instructions"], theirs["instructions"],
        "the instructions text differs from the recorded one"
    );
}

#[test]
fn an_unknown_flag_is_a_usage_refusal_with_nothing_on_stdout() {
    let machine = Machine::new("usage");
    let session = machine.session(&["--mode", "fast"], "");
    assert_eq!(
        session.code,
        Some(2),
        "a verb that takes no argument refuses one"
    );
    assert!(
        session.stdout.is_empty(),
        "a refusal writes no byte to stdout: {:?}",
        String::from_utf8_lossy(&session.stdout)
    );
    assert!(
        session.stderr.contains("takes no arguments"),
        "the refusal names the reason: {}",
        session.stderr
    );
    assert!(
        session.stderr.contains("usage: sangkar mcp"),
        "the refusal shows the block: {}",
        session.stderr
    );
}

#[test]
fn stdout_carries_one_json_object_per_response_and_no_other_bytes() {
    let machine = Machine::new("stdout");
    let (sent, _) = recorded();
    let session = machine.session(&[], &recorded_stdin(&sent));

    // The whole claim: the bytes are exactly the frames, each terminated by one newline, and the
    // count matches the client's requests that expected an answer (the `notifications/initialized`
    // frame expects none).
    let requests = sent.iter().filter(|frame| frame["id"].is_u64()).count();
    let frames = session.frames();
    assert_eq!(frames.len(), requests, "one frame per request, and no more");
    let expected: usize = frames
        .iter()
        .map(|frame| {
            serde_json::to_string(frame)
                .expect("a frame serialises")
                .len()
                + 1
        })
        .sum();
    assert_eq!(
        session.stdout.len(),
        expected,
        "every byte on stdout belongs to a frame or its terminator"
    );
    assert_eq!(session.code, Some(0), "stderr was: {}", session.stderr);
}

#[test]
fn an_invalid_argument_is_the_recorded_in_band_error_envelope() {
    let machine = Machine::new("invalid-argument");
    let (sent, recorded) = recorded();
    let session = machine.session(&[], &recorded_stdin(&sent));

    // The recorded id 4: `status` with a string `budget`. A manifest refusal, so the answer comes
    // from the boundary spec — and it is a JSON-RPC *result* with `isError`, never a protocol error.
    let answer = session.answer(4);
    assert!(
        answer.get("error").is_none(),
        "a refused argument must not be a JSON-RPC error: {answer}"
    );
    let result = &answer["result"];
    assert_eq!(result["isError"], Value::Bool(true));
    assert!(
        result.get("structuredContent").is_none(),
        "the envelope is the only content: {result}"
    );
    let content = result["content"]
        .as_array()
        .expect("one content block per answer");
    assert_eq!(content.len(), 1, "exactly one text block");
    assert_eq!(content[0]["type"], "text");
    let text = content[0]["text"].as_str().expect("the text block");
    assert!(
        !text.ends_with('\n'),
        "an MCP text block carries no trailing newline"
    );
    assert_eq!(
        text,
        recorded["4"]["result"]["content"][0]["text"]
            .as_str()
            .expect("the recorded text"),
        "the refusal's envelope differs from the recorded one"
    );
    assert!(text.contains("code: internal"));
    assert!(text.contains("must be an integer"));
}

#[test]
fn an_absent_graph_is_a_bootstrapping_envelope_in_band() {
    let machine = Machine::new("absent");
    let (sent, recorded) = recorded();
    let session = machine.session(&[], &recorded_stdin(&sent));

    // The recorded id 3: `status` on a data dir with no graph. The summary embeds the path, so the
    // recording's own data dir is substituted before the comparison; every other byte is the same.
    let answer = session.answer(3);
    assert!(answer.get("error").is_none());
    let result = &answer["result"];
    assert_eq!(
        result["isError"],
        Value::Bool(true),
        "bootstrapping is a failure"
    );
    let text = result["content"][0]["text"]
        .as_str()
        .expect("the text block");
    let recorded = recorded["3"]["result"]["content"][0]["text"]
        .as_str()
        .expect("the recorded text");
    let recorded = machine.recorded_text(recorded);

    // The envelope up to its `error` object is byte-identical, which is the part this surface
    // builds. The error block carries the same three-plus-one lines in a different order here,
    // because `sangkar_core::envelope::bootstrapping` writes `hint` before `retry_after_ms` where
    // the TypeScript's `bootstrapping()` appends the hint last; that is the envelope constructor's
    // order (TASK-025), not this surface's, so the fields are compared as a set and the difference
    // is stated rather than hidden.
    let (ours_head, ours_error) = text.split_once("error:\n").expect("the recorded shape");
    let (recorded_head, recorded_error) =
        recorded.split_once("error:\n").expect("the recorded shape");
    assert_eq!(
        ours_head, recorded_head,
        "the envelope above `error` differs"
    );
    assert_eq!(
        sorted_lines(ours_error),
        sorted_lines(recorded_error),
        "the error block carries different fields"
    );
}

fn sorted_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().collect();
    lines.sort_unstable();
    lines
}

#[test]
fn an_unknown_tool_is_an_error_envelope_that_names_the_surface() {
    let machine = Machine::new("unknown-tool");
    let stdin = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-surface-test","version":"1.0.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"nosuchtool","arguments":{}}}"#
    );
    let session = machine.session(&["--json"], &stdin);
    let answer = session.answer(2);
    assert!(
        answer.get("error").is_none(),
        "an unknown tool must not be a JSON-RPC error: {answer}"
    );
    let result = &answer["result"];
    assert_eq!(result["isError"], Value::Bool(true));
    let text = result["content"][0]["text"]
        .as_str()
        .expect("the text block");
    // `--json`: the answer text is the JSON envelope, and it is still one object with no newline.
    let envelope: Value = serde_json::from_str(text).expect("the JSON envelope");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["error"]["code"], "not_found");
    assert_eq!(envelope["read_status"]["stale"], Value::Bool(true));
    assert_eq!(envelope["read_status"]["decisive"], Value::Bool(false));
    assert_eq!(envelope["coverage"]["complete"], Value::Bool(false));
    assert!(
        envelope["next_tools"]
            .as_array()
            .expect("a list")
            .is_empty()
    );
    let hint = envelope["error"]["hint"].as_str().expect("the hint");
    for tool in &TOOLS {
        assert!(
            hint.contains(tool.name),
            "the hint names `{}`: {hint}",
            tool.name
        );
    }
}

#[test]
fn a_refused_cursor_is_the_handlers_own_envelope() {
    let machine = Machine::new("cursor");
    publish(
        &machine.data_dir.join("graph.db"),
        "g-cursor01",
        &machine.tree,
    );
    let stdin = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-surface-test","version":"1.0.0"}}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"trace","arguments":{"query":"anything","cursor":"not-a-token"}}}"#
    );
    let session = machine.session(&["--json"], &stdin);
    let answer = session.answer(2);
    assert!(
        answer.get("error").is_none(),
        "a refused cursor must not be a JSON-RPC error: {answer}"
    );
    let result = &answer["result"];
    assert_eq!(result["isError"], Value::Bool(true));
    let envelope: Value = serde_json::from_str(
        result["content"][0]["text"]
            .as_str()
            .expect("the text block"),
    )
    .expect("the JSON envelope");
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["error"]["code"], "cursor_invalid");
    // The handler answered, so the envelope carries the graph it read and the route's own promise.
    assert_eq!(envelope["read_status"]["generation"], "g-cursor01");
    assert_eq!(envelope["read_status"]["stale"], Value::Bool(false));
    assert_eq!(envelope["read_status"]["decisive"], Value::Bool(true));
    assert_eq!(envelope["next_tools"], serde_json::json!(["expand"]));
}

#[test]
fn the_answer_is_toon_by_default_and_json_with_the_global() {
    let machine = Machine::new("answer-form");
    publish(
        &machine.data_dir.join("graph.db"),
        "g-form0001",
        &machine.tree,
    );
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}}"#;
    let initialize = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-surface-test","version":"1.0.0"}}}"#;
    let stdin = format!("{initialize}\n{call}\n");

    let toon = machine.session(&[], &stdin);
    let toon_text = toon.answer(2)["result"]["content"][0]["text"]
        .as_str()
        .expect("the text block")
        .to_owned();
    assert_eq!(
        toon.answer(2)["result"]["isError"],
        Value::Bool(false),
        "a published graph answers: {}",
        toon.stderr
    );
    assert!(
        toon_text.starts_with("status: ok"),
        "the default answer is TOON: {toon_text}"
    );

    let json = machine.session(&["--json"], &stdin);
    let json_text = json.answer(2)["result"]["content"][0]["text"]
        .as_str()
        .expect("the text block")
        .to_owned();
    let envelope: Value = serde_json::from_str(&json_text).expect("the JSON envelope");
    assert_eq!(envelope["status"], "ok");
    assert_eq!(envelope["read_status"]["generation"], "g-form0001");
    assert!(
        !json_text.ends_with('\n'),
        "an MCP text block carries no trailing newline"
    );
}

// -------------------------------------------------------------------------------------------
// The panic boundary: a handler that panics is one more in-band `internal`, not a dead session.
// -------------------------------------------------------------------------------------------

/// A tool whose handler panics, published under the real `status` name.
///
/// The name matters: a call is parsed against the parser table the registry derives from
/// [`TOOLS`], so a tool this build does not publish has no declared arguments to parse. Publishing
/// a panicking handler under a registered name keeps the whole call path real — the argument parse,
/// the repository selection, the store, the context — and varies only the one thing under test.
static PANICS: [RegisteredTool; 1] = [RegisteredTool {
    name: "status",
    description: "Panics, so the boundary can be tested.",
    describe: panics_descriptor,
    handle: panics_handler,
}];

fn panics_descriptor() -> Descriptor {
    Descriptor {
        params: Vec::new(),
        example: None,
    }
}

fn panics_handler(_context: &ToolContext, _args: &Args) -> Envelope<Value> {
    panic!("the handler panicked on purpose")
}

/// The surface a test drives by hand: the same construction the `mcp` verb performs, with the tool
/// table replaced by the caller's.
fn surface_of(machine: &Machine, tools: &'static [RegisteredTool]) -> Surface {
    let mut argv = vec!["mcp".to_owned()];
    argv.extend(machine.globals());
    let env = |name: &str| machine.env(name);
    let startup = config::load_config(&argv, &env, machine.home()).expect("a resolvable machine");
    let registry = sangkar_core::registry::read_registry(&startup.registry_path);
    // The same derivation the `mcp` verb performs: `--repo-root` is the process's pin, and without
    // it a call from a working directory that names no repository would be a `not_found` refusal
    // rather than the handler under test.
    let pinned_root = config::flag_value(&argv, "repo-root").map(PathBuf::from);
    Surface::new(
        startup,
        registry,
        machine.home().to_path_buf(),
        pinned_root,
        true,
    )
    .with_tools(tools)
}

/// The envelope inside one `CallToolResult`'s text block.
fn envelope_of(result: &rmcp::model::CallToolResult) -> Value {
    assert_eq!(result.content.len(), 1, "exactly one content block");
    let text = result.content[0]
        .as_text()
        .expect("a text block")
        .text
        .clone();
    serde_json::from_str(&text).expect("a JSON envelope")
}

#[test]
fn a_panicking_handler_answers_internal_and_the_surface_keeps_answering() {
    let machine = Machine::new("panic");
    publish(
        &machine.data_dir.join("graph.db"),
        "g-panic001",
        &machine.tree,
    );
    let surface = surface_of(&machine, &PANICS);

    let answer = surface.call("status", None);
    assert_eq!(answer.is_error, Some(true), "a panic is an in-band error");
    let envelope = envelope_of(&answer);
    assert_eq!(envelope["status"], "error");
    assert_eq!(envelope["error"]["code"], "internal");
    assert_eq!(
        envelope["error"]["message"], "the handler panicked on purpose",
        "the panic payload is the message the client reads"
    );
    assert_eq!(
        envelope["summary"], "status could not answer",
        "the boundary's own headline, not the handler's"
    );
    assert_eq!(envelope["read_status"]["stale"], Value::Bool(true));
    assert_eq!(
        envelope["read_status"]["stale_reason"],
        "the call failed before the graph answered"
    );
    assert_eq!(envelope["read_status"]["decisive"], Value::Bool(false));
    assert_eq!(envelope["coverage"]["complete"], Value::Bool(false));
    assert_eq!(envelope["read_status"]["generation"], "g-panic001");
    assert_eq!(
        envelope["next_tools"],
        serde_json::json!(["summary", "search"]),
        "the routing table, not the handler's guess"
    );

    // The session is not poisoned by the panic: a second panicking call is caught the same way,
    // which also proves the hook the catcher installed was put back (the second call would
    // otherwise run with a hook it never restored).
    let again = surface.call("status", None);
    assert_eq!(again.is_error, Some(true));
    assert_eq!(envelope_of(&again)["error"]["code"], "internal");

    // And an ordinary call still answers through the same surface.
    let ordinary = surface_of(&machine, &TOOLS);
    let status = ordinary.call("status", None);
    assert_eq!(status.is_error, Some(false), "status answers: {status:?}");
    assert_eq!(envelope_of(&status)["status"], "ok");
}

// -------------------------------------------------------------------------------------------
// Freshness: the graph is opened per call, so a publish mid-session is answered by the new one.
// -------------------------------------------------------------------------------------------

#[test]
fn a_publish_between_two_calls_is_answered_by_the_new_generation() {
    let machine = Machine::new("freshness");
    let mut argv = vec!["mcp".to_owned()];
    argv.extend(machine.globals());
    let mut child = Command::new(SANGGAR)
        .args(&argv)
        .env("SANGKAR_HOME", machine.home())
        .env("HOME", machine.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the server");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();

    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "mcp-surface-test", "version": "1.0.0"},
            },
        }),
    );
    assert_eq!(read_frame(&mut lines)["id"], 1);

    // Nothing published yet: the call answers `bootstrapping`.
    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {"name": "status", "arguments": {}},
        }),
    );
    let before = read_frame(&mut lines);
    assert_eq!(before["id"], 2);
    assert_eq!(before["result"]["isError"], Value::Bool(true));
    let text = before["result"]["content"][0]["text"]
        .as_str()
        .expect("the text block");
    assert!(
        text.starts_with("status: bootstrapping"),
        "the first call sees no graph: {text}"
    );

    // A worker publishes the way it really does, while the session is running. The next call opens
    // the file again, so it reads the new generation — that is the per-call open this surface chose
    // over a pooled handle, and this is the assertion that keeps the choice honest.
    publish(
        &machine.data_dir.join("graph.db"),
        "g-fresh001",
        &machine.tree,
    );

    write_frame(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {"name": "status", "arguments": {}},
        }),
    );
    let after = read_frame(&mut lines);
    assert_eq!(after["id"], 3);
    assert_eq!(after["result"]["isError"], Value::Bool(false));
    let text = after["result"]["content"][0]["text"]
        .as_str()
        .expect("the text block");
    assert!(
        text.starts_with("status: ok"),
        "the second call reads the published generation: {text}"
    );
    assert!(
        text.contains("g-fresh001"),
        "the answer names the generation: {text}"
    );

    drop(stdin);
    let status = child.wait().expect("wait for the server");
    assert_eq!(status.code(), Some(0));
}

/// One session's whole stdout, line by line.
type Frames = std::io::Lines<BufReader<std::process::ChildStdout>>;

/// Writes one JSON-RPC frame to a live session, terminated by the newline the transport frames on.
fn write_frame(stdin: &mut std::process::ChildStdin, frame: &Value) {
    stdin
        .write_all(format!("{}\n", serde_json::to_string(frame).expect("a frame")).as_bytes())
        .expect("write a frame");
    stdin.flush().expect("flush a frame");
}

/// The next response frame, read off a live session.
fn read_frame(lines: &mut Frames) -> Value {
    let line = lines
        .next()
        .expect("an answer per request")
        .expect("a readable line");
    serde_json::from_str(&line).expect("a JSON frame")
}

// -------------------------------------------------------------------------------------------
// Shutdown: SIGTERM drains the calls in flight and exits 0.
// -------------------------------------------------------------------------------------------
#[test]
fn a_signal_drains_the_session_and_exits_zero() {
    let machine = Machine::new("signal");
    let mut argv = vec!["mcp".to_owned()];
    argv.extend(machine.globals());
    let mut child = Command::new(SANGGAR)
        .args(&argv)
        .env("SANGKAR_HOME", machine.home())
        .env("HOME", machine.home())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the server");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut lines = BufReader::new(stdout).lines();

    // The handshake has to be answered before the signal, so the process is provably inside the
    // session loop when it arrives.
    stdin
        .write_all(
            format!(
                "{}\n",
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"mcp-surface-test","version":"1.0.0"}}}"#
            )
            .as_bytes(),
        )
        .expect("write the handshake");
    stdin.flush().expect("flush the handshake");
    let handshake = lines
        .next()
        .expect("a handshake answer")
        .expect("a readable handshake answer");
    let handshake: Value = serde_json::from_str(&handshake).expect("a JSON frame");
    assert_eq!(handshake["id"], 1);

    // `kill` rather than libc: the test needs one signal, and a shell utility cannot be got wrong
    // the way an unsafe `libc::kill` call can.
    let signalled = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .expect("run kill");
    assert!(signalled.success(), "kill -TERM reached pid {}", child.id());

    // Nothing else may be written, and stdin stays open: the signal, not EOF, ends this session.
    let rest: Vec<String> = lines.map(|line| line.expect("a readable line")).collect();
    let status = child.wait().expect("wait for the server");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr is piped")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    assert!(
        rest.is_empty(),
        "the session wrote more than it was asked: {rest:?}"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "a signal is a clean exit, not a kill; stderr was: {stderr}"
    );
    assert!(
        stderr.contains("mcp.signal: signal=SIGTERM"),
        "the signal was observed through the session loop: {stderr}"
    );
    assert!(
        stderr.contains("mcp.shutdown: reason=signal"),
        "the drain path ran: {stderr}"
    );
}

/// The registry's order is asserted by `tests/dispatch.rs`; this is the one assertion this file
/// needs of it, because the hint an unknown tool returns lists the names in that order.
#[test]
fn the_hint_lists_the_registry_in_published_order() {
    let machine = Machine::new("hint");
    let surface = surface_of(&machine, &TOOLS);
    let envelope = envelope_of(&surface.call("nosuchtool", None));
    let hint = envelope["error"]["hint"].as_str().expect("the hint");
    let listed: Vec<&str> = hint
        .trim_start_matches("this server registers: ")
        .split(", ")
        .collect();
    let names: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
    assert_eq!(listed, names);
}
