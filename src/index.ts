/**
 * The composition root (`TECH-SPEC.md` §7): the one module that knows both halves of the app. It
 * resolves the config and the registry and runs the open-time guard, registers the seven tools,
 * dispatches a CLI invocation to one of them, serves every registered repository from a pool of
 * readers, and wires the exit path — so every other module can be tested without a server and
 * without a database.
 *
 * Two rules shape the file:
 *
 * - **stdout is the MCP protocol, or one CLI rendering.** No `console.log`, and no
 *   `process.stdout.write` outside {@link runTool} — one envelope, or the human `status` lines, or
 *   the usage block a refusal asked for; every diagnostic goes through `#shared/log`, which owns
 *   stderr. A stray line here corrupts the session rather than merely looking untidy.
 * - **the manifest is the only schema.** Each tool's `input` literal is turned into the JSON Schema
 *   `tools/list` publishes and the arguments `tools/call` accepts, so the advertised surface and the
 *   parsed one cannot drift.
 *
 * Why the low-level `Server` and not `McpServer`: `McpServer.registerTool` takes a Zod raw shape as
 * its `inputSchema`, and zod is not a declared dependency of this app (`package.json`) — it is
 * only reachable transitively through the SDK. The plan's schema is a literal in each manifest, so
 * `Server` plus `setRequestHandler` keeps the SDK and keeps that single source of truth, instead of
 * adding a dependency and a second schema dialect to express the same seven tools.
 *
 * The seven manifests are imported explicitly (`TECH-SPEC.md` §7: the registry is seven lines, no
 * directory scanning, and the dependency graph stays readable). The worker is the only
 * `./indexer/*` import, and it is root-only by design: a tool may never spawn an index run of its
 * own, and nothing here polls a tree — the daemon owns the watchers, one per registered repo
 * (`src/daemon/**`), so a `sangkar mcp` process only ever reads.
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
import {
  CliUsageError,
  type InputField,
  type InputLiteral,
  parseArgs,
  parseCliArgsAndNames,
  splitGlobals,
  type ToolArgs,
  usageOf,
} from "#shared/cli/args";
import {
  EXIT_FAILURE,
  EXIT_USAGE,
  exitCodeOf,
  renderEnvelope,
  statusLines,
} from "#shared/cli/render";
import { type AppConfig, flagValue, loadConfig, withRepo } from "#shared/config";
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
import {
  canonicalRoot,
  type Registry,
  readRegistry,
  registerRepo,
  selectRepo,
} from "#shared/registry";
import { readRepoConfig } from "#shared/repo-config";
import { type Conn, openReader } from "#shared/store/db";
import { type LockHandle, release } from "#shared/store/lock";
import { readSchemaVersion, schemaVersion } from "#shared/store/migrate";
import { createStorePool, type StorePool } from "#shared/store/pool";
import type { ToolContext } from "#shared/store/read/context";
import { type GraphReader, makeReader } from "#shared/store/read/reader";
import { telemetryForRepo } from "#shared/telemetry/for-repo";
import type { SessionMode } from "#shared/telemetry/record";
import type { Telemetry } from "#shared/telemetry/telemetry";
import { runWorker, type WorkerMode } from "./indexer/worker";
import { handle as expandHandle } from "./tools/expand/handler";
import {
  example as expandExample,
  input as expandInput,
  manifest as expandManifest,
} from "./tools/expand/manifest";
import { handle as impactHandle } from "./tools/impact/handler";
import {
  example as impactExample,
  input as impactInput,
  manifest as impactManifest,
} from "./tools/impact/manifest";
import { handle as indexHandle } from "./tools/index/handler";
import {
  example as indexExample,
  input as indexInput,
  manifest as indexManifest,
} from "./tools/index/manifest";
import { handle as searchHandle } from "./tools/search/handler";
import {
  example as searchExample,
  input as searchInput,
  manifest as searchManifest,
} from "./tools/search/manifest";
import { handle as statusHandle } from "./tools/status/handler";
import {
  STATUS_RETRY_AFTER_MS,
  example as statusExample,
  input as statusInput,
  manifest as statusManifest,
} from "./tools/status/manifest";
import { handle as summaryHandle } from "./tools/summary/handler";
import {
  example as summaryExample,
  input as summaryInput,
  manifest as summaryManifest,
} from "./tools/summary/manifest";
import { handle as traceHandle } from "./tools/trace/handler";
import {
  example as traceExample,
  input as traceInput,
  manifest as traceManifest,
} from "./tools/trace/manifest";

/** The graph file inside the resolved data dir (`DATABASE.md` §3.0). */
const DB_FILE = "graph.db";

/**
 * The identity an MCP client reports in `serverInfo`. The version is a literal because the root may
 * not import `package.json` — `tests/boundary.test.ts` confines its imports to `#shared/*`, the
 * indexer and the tool modules — and a second copy of the version is cheaper than a boundary hole.
 */
const SERVER_NAME = "sangkar";
const SERVER_VERSION = "0.3.0-beta.2";

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

/** The one call signature the registry stores; each tool's own args type is narrowed inside it. */
type ToolHandler = (
  ctx: ToolContext,
  args: ToolArgs,
) => Envelope<unknown> | Promise<Envelope<unknown>>;

