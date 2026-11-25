//! The process entry point: six reserved verbs, every other first token a tool name, and the
//! composition root the tool path runs through.
//!
//! Ported from `bin/sangkar.ts` and the composition half of `src/index.ts`. The two rules that shape
//! this file are both about streams:
//!
//! - **A refusal writes the usage block and nothing else.** A verb's own block comes from
//!   [`crate::args::verb_spec`] — one copy, pinned character for character by
//!   `tests/args_render.rs` — and the tool surface's banner is [`USAGE`]. A failure path writes no
//!   byte to stdout: stdout carries one rendered envelope, or the answer a verb owns.
//! - **The name is the first token that is not a global flag.** [`crate::args::resolve_command`]
//!   finds it, and the verbs receive the whole line minus that token, globals still in it, so
//!   `sangkar --json status` and `sangkar status --json` name the same command.
//!
//! # The six verbs
//!
//! `mcp`, `worker`, `index`, `telemetry`, `daemon`, `repo` are reserved; anything else is a tool
//! name, and a name that is neither is a usage refusal. `index` is both on purpose: the verb is the
//! bootstrap (a fresh tree has no store for a tool call to answer from) and the tool is what a
//! client calls, with identical arguments.
//!
//! One verb has no module yet — `daemon` (TASK-056) — and that arm refuses **honestly**: one line on
//! stderr naming the task that implements it, [`EXIT_FAILURE`], and no invented protocol. Faking a
//! surface here would hide the missing work behind a stream a client parses. The rest all answer:
//! `mcp` serves the stdio session in [`crate::mcp`], `worker` is the run `index` spawns, and
//! `telemetry` writes or serves the report in [`crate::telemetry_cmd`].
//!
//! # The composition root
//!
//! A tool call resolves its configuration once ([`config::load_config`]), selects the repository the
//! call names ([`sangkar_core::registry::select_repo`]), opens the published graph
//! ([`crate::store_lifecycle::open_store`]), builds the [`ToolContext`] the handler receives, calls
//! it, renders one envelope and exits with what that envelope earned. A missing graph is not an
//! error: `status` answers its bare "no index at … yet" line and exits 0, and every other tool
//! answers a `bootstrapping` envelope and exits 1 — the first session before the first publish is
//! normal, and every handler already knows how to answer that.
//!
//! The post-conditions the MCP surface applies to every handler answer (`next_tools`, the
//! `root_mismatch` disclosure, `decisive`) belong to [`crate::route`], and both surfaces apply them:
//! this path routes the handler's envelope exactly as `src/index.ts:1237` does, so a CLI answer and a
//! protocol answer differ in their envelope, not in their composition. The two answers this file
//! builds itself — the absence envelope and the refusals — carry `next_tools` and `decisive` already,
//! because they are built here rather than by a handler.
//!
//! # Exit codes
//!
//! `0` success, `1` a failure (a refusal, a bootstrapping answer, an error envelope), `2` the argv or
//! the configuration (an unknown verb, a bad flag, a repo-selection or index-gate refusal). There is
//! no fourth code: the missing-interpreter preflight this binary never had is gone, and what is left
//! of it is the npm launcher shim's own failure, outside this crate.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::LazyLock;

use sangkar_core::config::{self, AppConfig, Env, flag_value};
use sangkar_core::cursor::Cursor;
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, TestCensus, bootstrapping,
};
use sangkar_core::errors::AppError;
use sangkar_core::global_config;
use sangkar_core::instructions;
use sangkar_core::log;
use sangkar_core::registry::{self as core_registry, SelectRepoInput, canonical_root};
use sangkar_store::pool::DATABASE_FILE;
use sangkar_store::read::context::{Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};
use sangkar_tools::registry::{
    self as tools, Default as ToolDefault, Descriptor, Items as ToolItems, Kind, RegisteredParam,
};
use sangkar_tools::status::manifest::STATUS_RETRY_AFTER_MS;
use serde_json::Value;

use crate::args::{
    CliUsageError, DefaultValue, InputField, InputLiteral, Items, Verb, parse_cli_args, parse_verb,
    resolve_command, split_globals, usage_of, verb_spec,
};
use crate::index_gate::{confirm_tree, gate_input};
use crate::render::{EXIT_FAILURE, EXIT_USAGE, exit_code_of, render_output};
use crate::repo::run_repo_cli;
use crate::store_lifecycle::{Store, StoreFailure, absent_reason, open_store};

