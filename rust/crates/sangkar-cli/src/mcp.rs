//! The MCP surface: `rmcp` 3.11 over newline-delimited JSON-RPC on stdio, two handlers, and every
//! answer framed as one in-band text block.
//!
//! Ported from the `runMcp`/`callTool`/`answer` half of `src/index.ts`. The transport is `rmcp`
//! 3.11's stdio transport, not a hand-rolled frame: `rust/spikes/mcp/DECISION.md` measured it
//! against this repository's recorded session and found it reproduces the TypeScript server on all
//! four framing questions (a `\n` terminator and no `Content-Length`, no `notifications/initialized`
//! gate, in-band `isError` results, and the raw `inputSchema` object), so the ≈400-line fallback the
//! plan held in reserve was not built.
//!
//! # What the three caveats in that decision mean here
//!
//! - **Responses may arrive out of request order.** `rmcp` spawns every request's handler as its own
//!   task, so a burst comes back in whatever order the tasks finish. That is legal JSON-RPC and this
//!   module does nothing to "fix" it; `tests/mcp_surface.rs` correlates by `id` instead, which is
//!   the only thing a conforming client may rely on.
//! - **`inputSchema` key order is the `serde_json` map's.** `sangkar-cli` depends on
//!   `serde_json/preserve_order`, so the manifest's declaration order reaches the wire unchanged;
//!   without that feature every object inside a schema would come back sorted.
//! - **`rmcp` does not validate `arguments` against `inputSchema`.** It treats the schema as an
//!   opaque object it forwards, so argument refusal stays where it already lives: the ported
//!   `parse_args`, answering `CallToolResult::error` in band. A client that sends `budget: "many"`
//!   gets a `status: error` envelope with a hint, never a JSON-RPC error.
//!
//! # The two handlers
//!
//! `tools/list` is the registry's own descriptors, one `Tool` each, so the published surface has
//! exactly one home. `tools/call` is [`Surface::call`], which is one answer per call:
//! `{content: [{type: "text", text: mcp_text(envelope, output_json)}], isError}` — and no
//! `structuredContent`, because the envelope already *is* the documented shape and doubling it
//! would double the tokens of every answer.
//!
//! # Everything is in band
//!
//! An unknown tool, an argument the manifest refuses, a refused repository, an absent graph, a
//! refused cursor and a handler that panics all reach the client the same way: a text envelope with
//! `status: error` (or `bootstrapping`, which is also `isError: true`) inside a JSON-RPC *result*.
//! The `Err(McpError)` path is never taken on purpose: a client that renders protocol errors
//! opaquely would show "internal error" where the envelope says exactly which argument was wrong.
//!
//! # The panic boundary
//!
//! A handler is plain synchronous code, so a bug in one is a `panic!` and not a rejected future.
//! [`Surface::call`] catches it with [`std::panic::catch_unwind`] and answers it as the same
//! `fail(...)` an error-returning handler gets, rather than letting the unwind reach `tokio::spawn`
//! and take the session down mid-response. The default hook's backtrace is suppressed for the
//! duration and restored by a drop guard, so an unwind out of the catcher cannot leave the process
//! without a hook; the log line still names the tool and the panic message.
//!
//! # Shutdown
//!
//! The session ends when the client closes stdin, or when SIGINT/SIGTERM arrives. The TypeScript
//! waits for the calls already accepted before it stops, and so does this: a signal stops new calls
//! from starting, waits for every one in flight to be answered, and only then cancels the service.
//! Closing the transport first would drop exactly the answers a scripted client is waiting for.
//!
//! # stdout discipline
//!
//! Exactly one JSON object per response, terminated by `\n`, and nothing else — no banner, no
//! progress line, no `println!`. Every diagnostic goes through `sangkar_core::log` to stderr.
//! `tests/mcp_surface.rs` drives a full recorded session through the binary and asserts that every
//! byte on stdout is one of those frames.
//!
//! # One open per call
//!
//! The TypeScript pools one handle per repository and refreshes it when the file on disk is a newer
//! generation. This port opens the published graph per call instead. The guarantee that matters is
//! the same — a publish renames a new inode over `graph.db`, so a handle opened now is current by
//! construction — and the cost is the open the refresh path pays anyway on anything but the
//! unchanged case. That is why `store_lifecycle`'s `refresh_store` is not used here: there is no
//! handle to refresh.

