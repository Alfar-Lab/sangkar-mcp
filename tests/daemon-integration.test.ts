import { afterAll, beforeAll, describe, expect, spyOn, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join, resolve } from "node:path";
import { type LogFields, log } from "#shared/log";
import { readRegistry } from "#shared/registry";
import { WATCH_INTERVAL_DEFAULT_MS, writeRepoConfig } from "#shared/repo-config";
import { runDaemonForeground } from "../src/daemon/main";
import type { WatcherOptions } from "../src/daemon/reconcile";
import type { SpawnWorker, WatchHandle, WatchStats } from "../src/indexer/watch";

/**
 * The whole feature, once: two registered repositories watched and reported by one daemon, and a
 * third tree that no registry knows, still answerable per command.
 *
 * The five cases below are one file because they are one story — the CLI registers, the daemon
 * reconciles and serves, an ad-hoc `status` answers anyway, and the stop leaves nothing behind. What
 * makes them affordable is that every expensive seam is a fixture rather than a process:
 *
 * - the graphs come from three real `sangkar index` runs (`bin/sangkar.ts`, spawned with
 *   `process.execPath`), which is also what registers A and B and what proves `--no-register`;
 * - the daemon runs **in this process** with `openWatcher` and `spawn` injected, so no watcher
 *   loop, no poll timer and no worker child exists — a fake handle has no scheduler behind it;
 * - no poll is ever awaited as a duration: `intervalMs: 5` plus an `onTick` counter is three ticks,
 *   and the assertion is on the count, not on elapsed time.
 *
 * Hermeticity is the reason the fixture is four `mkdtemp` directories: one home (`SANGKAR_HOME` for
 * every spawn, `daemon.pid` for the daemon) and three trees. Every `SANGKAR_*` key is deleted from
 * every spawn environment before this suite's two are set, the daemon's `home` is an argument rather
 * than an environment read, and every directory is removed in `afterAll` — so nothing here reads or
 * writes the developer's real `~/.config/sangkar`, and a home read that leaked would land in the
 * fixture instead.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "daemon-integration-test-cursor-key-0123456789";

/** Three CLI spawns, a dozen ticks and two ::1 sessions; a wedge must fail, not hang the suite. */
const RUN_TIMEOUT_MS = 120_000;

/**
 * A's own poll period, deliberately not `WATCH_INTERVAL_DEFAULT_MS`: a watcher opened from a
 * defaulted read would still be inside the legal range, so only a distinct number can prove the
 * repo's `config.json` was the source.
 */
const A_WATCH_INTERVAL_MS = 2_500;

/** Every temp directory this suite made, removed in one pass. */
const dirs: string[] = [];

/**
 * `realpathSync` because `readRegistry` and `canonicalRoot` canonicalise every root: the reconciler's
 * map is keyed by the canonical spelling, and a `TMPDIR` that is a symlink would hand the fixture
 * two names for one directory.
 */