/// The whole surface: what `sangkar --help` answers, and the banner an unknown name refuses with.
///
/// Byte for byte `bin/sangkar.ts`'s `USAGE` — the verbs, the seven tools and the global flags, in
/// that order — because it is the one text a caller sees before it knows what to ask for. It is a
/// `&'static str` with the template literal's own newlines, trailing one included, so writing it
/// needs no separator of its own.
pub const USAGE: &str = "\
sangkar <name> [arguments] [global flags] — the name comes first.

verbs
  mcp     MCP server on stdio (default): the tool surface a client talks to
  worker  index worker process, spawned by the server
  index   index the repository; also a tool, with identical arguments
  telemetry  write a self-contained HTML report of the recorded usage (local only; recording is on by default)
  daemon  run, start, stop or inspect the daemon that watches the registered repositories
  repo    add, remove, list or configure the repositories this installation serves

tools
  status   report index status as a TOON envelope (JSON with --json)
  summary  summarize the current situation in a few hundred tokens
  trace    follow a call chain from one symbol
  search   find symbols or text in the indexed code
  impact   list what a change set can reach
  expand   fetch more detail for one item from a previous result
  index    index the repository (the verb above)

global flags (either side of the name: the name is the first token that is not one of them)
  --repo-root <path>  the tree to index and serve
  --data-dir <path>   where the graph is stored
  --json              print JSON instead of the default TOON envelope

examples
  sangkar trace 'handleRequest' --depth 1
  sangkar mcp --repo-root /path/to/repo
  sangkar telemetry --since 7d
  sangkar repo add .
  sangkar daemon status

sangkar index is how the first index is created; until then every tool answers bootstrapping.
";

/// One process run: the command line after the program name, the machine, the terminal and the three
/// streams.
///
/// Everything ambient is injected for the same reason [`sangkar_core::config`] injects its
/// environment lookup: a test states a machine — a temporary home, a working directory, a stdin that
/// answers a question — instead of editing the process it shares with every other test. The binary
/// passes the real handles; nothing in this crate opens them itself.
pub struct Invocation<'a> {
    /// The tokens after the program name, as the shell split them.
    pub argv: Vec<String>,
    /// The environment lookup: `SANGKAR_HOME`, `HOME`, `XDG_CONFIG_HOME`, `SANGKAR_CURSOR_KEY`, …
    pub env: Env<'a>,
    /// The working directory, which is also the tree autodetection starts from.
    pub cwd: PathBuf,
    /// Whether stdin is a terminal: the index gate asks its question only when it can.
    pub tty: bool,
    /// Where the index gate reads its answer.
    pub stdin: &'a mut dyn std::io::BufRead,
    /// One rendered envelope, or the answer a verb owns.
    pub stdout: &'a mut dyn std::io::Write,
    /// Every diagnostic, every refusal and every usage block.
    pub stderr: &'a mut dyn std::io::Write,
}

