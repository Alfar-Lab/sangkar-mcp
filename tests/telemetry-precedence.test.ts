import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type AppConfig, loadConfig } from "#shared/config";
import { type RepoConfig, WATCH_INTERVAL_DEFAULT_MS } from "#shared/repo-config";
import { type RepoTelemetryInput, telemetryForRepo } from "#shared/telemetry/for-repo";
import type { CallSpec, TelemetryRecord } from "#shared/telemetry/record";

/**
 * The recording precedence, executable: one function decides whether a repository gets a recorder
 * and where that recorder writes. The four cases are the truth table of `recordingEnabled` —
 * machine consent, registration, then the repo's own file — and the enabled case is proved by the
 * daily file it writes, not by the object it returns.
 *
 * Every case owns a `mkdtemp` repo and hands `loadConfig` its own environment, so nothing here
 * reads $HOME/.config/sangkar, this repository's `.sangkar/`, or an inherited `SANGKAR_*`.
 */

const APP_ROOT = join(import.meta.dir, "..");
const FIXED_TS = "2026-11-18T10:00:00.000Z";
const FIXED_DATE = "2026-11-18";
const SERVER_VERSION = "0.1.0-beta.0";
const GRAPH_SCHEMA_VERSION = 3;

const dirs: string[] = [];

function tempRepo(): string {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-telemetry-for-repo-"));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

/**
 * The machine config for one temp repo. Its data dir is deliberately not the repo's `.sangkar`:
 * the recorder must take its destination from `repo.dataDir`, and a shared dir would hide that.
 */
function configOf(repoRoot: string, consent: boolean): AppConfig {
  return loadConfig({
    argv: ["--repo-root", repoRoot, "--data-dir", "machine-store"],
    env: {
      SANGKAR_HOME: join(repoRoot, "home"),
      SANGKAR_TELEMETRY: consent ? "on" : "off",
    },
    cwd: repoRoot,
  });
}

function repoOf(repoRoot: string, telemetry: boolean): RepoConfig {
  return {
    repoRoot,
    dataDir: join(repoRoot, ".sangkar"),
    watch: true,
    watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
    telemetry,
    source: "global",
  };
}

interface CaseOptions {
  readonly consent: boolean;
  readonly repoTelemetry: boolean;
  readonly registered: boolean;
}

interface Seeded {
  readonly input: RepoTelemetryInput;
  readonly repoRoot: string;
  readonly telemetryDir: string;
  readonly machineTelemetryDir: string;
}

function caseOf(options: CaseOptions): Seeded {
  const repoRoot = tempRepo();
  const repo = repoOf(repoRoot, options.repoTelemetry);
  const config = configOf(repoRoot, options.consent);
  return {
    repoRoot,
    telemetryDir: join(repo.dataDir, "telemetry"),
    machineTelemetryDir: config.telemetry.dir,
    input: {
      config,
      repo,
      registered: options.registered,
      sessionMode: "mcp",
      serverVersion: SERVER_VERSION,
      graphSchemaVersion: GRAPH_SCHEMA_VERSION,
      now: () => new Date(FIXED_TS),
    },
  };
}

function callSpec(overrides: Partial<CallSpec> = {}): CallSpec {
  return {
    tool: "trace",
    args: { query: "handleRequest", budget: 3200 },
    suppliedNames: ["query"],
    envelope: null,
    status: "ok",
    errorCode: null,
    durationMs: 12,
    ...overrides,
  };
}

/**
 * A discarded recorder is the empty one: `nullTelemetry()` hands out `""` for both identifiers,
 * takes no directory and writes nothing, so a no-consent case that left a file behind fails here.
 */
function expectNoRecorder(seeded: Seeded): void {
  const recorder = telemetryForRepo(seeded.input);
  expect(recorder.installId).toBe("");
  expect(recorder.sessionId).toBe("");

  recorder.recordCall(callSpec());
  recorder.flush();
  recorder.close();

  expect(existsSync(seeded.telemetryDir)).toBe(false);
  expect(existsSync(seeded.input.repo.dataDir)).toBe(false);
  expect(existsSync(seeded.machineTelemetryDir)).toBe(false);
}

describe("the one enabled case", () => {
  test("consent + registration + repo telemetry:true writes one daily JSONL file in the repo's data dir", () => {
    const seeded = caseOf({ consent: true, repoTelemetry: true, registered: true });
    const recorder = telemetryForRepo(seeded.input);

    expect(recorder.installId).not.toBe("");
    expect(recorder.sessionId).not.toBe("");

    recorder.recordCall(callSpec());
    recorder.flush();
    recorder.close();

    const files = readdirSync(seeded.telemetryDir).filter((name) => name.endsWith(".jsonl"));
    expect(files).toEqual([`${FIXED_DATE}.jsonl`]);

    const path = join(seeded.telemetryDir, `${FIXED_DATE}.jsonl`);
    expect(path).toBe(join(seeded.repoRoot, ".sangkar", "telemetry", `${FIXED_DATE}.jsonl`));
    const records = readFileSync(path, "utf8")
      .trimEnd()
      .split("\n")
      .map((line) => JSON.parse(line) as TelemetryRecord);
    expect(records.map((record) => record.kind)).toEqual(["session", "call"]);

    // The destination is the repo's own data dir, never the machine config's telemetry dir.
    expect(existsSync(seeded.machineTelemetryDir)).toBe(false);
  });
});

describe("each single no is enough", () => {
  test("machine consent off records nothing even though the repo opts in and is registered", () => {
    expectNoRecorder(caseOf({ consent: false, repoTelemetry: true, registered: true }));
  });

  test("an unregistered repo records nothing even with consent and its own telemetry true", () => {
    expectNoRecorder(caseOf({ consent: true, repoTelemetry: true, registered: false }));
  });

  test("a repo whose own file says telemetry:false records nothing even with consent and registration", () => {
    expectNoRecorder(caseOf({ consent: true, repoTelemetry: false, registered: true }));
  });
});

describe("the gate has one owner", () => {
  test("the module re-derives no consent rule and reads no environment", () => {
    const source = readFileSync(join(APP_ROOT, "src/shared/telemetry/for-repo.ts"), "utf8");
    expect(source).not.toMatch(/process\.env/);
    expect(source).not.toMatch(/Bun\.env/);
    expect(source).not.toMatch(/SANGKAR_TELEMETRY/);
    // The precedence itself is imported from its owner rather than spelled again here.
    expect(source).toContain('from "#shared/repo-config"');
  });
});
