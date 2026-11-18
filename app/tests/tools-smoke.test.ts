import { Database } from "bun:sqlite";
import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { TOOL_NAMES } from "#shared/instructions";
import { runWorker, WORKER_EXIT } from "../src/indexer/worker";

/**
 * The phase 2 closing gate (`TASK-028`): the seven registered tools answering over the real MCP
 * protocol against a database the real worker published.
 *
 * Every other tool test in this directory either drives a handler directly or hands it a
 * hand-written `GraphReader`. Those prove what each tool does with a reader's answers; none of them
 * proves that the composition root publishes the surface, parses the arguments and wires the store
 * the way a client actually meets it. So this file is deliberately end to end: the worker indexes a
 * two-file TypeScript fixture into a temp data dir, and a spawned server — rooted at that fixture
 * by its own flags — is driven over stdio exactly as a client drives them.
 *
 * The fixture is chosen so each tool has something true to say rather than an empty list:
 * `src/report.ts` declares a class with two methods and two functions, one of which calls the other
 * in the same file, and `src/cli.ts` calls across the file boundary. That gives `trace` a call edge
 * in both directions, `search` one exact-name hit that must rank first, `impact` a cross-file
 * dependent, and `expand` a class whose members are the two methods.
 *
 * The working tree is drifted *after* the publish on purpose: `summary {scope: "change"}` has
 * nothing to describe on a clean tree, and the drift is what makes its `base` and `risk` meaningful.
 *
 * Nothing here is a unit under test, so there is no seam to stub. The waits are bounded instead:
 * one hard deadline per session kills the child, `finish()` always runs, and the temp workspace is
 * removed in `afterAll`, so a wedged server fails the suite rather than hanging it.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "codegraph.ts");

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "tools-smoke-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

/** Generous for one spawn, one fixture index and eight calls; short enough that a hang is a failure. */
const HARD_TIMEOUT_MS = 60_000;

/** `bun test`'s 5 s default would fail on the worker run, not on an assertion. */
const TEST_TIMEOUT_MS = HARD_TIMEOUT_MS + 15_000;

const REPORT_PATH = "src/report.ts";
const CLI_PATH = "src/cli.ts";

const REPORT_SOURCE = `export class ReportBuilder {
  private readonly rows: string[] = [];

  add(row: string): void {
    this.rows.push(row);
  }

  renderReport(): string {
    return this.rows.join("\\n");
  }
}

export function buildReport(rows: string[]): string {
  const builder = new ReportBuilder();
  for (const row of rows) {
    builder.add(row);
  }
  return builder.renderReport();
}

export function emitReport(rows: string[]): string {
  return buildReport(rows);
}
`;

const CLI_SOURCE = `import { emitReport } from "./report";

export function main(): string {
  return emitReport(["alpha", "beta"]);
}
`;

const BUILD_REPORT_QN = `${REPORT_PATH}::buildReport`;
const EMIT_REPORT_QN = `${REPORT_PATH}::emitReport`;
const CLASS_QN = `${REPORT_PATH}::ReportBuilder`;
const RENDER_REPORT_QN = `${REPORT_PATH}::ReportBuilder.renderReport`;
const MAIN_QN = `${CLI_PATH}::main`;

/** The nine fields `TOOLS-SPEC.md` §1.1 requires of every answer, in its own order. */
const ENVELOPE_FIELDS = [
  "status",
  "summary",
  "read_status",
  "coverage",
  "totals",
  "truncated",
  "truncation_reason",
  "next",
  "next_tools",
] as const;

/** One JSON-RPC message; only the members this test reads are declared. */
interface JsonRpcMessage {
  readonly jsonrpc?: unknown;
  readonly id?: unknown;
  readonly result?: unknown;
  readonly error?: unknown;
}

/** The envelope after {@link envelopeOf} has checked it, so a test can read what it asserted. */
interface EnvelopeBody {
  readonly status: string;
  readonly summary: string;
  readonly read_status: {
    readonly generation: string;
    readonly indexed_at: string;
    readonly stale: boolean;
  };
  readonly coverage: {
    readonly complete: boolean;
    readonly indexed: number;
    readonly unresolved_edges: number;
  };
  readonly totals: Record<string, unknown>;
  readonly truncated: boolean;
  readonly truncation_reason: string | null;
  readonly next: string | null;
  readonly next_tools: readonly string[];
  readonly payload: unknown;
  readonly error?: unknown;
}

