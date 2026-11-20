import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { LogFields, Logger } from "#shared/log";
import {
  RECONCILE_INTERVAL_MS,
  type ReconcileHandle,
  startReconcile,
  type WatcherOptions,
} from "../src/daemon/reconcile";
import type { WatchHandle, WatchStats } from "../src/indexer/watch";

/**
 * The reconciler's contract is a count and a decision per root, so every case drives real
 * `tick()` passes over a real registry and real `<root>/.sangkar/config.json` files, and injects
 * `openWatcher` so no watcher, no child process and no poll timer exists at all. Nothing here
 * sleeps: a fake handle has no loop, so a tick is the whole unit of work.
 *
 * The fixtures are `mkdtemp` directories under one temp home, removed in `afterAll`. `SANGKAR_HOME`
 * is pointed at the temp home anyway, so a home read that leaked out of `#shared/config.ts` would
 * resolve inside the fixture instead of into the developer's real installation.
 */

const dirs: string[] = [];

function tempDir(prefix: string): string {
  // `realpathSync` because `readRegistry` canonicalises every entry: the map's keys are canonical.
  const dir = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

interface Fixture {
  readonly home: string;
  readonly registryPath: string;
}

/** A temp home, with `SANGKAR_HOME` pinned to it so a leaked read lands in the fixture. */
function newHome(): Fixture {
  const home = tempDir("sangkar-reconcile-home-");
  process.env.SANGKAR_HOME = home;
  return { home, registryPath: join(home, "sangkar.json") };
}

/** A temp repository root, plus the config file the daemon reads for it. */
interface Repo {
  readonly root: string;
  configure(settings: { readonly watch?: boolean; readonly watch_interval?: number }): void;
}

function newRepo(
  settings: { readonly watch?: boolean; readonly watch_interval?: number } = {},
): Repo {
  const root = tempDir("sangkar-reconcile-repo-");
  const repo: Repo = {
    root,
    configure(next): void {
      const dataDir = join(root, ".sangkar");
      mkdirSync(dataDir, { recursive: true });
      writeFileSync(join(dataDir, "config.json"), JSON.stringify(next));
    },
  };
  if (Object.keys(settings).length > 0) {
    repo.configure(settings);
  }
  return repo;
}

/** Writes the registry in the shape `readRegistry` accepts: version 1 and absolute paths. */
function writeRegistry(fixture: Fixture, roots: readonly string[]): void {
  writeFileSync(fixture.registryPath, JSON.stringify({ version: 1, repos: roots }));
}

/** A handle with no loop behind it: `stop()` is counted and `stats()` is a fixed snapshot. */
interface FakeWatcher {
  readonly options: WatcherOptions;
  readonly handle: WatchHandle;
  readonly stops: () => number;
}

function fakeWatcher(options: WatcherOptions): FakeWatcher {
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
  return { options, handle, stops: () => stops };
}

interface WatcherSpy {
  readonly opened: FakeWatcher[];
  readonly openWatcher: (options: WatcherOptions) => WatchHandle;
}

/** Every creation is recorded in order, so "exactly once" and "restarted once" are both readable. */
function watcherSpy(): WatcherSpy {
  const opened: FakeWatcher[] = [];
  return {
    opened,
    openWatcher: (options: WatcherOptions): WatchHandle => {
      const watcher = fakeWatcher(options);
      opened.push(watcher);
      return watcher.handle;
    },
  };
}

interface Recorded {
  readonly level: string;
  readonly msg: string;
  readonly fields: LogFields;
}

/** A logger that keeps the lines, because "one deduped line" is a testable claim. */
function recorder(): { readonly logger: Logger; readonly lines: Recorded[] } {
  const lines: Recorded[] = [];
  const add =
    (level: string) =>
    (msg: string, fields?: LogFields): void => {
      lines.push({ level, msg, fields: fields ?? {} });
    };
  return {
    lines,
    logger: { debug: add("debug"), info: add("info"), warn: add("warn"), error: add("error") },
  };
}

interface Harness {
  readonly fixture: Fixture;
  readonly spy: WatcherSpy;
  readonly lines: Recorded[];
  readonly handle: ReconcileHandle;
}

/** One reconciler over `fixture`, with an injected watcher factory, clock and logger. */
function reconcileOver(
  fixture: Fixture,
  spy: WatcherSpy,
  extra: { readonly openWatcher?: (options: WatcherOptions) => WatchHandle } = {},
): Harness {
  const recorded = recorder();
  const handle = startReconcile({
    home: fixture.home,
    openWatcher: extra.openWatcher ?? spy.openWatcher,
    now: () => 1_700_000_000_000,
    log: recorded.logger,
  });
  return { fixture, spy, lines: recorded.lines, handle };
}

function countMsg(lines: readonly Recorded[], msg: string): number {
  return lines.filter((line) => line.msg === msg).length;
}

describe("startReconcile", () => {
  test("one tick opens exactly one watcher per watched repo and three ticks open no more", async () => {
    const fixture = newHome();
    const alpha = newRepo();
    const beta = newRepo({ watch_interval: 4_000 });
    writeRegistry(fixture, [alpha.root, beta.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    const first = await harness.handle.tick();
    expect(first.watched).toEqual([alpha.root, beta.root]);
    expect(first.started).toEqual([alpha.root, beta.root]);
    expect(first.stopped).toEqual([]);
    expect(first.missing).toEqual([]);
    expect(first.skipped).toEqual([]);
    expect(harness.spy.opened.length).toBe(2);

    // Every watcher was opened for its own repo, at its own interval, on `<root>/.sangkar`.
    const alphaWatcher = harness.spy.opened[0]?.options;
    const betaWatcher = harness.spy.opened[1]?.options;
    expect(alphaWatcher?.repoRoot).toBe(alpha.root);
    expect(alphaWatcher?.dataDir).toBe(join(alpha.root, ".sangkar"));
    // An unconfigured repo takes `DEFAULT_REPO_SETTINGS.watchIntervalMs`.
    expect(alphaWatcher?.intervalMs).toBe(2_000);
    expect(betaWatcher?.repoRoot).toBe(beta.root);
    expect(betaWatcher?.intervalMs).toBe(4_000);

    // Two further passes over an unchanged registry must reuse the handles, not replace them.
    await harness.handle.tick();
    const third = await harness.handle.tick();
    expect(harness.spy.opened.length).toBe(2);
    expect(third.started).toEqual([]);
    expect(third.stopped).toEqual([]);
    expect(third.watched).toEqual([alpha.root, beta.root]);
    expect([...harness.handle.stats().keys()]).toEqual([alpha.root, beta.root]);
    expect(harness.spy.opened.every((watcher) => watcher.stops() === 0)).toBe(true);

    await harness.handle.stop();
  });

  test("watch: false starts no watcher and is reported as skipped", async () => {
    const fixture = newHome();
    const quiet = newRepo({ watch: false });
    const loud = newRepo();
    writeRegistry(fixture, [quiet.root, loud.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    const report = await harness.handle.tick();
    expect(report.skipped).toEqual([quiet.root]);
    expect(report.watched).toEqual([loud.root]);
    expect(report.missing).toEqual([]);
    expect(harness.spy.opened.length).toBe(1);
    expect(harness.spy.opened[0]?.options.repoRoot).toBe(loud.root);

    await harness.handle.stop();
  });

  test("a missing root is reported once, logged once and starts nothing", async () => {
    const fixture = newHome();
    const gone = newRepo();
    rmSync(gone.root, { recursive: true, force: true });
    writeRegistry(fixture, [gone.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    const first = await harness.handle.tick();
    expect(first.missing).toEqual([gone.root]);
    expect(first.watched).toEqual([]);
    expect(first.started).toEqual([]);
    expect(harness.spy.opened.length).toBe(0);

    const second = await harness.handle.tick();
    expect(second.missing).toEqual([gone.root]);
    expect(harness.spy.opened.length).toBe(0);
    // One line for the transition, not one per tick: a deleted tree must not flood stderr.
    expect(countMsg(harness.lines, "registered repo is gone, not watching it")).toBe(1);

    await harness.handle.stop();
  });

  test("a raised watch_interval restarts only that repo and reports it as both started and stopped", async () => {
    const fixture = newHome();
    const alpha = newRepo();
    const beta = newRepo({ watch_interval: 2_000 });
    writeRegistry(fixture, [alpha.root, beta.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    await harness.handle.tick();
    expect(harness.spy.opened.length).toBe(2);
    const alphaFirst = harness.spy.opened[0];
    const betaFirst = harness.spy.opened[1];

    beta.configure({ watch_interval: 9_000 });
    const report = await harness.handle.tick();
    expect(report.stopped).toEqual([beta.root]);
    expect(report.started).toEqual([beta.root]);
    expect(report.watched).toEqual([alpha.root, beta.root]);
    expect(harness.spy.opened.length).toBe(3);
    expect(harness.spy.opened[2]?.options).toEqual({
      repoRoot: beta.root,
      dataDir: join(beta.root, ".sangkar"),
      intervalMs: 9_000,
    });
    // Only the retuned repo's watcher was ended; the untouched one keeps its baseline.
    expect(alphaFirst?.stops()).toBe(0);
    expect(betaFirst?.stops()).toBe(1);

    // A stable interval after the retune is not another restart.
    const settled = await harness.handle.tick();
    expect(settled.started).toEqual([]);
    expect(harness.spy.opened.length).toBe(3);

    await harness.handle.stop();
  });

  test("a repo dropped from sangkar.json has its watcher stopped and forgotten", async () => {
    const fixture = newHome();
    const alpha = newRepo();
    const beta = newRepo();
    writeRegistry(fixture, [alpha.root, beta.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    await harness.handle.tick();
    expect(harness.spy.opened.length).toBe(2);
    const betaWatcher = harness.spy.opened[1];

    writeRegistry(fixture, [alpha.root]);
    const report = await harness.handle.tick();
    expect(report.stopped).toEqual([beta.root]);
    expect(report.watched).toEqual([alpha.root]);
    expect(report.started).toEqual([]);
    expect(betaWatcher?.stops()).toBe(1);
    expect([...harness.handle.stats().keys()]).toEqual([alpha.root]);

    await harness.handle.stop();
  });

  test("stop() ends every watcher and a later tick() creates nothing", async () => {
    const fixture = newHome();
    const alpha = newRepo();
    const beta = newRepo();
    writeRegistry(fixture, [alpha.root, beta.root]);
    const harness = reconcileOver(fixture, watcherSpy());

    await harness.handle.tick();
    expect(harness.spy.opened.length).toBe(2);

    await harness.handle.stop();
    expect(harness.spy.opened.every((watcher) => watcher.stops() === 1)).toBe(true);
    // `stop()` is idempotent, so a second shutdown path cannot double-count.
    await harness.handle.stop();
    expect(harness.spy.opened.every((watcher) => watcher.stops() === 1)).toBe(true);

    const after = await harness.handle.tick();
    expect(after).toEqual({ watched: [], started: [], stopped: [], missing: [], skipped: [] });
    expect(harness.spy.opened.length).toBe(2);
    expect(harness.handle.stats().size).toBe(0);
  });

  test("a watcher that fails to open is skipped without stranding the other repos", async () => {
    const fixture = newHome();
    const broken = newRepo();
    const fine = newRepo();
    writeRegistry(fixture, [broken.root, fine.root]);
    const spy = watcherSpy();
    const harness = reconcileOver(fixture, spy, {
      openWatcher: (options: WatcherOptions): WatchHandle => {
        if (options.repoRoot === broken.root) {
          throw new Error("no watcher for this tree");
        }
        return spy.openWatcher(options);
      },
    });

    const report = await harness.handle.tick();
    expect(report.skipped).toEqual([broken.root]);
    expect(report.watched).toEqual([fine.root]);
    expect(spy.opened.length).toBe(1);
    expect(countMsg(harness.lines, "watcher could not be started")).toBe(1);

    await harness.handle.stop();
  });

  test("the module owns no timer: one tick is one pass and the interval is the daemon's", async () => {
    // §"exactly one scheduler": if this file grew a timer, `runDaemon`'s loop would double it.
    expect(RECONCILE_INTERVAL_MS).toBe(5_000);
    const source = readFileSync(resolve(import.meta.dir, "../src/daemon/reconcile.ts"), "utf8");
    expect(source).not.toMatch(/setInterval/);
    expect(source).not.toMatch(/setTimeout/);
  });
});