export interface RegisteredTool {
  readonly name: string;
  readonly description: string;
  readonly input: InputLiteral;
  /**
   * One call the manifest declares as pasteable. Optional because a tool whose smallest useful
   * call is not worth spelling publishes no `examples` rather than a made-up one.
   */
  readonly example?: Readonly<Record<string, unknown>> | undefined;
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
 * then the admin repair. All seven are registered unconditionally — one surface for the MCP client
 * and the CLI alike; what keeps a repair out of the way is the instructions, not a filter.
 */
const TOOLS: readonly RegisteredTool[] = [
  { ...statusManifest, input: statusInput, example: statusExample, handle: bind(statusHandle) },
  { ...summaryManifest, input: summaryInput, example: summaryExample, handle: bind(summaryHandle) },
  { ...traceManifest, input: traceInput, example: traceExample, handle: bind(traceHandle) },
  { ...searchManifest, input: searchInput, example: searchExample, handle: bind(searchHandle) },
  { ...impactManifest, input: impactInput, example: impactExample, handle: bind(impactHandle) },
  { ...expandManifest, input: expandInput, example: expandExample, handle: bind(expandHandle) },
  { ...indexManifest, input: indexInput, example: indexExample, handle: bind(indexHandle) },
];

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * One array field's item schema, in the type the manifest declared and `parseArgs` therefore
 * enforces: publishing `string` for `items: { type: "integer" }` would advertise a call the
 * parser accepts only after coercing it, which is not the contract the schema is supposed to state.
 * A declared `enum` is a string item carrying `values`, for the same reason `propertySchemaOf`
 * spells an enum property as a string: JSON Schema has no `enum` type.
 */
function itemSchemaOf(field: InputField): Record<string, unknown> {
  const items = field.items;
  const declared = items?.type ?? "string";
  const schema: Record<string, unknown> = { type: declared === "enum" ? "string" : declared };
  if (items?.values !== undefined) {
    schema.enum = [...items.values];
  }
  return schema;
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

/**
 * The manifest's `input` table as the `inputSchema` `tools/list` publishes, plus the manifest's one
 * example call when it declares one. `examples` is a JSON Schema keyword at object level, and the
 * SDK's `inputSchema` shape is a catchall, so it travels to the client unchanged.
 */
function inputSchemaOf(
  input: InputLiteral,
  example: Readonly<Record<string, unknown>> | undefined,
): Tool["inputSchema"] {
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
  if (example !== undefined) {
    schema.examples = [example];
  }
  return schema;
}

/** A `tools/list` entry: name, the model-facing description, and the schema from the manifest. */
function descriptorOf(tool: RegisteredTool): Tool {
  return {
    name: tool.name,
    description: tool.description,
    inputSchema: inputSchemaOf(tool.input, tool.example),
  };
}

/**
 * What the store's *file* looked like when the handle was opened. A publish renames a new inode
 * over the live path (`store/publish.ts`), so `ino` is the signal; `size` and `mtimeMs` are what
 * catch a filesystem that hands the replacement the inode of the file it replaced.
 */
export interface FileFingerprint {
  readonly ino: number;
  readonly size: number;
  readonly mtimeMs: number;
}

/** One claim the open-time guard can make about the database it found (`DATABASE.md` §5). */
export type Store =
  | {
      readonly kind: "ready";
      readonly conn: Conn;
      readonly context: ToolContext;
      /** The one non-fatal disagreement: the same tree, recorded under its old path. */
      readonly staleReason: string | null;
      /** The file this handle is serving, so a newer one is a comparison rather than a guess. */
      readonly fingerprint: FileFingerprint | null;
    }
  | { readonly kind: "absent"; readonly reason: string }
  | { readonly kind: "refused"; readonly error: AppError };

/**
 * The file's identity as the OS reports it, or `null` when there is nothing to fingerprint. A
 * missing file is the ordinary "nothing published yet" answer, and an unreadable one is the same
 * answer from this side: neither is a newer generation.
 */
export function fingerprintOf(dbPath: string): FileFingerprint | null {
  try {
    const stat = statSync(dbPath);
    return { ino: stat.ino, size: stat.size, mtimeMs: stat.mtimeMs };
  } catch (error) {
    const code = (error as NodeJS.ErrnoException).code;
    if (code === "ENOENT" || code === "EACCES") {
      return null;
    }
    throw error;
  }
}

/**
 * Whether two fingerprints name the same publication. An unknown fingerprint (`null`) is never the
 * same one: the safe direction is re-opening a handle that was already current.
 */
function sameFile(a: FileFingerprint | null, b: FileFingerprint): boolean {
  return a !== null && a.ino === b.ino && a.size === b.size && a.mtimeMs === b.mtimeMs;
}

/**
 * The refusal a graph written by a newer build deserves. One message for both open paths: a
 * generation this build cannot read is a refusal, never a swap.
 */
function newerSchemaError(found: number, dbPath: string): AppError {
  return new AppError(
    "schema_too_new",
    `the graph at ${dbPath} is schema v${found}; this build ships v${schemaVersion()}`,
    {
      hint: "upgrade the server, or move the data dir aside and re-index with `sangkar index`",
    },
  );
}

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
        hint: "point --data-dir at this tree's own .sangkar directory",
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
    hint: "run `sangkar index` to rebuild it",
  });
}