/** What one server process wrote, collected at {@link Session.finish}. */
interface SessionReport {
  readonly timedOut: boolean;
  readonly exitCode: number;
  /** Requests written to stdin; each one is owed exactly one stdout line. */
  readonly requests: number;
  readonly stdoutLines: number;
  /** Stdout lines that are not JSON at all: any of these corrupts the session. */
  readonly nonJsonLines: readonly string[];
  /** Stdout lines that parse but are not JSON-RPC 2.0 objects. */
  readonly protocolViolations: readonly string[];
  /** Responses nobody awaited, which would mean a duplicated or invented answer. */
  readonly unsolicitedResponses: number;
  readonly stderrLines: readonly string[];
  readonly elapsedMs: number;
}

interface Session {
  readonly tools: readonly string[];
  readonly protocolVersion: string;
  readonly serverName: string;
  /** One `tools/call`, parsed into the envelope every answer must be. */
  call(name: string, args: Record<string, unknown>): Promise<EnvelopeBody>;
  /** Closes stdin, drains the process and reports what it wrote. Idempotent. */
  finish(): Promise<SessionReport>;
}

function asRecord(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`expected a JSON object, got ${JSON.stringify(value)}`);
  }
  return value as Record<string, unknown>;
}

function asArray(value: unknown): readonly unknown[] {
  if (!Array.isArray(value)) {
    throw new Error(`expected a JSON array, got ${JSON.stringify(value)}`);
  }
  return value;
}

function asString(value: unknown): string {
  if (typeof value !== "string") {
    throw new Error(`expected a string, got ${JSON.stringify(value)}`);
  }
  return value;
}

function asNumber(value: unknown): number {
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`expected a finite number, got ${JSON.stringify(value)}`);
  }
  return value;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * The nine envelope fields, the relation value and the published generation, checked once per
 * response so no caller can assert a tool's payload while skipping the contract around it.
 *
 * The generation is required to be non-empty because every call in this file is made against a
 * published database: an empty one would mean a tool answered `bootstrapping` from a store that
 * was supposed to be ready, which is the exact failure a fake reader cannot show.
 */
function envelopeOf(tool: string, text: string): EnvelopeBody {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    throw new Error(`${tool}: the tool result text is not JSON: ${text.slice(0, 200)}`);
  }
  const record = asRecord(parsed);
  for (const field of ENVELOPE_FIELDS) {
    expect({ tool, field, present: Object.hasOwn(record, field) }).toEqual({
      tool,
      field,
      present: true,
    });
  }
  const readStatus = asRecord(record.read_status);
  expect(asString(readStatus.generation)).toMatch(/^g-[0-9a-f]{6}$/);
  expect(["eq", "gte"]).toContain(asString(asRecord(record.totals).total_relation));
  expect(["ok", "ambiguous", "not_found", "error", "bootstrapping"]).toContain(
    asString(record.status),
  );
  // The cast is the boundary this helper exists to check: every field above was just asserted.
  return record as unknown as EnvelopeBody;
}

/**
 * One server over stdio, driven the way a client drives it: write requests, read stdout lines until
 * the ids that were awaited have answered, then close stdin and drain.
 *
 * Responses are dispatched by id through a waiter map rather than by position, because the calls in
 * this file are staged: `expand` needs the class qn an earlier `search` returned, and reading by
 * arrival order would make the pump and the test share an index.
 */