use std::panic::{AssertUnwindSafe, catch_unwind, set_hook, take_hook};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, ServerInitializeError};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, ServiceExt};
use serde_json::Value;
use tokio::sync::RwLock;

use sangkar_core::config::{self, AppConfig};
use sangkar_core::envelope::{BootstrappingSpec, Envelope, bootstrapping};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::instructions::INSTRUCTIONS_TEXT;
use sangkar_core::log;
use sangkar_core::registry::{self as core_registry, Registry, SelectRepoInput, canonical_root};
use sangkar_core::repo_config::read_repo_config;
use sangkar_core::runtime::server_version;
use sangkar_store::pool::DATABASE_FILE;
use sangkar_store::read::context::{LogField, Logger};
use sangkar_store::read::reader::{GraphMeta, GraphReader};
use sangkar_tools::registry::{RegisteredTool, TOOLS};
use sangkar_tools::status::manifest::STATUS_RETRY_AFTER_MS;

use crate::args::parse_args;
use crate::dispatch::{context_of, input_table};
use crate::render::{EXIT_FAILURE, mcp_text};
use crate::route::{GraphFacts, boundary_fail, boundary_spec, is_error_status, route};
use crate::store_lifecycle::{Store, StoreFailure, absent_reason, open_store};

/// The `serverInfo.name` this process reports: the TypeScript's `SERVER_NAME`, and the name the
/// recorded session carries.
pub const SERVER_NAME: &str = "sangkar";

/// One process's whole serving state: what the surface publishes, where a call resolves from, and
/// how the answer is rendered.
///
/// Everything here is read-only per call, which is what lets [`Surface::call`] take `&self` and the
/// handler be shared across `rmcp`'s concurrent dispatches.
pub struct Surface {
    /// The tool table in `tools/list` order: [`TOOLS`] in production, a test's own table when the
    /// test needs a handler this build does not publish (the panic boundary).
    tools: &'static [RegisteredTool],
    /// The configuration the process started on: the pinned root, the machine home and the cursor
    /// key. A call that names another repository is served from `with_repo`'s answer, never by
    /// mutating this.
    startup: AppConfig,
    /// The machine registry a call's `repo` argument is resolved against.
    registry: Registry,
    /// The working directory a call that named no repository resolves from.
    cwd: PathBuf,
    /// `--repo-root`: a process-level default, never a barrier that hides a bad `repo`.
    pinned_root: Option<PathBuf>,
    /// `--json` on the command line: the answer text is JSON instead of the default TOON.
    output_json: bool,
}

impl Surface {
    /// The surface a process serves.
    #[must_use]
    pub fn new(
        startup: AppConfig,
        registry: Registry,
        cwd: PathBuf,
        pinned_root: Option<PathBuf>,
        output_json: bool,
    ) -> Self {
        Self {
            tools: &TOOLS,
            startup,
            registry,
            cwd,
            pinned_root,
            output_json,
        }
    }

    /// The same surface publishing another table. The only caller is a test that needs a handler to
    /// misbehave, so the seven this build ships stay the seven a real client sees.
    #[must_use]
    pub fn with_tools(mut self, tools: &'static [RegisteredTool]) -> Self {
        self.tools = tools;
        self
    }