/// One run: resolve the name, answer `--help`, dispatch to the verb or the tool, and hand back the
/// code the process exits with.
///
/// The order is the TypeScript's and each step is a decision it made deliberately:
///
/// 1. `--help` with no command names the whole surface, and `help` is the same request spelled with
///    a word — both answered before anything is loaded or created;
/// 2. a reserved verb's `--help` answers that verb's block and nothing runs, so `sangkar index
///    --help` cannot start an index run;
/// 3. the machine-level settings file is created before any verb reads it: a home without one is a
///    fresh installation, not a broken one, and a `--help` above never writes to the config
///    directory;
/// 4. the verb, then the tool, and only a *tool* usage exit earns the surface banner — a verb owns
///    its own meaning of 2 (`index` reports the worker's nothing-to-do), so printing the banner for
///    it would tell an operator their command was malformed when the run actually succeeded.
pub fn dispatch(run: &mut Invocation<'_>) -> i32 {
    // The line is read many times — the resolver, the verb tail, the config lookup and each verb's
    // own flag reader — and the invocation owns it, so one clone is cheaper than threading a borrow
    // of one field through a call that needs the whole struct.
    let argv = run.argv.clone();
    let resolution = resolve_command(&argv);
    let requested = resolution
        .name
        .clone()
        .unwrap_or_else(|| Verb::Mcp.name().to_owned());
    let verb = Verb::from_name(&requested);

    if (resolution.help && resolution.name.is_none()) || requested == "help" {
        return write(run.stdout, USAGE.as_bytes(), 0);
    }
    if let Some(verb) = verb.filter(|_| resolution.help) {
        let block = format!("{}\n", verb_spec(verb).usage);
        return write(run.stdout, block.as_bytes(), 0);
    }

    // Just the home, not a whole `AppConfig`: loading the configuration here would read the file
    // again and repeat its warnings once per verb, and the verb below resolves its own.
    // Whether it wrote the file is not this process's answer to report: a home it may not write is
    // one warning from the writer and the defaults, not a refusal.
    let _ = global_config::ensure_global_config(&config::resolve_home(run.env));

    // The whole line minus the command's own name. `rest` is always a suffix of `argv`, so the name
    // sits at `argv.len() - rest.len() - 1`; handing the verbs this line rather than the tail is what
    // makes a global's position irrelevant to every command, not only to the tools.
    let verb_argv = line_without_name(&argv, &resolution.rest, resolution.name.is_some());

    if let Some(verb) = verb {
        return run_verb(run, verb, &verb_argv);
    }
    let code = run_tool(run, &requested, &argv);
    if code == EXIT_USAGE {
        // 2 is the usage exit code for a tool name that was called wrongly, and the refusal above
        // already wrote its reason: the banner completes it.
        let _ = run.stderr.write_all(USAGE.as_bytes());
        let _ = run.stderr.flush();
    }
    code
}

/// The command line with the command's own token removed, every other token left in place.
fn line_without_name(argv: &[String], rest: &[String], named: bool) -> Vec<String> {
    if !named {
        return rest.to_vec();
    }
    let cut = argv.len() - rest.len() - 1;
    let mut line = argv[..cut].to_vec();
    line.extend(rest.iter().cloned());
    line
}

/// The six verbs, each reached by its own name.
///
/// `repo` is the machine registry and one tree's settings, a verb with no graph to load; `index` is
/// the gate plus the run; `mcp` is the stdio session; `worker` is the run itself, the process
/// `index` spawns; `telemetry` writes or serves the report ([`crate::telemetry_cmd`]). `daemon` has
/// no module yet, and says which task lands it rather than pretending to answer.
fn run_verb(run: &mut Invocation<'_>, verb: Verb, verb_argv: &[String]) -> i32 {
    match verb {
        Verb::Repo => run_repo_cli(verb_argv, run.env, &run.cwd, run.stdout, run.stderr),
        Verb::Index => run_index(run, verb_argv),
        Verb::Mcp => run_mcp(run, verb_argv),
        Verb::Worker => run_worker_verb(run, verb_argv),
        // Named in full rather than imported: this is the one arm that reaches the module, and a
        // `use` above would suggest the rest of the dispatch reads it too.
        Verb::Telemetry => crate::telemetry_cmd::run_telemetry(
            verb_argv, run.env, &run.cwd, run.stdout, run.stderr,
        ),
        Verb::Daemon => not_yet(run.stderr, "the daemon verb", "TASK-056"),
    }
}

/// The `worker` verb: the index run in this process, which is what `index` spawns as a child.
///
/// No gate and no question: this process is started for a tree another process already confirmed,
/// and a prompt here would wait on a stdin nobody is holding. The exit code is the run's own —
/// `0` published, `1` failed, `2` nothing to do, `75` another indexer holds the lock — and nothing
/// is written to stdout, because the caller either renders its own answer or is a session whose
/// stdout carries a protocol.
fn run_worker_verb(run: &mut Invocation<'_>, verb_argv: &[String]) -> i32 {
    let invocation = match parse_verb(Verb::Worker, verb_argv) {
        Ok(invocation) => invocation,
        Err(error) => return usage_refusal(run.stderr, &error),
    };
    let config = match load(run, verb_argv) {
        Ok(config) => config,
        Err(code) => return code,
    };
    // `--mode` is a declared enum, so a typo cannot reach here; an absent one is `None`, which the
    // worker resolves to `full` on a forced run and `fast` otherwise.
    let mode = match invocation.args.text("mode") {
        Some("full") => Some(sangkar_index::worker::WorkerMode::Full),
        Some("fast") => Some(sangkar_index::worker::WorkerMode::Fast),
        _ => None,
    };
    let options = sangkar_index::worker::WorkerOptions {
        repo_root: config.repo_root,
        data_dir: config.data_dir,
        force: invocation.args.switch("force"),
        mode,
    };
    sangkar_index::worker::run_worker(&options).code()
}

