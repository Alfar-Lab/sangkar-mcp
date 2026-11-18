import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, writeFileSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import {
  backoffMs,
  DEFAULT_DEBOUNCE_MS,
  DEFAULT_INTERVAL_MS,
  MAX_BACKOFF_MS,
  type SpawnTarget,
  type SpawnWorker,
  startWatch,
  type WatchHandle,
} from "../src/indexer/watch";

/**
 * The watcher's contract is timing and process behaviour, so every test drives the real loop with
 * short intervals and a stubbed spawn: `startWatch` decides *when*, and a stub is the only way to
 * assert "exactly one spawn" without paying for an index run. No test may sleep for seconds, so
 * every wait is a bounded poll — `until` fails loudly instead of hanging.
 *
 * The fixtures are `mkdtemp` directories. Most are deliberately not git checkouts: that is the
 * degraded mtime detector, which is also the cheapest way to produce a change (touch a file). One
 * test builds a real checkout to prove the git detector is what answers there, and that a bare
 * `touch` is not a change.
 */

const dirs: string[] = [];
const watched: WatchHandle[] = [];

afterEach(async () => {
  for (const handle of watched.splice(0)) {
    await handle.stop();
  }
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A timer that never keeps the test process alive. */
function sleep(ms: number): Promise<void> {
  return new Promise((resolveSleep) => {
    const handle = setTimeout(resolveSleep, ms);
    handle.unref();
  });
}

/** Polls `condition` every couple of milliseconds, and fails the test rather than hanging. */
async function until(condition: () => boolean, label: string, timeoutMs = 3_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!condition()) {
    if (Date.now() > deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${label}`);
    }
    await sleep(2);
  }
}

/** Fails the test if `promise` has not settled inside `ms`; the timer is unref'd. */
async function within(promise: Promise<void>, ms: number, label: string): Promise<void> {
  let expired = false;
  const deadline = sleep(ms).then(() => {
    expired = true;
  });
  await Promise.race([promise, deadline]);
  if (expired) {
    throw new Error(`${label} did not settle within ${ms} ms`);
  }
}

interface Fixture {
  readonly repoRoot: string;
  readonly dataDir: string;
}

/** A temp working tree, plus the data dir the product would use — outside the tree by default. */
async function fixture(files: Record<string, string> = {}): Promise<Fixture> {
  const repoRoot = await mkdtemp(join(tmpdir(), "sangkar-watch-"));
  dirs.push(repoRoot);
  const sources: Record<string, string> = { "app.py": "def run():\n    return 1\n", ...files };
  for (const [path, content] of Object.entries(sources)) {
    const absolute = join(repoRoot, path);
    await mkdir(dirname(absolute), { recursive: true });
    await writeFile(absolute, content, "utf8");
  }
  return { repoRoot, dataDir: join(repoRoot, ".sangkar") };
}

interface Recorded {
  readonly cmd: readonly string[];
  readonly target: SpawnTarget;
  /** When the spawn was attempted, so a test can measure the debounce and the backoff. */
  readonly at: number;
}

interface Stub {
  readonly spawn: SpawnWorker;
  readonly calls: Recorded[];
}

/** A spawn stub that reports `codeFor(attempt)` and records every call. */
function stubSpawn(codeFor: (attempt: number) => number): Stub {
  const calls: Recorded[] = [];
  return {
    calls,
    spawn: (cmd, target) => {
      calls.push({ cmd, target, at: Date.now() });
      return { exited: Promise.resolve(codeFor(calls.length)), kill: () => undefined };
    },
  };
}

function watchFixture(
  fixtureIn: Fixture,
  stub: Stub,
  timings: { intervalMs: number; debounceMs: number },
): WatchHandle {
  const handle = startWatch({
    repoRoot: fixtureIn.repoRoot,
    dataDir: fixtureIn.dataDir,
    intervalMs: timings.intervalMs,
    debounceMs: timings.debounceMs,
    spawn: stub.spawn,
  });
  watched.push(handle);
  return handle;
}

/** Waits until the loop has taken its first observation, which is the baseline. */
async function baselineCaptured(handle: WatchHandle): Promise<void> {
  await until(() => handle.stats().polls >= 1, "the first poll (baseline)");
  expect(handle.stats().spawns).toBe(0);
}

function git(root: string, ...args: readonly string[]): void {
  const result = Bun.spawnSync({
    cmd: ["git", "--no-pager", "-C", root, ...args],
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")} failed: ${result.stderr.toString()}`);
  }
}

function commit(root: string, message: string): void {
  git(
    root,
    "-c",
    "user.name=watch fixture",
    "-c",
    "user.email=watch@example.invalid",
    "-c",
    "commit.gpgsign=false",
    "commit",
    "-q",
    "-m",
    message,
  );
}

describe("startWatch", () => {
  test("the defaults are the documented 2 s poll and 500 ms debounce", () => {
    expect(DEFAULT_INTERVAL_MS).toBe(2_000);
    expect(DEFAULT_DEBOUNCE_MS).toBe(500);
  });

  test("one file change spawns the worker exactly once, after the debounce window", async () => {
    const repo = await fixture();
    const stub = stubSpawn(() => 0);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 60 });

    await baselineCaptured(handle);
    expect(stub.calls.length).toBe(0);

    const changedAt = Date.now();
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");

    await until(() => stub.calls.length === 1, "the spawn caused by the change");
    const call = stub.calls[0];
    if (call === undefined) {
      throw new Error("no spawn recorded");
    }
    // The quiet window elapsed before the spawn: that is the debounce, not the poll interval.
    expect(call.at - changedAt).toBeGreaterThanOrEqual(60);
    // The change was read through mtimes, because a plain temp directory is not a checkout.
    expect(handle.stats().detector).toBe("mtime");
    expect(call.target).toEqual({ repoRoot: repo.repoRoot, dataDir: repo.dataDir });
    // The child is the CLI's worker mode: `bun run bin/sangkar.ts worker`.
    expect(call.cmd[1]).toBe("run");
    expect(call.cmd[2]?.endsWith("bin/sangkar.ts")).toBe(true);
    expect(call.cmd[3]).toBe("worker");

    // The successful run adopts the observed state as the baseline, so nothing repeats.
    await sleep(140);
    expect(stub.calls.length).toBe(1);
    expect(handle.stats().lastExitCode).toBe(0);
    expect(handle.stats().failures).toBe(0);
  });

  test("an unchanged tree spawns nothing", async () => {
    const repo = await fixture();
    const stub = stubSpawn(() => 0);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 20 });

    await baselineCaptured(handle);
    await until(() => handle.stats().polls >= 5, "five polls");
    expect(stub.calls.length).toBe(0);
  });

  test("a burst of writes inside the debounce window still spawns once", async () => {
    const repo = await fixture();
    const stub = stubSpawn(() => 0);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 120 });

    await baselineCaptured(handle);
    const pollsBefore = handle.stats().polls;

    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");
    // The change is detected on the next poll, which puts the loop inside the quiet window.
    await until(() => handle.stats().polls > pollsBefore, "detection of the first write");
    await sleep(5);
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 3\n", "utf8");

    await until(() => stub.calls.length >= 1, "the single spawn");
    await sleep(200);
    // The re-check after the window saw the second write, so one spawn covers both.
    expect(stub.calls.length).toBe(1);
  });

  test("a 75 (lock held) is not a failure and is retried on the next tick", async () => {
    const repo = await fixture();
    const stub = stubSpawn(() => 75);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 10 });

    await baselineCaptured(handle);
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");

    await until(() => stub.calls.length >= 2, "a second attempt after the locked exit");
    const stats = handle.stats();
    expect(stats.failures).toBe(0);
    expect(stats.backoffMs).toBe(0);
    expect(stats.lastExitCode).toBe(75);
  });

  test("a failing run backs off, doubling to the 300 s cap", async () => {
    // The rule itself, without waiting five minutes for the cap to be reached for real.
    expect(backoffMs(20, 0)).toBe(0);
    expect(backoffMs(20, 1)).toBe(40);
    expect(backoffMs(20, 2)).toBe(80);
    expect(backoffMs(20, 3)).toBe(160);
    expect(backoffMs(1_000, 8)).toBe(256_000);
    expect(backoffMs(1_000, 9)).toBe(MAX_BACKOFF_MS);
    expect(backoffMs(1_000, 40)).toBe(MAX_BACKOFF_MS);

    // And the loop applies it: the second attempt waits for the doubled delay, not the interval.
    const repo = await fixture();
    const stub = stubSpawn(() => 1);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 10 });

    await baselineCaptured(handle);
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");

    await until(() => handle.stats().failures === 2, "two failed runs");
    const [first, second] = stub.calls;
    if (first === undefined || second === undefined) {
      throw new Error("expected two spawn attempts");
    }
    expect(handle.stats().backoffMs).toBe(80);
    expect(second.at - first.at).toBeGreaterThanOrEqual(30);
    // The failed state is retried, never dropped: the baseline was not advanced.
    await until(() => (stub.calls.length ?? 0) >= 3, "a third attempt after the backoff");
  });

  test("stop() kills a running child and resolves", async () => {
    const repo = await fixture();
    let killed = false;
    let settleChild: ((code: number) => void) | null = null;
    const calls: number[] = [];
    const spawn: SpawnWorker = () => {
      calls.push(Date.now());
      return {
        exited: new Promise<number>((resolveExit) => {
          settleChild = resolveExit;
        }),
        kill: () => {
          killed = true;
          // A killed child still reports a code; `stop()` must have been the reason.
          settleChild?.(143);
        },
      };
    };

    const handle = startWatch({
      repoRoot: repo.repoRoot,
      dataDir: repo.dataDir,
      intervalMs: 20,
      debounceMs: 10,
      spawn,
    });
    watched.push(handle);

    await baselineCaptured(handle);
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");
    await until(() => calls.length === 1, "the spawn whose child never exits on its own");

    const stopping = handle.stop();
    await within(stopping, 1_000, "stop()");
    expect(killed).toBe(true);
    // The exit code of the child we killed was awaited, so no zombie is left behind.
    expect(handle.stats().lastExitCode).toBe(143);
    // A killed child is not a failure: the repository said nothing about itself.
    expect(handle.stats().failures).toBe(0);

    const spawnsAfterStop = handle.stats().spawns;
    await sleep(120);
    expect(handle.stats().spawns).toBe(spawnsAfterStop);
    // `stop()` is idempotent, so a second signal handler cannot double-kill.
    await within(handle.stop(), 100, "the second stop()");
  });

  test("a data dir inside the tree is never watched, so the worker cannot feed itself", async () => {
    // The worker writes `graph.db` and its lock into the data dir; if that dir were part of the
    // signature, every run would look like a change and spawn the next one.
    const repo = await fixture();
    const dataDir = join(repo.repoRoot, "graphs");
    const spawn: SpawnWorker = () => {
      // Stands in for a run that publishes: the data dir gains a file with a parseable extension,
      // which is the worst case for the guard.
      mkdirSync(dataDir, { recursive: true });
      writeFileSync(join(dataDir, "bleed.py"), "value = 1\n");
      return { exited: Promise.resolve(0), kill: () => undefined };
    };

    const handle = startWatch({
      repoRoot: repo.repoRoot,
      dataDir,
      intervalMs: 20,
      debounceMs: 10,
      spawn,
    });
    watched.push(handle);

    await baselineCaptured(handle);
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 2\n", "utf8");
    await until(() => handle.stats().spawns === 1, "the first spawn");
    await sleep(200);
    expect(handle.stats().spawns).toBe(1);
  });

  test("a git checkout is watched through HEAD and porcelain, so a touch is not a change", async () => {
    const repo = await fixture();
    git(repo.repoRoot, "init", "-q");
    git(repo.repoRoot, "add", "-A");
    commit(repo.repoRoot, "fixture");

    const stub = stubSpawn(() => 0);
    const handle = watchFixture(repo, stub, { intervalMs: 20, debounceMs: 20 });

    await baselineCaptured(handle);
    expect(handle.stats().detector).toBe("git");

    // Same bytes, new mtime: porcelain stays clean, so the graph already covers this state.
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 1\n", "utf8");
    await until(() => handle.stats().polls >= 3, "three polls after the touch");
    expect(stub.calls.length).toBe(0);

    // A real edit does change the status, and is spawned once.
    await writeFile(join(repo.repoRoot, "app.py"), "def run():\n    return 42\n", "utf8");
    await until(() => stub.calls.length === 1, "the spawn caused by the dirty status");
    await sleep(150);
    expect(stub.calls.length).toBe(1);

    // Committing moves HEAD, which changes the signature without touching a working-tree byte.
    git(repo.repoRoot, "add", "-A");
    commit(repo.repoRoot, "change");
    await until(() => stub.calls.length === 2, "the spawn caused by the new HEAD");
    await sleep(150);
    expect(stub.calls.length).toBe(2);
    // The data dir keeps the checkout clean, so a worker run cannot feed itself back to the loop.
    expect(resolve(repo.dataDir)).toBe(join(repo.repoRoot, ".sangkar"));
  });
});