    /// The tool table, in `tools/list` order.
    #[must_use]
    pub fn tools(&self) -> &'static [RegisteredTool] {
        self.tools
    }

    /// The `tools/list` result: the registry's descriptors, untouched.
    #[must_use]
    pub fn list(&self) -> ListToolsResult {
        ListToolsResult::with_all_items(self.tools.iter().map(tool_of).collect())
    }

    /// One `tools/call`, in the order the contract fixes: find the tool, parse against its manifest,
    /// resolve the repository the call is about, then answer from that repository's own reader.
    ///
    /// The order is load-bearing. [`parse_args`] runs first because the selector reads the *parsed*
    /// `repo` (so a declared string is checked before it can name anything); selection runs before a
    /// store is touched, because `not_found` and `ambiguous` are facts about the registry rather
    /// than about any graph; and `repo` is dropped from the record the handler receives, because it
    /// chose the store and is not an argument of the tool.
    #[must_use]
    pub fn call(&self, name: &str, arguments: Option<&JsonObject>) -> CallToolResult {
        mcp_answer(self.answer(name, arguments), self.output_json)
    }

    /// [`Self::call`]'s envelope: the handler's own answer with [`route`]'s post-conditions, or one
    /// of the boundary answers this composition root builds itself.
    fn answer(&self, name: &str, arguments: Option<&JsonObject>) -> Envelope<Value> {
        let Some(tool) = self.tools.iter().find(|tool| tool.name == name) else {
            let error = AppError::new(
                ErrorCode::NOT_FOUND,
                format!("no tool named \"{name}\" in this surface"),
            )
            .with_hint(format!("this server registers: {}", self.registered()));
            return boundary_fail(
                &self.startup_facts(),
                name,
                error,
                format!("{name} is not registered"),
                "the tool does not exist in this surface",
            );
        };

        // The arguments are refused before a repository can be chosen, so the facts come from the
        // tree this process started on. Same wording as a handler that threw: the call never
        // reached a graph, and nothing here may claim it did.
        let raw = match arguments {
            Some(arguments) => Value::Object(arguments.clone()),
            None => Value::Object(JsonObject::new()),
        };
        let parsed = match parse_args(tool.name, input_table(tool), &raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                log_failure(tool.name, &error);
                return boundary_fail(
                    &self.startup_facts(),
                    tool.name,
                    error,
                    format!("{} could not answer", tool.name),
                    "the call failed before the graph answered",
                );
            }
        };

        let requested = parsed.get("repo").and_then(Value::as_str);
        let mut selection = SelectRepoInput::new(&self.registry, &self.cwd);
        if let Some(requested) = requested {
            selection = selection.requested(requested);
        }
        if let Some(pinned) = self.pinned_root.as_deref() {
            selection = selection.pinned_root(pinned);
        }
        let repo_root = match core_registry::select_repo(&selection) {
            Ok(root) => root,
            // A refusal still carries the registry as its hint, so the caller can act without
            // guessing.
            Err(error) => {
                return boundary_fail(
                    &self.startup_facts(),
                    tool.name,
                    error.clone(),
                    error.message.clone(),
                    "no repository could be selected for this call",
                );
            }
        };

        let config = self.repo_config(&repo_root);
        let ready = match opened(&config) {
            Opened::Ready {
                facts,
                stale_reason,
                store,
            } => Ready {
                config,
                facts,
                stale_reason,
                store,
            },
            Opened::Absent(reason) => {
                return bootstrapping(BootstrappingSpec::new(
                    boundary_spec(&GraphFacts::none(), tool.name, reason, "nothing has been indexed yet"),
                    STATUS_RETRY_AFTER_MS,
                )
                .with_hint(
                    "the watcher starts a run when the tree changes; call status to see whether one \
                     is in flight",
                ));
            }
            Opened::Refused { error, facts } => {
                return boundary_fail(
                    &facts,
                    tool.name,
                    error.clone(),
                    error.message.clone(),
                    "the graph was refused at startup",
                );
            }
        };

        // `repo` chose the store above; the handler gets the arguments that describe the question.
        let mut args = parsed;
        args.remove("repo");
        let context = match context_of(&ready.config, Rc::clone(&ready.store), tool.name) {
            Ok(context) => context,
            Err(error) => {
                return boundary_fail(
                    &ready.facts,
                    tool.name,
                    error.clone(),
                    error.message.clone(),
                    "the call could not prepare to read the graph",
                );
            }
        };

        let answered = catch_panic(|| (tool.handle)(&context, &args));
        let envelope = match answered {
            Ok(envelope) => envelope,
            // A handler that returns its errors reaches this point only by panicking; the panic is
            // this boundary's `internal`, and the handler's own code cannot leak through it.
            Err(panic) => {
                let error = panic_error(&panic);
                log_failure(tool.name, &error);
                return boundary_fail(
                    &ready.facts,
                    tool.name,
                    error,
                    format!("{} could not answer", tool.name),
                    "the call failed before the graph answered",
                );
            }
        };
        route(envelope, tool.name, ready.stale_reason.as_deref())
    }

    /// The names a client may call, in `tools/list` order: what the unknown-tool hint lists.
    fn registered(&self) -> String {
        self.tools
            .iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// One repository's settings. Any other tree is served from its own `.sangkar`, but the tree
    /// this process started on keeps the config it started with: an explicit `--data-dir` was said
    /// about *that* tree, so rebuilding it from the root would send the server looking for a graph
    /// the caller explicitly placed somewhere else.
    fn repo_config(&self, repo_root: &Path) -> AppConfig {
        let base = if canonical_root(&self.startup.repo_root, Some(&self.startup.home))
            == canonical_root(repo_root, Some(&self.startup.home))
        {
            self.startup.clone()
        } else {
            config::with_repo(&self.startup, repo_root, None)
        };
        // The one place the source is settled: this is the first point that knows which tree the
        // call answers from, because `--repo-root` is only a default and an explicit `repo`
        // outranks it.
        let repo = read_repo_config(&base.repo_root, Some(&base.data_dir), base.repo_defaults);
        AppConfig {
            repo_config_source: repo.source,
            ..base
        }
    }

    /// The graph facts a boundary answer may still claim, read from the tree this process started
    /// on: a call that selected no repository has no other candidate, and a refusal still owes the
    /// client a `read_status`. A graph that cannot be opened contributes nothing, which is the
    /// answer itself.
    fn startup_facts(&self) -> GraphFacts {
        let config = self.repo_config(&self.startup.repo_root);
        match open_store(&config.data_dir) {
            Ok(store) => facts_of(store.reader()),
            Err(_) => GraphFacts::none(),
        }
    }
}

