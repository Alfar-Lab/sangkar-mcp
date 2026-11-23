import { Database } from "bun:sqlite";
import { expect, test } from "bun:test";
import { existsSync, statSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { runWorker } from "../src/indexer/worker";

/**
 * The packaging gate: the standalone artifact is built, spawned and driven over stdio, and it has to
 * answer a real `tools/call status` about an index the worker published — not merely complete
 * `initialize`. Everything up to that point can pass while the server is unable to read a graph, so
 * the assertion the task exists for is the generation name coming back out of the binary.
 *
 * Three properties are asserted by construction rather than by inspection:
 *
 * - **The migrations travel inside the executable.** The runner reads `.sql` files from
 *   `import.meta.dir/migrations`, which inside a compiled binary is the virtual `/$bunfs/root`. The
 *   build therefore passes `--asset <migrations>`, and the test asserts there is no `migrations/`
 *   directory on disk next to the binary — the SQL it applied came from the embedded copy.
 * - **No grammar is loaded at startup.** Grammars resolve through `node_modules` (`web-tree-sitter`,
 *   `tree-sitter-wasms`) and load on first parse. The binary is spawned from a temp directory with no
 *   `node_modules` anywhere above it, so a grammar load at startup would throw and the session would
 *   never answer; the status call arriving is the evidence that startup needed none.
 * - **stdout is the protocol.** Every non-blank stdout line is parsed as JSON, and the count is
 *   exactly the two responses: diagnostics are stderr records, all of them JSON.
 *
 * The build is guarded at 180 s per the task contract. When it trips, the test is registered as a
 * skip with the measured reason instead of failing, because the artifact being slow to compile is an
 * environment fact and the assertions above are still worth landing and running where it fits.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const MIGRATIONS_DIR = join(APP_ROOT, "src", "shared", "store", "migrations");

/** The contract's bound on the compile. A build that cannot finish inside it is not shipped. */
const COMPILE_TIMEOUT_MS = 180_000;

/** Generous for three handshakes, short enough that a wedged server is a failure. */
const HARD_TIMEOUT_MS = 60_000;

/** `#shared/cursor` refuses anything under 32 bytes. */
const CURSOR_KEY = "binary-smoke-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

const WIDGET = "src/widget.py";
const CALLER = "src/caller.py";

const WIDGET_SOURCE = `def render(title):
    return title
`;

const CALLER_SOURCE = `from widget import render


def run():
    return render("x")
`;

interface Build {
  /** The compile passed 180 s and was killed. */
  readonly timedOut: boolean;
  /** Exit 0 and the outfile exists. */
  readonly ok: boolean;
  readonly ms: number;
  readonly bytes: number;
  readonly binary: string;
  /** The temp dir holding the binary, its cwd, and the fixture; removed by the test. */
  readonly root: string;
  /** The build's own output, quoted in the failure message. */
  readonly detail: string;
}

/**
 * `bun build --compile` into a temp dir. `--asset` is not optional: without the embedded migrations
 * a compiled binary reports a shipped schema version of 0 and refuses every real index, which is the
 * failure this test would otherwise spend its assertions discovering.
 */
async function compile(): Promise<Build> {
  const root = await mkdtemp(join(tmpdir(), "sangkar-binary-smoke-"));
  const binary = join(root, "sangkar");
  const started = performance.now();
  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "build",
      "--compile",
      "bin/sangkar.ts",
      "--asset",
      MIGRATIONS_DIR,
      "--outfile",
      binary,
    ],
    cwd: APP_ROOT,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });

  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    proc.kill("SIGKILL");
  }, COMPILE_TIMEOUT_MS);
  try {
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    const bytes = existsSync(binary) ? statSync(binary).size : 0;
    return {
      timedOut,
      ok: exitCode === 0 && bytes > 0,
      ms: Math.round(performance.now() - started),
      bytes,
      binary,
      root,
      detail: `${stdout}\n${stderr}`.trim(),
    };
  } finally {
    clearTimeout(timer);
  }
}

const build = await compile();

interface Fixture {
  readonly repoRoot: string;
  /** The data dir the server picks by default: `<repo>/.sangkar`. */
  readonly dataDir: string;
  /** The child's machine home, so the config.json its first run creates stays in this fixture. */
  readonly home: string;
  /** What the worker published; the binary has to name it back. */
  readonly generation: string;
  readonly nodes: number;
}

