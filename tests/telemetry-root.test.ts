import { afterAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { appendFile, mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

/**
 * The telemetry wiring, proven through the shipped executable: every record here was written by a
 * real process — `bin/sangkar.ts`, spawned with `process.execPath` — over a real temp tree and its
 * own data dir. Nothing imports `runWorker`, a handler or the server's registry, because the point
 * of the file is the path a user runs: the entry, the argv grammar, the composition root's
 * recorder, the worker's single exit path, and the sink that puts the line on disk.
 *
 * Five properties justify the shape:
 *
 * - **Records are read from `<data>/telemetry/*.jsonl`, every line parsed.** The daily file is
 *   append-only and two processes may write the same one, so a read tolerates several files and
 *   groups nothing: each line stands alone.
 * - **No setting is inherited.** The spawn helper deletes every `SANGKAR_*` key first and then sets
 *   only what a case needs, so nothing passes because this machine happened to export it.
 * - **A defaulted name is not a supplied name.** `parseArgs` fills every declared default, so
 *   `arg_names_set` has to come from the argv reader and the request's own keys — `status` records
 *   `[]`, and `trace --depth 1` records `["depth","query"]` while `budget_tokens` still reports the
 *   defaulted budget in force.
 * - **Privacy is asserted against the file, not the builder.** The temp repo path appears in no
 *   line, and a path-shaped tool name a client invents is recorded as `"unknown"` and never stored.
 * - **A successful process is not an error.** The no-graph `status` line exits 0 and records
 *   `absent`: folding it into `error` would label the commonest CLI invocation a failure.
 *
 * Every spawn gets a 120 s budget (a cold index loads tree-sitter grammars and writes a real
 * database); a wedge fails as a timeout instead of hanging the suite.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** Generous for a cold index and three handshakes; short enough that a wedge is a failure. */
const RUN_TIMEOUT_MS = 120_000;

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "telemetry-root-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

/**
 * The path-shaped "tool name" a client has no business sending. It is not an argument and must not
 * survive into a line, so it is a literal rather than a temp path: the assertion then cannot pass
 * because two temp paths happened to differ.
 */
const SECRET_TOOL_NAME = "/tmp/sangkar-telemetry-fixture/repo/secret.ts";

/** Three small files: one declaration, one caller, one class wrapping the call. */
const SOURCES: Readonly<Record<string, string>> = {
  "src/a.ts": `export function handleRequest(): number {
  return 1;
}
`,
  "src/b.ts": `import { handleRequest } from "./a";

export function caller(): number {
  return handleRequest();
}
`,
  "src/c.ts": `export class Service {
  run(): number {
    return 1;
  }
}
`,
};

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

interface Tree {
  readonly root: string;
  readonly repo: string;
  readonly data: string;
}

interface SessionRecord {
  readonly ts: string;
  readonly v: number;
  readonly kind: "session";
  readonly install_id: string;
  readonly session_id: string;
  readonly session_mode: string;
}

interface IndexRunRecord {
  readonly ts: string;
  readonly v: number;
  readonly kind: "index_run";
  readonly install_id: string;
  readonly session_id: string;
  readonly mode: string;
  readonly exit_code: number;
  readonly was_forced: boolean;
  readonly generation_advanced: boolean;
  readonly files_seen: number;
  readonly files_indexed: number;
  readonly files_changed_in_run: number;
  readonly languages: Readonly<Record<string, number>>;
  readonly db_size_bytes: number;
}

interface CallRecord {
  readonly ts: string;
  readonly v: number;
  readonly kind: "call";
  readonly install_id: string;
  readonly session_id: string;
  readonly seq: number;
  readonly tool: string;
  readonly status: string;
  readonly arg_names_set: readonly string[];
  readonly budget_tokens: number;
  readonly payload_tokens: number;
  readonly truncated: boolean;
}

/**
 * The fields these cases assert, hand-written on purpose: a type imported from
 * `#shared/telemetry/record` would follow the source if a field were renamed, while a reader of the
 * file has to see the field names the report promised.
 */
type TelemetryRecord = SessionRecord | IndexRunRecord | CallRecord;

interface Telemetry {
  /** Every entry in the directory, so the daily-file shape is asserted against the real listing. */
  readonly files: readonly string[];
  readonly text: string;
  readonly records: readonly TelemetryRecord[];
}

/** The repository's own settings, removed: nothing can pass because of an inherited variable. */
function cleanEnv(): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  return env;
}

/** Temp roots are removed in `afterAll`, so a failed assertion cannot leave a directory behind. */
const roots: string[] = [];

/** One fixture tree per case, each with its own data dir. */
async function makeTree(label: string): Promise<Tree> {
  const root = await mkdtemp(join(tmpdir(), `sangkar-telemetry-${label}-`));
  roots.push(root);
  const repo = join(root, "repo");
  const data = join(root, "data");
  await mkdir(data, { recursive: true });
  for (const [path, content] of Object.entries(SOURCES)) {
    const file = join(repo, path);
    await mkdir(dirname(file), { recursive: true });
    await writeFile(file, content);
  }
  return { root, repo, data };
}

/** A wrapper is quiet by construction: this file never needs the watcher, so it never runs one. */
function childEnv(extra: Record<string, string>): Record<string, string> {
  return { ...cleanEnv(), SANGKAR_NO_WATCH: "1", ...extra };
}

function collect(proc: ReturnType<typeof Bun.spawn>): Promise<Run> {
  return (async () => {
    const [stdout, stderr, code] = await Promise.all([
      new Response(proc.stdout as ReadableStream<Uint8Array>).text(),
      new Response(proc.stderr as ReadableStream<Uint8Array>).text(),
      proc.exited,
    ]);
    return { code, stdout, stderr };
  })();
}

async function runCli(args: readonly string[], extra: Record<string, string> = {}): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd: APP_ROOT,
    env: childEnv(extra),
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const timer = setTimeout(() => proc.kill("SIGKILL"), RUN_TIMEOUT_MS);
  try {
    return await collect(proc);
  } finally {
    clearTimeout(timer);
    proc.kill();
  }
}