/**
 * `dbSizeBytes` is the `size` of the fingerprint taken *before* the handle was opened, so it
 * describes the file this reader serves and is the one place a payload may read a size from.
 */
function buildContext(
  config: AppConfig,
  reader: GraphReader,
  dbSizeBytes: number | null,
): ToolContext {
  return {
    reader,
    // Bound to the repository it serves: one process answers for several trees, and a page token
    // minted in one must not address a page in another (`#shared/cursor` compares the stamp after
    // the MAC proved it genuine, so the refusal is `cursor_mismatch`, never a wrong page).
    cursor: createCursors({ key: config.cursorKey, repoId: canonicalRoot(config.repoRoot) }),
    log,
    now: () => new Date(),
    config,
    dbSizeBytes,
  };
}

/**
 * Open-time guard: open the reader read-only, refuse a graph this build must not serve, and treat a
 * missing (or empty) file as "nothing indexed yet" rather than as an error — a first `mcp` session
 * before the first publish is normal, and every tool already knows how to answer `bootstrapping`.
 *
 * The handle is re-opened whenever the file on disk is a newer generation ({@link refreshStore}).
 * DATABASE.md §5's guarantee is about a reader that is *not* refreshed — it keeps serving the
 * generation it opened — not about a server that must answer questions about a tree; an unchanged
 * file still costs a fingerprint comparison, never an `open()`.
 */
export function openStore(config: AppConfig): Store {
  const dbPath = join(config.dataDir, DB_FILE);
  if (!existsSync(dbPath)) {
    return { kind: "absent", reason: `no index at ${dbPath} yet` };
  }
  let conn: Conn | null = null;
  try {
    // Fingerprinted before the handle is opened: a file replaced in between is then recorded as
    // *older* than the generation this connection read, so the next call refreshes again. Stamping
    // the newer print would leave the process serving the older graph until the following change.
    const fingerprint = fingerprintOf(dbPath);
    if (fingerprint === null || fingerprint.size === 0) {
      return { kind: "absent", reason: `the index at ${dbPath} is empty` };
    }
    conn = openReader(dbPath);
    const found = readSchemaVersion(conn.sqlite);
    if (found > schemaVersion()) {
      conn.close();
      return { kind: "refused", error: newerSchemaError(found, dbPath) };
    }
    const reader = makeReader(conn);
    const meta = reader.metaFor();
    if (meta.projectRoot === null) {
      // DATABASE.md §5 compares `meta.project_root`; an index that recorded none gives the guard
      // nothing to compare, so the graph is served and the gap is logged rather than assumed away.
      log.warn("store.identity_unrecorded", { dbPath, generation: meta.generation });
    } else {
      // The recorded half of the same fact, at `info`: a graph published by this build names the
      // tree and the branch it came from, and a healthy start must not read as a warning.
      log.info("store.identity_recorded", {
        projectRoot: meta.projectRoot,
        branch: meta.projectBranch ?? null,
      });
    }
    const guard = guardRoot(meta.projectRoot, config.repoRoot, dbPath);
    if (guard.error !== null) {
      conn.close();
      return { kind: "refused", error: guard.error };
    }
    return {
      kind: "ready",
      conn,
      context: buildContext(config, reader, fingerprint.size),
      staleReason: guard.staleReason,
      fingerprint,
    };
  } catch (error) {
    conn?.close();
    return { kind: "refused", error: refusalOf(error) };
  }
}

/**
 * Re-opens the store when the file on disk is a newer generation.
 *
 * The decision is made on the *file*, not on the store variant: an absent store whose file has just
 * appeared must become ready, which is the flow a server started before the first index goes
 * through on its very next call.
 *
 * A replacement is built before anything is destroyed, so a generation that cannot be served — a
 * newer schema, another tree's graph, a torn file — leaves the previous reader in place with the
 * warning attached instead of dropping the session to an error it cannot recover from.
 */
