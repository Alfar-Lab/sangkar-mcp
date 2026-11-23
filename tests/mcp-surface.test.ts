import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { decode } from "@toon-format/toon";
import { TOOL_NAMES } from "#shared/instructions";

/**
 * The composition root's protocol surface (`TECH-SPEC.md` §11), checked by speaking to a real server
 * process instead of importing its registry: the point of this test is that `initialize` and
 * `tools/list` survive the SDK's own framing, that one surface publishes all seven tools, and that
 * nothing but JSON-RPC reaches stdout.
 *
 * Every run gets its own tree and data dir, `SANGKAR_NO_WATCH=1` (a watcher would spawn a worker
 * over an empty fixture and make the run's timing depend on git) and a cursor key of the required
 * length. No database is published, deliberately: `tools/list` must answer before the first index,
 * because that is the state a client connects in.
 *
 * The server is driven the way a client drives it — two requests and one notification, then stdin
 * closed — and the run is bounded by a hard timeout so a wedged server fails the test instead of
 * hanging the suite.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** Generous for two handshakes, short enough that a hang is a failure rather than a wait. */
const HARD_TIMEOUT_MS = 30_000;

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "mcp-surface-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

/** What a client sends to open a session and list the surface; the notification has no response. */
const REQUESTS = [
  {
    jsonrpc: "2.0",
    id: 1,
    method: "initialize",
    params: {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "mcp-surface-test", version: "0.0.0" },
    },
  },
  { jsonrpc: "2.0", method: "notifications/initialized" },
  { jsonrpc: "2.0", id: 2, method: "tools/list", params: {} },
] as const;

/** The id a `tools/call` takes in the sessions below, so `[1, 2, 3]` is the whole answer order. */
const CALL_ID = 3;

interface JsonRpcError {
  readonly code: number;
  readonly message: string;
}

interface JsonRpcResponse {
  readonly id?: number | string;
  readonly result?: unknown;
  readonly error?: JsonRpcError;
}

interface ToolDescriptor {
  readonly name: string;
  readonly description?: string;
  readonly inputSchema: {
    readonly type: string;
    readonly properties?: Record<string, Record<string, unknown>>;
    readonly required?: readonly string[];
    readonly examples?: readonly Record<string, unknown>[];
  };
}

interface Session {
  /** Messages that carried an id: one per request, in arrival order. */
  readonly responses: readonly JsonRpcResponse[];
  /** Every non-blank stdout line, parsed or not. */
  readonly stdoutLines: readonly string[];
  /** The stdout lines that are not JSON at all: the protocol-corruption count. */
  readonly nonJsonLines: readonly string[];
  /** Every non-blank stderr line, which must all be `log`'s JSON records. */
  readonly stderrLines: readonly string[];
  readonly exitCode: number;
}

function parseLines(stream: string): { json: unknown[]; nonJson: string[] } {
  const json: unknown[] = [];
  const nonJson: string[] = [];
  for (const line of stream.split("\n")) {
    if (line.trim() === "") {
      continue;
    }
    try {
      json.push(JSON.parse(line));
    } catch {
      nonJson.push(line);
    }
  }
  return { json, nonJson };
}

/**
 * One server session over stdio. The child is killed in `finally` whether or not it exited, and its
 * temp tree is removed there too, so a failed assertion cannot leave a process or a directory behind.
 *
 * `call` adds one `tools/call` after the handshake, and `outputJson` starts the child with `--json`:
 * those two switches are exactly what the answer's rendering depends on.
 */