async function openSession(options: {
  readonly repoRoot: string;
  readonly dataDir: string;
}): Promise<Session> {
  const startedAt = Date.now();
  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "mcp",
      "--repo-root",
      options.repoRoot,
      "--data-dir",
      options.dataDir,
    ],
    cwd: APP_ROOT,
    env: {
      ...process.env,
      CODEGRAPH_CURSOR_KEY: CURSOR_KEY,
      // The tree and the data dir are this process's own flags; these two are stated, not
      // inherited, so no harness setting can decide what this test proves.
      CODEGRAPH_NO_WATCH: "1",
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  const stderrDrain = new Response(proc.stderr).text();
  const waiters = new Map<
    number,
    { resolve: (message: JsonRpcMessage) => void; reject: (error: Error) => void }
  >();
  const stdoutLines: string[] = [];
  const nonJsonLines: string[] = [];
  const protocolViolations: string[] = [];
  let unsolicitedResponses = 0;
  let requests = 0;
  let buffered = "";
  let eof = false;
  let timedOut = false;
  let nextId = 1;

  // The only bound on a wedged server: it is killed, which closes stdout, which settles every
  // pending waiter with an error instead of leaving the suite parked on a read.
  const killTimer = setTimeout(() => {
    timedOut = true;
    proc.kill("SIGKILL");
  }, HARD_TIMEOUT_MS);

  function ingest(line: string): void {
    if (line.trim() === "") {
      return;
    }
    stdoutLines.push(line);
    let parsed: unknown;
    try {
      parsed = JSON.parse(line);
    } catch {
      nonJsonLines.push(line);
      return;
    }
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      protocolViolations.push(line);
      return;
    }
    const message = parsed as JsonRpcMessage;
    if (message.jsonrpc !== "2.0") {
      protocolViolations.push(line);
    }
    const id = message.id;
    const waiter = typeof id === "number" ? waiters.get(id) : undefined;
    if (typeof id !== "number" || waiter === undefined) {
      unsolicitedResponses += 1;
      return;
    }
    waiters.delete(id);
    waiter.resolve(message);
  }

  const pump = (async () => {
    try {
      for (;;) {
        const chunk = await reader.read();
        if (chunk.done) {
          break;
        }
        buffered += decoder.decode(chunk.value, { stream: true });
        let newline = buffered.indexOf("\n");
        while (newline !== -1) {
          ingest(buffered.slice(0, newline));
          buffered = buffered.slice(newline + 1);
          newline = buffered.indexOf("\n");
        }
      }
      buffered += decoder.decode();
      if (buffered !== "") {
        ingest(buffered);
        buffered = "";
      }
    } catch (error) {
      protocolViolations.push(`stdout could not be read: ${messageOf(error)}`);
    } finally {
      eof = true;
      for (const waiter of waiters.values()) {
        waiter.reject(new Error("the server closed stdout before answering"));
      }
      waiters.clear();
    }
  })();

  async function send(message: unknown): Promise<void> {
    if (eof) {
      throw new Error("the server stopped before the request could be written");
    }
    proc.stdin.write(`${JSON.stringify(message)}\n`);
    await proc.stdin.flush();
  }

  async function request(method: string, params: Record<string, unknown>): Promise<JsonRpcMessage> {
    const id = nextId;
    nextId += 1;
    requests += 1;
    const answered = new Promise<JsonRpcMessage>((resolvePromise, rejectPromise) => {
      waiters.set(id, { resolve: resolvePromise, reject: rejectPromise });
    });
    await send({ jsonrpc: "2.0", id, method, params });
    return await answered;
  }

  let finished: Promise<SessionReport> | null = null;
  function finish(): Promise<SessionReport> {
    finished ??= (async () => {
      proc.stdin.end();
      await pump;
      const exitCode = await proc.exited;
      clearTimeout(killTimer);
      const stderr = await stderrDrain;
      return {
        timedOut,
        exitCode,
        requests,
        stdoutLines: stdoutLines.length,
        nonJsonLines,
        protocolViolations,
        unsolicitedResponses,
        stderrLines: stderr.split("\n").filter((line) => line.trim() !== ""),
        elapsedMs: Date.now() - startedAt,
      };
    })();
    return finished;
  }

  async function call(name: string, args: Record<string, unknown>): Promise<EnvelopeBody> {
    const response = await request("tools/call", { name, arguments: args });
    if (response.error !== undefined) {
      throw new Error(
        `tools/call ${name} answered with a JSON-RPC error: ${JSON.stringify(response.error)}`,
      );
    }
    const result = asRecord(response.result);
    const content = asArray(result.content);
    const part = asRecord(content[0]);
    if (part.type !== "text") {
      throw new Error(`tools/call ${name} returned a ${String(part.type)} part, not text`);
    }
    const envelope = envelopeOf(name, asString(part.text));
    // `answer()` sets `isError` exactly when the envelope is a failure, so a client reading only
    // the flag and one reading only the body must agree.
    expect({ name, isError: result.isError === true }).toEqual({
      name,
      isError: envelope.status === "error" || envelope.status === "bootstrapping",
    });
    return envelope;
  }

  try {
    const initialized = await request("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "tools-smoke-test", version: "0.0.0" },
    });
    if (initialized.error !== undefined) {
      throw new Error(`initialize failed: ${JSON.stringify(initialized.error)}`);
    }
    const handshake = asRecord(initialized.result);
    await send({ jsonrpc: "2.0", method: "notifications/initialized" });

    const listed = await request("tools/list", {});
    const descriptors = asArray(asRecord(listed.result).tools).map(asRecord);
    const tools = descriptors.map((descriptor) => asString(descriptor.name)).sort();
    const serverInfo = asRecord(handshake.serverInfo);

    return {
      tools,
      protocolVersion: asString(handshake.protocolVersion),
      serverName: asString(serverInfo.name),
      call,
      finish,
    };
  } catch (error) {
    clearTimeout(killTimer);
    proc.kill("SIGKILL");
    await pump;
    throw error;
  }
}

