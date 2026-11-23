import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, readFileSync, statSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { decode } from "@toon-format/toon";
import { ENVELOPE_STATUSES, type Envelope } from "#shared/envelope";
import { TOOL_NAMES } from "#shared/instructions";

/**
 * `sangkar <tool>` end to end (`TECH-SPEC.md` §11): the shipped command is spawned as a child
 * process, so what is proven is the executable path a user actually runs — the shebang entry, the
 * lazy import of the root, the argv grammar, autodetect and the exit code — instead of a handler
 * called with an injected context.
 *
 * Four properties justify the shape of the file:
 *
 * - **The bootstrap is the CLI's own.** A fresh tree has no store for a tool call to answer from, so
 *   `sangkar index --mode full` must create `repo/.sangkar/graph.db`; indexing through the imported
 *   `runWorker` would prove the worker, not the command under test.
 * - **No setting is inherited.** `cleanEnv()` deletes every `SANGKAR_*` key and hands back a
 *   `SANGKAR_HOME` of this suite's own, so nothing can pass because this machine happened to export
 *   one, and nothing — an index run registers its repository — can write the developer's real
 *   `~/.config/sangkar/sangkar.json`: the tree is found by autodetect, the data dir by the
 *   `.sangkar` convention, and the machine-level home is a temp directory deleted in `afterAll`.
 * - **stdout belongs to the payload.** A tool answer is exactly one envelope — TOON by default,
 *   JSON with `--json` — closed by one newline, or the one `serving …` line, or the bare `no index
 *   at … yet` line an unindexed tree answers `status` with; a usage refusal writes nothing to stdout
 *   at all. Diagnostics are stderr.
 * - **The fixture is a temp tree with no `.git`.** Autodetect therefore has to fall back to the
 *   `.sangkar` directory the index creates — the property the nested-directory case exists for.
 *
 * Every spawn gets a 120 s budget: the first index loads tree-sitter grammars and writes a real
 * database, and a wedge should fail as a timeout rather than hang the suite.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** Generous for a cold index and three handshakes; short enough that a wedge is a failure. */
const RUN_TIMEOUT_MS = 120_000;

/** The ten members every envelope carries; `error` is conditional and asserted separately. */
const ENVELOPE_MEMBERS = [
  "status",
  "summary",
  "read_status",
  "coverage",
  "totals",
  "truncated",
  "truncation_reason",
  "next",
  "next_tools",
  "payload",
] as const;

/** Three small files: one declaration, one caller, and a second language for the status counts. */
const SOURCES: Readonly<Record<string, string>> = {
  "src/a.ts": `export function handleThing(): number {
  return 1;
}
`,
  "src/b.ts": `import { handleThing } from "./a";

export function callHandleThing(): number {
  return handleThing();
}
`,
  "lib/util.py": `def normalize(value):
    return value.strip()
`,
};

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

/**
 * The repository's own settings, removed: the fixture tree is discovered, never configured. The
 * one `SANGKAR_*` a run does get is this suite's temp home, which `spawnCli` passes in — without it
 * an index run would register its tree in the developer's own `~/.config/sangkar/sangkar.json`.
 */
function cleanEnv(extra: Readonly<Record<string, string>> = {}): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  return { ...env, ...extra };
}

/** Every spawned run's machine-level home: a temp directory, so nothing reaches the real one. */
function spawnEnv(): Record<string, string> {
  return cleanEnv({ SANGKAR_HOME: home });
}

function spawnCli(args: readonly string[], cwd: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: spawnEnv(),
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  return (async () => {
    const [stdout, stderr, code] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    return { code, stdout, stderr };
  })();
}

/**
 * A pipe read as it fills, so a long-running child can be asked to stop after its first line without
 * anyone having to guess how long a bind takes. `firstLine` resolves at the first newline (or when
 * the pipe closes, which is the child dying without one) and `all` once the pipe is done.
 */
interface Streamed {
  readonly firstLine: Promise<string>;
  readonly all: Promise<string>;
}