/**
 * One MCP session over stdio, driven the way a client drives it: hand-written frames, then stdin
 * closed. Closing stdin is the documented hang-up (`runMcp`'s `clientGone`), and the records are
 * only read after `proc.exited` has resolved, because the server flushes on that path; the timeout
 * is a backstop that gives it the documented SIGTERM shutdown before anything is killed outright.
 */
async function runMcp(tree: Tree, frames: readonly unknown[]): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "mcp",
      "--repo-root",
      tree.repo,
      "--data-dir",
      tree.data,
    ],
    cwd: APP_ROOT,
    env: childEnv({ SANGKAR_CURSOR_KEY: CURSOR_KEY }),
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });
  const timer = setTimeout(() => {
    proc.kill("SIGTERM");
    setTimeout(() => proc.kill("SIGKILL"), 5_000);
  }, RUN_TIMEOUT_MS);
  try {
    proc.stdin.write(frames.map((frame) => `${JSON.stringify(frame)}\n`).join(""));
    await proc.stdin.flush();
    proc.stdin.end();
    return await collect(proc);
  } finally {
    clearTimeout(timer);
    proc.kill();
  }
}

/** A failure that carries the child's stderr, because "exited 1" alone never explains itself. */
function expectExit(run: Run, code: number, what: string): void {
  if (run.code !== code) {
    throw new Error(`${what} exited ${run.code}, expected ${code}. stderr:\n${run.stderr}`);
  }
  expect(run.code).toBe(code);
}

/**
 * Every `*.jsonl` line under `<data>/telemetry`, parsed. A torn or unparsable line throws, which is
 * the point: an append that lands inside a line is exactly the corruption this reads for.
 */
