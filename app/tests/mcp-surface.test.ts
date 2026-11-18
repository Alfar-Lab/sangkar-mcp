import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { AGENT_TOOL_NAMES, TOOL_NAMES } from "#shared/instructions";

/**
 * The composition root's protocol surface (`TECH-SPEC.md` §11), checked by speaking to a real server
 * process instead of importing its registry: the point of this test is that `initialize` and
 * `tools/list` survive the SDK's own framing, that the profile from `CODEGRAPH_PROFILE` decides
 * which names are published, and that nothing but JSON-RPC reaches stdout.
 *
 * Every run gets its own tree and data dir, `CODEGRAPH_NO_WATCH=1` (a watcher would spawn a worker
 * over an empty fixture and make the run's timing depend on git) and a cursor key of the required
 * length. No database is published, deliberately: `tools/list` must answer before the first index,
 * because that is the state a client connects in.
 *
 * The server is driven the way a client drives it — two requests and one notification, then stdin
 * closed — and the run is bounded by a hard timeout so a wedged server fails the test instead of
 * hanging the suite.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "codegraph.ts");

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
 */
async function session(profile: "" | "admin"): Promise<Session> {
  const root = await mkdtemp(join(tmpdir(), "codegraph-mcp-surface-"));
  const repoRoot = join(root, "repo");
  const dataDir = join(root, "data");
  await mkdir(repoRoot, { recursive: true });
  await mkdir(dataDir, { recursive: true });

  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, "mcp"],
    cwd: APP_ROOT,
    env: {
      ...process.env,
      CODEGRAPH_REPO_ROOT: repoRoot,
      CODEGRAPH_DATA_DIR: dataDir,
      CODEGRAPH_CURSOR_KEY: CURSOR_KEY,
      // Every setting is stated, not inherited: a harness with CODEGRAPH_PROFILE set must not
      // decide what this test proves.
      CODEGRAPH_PROFILE: profile,
      CODEGRAPH_NO_WATCH: "1",
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const timer = setTimeout(() => proc.kill("SIGKILL"), HARD_TIMEOUT_MS);
  try {
    proc.stdin.write(REQUESTS.map((request) => `${JSON.stringify(request)}\n`).join(""));
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

describe("MCP surface: seven tools, two profiles, one clean stdout", () => {
  test(
    "the agent profile publishes six tools over two JSON-RPC responses",
    async () => {
      const run = await session("");
      const names = toolNames(run);

      // The handshake, then the list. A third response would mean a notification was answered.
      expect(run.responses).toHaveLength(2);
      expect(run.responses.map((response) => response.id)).toEqual([1, 2]);
      expect(run.responses[0]?.error).toBeUndefined();

      const initialized = run.responses[0]?.result as
        | { protocolVersion?: string; serverInfo?: { name?: string } }
        | undefined;
      expect(initialized?.serverInfo?.name).toBe("codegraph");
      expect(initialized?.protocolVersion).toBe(PROTOCOL_VERSION);

      expect(names).toHaveLength(AGENT_TOOL_NAMES.length);
      expect(names).toEqual([...AGENT_TOOL_NAMES].sort());
      expect(names).not.toContain("index");

      // stdout is the protocol: one JSON object per line, nothing else, and the diagnostics on
      // stderr are all `#shared/log` records.
      expect(run.nonJsonLines).toEqual([]);
      expect(run.stdoutLines.filter((line) => line.trim() !== "")).toHaveLength(2);
      for (const line of run.stderrLines) {
        expect(() => JSON.parse(line)).not.toThrow();
      }

      // The exit path: stdin closed, the server stopped and returned 0 rather than being killed.
      expect(run.exitCode).toBe(0);
    },
    HARD_TIMEOUT_MS,
  );

  test(
    "the admin profile adds `index`, and the manifests' schema is what is published",
    async () => {
      const run = await session("admin");
      const names = toolNames(run);

      expect(run.nonJsonLines).toEqual([]);
      expect(names).toHaveLength(TOOL_NAMES.length);
      expect(names).toEqual([...TOOL_NAMES].sort());
      expect(names).toContain("index");

      const trace = toolsOf(run).find((tool) => tool.name === "trace");
      expect(trace?.inputSchema.type).toBe("object");
      // `required` is `query` alone: at most one required argument per tool (TOOLS-SPEC.md §1).
      expect(trace?.inputSchema.required).toEqual(["query"]);
      expect(trace?.inputSchema.properties?.query).toEqual({ type: "string" });
      // `default`, `min` and `max` travel with the property, so a client can size its call.
      expect(trace?.inputSchema.properties?.depth).toEqual({
        type: "integer",
        minimum: 1,
        maximum: 4,
        default: 2,
      });
      expect(trace?.inputSchema.properties?.direction).toEqual({
        type: "string",
        enum: ["all", "in", "out"],
        default: "all",
      });

      const status = toolsOf(run).find((tool) => tool.name === "status");
      expect(status?.inputSchema.properties?.paths).toEqual({
        type: "array",
        items: { type: "string" },
        default: [],
      });
      expect(status?.inputSchema.required).toBeUndefined();
    },
    HARD_TIMEOUT_MS,
  );
});