async function session(
  options: { readonly call?: string; readonly outputJson?: boolean } = {},
): Promise<Session> {
  const root = await mkdtemp(join(tmpdir(), "sangkar-mcp-surface-"));
  const repoRoot = join(root, "repo");
  const dataDir = join(root, "data");
  await mkdir(repoRoot, { recursive: true });
  await mkdir(dataDir, { recursive: true });

  const requests: readonly unknown[] =
    options.call === undefined
      ? REQUESTS
      : [
          ...REQUESTS,
          {
            jsonrpc: "2.0",
            id: CALL_ID,
            method: "tools/call",
            params: { name: options.call, arguments: {} },
          },
        ];

  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "mcp",
      "--repo-root",
      repoRoot,
      "--data-dir",
      dataDir,
      ...(options.outputJson === true ? ["--json"] : []),
    ],
    cwd: APP_ROOT,
    env: {
      ...process.env,
      // The roots are on the command line, not in the child's environment, so the session is rooted
      // by the flags it states and nothing an inherited variable says can decide what this proves.
      // The home is the fixture's own: the first run creates `<home>/config.json`, and an inherited
      // `SANGKAR_HOME` would put that file in the developer's real config directory.
      SANGKAR_HOME: join(root, "home"),
      SANGKAR_CURSOR_KEY: CURSOR_KEY,
      SANGKAR_NO_WATCH: "1",
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const timer = setTimeout(() => proc.kill("SIGKILL"), HARD_TIMEOUT_MS);
  try {
    proc.stdin.write(requests.map((request) => `${JSON.stringify(request)}\n`).join(""));
    await proc.stdin.flush();
    proc.stdin.end();

    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);

    const out = parseLines(stdout);
    const responses = out.json.filter((message): message is JsonRpcResponse => {
      return typeof message === "object" && message !== null && "id" in message;
    });
    return {
      responses,
      stdoutLines: stdout.split("\n").filter((line) => line.trim() !== ""),
      nonJsonLines: out.nonJson,
      stderrLines: stderr.split("\n").filter((line) => line.trim() !== ""),
      exitCode,
    };
  } finally {
    clearTimeout(timer);
    proc.kill();
    await rm(root, { recursive: true, force: true });
  }
}

function toolNames(session: Session): string[] {
  const list = session.responses[1];
  const result = list?.result as { tools?: readonly ToolDescriptor[] } | undefined;
  return (result?.tools ?? []).map((tool) => tool.name).sort();
}

function toolsOf(session: Session): readonly ToolDescriptor[] {
  const result = session.responses[1]?.result as { tools?: readonly ToolDescriptor[] } | undefined;
  return result?.tools ?? [];
}

/**
 * The text part of the last answer, which for a `tools/call` session is that call's result: one
 * string field, so nothing here may look at it as a line stream.
 */
function toolAnswer(run: Session): { readonly text: string; readonly isError: boolean } {
  const response = run.responses.at(-1);
  const result = response?.result as
    | { content?: readonly { type?: string; text?: string }[]; isError?: boolean }
    | undefined;
  const text = result?.content?.[0]?.text;
  if (typeof text !== "string") {
    throw new Error(`tools/call returned no text content: ${JSON.stringify(response)}`);
  }
  return { text, isError: result?.isError === true };
}

