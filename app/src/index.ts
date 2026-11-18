/**
 * The composition root (`TECH-SPEC.md` §7): the one module that knows both halves of the app. It
 * does four things and nothing else — resolve the config and run the open-time guard, register the
 * tools the profile allows, start the watcher, and wire the exit path — so every other module can
 * be tested without a server and without a database.
 *
 * Two rules shape the file:
 *
 * - **stdout is the MCP protocol.** No `console.log`, no `process.stdout.write` outside an explicit
 *   CLI mode; every diagnostic goes through `#shared/log`, which owns stderr. A stray line here
 *   corrupts the session rather than merely looking untidy.
 * - **the manifest is the only schema.** Each tool's `input` literal is turned into the JSON Schema
 *   `tools/list` publishes and the arguments `tools/call` accepts, so the advertised surface and the
 *   parsed one cannot drift.
 *
 * Why the low-level `Server` and not `McpServer`: `McpServer.registerTool` takes a Zod raw shape as
 * its `inputSchema`, and zod is not a declared dependency of this app (`app/package.json`) — it is
 * only reachable transitively through the SDK. The plan's schema is a literal in each manifest, so
 * `Server` plus `setRequestHandler` keeps the SDK and keeps that single source of truth, instead of
 * adding a dependency and a second schema dialect to express the same seven tools.
 *
 * The seven manifests are imported explicitly (`TECH-SPEC.md` §7: the registry is seven lines, no
 * directory scanning, and the dependency graph stays readable). The watcher and the worker are the
 * only `./indexer/*` imports, and they are root-only by design: a tool may never spawn an index run
 * of its own, and the watcher is never allowed to import the writer it spawns.
 */
import { existsSync, realpathSync, statSync } from "node:fs";
import { join } from "node:path";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  type CallToolResult,
  ListToolsRequestSchema,
  type Tool,
} from "@modelcontextprotocol/sdk/types.js";
import { type AppConfig, loadConfig, type Profile } from "#shared/config";
import { createCursors } from "#shared/cursor";
import {
  bootstrapping,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  type ReadStatus,
} from "#shared/envelope";
import { AppError, isAppError } from "#shared/errors";
import { INSTRUCTIONS_TEXT, nextToolsFor } from "#shared/instructions";
import { type Logger, log } from "#shared/log";
import { type Conn, openReader } from "#shared/store/db";
import { type LockHandle, release } from "#shared/store/lock";
import { readSchemaVersion, schemaVersion } from "#shared/store/migrate";
import type { ToolContext } from "#shared/store/read/context";
import { type GraphReader, makeReader } from "#shared/store/read/reader";
import { startWatch, type WatchHandle } from "./indexer/watch";
import { runWorker, type WorkerMode } from "./indexer/worker";
import { handle as expandHandle } from "./tools/expand/handler";
import { input as expandInput, manifest as expandManifest } from "./tools/expand/manifest";
import { handle as impactHandle } from "./tools/impact/handler";
import { input as impactInput, manifest as impactManifest } from "./tools/impact/manifest";
import { handle as indexHandle } from "./tools/index/handler";
import { input as indexInput, manifest as indexManifest } from "./tools/index/manifest";
import { handle as searchHandle } from "./tools/search/handler";
import { input as searchInput, manifest as searchManifest } from "./tools/search/manifest";
import { handle as statusHandle } from "./tools/status/handler";
import {
  STATUS_RETRY_AFTER_MS,
  input as statusInput,
  manifest as statusManifest,
} from "./tools/status/manifest";
import { handle as summaryHandle } from "./tools/summary/handler";
import { input as summaryInput, manifest as summaryManifest } from "./tools/summary/manifest";
import { handle as traceHandle } from "./tools/trace/handler";
import { input as traceInput, manifest as traceManifest } from "./tools/trace/manifest";

/** The graph file inside the resolved data dir (`DATABASE.md` §3.0). */
const DB_FILE = "graph.db";

/**
 * The identity an MCP client reports in `serverInfo`. The version is a literal because the root may
 * not import `app/package.json` — `tests/boundary.test.ts` confines its imports to `#shared/*`, the
 * indexer and the tool modules — and a second copy of the version is cheaper than a boundary hole.
 */
const SERVER_NAME = "codegraph";
const SERVER_VERSION = "0.1.0";

