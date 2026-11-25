//! The process entry point (`bin/sangkar.ts`) and the tool registry it dispatches to.
//!
//! Every case drives [`dispatch`] with a stated machine — a temporary home injected through the
//! environment seam, a temporary working directory, a stdin that answers the gate's question or
//! closes without answering — and reads the two buffers instead of the process's streams. What is
//! asserted is what a caller sees: the bytes on each stream and the code the process would exit
//! with.
//!
//! Three things are pinned beyond the ported cases, because they are this port's own risk:
//!
//! - **the registry order and every schema**, against the frame the TypeScript server really sent
//!   (`rust/tests/vectors/mcp_session.ndjson`, recorded by TASK-011). The comparison is on the
//!   serialised entries, so a re-ordered key is a failure and not only a swapped value.
//! - **stdout stays empty on every failure path**, because stdout carries one rendered envelope and a
//!   refusal that wrote there would corrupt the bytes a caller parses.
//! - **the name is the first token that is not a global flag**, so `--repo-root <tree> status` and
//!   `status --repo-root <tree>` are one command.

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_cli::dispatch::{Invocation, USAGE, dispatch};
use sangkar_core::errors::AppError;
use sangkar_core::instructions::TOOL_NAMES;
use sangkar_store::migrate::migrate;
use sangkar_store::migrations::schema_version;
use sangkar_store::publish::{begin_write, with_stage};
use sangkar_tools::registry::TOOLS;
use serde_json::Value;

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
            "sangkar-dispatch-{label}-{}-{nanos}-{}",
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

/// What one invocation answered.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

/// One machine: a home whose settings file and registry are the run's, and the trees a case makes in
/// it.
struct Machine {
    root: TempDir,
}

impl Machine {
    fn new(label: &str) -> Self {
        Self {
            root: TempDir::new(label),
        }
    }

    fn home(&self) -> &Path {
        self.root.path()
    }

    /// A tree that looks like a repository, so autodetection stops there instead of walking out of
    /// the temporary root.
    fn tree(&self, name: &str) -> PathBuf {
        let tree = self.home().join(name);
        fs::create_dir_all(tree.join(".git")).expect("create the tree");
        tree
    }

    /// One run of the dispatcher in `cwd`, with no terminal and a stdin that closes without an
    /// answer — the answer a script gives.
    fn run_in(&self, cwd: &Path, argv: &[&str]) -> Run {
        self.run_with(cwd, argv, "")
    }

    /// One run in the machine's home, with `stdin` as the bytes the gate may read.
    fn run_with(&self, cwd: &Path, argv: &[&str], stdin: &str) -> Run {
        let home = self.home().display().to_string();
        let env = |name: &str| match name {
            "SANGKAR_HOME" | "HOME" => Some(home.clone()),
            _ => None,
        };
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        let mut answered = Cursor::new(stdin.as_bytes().to_vec());
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = dispatch(&mut Invocation {
            argv,
            env: &env,
            cwd: cwd.to_path_buf(),
            tty: false,
            stdin: &mut answered,
            stdout: &mut stdout,
            stderr: &mut stderr,
        });
        Run {
            code,
            stdout: String::from_utf8(stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(stderr).expect("stderr is UTF-8"),
        }
    }
}

/// Publishes one generation the way the worker does — a stage, `migrate`, one batch writing the
/// generation row plus the `meta` keys a reader reads back, then `with_stage`'s seal and rename over
/// the live file.
///
/// A real publication rather than a stub: the point of the tool case below is that the handler
/// answers from the file the composition root opened, and a fake store would prove the fake.
fn publish(live: &Path, generation: &str) {
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
             INSERT OR REPLACE INTO meta (key, value) VALUES ('project_name', 'dispatch-fixture');\
             INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '{}');",
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

/// A successful run's stdout as the JSON envelope it is.
fn envelope_of(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).expect("the answer is one JSON document")
}

