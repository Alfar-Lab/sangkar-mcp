#!/usr/bin/env bun
/**
 * Captures the TypeScript oracle's answers into `rust/tests/golden/**`, one file per (fixture, tool,
 * argument set).
 *
 * The shapes are fixed by `rust/tests/parity/README.md`: a golden is
 * `{args, stdout, exit_code}`, where `args` is the argv tail after the tool name (the global
 * `--json --repo-root --data-dir` flags are appended by whoever replays it) and `stdout` is the
 * exact bytes the TypeScript CLI wrote. Nothing is normalized or hand-edited here — a capture that
 * looks wrong is fixed by changing the fixture or the invocation and capturing again.
 *
 * Two properties keep a capture reproducible:
 *
 * - every fixture is archived to a temp directory outside git, because `payload.project` and the
 *   change-scope tools read git: a checked-in fixture inside this repository would stamp every
 *   golden with the commit it was captured at, and would answer from the repository's own diff.
 *   The archive also guarantees nothing is written inside a checked-in fixture.
 * - `SANGKAR_CURSOR_KEY` is fixed, so the `next` tokens a page mints sign with a key the Rust side
 *   knows and can verify. The generation id inside those tokens is random per publish (see the
 *   README's per-run field table), so a `next` token is reproducible given its generation, not
 *   across publishes.
 *
 * Usage, from the repository root: `~/.bun/bin/bun run scripts/parity/capture-goldens.ts`
 * Set `SANGKAR_PARITY_KEEP=1` to leave the temp archives (and their databases) in place.
 */
import { spawnSync } from "node:child_process";
import { chmod, cp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const REPO_ROOT = resolve(import.meta.dir, "..", "..");
const CLI = join(REPO_ROOT, "bin", "sangkar.ts");
const FIXTURES = join(REPO_ROOT, "rust", "tests", "parity", "fixtures");
const GOLDENS = join(REPO_ROOT, "rust", "tests", "golden");
const WORK_ROOT = join(tmpdir(), "sangkar-parity-capture");

/**
 * The key every capture runs with, and the key the Rust harness must mint and verify with. 48
 * bytes, above the 32-byte floor `src/shared/cursor.ts` enforces, and deliberately not a secret.
 * Documented verbatim in `rust/tests/parity/README.md`.
 */
const CURSOR_KEY = "sangkar-parity-fixed-cursor-key-0123456789abcdef";

/** The answer a capture records: the exact bytes, and the process code they came with. */
interface Golden {
  args: string[];
  stdout: string;
  exit_code: number;
}

interface Archive {
  /** Archive of the fixture, indexed once. Reads are served from here. */
  readonly repoRoot: string;
  /** A second, cold archive: the `index` call is the one tool that writes, so it gets its own. */
  readonly indexRoot: string;
  readonly dataDir: string;
  readonly indexDataDir: string;
  readonly home: string;
}

/**
 * The probes the matrix names. They are polyglot's names on purpose: the same arguments run against
 * all three fixtures, so `broken` and `empty` capture the not-found and empty-graph answers for the
 * identical call, which is the comparison the port needs.
 */
const TRACE_QUERY = "alpha";
const TRACE_DUPLICATE = "parseInput";
const SEARCH_TOKEN = "request";
const IMPACT_PATH = "src/chain.ts";
const SUMMARY_AREA = "src";
const SCOPE_PATH = "src";
/** The expand target when the fixture's status answer lists no path (`empty`). */
const FALLBACK_TARGET = "src/chain.ts";

/**
 * The small budgets of the pagination captures. Chosen so polyglot — the one fixture with more items
 * than a page — cuts with items shown and a `next` token minted; on the smaller fixtures the same
 * budget may cut nothing, which is the truthful answer and stays a golden.
 */
const PAGE_BUDGET = {
  status: 600,
  summary: 400,
  trace: 500,
  search: 300,
  impact: 500,
} as const;

const FIXTURE_NAMES = ["polyglot", "broken", "empty"] as const;
type FixtureName = (typeof FIXTURE_NAMES)[number];

/** One CLI run, stdout and stderr split: stderr is logging and never part of a golden. */
function runCli(
  argv: readonly string[],
  home: string,
  repoRoot: string,
  dataDir: string,
): { stdout: string; stderr: string; code: number } {
  const result = spawnSync(
    process.execPath,
    ["run", CLI, ...argv, "--json", "--repo-root", repoRoot, "--data-dir", dataDir],
    {
      // The app root, not the fixture: the CLI resolves its own entry and app paths from here.
      cwd: REPO_ROOT,
      encoding: "utf8",
      maxBuffer: 64 * 1024 * 1024,
      timeout: 240_000,
      env: {
        ...process.env,
        // Every machine-level write (registry, settings) lands in the temp home, never the caller's.
        SANGKAR_HOME: home,
        SANGKAR_CURSOR_KEY: CURSOR_KEY,
        // Hermetic: a capture must not append to a telemetry file, and no golden reads one.
        SANGKAR_TELEMETRY: "off",
      },
    },
  );
  if (result.error !== undefined) {
    throw new Error(`${argv[0]} could not run: ${result.error.message}`);
  }
  return { stdout: result.stdout ?? "", stderr: result.stderr ?? "", code: result.status ?? -1 };
}

/**
 * A fixture archive outside git. `broken`'s unreadable directory is created here and only here: it
 * is a runtime condition, not a checked-in permission bit, so git stays clean and a walk that
 * cannot read it reports `unreadable` instead of silently skipping it.
 */
async function archive(fixture: FixtureName, location: string): Promise<string> {
  await cp(join(FIXTURES, fixture), location, { recursive: true });
  if (fixture === "broken") {
    const locked = join(location, "locked");
    await mkdir(locked, { recursive: true });
    await writeFile(join(locked, "notes.ts"), "export const secret = 1;\n");
    await chmod(locked, 0o000);
  }
  return location;
}

/** Undo the chmod before removal: a `000` directory cannot be read by its own cleanup walk. */
async function release(location: string): Promise<void> {
  const locked = join(location, "locked");
  await chmod(locked, 0o755).catch(() => undefined);
}

function parseEnvelope(label: string, golden: Golden): Record<string, unknown> {
  try {
    const parsed: unknown = JSON.parse(golden.stdout);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      throw new Error(`stdout is a JSON ${Array.isArray(parsed) ? "array" : "primitive"}`);
    }
    return parsed as Record<string, unknown>;
  } catch (error) {
    throw new Error(`${label}: --json stdout did not parse: ${(error as Error).message}`);
  }
}