/** A git invocation that fails loudly; the fixture's only use of git is its identity. */
function git(root: string, ...args: readonly string[]): string {
  const result = Bun.spawnSync({
    cmd: ["git", "--no-pager", "-C", root, ...args],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString()}`);
  }
  return result.stdout.toString();
}

/** Opens the published graph read-only, the mode every reader in this repository runs under. */
function query<T>(dbPath: string, sql: string): T[] {
  const handle = new Database(dbPath, { readonly: true });
  try {
    return handle.query<T, []>(sql).all();
  } finally {
    handle.close();
  }
}

function metaValue(dbPath: string, key: string): string | null {
  const rows = query<{ value: string }>(dbPath, `SELECT value FROM meta WHERE key = '${key}'`);
  return rows[0]?.value ?? null;
}

function generationRows(dbPath: string): number {
  return query<{ rows: number }>(dbPath, "SELECT count(*) AS rows FROM generation")[0]?.rows ?? 0;
}

let workspace: string | null = null;
let dataDir = "";
let generation: string | null = null;
let head = "";
let serverSession: Session | null = null;

function server(): Session {
  if (serverSession === null) {
    throw new Error("the server session was not opened; read the beforeAll failure first");
  }
  return serverSession;
}

function publishedGeneration(): string {
  if (generation === null) {
    throw new Error("the fixture was not published; read the beforeAll failure first");
  }
  return generation;
}

beforeAll(async () => {
  workspace = await mkdtemp(join(tmpdir(), "codegraph-tools-smoke-"));
  const repoRoot = join(workspace, "repo");
  dataDir = join(workspace, "data");
  await mkdir(join(repoRoot, "src"), { recursive: true });
  await mkdir(dataDir, { recursive: true });
  await writeFile(join(repoRoot, REPORT_PATH), REPORT_SOURCE, "utf8");
  await writeFile(join(repoRoot, CLI_PATH), CLI_SOURCE, "utf8");

  // A commit is what gives the generation a `base_commit`, and `base` in a change summary is that
  // commit: without it the payload's `base` would be the empty string and prove nothing.
  git(repoRoot, "init", "-q");
  git(repoRoot, "add", "-A");
  git(
    repoRoot,
    "-c",
    "user.name=tools smoke",
    "-c",
    "user.email=tools-smoke@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    "fixture",
  );
  head = git(repoRoot, "rev-parse", "HEAD").trim();

  // The worker runs in-process over the fixture; only the MCP servers must be real children.
  const code = await runWorker({ repoRoot, dataDir, force: true, mode: "full" });
  expect(code).toBe(WORKER_EXIT.published);

  const dbPath = join(dataDir, "graph.db");
  generation = metaValue(dbPath, "generation");
  expect(generation).toMatch(/^g-[0-9a-f]{6}$/);
  expect(generationRows(dbPath)).toBe(1);

  // Drift the tree *after* the publish: `summary {scope: "change"}` needs a diff to describe.
  await writeFile(join(repoRoot, REPORT_PATH), `${REPORT_SOURCE}\n// drift after the publish\n`);

  serverSession = await openSession({ repoRoot, dataDir });
}, TEST_TIMEOUT_MS);