describe("MCP surface: the answer text, TOON by default", () => {
  test(
    "tools/call status decodes as TOON by default and parses as JSON under --json",
    async () => {
      const toon = await session({ call: "status" });
      const json = await session({ call: "status", outputJson: true });

      for (const run of [toon, json]) {
        // The framing is untouched by the rendering: one JSON-RPC message per stdout line, so a
        // TOON payload stays inside the `text` field and never reaches the protocol stream.
        expect(run.nonJsonLines).toEqual([]);
        expect(run.stdoutLines).toHaveLength(3);
        expect(run.responses.map((response) => response.id)).toEqual([1, 2, CALL_ID]);
        expect(run.exitCode).toBe(0);
      }

      // The default: TOON, which is not the JSON document it replaces, and no trailing newline
      // because a result is a string field rather than a line.
      const toonText = toolAnswer(toon).text;
      expect(toonText.startsWith("{")).toBe(false);
      expect(toonText.endsWith("\n")).toBe(false);
      const decoded = decode(toonText) as Record<string, unknown>;
      expect(decoded.status).toBe("bootstrapping");
      expect(toolAnswer(toon).isError).toBe(true);

      // `--json` on the server: the same shape and the same verdict, in the document a JSON client
      // parses. Each session owns a temp tree, so the summary's path is its own; the members and the
      // status are what two renderings of one state must agree on.
      const jsonText = toolAnswer(json).text;
      expect(jsonText.startsWith("{")).toBe(true);
      expect(jsonText.endsWith("\n")).toBe(false);
      const parsed = JSON.parse(jsonText) as Record<string, unknown>;
      expect(Object.keys(parsed).sort()).toEqual(Object.keys(decoded).sort());
      expect(parsed.status).toBe(decoded.status);
      expect(parsed.summary).toMatch(/^no index at .* yet$/);
      expect(toolAnswer(json).isError).toBe(true);
    },
    HARD_TIMEOUT_MS,
  );

  test(
    "tools/call index is reachable as a tool and answers in TOON too",
    async () => {
      // The seventh name is not a CLI tool command — `bin/sangkar.ts` owns `index` as the bootstrap
      // verb — so its envelope rendering is asserted where the tool is reachable. No index run
      // happens: with no published graph the store is absent, and that is the answer.
      const run = await session({ call: "index" });
      expect(run.nonJsonLines).toEqual([]);
      expect(run.exitCode).toBe(0);
      const text = toolAnswer(run).text;
      expect(text.startsWith("{")).toBe(false);
      expect(decode(text)).toMatchObject({ status: "bootstrapping" });
    },
    HARD_TIMEOUT_MS,
  );
});