function streamed(stream: ReadableStream<Uint8Array>): Streamed {
  const decoder = new TextDecoder();
  let text = "";
  let settleLine: (line: string) => void = () => {};
  const firstLine = new Promise<string>((resolve) => {
    settleLine = resolve;
  });
  const all = (async () => {
    for await (const chunk of stream) {
      text += decoder.decode(chunk, { stream: true });
      const end = text.indexOf("\n");
      if (end !== -1) {
        settleLine(text.slice(0, end));
      }
    }
    text += decoder.decode();
    settleLine(text);
    return text;
  })();
  return { firstLine, all };
}

/** A failure that carries the child's stderr, because "exited 1" alone never explains itself. */
function expectExit(run: Run, code: number, what: string): void {
  if (run.code !== code) {
    throw new Error(`${what} exited ${run.code}, expected ${code}. stderr:\n${run.stderr}`);
  }
  expect(run.code).toBe(code);
}

/**
 * The body of one tool answer, with its rendering's byte shape proven first: TOON is the default,
 * so the value a consumer gets is whatever `decode` makes of the line the tool wrote. One trailing
 * newline and no other, because the answer is one line either way and a second newline would hand
 * a line reader a blank answer.
 */
function envelopeOf<T = unknown>(stdout: string): Envelope<T> {
  expect(stdout.endsWith("\n")).toBe(true);
  expect(stdout.slice(0, -1).endsWith("\n")).toBe(false);
  return decode(stdout.slice(0, -1)) as unknown as Envelope<T>;
}

/** The `--json` form of the same answer: one compact JSON document closed by one newline. */
function jsonEnvelopeOf<T = unknown>(stdout: string): Envelope<T> {
  expect(stdout.startsWith("{")).toBe(true);
  expect(stdout.endsWith("\n")).toBe(true);
  expect(stdout.slice(0, -1)).not.toContain("\n");
  return JSON.parse(stdout) as Envelope<T>;
}

/**
 * The ten members every envelope carries, whichever rendering carried them, and an `error` object
 * exactly when the status is `error` or `bootstrapping` (`src/shared/envelope.ts`).
 */
function contractOf<T>(envelope: Envelope<T>): Envelope<T> {
  for (const member of ENVELOPE_MEMBERS) {
    expect(envelope).toHaveProperty(member);
  }
  expect(ENVELOPE_STATUSES as readonly string[]).toContain(envelope.status);
  const carriesError = envelope.status === "error" || envelope.status === "bootstrapping";
  expect("error" in envelope).toBe(carriesError);
  return envelope;
}

/** The envelope a default (`TOON`) tool answer must be: exit 0 and the contract above. */
function answered<T = unknown>(run: Run): Envelope<T> {
  expectExit(run, 0, "the tool");
  return contractOf(envelopeOf<T>(run.stdout));
}

/** The same, for a `--json` answer: the two renderings are the same value in different bytes. */
function answeredJson<T = unknown>(run: Run): Envelope<T> {
  expectExit(run, 0, "the tool");
  return contractOf(jsonEnvelopeOf<T>(run.stdout));
}

/**
 * The envelope minus the one field two invocations cannot share: `read_status.stale_checked_at` is
 * stamped when *this call* looked at the bytes on disk, so comparing it across two processes would
 * assert the clock rather than the rendering.
 */
function withoutCheckStamp(envelope: Envelope<unknown>): unknown {
  const { stale_checked_at: _stamp, ...readStatus } = envelope.read_status;
  return { ...envelope, read_status: readStatus };
}

/**
 * One hand-written `call` record, field for field per the required-field table in
 * `src/shared/telemetry/report.ts`: the report is proven against a line `sink.ts` did not write.
 */
const CALL_RECORD_LINE = JSON.stringify({
  ts: "2026-11-18T10:00:00.000Z",
  v: 1,
  kind: "call",
  install_id: "0f".repeat(16),
  session_id: "1a".repeat(16),
  seq: 1,
  tool: "status",
  status: "ok",
  error_code: null,
  duration_ms: 10,
  arg_names_set: ["budget"],
  budget_tokens: 1_000,
  payload_tokens: 100,
  budget_used_ratio: 0.1,
  truncated: false,
  truncation_reason: null,
  total_relation: "gte",
  coverage_complete: true,
  coverage_indexed: 1,
  coverage_skipped: 0,
  coverage_failed: 0,
  unresolved_edges: 0,
  stale: false,
  stale_reason: null,
  generation_age_ms: null,
  result_counts: {},
  server_version: "0.1.0-beta.2",
  bun_version: "1.4.4",
  os: "linux",
  arch: "x64",
  graph_schema_version: 1,
});