/// The `mcp` verb: the stdio server, or the usage refusal that keeps it off a stream a client would
/// parse.
///
/// The globals this verb acts on are the ones the surface itself is built from — `--json` (the
/// answer form) and `--repo-root`/`--data-dir` (through the configuration) — and every other token
/// is refused here, before a byte of protocol is written. The server owns stdin and stdout from
/// there: the injected streams are the process's own handles, and the only thing this file does
/// with them is leave them alone.
fn run_mcp(run: &mut Invocation<'_>, verb_argv: &[String]) -> i32 {
    let globals = match split_globals(verb_argv) {
        Ok(globals) => globals,
        Err(error) => return usage_refusal(run.stderr, &error),
    };
    // The verb takes no argument of its own. The TypeScript server ignores a leftover token; this
    // one refuses it, because an operator who typed `--jsonn` or `--mode fast` meant to configure
    // something and a silently ignored flag is the failure they would only notice later. Every
    // other implemented verb refuses the same way.
    if let Some(unexpected) = globals.rest.first() {
        let error = CliUsageError::new(
            format!("`sangkar mcp` takes no arguments, but the line carries \"{unexpected}\""),
            verb_spec(Verb::Mcp).usage,
        );
        return usage_refusal(run.stderr, &error);
    }
    let config = match load(run, verb_argv) {
        Ok(config) => config,
        Err(code) => return code,
    };
    // Read once per process, exactly as the tool path does: a call's `repo` resolves against this
    // snapshot, and `--repo-root` is only the default a call that names none falls back to.
    let registry = core_registry::read_registry(&config.registry_path);
    let pinned = flag_value(verb_argv, "repo-root").map(PathBuf::from);
    let surface = crate::mcp::Surface::new(config, registry, run.cwd.clone(), pinned, globals.json);
    crate::mcp::serve(surface)
}

/// What a verb with no module answers today: one line naming the task, [`EXIT_FAILURE`], no stdout.
///
/// This is not a stub of the surface — there is no protocol here to be wrong about. A script that
/// called `sangkar daemon` and got a plausible-looking answer would believe a daemon replied.
fn not_yet(err: &mut dyn std::io::Write, what: &str, task: &str) -> i32 {
    let _ = writeln!(
        err,
        "sangkar: {what} is not implemented in this binary yet; {task} lands it"
    );
    EXIT_FAILURE
}

/// The `index` verb: the gate first, then the run, then the registration a successful run earns.
///
/// Nothing is guessed before the gate answers: a run nobody meant leaves no database, no registry
/// entry and no telemetry behind, which is why [`confirm_tree`] runs before the worker is started
/// rather than after it failed.
fn run_index(run: &mut Invocation<'_>, verb_argv: &[String]) -> i32 {
    let invocation = match parse_verb(Verb::Index, verb_argv) {
        Ok(invocation) => invocation,
        Err(error) => return usage_refusal(run.stderr, &error),
    };
    let config = match load(run, verb_argv) {
        Ok(config) => config,
        Err(code) => return code,
    };
    let gate = gate_input(&config, &run.cwd, invocation.args.switch("yes"), run.tty);
    if let Some(code) = confirm_tree(&gate, &mut run.stdin, &mut run.stderr) {
        return code;
    }
    // An unrecognised mode is impossible here (`--mode` is a declared enum), and the worker's own
    // `full` default is what the caller who named none gets.
    let mode = invocation.args.text("mode").unwrap_or("full");
    let code = match start_worker(&config, mode, invocation.args.switch("force")) {
        Ok(code) => code,
        Err(error) => return failure_refusal(run.stderr, &error, EXIT_FAILURE),
    };
    // Only the verb registers, and only a run that published or found nothing to do: `worker` is
    // spawned for a tree the server already knows, and a registration of a failed run would add a
    // repository the next call cannot read. A refused registration never changes the run's code.
    if matches!(code, 0 | 2) && !invocation.args.switch("no-register") {
        let registered = core_registry::register_repo(&config.registry_path, &config.repo_root);
        log::info(
            &format!("index.registered: added={}", registered.added),
            &[],
        );
    }
    code
}

