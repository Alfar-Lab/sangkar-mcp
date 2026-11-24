#!/usr/bin/env bun
/**
 * The Phase 0 MCP transport recorder (`RUST-MIGRATION.md` §8 R2, plan `rust-migration` TASK-011).
 *
 * Two modes, one wire:
 *
 * - **record** (default): starts the TypeScript server exactly as a client would — `sangkar mcp` on
 *   stdio, with a `--data-dir` under `$TMPDIR` so the repository's `.sangkar/graph.db` is never
 *   opened or written — sends the fixed session below, and writes every frame it saw to
 *   `rust/tests/vectors/mcp_session.ndjson` (`{"dir":"c2s"|"s2c","raw":"<the bytes>"}`, one line per
 *   frame). The TypeScript answers are also filed, parsed, in `mcp_session_ts_only.json`.
 * - **replay** (`--replay <binary>`): feeds the recorded client frames to another MCP server (the
 *   `rust/spikes/mcp` spike), compares by request id, and answers the four questions the decision
 *   rest on. `rust/spikes/mcp/replay_s2c.ndjson` holds the bytes it got back.
 *
 * The recorder writes and reads raw frames rather than going through `@modelcontextprotocol/sdk`'s
 * client: the vector is evidence about *bytes on the wire* (terminators, framing, in-band envelopes),
 * and a client that re-serialises would put its own encoder between the two servers being compared.
 * `raw` is captured as it arrived — never re-encoded — and the only thing added is the newline a
 * client appends, which is recorded with the frame it belongs to.
 *
 * The session deliberately asks for five things and no more: `initialize`, the `initialized`
 * notification, `tools/list`, `status` with `{}`, and `status` with a `budget` the manifest declares
 * as `integer` (`src/tools/status/manifest.ts`) but sent as a string, which `parseArgs` must refuse.
 */
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const REPO_ROOT = process.cwd();
const VECTORS = join(REPO_ROOT, "rust/tests/vectors");
const SESSION_PATH = join(VECTORS, "mcp_session.ndjson");
const TS_ONLY_PATH = join(VECTORS, "mcp_session_ts_only.json");
const SPIKE_DIR = join(REPO_ROOT, "rust/spikes/mcp");
const REPLAY_PATH = join(SPIKE_DIR, "replay_s2c.ndjson");
/** The version `@modelcontextprotocol/sdk` 1.32.2 announces as `LATEST_PROTOCOL_VERSION`. */
const PROTOCOL_VERSION = "2025-11-25";
/** Generous: `status` opens a store on a cold temp directory; a hang must still be a failure. */
const FRAME_TIMEOUT_MS = 30_000;
const EXIT_TIMEOUT_MS = 10_000;

type Direction = "c2s" | "s2c";

interface WireFrame {
  readonly dir: Direction;
  readonly raw: string;
}

interface ClientFrame {
  /** `null` for a notification: no id, and therefore no answer to wait for. */
  readonly id: number | null;
  readonly method: string;
  readonly raw: string;
}

/** One JSON-RPC frame, spelled the way the TypeScript SDK's client spells it. */
function clientFrame(id: number | null, method: string, params?: unknown): ClientFrame {
  const body: Record<string, unknown> = { jsonrpc: "2.0", ...(id === null ? {} : { id }), method };
  if (params !== undefined) {
    body.params = params;
  }
  return { id, method, raw: JSON.stringify(body) };
}

/**
 * The recorded session. Ids are the ones a client hands out, so they are stable artefacts: the
 * replay sends the same ids and correlates the two servers' answers by them rather than by arrival
 * order (a server is free to answer concurrently, and both do).
 */
function recordedSession(): readonly ClientFrame[] {
  return [
    clientFrame(1, "initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "sangkar-parity-recorder", version: "1.0.0" },
    }),
    clientFrame(null, "notifications/initialized"),
    clientFrame(2, "tools/list"),
    // The orientation call, with the arguments an agent actually sends.
    clientFrame(3, "tools/call", { name: "status", arguments: {} }),
    // Deliberately invalid: `budget` is declared `{ type: "integer" }`, so a string is refused
    // before any graph is read. Question (c) is what the *server* does with such a call.
    clientFrame(4, "tools/call", { name: "status", arguments: { budget: "many" } }),
  ];
}