export function refreshStore(store: Store, config: AppConfig): Store {
  const dbPath = join(config.dataDir, DB_FILE);
  const now = fingerprintOf(dbPath);
  if (now === null) {
    // A vanished file is not a newer generation: an absent store stays absent, and a ready one keeps
    // serving what it already read.
    return store;
  }
  if (store.kind === "ready" && sameFile(store.fingerprint, now)) {
    return store;
  }

  let conn: Conn | null = null;
  try {
    conn = openReader(dbPath);
    const found = readSchemaVersion(conn.sqlite);
    if (found > schemaVersion()) {
      throw newerSchemaError(found, dbPath);
    }
    const reader = makeReader(conn);
    const meta = reader.metaFor();
    const guard = guardRoot(meta.projectRoot, config.repoRoot, dbPath);
    if (guard.error !== null) {
      throw guard.error;
    }
    const replacement: Store = {
      kind: "ready",
      conn,
      context: buildContext(config, reader, now.size),
      staleReason: guard.staleReason,
      fingerprint: now,
    };
    // Ownership moves to `replacement` before the previous handle is closed, so the `catch` below
    // can never close the connection this call just built.
    conn = null;
    if (store.kind === "ready") {
      store.conn.close();
    }
    return replacement;
  } catch (error) {
    conn?.close();
    log.warn("store.refresh_failed", { reason: messageOf(error), dbPath });
    if (store.kind === "ready") {
      // The reader still answers, but from a generation this process now knows is behind: the
      // disclosure is what keeps that answer honest rather than silently old.
      return { ...store, staleReason: "a newer generation could not be opened" };
    }
    return store;
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
  // Nothing that skipped the handler can be the answer that ends an agent's search.
  return {
    generation,
    indexed_at: indexedAt,
    stale: true,
    stale_reason: reason,
    decisive: false,
  };
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
 * The tool's answer with the root's three post-conditions applied: `next_tools` from the routing
 * table (replacing whatever the handler set, never dropping the field), the guard's `root_mismatch`
 * as a staleness disclosure rather than a silent difference, and `decisive` derived from the
 * staleness that survived both.
 */
function route(
  envelope: Envelope<unknown>,
  tool: string,
  staleReason: string | null,
): Envelope<unknown> {
  // The `root_mismatch` downgrade is the last word on staleness, so `decisive` is read from the
  // final value: `stale && decisive` must not be able to describe the same answer.
  const readStatus: ReadStatus =
    staleReason === null
      ? envelope.read_status
      : { ...envelope.read_status, stale: true, stale_reason: staleReason };
  return {
    ...envelope,
    next_tools: nextToolsFor(tool),
    read_status: { ...readStatus, decisive: !readStatus.stale },
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

/**
 * What one protocol call needs from the process it runs in: the surface it publishes, the registry
 * a `repo` argument is resolved against, the configuration the process started on (which owns the
 * pin and the machine-level home), and the two seams that keep the pool and the recorders out of
 * {@link callTool}.
 *
 * The seams are what make the selection order testable without a server: `openFor` hands back a
 * store *and* the context bound to that repository, and `recorderFor` hands back the one recorder
 * that repository's calls belong to.
 */
export interface CallSurface {
  readonly tools: ReadonlyMap<string, RegisteredTool>;
  readonly registry: Registry;
  /** `loadConfig()`'s answer: the root this process resolved, its data dir and its cursor key. */
  readonly startup: AppConfig;
  /** `--repo-root`: the default for a call that names no repository, never a barrier. */
  readonly pinnedRoot: string | null;
  openFor(repoRoot: string): { readonly store: Store; readonly context: ToolContext };
  recorderFor(repoRoot: string): Telemetry;
}

/**
 * One repository's settings. Any other tree is served from `withRepo`'s own `.sangkar`, but the tree
 * this process started on keeps the config it started with: an explicit `--data-dir` was said about
 * *that* tree, so rebuilding it from the root would send the process looking for a graph the caller
 * explicitly placed somewhere else.
 */
function repoConfigIn(startup: AppConfig, repoRoot: string): AppConfig {
  return canonicalRoot(startup.repoRoot) === canonicalRoot(repoRoot)
    ? startup
    : withRepo(startup, repoRoot);
}

/**
 * The context of a store that is not `ready`. An absent or refused graph has no reader, and no
 * branch reaches for one: {@link callTool} answers `bootstrapping` or the refusal before a handler
 * exists. The reader is therefore a property read that throws rather than a fake that would answer
 * — a future branch that consulted it fails loudly instead of reading nothing.
 */
function unreadableReader(): GraphReader {
  const refuse = (): never => {
    throw new AppError("internal", "no graph is open for this repository");
  };
  return new Proxy({} as GraphReader, { get: () => refuse });
}

/**
 * The store a pre-selection answer reads its counts from: the tree this process started on. A call
 * that named no repository — or one this installation does not serve — has no other candidate, and
 * a refusal still owes the client a `read_status`, so the startup handle is the only honest one.
 */
function startupStoreOf(surface: CallSurface): Store {
  return surface.openFor(surface.startup.repoRoot).store;
}

/**
 * One `tools/call`, in the order the contract fixes: find the tool, parse against its manifest,
 * resolve the repository the call is about, then answer from that repository's own reader.
 *
 * The order is load-bearing. `parseArgs` runs first because the selector reads the *parsed* `repo`
 * (so a declared string is checked before it can name anything); selection runs before a store is
 * touched, because `not_found` and `ambiguous` are facts about the registry rather than about any
 * graph; and `repo` is dropped from the record the handler receives, because it chose the store and
 * is not an argument of the tool.
 */
async function callTool(
  surface: CallSurface,
  params: { readonly name: string; readonly arguments?: Record<string, unknown> | undefined },
): Promise<CallToolResult> {
  const started = Date.now();
  let envelope: Envelope<unknown> | null = null;
  let parsed: ToolArgs = {};
  let supplied: readonly string[] = [];
  let errorCode: string | null = null;
  // What the record is filed under when nothing was selected: the only tree the call could mean.
  let recordedRoot = surface.startup.repoRoot;
  try {
    const tool = surface.tools.get(params.name);
    if (tool !== undefined) {
      // The intersection with the manifest is what keeps `arg_names_set` (the one field allowed to
      // be dynamic) from carrying a client-supplied key name: `parseArgs` drops an undeclared key,
      // and a key the schema does not declare is not an argument.
      supplied = Object.keys(params.arguments ?? {}).filter((name) =>
        Object.hasOwn(tool.input, name),
      );
    }
    if (tool === undefined) {
      const error = new AppError("not_found", `no tool named "${params.name}" in this surface`, {
        hint: `this server registers: ${[...surface.tools.keys()].join(", ")}`,
      });
      envelope = fail(
        error,
        boundarySpec(
          startupStoreOf(surface),
          params.name,
          `${params.name} is not registered`,
          "the tool does not exist in this surface",
        ),
      );
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }

    try {
      parsed = parseArgs(tool.name, tool.input, params.arguments);
    } catch (error) {
      // The arguments were refused before a repository could be chosen, so the counts come from the
      // tree this process started on. Same wording as a handler that threw: the call never reached
      // a graph, and nothing here may claim it did.
      scopedLog(tool.name).error("tool.failed", {
        code: isAppError(error) ? error.code : "internal",
        reason: messageOf(error),
      });
      envelope = fail(
        error,
        boundarySpec(
          startupStoreOf(surface),
          tool.name,
          `${tool.name} could not answer`,
          "the call failed before the graph answered",
        ),
      );
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }

    const selection = selectRepo({
      registry: surface.registry,
      requested: typeof parsed.repo === "string" ? parsed.repo : undefined,
      cwd: process.cwd(),
      pinnedRoot: surface.pinnedRoot,
    });
    if (!selection.ok) {
      // A refusal still carries the registry as its hint, so the caller can act without guessing.
      envelope = fail(
        selection.error,
        boundarySpec(
          startupStoreOf(surface),
          tool.name,
          selection.error.message,
          "no repository could be selected for this call",
        ),
      );
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }
    recordedRoot = selection.repoRoot;

    // `repo` picked the store above; the handler gets the arguments that describe the question.
    const { repo: _selector, ...rest } = parsed;
    const { store, context } = surface.openFor(selection.repoRoot);
    if (store.kind === "refused") {
      envelope = fail(
        store.error,
        boundarySpec(store, tool.name, store.error.message, "the graph was refused at startup"),
      );
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }
    if (store.kind === "absent") {
      envelope = bootstrapping({
        ...boundarySpec(store, tool.name, store.reason, "nothing has been indexed yet"),
        retry_after_ms: STATUS_RETRY_AFTER_MS,
        hint: "the watcher starts a run when the tree changes; call status to see whether one is in flight",
      });
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }

    const scoped: ToolContext = { ...context, log: scopedLog(tool.name) };
    try {
      envelope = route(await tool.handle(scoped, rest), tool.name, store.staleReason);
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    } catch (error) {
      // A handler returns its errors; reaching this point means a bug or a boundary refusal, and
      // `fail` keeps an `AppError`'s code (`budget_too_large`, `index_locked`) while collapsing
      // anything else to `internal`.
      scoped.log.error("tool.failed", {
        code: isAppError(error) ? error.code : "internal",
        reason: messageOf(error),
      });
      envelope = fail(
        error,
        boundarySpec(
          store,
          tool.name,
          `${tool.name} could not answer`,
          "the call failed before the graph answered",
        ),
      );
      errorCode = envelope.error?.code ?? null;
      return answer(envelope);
    }
  } finally {
    // `params.name` is whatever the client sent, and on the unknown-tool branch there is no tool to
    // take a name from, so the record's `tool` is always one of the seven or the literal "unknown".
    const recordedTool = surface.tools.has(params.name) ? params.name : "unknown";
    surface.recorderFor(recordedRoot).recordCall({
      tool: recordedTool,
      args: parsed,
      suppliedNames: supplied,
      envelope,
      status: envelope?.status ?? "absent",
      errorCode,
      durationMs: Date.now() - started,
    });
  }
}

/** The exit path's inventory: what a signal has to unwind, and whether it already ran. */
interface Lifecycle {
  /**
   * Every reader this process opened, keyed by repository. The pool is the only closer: a handle a
   * call swapped out is closed by the refresh itself, so nothing here can close one twice.
   */
  readonly pool: StorePool<Store>;
  /**
   * One recorder per served repository, closed last: a record is only useful once the answer it
   * describes was written, and closing flushes each buffer once on whichever exit path won.
   */
  readonly recorders: ReadonlyMap<string, Telemetry>;
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
 * Release the lock, then close every reader, then every recorder: a record describes an answer the
 * reader produced, so nothing may be flushed after the handle behind it is gone. The pool is the
 * only closer of a reader, so a superseded generation closed by its refresh cannot be closed again
 * here. Idempotent: a signal and an EOF on stdin can race, and the second call must not close a
 * closed handle.
 */
async function shutdown(lifecycle: Lifecycle, reason: string): Promise<void> {
  if (lifecycle.closed) {
    return;
  }
  lifecycle.closed = true;
  log.info("mcp.shutdown", { reason });
  if (lifecycle.lock !== null) {
    const lock = lifecycle.lock;
    lifecycle.lock = null;
    await release(lock);
  }
  // The only closer of a reader: no handle is remembered anywhere else, so a call that swapped in a
  // newer generation cannot leave the live one open or close a superseded one twice.
  lifecycle.pool.closeAll();
  for (const telemetry of lifecycle.recorders.values()) {
    telemetry.close();
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

async function runMcp(
  config: AppConfig,
  registry: Registry,
  pinnedRoot: string | null,
): Promise<number> {
  const byName = new Map(TOOLS.map((tool) => [tool.name, tool]));

  // One handle per repository, opened on first use. `close` is the only thing that ever closes a
  // reader, so a call that swapped in a newer generation cannot be closed twice — the refreshed
  // store is what the pool holds and what `shutdown` closes.
  const pool = createStorePool<Store>({
    open: (root) => openStore(repoConfigIn(config, root)),
    close: (store) => {
      if (store.kind === "ready") {
        store.conn.close();
      }
    },
  });

  // One recorder per repository, built the first time that repository answers: a session record
  // belongs to the tree the session served, and `telemetryForRepo` owns the
  // consent/registration/repo-file precedence rather than this file re-deriving it.
  const recorders = new Map<string, Telemetry>();

  /**
   * One repository's reader and the context bound to it.
   *
   * `refreshStore` runs on every call because another process publishes by renaming a new file over
   * `graph.db`; the pool holds the handle in between so a session that names two trees does not pay
   * two cold opens per call. The context is rebuilt for *this* root — `config.repoRoot` and
   * `dataDir` name the tree being answered from, and the cursor is bound to it — rather than reused
   * from the store, which was built from this root but carries no repository identity of its own.
   */
  const openFor = (repoRoot: string): { readonly store: Store; readonly context: ToolContext } => {
    const root = canonicalRoot(repoRoot);
    const repoConfig = repoConfigIn(config, root);
    const store = refreshStore(pool.acquire(root), repoConfig);
    pool.replace(root, store);
    const reader = store.kind === "ready" ? store.context.reader : unreadableReader();
    // The size belongs to the handle this call serves: the print `refreshStore` recorded (or kept)
    // for the file it opened, never a stat of the path.
    const dbSizeBytes = store.kind === "ready" ? (store.fingerprint?.size ?? null) : null;
    return { store, context: buildContext(repoConfig, reader, dbSizeBytes) };
  };

  const recorderFor = (repoRoot: string): Telemetry => {
    const root = canonicalRoot(repoRoot);
    const known = recorders.get(root);
    if (known !== undefined) {
      return known;
    }
    const telemetry = telemetryForRepo({
      config,
      repo: readRepoConfig(root, { dataDir: repoConfigIn(config, root).dataDir }),
      registered: registry.repos.includes(root),
      sessionMode: "mcp",
      serverVersion: SERVER_VERSION,
      graphSchemaVersion: schemaVersion(),
    });
    recorders.set(root, telemetry);
    return telemetry;
  };

  const surface: CallSurface = {
    tools: byName,
    registry,
    startup: config,
    pinnedRoot,
    openFor,
    recorderFor,
  };

  // The tree this process started on: what `mcp.ready` reports, and the store a call that selected
  // no repository reads its counts from. A refusal here is logged once — every call that selects
  // that tree answers with it, so the agent can read the reason instead of losing the server.
  const atStartup = openFor(config.repoRoot);
  if (atStartup.store.kind === "refused") {
    log.error("mcp.refused", {
      code: atStartup.store.error.code,
      reason: atStartup.store.error.message,
    });
  }

  const lifecycle: Lifecycle = { pool, recorders, lock: null, closed: false };

  const server = new Server(
    { name: SERVER_NAME, version: SERVER_VERSION },
    // `tools` is required for `tools/list` and `tools/call`; the instructions text is how the usage
    // guide reaches the agent, since MCP publishes tools and not prompt text.
    { capabilities: { tools: {} }, instructions: INSTRUCTIONS_TEXT },
  );
  server.setRequestHandler(ListToolsRequestSchema, () => ({ tools: TOOLS.map(descriptorOf) }));
  const pending = pendingCalls();
  server.setRequestHandler(CallToolRequestSchema, async (request) => {
    pending.begin();
    try {
      // Eviction happens here rather than on a timer: the process is already awake, no handle is
      // mid-read, and an idle one gives its file descriptors back before this call opens another.
      pool.releaseIdle();
      return await callTool(surface, request.params);
    } finally {
      pending.end();
    }
  });

  const onSignal = (signal: NodeJS.Signals): void => {
    log.info("mcp.signal", { signal });
    void shutdown(lifecycle, signal).then(() => process.exit(0));
  };
  process.once("SIGINT", onSignal);
  process.once("SIGTERM", onSignal);

  await server.connect(new StdioServerTransport());
  log.info("mcp.ready", {
    repos: registry.repos.length,
    store: atStartup.store.kind,
    tools: [...byName.keys()],
  });

  await clientGone();
  await pending.settled();
  await shutdown(lifecycle, "client closed stdin");
  return 0;
}

/**
 * The flags `index/handler.ts` puts on the child's command line (`worker --mode full [--force]`).
 * An unrecognised mode is ignored rather than refused, so the worker's own default still applies
 * and a future flag cannot break an older server.
 */
function workerOptions(argv: readonly string[], config: AppConfig, telemetry: Telemetry) {
  const raw = flagValue(argv, "mode");
  const mode: WorkerMode | undefined = raw === "full" || raw === "fast" ? raw : undefined;
  return {
    repoRoot: config.repoRoot,
    dataDir: config.dataDir,
    // The instance `main` built: the worker records its `index_run` through it and never builds one
    // of its own, so a process emits exactly one session record whichever mode it is.
    telemetry,
    ...(mode === undefined ? {} : { mode }),
    ...(argv.includes("--force") ? { force: true } : {}),
  };
}

/**
 * One tool invocation from the command line: the same manifest, the same store guard and the same
 * envelope a protocol call gets, written to stdout once. This is not a protocol stream — it exists so
 * an operator can use a tool without an MCP client — and it is the only place in this file that
 * writes to stdout.
 *
 * `--help` is answered before the arguments are validated, because a usage block is exactly what a
 * caller asking for help wants even when a required argument is still missing. `splitGlobals` and
 * `parseCliArgsAndNames` share one refusal path because both refuse the argv rather than the call, and
 * both carry the usage block printed here.
 *
 * The whole body sits inside one `try`/`finally` so every outcome — including a usage refusal that
 * never reached a handler — leaves exactly one `call` record. `--help` is the exception: it asks for
 * the usage block, not for a call.
 */
async function runTool(
  name: string,
  argv: readonly string[],
  config: AppConfig,
  registry: Registry,
  pinnedRoot: string | null,
  telemetry: Telemetry,
): Promise<number> {
  const tool = TOOLS.find((candidate) => candidate.name === name);
  if (tool === undefined) {
    // Unreachable through `main`, which resolves the name first; kept so this function's contract
    // does not depend on its caller having done the lookup.
    process.stderr.write(`sangkar: unknown tool "${name}"\n`);
    return EXIT_USAGE;
  }

  const started = Date.now();
  let json = false;
  let parsed: ToolArgs = {};
  let supplied: readonly string[] = [];
  let envelope: Envelope<unknown> | null = null;
  let errorCode: string | null = null;
  let helpRequested = false;
  // Every exit goes through `finish`, so the recorded `status` and the process exit code cannot
  // contradict each other. A throw out of the body leaves the default, which is the code `main`
  // reports for that case too.
  let exitCode: number = EXIT_FAILURE;
  const finish = (code: number): number => {
    exitCode = code;
    return code;
  };

  try {
    try {
      const globals = splitGlobals(argv);
      json = globals.json;
      if (globals.help) {
        // A usage block is not a call: a request for help is recorded by nothing.
        helpRequested = true;
        process.stdout.write(`${usageOf(name, tool.input)}\n`);
        return 0;
      }
      // The supplied names come from the argv reader, never from the parsed record: `parseArgs`
      // fills every declared default, and a defaulted name is not a fact about this caller.
      ({ args: parsed, supplied } = parseCliArgsAndNames(name, tool.input, globals.rest));
    } catch (error) {
      if (error instanceof CliUsageError) {
        process.stderr.write(`${error.message}\n${error.usage}\n`);
        return finish(EXIT_USAGE);
      }
      throw error;
    }

    const selection = selectRepo({
      registry,
      requested: typeof parsed.repo === "string" ? parsed.repo : undefined,
      cwd: process.cwd(),
      pinnedRoot,
    });
    if (!selection.ok) {
      // No repository this installation can answer for: the argv is what is wrong, so this is the
      // usage exit — the same code `main` gives every refusal that happens before work starts.
      errorCode = selection.error.code;
      process.stderr.write(`${selection.error.message}\n`);
      if (selection.error.hint !== undefined) {
        process.stderr.write(`${selection.error.hint}\n`);
      }
      return finish(EXIT_USAGE);
    }

    // The selected tree, not the startup one: `--repo-root` is a default and an explicit `repo`
    // outranks it, so the store must be the one the call named.
    const repoConfig = repoConfigIn(config, selection.repoRoot);
    // `repo` picked the tree above; the handler gets the arguments that describe the question.
    const { repo: _selector, ...rest } = parsed;
    const store = openStore(repoConfig);
    if (store.kind === "refused") {
      // No envelope is built here, so the refusal's own code is the only error code there is.
      errorCode = store.error.code;
      process.stderr.write(`${store.error.message}\n`);
      if (store.error.hint !== undefined) {
        process.stderr.write(`${store.error.hint}\n`);
      }
      return finish(EXIT_FAILURE);
    }
    if (store.kind === "absent") {
      // `status` is the one tool that answers an empty tree instead of failing: "no index yet" is its
      // whole job, and the line an operator reads before the first index run.
      if (name === "status") {
        process.stdout.write(`${store.reason}\n`);
        return finish(0);
      }
      envelope = bootstrapping({
        ...boundarySpec(store, name, store.reason, "nothing has been indexed yet"),
        retry_after_ms: STATUS_RETRY_AFTER_MS,
        hint: "the watcher starts a run when the tree changes; call status to see whether one is in flight",
      });
      process.stdout.write(renderEnvelope(envelope));
      return finish(EXIT_FAILURE);
    }

    // A handler returns its errors, so a throw here is a bug or a boundary refusal — caught, logged
    // and rendered exactly as `callTool` renders it.
    const context: ToolContext = { ...store.context, log: scopedLog(name) };
    try {
      envelope = route(await tool.handle(context, rest), name, store.staleReason);
    } catch (error) {
      context.log.error("tool.failed", {
        code: isAppError(error) ? error.code : "internal",
        reason: messageOf(error),
      });
      envelope = fail(
        error,
        boundarySpec(
          store,
          name,
          `${name} could not answer`,
          "the call failed before the graph answered",
        ),
      );
      process.stdout.write(renderEnvelope(envelope));
      return finish(EXIT_FAILURE);
    }
    process.stdout.write(
      name === "status" && !json ? statusLines(envelope) : renderEnvelope(envelope),
    );
    return finish(exitCodeOf(envelope));
  } finally {
    if (!helpRequested) {
      telemetry.recordCall({
        tool: name,
        args: parsed,
        suppliedNames: supplied,
        envelope,
        // `absent` is the no-graph `status` line: a successful process that produced no envelope.
        // `envelope?.status ?? "error"` would have recorded the commonest CLI invocation as a
        // failure, which is why the fallback reads the exit code `finish` kept.
        status: envelope?.status ?? (exitCode === 0 ? "absent" : "error"),
        errorCode: envelope?.error?.code ?? errorCode,
        durationMs: Date.now() - started,
      });
    }
    // A tool invocation is a one-shot process: nothing else would ever flush this buffer.
    telemetry.close();
  }
}

/**
 * The one entry `bin/sangkar.ts` calls. Every mode resolves the same config first, so the tree and
 * the data dir are decided in exactly one place, and a start-up failure is a logged exit 2 rather
 * than an exception the CLI would print to stdout.
 *
 * The first token is the mode. `mcp`, `worker` and `index` are the verbs this process owns, and
 * every other name is a tool from the registry: `index` is both on purpose, because the verb is the
 * bootstrap and the repair path (a fresh tree has no store for a tool call to answer from) while the
 * tool of the same name is what a client calls. Both read the same `--mode` and `--force`.
 */
export async function main(mode: string, argv: string[]): Promise<number> {
  try {
    // `flagValue` scans the whole argv, so a global flag may sit on either side of the tool token.
    const config = loadConfig({ argv });
    const sessionMode: SessionMode =
      mode === "mcp" ? "mcp" : mode === "worker" || mode === "index" ? "worker" : "cli";
    // Read once per process: every call of this process resolves its `repo` against this snapshot
    // (`selectRepo`), and `--repo-root` is only the *default* a call that names none falls back to.
    const registry = readRegistry(config.registryPath);
    const pinnedRoot = flagValue(argv, "repo-root") ?? null;

    if (mode === "mcp") {
      // The server builds one recorder per repository it serves and closes them in `shutdown`.
      return await runMcp(config, registry, pinnedRoot);
    }

    // The one recorder of a one-shot process, built here because the root is the only place that
    // knows the mode and owns the exit path; `telemetryForRepo` owns the precedence. Its
    // construction is what emits the session record.
    const telemetry = telemetryForRepo({
      config,
      repo: readRepoConfig(config.repoRoot, { dataDir: config.dataDir }),
      registered: registry.repos.includes(canonicalRoot(config.repoRoot)),
      sessionMode,
      serverVersion: SERVER_VERSION,
      graphSchemaVersion: schemaVersion(),
    });
    try {
      if (mode === "index" || mode === "worker") {
        const code = await runWorker(workerOptions(argv, config, telemetry));
        if (mode === "index") {
          registerIfAsked(config, argv, code);
        }
        return code;
      }
      const tool = TOOLS.find((candidate) => candidate.name === mode);
      if (tool === undefined) {
        process.stderr.write(`sangkar: unknown tool "${mode}"\n`);
        return EXIT_USAGE;
      }
      return await runTool(tool.name, argv.slice(1), config, registry, pinnedRoot, telemetry);
    } finally {
      // A one-shot process flushes its own buffer; `mcp` owns its closes through `shutdown`. Both
      // closes are idempotent, so a refusal that returned early still writes its session record.
      telemetry.close();
    }
  } catch (error) {
    log.error("startup failed", {
      mode,
      code: isAppError(error) ? error.code : "internal",
      reason: messageOf(error),
    });
    return EXIT_USAGE;
  }
}

/**
 * The `index` verb's second duty: a tree that just indexed successfully is one the next session can
 * serve, so it joins the machine registry. `--no-register` is the operator's opt-out, and a
 * registration failure is one warning — the run's own exit code is the outcome this process reports.
 *
 * Only the verb registers. `worker` is spawned by a server for a tree it already knows, and a
 * watcher run must never resurrect a repository an operator removed from the registry.
 */
function registerIfAsked(config: AppConfig, argv: readonly string[], code: number): void {
  if ((code !== 0 && code !== 2) || argv.includes("--no-register")) {
    return;
  }
  try {
    const result = registerRepo(config.registryPath, config.repoRoot, { log });
    log.info("index.registered", { repoRoot: config.repoRoot, added: result.added });
  } catch (error) {
    log.warn("index.register_failed", { reason: messageOf(error) });
  }
}