/** Two Python files where the caller sorts before the file it calls, indexed by the real worker. */
async function fixture(root: string): Promise<Fixture> {
  const repoRoot = join(root, "repo");
  const home = join(root, "home");
  await mkdir(home, { recursive: true });
  await mkdir(join(repoRoot, "src"), { recursive: true });
  await writeFile(join(repoRoot, WIDGET), WIDGET_SOURCE, "utf8");
  await writeFile(join(repoRoot, CALLER), CALLER_SOURCE, "utf8");

  const dataDir = join(repoRoot, ".sangkar");
  const code = await runWorker({ repoRoot, dataDir, force: false, mode: "full" });
  if (code !== 0) {
    throw new Error(`the worker exited ${code} over ${repoRoot}`);
  }

  const handle = new Database(join(dataDir, "graph.db"), { readonly: true });
  try {
    const row = handle
      .query<{ generation: string | null; nodes: number | null }, []>(
        "SELECT (SELECT value FROM meta WHERE key = 'generation') AS generation," +
          " (SELECT nodes FROM generation ORDER BY seq DESC LIMIT 1) AS nodes",
      )
      .get();
    if (row?.generation === null || row === null) {
      throw new Error("the worker published no generation");
    }
    return { repoRoot, dataDir, home, generation: row.generation, nodes: row.nodes ?? 0 };
  } finally {
    handle.close();
  }
}

interface JsonRpcError {
  readonly code: number;
  readonly message: string;
}

interface JsonRpcResponse {
  readonly id?: number | string;
  readonly result?: unknown;
  readonly error?: JsonRpcError;
}