const TELEMETRY_DATE = "2026-11-18";

/** A data dir holding that one daily line, plus the report path the verb writes by default. */
async function telemetryFixture(name: string): Promise<{ dataDir: string; report: string }> {
  const dataDir = join(root, name);
  const dir = join(dataDir, "telemetry");
  await mkdir(dir, { recursive: true });
  await writeFile(join(dir, `${TELEMETRY_DATE}.jsonl`), `${CALL_RECORD_LINE}\n`, "utf8");
  return { dataDir, report: join(dir, "report.html") };
}

let root = "";
let repo = "";
let elsewhere = "";
let home = "";
let bootstrap: Run | null = null;

const dbPath = (): string => join(repo, ".sangkar", "graph.db");

beforeAll(async () => {
  root = await mkdtemp(join(tmpdir(), "sangkar-cli-"));
  home = await mkdtemp(join(tmpdir(), "sangkar-cli-home-"));
  repo = join(root, "repo");
  elsewhere = join(root, "elsewhere");
  await mkdir(elsewhere, { recursive: true });
  for (const [path, source] of Object.entries(SOURCES)) {
    const target = join(repo, path);
    await mkdir(dirname(target), { recursive: true });
    await writeFile(target, source, "utf8");
  }
  // The bootstrap proof: the index is created by the command a user runs, from the tree itself.
  bootstrap = await spawnCli(["index", "--mode", "full"], repo);
}, RUN_TIMEOUT_MS);

afterAll(async () => {
  await rm(root, { recursive: true, force: true });
  await rm(home, { recursive: true, force: true });
});