function tempDir(prefix: string): string {
  const dir = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

interface Sandbox {
  /** The machine-level home: `sangkar.json` and `daemon.pid` both live here. */
  readonly home: string;
  /** Indexed by the CLI, which registers it; its `config.json` sets A_WATCH_INTERVAL_MS. */
  readonly repoA: string;
  /** Indexed by the CLI, which registers it; no `config.json`, so everything is defaulted. */
  readonly repoB: string;
  /** Indexed with `--no-register` and never registered: the ad-hoc tree of case 4. */
  readonly loose: string;
}

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

/**
 * The repository's own variables are removed and then this suite's two are set: an inherited
 * `SANGKAR_HOME` would decide which registry a child reads, and an inherited
 * `SANGKAR_TELEMETRY=off` would hide the recording asymmetry case 4 asserts.
 */
function cleanEnv(home: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  return { ...env, SANGKAR_HOME: home, SANGKAR_CURSOR_KEY: CURSOR_KEY };
}

/** One CLI invocation, in the tree it names and in this suite's own machine home. */
async function spawnCli(args: readonly string[], cwd: string, home: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: cleanEnv(home),
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { code, stdout, stderr };
}

/** One source file per tree, named so an answer proves *which* graph it came from. */
function writeSource(repoRoot: string, fileName: string): void {
  const file = join(repoRoot, "src", fileName);
  mkdirSync(resolve(file, ".."), { recursive: true });
  const symbol = fileName
    .replace(/^a-/, "")
    .replace(/\.ts$/, "")
    .replace(/[^A-Za-z0-9]/g, "");
  writeFileSync(file, `export function ${symbol}Thing(): number {\n  return 1;\n}\n`, "utf8");
}

/** The registry as it sits on disk, parsed without going through the code under test. */
function registryOnDisk(home: string): unknown {
  return JSON.parse(readFileSync(join(home, "sangkar.json"), "utf8"));
}

/** `<repo>/.sangkar`, spelled the way `#shared/config.ts` spells it. */
function dataDirOf(repoRoot: string): string {
  return join(repoRoot, ".sangkar");
}

/** What an `index` run published: the graph the daemon's report and an ad-hoc `status` read. */
function graphExists(repoRoot: string): boolean {
  return existsSync(join(dataDirOf(repoRoot), "graph.db"));
}

/** The telemetry directory a recording run writes into; its absence is case 4's asymmetry. */
function telemetryExists(repoRoot: string): boolean {
  return existsSync(join(dataDirOf(repoRoot), "telemetry"));
}

let box!: Sandbox;
let indexRuns!: { readonly a: Run; readonly b: Run; readonly loose: Run };

/**
 * The fixtures and the three CLI runs, built once: the graphs every later case reads are the ones
 * the shipped verb published, so no case can pass on a graph this suite fabricated.
 */
async function buildSandbox(): Promise<void> {
  const home = tempDir("sangkar-daemon-integration-home-");
  const repoA = tempDir("sangkar-daemon-integration-alpha-");
  const repoB = tempDir("sangkar-daemon-integration-beta-");
  const loose = tempDir("sangkar-daemon-integration-loose-");
  writeSource(repoA, "a-alpha.ts");
  writeSource(repoB, "a-beta.ts");
  writeSource(loose, "a-gamma.ts");
  box = { home, repoA, repoB, loose };

  const a = await spawnCli(["index", "--repo-root", repoA], repoA, home);
  const b = await spawnCli(["index", "--repo-root", repoB], repoB, home);
  const looseRun = await spawnCli(["index", "--repo-root", loose, "--no-register"], loose, home);
  indexRuns = { a, b, loose: looseRun };

  // A's own settings, written after its index run so the file names a period no default produces.
  // A registered repo records; the defaulted `telemetry: true` is left at its default on purpose.
  writeRepoConfig({
    repoRoot: repoA,
    dataDir: dataDirOf(repoA),
    watch: true,
    watchIntervalMs: A_WATCH_INTERVAL_MS,
    telemetry: true,
  });
  // B declares the defaults on purpose: a file beside the graph is what `readRepoConfig` expects, so
  // the only thing B can prove is that the daemon read *B's* settings and not A's.
  writeRepoConfig({
    repoRoot: repoB,
    dataDir: dataDirOf(repoB),
    watch: true,
    watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
    telemetry: true,
  });
}

beforeAll(buildSandbox, RUN_TIMEOUT_MS);

/** A watcher factory that records every open: "one per registered root" is then a count. */
interface WatcherSpy {
  readonly opened: { readonly options: WatcherOptions; readonly stops: () => number }[];
  readonly openWatcher: (options: WatcherOptions) => WatchHandle;
}

function watcherSpy(): WatcherSpy {
  const opened: WatcherSpy["opened"][number][] = [];
  return {
    opened,
    openWatcher: (options: WatcherOptions): WatchHandle => {
      let stops = 0;
      const stats: WatchStats = {
        polls: 0,
        spawns: 0,
        failures: 0,
        backoffMs: 0,
        lastExitCode: null,
        lastSpawnAt: null,
        detector: null,
      };
      const handle: WatchHandle = {
        stop: async (): Promise<void> => {
          stops += 1;
        },
        stats: (): WatchStats => stats,
      };
      opened.push({ options, stops: () => stops });
      return handle;
    },
  };
}

/**
 * The worker spawn seam, injected and expected to stay unused. Recording the call and throwing makes
 * an accidental child a visible failure instead of a process this suite has to clean up; the
 * watcher's own spawn ladder is `tests/watch.test.ts`'s subject, not this file's.
 */
const spawns: (readonly string[])[] = [];
const spawn: SpawnWorker = (cmd) => {
  spawns.push(cmd);
  throw new Error("the daemon was handed a watcher factory that spawns; this suite injects one");
};

interface Recorded {
  readonly msg: string;
  readonly fields: LogFields;
}

/**
 * The daemon's own diagnostics, captured instead of printed. `daemon.ready` is the only place the
 * bound url exists when the caller asked for an ephemeral port, so reading it back is the honest way
 * to fetch the page the process is actually serving.
 */
interface LogSpy {
  readonly recorded: Recorded[];
  readonly restore: () => void;
}

function recordLog(): LogSpy {
  const recorded: Recorded[] = [];
  const spy = spyOn(log, "info").mockImplementation((msg: string, fields?: LogFields): void => {
    recorded.push({ msg, fields: fields ?? {} });
  });
  return { recorded, restore: () => spy.mockRestore() };
}

function readyUrl(recorded: readonly Recorded[]): string {
  const ready = recorded.find((entry) => entry.msg === "daemon.ready");
  const url = ready?.fields.url;
  return typeof url === "string" ? url : "";
}

function deferred(): { readonly promise: Promise<void>; readonly resolve: () => void } {
  let settle = (): void => {};
  const promise = new Promise<void>((done) => {
    settle = done;
  });
  return { promise, resolve: () => settle() };
}

describe("daemon: two registered repos and one ad-hoc tree", () => {
  test("case 1: the CLI registers A and B, publishes both graphs, and --no-register adds nothing", () => {
    // The three index runs succeeded: a failure here would be a fixture problem, not an assertion.
    expect(indexRuns.a.code).toBe(0);
    expect(indexRuns.b.code).toBe(0);
    expect(indexRuns.loose.code).toBe(0);

    // Read back both ways: the file as it sits on disk, and through the module the daemon reads.
    expect(registryOnDisk(box.home)).toMatchObject({
      version: 1,
      repos: [box.repoA, box.repoB],
    });
    const registry = readRegistry(join(box.home, "sangkar.json"));
    expect([...registry.repos].sort()).toEqual([box.repoA, box.repoB].sort());
    expect(registry.repos).not.toContain(box.loose);

    // Each run published its own tree's graph — registered or not, the index verb is the publisher.
    expect(graphExists(box.repoA)).toBe(true);
    expect(graphExists(box.repoB)).toBe(true);
    expect(graphExists(box.loose)).toBe(true);

    // And the ad-hoc run left the registry at two entries: `--no-register` is an opt-out, not a hint.
    expect(registry.repos).toHaveLength(2);
  });

  test(
    "cases 2-5: the daemon serves both labels, watches each registered root once, and the ad-hoc tree stays unrecorded",
    async () => {
      const watchers = watcherSpy();
      const lines = recordLog();
      const controller = new AbortController();
      const threeTicks = deferred();
      let ticks = 0;

      const run = runDaemonForeground({
        home: box.home,
        port: 0,
        spawn,
        openWatcher: watchers.openWatcher,
        intervalMs: 5,
        signal: controller.signal,
        onTick: async (): Promise<void> => {
          ticks += 1;
          if (ticks >= 3) {
            threeTicks.resolve();
          }
        },
      });

      try {
        // Case 2. Three passes of the daemon's own loop, counted rather than waited for.
        await threeTicks.promise;

        // The lock is held while the process serves, and it is the file `daemon status` reads.
        expect(existsSync(join(box.home, "daemon.pid"))).toBe(true);

        const url = readyUrl(lines.recorded);
        expect(url).toStartWith("http://127.0.0.1:");
        const response = await fetch(url);
        expect(response.status).toBe(200);
        expect(response.headers.get("content-type")).toContain("text/html");
        const html = await response.text();

        // One section per registered repository, labelled `<basename> — <root>`, so the page is this
        // home's registry and not a neighbour's. The unregistered tree is in neither.
        expect(html).toContain("<title>sangkar telemetry report</title>");
        expect(html).toContain(box.repoA);
        expect(html).toContain(box.repoB);
        expect(html).toContain(basename(box.repoA));
        expect(html).toContain(basename(box.repoB));
        expect(html).toContain("repositories <span>2</span>");
        expect(html).not.toContain(box.loose);

        // Exactly one watcher per watched repo, and none for the tree nobody registered.
        expect(watchers.opened).toHaveLength(2);
        expect(watchers.opened.map((watcher) => watcher.options.repoRoot).sort()).toEqual(
          [box.repoA, box.repoB].sort(),
        );
        expect(watchers.opened.every((watcher) => watcher.options.repoRoot !== box.loose)).toBe(
          true,
        );
        // No child process exists: the daemon opened nothing and spawned nothing.
        expect(spawns).toEqual([]);

        // Case 3. A's period is its own `config.json`, not the default; B's is the default.
        const configOnDisk: unknown = JSON.parse(
          readFileSync(join(dataDirOf(box.repoA), "config.json"), "utf8"),
        );
        expect(configOnDisk).toMatchObject({ watch_interval: A_WATCH_INTERVAL_MS });
        const watchedA = watchers.opened.find((watcher) => watcher.options.repoRoot === box.repoA);
        const watchedB = watchers.opened.find((watcher) => watcher.options.repoRoot === box.repoB);
        expect(watchedA?.options.intervalMs).toBe(A_WATCH_INTERVAL_MS);
        expect(watchedA?.options.dataDir).toBe(dataDirOf(box.repoA));
        // B's file declares the default, so B's watcher proves the read is per-root, not per-home.
        expect(watchedB?.options.intervalMs).toBe(WATCH_INTERVAL_DEFAULT_MS);
        expect(watchedB?.options.dataDir).toBe(dataDirOf(box.repoB));

        // Three ticks happened and the count is still two: no duplicate watcher, no unregistered root.
        expect(ticks).toBeGreaterThanOrEqual(3);
        expect(watchers.opened).toHaveLength(2);

        // Case 4. A registered tree records; the tree nobody registered records nothing, whether or
        // not it is the one being served. Both runs are the shipped verb on the shipped entry.
        const registered = await spawnCli(["status", "--repo-root", box.repoA], box.home, box.home);
        expect(registered.code).toBe(0);
        expect(telemetryExists(box.repoA)).toBe(true);

        const adHoc = await spawnCli(["status", "--repo-root", box.loose], box.home, box.home);
        expect(adHoc.code).toBe(0);
        // The answer is that tree's own graph: its project line names it.
        expect(adHoc.stdout).toContain(box.loose);
        expect(telemetryExists(box.loose)).toBe(false);

        // Case 5. The abort is the stop: the loop ends, the server closes, the lock goes last.
        controller.abort();
        expect(await run).toBe(0);
        expect(existsSync(join(box.home, "daemon.pid"))).toBe(false);
        // The watcher each registered repo held was ended exactly once by that one shutdown.
        expect(watchers.opened.map((watcher) => watcher.stops())).toEqual([1, 1]);
      } finally {
        // A failed assertion above must still stop the loop and close the listener.
        controller.abort();
        await run.catch(() => 1);
        lines.restore();
      }
    },
    RUN_TIMEOUT_MS,
  );
});