/// The child command today: the same invocation `sangkar_tools::index::handler` builds for the
/// `index` tool, spawned rather than run in a context.
///
/// It is not shared with that handler because the handler's builder takes a [`ToolContext`], and this
/// verb is the one path that must work with *no* graph to open — a fresh tree is what it is for.
/// `AppConfig::worker_cmd` defaults to this executable, whose `worker` arm is TASK-046's, and
/// `SANGKAR_WORKER_CMD` points it at a worker this build does not carry yet.
fn start_worker(config: &AppConfig, mode: &str, force: bool) -> Result<i32, AppError> {
    let mut configured = config.worker_cmd.iter();
    let Some(program) = configured.next() else {
        return Err(
            AppError::internal("no worker command is configured").with_hint(
                "set SANGKAR_WORKER_CMD, or run the binary that carries its own worker mode",
            ),
        );
    };
    let mut command = std::process::Command::new(program);
    command
        .args(configured)
        .arg("worker")
        .arg("--repo-root")
        .arg(&config.repo_root)
        .arg("--data-dir")
        .arg(&config.data_dir)
        .arg("--mode")
        .arg(mode);
    if force {
        command.arg("--force");
    }
    // Inherited, not captured: the child's stderr is the run's whole report, and its stdout is
    // nothing by contract (a worker that printed there would corrupt the session that spawned it).
    command
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit());
    match command.status() {
        Ok(status) => Ok(status.code().unwrap_or(EXIT_FAILURE)),
        Err(error) => Err(
            AppError::internal(format!("could not start the index worker: {error}"))
                .with_hint("check SANGKAR_WORKER_CMD, or run `sangkar index` on a build with it"),
        ),
    }
}