/// A graph that opened and passed the identity guard: what the handler and the route need.
struct Ready {
    config: AppConfig,
    facts: GraphFacts,
    /// `root_mismatch` when the graph's recorded tree has moved, `None` when it is this one. The
    /// refusal half of the same comparison never reaches a handler, so it never reaches here.
    stale_reason: Option<String>,
    store: Rc<Store>,
}

/// One `tools/list` entry: the registry's descriptor, copied field for field into the three slots
/// `rmcp` publishes. The descriptor is built by `sangkar_tools::registry`, so this function is a
/// projection rather than a second derivation — and a total one: a descriptor missing a field
/// answers that field empty instead of panicking inside `tools/list`.
fn tool_of(tool: &RegisteredTool) -> Tool {
    let descriptor = tool.descriptor();
    let name = match descriptor.get("name") {
        Some(Value::String(name)) => name.clone(),
        _ => String::new(),
    };
    let description = match descriptor.get("description") {
        Some(Value::String(text)) => text.clone(),
        _ => String::new(),
    };
    let schema = match descriptor.get("inputSchema") {
        Some(Value::Object(schema)) => schema.clone(),
        _ => JsonObject::new(),
    };
    Tool::new(name, description, Arc::new(schema))
}

/// The MCP result: the envelope as one text part, TOON by default and JSON when the process was
/// started with `--json`. It is not also sent as `structuredContent`, and `isError` is true exactly
/// when the envelope carries an `error`, so a client that reads only the flag and one that reads
/// only the body agree.
fn mcp_answer(envelope: Envelope<Value>, output_json: bool) -> CallToolResult {
    let is_error = is_error_status(envelope.status);
    match mcp_text(&envelope, output_json) {
        Ok(text) => {
            let content = vec![ContentBlock::text(text)];
            if is_error {
                CallToolResult::error(content)
            } else {
                CallToolResult::success(content)
            }
        }
        // A rendering failure is the one thing an envelope cannot describe — there is no text to
        // put in it. It is still in band: the client gets the reason as the tool's message, and the
        // session keeps answering.
        Err(error) => {
            log::error(&format!("mcp.render_failed: {}", error.message), &[]);
            CallToolResult::error(vec![ContentBlock::text(error.message)])
        }
    }
}

/// What [`opened`] found: a graph to serve with the facts it contributes, or the reason there is
/// none.
enum Opened {
    Ready {
        facts: GraphFacts,
        stale_reason: Option<String>,
        store: Rc<Store>,
    },
    /// No file, or the zero-byte stage a torn publish left behind: `bootstrapping`, not an error.
    Absent(String),
    /// A graph this build must not serve.
    Refused { error: AppError, facts: GraphFacts },
}