interface Answer {
  readonly resolve: (raw: string) => void;
  readonly reject: (error: Error) => void;
}

/**
 * One child process, read as a stream of newline-terminated frames. Everything the class notices
 * about the bytes is kept, because the questions are about the bytes: how many newlines arrived,
 * whether a carriage return ever did, whether the stream mentioned `Content-Length`, and whether
 * anything was left over after the last terminator.
 */
class Session {
  readonly frames: WireFrame[] = [];
  /** Answers by request id, keyed by `String(id)` so a string id from another client would not throw. */
  readonly answers = new Map<string, string>();
  exitCode: number | null = null;
  stderrText = "";
  bytesIn = 0;
  newlines = 0;
  carriageReturns = 0;
  contentLengthMentions = 0;
  leftover = "";

  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<string, Answer>();
  private buffer = Buffer.alloc(0);
  private readonly exited: Promise<number | null>;

  constructor(
    readonly label: string,
    readonly command: string,
    readonly args: readonly string[],
  ) {
    this.child = spawn(command, [...args], { cwd: REPO_ROOT, stdio: ["pipe", "pipe", "pipe"] });
    this.exited = new Promise((resolve) => {
      this.child.once("error", (error) => {
        for (const answer of [...this.pending.values()]) {
          answer.reject(new Error(`${label}: could not start ${command}: ${error.message}`));
        }
        resolve(null);
      });
      this.child.once("close", (code) => {
        this.exitCode = code;
        resolve(code);
        for (const answer of [...this.pending.values()]) {
          answer.reject(new Error(`${label}: exited (${code}) with an unanswered request`));
        }
      });
    });
    this.child.stdout.on("data", (chunk: Buffer) => this.absorb(chunk));
    this.child.stderr.on("data", (chunk: Buffer) => {
      this.stderrText += chunk.toString("utf8");
    });
  }

  /**
   * Resolve the answer to `id`, whether or not it has arrived yet. Registration happens before the
   * caller writes the frame, so a fast server cannot answer into the void.
   */
  expect(id: number): Promise<string> {
    const key = String(id);
    const known = this.answers.get(key);
    if (known !== undefined) {
      return Promise.resolve(known);
    }
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(key);
        reject(new Error(`${this.label}: no answer for id ${id} within ${FRAME_TIMEOUT_MS}ms`));
      }, FRAME_TIMEOUT_MS);
      this.pending.set(key, {
        resolve: (raw) => {
          clearTimeout(timer);
          resolve(raw);
        },
        reject: (error) => {
          clearTimeout(timer);
          reject(error);
        },
      });
    });
  }

  send(frame: ClientFrame): void {
    this.frames.push({ dir: "c2s", raw: frame.raw });
    this.child.stdin.write(`${frame.raw}\n`);
  }

  /** Send and wait, which is what a client does one request at a time. */
  async request(frame: ClientFrame): Promise<string> {
    if (frame.id === null) {
      this.send(frame);
      return "";
    }
    const answer = this.expect(frame.id);
    this.send(frame);
    return await answer;
  }

  async close(): Promise<number | null> {
    this.child.stdin.end();
    const timeout = setTimeout(() => this.child.kill("SIGKILL"), EXIT_TIMEOUT_MS);
    try {
      return await this.exited;
    } finally {
      clearTimeout(timeout);
      if (this.buffer.length > 0) {
        this.leftover = this.buffer.toString("utf8");
      }
    }
  }

  private absorb(chunk: Buffer): void {
    this.bytesIn += chunk.length;
    this.newlines += countBytes(chunk, 0x0a);
    this.carriageReturns += countBytes(chunk, 0x0d);
    this.contentLengthMentions += countAll(chunk.toString("utf8"), "Content-Length");
    this.buffer = Buffer.concat([this.buffer, chunk]);
    for (;;) {
      const end = this.buffer.indexOf(0x0a);
      if (end === -1) {
        return;
      }
      // The frame is the bytes *before* the terminator: a `\r` would stay in `raw` on purpose,
      // because a CRLF server is exactly the kind of difference this vector exists to catch.
      const raw = this.buffer.subarray(0, end).toString("utf8");
      this.buffer = this.buffer.subarray(end + 1);
      this.frames.push({ dir: "s2c", raw });
      const id = idOf(raw);
      if (id !== null) {
        this.answers.set(String(id), raw);
        const answer = this.pending.get(String(id));
        if (answer !== undefined) {
          this.pending.delete(String(id));
          answer.resolve(raw);
        }
      }
    }
  }
}