/// One tool invocation from the command line: the same manifest, the same store guard and the same
/// envelope a protocol call gets, rendered once to stdout.
fn run_tool(run: &mut Invocation<'_>, name: &str, argv: &[String]) -> i32 {
    let Some(tool) = tools::find(name) else {
        let _ = writeln!(run.stderr, "sangkar: unknown tool \"{name}\"");
        return EXIT_USAGE;
    };

    let globals = match split_globals(argv) {
        Ok(globals) => globals,
        Err(error) => return usage_refusal(run.stderr, &error),
    };
    // Answered before the arguments are validated, because a usage block is exactly what a caller
    // asking for help wants even when a required argument is still missing.
    if globals.help {
        let block = format!("{}\n", usage_of(name, input_table(tool)));
        return write(run.stdout, block.as_bytes(), 0);
    }
    // `rest` still holds the command's own name when the caller wrote it first; the parser reads
    // arguments, and the name is not one.
    let tool_argv = match globals.rest.first() {
        Some(first) if first == name => &globals.rest[1..],
        _ => &globals.rest[..],
    };
    let parsed = match parse_cli_args(name, input_table(tool), tool_argv) {
        Ok(parsed) => parsed,
        Err(error) => return usage_refusal(run.stderr, &error),
    };

    let startup = match load(run, argv) {
        Ok(config) => config,
        Err(code) => return code,
    };
    // Read once per process: a call's `repo` resolves against this snapshot, and `--repo-root` is
    // only the default a call that names none falls back to.
    let registry = core_registry::read_registry(&startup.registry_path);
    let pinned = flag_value(argv, "repo-root").map(PathBuf::from);
    let requested = parsed.get("repo").and_then(Value::as_str);
    let mut selection = SelectRepoInput::new(&registry, &run.cwd);
    if let Some(requested) = requested {
        selection = selection.requested(requested);
    }
    if let Some(pinned) = pinned.as_deref() {
        selection = selection.pinned_root(pinned);
    }
    let repo_root = match core_registry::select_repo(&selection) {
        Ok(root) => root,
        // No repository this installation can answer for: the argv is what is wrong, so this is the
        // usage exit — the same code every refusal that happens before work starts earns.
        Err(error) => return failure_refusal(run.stderr, &error, EXIT_USAGE),
    };
    // The selected tree, not the startup one: an explicit `repo` outranks the pin, so the store
    // opened must be the one the call named. An explicit `--data-dir` survives the rebind for the
    // reason `Surface::repo_config` keeps it (`mcp.rs:327-338`): it was said about the tree being
    // served, and rebuilding the default from the root would send this call to `.sangkar/graph.db`
    // while the graph the caller named sat where they put it. Relative values resolve against the
    // same root `load_config` resolves them against.
    let explicit_data_dir = flag_value(argv, "data-dir")
        .filter(|value| !value.is_empty())
        .map(|raw| config::resolve_path(&repo_root, Path::new(&raw)));
    let config = config::with_repo(&startup, &repo_root, explicit_data_dir.as_deref());

    let store = match open_store(&config.data_dir) {
        Ok(store) => Rc::new(store),
        Err(StoreFailure::Missing) => return absent(run, name, &config, globals.json),
        Err(failure) => {
            let error = failure.error().unwrap_or_else(|| {
                AppError::internal(format!("the graph could not be opened: {failure:?}"))
            });
            return failure_refusal(run.stderr, &error, EXIT_FAILURE);
        }
    };
    let context = match context_of(&config, Rc::clone(&store), name) {
        Ok(context) => context,
        Err(error) => return failure_refusal(run.stderr, &error, EXIT_FAILURE),
    };

    let envelope = (tool.handle)(&context, &parsed);
    // The composition root's post-conditions, applied where the TypeScript applies them
    // (`src/index.ts:1237`): `next_tools` from the routing table and `decisive` derived from the
    // staleness that survived the store. A CLI answer without them is one a caller cannot compare
    // with the protocol's answer to the same call.
    let envelope = crate::route::route(envelope, name, store.stale_reason());
    match render_output(&envelope, globals.json) {
        Ok(rendered) => write(run.stdout, rendered.as_bytes(), exit_code_of(&envelope)),
        Err(error) => failure_refusal(run.stderr, &error, EXIT_FAILURE),
    }
}

/// The configuration every tool path and the `index` verb share, with a start-up failure reported as
/// the usage refusal it is: a configuration this process cannot resolve is argv-shaped input, not a
/// call that reached the graph.
fn load(run: &Invocation<'_>, argv: &[String]) -> Result<AppConfig, i32> {
    match config::load_config(argv, run.env, &run.cwd) {
        Ok(config) => Ok(config),
        Err(error) => {
            log::error(&format!("startup failed: {}", error.message), &[]);
            Err(EXIT_USAGE)
        }
    }
}

/// No graph to serve: `status` answers the bare line an operator reads before the first index run,
/// and every other tool answers a `bootstrapping` envelope and exits 1.
///
/// `status` is the deliberate exception to "every tool answers with an envelope" — "no index yet" is
/// its whole job. Every other tool's absence answer is a failure, because a tool that could not read
/// a graph has not answered the question it was asked, and an exit 0 would say it had.
fn absent(run: &mut Invocation<'_>, name: &str, config: &AppConfig, json: bool) -> i32 {
    let reason = absent_reason(&config.data_dir.join(DATABASE_FILE));
    if name == "status" {
        let line = format!("{reason}\n");
        return write(run.stdout, line.as_bytes(), 0);
    }
    let spec = EnvelopeSpec::new(
        reason,
        unchecked_read_status("nothing has been indexed yet"),
        unknown_coverage(),
    )
    .with_next_tools(instructions::next_tools_for(name));
    let envelope: Envelope<Value> = bootstrapping(
        BootstrappingSpec::new(spec, STATUS_RETRY_AFTER_MS).with_hint(
            "the watcher starts a run when the tree changes; call status to see whether one is in flight",
        ),
    );
    match render_output(&envelope, json) {
        Ok(rendered) => write(run.stdout, rendered.as_bytes(), EXIT_FAILURE),
        Err(error) => failure_refusal(run.stderr, &error, EXIT_FAILURE),
    }
}