/// Opens the published graph under `config.data_dir`, refusing one that belongs to another tree.
///
/// The fingerprint and the schema check are [`crate::store_lifecycle::open_store`]'s; what this
/// function adds is `DATABASE.md` §5's identity guard, ported from `src/index.ts`'s `guardRoot`.
/// A graph whose recorded `project_root` is a tree that no longer exists is served **with** a
/// `root_mismatch` disclosure (the tree moved or was copied, and the generation is what the client
/// asked for), while a graph whose recorded root is still there and is not this one is refused:
/// serving a graph that belongs to another tree is the worst failure this design can produce.
fn opened(config: &AppConfig) -> Opened {
    let db_path = config.data_dir.join(DATABASE_FILE);
    let store = match open_store(&config.data_dir) {
        Ok(store) => store,
        // No file, or one this process may not stat: neither is a generation to serve.
        Err(StoreFailure::Missing) => return Opened::Absent(absent_reason(&db_path)),
        Err(failure) => {
            let error = failure.error().unwrap_or_else(|| {
                AppError::internal(format!("the graph could not be opened: {failure:?}"))
            });
            return Opened::Refused {
                error,
                facts: GraphFacts::none(),
            };
        }
    };

    let meta = match store.reader().meta_for() {
        Ok(meta) => meta,
        // The reader is the thing that failed: no facts, and the reason to refuse.
        Err(error) => {
            return Opened::Refused {
                error,
                facts: GraphFacts::none(),
            };
        }
    };
    if meta.project_root.is_none() {
        // `DATABASE.md` §5 compares `meta.project_root`; an index that recorded none gives the guard
        // nothing to compare, so the graph is served and the gap is logged rather than assumed away.
        log::warn(
            &format!("store.identity_unrecorded: {}", db_path.display()),
            &[],
        );
    }
    let facts = facts_of(store.reader());
    match guard_root(meta.project_root.as_deref(), &config.repo_root, &db_path) {
        Ok(stale_reason) => Opened::Ready {
            facts,
            stale_reason,
            store: Rc::new(store),
        },
        Err(error) => Opened::Refused { error, facts },
    }
}

/// DATABASE.md §5's three-way comparison, reduced to what this process can observe: the recorded
/// root equals this one (serve, no disclosure), the recorded root no longer exists (the tree moved
/// or was copied — serve, and disclose it as `stale_reason: "root_mismatch"`), or a different tree
/// is still there (refuse).
fn guard_root(
    recorded: Option<&str>,
    repo_root: &Path,
    db_path: &Path,
) -> Result<Option<String>, AppError> {
    let Some(recorded) = recorded else {
        return Ok(None);
    };
    let recorded = Path::new(recorded);
    if canonical_root(recorded, None) == canonical_root(repo_root, None) {
        return Ok(None);
    }
    if !recorded.exists() {
        return Ok(Some("root_mismatch".to_owned()));
    }
    Err(AppError::new(
        ErrorCode::WRONG_PROJECT_DB,
        format!(
            "the graph at {} belongs to {}, not {}",
            db_path.display(),
            recorded.display(),
            repo_root.display()
        ),
    )
    .with_hint("point --data-dir at this tree's own .sangkar directory"))
}

/// The two facts a graph contributes to an answer it did not produce: the generation it serves and
/// the `published_at` of the served publish.
///
/// `sangkar_store`'s `GraphMeta` carries no `indexed_at`, so the newest generation row is what every
/// handler already publishes as one; a reader that cannot answer contributes the empty string rather
/// than a refusal, because the answer this feeds is already the refusal.
fn facts_of(reader: &dyn GraphReader) -> GraphFacts {
    let generation = reader
        .meta_for()
        .ok()
        .and_then(|meta: GraphMeta| meta.generation)
        .unwrap_or_default();
    let indexed_at = reader
        .latest_generation()
        .ok()
        .flatten()
        .map_or_else(String::new, |row| row.published_at);
    GraphFacts {
        generation,
        indexed_at,
    }
}

/// The error a panic becomes: `internal`, with the panic payload's message when it is a string and a
/// fixed sentence when it is not.
fn panic_error(panic: &Box<dyn std::any::Any + Send>) -> AppError {
    let message = panic
        .downcast_ref::<&str>()
        .map(|text| (*text).to_owned())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "the handler panicked".to_owned());
    AppError::internal(message)
}

/// Run `handler` and turn a panic into `Err`, with the default hook's message suppressed for the
/// duration.
///
/// The suppression is serialised by a process-wide lock because a hook is process-global and `rmcp`
/// dispatches calls concurrently: two panics interleaving their installations would leave one hook
/// where the other was. A drop guard restores the previous hook even if something unwinds through
/// this frame, so the process never keeps a silent hook past the call that needed it.
///
/// The cost of a process-global hook, stated rather than hidden: a panic on some other thread while
/// a handler is inside this catcher writes no default message either. Nothing in this process panics
/// outside a handler on purpose, the answer is still lost rather than the session, and the log line
/// this boundary writes names the tool.
fn catch_panic<T>(handler: impl FnOnce() -> T) -> Result<T, Box<dyn std::any::Any + Send>> {
    static HOOK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guarding = HOOK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let restore = RestoreHook(Some(take_hook()));
    set_hook(Box::new(|_| {}));
    let answered = catch_unwind(AssertUnwindSafe(handler));
    drop(restore);
    drop(guarding);
    answered
}