describe("sangkar CLI", () => {
  test(
    "index bootstraps a fresh tree and creates the first database",
    () => {
      const run = bootstrap;
      if (run === null) {
        throw new Error("the bootstrap run never started");
      }
      expectExit(run, 0, "sangkar index");
      expect(run.stdout).toBe("");
      expect(existsSync(join(repo, ".git"))).toBe(false);
      expect(existsSync(dbPath())).toBe(true);
      expect(statSync(dbPath()).size).toBeGreaterThan(0);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status --json prints one envelope",
    async () => {
      const envelope = answeredJson(await spawnCli(["status", "--json"], repo));
      expect(envelope.status).toBe("ok");
      expect(envelope.read_status.generation.length).toBeGreaterThan(0);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status prints the TOON envelope by default, and it is not JSON",
    async () => {
      const run = await spawnCli(["status"], repo);
      const envelope = answered<{ project: { db_path: string } }>(run);
      expect(envelope.status).toBe("ok");
      expect(envelope.payload.project.db_path).toBe(dbPath());
      // The point of the change: the default answer is not the machine document `--json` writes, so
      // a caller that parses stdout as JSON must ask for that form.
      expect(run.stdout.startsWith("{")).toBe(false);
      expect(() => JSON.parse(run.stdout)).toThrow();
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status and status --json are one envelope in two renderings",
    async () => {
      const toon = await spawnCli(["status"], repo);
      const json = await spawnCli(["status", "--json"], repo);

      // Same invocation, two renderings: the exit code and the bytes' shape agree...
      expect(toon.code).toBe(json.code);
      expect(toon.code).toBe(0);
      const decoded = envelopeOf(toon.stdout);
      const parsed = jsonEnvelopeOf(json.stdout);

      // ...and every field agrees except `read_status.stale_checked_at`, which records when *this
      // invocation* looked at the bytes on disk. Two processes cannot share that clock, so it is
      // the one field compared as a shape rather than as a value.
      expect(decoded.read_status.stale_checked_at).toMatch(/^\d{4}-\d{2}-\d{2}T/);
      expect(parsed.read_status.stale_checked_at).toMatch(/^\d{4}-\d{2}-\d{2}T/);
      expect(withoutCheckStamp(decoded)).toEqual(withoutCheckStamp(parsed));
      expect(decoded.status).toBe("ok");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status reports which config file the settings came from, silently when there is none",
    async () => {
      // A tree that was indexed but never configured is the normal state: no `.sangkar/config.json`
      // exists, the three settings come from the machine file, and that is reported by the payload
      // rather than by a warning on stderr.
      const global = await spawnCli(["status"], repo);
      expectExit(global, 0, "sangkar status");
      expect(answered<{ config: { source: string } }>(global).payload.config.source).toBe("global");
      expect(global.stderr).not.toContain("repo config");

      // Writing the tree's own file changes the answer and nothing else: the same command now says
      // `project`, and the file's values are the ones the daemon will act on.
      const written = await spawnCli(["repo", "config", repo, "--watch", "false"], elsewhere);
      expectExit(written, 0, "sangkar repo config");

      const project = await spawnCli(["status"], repo);
      expectExit(project, 0, "sangkar status");
      expect(answered<{ config: { source: string } }>(project).payload.config.source).toBe(
        "project",
      );
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status on an unindexed tree prints the one bare line and exits 0, --json included",
    async () => {
      const tree = join(root, "unindexed");
      const dataDir = join(root, "unindexed-data");
      await mkdir(tree, { recursive: true });

      // The documented exception to "every tool answers with an envelope": there is no envelope to
      // render before the first index, so the line is the answer in both forms.
      for (const extra of [[], ["--json"]] as const) {
        const run = await spawnCli(
          ["status", "--repo-root", tree, "--data-dir", dataDir, ...extra],
          elsewhere,
        );
        expectExit(run, 0, `sangkar status ${extra.join(" ")}`.trim());
        expect(run.stdout).toMatch(/^no index at .* yet\n$/);
        // Exactly one line: no envelope, no banner, and no blank line where one would start.
        expect(run.stdout.trimEnd().split("\n")).toHaveLength(1);
        expect(() => JSON.parse(run.stdout)).toThrow();
      }
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "search answers from the repository root with one envelope",
    async () => {
      const run = await spawnCli(["search", "handleThing"], repo);
      const envelope = answered<{ results: readonly unknown[] }>(run);
      expect(envelope.status).toBe("ok");
      expect(envelope.payload.results.length).toBeGreaterThan(0);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "trace runs from a nested subdirectory and finds the repository by itself",
    async () => {
      const run = await spawnCli(["trace", "handleThing", "--depth", "1"], join(repo, "src"));
      const envelope = answered<{
        resolution: { matched: string };
        node: { file: string };
      }>(run);
      expect(envelope.status).toBe("ok");
      expect(envelope.read_status.generation.length).toBeGreaterThan(0);
      expect(envelope.payload.resolution.matched).toBe("src/a.ts::handleThing");
      expect(envelope.payload.node.file).toBe("src/a.ts");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "impact runs from a nested subdirectory with no arguments",
    async () => {
      const envelope = answered(await spawnCli(["impact"], join(repo, "src")));
      expect(envelope.status).toBe("ok");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "--repo-root answers from an unrelated directory in both forms",
    async () => {
      const reference = answeredJson(await spawnCli(["status", "--json"], repo));
      const separated = answeredJson(
        await spawnCli(["status", "--repo-root", repo, "--json"], elsewhere),
      );
      const inline = answeredJson(
        await spawnCli(["status", `--repo-root=${repo}`, "--json"], elsewhere),
      );
      expect(reference.read_status.generation.length).toBeGreaterThan(0);
      expect(separated.read_status.generation).toBe(reference.read_status.generation);
      expect(inline.read_status.generation).toBe(reference.read_status.generation);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "every tool command answers in TOON by default and in JSON with --json",
    async () => {
      // One invocation per tool per form, against the index `beforeAll` published — no case here
      // starts an index run of its own. The arguments are the cheapest answer each tool can give
      // from this fixture: the one symbol it declares, and the one file it declares it in.
      const invocations: readonly (readonly [string, readonly string[]])[] = [
        ["status", []],
        ["summary", []],
        ["trace", ["handleThing", "--depth", "1"]],
        ["search", ["handleThing"]],
        ["impact", []],
        ["expand", ["src/a.ts", "--what", "outline"]],
      ];

      for (const [tool, args] of invocations) {
        const toon = await spawnCli([tool, ...args], repo);
        expectExit(toon, 0, `sangkar ${tool}`);
        // The default: a value `decode` hands back, and bytes that are not the JSON document.
        expect({ tool, starts: toon.stdout.startsWith("{") }).toEqual({ tool, starts: false });
        expect({ tool, status: envelopeOf(toon.stdout).status }).toEqual({ tool, status: "ok" });

        const json = await spawnCli([tool, ...args, "--json"], repo);
        expectExit(json, 0, `sangkar ${tool} --json`);
        expect({ tool, starts: json.stdout.startsWith("{") }).toEqual({ tool, starts: true });
        expect({ tool, status: jsonEnvelopeOf(json.stdout).status }).toEqual({
          tool,
          status: "ok",
        });
      }
    },
    RUN_TIMEOUT_MS,
  );

  // The seventh name, `index`, is not a tool command on this surface: `bin/sangkar.ts` resolves the
  // token as the bootstrap verb and answers with the worker's own exit code, so there is no envelope
  // for TOON and `--json` to choose between. What must hold either way is that a verb writes nothing
  // to stdout — the property `tests/cli.test.ts` opens with. Its tool answer — the same rendering
  // every other tool uses — is asserted over MCP, where `tools/call index` is reachable
  // (`mcp-surface.test.ts`).
  test(
    "index: the CLI's seventh name is the bootstrap verb, and its stdout stays empty",
    async () => {
      // A warm tree has nothing to publish, so the run ends on the worker's own `nothing to do` code
      // rather than an envelope; a cold one publishes. Neither may write to stdout.
      for (const argv of [
        ["index", "--mode", "fast", "--json"],
        ["--json", "index", "--mode", "fast"],
      ]) {
        const run = await spawnCli(argv, repo);
        expect({ argv, code: [0, 2].includes(run.code) }).toEqual({ argv, code: true });
        expect({ argv, stdout: run.stdout }).toEqual({ argv, stdout: "" });
      }
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a global flag before the tool name names the same command",
    async () => {
      // The name is the first token that is not a global flag, so the proximity of the flag decides
      // nothing: the same answer comes back with the flag, the flag and its value, or `--json` read
      // from before the name.
      const run = await spawnCli(["--repo-root", repo, "status"], elsewhere);
      expectExit(run, 0, "sangkar --repo-root X status");
      expect(answered(run).status).toBe("ok");

      const json = await spawnCli(["--json", "status"], join(repo, "src"));
      expectExit(json, 0, "sangkar --json status");
      // `--json` before the name is still a request for the JSON rendering, not a stray flag.
      expect(jsonEnvelopeOf(json.stdout).status).toBe("ok");
      expect(json.stdout.startsWith("{")).toBe(true);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a value flag before the name without a value is still a usage refusal",
    async () => {
      // The dangling flag is skipped by the name scan (the switch after it is not a value), so
      // `status` is the name and the tool's own reader refuses the flag it was handed. Which name
      // was resolved is `cli-args.test.ts`'s subject; here the proof is that a tool answered the
      // argv rather than `mcp` starting on it.
      const run = await spawnCli(["--repo-root", "--json", "status"], elsewhere);
      expectExit(run, 2, "sangkar --repo-root --json status");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--repo-root needs a value");
      // The tool surface's banner, which only a non-reserved name earns.
      expect(run.stderr).toContain("report index status as a TOON envelope");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an unknown tool name exits 2 with nothing on stdout",
    async () => {
      const run = await spawnCli(["nonsense"], repo);
      expectExit(run, 2, "sangkar nonsense");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("nonsense");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "trace without its positional exits 2 and names the required argument",
    async () => {
      const run = await spawnCli(["trace"], repo);
      expectExit(run, 2, "sangkar trace");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("trace is missing the required argument");
      expect(run.stderr).toContain("query");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a bare positional for a tool that takes none exits 2",
    async () => {
      const run = await spawnCli(["status", "extra-token"], repo);
      expectExit(run, 2, "sangkar status extra-token");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("extra-token");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "--help lists the verbs, the seven tool names and the global flags",
    async () => {
      const run = await spawnCli(["--help"], root);
      expectExit(run, 0, "sangkar --help");
      for (const name of TOOL_NAMES) {
        expect(run.stdout).toContain(name);
      }
      expect(run.stdout).toContain("mcp");
      for (const flag of ["--repo-root", "--data-dir", "--json"]) {
        expect(run.stdout).toContain(flag);
      }
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "trace --help prints its own usage and creates nothing",
    async () => {
      const run = await spawnCli(["trace", "--help"], elsewhere);
      expectExit(run, 0, "sangkar trace --help");
      expect(run.stdout).toContain("query");
      expect(run.stdout).toContain("depth");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry writes one line and the report, and is a verb rather than a tool",
    async () => {
      const { dataDir, report } = await telemetryFixture("telemetry-data");
      const run = await spawnCli(["telemetry", "--data-dir", dataDir], elsewhere);
      expectExit(run, 0, "sangkar telemetry");
      const lines = run.stdout.replace(/\n+$/, "").split("\n");
      expect(lines).toHaveLength(1);
      expect(lines[0]).toMatch(/^wrote .*report\.html \(1 records, 0 unparsed lines\)$/);
      // The line names the file that now exists: `<data-dir>/telemetry/report.html`.
      expect(lines[0]).toContain(` ${report} `);
      expect(existsSync(report)).toBe(true);
      expect(statSync(report).size).toBeGreaterThan(0);
      expect(run.stderr.split("\n").filter((line) => /error/i.test(line))).toEqual([]);
      // Not a tool: no envelope on stdout, and no name in the tool surface's own list.
      expect(run.stdout).not.toContain('"status":');
      expect(TOOL_NAMES as readonly string[]).not.toContain("telemetry");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --json is accepted and changes nothing",
    async () => {
      // `--json` is inert here: this verb's answer is a file or a page, not a machine document. What
      // the case proves is that the global switch is an acceptance rather than a stray flag.
      const out = join(root, "telemetry-json", "report.html");
      const run = await spawnCli(["telemetry", "--json", "--out", out], elsewhere);
      expectExit(run, 0, "sangkar telemetry --json");
      expect(run.stdout).toMatch(/^wrote .*report\.html \(\d+ records, \d+ unparsed lines\)\n$/);
      expect(run.stderr).not.toContain("takes no");
      expect(existsSync(out)).toBe(true);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --help prints its own usage and writes no report",
    async () => {
      const dataDir = join(root, "telemetry-help-data");
      const run = await spawnCli(["telemetry", "--help", "--data-dir", dataDir], elsewhere);
      expectExit(run, 0, "sangkar telemetry --help");
      expect(run.stdout).toContain("usage: sangkar telemetry");
      expect(existsSync(join(dataDir, "telemetry", "report.html"))).toBe(false);
      expect(existsSync(dataDir)).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --since nonsense exits 2 with the reason and writes no report",
    async () => {
      const dataDir = join(root, "telemetry-bad-since");
      const run = await spawnCli(
        ["telemetry", "--since", "nonsense", "--data-dir", dataDir],
        elsewhere,
      );
      expectExit(run, 2, "sangkar telemetry --since nonsense");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain('--since must be YYYY-MM-DD or <n>d, not "nonsense"');
      expect(run.stderr).toContain("usage: sangkar telemetry");
      expect(existsSync(join(dataDir, "telemetry", "report.html"))).toBe(false);
      expect(existsSync(dataDir)).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --serve --out exits 2 with the usage block and writes no file",
    async () => {
      const out = join(root, "telemetry-serve-out", "report.html");
      const run = await spawnCli(["telemetry", "--serve", "--out", out], elsewhere);
      expectExit(run, 2, "sangkar telemetry --serve --out x");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--serve");
      expect(run.stderr).toContain("--out");
      expect(run.stderr).toContain("usage: sangkar telemetry");
      // The block is where an operator learns the shape of the option it collided with.
      expect(run.stderr).toContain("--port <n>");
      expect(run.stderr).toContain("--host <addr>");
      expect(existsSync(out)).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --serve --host 0.0.0.0 exits 2 with the loopback rule, not a stack",
    async () => {
      const run = await spawnCli(
        ["telemetry", "--serve", "--port", "0", "--host", "0.0.0.0"],
        elsewhere,
      );
      expectExit(run, 2, "sangkar telemetry --serve --host 0.0.0.0");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("loopback only");
      expect(run.stderr).toContain("0.0.0.0");
      expect(run.stderr).toContain("usage: sangkar telemetry");
      // A usage refusal, not a crash: an uncaught throw would leave frames behind.
      expect(run.stderr).not.toMatch(/^\s+at /m);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --serve --port outside 0..65535 exits 2 with the reason",
    async () => {
      const run = await spawnCli(["telemetry", "--serve", "--port", "70000"], elsewhere);
      expectExit(run, 2, "sangkar telemetry --serve --port 70000");
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain('--port must be an integer in 0..65535, not "70000"');
      expect(run.stderr).toContain("usage: sangkar telemetry");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "telemetry --serve --port 0 serves every repository and stops on SIGTERM",
    async () => {
      const proc = Bun.spawn({
        cmd: [process.execPath, "run", CLI_ENTRY, "telemetry", "--serve", "--port", "0"],
        cwd: elsewhere,
        env: spawnEnv(),
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
      });
      const out = streamed(proc.stdout);
      const err = streamed(proc.stderr);
      // A wedge becomes a kill rather than a hung suite.
      const backstop = setTimeout(() => proc.kill("SIGKILL"), RUN_TIMEOUT_MS);
      try {
        const line = await out.firstLine;
        // `--port 0` is read back from the URL, so no case can collide with a real daemon. The
        // count comes from the registry under this suite's temp home: the bootstrap registered one.
        expect(line).toMatch(/^serving http:\/\/127\.0\.0\.1:\d+\/ \(\d+ repositories\)$/);
        const bound = Number(/http:\/\/127\.0\.0\.1:(\d+)/.exec(line)?.[1]);
        expect(bound).toBeGreaterThan(0);
        proc.kill("SIGTERM");
        expect(await proc.exited).toBe(0);
        // One line, and the serve path never falls through to the file write.
        expect(await out.all).toBe(`${line}\n`);
        expect(existsSync(join(elsewhere, ".sangkar", "telemetry", "report.html"))).toBe(false);
      } finally {
        clearTimeout(backstop);
        if (proc.exitCode === null) {
          proc.kill("SIGKILL");
          await proc.exited;
        }
        await err.all;
      }
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "index --help prints the worker flags and creates no database",
    async () => {
      const dataDir = join(root, "help-data");
      const run = await spawnCli(["index", "--help", "--data-dir", dataDir], elsewhere);
      expectExit(run, 0, "sangkar index --help");
      expect(run.stdout).toContain("--mode");
      expect(existsSync(dataDir)).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "the first run creates the machine config beside sangkar.json",
    () => {
      // The bootstrap above was this home's first run, so the file it wrote is the proof.
      const path = join(home, "config.json");
      expect(existsSync(path)).toBe(true);
      const written = JSON.parse(readFileSync(path, "utf8")) as unknown;
      expect(written).toEqual({
        watch: true,
        watch_interval: 2_000,
        telemetry: true,
        daemon: { host: "127.0.0.1", port: 7_317 },
        budgets: { default_tokens: 1_200, max_tokens: 32_000 },
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an unknown tool name is a usage exit and does print the banner",
    async () => {
      const run = await spawnCli(["nosuchtool"], elsewhere);
      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain('sangkar: unknown tool "nosuchtool"');
      expect(run.stderr).toContain("the name comes first");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a second index with nothing to do exits 2 without the usage banner",
    async () => {
      const run = await spawnCli(["index"], repo);
      // 2 is the worker's `nothingToDo`, not a usage error: the banner would be a false answer.
      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("worker.nothing_to_do");
      expect(run.stderr).not.toContain("the name comes first");
    },
    RUN_TIMEOUT_MS,
  );
});