/**
 * The coverage a response claims when no handler assembled one: nothing is indexed *by this answer*,
 * so `complete: false` and every absence claim it might otherwise support is withdrawn.
 */
const UNKNOWN_COVERAGE: Coverage = {
  complete: false,
  indexed: 0,
  skipped: 0,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

/**
 * One declared argument, exactly the literal a manifest exports (`TECH-SPEC.md` §6). Deliberately
 * loose — `type` is a plain string — because one manifest writes its table without `as const`, so a
 * stricter union would need a cast here to say what the switch below already checks at runtime.
 */
interface InputField {
  readonly type: string;
  readonly required?: boolean;
  readonly default?: unknown;
  readonly min?: number;
  readonly max?: number;
  readonly values?: readonly string[];
  readonly items?: { readonly type?: string; readonly values?: readonly string[] };
  readonly description?: string;
}

type InputLiteral = Readonly<Record<string, InputField>>;

/** The argument map a validated call hands a handler: declared keys only, defaults applied. */
type ToolArgs = Readonly<Record<string, unknown>>;

/** The one call signature the registry stores; each tool's own args type is narrowed inside it. */
type ToolHandler = (
  ctx: ToolContext,
  args: ToolArgs,
) => Envelope<unknown> | Promise<Envelope<unknown>>;

interface RegisteredTool {
  readonly name: string;
  readonly description: string;
  readonly profile: Profile;
  readonly input: InputLiteral;
  readonly handle: ToolHandler;
}

/**
 * Binds a handler whose args type is the manifest's mirror. The cast is the boundary, not a promise
 * the type system keeps: {@link parseArgs} has already refused every value the manifest does not
 * declare, so the only thing being asserted is that the checked record is the handler's own shape.
 */
function bind<A>(
  handler: (ctx: ToolContext, args: A) => Envelope<unknown> | Promise<Envelope<unknown>>,
): ToolHandler {
  return (ctx, args) => handler(ctx, args as A);
}

/**
 * The registry, in the order `tools/list` reports: the orientation tools first, then the lookups,
 * then the admin repair. `profile` comes from the manifest, so a tool that should not be reachable
 * from an agent session says so once, next to its description.
 */
const TOOLS: readonly RegisteredTool[] = [
  { ...statusManifest, input: statusInput, handle: bind(statusHandle) },
  { ...summaryManifest, input: summaryInput, handle: bind(summaryHandle) },
  { ...traceManifest, input: traceInput, handle: bind(traceHandle) },
  { ...searchManifest, input: searchInput, handle: bind(searchHandle) },
  { ...impactManifest, input: impactInput, handle: bind(impactHandle) },
  { ...expandManifest, input: expandInput, handle: bind(expandHandle) },
  { ...indexManifest, input: indexInput, handle: bind(indexHandle) },
];

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * The tool surface one profile may see (DESIGN.md §4). `agent` gets the six query tools; `admin`
 * adds `index`. The same filter decides what `tools/list` publishes and what `tools/call` accepts,
 * so a profile cannot be widened by calling a name it was not offered.
 */
function registerableTools(profile: Profile): RegisteredTool[] {
  return TOOLS.filter((tool) => tool.profile === "agent" || profile === "admin");
}

function itemSchemaOf(field: InputField): Record<string, unknown> {
  const values = field.items?.values;
  return values === undefined ? { type: "string" } : { type: "string", enum: [...values] };
}

/**
 * One declared argument as JSON Schema. `enum` becomes a string schema with `enum` values (JSON
 * Schema has no enum type), `min`/`max` become `minimum`/`maximum`, and a declared default travels
 * with the property so a client can see what an omitted argument means before sending it.
 */
function propertySchemaOf(field: InputField): Record<string, unknown> {
  const schema: Record<string, unknown> =
    field.type === "enum"
      ? { type: "string", enum: [...(field.values ?? [])] }
      : field.type === "array"
        ? { type: "array", items: itemSchemaOf(field) }
        : { type: field.type };
  if (field.min !== undefined) {
    schema.minimum = field.min;
  }
  if (field.max !== undefined) {
    schema.maximum = field.max;
  }
  if (field.description !== undefined) {
    schema.description = field.description;
  }
  if (field.default !== undefined) {
    schema.default = field.default;
  }
  return schema;
}

/** The manifest's `input` table as the `inputSchema` `tools/list` publishes. */
function inputSchemaOf(input: InputLiteral): Tool["inputSchema"] {
  const properties: Record<string, object> = {};
  const required: string[] = [];
  for (const [name, field] of Object.entries(input)) {
    properties[name] = propertySchemaOf(field);
    if (field.required === true) {
      required.push(name);
    }
  }
  const schema: Tool["inputSchema"] = { type: "object", properties };
  if (required.length > 0) {
    schema.required = required;
  }
  return schema;
}

/** A `tools/list` entry: name, the model-facing description, and the schema from the manifest. */
function descriptorOf(tool: RegisteredTool): Tool {
  return {
    name: tool.name,
    description: tool.description,
    inputSchema: inputSchemaOf(tool.input),
  };
}

function matchesItem(field: InputField, value: unknown): boolean {
  if (typeof value !== "string") {
    return false;
  }
  const values = field.items?.values;
  return values === undefined || values.includes(value);
}

function matchesType(field: InputField, value: unknown): boolean {
  switch (field.type) {
    case "string":
      return typeof value === "string";
    case "integer":
      return typeof value === "number" && Number.isInteger(value);
    case "boolean":
      return typeof value === "boolean";
    case "enum":
      return typeof value === "string" && (field.values ?? []).includes(value);
    case "array":
      return Array.isArray(value) && value.every((item) => matchesItem(field, item));
    default:
      // A manifest field with an unknown `type` is a bug in the tool, never in the call.
      return false;
  }
}

/** What the argument should have been, in the one line the agent's retry needs. */
function expectationOf(name: string, field: InputField): string {
  switch (field.type) {
    case "integer": {
      const floor = field.min ?? Number.MIN_SAFE_INTEGER;
      const ceiling = field.max ?? Number.MAX_SAFE_INTEGER;
      return `"${name}" must be an integer between ${floor} and ${ceiling}`;
    }
    case "enum":
      return `"${name}" must be one of: ${(field.values ?? []).join(", ")}`;
    case "array": {
      const values = field.items?.values;
      return values === undefined
        ? `"${name}" must be an array of strings`
        : `"${name}" must be an array of: ${values.join(", ")}`;
    }
    case "boolean":
      return `"${name}" must be true or false`;
    default:
      return `"${name}" must be a string`;
  }
}

/**
 * Arguments against the manifest literal: unknown keys are dropped, declared defaults fill an
 * absent argument, a missing required argument and a wrong type are refused. Refusals carry the
 * closed `internal` code because they are our own schema disagreeing with the client, and the hint
 * names the argument so the retry is a correction rather than a guess.
 */
function parseArgs(tool: string, input: InputLiteral, raw: unknown): ToolArgs {
  const source: Record<string, unknown> =
    typeof raw === "object" && raw !== null && !Array.isArray(raw)
      ? (raw as Record<string, unknown>)
      : {};
  const args: Record<string, unknown> = {};
  for (const [name, field] of Object.entries(input)) {
    const value = source[name];
    if (value === undefined) {
      if (field.default !== undefined) {
        args[name] = field.default;
      } else if (field.required === true) {
        throw new AppError("internal", `${tool} is missing the required argument "${name}"`, {
          hint: expectationOf(name, field),
        });
      }
      continue;
    }
    if (!matchesType(field, value)) {
      throw new AppError("internal", `${tool} received an invalid "${name}" argument`, {
        hint: expectationOf(name, field),
      });
    }
    args[name] = value;
  }
  return args;
}

/** One claim the open-time guard can make about the database it found (`DATABASE.md` §5). */
type Store =
  | {
      readonly kind: "ready";
      readonly conn: Conn;
      readonly context: ToolContext;
      /** The one non-fatal disagreement: the same tree, recorded under its old path. */
      readonly staleReason: string | null;
    }
  | { readonly kind: "absent"; readonly reason: string }
  | { readonly kind: "refused"; readonly error: AppError };

/** `realpath` where the path exists, `resolve` otherwise: two spellings of one tree must compare equal. */
function canonical(path: string): string {
  try {
    return realpathSync(path);
  } catch {
    return path;
  }
}

/** What the guard decided: a disclosure to attach to every answer, and possibly a refusal. */
interface RootGuard {
  /** `root_mismatch` when the recorded tree moved, or `null` when the recorded root is this one. */
  readonly staleReason: string | null;
  /** The refusal, or `null` when the graph may be served. */
  readonly error: AppError | null;
}

/**
 * DATABASE.md §5's three-way comparison, reduced to what this process can observe: the recorded
 * root equals this one (serve), the recorded root no longer exists (the tree moved or was copied —
 * serve, and disclose it as `stale_reason: "root_mismatch"`), or a different tree is still there
 * (refuse). Serving a graph that belongs to another tree is the worst failure this design can
 * produce, so the last case is a refusal rather than a warning.
 */
function guardRoot(recorded: string | null, repoRoot: string, dbPath: string): RootGuard {
  if (recorded === null || canonical(recorded) === canonical(repoRoot)) {
    return { staleReason: null, error: null };
  }
  if (!existsSync(recorded)) {
    return { staleReason: "root_mismatch", error: null };
  }
  return {
    staleReason: null,
    error: new AppError(
      "wrong_project_db",
      `the graph at ${dbPath} belongs to ${recorded}, not ${repoRoot}`,
      {
        hint: "point CODEGRAPH_DATA_DIR (or --data-dir) at this tree's own .codegraph directory",
      },
    ),
  };
}

/**
 * Anything that kept the read handle from being usable, as a refusal. An `AppError` keeps its code —
 * `unsupported_filesystem` from the PRAGMA read-back is the case that matters — and everything else
 * (a truncated file, a missing table) is `internal` with the rebuild hint, because a graph this
 * process cannot read is a graph the indexer has to repair.
 */
function refusalOf(error: unknown): AppError {
  if (isAppError(error)) {
    return error;
  }
  return new AppError("internal", `the graph could not be opened: ${messageOf(error)}`, {
    hint: "run `codegraph index` to rebuild it",
  });
}

function buildContext(config: AppConfig, reader: GraphReader): ToolContext {
  return {
    reader,
    cursor: createCursors({ key: config.cursorKey }),
    log,
    now: () => new Date(),
    config,
  };
}

/**
 * Open-time guard: open the reader read-only, refuse a graph this build must not serve, and treat a
 * missing (or empty) file as "nothing indexed yet" rather than as an error — a first `mcp` session
 * before the first publish is normal, and every tool already knows how to answer `bootstrapping`.
 *
 * The handle is opened once, here, and never re-opened: DATABASE.md §5 says a reader keeps serving
 * the generation it opened, so freshness across a publish belongs to the freshness work, not to a
 * reconnect per call.
 */
function openStore(config: AppConfig): Store {
  const dbPath = join(config.dataDir, DB_FILE);
  if (!existsSync(dbPath)) {
    return { kind: "absent", reason: `no index at ${dbPath} yet` };
  }
  let conn: Conn | null = null;
  try {
    if (statSync(dbPath).size === 0) {
      return { kind: "absent", reason: `the index at ${dbPath} is empty` };
    }
    conn = openReader(dbPath);
    const found = readSchemaVersion(conn.sqlite);
    const shipped = schemaVersion();
    if (found > shipped) {
      const error = new AppError(
        "schema_too_new",
        `the graph at ${dbPath} is schema v${found}; this build ships v${shipped}`,
        {
          hint: "upgrade the server, or move the data dir aside and re-index with `codegraph index`",
        },
      );
      conn.close();
      return { kind: "refused", error };
    }
    const reader = makeReader(conn);
    const meta = reader.metaFor();
    if (meta.projectRoot === null) {
      // DATABASE.md §5 compares `meta.project_root`; an index that recorded none gives the guard
      // nothing to compare, so the graph is served and the gap is logged rather than assumed away.
      log.warn("store.identity_unrecorded", { dbPath, generation: meta.generation });
    }
    const guard = guardRoot(meta.projectRoot, config.repoRoot, dbPath);
    if (guard.error !== null) {
      conn.close();
      return { kind: "refused", error: guard.error };
    }
    return {
      kind: "ready",
      conn,
      context: buildContext(config, reader),
      staleReason: guard.staleReason,
    };
  } catch (error) {
    conn?.close();
    return { kind: "refused", error: refusalOf(error) };
  }
}

/**
 * `#shared/log` under the tool's name, built per call: every line a handler writes is attributable
 * without each call site repeating the tool name, and the context itself stays one object.
 */
function scopedLog(tool: string): Logger {
  return {
    debug: (msg, fields) => log.debug(msg, { tool, ...fields }),
    info: (msg, fields) => log.info(msg, { tool, ...fields }),
    warn: (msg, fields) => log.warn(msg, { tool, ...fields }),
    error: (msg, fields) => log.error(msg, { tool, ...fields }),
  };
}

/**
 * The `read_status` a response carries when no handler built one: whatever generation the reader
 * knows (if it can be read at all) and an explicit `stale: true`, because a failure or an empty
 * index supports no freshness claim — `relationFor` then downgrades every total to `gte`.
 */
function uncheckedReadStatus(store: Store, reason: string): ReadStatus {
  let generation = "";
  let indexedAt = "";
  if (store.kind === "ready") {
    try {
      const meta = store.context.reader.metaFor();
      generation = meta.generation ?? "";
      indexedAt = meta.indexedAt ?? "";
    } catch {
      // The reader is the thing that failed; the empty generation is the honest answer.
    }
  }
  return { generation, indexed_at: indexedAt, stale: true, stale_reason: reason };
}

/** The spec every non-handler answer is built from, including the `next_tools` routing promise. */
function boundarySpec(store: Store, tool: string, summary: string, reason: string): EnvelopeSpec {
  return {
    summary,
    read_status: uncheckedReadStatus(store, reason),
    coverage: UNKNOWN_COVERAGE,
    next_tools: nextToolsFor(tool),
  };
}

/**
 * The tool's answer with the root's two post-conditions applied: `next_tools` from the routing
 * table (replacing whatever the handler set, never dropping the field), and the guard's
 * `root_mismatch` as a staleness disclosure rather than a silent difference.
 */
function route(
  envelope: Envelope<unknown>,
  tool: string,
  staleReason: string | null,
): Envelope<unknown> {
  const routed: Envelope<unknown> = { ...envelope, next_tools: nextToolsFor(tool) };
  if (staleReason === null) {
    return routed;
  }
  return {
    ...routed,
    read_status: { ...routed.read_status, stale: true, stale_reason: staleReason },
  };
}

/**
 * The MCP result: the envelope as one JSON text part. It is not also sent as `structuredContent`
 * because the envelope already *is* the documented shape (`TOOLS-SPEC.md` §1.1) and doubling it
 * would double the tokens of every answer. `isError` is true exactly when the envelope carries an
 * `error` object, so a client that reads only the flag and one that reads only the body agree.
 */
function answer(envelope: Envelope<unknown>): CallToolResult {
  return {
    content: [{ type: "text", text: JSON.stringify(envelope) }],
    isError: envelope.status === "error" || envelope.status === "bootstrapping",
  };
}

async function callTool(
  store: Store,
  tools: ReadonlyMap<string, RegisteredTool>,
  params: { readonly name: string; readonly arguments?: Record<string, unknown> | undefined },
): Promise<CallToolResult> {
  const tool = tools.get(params.name);
  if (tool === undefined) {
    const error = new AppError("not_found", `no tool named "${params.name}" in this profile`, {
      hint: `this server registers: ${[...tools.keys()].join(", ")}`,
    });
    return answer(
      fail(
        error,
        boundarySpec(
          store,
          params.name,
          `${params.name} is not registered`,
          "the tool does not exist in this profile",
        ),
      ),
    );
  }
  if (store.kind === "refused") {
    return answer(
      fail(
        store.error,
        boundarySpec(store, tool.name, store.error.message, "the graph was refused at startup"),
      ),
    );
  }
  if (store.kind === "absent") {
    return answer(
      bootstrapping({
        ...boundarySpec(store, tool.name, store.reason, "nothing has been indexed yet"),
        retry_after_ms: STATUS_RETRY_AFTER_MS,
        hint: "the watcher starts a run when the tree changes; call status to see whether one is in flight",
      }),
    );
  }

  const context: ToolContext = { ...store.context, log: scopedLog(tool.name) };
  try {
    const envelope = await tool.handle(context, parseArgs(tool.name, tool.input, params.arguments));
    return answer(route(envelope, tool.name, store.staleReason));
  } catch (error) {
    // A handler returns its errors; reaching this point means a bug or a boundary refusal, and
    // `fail` keeps an `AppError`'s code (`budget_too_large`, `index_locked`) while collapsing
    // anything else to `internal`.
    context.log.error("tool.failed", {
      code: isAppError(error) ? error.code : "internal",
      reason: messageOf(error),
    });
    return answer(
      fail(
        error,
        boundarySpec(
          store,
          tool.name,
          `${tool.name} could not answer`,
          "the call failed before the graph answered",
        ),
      ),
    );
  }
}

/** The exit path's inventory: what a signal has to unwind, and whether it already ran. */
interface Lifecycle {
  watcher: WatchHandle | null;
  conn: Conn | null;
  /**
   * Always `null` today, and kept anyway: the server process never takes the write lock, because
   * `DATABASE.md` §5 makes the worker the single writer and `runWorker` releases its own lock in a
   * `finally`. The slot exists so the one shutdown path releases whatever a future mode holds
   * instead of growing a second one.
   */
  lock: LockHandle | null;
  closed: boolean;
}

/**
 * Stop the watcher, release the lock, close the database — in that order, because the watcher may
 * have a child mid-run and the handle must outlive the last thing that reads from it. Idempotent:
 * a signal and an EOF on stdin can race, and the second call must not close a closed handle.
 */
async function shutdown(lifecycle: Lifecycle, reason: string): Promise<void> {
  if (lifecycle.closed) {
    return;
  }
  lifecycle.closed = true;
  log.info("mcp.shutdown", { reason });
  if (lifecycle.watcher !== null) {
    const watcher = lifecycle.watcher;
    lifecycle.watcher = null;
    await watcher.stop();
  }
  if (lifecycle.lock !== null) {
    const lock = lifecycle.lock;
    lifecycle.lock = null;
    await release(lock);
  }
  if (lifecycle.conn !== null) {
    lifecycle.conn.close();
    lifecycle.conn = null;
  }
}

/**
 * The client hanging up. The transport reports `onclose` only for an explicit `close()`, and
 * `Protocol.connect` owns that hook, so EOF on stdin is the signal the root can observe — it is how
 * a stdio client ends a session.
 */
function clientGone(): Promise<void> {
  return new Promise((resolve) => {
    process.stdin.once("end", () => resolve());
  });
}

/**
 * The calls in flight, and why the exit path waits for them: closing stdin ends a session, but a
 * call already accepted still owes the client an answer. Without this, a client that writes its
 * calls and hangs up keeps only the answers that happened to be synchronous and loses every
 * asynchronous one — the worst possible subset to drop, and exactly what a scripted client does.
 */
function pendingCalls(): { begin(): void; end(): void; settled(): Promise<void> } {
  let pending = 0;
  let idle: (() => void) | null = null;

  const check = (): void => {
    if (pending === 0 && idle !== null) {
      const done = idle;
      idle = null;
      done();
    }
  };

  return {
    begin(): void {
      pending += 1;
    },
    end(): void {
      pending -= 1;
      check();
    },
    async settled(): Promise<void> {
      if (pending > 0) {
        await new Promise<void>((resolve) => {
          idle = resolve;
        });
      }
      // One macrotask after the last handler resolves: the reply it earned is written by the
      // transport on the microtask queue behind it, and the database must outlive that write.
      await new Promise<void>((resolve) => setTimeout(resolve, 0));
    },
  };
}

/**
 * `CODEGRAPH_NO_WATCH` is read here rather than in `#shared/config` because it is not a tool setting:
 * it decides whether *this process* owns a poller, which is the root's business alone. Unset (or
 * empty) means watch; any other value disables the watcher, so a test harness and a supervised
 * deployment both have a switch that cannot be mistaken for a preference.
 */
function watcherDisabled(env: Record<string, string | undefined>): boolean {
  const raw = env.CODEGRAPH_NO_WATCH;
  return raw !== undefined && raw !== "";
}

async function runMcp(config: AppConfig): Promise<number> {
  const store = openStore(config);
  if (store.kind === "refused") {
    // Refused once, logged once, and every call answers with that refusal: the session stays usable
    // so the agent can read the reason instead of losing the whole server to one path mismatch.
    log.error("mcp.refused", { code: store.error.code, reason: store.error.message });
  }

  const tools = registerableTools(config.profile);
  const byName = new Map(tools.map((tool) => [tool.name, tool]));

  const server = new Server(
    { name: SERVER_NAME, version: SERVER_VERSION },
    // `tools` is required for `tools/list` and `tools/call`; the instructions text is how the usage
    // guide reaches the agent, since MCP publishes tools and not prompt text.
    { capabilities: { tools: {} }, instructions: INSTRUCTIONS_TEXT },
  );
  server.setRequestHandler(ListToolsRequestSchema, () => ({ tools: tools.map(descriptorOf) }));
  const pending = pendingCalls();
  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    pending.begin();
    try {
      return await callTool(store, byName, request.params);
    } finally {
      pending.end();
    }
  });

  const lifecycle: Lifecycle = {
    watcher: null,
    conn: store.kind === "ready" ? store.conn : null,
    lock: null,
    closed: false,
  };
  const onSignal = (signal: NodeJS.Signals): void => {
    log.info("mcp.signal", { signal });
    void shutdown(lifecycle, signal).then(() => process.exit(0));
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  // A refused store is not watched: the graph belongs to a tree this process may not serve, and an
  // index run here would rewrite another project's database.
  if (!watcherDisabled(process.env) && store.kind !== "refused") {
    lifecycle.watcher = startWatch({ repoRoot: config.repoRoot, dataDir: config.dataDir });
  }

  await server.connect(new StdioServerTransport());
  log.info("mcp.ready", {
    profile: config.profile,
    repoRoot: config.repoRoot,
    dataDir: config.dataDir,
    tools: [...byName.keys()],
    watching: lifecycle.watcher !== null,
    store: store.kind,
  });

  await clientGone();
  await pending.settled();
  await shutdown(lifecycle, "client closed stdin");
  return 0;
}