/// The process's previous panic hook, as [`take_hook`] hands it back.
type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Sync + Send + 'static>;

/// Puts the previous panic hook back when it goes out of scope, including on an unwind.
struct RestoreHook(Option<PanicHook>);

impl Drop for RestoreHook {
    fn drop(&mut self) {
        if let Some(hook) = self.0.take() {
            set_hook(hook);
        }
    }
}

/// The `tool.failed` line every refused call writes: the code and the reason, scoped to the tool,
/// exactly as the TypeScript's `scopedLog(tool).error("tool.failed", …)` does.
fn log_failure(tool: &str, error: &AppError) {
    Logger::new(tool).error(
        "tool.failed",
        &[
            ("code", LogField::Str(error.code.to_string())),
            ("reason", LogField::Str(error.message.clone())),
        ],
    );
}

/// The surface bound to one process invocation: the configuration `argv` resolves, the machine
/// registry, and the `--json` answer form. Kept beside [`Surface::new`] so the binary's verb arm has
/// one place to build the serving state and a test has the same one.
///
/// # Errors
///
/// [`config::load_config`]'s refusal: a cursor key shorter than the cursor module's floor, or a
/// process whose own executable cannot be resolved.
pub fn surface_of(
    argv: &[String],
    env: config::Env<'_>,
    cwd: &Path,
    output_json: bool,
) -> Result<Surface, AppError> {
    let startup = config::load_config(argv, env, cwd)?;
    let registry = core_registry::read_registry(&startup.registry_path);
    let pinned_root = config::flag_value(argv, "repo-root").map(PathBuf::from);
    Ok(Surface::new(
        startup,
        registry,
        cwd.to_path_buf(),
        pinned_root,
        output_json,
    ))
}

/// The server `rmcp` dispatches to: [`Surface`] plus the drain gate a shutdown waits on.
struct Server {
    surface: Surface,
    /// Held for the whole of one `tools/call`. A signal takes the *write* side, so it waits for the
    /// calls already accepted and stops a new one from starting; no call is ever refused by it in
    /// normal operation, because nothing takes the write side but a shutdown.
    inflight: Arc<RwLock<()>>,
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        // `capabilities.tools` is what makes `tools/list` and `tools/call` answerable, and the
        // instructions text is how the usage guide reaches the agent: MCP publishes tools and not
        // prompt text, so this field is the only channel for it.
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(SERVER_NAME, server_version()))
            // The guide, without the source file's trailing newline: the TypeScript publishes its
            // `INSTRUCTIONS_TEXT` constant, which is the file `.trimEnd()`ed (`tests/instructions
            // .test.ts` asserts exactly that), so the untrimmed include would be one byte longer
            // than the recorded `initialize` result.
            .with_instructions(INSTRUCTIONS_TEXT.trim_end())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // No pagination: seven entries fit in one response by construction, and a cursor the client
        // sends is ignored rather than refused — the surface answers the whole list, which is what
        // the TypeScript does.
        Ok(self.surface.list())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        // Every outcome is a result: the envelope, in band, with `isError` when it carries an error.
        // `Err(McpError)` would be a protocol error the client renders opaquely.
        let _in_flight = Arc::clone(&self.inflight).read_owned().await;
        Ok(self
            .surface
            .call(&request.name, request.arguments.as_ref())
            .into())
    }
}

/// Serve one stdio session and answer the process's exit code.
///
/// The session ends on stdin EOF (the client hanging up, which is how a stdio client ends a session)
/// or on SIGINT/SIGTERM, and both are a success: a client that closed the pipe asked for the server
/// to stop, and a supervisor's signal is how a service is stopped. Requests already accepted are
/// answered before the service is cancelled.
pub fn serve(surface: Surface) -> i32 {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            log::error(&format!("mcp.runtime_failed: {error}"), &[]);
            return EXIT_FAILURE;
        }
    };
    let code = runtime.block_on(serve_session(surface));
    // Not a plain drop: a session that ends on a signal still has `tokio::io::stdin()`'s blocking
    // read parked on a pipe the client has not closed, and dropping a runtime waits for every
    // blocking task to finish — the one read that cannot finish until the process exits. The
    // background shutdown returns immediately and the process this returns to exits; the parked
    // thread goes with it, having nothing left to read for.
    runtime.shutdown_background();
    code
}