afterAll(async () => {
  try {
    // Idempotent: the last test already finished it, and this only covers an earlier failure.
    await serverSession?.finish();
  } finally {
    if (workspace !== null) {
      await rm(workspace, { recursive: true, force: true });
      workspace = null;
    }
  }
});

describe("the seven tools over MCP against a published generation", () => {
  test("the server publishes the seven tools on its one surface", () => {
    const surface = server();

    expect({ server: surface.serverName, version: surface.protocolVersion }).toEqual({
      server: "codegraph",
      version: PROTOCOL_VERSION,
    });
    expect(surface.tools).toHaveLength(7);
    expect(surface.tools).toEqual([...TOOL_NAMES].sort());
    expect(surface.tools).toContain("index");
  });

  test("status reports the generation the worker published", async () => {
    const envelope = await server().call("status", {});
    expect(envelope.status).toBe("ok");
    expect(envelope.read_status.generation).toBe(publishedGeneration());
    expect(envelope.read_status.indexed_at).not.toBe("");

    const payload = asRecord(envelope.payload);
    const project = asRecord(payload.project);
    const graph = asRecord(payload.graph);
    expect(asString(project.db_path).endsWith("graph.db")).toBe(true);
    expect(asNumber(graph.nodes)).toBeGreaterThan(0);
    expect(asNumber(graph.edges)).toBeGreaterThan(0);
    expect(asRecord(payload.health).indexer).toBe("idle");
    expect(asArray(payload.languages).length).toBeGreaterThan(0);
  });

  test("summary scope change returns the base commit and a risk with driver sentences", async () => {
    const envelope = await server().call("summary", { scope: "change" });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    expect(asString(payload.base)).toBe(head);

    const changed = asArray(payload.changed_files).map(asString);
    expect(changed).toContain(REPORT_PATH);
    // A deletion would be a tombstone, and this fixture has none.
    expect(asArray(payload.removed_symbols)).toEqual([]);

    const risk = asRecord(payload.risk);
    expect(asNumber(risk.score)).toBeGreaterThanOrEqual(0);
    expect(asNumber(risk.score)).toBeLessThanOrEqual(1);
    expect(["low", "medium", "high"]).toContain(asString(risk.band));
    const drivers = asArray(risk.drivers).map(asString);
    expect(drivers.length).toBeGreaterThan(0);
    for (const driver of drivers) {
      // A driver is a sentence an agent can quote, not a bare count.
      expect(driver.split(" ").length).toBeGreaterThan(1);
    }
  });

  test("trace on a known function returns a resolution block and its call edges", async () => {
    const envelope = await server().call("trace", { query: "emitReport" });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    const resolution = asRecord(payload.resolution);
    expect(asString(resolution.query)).toBe("emitReport");
    expect(asString(resolution.matched)).toBe(EMIT_REPORT_QN);
    expect(asString(resolution.kind)).toBe("function");
    expect(asString(resolution.how)).not.toBe("");
    expect(asString(resolution.confidence)).toBe("extracted");

    expect(asString(asRecord(payload.node).qn)).toBe(EMIT_REPORT_QN);

    const incoming = asArray(payload.in)
      .map(asRecord)
      .flatMap((group) => asArray(group.items).map(asRecord));
    expect(incoming.map((item) => asString(item.qn))).toContain(MAIN_QN);

    const outgoing = asArray(payload.out)
      .map(asRecord)
      .flatMap((group) => asArray(group.items).map(asRecord));
    expect(outgoing.map((item) => asString(item.qn))).toContain(BUILD_REPORT_QN);
    // An unresolved call site the walk did not bind is named, never dropped: that list is what
    // makes a later absence claim readable.
    for (const row of asArray(payload.unresolved).map(asRecord)) {
      expect(asString(row.name)).not.toBe("");
      expect(asString(row.evidence)).not.toBe("");
      expect(asNumber(row.line)).toBeGreaterThan(0);
    }
  });

  test("search ranks the exact name first", async () => {
    const envelope = await server().call("search", { query: "renderReport" });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    expect(payload.mode).toBe("bm25");
    const results = asArray(payload.results).map(asRecord);
    expect(results.length).toBeGreaterThan(0);
    expect(asString(results[0]?.qn)).toBe(RENDER_REPORT_QN);
    for (const result of results) {
      // `degree` is the documented disambiguator; a missing one would be an undocumented payload.
      const degree = asRecord(result.degree);
      expect(asNumber(degree.in)).toBeGreaterThanOrEqual(0);
      expect(asNumber(degree.out)).toBeGreaterThanOrEqual(0);
    }
  });

  test("impact on a fixture file reaches the cross-file caller", async () => {
    const envelope = await server().call("impact", { paths: [REPORT_PATH] });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    const changed = asArray(payload.changed).map(asRecord);
    expect(changed).toHaveLength(1);
    expect(asString(changed[0]?.path)).toBe(REPORT_PATH);
    expect(asNumber(changed[0]?.symbols)).toBeGreaterThan(0);

    // A real generation has the call edges; an empty list here would still be explicit, so the
    // assertion names the row this fixture was built to produce.
    const impacted = asArray(payload.impacted).map(asRecord);
    expect(impacted.length).toBeGreaterThan(0);
    expect(impacted.map((row) => asString(row.qn))).toContain(MAIN_QN);
    for (const row of impacted) {
      expect(asNumber(row.hop)).toBeGreaterThanOrEqual(1);
      expect(asString(row.via)).not.toBe("");
      expect(asString(row.tier)).not.toBe("");
      expect(typeof row.tested).toBe("boolean");
    }

    const risk = asRecord(payload.risk);
    expect(asArray(risk.drivers).length).toBeGreaterThan(0);
    expect(asNumber(asRecord(envelope.totals).impacted_total)).toBeGreaterThan(0);
  });

  test("expand on the class qn from search returns its members", async () => {
    const found = await server().call("search", { query: "ReportBuilder", kind: "class" });
    expect(found.status).toBe("ok");
    const hits = asArray(asRecord(found.payload).results).map(asRecord);
    expect(hits).toHaveLength(1);
    const classQn = asString(hits[0]?.qn);
    expect(classQn).toBe(CLASS_QN);

    const envelope = await server().call("expand", { target: classQn });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    expect(payload.what).toBe("members");
    // Inference is disclosed, never silent (`TOOLS-SPEC.md` §7).
    expect(asString(payload.why)).toBe("inferred: class -> members");
    expect(asString(asRecord(payload.target).qn)).toBe(classQn);

    const members = asArray(payload.members).map(asRecord);
    const names = members.map((member) => asString(member.name));
    expect(names).toEqual(expect.arrayContaining(["add", "renderReport"]));
    expect(asNumber(asRecord(envelope.totals).members_total)).toBeGreaterThanOrEqual(
      members.length,
    );
    for (const member of members) {
      expect(asString(member.kind)).toBe("method");
    }
  });

  test("the index tool runs a real worker and publishes", async () => {
    expect(server().tools).toContain("index");

    const envelope = await server().call("index", { force: true, mode: "fast" });
    expect(envelope.status).toBe("ok");

    const payload = asRecord(envelope.payload);
    expect(payload.mode).toBe("fast");
    expect(asString(payload.previous_generation)).toBe(publishedGeneration());
    expect(asNumber(asRecord(payload.files).parsed)).toBeGreaterThan(0);

    // The proof that this was a real publish and not an envelope about one: the generation the
    // tool named is the one the database now serves. A forced run seeds from an empty stage, so it
    // leaves a single fresh row rather than appending to the previous generation's history.
    const dbPath = join(dataDir, "graph.db");
    const published = asString(envelope.read_status.generation);
    expect(published).not.toBe(publishedGeneration());
    expect(metaValue(dbPath, "generation")).toBe(published);
    expect(generationRows(dbPath)).toBe(1);
  });

  test("the server wrote only JSON-RPC to stdout and exited cleanly", async () => {
    const report = await server().finish();

    expect(report.timedOut).toBe(false);
    expect(report.nonJsonLines).toEqual([]);
    expect(report.protocolViolations).toEqual([]);
    expect(report.unsolicitedResponses).toBe(0);
    expect(report.exitCode).toBe(0);
    // One line per request: nothing was answered twice and nothing was written uninvited.
    expect(report.stdoutLines).toBe(report.requests);
  });
});