/// The `read_status` a response carries when no handler built one: an explicit `stale: true`,
/// because an empty index supports no freshness claim, and `decisive: false`, because nothing that
/// skipped the handler can be the answer that ends an agent's search.
fn unchecked_read_status(reason: &str) -> ReadStatus {
    ReadStatus {
        generation: String::new(),
        indexed_at: String::new(),
        stale: true,
        stale_reason: Some(reason.to_owned()),
        stale_checked_at: None,
        decisive: Some(false),
    }
}

/// The coverage a response claims when no handler assembled one: nothing is indexed *by this
/// answer*, so `complete: false` and every absence claim it might otherwise support is withdrawn.
fn unknown_coverage() -> Coverage {
    Coverage {
        complete: false,
        indexed: 0,
        skipped: 0,
        failed: 0,
        unresolved_edges: 0,
        not_indexed: Vec::new(),
        tests: None,
    }
}

/// The [`ToolContext`] one call receives: the store's own reader, the cursor bound to the tree it
/// serves, the scoped logger, the clock, the resolved configuration and the size of the file the
/// reader opened.
///
/// Shared with [`crate::mcp`]: the two surfaces read the same graph through the same context, so a
/// handler cannot behave differently depending on how it was called.
pub(crate) fn context_of(
    config: &AppConfig,
    store: Rc<Store>,
    tool: &str,
) -> Result<ToolContext, AppError> {
    // Bound to the repository it serves: one process answers for several trees, and a page token
    // minted in one must not address a page in another.
    let repo = canonical_root(&config.repo_root, Some(&config.home))
        .to_string_lossy()
        .into_owned();
    let cursor = Cursor::new(config.cursor_key.as_bytes().to_vec(), Some(repo))?;
    let reader: Rc<dyn GraphReader> = Rc::new(StoreReader(Rc::clone(&store)));
    Ok(ToolContext {
        reader,
        cursor: Rc::new(cursor),
        log: Logger::new(tool),
        now: now_iso,
        config: Rc::new(config.clone()),
        db_size_bytes: store.db_size_bytes(),
    })
}

/// The store's reader as the context holds it.
///
/// [`Store`] owns the connection — one handle per process, reopened by
/// [`crate::store_lifecycle::refresh_store`] when a publish lands — while [`ToolContext`] holds an
/// owned `Rc<dyn GraphReader>` so a handler cannot close what it borrowed. This adapter is the seam
/// between the two: it *is* the lifecycle handle, so a context never opens a second connection to
/// the file the lifecycle is watching, and every read still goes through the one handle whose
/// generation `status` reports.
struct StoreReader(Rc<Store>);

impl GraphReader for StoreReader {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        self.0.reader().find_by_qn(qn)
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        self.0.reader().find_by_name(name, kinds)
    }

    fn members(&self, parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        self.0.reader().members(parent_id)
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        self.0.reader().nodes_by_file(file_id)
    }

    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        self.0.reader().inbound(node_id, kinds)
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        self.0.reader().outbound(node_id, kinds)
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        self.0.reader().metrics_for(node_id)
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        self.0.reader().rows_for_paths(prefixes)
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError> {
        self.0.reader().not_indexed_all()
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        self.0.reader().not_indexed_count()
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        self.0.reader().unresolved_for_files(file_ids)
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        self.0.reader().file_by_path(path)
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        self.0.reader().all_files()
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        self.0.reader().census_by_language()
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        self.0.reader().tests_census()
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        self.0.reader().meta_for()
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        self.0.reader().latest_generation()
    }

    fn removed_symbols_for(&self, since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        self.0.reader().removed_symbols_for(since_generation)
    }

    fn entry_points(&self, limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        self.0.reader().entry_points(limit)
    }

    fn hotspots(&self, limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        self.0.reader().hotspots(limit)
    }

    fn test_gaps(&self, ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        self.0.reader().test_gaps(ids)
    }

    fn read_node_fts(&self, match_query: &str, limit: usize) -> Result<Vec<FtsHit>, AppError> {
        self.0.reader().read_node_fts(match_query, limit)
    }
}