function payloadOf(envelope: Record<string, unknown>): Record<string, unknown> {
  const payload = envelope.payload;
  return typeof payload === "object" && payload !== null && !Array.isArray(payload)
    ? (payload as Record<string, unknown>)
    : {};
}

function coverageOf(envelope: Record<string, unknown>): Record<string, unknown> {
  const coverage = envelope.coverage;
  return typeof coverage === "object" && coverage !== null && !Array.isArray(coverage)
    ? (coverage as Record<string, unknown>)
    : {};
}

/** The golden writer: stable key order and one trailing newline, so a recapture diffs readably. */
function serialize(golden: Golden): string {
  return `${JSON.stringify(
    { args: golden.args, stdout: golden.stdout, exit_code: golden.exit_code },
    null,
    2,
  )}\n`;
}

async function captureFixture(
  fixture: FixtureName,
): Promise<Array<{ name: string; golden: Golden }>> {
  const root = join(WORK_ROOT, fixture);
  const archiveOf: Archive = {
    repoRoot: await archive(fixture, join(root, fixture)),
    indexRoot: await archive(fixture, join(root, `index-${fixture}`)),
    dataDir: join(root, "data"),
    indexDataDir: join(root, "index-data"),
    home: join(root, "home"),
  };
  await mkdir(archiveOf.dataDir, { recursive: true });
  await mkdir(archiveOf.indexDataDir, { recursive: true });
  await mkdir(archiveOf.home, { recursive: true });

  // One publish per archive, before any read: every read tool answers from an indexed generation.
  // The setup run is not a golden — `index.json` is captured separately, on the cold archive.
  const setup = runCli(
    ["index", "--mode", "fast"],
    archiveOf.home,
    archiveOf.repoRoot,
    archiveOf.dataDir,
  );
  if (setup.code !== 0) {
    throw new Error(`${fixture}: the setup index failed (exit ${setup.code}):\n${setup.stderr}`);
  }
  if (setup.stdout !== "") {
    throw new Error(`${fixture}: the setup index wrote to stdout, which the verb never does`);
  }

  const captures: Array<{ name: string; golden: Golden }> = [];
  /**
   * One golden: the tool, the argv tail after its name, and the archive to answer from. `args` in
   * the file is that tail alone — the tool name is the file's own field of the matrix — so a
   * replay appends exactly the flags this function appended.
   */
  const record = (
    tool: string,
    name: string,
    args: string[],
    repoRoot = archiveOf.repoRoot,
    dataDir = archiveOf.dataDir,
  ) => {
    const run = runCli([tool, ...args], archiveOf.home, repoRoot, dataDir);
    const golden: Golden = { args, stdout: run.stdout, exit_code: run.code };
    const envelope = parseEnvelope(`${fixture}/${name}`, golden);
    if (run.code !== 0) {
      throw new Error(
        `${fixture}/${name}: every captured call answers with an envelope and exit 0; got ${run.code}:\n${run.stderr}`,
      );
    }
    captures.push({ name, golden });
  };

  /**
   * The one call without an envelope: `index` is the CLI's bootstrap verb, so it answers with the
   * worker's own exit code and keeps stdout empty (`bin/sangkar.ts`). Its golden is exactly that —
   * `stdout: ""` and the code a real publish returns — and asserting the empty stdout is the
   * contract, not a convenience.
   */
  const recordRaw = (
    tool: string,
    name: string,
    args: string[],
    repoRoot: string,
    dataDir: string,
  ) => {
    const run = runCli([tool, ...args], archiveOf.home, repoRoot, dataDir);
    const golden: Golden = { args, stdout: run.stdout, exit_code: run.code };
    if (run.code !== 0) {
      throw new Error(`${fixture}/${name}: expected exit 0, got ${run.code}:\n${run.stderr}`);
    }
    captures.push({ name, golden });
    return golden;
  };

  record("status", "status", []);
  record("status", "status.paths", ["--paths", SCOPE_PATH]);
  record("status", "status.page", [
    "--paths",
    SCOPE_PATH,
    "--budget",
    String(PAGE_BUDGET.status),
  ]);
  record("summary", "summary", ["--scope", "repo"]);
  record("summary", "summary.area", ["--scope", "area", "--target", SUMMARY_AREA]);
  record("summary", "summary.change", ["--scope", "change", "--base", "HEAD"]);
  record("summary", "summary.page", [
    "--scope",
    "repo",
    "--budget",
    String(PAGE_BUDGET.summary),
  ]);
  record("trace", "trace", ["--query", TRACE_QUERY, "--depth", "2"]);
  record("trace", "trace.duplicate", ["--query", TRACE_DUPLICATE, "--depth", "1"]);
  record("trace", "trace.page", [
    "--query",
    TRACE_QUERY,
    "--depth",
    "2",
    "--budget",
    String(PAGE_BUDGET.trace),
  ]);
  record("search", "search", ["--query", SEARCH_TOKEN]);
  record("search", "search.kind", [
    "--query",
    SEARCH_TOKEN,
    "--kind",
    "function",
    "--path",
    SCOPE_PATH,
  ]);
  record("search", "search.page", [
    "--query",
    SEARCH_TOKEN,
    "--budget",
    String(PAGE_BUDGET.search),
  ]);
  record("impact", "impact", ["--paths", IMPACT_PATH, "--depth", "2"]);
  record("impact", "impact.base", ["--base", "HEAD"]);
  record("impact", "impact.page", [
    "--paths",
    IMPACT_PATH,
    "--depth",
    "2",
    "--budget",
    String(PAGE_BUDGET.impact),
  ]);

  // The expand target is the first path the status answer above reported: it is the only address
  // that answer carries, and a real one beats a hard-coded name that could drift from the fixture.
  const statusPaths = captures.find((capture) => capture.name === "status.paths");
  if (statusPaths === undefined) {
    throw new Error(`${fixture}: status.paths was not captured`);
  }
  const reportedPaths = payloadOf(
    parseEnvelope(`${fixture}/status.paths`, statusPaths.golden),
  ).paths;
  const firstPath =
    Array.isArray(reportedPaths) && typeof reportedPaths[0]?.path === "string"
      ? (reportedPaths[0].path as string)
      : FALLBACK_TARGET;
  record("expand", "expand", ["--target", firstPath, "--what", "members"]);
  record("expand", "expand.outline", ["--target", firstPath, "--what", "outline"]);

  // The seventh tool: the only one that writes, so it runs against the cold archive and the answer
  // is the worker's own contract (a real publish exits 0 and writes nothing to stdout).
  const indexGolden = recordRaw(
    "index",
    "index",
    ["--mode", "fast"],
    archiveOf.indexRoot,
    archiveOf.indexDataDir,
  );
  if (indexGolden.stdout !== "") {
    throw new Error(
      `index: the bootstrap verb must keep stdout empty, got ${indexGolden.stdout.length} bytes`,
    );
  }

  const status = captures.find((capture) => capture.name === "status");
  if (status === undefined) {
    throw new Error(`${fixture}: status was not captured`);
  }
  const statusEnvelope = parseEnvelope(`${fixture}/status`, status.golden);
  const coverage = coverageOf(statusEnvelope);
  const notIndexed = Array.isArray(coverage.not_indexed) ? coverage.not_indexed : [];
  const reasons = new Set(
    notIndexed
      .map((entry) =>
        typeof entry === "object" && entry !== null
          ? (entry as { reason?: unknown }).reason
          : undefined,
      )
      .filter((reason): reason is string => typeof reason === "string"),
  );

  // The two facts the task's Done-when line names, asserted where they are cheap to check: a
  // fixture that stops producing them is a capture failure, not a quiet golden drift.
  if (fixture === "polyglot" && coverage.failed !== 0) {
    throw new Error(`polyglot: coverage.failed must be 0, got ${String(coverage.failed)}`);
  }
  if (fixture === "broken") {
    for (const reason of ["binary", "unreadable"]) {
      if (!reasons.has(reason)) {
        throw new Error(
          `broken: coverage.not_indexed has no ${reason} entry (got ${[...reasons].join(", ")})`,
        );
      }
    }
  }
  if (fixture === "polyglot") {
    // A page golden that stopped minting `next` would silently stop being a pagination oracle.
    for (const capture of captures) {
      if (!capture.name.endsWith(".page")) {
        continue;
      }
      const envelope = parseEnvelope(`polyglot/${capture.name}`, capture.golden);
      if (typeof envelope.next !== "string" || envelope.next === "") {
        throw new Error(`polyglot/${capture.name}: expected a next token, got none`);
      }
    }
  }

  return captures;
}