function flagValue(argv: readonly string[], name: string): string | undefined {
  const index = argv.indexOf(`--${name}`);
  return index === -1 ? undefined : argv[index + 1];
}

/**
 * The flags `index/handler.ts` puts on the child's command line (`worker --mode full [--force]`).
 * An unrecognised mode is ignored rather than refused, so the worker's own default still applies
 * and a future flag cannot break an older server.
 */
function workerOptions(argv: readonly string[], config: AppConfig) {
  const raw = flagValue(argv, "mode");
  const mode: WorkerMode | undefined = raw === "full" || raw === "fast" ? raw : undefined;
  return {
    repoRoot: config.repoRoot,
    dataDir: config.dataDir,
    ...(mode === undefined ? {} : { mode }),
    ...(argv.includes("--force") ? { force: true } : {}),
  };
}

/**
 * `status` as a CLI mode: a few human lines on stdout. This is not a protocol stream — the mode
 * exists so an operator can look without an MCP client — which is why the one stdout write in this
 * file is allowed here and nowhere else.
 */
function runStatus(config: AppConfig): number {
  const store = openStore(config);
  if (store.kind === "refused") {
    log.error("status.refused", { code: store.error.code, reason: store.error.message });
    return 1;
  }
  if (store.kind === "absent") {
    process.stdout.write(`${store.reason}\n`);
    return 0;
  }
  const envelope = statusHandle({ ...store.context, log: scopedLog("status") }, {});
  const lines: string[] = [`${envelope.status}: ${envelope.summary}`];
  const payload = envelope.payload;
  if (payload !== null) {
    lines.push(
      `project  ${payload.project.name ?? "unknown"} (${payload.project.root ?? "unrecorded"})`,
      `database ${payload.project.db_path}`,
      `graph    ${payload.graph.nodes} nodes, ${payload.graph.edges} edges, ${payload.graph.size_bytes} bytes`,
      `health   ${payload.health.indexer ?? "unknown"}, ${payload.health.consecutive_failures} consecutive failures`,
    );
  }
  process.stdout.write(`${lines.join("\n")}\n`);
  store.conn.close();
  return 0;
}

/**
 * The one entry `bin/codegraph.ts` calls. Every mode resolves the same config first, so the tree and
 * the data dir are decided in exactly one place, and a start-up failure is a logged exit 2 rather
 * than an exception the CLI would print to stdout.
 */
export async function main(mode: string, argv: string[]): Promise<number> {
  try {
    const config = loadConfig();
    switch (mode) {
      case "mcp":
        return await runMcp(config);
      case "index":
      case "worker":
        return await runWorker(workerOptions(argv, config));
      case "status":
        return runStatus(config);
      default:
        log.error("unknown mode", { mode });
        return 2;
    }
  } catch (error) {
    log.error("startup failed", {
      mode,
      code: isAppError(error) ? error.code : "internal",
      reason: messageOf(error),
    });
    return 2;
  }
}