#[test]
fn the_name_is_the_first_token_that_is_not_a_global_flag() {
    let machine = Machine::new("globals");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let before = machine.run_in(machine.home(), &["--repo-root", &arg, "status"]);
    let after = machine.run_in(machine.home(), &["status", "--repo-root", &arg]);
    assert_eq!(
        before.code, after.code,
        "a global's position changes nothing"
    );
    assert_eq!(before.stdout, after.stdout);

    // And the same for a verb: `repo` receives the whole line minus its own name, whatever the
    // order of the flags around it.
    let verb_before = machine.run_in(machine.home(), &["--json", "repo", "list"]);
    let verb_after = machine.run_in(machine.home(), &["repo", "list", "--json"]);
    assert_eq!(verb_before.code, 0);
    assert_eq!(verb_before.stdout, verb_after.stdout);
    assert_eq!(
        verb_before.stdout.trim_end(),
        r#"{"version":1,"repos":[]}"#,
        "the registry verb answers the JSON document for a machine with no entries"
    );
}

#[test]
fn every_reserved_name_reaches_its_own_module() {
    let machine = Machine::new("verbs");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    // `repo` is a real module: the registry document is the answer.
    let repo = machine.run_in(machine.home(), &["repo", "list", "--json"]);
    assert_eq!(repo.code, 0);
    assert_eq!(repo.stdout.trim_end(), r#"{"version":1,"repos":[]}"#);

    // `index` is a real module: the gate refuses a tree nothing vouched for, and says nothing on
    // stdout. No worker is started, which is the point of asking first.
    let deeper = tree.join("sub");
    fs::create_dir_all(&deeper).expect("create the nested directory");
    let index = machine.run_in(&deeper, &["index", "--data-dir", &arg]);
    assert_eq!(index.code, 2, "a gate refusal is a usage exit");
    assert!(index.stdout.is_empty());
    assert!(
        index.stderr.contains("is not registered as a repository"),
        "the gate names what it refused: {}",
        index.stderr
    );

    // The three whose module is a later task refuse by name: one line, the task that lands it, and
    // no invented protocol on stdout.
    for (verb, task) in [
        ("worker", "TASK-046"),
        ("telemetry", "TASK-054"),
        ("daemon", "TASK-056"),
    ] {
        let run = machine.run_in(machine.home(), &[verb]);
        assert_eq!(run.code, 1, "`{verb}` refuses");
        assert!(run.stdout.is_empty(), "`{verb}` writes nothing to stdout");
        assert!(
            run.stderr.contains(task),
            "`{verb}` names {task}: {}",
            run.stderr
        );
    }

    // `mcp` is a real module too, and the fourth of the group stopped being one: a client that
    // closes stdin before saying hello asked for nothing, so the server answers exit 0 rather than a
    // refusal that names a task. Nothing reaches stdout either way — there was no request to answer.
    let mcp = machine.run_in(machine.home(), &["mcp"]);
    assert_eq!(mcp.code, 0, "a client-gone stdin is not a failure");
    assert!(mcp.stdout.is_empty(), "no request, no frame");
}

#[test]
fn an_unknown_name_exits_two_with_the_banner_on_stderr_and_nothing_on_stdout() {
    let machine = Machine::new("bogus");
    let run = machine.run_in(machine.home(), &["bogus"]);
    assert_eq!(run.code, 2);
    assert!(run.stdout.is_empty(), "a usage refusal writes no stdout");
    assert_eq!(
        run.stderr,
        format!("sangkar: unknown tool \"bogus\"\n{USAGE}"),
        "the reason, then the whole surface, and no other byte"
    );
}

#[test]
fn no_failure_path_writes_a_byte_to_stdout() {
    let machine = Machine::new("silent");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();
    let deeper = tree.join("nested");
    fs::create_dir_all(&deeper).expect("create the nested directory");

    let cases: [(&[&str], &Path); 5] = [
        (&["nosuchtool"], machine.home()),
        (&["daemon"], machine.home()),
        // A tool named wrongly: the parser's refusal plus the banner, both on stderr.
        (&["trace", "--repo-root", &arg], machine.home()),
        (
            &["status", "--nonsense", "--repo-root", &arg],
            machine.home(),
        ),
        // A refused index run: the gate's three lines, on stderr.
        (&["index", "--data-dir", &arg], &deeper),
    ];
    for (argv, cwd) in cases {
        let run = machine.run_in(cwd, argv);
        assert!(
            run.stdout.is_empty(),
            "`{}` wrote {} bytes to stdout: {:?}",
            argv.join(" "),
            run.stdout.len(),
            run.stdout
        );
        assert!(
            !run.stderr.is_empty(),
            "`{}` refused without saying why",
            argv.join(" ")
        );
        assert!(
            run.code == 1 || run.code == 2,
            "`{}` exited {}: a failure is 1 or 2",
            argv.join(" "),
            run.code
        );
    }
}

#[test]
fn a_tool_name_resolves_to_the_registry_and_answers_from_the_published_graph() {
    let machine = Machine::new("tool");
    let tree = machine.tree("tree");
    let data_dir = tree.join(".sangkar");
    fs::create_dir_all(&data_dir).expect("create the data directory");
    publish(&data_dir.join("graph.db"), "g-d00001");

    // `status` is the orientation tool: the cheapest answer that proves the whole path — the config,
    // the selection, the store, the context, the handler, the rendering, the exit code.
    let run = machine.run_in(
        machine.home(),
        &[
            "status",
            "--repo-root",
            &tree.display().to_string(),
            "--json",
        ],
    );
    assert_eq!(run.code, 0, "stderr was: {}", run.stderr);
    let envelope = envelope_of(&run);
    assert_eq!(envelope["status"], "ok");
    assert_eq!(
        envelope["read_status"]["generation"], "g-d00001",
        "the handler answered from the file the composition root opened"
    );
    assert!(
        envelope["payload"].is_object(),
        "an ok answer carries the status payload"
    );

    // A name that is not in the registry is refused before anything is opened, and the reason is
    // the only thing on the two streams.
    let unknown = machine.run_in(
        machine.home(),
        &["nosuchtool", "--repo-root", &tree.display().to_string()],
    );
    assert_eq!(unknown.code, 2);
    assert!(
        unknown
            .stderr
            .contains("sangkar: unknown tool \"nosuchtool\"")
    );
}

#[test]
fn a_missing_graph_is_bootstrapping_for_every_tool_but_status() {
    let machine = Machine::new("absent");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    // `status` is the one tool that answers an empty tree instead of failing: "no index yet" is its
    // whole job, and the line is deliberately not an envelope — `--json` included.
    let status = machine.run_in(machine.home(), &["status", "--repo-root", &arg, "--json"]);
    assert_eq!(status.code, 0);
    assert!(
        status.stdout.contains("no index at"),
        "the bare line names the file: {}",
        status.stdout
    );
    assert!(
        serde_json::from_str::<Value>(&status.stdout).is_err(),
        "the absent line is not an envelope: {}",
        status.stdout
    );

    // Every other tool answers a `bootstrapping` envelope and exits 1: it could not read a graph,
    // so it has not answered the question it was asked.
    let search = machine.run_in(
        machine.home(),
        &["search", "anything", "--repo-root", &arg, "--json"],
    );
    assert_eq!(search.code, 1);
    let envelope = envelope_of(&search);
    assert_eq!(envelope["status"], "bootstrapping");
    assert_eq!(envelope["error"]["code"], "bootstrapping");
    assert_eq!(envelope["error"]["retry_after_ms"], 5_000);
    assert_eq!(envelope["read_status"]["decisive"], false);
    assert_eq!(
        envelope["next_tools"][0], "trace",
        "the absence answer carries the routing promise route() will reassert"
    );
}

#[test]
fn the_gate_accepts_a_named_tree_and_the_worker_returns_its_own_code() {
    let machine = Machine::new("index");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();
    let data_dir = tree.join(".sangkar");

    // A named tree is never questioned; the child is `/bin/true`, so the verb's code is the child's
    // own `0` and no index run happens in this process. `SANGKAR_WORKER_CMD` is the one seam that
    // names the child, which is exactly why the verb reads it through the config.
    let argv = ["index", "--repo-root", &arg, "--no-register"];
    let home = machine.home().display().to_string();
    let env = |name: &str| match name {
        "SANGKAR_HOME" | "HOME" => Some(home.clone()),
        "SANGKAR_WORKER_CMD" => Some("/bin/true".to_owned()),
        _ => None,
    };
    let argv: Vec<String> = argv.iter().map(|token| (*token).to_owned()).collect();
    let mut stdin = Cursor::new(Vec::new());
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let code = dispatch(&mut Invocation {
        argv,
        env: &env,
        cwd: tree.clone(),
        tty: false,
        stdin: &mut stdin,
        stdout: &mut stdout,
        stderr: &mut stderr,
    });
    assert_eq!(code, 0, "the child's own status is the verb's answer");
    assert!(stdout.is_empty(), "the verb writes no envelope");
    assert!(!data_dir.join("graph.db").exists(), "nothing was indexed");
}

#[test]
fn help_answers_on_stdout_and_a_refusal_answers_on_stderr() {
    let machine = Machine::new("help");

    let surface = machine.run_in(machine.home(), &["--help"]);
    assert_eq!(surface.code, 0);
    assert_eq!(surface.stdout, USAGE);
    assert!(surface.stderr.is_empty());

    // `sangkar help` is the same request spelled with a word.
    let spelled = machine.run_in(machine.home(), &["help"]);
    assert_eq!(spelled.code, 0);
    assert_eq!(spelled.stdout, USAGE);

    // A verb's block, answered before anything runs: `index --help` must not start a run.
    let verb = machine.run_in(machine.home(), &["index", "--help"]);
    assert_eq!(verb.code, 0);
    assert!(verb.stdout.starts_with("usage: sangkar index "));
    assert!(verb.stdout.contains("--yes"));

    // A tool's block, generated from the manifest the registry publishes.
    let tool = machine.run_in(machine.home(), &["trace", "--help"]);
    assert_eq!(tool.code, 0);
    assert!(tool.stdout.starts_with("sangkar trace [<query>] [options]"));
    assert!(tool.stdout.contains("--depth <integer>"));
}

#[test]
fn the_registry_order_and_every_schema_match_the_recorded_tools_list_frame() {
    let vector = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("tests")
        .join("vectors")
        .join("mcp_session.ndjson");
    let session = fs::read_to_string(&vector).expect("read the recorded session");
    let mut listed: Option<Value> = None;
    for line in session.lines().filter(|line| !line.trim().is_empty()) {
        let record: Value = serde_json::from_str(line).expect("a recorded line");
        if record["dir"] != "s2c" {
            continue;
        }
        let raw = record["raw"].as_str().expect("a recorded frame");
        let frame: Value = serde_json::from_str(raw).expect("a JSON-RPC frame");
        if frame["result"]["tools"].is_array() {
            listed = Some(frame["result"]["tools"].clone());
            break;
        }
    }
    let recorded = listed
        .expect("the session carries a tools/list answer")
        .as_array()
        .cloned();
    let recorded = recorded.expect("tools/list answers an array");

    let names: Vec<&str> = TOOLS.iter().map(|tool| tool.name).collect();
    assert_eq!(
        names,
        TOOL_NAMES.to_vec(),
        "the registry order is the published surface's order"
    );
    assert_eq!(
        recorded.len(),
        TOOLS.len(),
        "the recorded surface has seven tools"
    );

    for (index, (frame, tool)) in recorded.iter().zip(TOOLS.iter()).enumerate() {
        assert_eq!(
            frame["name"].as_str(),
            Some(tool.name),
            "the recorded tool at {index} is {}",
            tool.name
        );
        let derived = tool.descriptor();
        // Serialised, not just compared as a value: `preserve_order` makes the string a statement
        // about the key order the TypeScript sent, which a map comparison would not make.
        assert_eq!(
            serde_json::to_string(frame).expect("the recorded entry"),
            serde_json::to_string(&derived).expect("the derived entry"),
            "the {} descriptor differs from the recorded frame",
            tool.name
        );
    }
}