async function main(): Promise<void> {
  await rm(WORK_ROOT, { recursive: true, force: true });
  await mkdir(WORK_ROOT, { recursive: true });

  const captured: Array<{ fixture: FixtureName; name: string; golden: Golden }> = [];
  try {
    for (const fixture of FIXTURE_NAMES) {
      for (const capture of await captureFixture(fixture)) {
        captured.push({ fixture, name: capture.name, golden: capture.golden });
        process.stdout.write(`captured ${fixture}/${capture.name}.json\n`);
      }
    }
  } finally {
    for (const fixture of FIXTURE_NAMES) {
      await release(join(WORK_ROOT, fixture, fixture));
      await release(join(WORK_ROOT, fixture, `index-${fixture}`));
    }
    if (process.env.SANGKAR_PARITY_KEEP !== "1") {
      await rm(WORK_ROOT, { recursive: true, force: true });
    }
  }

  if (captured.length < 21) {
    throw new Error(`captured ${captured.length} golden files; the harness needs at least 21`);
  }

  // Written only after every capture succeeded: a failed run leaves the previous bank untouched
  // instead of half-replacing it.
  await rm(GOLDENS, { recursive: true, force: true });
  for (const entry of captured) {
    const directory = join(GOLDENS, entry.fixture);
    await mkdir(directory, { recursive: true });
    await writeFile(join(directory, `${entry.name}.json`), serialize(entry.golden));
  }
  process.stdout.write(
    `wrote ${captured.length} golden files under rust/tests/golden (cursor key ${CURSOR_KEY})\n`,
  );
}

await main();