function countBytes(chunk: Buffer, wanted: number): number {
  let total = 0;
  for (const byte of chunk) {
    if (byte === wanted) {
      total += 1;
    }
  }
  return total;
}

function countAll(haystack: string, needle: string): number {
  let total = 0;
  for (let at = haystack.indexOf(needle); at !== -1; at = haystack.indexOf(needle, at + needle.length)) {
    total += 1;
  }
  return total;
}

function idOf(raw: string): number | null {
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed === "object" && parsed !== null && "id" in parsed) {
      const id = (parsed as { id: unknown }).id;
      return typeof id === "number" ? id : null;
    }
  } catch {
    // A frame that is not JSON is evidence too: it is kept in `frames` and reported as unparsable.
  }
  return null;
}

function framingOf(session: Session): Record<string, number | boolean | string> {
  return {
    frames: session.frames.filter((frame) => frame.dir === "s2c").length,
    bytesIn: session.bytesIn,
    newlines: session.newlines,
    carriageReturns: session.carriageReturns,
    contentLengthMentions: session.contentLengthMentions,
    leftoverBytes: Buffer.byteLength(session.leftover),
    exitCode: session.exitCode ?? -1,
  };
}

function bunPath(): string {
  // The recorder may only run under bun: `process.execPath` is then the same binary the TypeScript
  // server is started with, which keeps the child's runtime identical to the one this process used.
  if (process.versions.bun === undefined) {
    throw new Error("mcp-session.ts runs under bun: use ~/.bun/bin/bun run scripts/parity/mcp-session.ts");
  }
  return process.execPath;
}

async function record(): Promise<number> {
  // Outside the repository on purpose: a fresh `--data-dir` means `status` answers "nothing indexed
  // yet" without the recorder ever opening the tree's own graph, and the fixed name keeps the
  // vector's bytes stable (nothing ever publishes a graph there, so the directory stays empty).
  const dataDir = process.env.SANGKAR_PARITY_DATA_DIR ?? join(tmpdir(), "sangkar-parity-mcp-session");
  mkdirSync(dataDir, { recursive: true });

  const args = ["run", "bin/sangkar.ts", "mcp", "--repo-root", ".", "--data-dir", dataDir];
  const session = new Session("typescript", bunPath(), args);
  for (const frame of recordedSession()) {
    await session.request(frame);
  }
  await session.close();

  mkdirSync(VECTORS, { recursive: true });
  const lines = session.frames.map((frame) => JSON.stringify(frame));
  writeFileSync(SESSION_PATH, `${lines.join("\n")}\n`);

  const tsOnly = {
    protocolVersionRequested: PROTOCOL_VERSION,
    serverCommand: `bun run bin/sangkar.ts mcp --repo-root . --data-dir ${dataDir}`,
    framing: framingOf(session),
    responses: session.frames
      .filter((frame) => frame.dir === "s2c")
      .map((frame) => ({ id: idOf(frame.raw), raw: frame.raw })),
  };
  writeFileSync(TS_ONLY_PATH, `${JSON.stringify(tsOnly, null, 2)}\n`);

  process.stdout.write(
    [
      `recorded ${session.frames.length} frames → ${SESSION_PATH}`,
      `  s2c frames: ${tsOnly.framing.frames}, bytes: ${session.bytesIn}, newlines: ${session.newlines}`,
      `  CR bytes: ${session.carriageReturns}, Content-Length mentions: ${session.contentLengthMentions}`,
      `  leftover bytes: ${Buffer.byteLength(session.leftover)}, child exit: ${session.exitCode}`,
      `typescript answers → ${TS_ONLY_PATH}`,
      ...session.stderrText
        .split("\n")
        .filter((line) => line !== "")
        .map((line) => `  stderr: ${line}`),
    ].join("\n") + "\n",
  );
  if (session.frames.length < 6) {
    process.stderr.write(`expected at least 6 frames, recorded ${session.frames.length}\n`);
    return 1;
  }
  return session.exitCode === 0 ? 0 : 1;
}