async function readTelemetry(dataDir: string): Promise<Telemetry> {
  const dir = join(dataDir, "telemetry");
  const files = (await readdir(dir)).sort();
  const chunks: string[] = [];
  for (const name of files) {
    if (name.endsWith(".jsonl")) {
      chunks.push(await readFile(join(dir, name), "utf8"));
    }
  }
  const text = chunks.join("");
  const records: TelemetryRecord[] = [];
  for (const line of text.split("\n")) {
    if (line.trim() === "") {
      continue;
    }
    records.push(JSON.parse(line) as TelemetryRecord);
  }
  return { files, text, records };
}

function ofKind<K extends TelemetryRecord["kind"]>(
  records: readonly TelemetryRecord[],
  kind: K,
): Extract<TelemetryRecord, { kind: K }>[] {
  return records.filter((record): record is Extract<TelemetryRecord, { kind: K }> => {
    return record.kind === kind;
  });
}

/** Exactly one, or a failure that names what was expected and how many arrived. */
function one<T>(items: readonly T[], what: string): T {
  if (items.length !== 1) {
    throw new Error(`expected exactly one ${what}, found ${items.length}`);
  }
  const only = items[0];
  if (only === undefined) {
    throw new Error(`expected exactly one ${what}, found none`);
  }
  return only;
}

function chronological<T extends { readonly ts: string }>(records: readonly T[]): T[] {
  return [...records].sort((left, right) => Date.parse(left.ts) - Date.parse(right.ts));
}

const ID_PATTERN = /^[0-9a-f]{32}$/;
const DAILY_FILE_PATTERN = /^\d{4}-\d{2}-\d{2}\.jsonl$/;

interface CaseA {
  readonly tree: Tree;
  readonly index: Run;
  readonly status: Run;
  readonly trace: Run;
  readonly telemetry: Telemetry;
}

let caseARun: Promise<CaseA> | null = null;

/**
 * Case A's tree, built once and shared by the cases that read its records (A itself, A2 and the
 * privacy check), so the suite pays for one index, one `status` and one `trace`.
 *
 * The index command is the bare shipped one: no `--mode`, no `--force`. A first build resolves
 * itself to `full` and reports `was_forced`, because the store had never published a generation and
 * there is nothing to diff against (`hasPublishedGeneration` in `src/indexer/worker.ts`). Asking for
 * `--mode full` would have proven the caller's argument instead of the worker's own resolution.
 */
function caseA(): Promise<CaseA> {
  caseARun ??= (async () => {
    const tree = await makeTree("indexed");
    const index = await runCli(["index", "--repo-root", tree.repo, "--data-dir", tree.data]);
    const status = await runCli(["status", "--repo-root", tree.repo, "--data-dir", tree.data]);
    const trace = await runCli([
      "trace",
      "handleRequest",
      "--depth",
      "1",
      "--repo-root",
      tree.repo,
      "--data-dir",
      tree.data,
    ]);
    return { tree, index, status, trace, telemetry: await readTelemetry(tree.data) };
  })();
  return caseARun;
}

afterAll(async () => {
  await Promise.all(roots.map((root) => rm(root, { recursive: true, force: true })));
});