/// [`serve`]'s body, so the runtime is built once and a whole session is one future.
async fn serve_session(surface: Surface) -> i32 {
    let tool_count = surface.tools().len();
    let server = Server {
        surface,
        inflight: Arc::new(RwLock::new(())),
    };
    let inflight = Arc::clone(&server.inflight);
    // Installed before the handshake, so a signal that arrives while the client is still
    // introducing itself is observed instead of killing the process mid-frame.
    let mut signals = Signals::install();

    let running = match server.serve(stdio()).await {
        Ok(running) => running,
        // A client that closed stdin before saying hello asked for nothing: that is the same
        // client-gone signal as a later EOF, not a failure. Any other initialize failure is the
        // protocol refusing the process, which is.
        Err(ServerInitializeError::ConnectionClosed(_)) => {
            log::info("mcp.shutdown: reason=client closed stdin", &[]);
            return 0;
        }
        Err(error) => {
            log::error(&format!("mcp.initialize_failed: {error}"), &[]);
            return EXIT_FAILURE;
        }
    };
    log::info(&format!("mcp.ready: tools={tool_count}"), &[]);

    let cancellation = running.cancellation_token();
    let mut quit = Box::pin(running.waiting());
    tokio::select! {
        // The client closed stdin, or the transport failed: either way the session is over.
        _ = &mut quit => {
            log::info("mcp.shutdown: reason=client closed stdin", &[]);
            return 0;
        }
        signal = signals.recv() => {
            log::info(&format!("mcp.signal: signal={signal}"), &[]);
        }
    }

    // A signal: no new call starts, the ones already accepted answer, then the transport closes.
    // Only then is the service cancelled, because a response written after the loop stops has no
    // one to carry it to the client.
    {
        let _drained = inflight.write().await;
    }
    cancellation.cancel();
    let _ = quit.await;
    log::info("mcp.shutdown: reason=signal", &[]);
    0
}

/// The two termination signals, installed once per session.
struct Signals {
    #[cfg(unix)]
    interrupt: Option<tokio::signal::unix::Signal>,
    #[cfg(unix)]
    terminate: Option<tokio::signal::unix::Signal>,
}

impl Signals {
    fn install() -> Self {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let install = |kind: SignalKind, name: &str| match signal(kind) {
                Ok(stream) => Some(stream),
                // A signal stream that cannot be installed leaves the process with the signal's
                // default disposition. The session is not refused for it — a client can still be
                // served — but the reason is on stderr rather than silently assumed away.
                Err(error) => {
                    log::warn(
                        &format!("mcp.signal_failed: signal={name} error={error}"),
                        &[],
                    );
                    None
                }
            };
            Self {
                interrupt: install(SignalKind::interrupt(), "SIGINT"),
                terminate: install(SignalKind::terminate(), "SIGTERM"),
            }
        }
        #[cfg(not(unix))]
        {
            Self {}
        }
    }

    /// The name of the signal that arrived. A stream that could not be installed parks instead of
    /// firing, so the other one — or stdin EOF — is what ends the session.
    async fn recv(&mut self) -> &'static str {
        #[cfg(unix)]
        {
            tokio::select! {
                name = next_signal(&mut self.interrupt, "SIGINT") => name,
                name = next_signal(&mut self.terminate, "SIGTERM") => name,
            }
        }
        #[cfg(not(unix))]
        {
            // The only signal a non-unix build can observe is ctrl-c.
            match tokio::signal::ctrl_c().await {
                Ok(()) => "SIGINT",
                Err(error) => {
                    log::warn(&format!("mcp.signal_failed: error={error}"), &[]);
                    std::future::pending::<()>().await;
                    "SIGINT"
                }
            }
        }
    }
}

/// The name of the signal `stream` carries, or a future that never resolves when the stream could not
/// be installed.
#[cfg(unix)]
async fn next_signal(
    stream: &mut Option<tokio::signal::unix::Signal>,
    name: &'static str,
) -> &'static str {
    match stream {
        Some(stream) => {
            stream.recv().await;
            name
        }
        None => std::future::pending().await,
    }
}