interface Session {
  /** Messages that carried an id: one per request, in arrival order. */
  readonly responses: readonly JsonRpcResponse[];
  /** Every non-blank stdout line, parsed or not. */
  readonly stdoutLines: readonly string[];
  /** The stdout lines that are not JSON at all: the protocol-corruption count. */
  readonly nonJsonLines: readonly string[];
  /** Every non-blank stderr line, which must all be `#shared/log` records. */
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
 * One session over the binary's stdio: `initialize`, the initialized notification, then `status`.
 * The child runs from the temp dir that holds it — no `node_modules`, no `migrations/` — and the
 * tree and data dir it serves are argv flags, while the two settings that must not be inherited
 * from a harness are stated, so nothing outside this test decides what it proves.
 */
async function session(fixture: Fixture): Promise<Session> {
  const proc = Bun.spawn({
    cmd: [build.binary, "mcp", "--repo-root", fixture.repoRoot, "--data-dir", fixture.dataDir],
    cwd: build.root,
    env: {
      ...process.env,
      // The first run writes `<home>/config.json`; without this the binary would write it into the
      // developer's real config directory instead of the fixture.
      SANGKAR_HOME: fixture.home,
      SANGKAR_CURSOR_KEY: CURSOR_KEY,
      SANGKAR_NO_WATCH: "1",
    },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const requests = [
    {
      jsonrpc: "2.0",
      id: 1,
      method: "initialize",
      params: {
        protocolVersion: PROTOCOL_VERSION,
        capabilities: {},
        clientInfo: { name: "binary-smoke-test", version: "0.0.0" },
      },
    },
    { jsonrpc: "2.0", method: "notifications/initialized" },
    { jsonrpc: "2.0", id: 2, method: "tools/call", params: { name: "status", arguments: {} } },
  ];

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
  }
}

/** The `status` envelope, unwrapped from the MCP `content[0].text` JSON string. */
interface StatusEnvelope {
  readonly status: string;
  readonly read_status: {
    readonly generation: string;
    readonly indexed_at: string;
    readonly stale: boolean;
  };
  readonly coverage: {
    readonly complete: boolean;
    readonly indexed: number;
  };
  readonly payload: {
    readonly graph: { readonly nodes: number; readonly edges: number };
    readonly project: { readonly db_path: string };
    readonly languages?: readonly {
      readonly language: string;
      readonly files: number;
      readonly parsed: number;
    }[];
  } | null;
}

function statusOf(response: JsonRpcResponse | undefined): {
  envelope: StatusEnvelope;
  isError: boolean;
} {
  const result = response?.result as
    | { content?: readonly { type?: string; text?: string }[]; isError?: boolean }
    | undefined;
  const text = result?.content?.[0]?.text;
  if (typeof text !== "string") {
    throw new Error(`tools/call returned no text content: ${JSON.stringify(response)}`);
  }
  return { envelope: JSON.parse(text) as StatusEnvelope, isError: result?.isError === true };
}

if (build.timedOut) {
  test.skip(`binary smoke: skipped, bun build --compile exceeded ${COMPILE_TIMEOUT_MS / 1_000} s`, () => {});
} else {
  test(
    "the compiled binary answers tools/call status with the published generation",
    async () => {
      // A compile that failed for any other reason is a failure, with the build's own output.
      expect({
        ok: build.ok,
        detail: build.ok ? "" : build.detail,
      }).toEqual({ ok: true, detail: "" });

      const fix = await fixture(build.root);
      try {
        // The data dir the worker wrote is the one the binary reads: a real graph, no fixture stub.
        expect(await readFile(join(fix.dataDir, ".gitignore"), "utf8")).toBe("*\n");
        expect(existsSync(join(fix.dataDir, "graph.db"))).toBe(true);

        const run = await session(fix);
        const measured = {
          ok: run.nonJsonLines.length === 0,
          buildMs: build.ms,
          binaryBytes: build.bytes,
          generation: fix.generation,
          nodes: fix.nodes,
          responses: run.responses.length,
          exitCode: run.exitCode,
        };
        // One stderr JSON line, so the report quotes measured numbers rather than a passing assertion.
        process.stderr.write(
          `${JSON.stringify({ ts: new Date().toISOString(), level: "info", msg: "binary_smoke.measured", ...measured })}\n`,
        );

        // The handshake, then the tool call; a third response would mean a notification was answered.
        expect(run.responses.map((response) => response.id)).toEqual([1, 2]);
        expect(run.responses[0]?.error).toBeUndefined();
        const initialized = run.responses[0]?.result as
          | { protocolVersion?: string; serverInfo?: { name?: string } }
          | undefined;
        expect(initialized?.serverInfo?.name).toBe("sangkar");
        expect(initialized?.protocolVersion).toBe(PROTOCOL_VERSION);

        // stdout is the protocol: two JSON objects, nothing else.
        expect(run.nonJsonLines).toEqual([]);
        expect(run.stdoutLines).toHaveLength(2);

        const { envelope, isError } = statusOf(run.responses[1]);
        expect(isError).toBe(false);
        expect(envelope.status).toBe("ok");
        expect(envelope.read_status.generation).toBe(fix.generation);
        expect(envelope.read_status.indexed_at.length).toBeGreaterThan(0);
        expect(envelope.coverage.complete).toBe(true);
        expect(envelope.coverage.indexed).toBe(2);

        // The payload is the graph the worker published, read through the embedded migrations.
        expect(envelope.payload?.project.db_path).toBe(join(fix.dataDir, "graph.db"));
        expect(envelope.payload?.graph.nodes).toBe(fix.nodes);
        expect(envelope.payload?.graph.nodes).toBeGreaterThan(0);
        // The census is the worker's, not a re-parse: both files were answered from the database.
        // Only the keys this test owns are asserted, so a field added to the row cannot fail it.
        const python = envelope.payload?.languages?.find((row) => row.language === "python");
        expect(python?.files).toBe(2);
        expect(python?.parsed).toBe(2);

        // Diagnostics are `#shared/log` records, and the store came up ready — not refused.
        const records = run.stderrLines.map((line) => JSON.parse(line) as { msg?: string });
        expect(records.filter((record) => record.msg === undefined)).toEqual([]);
        expect(records.map((record) => record.msg)).toContain("mcp.ready");
        expect(run.stderrLines.join("\n")).not.toMatch(/grammar|schema_too_new/i);

        // No grammar, no migrations on disk: both came from the executable or were never touched.
        expect(existsSync(join(build.root, "node_modules"))).toBe(false);
        expect(existsSync(join(build.root, "migrations"))).toBe(false);

        // Closing stdin is the client leaving: the server stops itself rather than being killed.
        expect(run.exitCode).toBe(0);
      } finally {
        await rm(build.root, { recursive: true, force: true });
      }
    },
    HARD_TIMEOUT_MS + 30_000,
  );
}