describe("MCP surface: seven tools, one clean stdout", () => {
  test(
    "tools/list publishes exactly the seven names, and the manifests' schema is what is published",
    async () => {
      const run = await session();
      const names = toolNames(run);

      // The handshake, then the list. A third response would mean a notification was answered.
      expect(run.responses).toHaveLength(2);
      expect(run.responses.map((response) => response.id)).toEqual([1, 2]);
      expect(run.responses[0]?.error).toBeUndefined();

      const initialized = run.responses[0]?.result as
        | { protocolVersion?: string; serverInfo?: { name?: string } }
        | undefined;
      expect(initialized?.serverInfo?.name).toBe("sangkar");
      expect(initialized?.protocolVersion).toBe(PROTOCOL_VERSION);

      // One surface, seven names, each published once: `index` is registered like every other tool,
      // and the instructions rather than a registration filter are what keep it in reserve.
      expect(names).toHaveLength(7);
      expect(names).toEqual([...TOOL_NAMES].sort());
      expect(new Set(names).size).toBe(names.length);
      expect(names).toContain("index");

      // stdout is the protocol: one JSON object per line, nothing else, and the diagnostics on
      // stderr are all `#shared/log` records.
      expect(run.nonJsonLines).toEqual([]);
      expect(run.stdoutLines.filter((line) => line.trim() !== "")).toHaveLength(2);
      for (const line of run.stderrLines) {
        expect(() => JSON.parse(line)).not.toThrow();
      }

      // The exit path: stdin closed, the server stopped and returned 0 rather than being killed.
      expect(run.exitCode).toBe(0);

      // The one admin tool's arguments are both optional, spelled as JSON Schema: `enum` becomes a
      // string schema carrying its values, and the declared default travels with the property.
      const index = toolsOf(run).find((tool) => tool.name === "index");
      expect(index?.inputSchema.type).toBe("object");
      expect(index?.inputSchema.required).toBeUndefined();
      expect(index?.inputSchema.properties?.force).toMatchObject({
        type: "boolean",
        default: false,
      });
      expect(index?.inputSchema.properties?.mode).toMatchObject({
        type: "string",
        enum: ["full", "fast"],
        default: "full",
      });

      const trace = toolsOf(run).find((tool) => tool.name === "trace");
      expect(trace?.inputSchema.type).toBe("object");
      // `required` is `query` alone: at most one required argument per tool (TOOLS-SPEC.md §1).
      expect(trace?.inputSchema.required).toEqual(["query"]);
      expect(trace?.inputSchema.properties?.query).toMatchObject({
        type: "string",
        description: "The symbol to look up: a name, dotted name, path or `@decorator`.",
      });
      // `default`, `min` and `max` travel with the property, so a client can size its call.
      expect(trace?.inputSchema.properties?.depth).toMatchObject({
        type: "integer",
        minimum: 1,
        maximum: 4,
        default: 2,
        description: "How many hops of callers and callees to walk from the symbol.",
      });
      expect(trace?.inputSchema.properties?.direction).toMatchObject({
        type: "string",
        enum: ["all", "in", "out"],
        default: "all",
        description: "Which way to walk from the symbol: in, out or both.",
      });

      const status = toolsOf(run).find((tool) => tool.name === "status");
      expect(status?.inputSchema.properties?.paths).toMatchObject({
        type: "array",
        items: { type: "string" },
        default: [],
        description: "Paths whose per-path coverage and skip count this answer reports.",
      });
      expect(status?.inputSchema.required).toBeUndefined();

      // The declared item type travels with the array, so `expand.range` is published as the
      // integer pair the parser enforces rather than as the strings argv happens to carry.
      const expand = toolsOf(run).find((tool) => tool.name === "expand");
      expect(expand?.inputSchema.properties?.range).toMatchObject({
        type: "array",
        items: { type: "integer" },
      });
      expect(expand?.inputSchema.properties?.range?.items).toEqual({ type: "integer" });
      // An enum item is still a string item with values, never `type: "enum"`: that is not a
      // JSON Schema type, and `trace.kinds` is the field that declares one.
      expect(expand?.inputSchema.properties?.range?.items).not.toHaveProperty("enum");
      expect(trace?.inputSchema.properties?.kinds).toMatchObject({
        type: "array",
        items: {
          type: "string",
          enum: ["file", "module", "class", "function", "method", "test", "endpoint"],
        },
        description: "Narrow resolution to these node kinds when a name collides across kinds.",
      });

      // Each manifest's one pasteable call is published as `inputSchema.examples`, one entry per
      // tool: the example is a whole argument object, not a per-argument keyword.
      const examplesOf = (name: string): readonly Record<string, unknown>[] | undefined => {
        return toolsOf(run).find((tool) => tool.name === name)?.inputSchema.examples;
      };
      expect(examplesOf("status")).toEqual([{ paths: ["src"] }]);
      expect(examplesOf("summary")).toEqual([{ scope: "change" }]);
      expect(examplesOf("trace")).toEqual([{ query: "handleRequest", depth: 1 }]);
      expect(examplesOf("search")).toEqual([{ query: "publish", kind: "function" }]);
      expect(examplesOf("impact")).toEqual([{ depth: 2 }]);
      expect(examplesOf("expand")).toEqual([{ target: "src/indexer/resolve.ts", what: "outline" }]);
      expect(examplesOf("index")).toEqual([{ mode: "fast" }]);

      // Issue #8's acceptance criteria, stated over every published argument rather than over the
      // sample above: an argument explains itself — enum values or a description, never neither —
      // and every tool shows one pasteable call whose keys are arguments that tool declares. Both
      // expectations carry the tool name into the diff, because a bare `undefined` does not say
      // which of the published arguments is the defect; adding an argument without a description
      // has to fail here rather than reach a client that cannot size its call.
      for (const tool of toolsOf(run)) {
        const properties = Object.entries(tool.inputSchema.properties ?? {});
        const unexplained = properties
          .filter(([, property]) => (property.enum ?? property.description) === undefined)
          .map(([name]) => name);
        expect({ tool: tool.name, unexplained }).toEqual({ tool: tool.name, unexplained: [] });

        const examples = tool.inputSchema.examples;
        expect(examples).toBeArray();
        expect(examples).toHaveLength(1);
        expect(examples?.[0]).toBeObject();
        const undeclared = Object.keys(examples?.[0] ?? {}).filter((key) => {
          return !properties.some(([name]) => name === key);
        });
        expect({ tool: tool.name, undeclared }).toEqual({ tool: tool.name, undeclared: [] });
      }
    },
    HARD_TIMEOUT_MS,
  );
});