/// The clock every handler reads once per call, in the one spelling a staleness comparison can order
/// as text: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// The kernel formats the same string for a log line (`sangkar_core::log`), where it is private to
/// that module: a `ToolContext` needs it as a `fn() -> String`, so this is the CLI's copy of the same
/// twenty lines rather than a new dependency or a widened kernel API.
fn now_iso() -> String {
    let millis = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    };
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

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date for a day count since the UNIX
/// epoch.
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

/// Every tool's declared arguments as the parser's own table, derived once per process from the
/// registry.
///
/// `parse_cli_args` reads a `&'static` table and the registry converts seven manifests at runtime, so
/// the tables are built once and kept: a tool's declared arguments cannot change while the process
/// runs, and reading them from the registry rather than from a second copy is what keeps the CLI's
/// parser and the published schema from drifting apart.
/// One tool's parser table, under the name it answers to: the registry's name and the arguments the
/// manifest declared, in the manifest's order.
type NamedInputTable = (&'static str, Vec<(&'static str, InputField)>);

static INPUT_TABLES: LazyLock<Vec<NamedInputTable>> = LazyLock::new(|| {
    tools::TOOLS
        .iter()
        .map(|tool| (tool.name, input_table_of(tool)))
        .collect()
});

/// One tool's parser table.
pub(crate) fn input_table(tool: &tools::RegisteredTool) -> InputLiteral {
    INPUT_TABLES
        .iter()
        .find(|(name, _)| *name == tool.name)
        .map(|(_, table)| table.as_slice())
        .expect("every registered tool has an input table; both are derived from TOOLS")
}

/// One tool's descriptor as the parser's vocabulary.
fn input_table_of(tool: &tools::RegisteredTool) -> Vec<(&'static str, InputField)> {
    let Descriptor { params, .. } = (tool.describe)();
    params
        .iter()
        .map(|param| (param.name, input_field(param)))
        .collect()
}

/// One registered parameter as the parser's field: the same type, requiredness, default, bounds and
/// description, in the one shape `parse_args` validates a call against.
fn input_field(param: &RegisteredParam) -> InputField {
    let shape = match param.kind {
        Kind::Str => InputField::string(),
        Kind::Integer => InputField::integer(),
        Kind::Bool => InputField::boolean(),
        Kind::Enum(values) => InputField::enumeration(values),
        Kind::Array(items) => InputField::array(match items {
            ToolItems::Str => Items::String,
            ToolItems::Integer => Items::Integer,
            ToolItems::Enum(values) => Items::Enum(values),
        }),
    };
    InputField {
        required: param.required,
        default: param.default.map(|default| match default {
            ToolDefault::Text(text) => DefaultValue::Text(text),
            ToolDefault::Integer(number) => DefaultValue::Integer(number),
            ToolDefault::Bool(flag) => DefaultValue::Bool(flag),
            ToolDefault::List(values) => DefaultValue::List(values),
        }),
        min: param.min,
        max: param.max,
        description: Some(param.description),
        ..shape
    }
}

/// A refusal of the argv: the reason, then the block that says how the command is spelled.
fn usage_refusal(err: &mut dyn std::io::Write, error: &CliUsageError) -> i32 {
    let _ = writeln!(err, "{}\n{}", error.message, error.usage);
    EXIT_USAGE
}

/// A refusal or a failure that has a message and maybe a hint, and nothing on stdout.
///
/// A stream that cannot be written must not turn a refusal into a run, so the write is best-effort
/// and the code is the answer.
fn failure_refusal(err: &mut dyn std::io::Write, error: &AppError, code: i32) -> i32 {
    let _ = writeln!(err, "{}", error.message);
    if let Some(hint) = &error.hint {
        let _ = writeln!(err, "{hint}");
    }
    code
}

/// Write `bytes` to `out` and answer `code`; a sink that cannot take them is the code the caller
/// already decided on, never a success.
fn write(out: &mut dyn std::io::Write, bytes: &[u8], code: i32) -> i32 {
    let _ = out.write_all(bytes);
    let _ = out.flush();
    code
}