interface Exchange {
  readonly id: number;
  readonly method: string;
  readonly sent: string;
  readonly typescript: string | null;
  readonly spike: string | null;
}

function readRecorded(): readonly WireFrame[] {
  return readFileSync(SESSION_PATH, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line) => JSON.parse(line) as WireFrame);
}

/**
 * A second, deliberately incomplete session: `initialize`, then straight to `tools/list` with no
 * `notifications/initialized` in between. Question (b) is only answerable by running it.
 */
async function withoutInitialized(binary: string): Promise<Session> {
  const session = new Session("no-initialized", binary, []);
  await session.request(
    clientFrame(1, "initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "sangkar-parity-recorder", version: "1.0.0" },
    }),
  );
  await session.request(clientFrame(2, "tools/list"));
  await session.close();
  return session;
}

/**
 * The spike's own contract, exercised directly: `echo` must answer its arguments as JSON, and must
 * answer a bad argument in-band. The recorded session cannot show the second half — it names
 * `status`, which the spike does not register — so the replay adds these three calls and files them
 * separately from the recorded vector.
 */
async function probes(binary: string): Promise<Session> {
  const session = new Session("probes", binary, []);
  await session.request(
    clientFrame(1, "initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "sangkar-parity-recorder", version: "1.0.0" },
    }),
  );
  session.send(clientFrame(null, "notifications/initialized"));
  await session.request(clientFrame(10, "tools/call", { name: "echo", arguments: { text: "hello", count: 2 } }));
  await session.request(clientFrame(11, "tools/call", { name: "echo", arguments: { text: 42 } }));
  await session.request(clientFrame(12, "tools/call", { name: "status", arguments: {} }));
  await session.close();
  return session;
}

function parseOrNull(raw: string | null): Record<string, unknown> | null {
  if (raw === null) {
    return null;
  }
  try {
    return JSON.parse(raw) as Record<string, unknown>;
  } catch {
    return null;
  }
}

/** The same JSON value with every object's keys sorted, so two spellings can be compared by content. */
function sortedDeep(value: unknown): unknown {
  if (Array.isArray(value)) {
    return value.map(sortedDeep);
  }
  if (typeof value === "object" && value !== null) {
    const sorted: Record<string, unknown> = {};
    for (const key of Object.keys(value).sort()) {
      sorted[key] = sortedDeep((value as Record<string, unknown>)[key]);
    }
    return sorted;
  }
  return value;
}

function outcomeOf(raw: string | null): string {
  const parsed = parseOrNull(raw);
  if (parsed === null) {
    return "no answer";
  }
  if (parsed.error !== undefined) {
    return `jsonrpc error ${JSON.stringify(parsed.error)}`;
  }
  const result = parsed.result as { isError?: boolean } | undefined;
  return result?.isError === true ? "result isError=true" : "result isError=false";
}

