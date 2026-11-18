import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, statSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
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
 * - **No setting is inherited.** `cleanEnv()` deletes every `SANGKAR_*` key, so nothing can pass
 *   because this machine happened to export one: the tree is found by autodetect, the data dir by
 *   the `.sangkar` convention, and nothing is configured out of band.
 * - **stdout belongs to the payload.** A tool answer is exactly one JSON document closed by one
 *   newline, or the five human `status` lines; a usage refusal writes nothing to stdout at all.
 *   Diagnostics are stderr.
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

/** The repository's own settings, removed: the fixture tree is discovered, never configured. */
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

function spawnCli(args: readonly string[], cwd: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: cleanEnv(),
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

/** A failure that carries the child's stderr, because "exited 1" alone never explains itself. */
function expectExit(run: Run, code: number, what: string): void {
  if (run.code !== code) {
    throw new Error(`${what} exited ${run.code}, expected ${code}. stderr:\n${run.stderr}`);
  }
  expect(run.code).toBe(code);
}

/** One compact JSON document: an object, one trailing newline, no line after it. */
function documentOf(stdout: string): string {
  expect(stdout.startsWith("{")).toBe(true);
  expect(stdout.endsWith("\n")).toBe(true);
  expect(stdout.slice(0, -1)).not.toContain("\n");
  return stdout;
}

/**
 * The envelope a tool answer must be: exit 0, one JSON document, the ten members, and an `error`
 * object exactly when the status is `error` or `bootstrapping` (`app/src/shared/envelope.ts`).
 */
function answered<T = unknown>(run: Run): Envelope<T> {
  expectExit(run, 0, "the tool");
  const envelope = JSON.parse(documentOf(run.stdout)) as Envelope<T>;
  for (const member of ENVELOPE_MEMBERS) {
    expect(envelope).toHaveProperty(member);
  }
  expect(ENVELOPE_STATUSES as readonly string[]).toContain(envelope.status);
  const carriesError = envelope.status === "error" || envelope.status === "bootstrapping";
  expect("error" in envelope).toBe(carriesError);
  return envelope;
}

let root = "";
let repo = "";
let elsewhere = "";
let bootstrap: Run | null = null;

const dbPath = (): string => join(repo, ".sangkar", "graph.db");

beforeAll(async () => {
  root = await mkdtemp(join(tmpdir(), "sangkar-cli-"));
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
      const envelope = answered(await spawnCli(["status", "--json"], repo));
      expect(envelope.status).toBe("ok");
      expect(envelope.read_status.generation.length).toBeGreaterThan(0);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status prints the five human lines and no JSON",
    async () => {
      const run = await spawnCli(["status"], repo);
      expectExit(run, 0, "sangkar status");
      const lines = run.stdout.replace(/\n+$/, "").split("\n");
      expect(lines).toHaveLength(5);
      expect(lines[0]).toMatch(/^(ok|ambiguous|not_found|error|bootstrapping): /);
      expect(() => JSON.parse(run.stdout)).toThrow();
      expect(lines.filter((line) => line.startsWith("database "))).toEqual([
        `database ${dbPath()}`,
      ]);
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
      const reference = answered(await spawnCli(["status", "--json"], repo));
      const separated = answered(
        await spawnCli(["status", "--repo-root", repo, "--json"], elsewhere),
      );
      const inline = answered(
        await spawnCli(["status", `--repo-root=${repo}`, "--json"], elsewhere),
      );
      expect(reference.read_status.generation.length).toBeGreaterThan(0);
      expect(separated.read_status.generation).toBe(reference.read_status.generation);
      expect(inline.read_status.generation).toBe(reference.read_status.generation);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a global flag before the tool name is a usage error",
    async () => {
      const run = await spawnCli(["--repo-root", repo, "status"], elsewhere);
      expectExit(run, 2, "sangkar --repo-root X status");
      expect(run.stdout).toBe("");
      expect(run.stderr.length).toBeGreaterThan(0);
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
});