describe("telemetry through the shipped command", () => {
  test(
    "case A: an indexed tree records one full index_run, a worker session and a status call",
    async () => {
      const run = await caseA();
      expectExit(run.index, 0, "sangkar index");
      expectExit(run.status, 0, "sangkar status");

      const indexRun = one(ofKind(run.telemetry.records, "index_run"), "index_run record");
      expect({
        mode: indexRun.mode,
        exitCode: indexRun.exit_code,
        generationAdvanced: indexRun.generation_advanced,
        wasForced: indexRun.was_forced,
      }).toEqual({
        mode: "full",
        exitCode: 0,
        generationAdvanced: true,
        // A first build is forced: nothing was published to diff against.
        wasForced: true,
      });
      expect(indexRun.files_seen).toBeGreaterThanOrEqual(1);
      expect(indexRun.files_indexed).toBeGreaterThanOrEqual(1);
      expect(Object.keys(indexRun.languages).length).toBeGreaterThanOrEqual(1);
      expect(indexRun.db_size_bytes).toBeGreaterThan(0);

      const sessions = ofKind(run.telemetry.records, "session");
      const modes = sessions.map((session) => session.session_mode);
      expect(modes.filter((mode) => mode === "worker").length).toBeGreaterThanOrEqual(1);
      expect(modes.filter((mode) => mode === "cli").length).toBeGreaterThanOrEqual(1);

      const calls = ofKind(run.telemetry.records, "call");
      const statusCall = one(
        calls.filter((call) => call.tool === "status"),
        'call record with tool "status"',
      );
      expect({
        tool: statusCall.tool,
        status: statusCall.status,
        argNames: statusCall.arg_names_set,
        seq: statusCall.seq,
      }).toEqual({ tool: "status", status: "ok", argNames: [], seq: 1 });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case A: every record is v1 with a 32-hex identity, and a call shares its process's session",
    async () => {
      const run = await caseA();
      const telemetry = run.telemetry;

      const dailyFiles = telemetry.files.filter((name) => name.endsWith(".jsonl"));
      expect(dailyFiles.length).toBeGreaterThanOrEqual(1);
      for (const name of dailyFiles) {
        expect(name).toMatch(DAILY_FILE_PATTERN);
      }

      expect(telemetry.records.length).toBeGreaterThanOrEqual(3);
      for (const record of telemetry.records) {
        expect(record.v).toBe(1);
        expect(record.install_id).toMatch(ID_PATTERN);
        expect(record.session_id).toMatch(ID_PATTERN);
      }

      const cliSessions = ofKind(telemetry.records, "session").filter((session) => {
        return session.session_mode === "cli";
      });
      expect(cliSessions.length).toBeGreaterThanOrEqual(1);
      for (const call of ofKind(telemetry.records, "call")) {
        const owners = cliSessions.filter((session) => session.session_id === call.session_id);
        const owner = one(owners, `session record sharing session_id with the ${call.tool} call`);
        expect(Date.parse(owner.ts)).toBeLessThanOrEqual(Date.parse(call.ts));
      }
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case A2: supplied names are explicit-only, and the defaulted budget is still reported",
    async () => {
      const run = await caseA();
      expectExit(run.trace, 0, "sangkar trace");

      const traceCall = one(
        ofKind(run.telemetry.records, "call").filter((call) => call.tool === "trace"),
        'call record with tool "trace"',
      );
      // The bare token `handleRequest` is positional and lands in `query`: `positionalFieldOf`
      // (`src/shared/cli/args.ts:132-137`) names the one required string field, and the positional
      // branch of `readArgv` binds the token to it. `--depth 1` is a flag, so the set is those two.
      // `cursor` and `budget` are declared by the manifest and absent here: a default is not a
      // supplied name. The budget in force, though, is the defaulted 3200.
      expect(traceCall.arg_names_set).toEqual(["depth", "query"]);
      expect(traceCall.arg_names_set).not.toContain("cursor");
      expect(traceCall.arg_names_set).not.toContain("budget");
      expect(traceCall.budget_tokens).toBe(3200);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case A3: a bare first index is full and forced, and a bare index after one edit is fast",
    async () => {
      const tree = await makeTree("default-mode");
      // Both runs are the bare shipped command: the worker's own resolution, not the caller's choice.
      const first = await runCli(["index", "--repo-root", tree.repo, "--data-dir", tree.data]);
      expectExit(first, 0, "the first sangkar index");
      await appendFile(
        join(tree.repo, "src", "a.ts"),
        "// the tree changed between the two runs\n",
      );

      const second = await runCli(["index", "--repo-root", tree.repo, "--data-dir", tree.data]);
      expectExit(second, 0, "the second sangkar index");

      const runs = chronological(
        ofKind(await readTelemetry(tree.data).then((t) => t.records), "index_run"),
      );
      expect(runs.map((record) => record.mode)).toEqual(["full", "fast"]);
      const firstRun = runs[0];
      const fastRun = runs[1];
      if (firstRun === undefined || fastRun === undefined) {
        throw new Error("the two index runs did not write two records");
      }
      // A store that never published is a first build, so the run is forced and full even though
      // no `--mode` and no `--force` were passed.
      expect({ mode: firstRun.mode, wasForced: firstRun.was_forced }).toEqual({
        mode: "full",
        wasForced: true,
      });
      expect(firstRun.exit_code).toBe(0);
      expect({ mode: fastRun.mode, wasForced: fastRun.was_forced }).toEqual({
        mode: "fast",
        wasForced: false,
      });
      expect(fastRun.files_changed_in_run).toBeGreaterThanOrEqual(1);
      expect(fastRun.exit_code).toBe(0);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case B: SANGKAR_TELEMETRY=off leaves no telemetry directory behind",
    async () => {
      const tree = await makeTree("off");
      const run = await runCli(["status", "--repo-root", tree.repo, "--data-dir", tree.data], {
        SANGKAR_TELEMETRY: "off",
      });
      expectExit(run, 0, "sangkar status with SANGKAR_TELEMETRY=off");
      expect(existsSync(join(tree.data, "telemetry"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case C: the temp repo path never reaches a record",
    async () => {
      const run = await caseA();
      expect(run.telemetry.text.length).toBeGreaterThan(0);
      expect(run.telemetry.text).not.toContain(run.tree.repo);
      expect(run.telemetry.text).not.toContain(join(run.tree.repo, "src", "a.ts"));
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "case D: a client-supplied tool name is recorded as unknown and does not survive",
    async () => {
      const tree = await makeTree("mcp-name");
      const session = await runMcp(tree, [
        {
          jsonrpc: "2.0",
          id: 1,
          method: "initialize",
          params: {
            protocolVersion: PROTOCOL_VERSION,
            capabilities: {},
            clientInfo: { name: "telemetry-root-test", version: "0.0.0" },
          },
        },
        { jsonrpc: "2.0", method: "notifications/initialized" },
        {
          jsonrpc: "2.0",
          id: 2,
          method: "tools/call",
          params: { name: SECRET_TOOL_NAME, arguments: {} },
        },
      ]);
      expectExit(session, 0, "sangkar mcp");

      const telemetry = await readTelemetry(tree.data);
      const call = one(ofKind(telemetry.records, "call"), "call record");
      expect(call.tool).toBe("unknown");
      expect(ofKind(telemetry.records, "session").map((record) => record.session_mode)).toContain(
        "mcp",
      );
      // The name was a string the client invented. It is not an argument, and it must not be kept as
      // a tool name either: no line may carry it, the path-shaped tail, or either temp root.
      expect(telemetry.text).not.toContain("secret.ts");
      expect(telemetry.text).not.toContain(SECRET_TOOL_NAME);
      expect(telemetry.text).not.toContain(tree.repo);
      expect(telemetry.text).not.toContain(tree.data);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    'case E: a successful no-graph status is "absent", not "error"',
    async () => {
      const tree = await makeTree("absent");
      const run = await runCli(["status", "--repo-root", tree.repo, "--data-dir", tree.data]);
      expectExit(run, 0, "sangkar status with no graph");
      // The human no-graph line, not an envelope: no `{` document was built, which is why no bytes
      // of payload can have been measured.
      expect(run.stdout.trim()).not.toBe("");
      expect(run.stdout.trimStart().startsWith("{")).toBe(false);

      const call = one(
        ofKind(await readTelemetry(tree.data).then((t) => t.records), "call"),
        "call record",
      );
      expect({
        tool: call.tool,
        status: call.status,
        payloadTokens: call.payload_tokens,
        truncated: call.truncated,
      }).toEqual({ tool: "status", status: "absent", payloadTokens: 0, truncated: false });
    },
    RUN_TIMEOUT_MS,
  );
});