async function replay(binaryPath: string): Promise<number> {
  const recorded = readRecorded();
  const answersById = new Map<number, string>();
  for (const frame of recorded.filter((candidate) => candidate.dir === "s2c")) {
    const id = idOf(frame.raw);
    if (id !== null) {
      answersById.set(id, frame.raw);
    }
  }

  // 1. The recorded session, unchanged: same frames, same ids, same order.
  const replaySession = new Session("replay", binaryPath, []);
  const exchanges: Exchange[] = [];
  const clientFrames = recorded.filter((frame) => frame.dir === "c2s");
  for (const frame of clientFrames) {
    const parsed = parseOrNull(frame.raw);
    const id = typeof parsed?.id === "number" ? parsed.id : null;
    const method = typeof parsed?.method === "string" ? parsed.method : "<unparsable>";
    const answer = await replaySession.request({ id, method, raw: frame.raw });
    if (id !== null) {
      exchanges.push({
        id,
        method,
        sent: frame.raw,
        typescript: answersById.get(id) ?? null,
        spike: answer === "" ? null : answer,
      });
    }
  }
  await replaySession.close();

  // 2. The remaining questions, each its own short session.
  const gateSession = await withoutInitialized(binaryPath);
  const probeSession = await probes(binaryPath);

  mkdirSync(SPIKE_DIR, { recursive: true });
  writeFileSync(
    REPLAY_PATH,
    [
      ...replaySession.frames.map((frame) => JSON.stringify({ session: "replay", ...frame })),
      ...gateSession.frames.map((frame) => JSON.stringify({ session: "no-initialized", ...frame })),
      ...probeSession.frames.map((frame) => JSON.stringify({ session: "probes", ...frame })),
    ].join("\n") + "\n",
  );

  // 3. Question (d), the one comparison that can be exact: the declared raw schema versus the
  // object `tools/list` put on the wire. Two separate facts, because they can disagree: a JSON
  // object that survives the round trip with the same members but sorted keys is *semantically*
  // unchanged and *byte-wise* not, and TASK-034 needs to know which one it is looking at.
  const declared = JSON.parse(readFileSync(join(SPIKE_DIR, "echo_schema.json"), "utf8")) as unknown;
  const listed = parseOrNull(replaySession.answers.get("2") ?? null);
  const tools = (listed?.result as { tools?: { name: string; inputSchema: unknown }[] } | undefined)?.tools;
  const echoSchema = tools?.find((tool) => tool.name === "echo")?.inputSchema ?? null;
  const contentIdentical = echoSchema !== null && JSON.stringify(sortedDeep(echoSchema)) === JSON.stringify(sortedDeep(declared));
  const keyOrderPreserved = echoSchema !== null && JSON.stringify(echoSchema) === JSON.stringify(declared);

  // The recorded TypeScript framing, from the sidecar the record run wrote: (a) is a comparison.
  const tsOnly = JSON.parse(readFileSync(TS_ONLY_PATH, "utf8")) as { framing: Record<string, unknown> };

  const toolsCall = exchanges.filter((exchange) => exchange.method === "tools/call");
  const gateAnswer = gateSession.answers.get("2") ?? null;
  const gateOutcome = parseOrNull(gateAnswer);

  const report = {
    binary: binaryPath,
    recordedSession: {
      answers: exchanges.map((exchange) => ({
        id: exchange.id,
        method: exchange.method,
        typescript: outcomeOf(exchange.typescript),
        spike: outcomeOf(exchange.spike),
        identicalBytes: exchange.typescript === exchange.spike,
      })),
    },
    a_framing: {
      typescript: tsOnly.framing,
      spike: framingOf(replaySession),
      terminatorOnlyNewline:
        replaySession.newlines > 0 &&
        replaySession.carriageReturns === 0 &&
        replaySession.contentLengthMentions === 0,
    },
    b_initializedGate: {
      answeredToolsListWithoutInitialized: gateOutcome?.result !== undefined,
      answer: gateAnswer,
    },
    c_inBand: {
      recordedCallOutcomes: toolsCall.map((exchange) => ({
        id: exchange.id,
        spike: outcomeOf(exchange.spike),
      })),
      echoGood: outcomeOf(probeSession.answers.get("10") ?? null),
      echoBadArgument: outcomeOf(probeSession.answers.get("11") ?? null),
      echoBadArgumentRaw: probeSession.answers.get("11") ?? null,
      unknownTool: outcomeOf(probeSession.answers.get("12") ?? null),
      unknownToolRaw: probeSession.answers.get("12") ?? null,
    },
    d_schema: {
      contentIdentical,
      keyOrderPreserved,
      declared: JSON.stringify(declared),
      wire: echoSchema === null ? null : JSON.stringify(echoSchema),
    },
  };

  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
  process.stdout.write(`replay frames → ${REPLAY_PATH}\n`);

  // A timeout, an unanswered request, or a non-newline terminator is a failed replay.
  const failed =
    replaySession.leftover !== "" ||
    replaySession.carriageReturns !== 0 ||
    exchanges.some((exchange) => exchange.spike === null) ||
    probeSession.answers.size < 3;
  return failed ? 1 : 0;
}

async function main(): Promise<number> {
  const argv = process.argv.slice(2);
  if (argv[0] === "--replay") {
    const binary = argv[1];
    if (binary === undefined) {
      process.stderr.write("usage: mcp-session.ts --replay <path-to-server-binary>\n");
      return 2;
    }
    return await replay(binary);
  }
  return await record();
}

process.exit(await main());
