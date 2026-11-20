import { afterAll, describe, expect, spyOn, test } from "bun:test";
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
import { basename, join } from "node:path";
import { isAppError } from "#shared/errors";
import { type LogFields, log } from "#shared/log";
import { readPidLock } from "#shared/pidlock";
import { writeRegistry } from "#shared/registry";
import { type ReportServer, serveReport } from "#shared/telemetry/serve";
import { runDaemonForeground } from "../src/daemon/main";
import type { WatcherOptions } from "../src/daemon/reconcile";
import type { SpawnWorker, WatchHandle, WatchStats } from "../src/indexer/watch";

/**
 * The composition root, driven end to end: a real pid file, a real loopback listener and a real
 * registry, with only the two expensive seams injected — `openWatcher` (so no watcher process
 * exists) and `spawn` (so no worker is ever started). Every fixture is a `mkdtemp` directory under
 * the temp dir, removed in `afterAll`.
 *
 * Nothing sleeps on a poll: the daemon's tick is `intervalMs: 5` and the `onTick` seam resolves a
 * promise, so a case waits for a counted number of passes instead of a duration. Every server a
 * case starts is closed by the call under test, which is the last thing each case proves.
 *
 * The two files a daemon owns are read back through the same modules the daemon uses
 * (`readPidLock`, `readFileSync`) rather than through a hand-rolled parse, so "the pid file is
 * gone" means what `daemon status` would mean.
 */

const dirs: string[] = [];
let previousHome: string | undefined;

function tempDir(prefix: string): string {
  // Canonical, because `readRegistry` canonicalises every entry: the watcher's key is the root.
  const dir = realpathSync(mkdtempSync(join(tmpdir(), prefix)));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
  if (previousHome === undefined) {
    delete process.env.SANGKAR_HOME;
  } else {
    process.env.SANGKAR_HOME = previousHome;
  }
});

/** A temp home with `SANGKAR_HOME` pinned to it, so a home read that leaked lands in the fixture. */
function newHome(): string {
  if (previousHome === undefined) {
    previousHome = process.env.SANGKAR_HOME;
  }
  const home = tempDir("sangkar-daemon-main-home-");
  process.env.SANGKAR_HOME = home;
  return home;
}

/** A temp repository root: an existing directory plus the per-repo config the reconciler reads. */
function newRepo(): string {
  const root = tempDir("sangkar-daemon-main-repo-");
  mkdirSync(join(root, ".sangkar"), { recursive: true });
  writeFileSync(
    join(root, ".sangkar", "config.json"),
    JSON.stringify({ watch: true, watch_interval: 1000 }),
  );
  return root;
}

/** The registry as `readRegistry` accepts it: version 1, canonical absolute roots. */
function register(home: string, roots: readonly string[]): void {
  writeRegistry(join(home, "sangkar.json"), { version: 1, repos: roots });
}

/** The pid file the daemon takes inside its home, spelled the way the daemon spells it. */
function pidPath(home: string): string {
  return join(home, "daemon.pid");
}

/** What `readPidLock` answers for an absent file: no holder, nothing stale, nothing to report. */
const ABSENT = { held: false, pid: null, stale: false, startedAt: null } as const;

function deferred(): { readonly promise: Promise<void>; readonly resolve: () => void } {
  let settle = (): void => {};
  const promise = new Promise<void>((resolve) => {
    settle = resolve;
  });
  return { promise, resolve: () => settle() };
}

/**
 * A watcher factory that records every open and every stop, so "exactly one watcher" and "the
 * shutdown stopped it" are counts rather than inferences. The handle has no loop behind it.
 */
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
 * The spawn seam, injected but expected to stay unused: the composition root forwards it to the
 * reconciler, and the reconciler hands it to a watcher that this file never opens. Recording the
 * call and throwing makes an accidental spawn a visible failure instead of a child process.
 */
const spawns: (readonly string[])[] = [];
const spawn: SpawnWorker = (cmd) => {
  spawns.push(cmd);
  throw new Error("the composition root must not spawn a worker itself");
};

/** One `runDaemonForeground` call with the seams this file always injects. */
function start(
  home: string,
  watchers: WatcherSpy,
  extra: {
    readonly signal: AbortSignal;
    readonly onTick?: () => Promise<void>;
    readonly port?: number;
  },
): Promise<number> {
  return runDaemonForeground({
    home,
    port: extra.port ?? 0,
    spawn,
    openWatcher: watchers.openWatcher,
    intervalMs: 5,
    signal: extra.signal,
    ...(extra.onTick === undefined ? {} : { onTick: extra.onTick }),
  });
}

interface Recorded {
  readonly msg: string;
  readonly fields: LogFields;
}

/**
 * The daemon's own diagnostics, captured instead of printed. `daemon.ready` is the only place the
 * bound url exists when the caller asked for an ephemeral port, so reading it back is the honest
 * way to fetch the page the process is actually serving.
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

function readyLine(recorded: readonly Recorded[]): Recorded | undefined {
  return recorded.find((entry) => entry.msg === "daemon.ready");
}

function readyUrl(recorded: readonly Recorded[]): string {
  const url = readyLine(recorded)?.fields.url;
  return typeof url === "string" ? url : "";
}

describe("runDaemonForeground", () => {
  test("serves the report, keeps one watcher over many ticks, and leaves nothing behind on stop", async () => {
    const home = newHome();
    const repo = newRepo();
    register(home, [repo]);

    const watchers = watcherSpy();
    const lines = recordLog();
    const controller = new AbortController();
    const threeTicks = deferred();
    let ticks = 0;

    const run = start(home, watchers, {
      signal: controller.signal,
      onTick: async (): Promise<void> => {
        ticks += 1;
        if (ticks >= 3) {
          threeTicks.resolve();
        }
      },
    });

    try {
      // Three ticks prove the daemon's own loop is driving the reconciler, not a first pass alone.
      await threeTicks.promise;

      // The lock is held while the process serves: this is what a second start collides with.
      expect(existsSync(pidPath(home))).toBe(true);

      // The ready line names the home and the ephemeral port that was actually bound.
      const ready = readyLine(lines.recorded);
      expect(ready?.fields.home).toBe(home);
      expect(typeof ready?.fields.port).toBe("number");
      const url = readyUrl(lines.recorded);
      expect(url).toStartWith("http://127.0.0.1:");

      const response = await fetch(url);
      expect(response.status).toBe(200);
      expect(response.headers.get("content-type")).toContain("text/html");
      const html = await response.text();
      expect(html).toContain("<title>sangkar telemetry report</title>");
      // One section per registered repo, labelled with its basename, so the page is this home's.
      expect(html).toContain(basename(repo));

      // Each tick logs the counts it saw; the reconciler's own report is what they come from.
      const ticksLogged = lines.recorded.filter((entry) => entry.msg === "daemon.tick");
      expect(ticksLogged.length).toBeGreaterThanOrEqual(2);
      expect(ticksLogged[0]?.fields).toEqual({ watched: 1, missing: 0 });

      // Ticks never multiplied the watcher, and no worker was ever spawned for one.
      expect(watchers.opened.length).toBe(1);
      expect(watchers.opened[0]?.options.repoRoot).toBe(repo);
      expect(watchers.opened[0]?.options.dataDir).toBe(join(repo, ".sangkar"));
      expect(watchers.opened[0]?.options.intervalMs).toBe(1000);
      expect(spawns).toEqual([]);

      controller.abort();
      expect(await run).toBe(0);

      // The same call that started the server stopped it: the port refuses connections now.
      await expect(fetch(url)).rejects.toThrow();
    } finally {
      controller.abort();
      await run.catch(() => 1);
      lines.restore();
    }

    // The lock is released last, so it is gone once the call has returned.
    expect(await readPidLock(pidPath(home))).toEqual(ABSENT);
    expect(existsSync(pidPath(home))).toBe(false);
    // And the watcher the reconciler opened was stopped exactly once by the shutdown.
    expect(watchers.opened.map((watcher) => watcher.stops())).toEqual([1]);
  });

  test("a second daemon on the same home is refused with index_locked and the first keeps serving", async () => {
    const home = newHome();
    const repo = newRepo();
    register(home, [repo]);

    const watchers = watcherSpy();
    const lines = recordLog();
    const controller = new AbortController();
    const firstTick = deferred();

    const first = start(home, watchers, {
      signal: controller.signal,
      onTick: async (): Promise<void> => {
        firstTick.resolve();
      },
    });

    let refusal: unknown;
    const secondWatchers = watcherSpy();
    try {
      await firstTick.promise;
      const before = readFileSync(pidPath(home), "utf8");
      const url = readyUrl(lines.recorded);

      try {
        await start(home, secondWatchers, { signal: new AbortController().signal });
      } catch (error) {
        refusal = error;
      }

      // A refused start is an `index_locked` error, decided before anything can be started.
      expect(isAppError(refusal)).toBe(true);
      expect(isAppError(refusal) ? refusal.code : "").toBe("index_locked");
      expect(secondWatchers.opened.length).toBe(0);
      expect(spawns).toEqual([]);

      // The pid file still belongs to the first call: the same bytes, still held by this process.
      expect(readFileSync(pidPath(home), "utf8")).toBe(before);
      const state = await readPidLock(pidPath(home));
      expect(state.held).toBe(true);
      expect(state.pid).toBe(process.pid);

      // And the refused call left the first one serving.
      const response = await fetch(url);
      expect(response.status).toBe(200);

      controller.abort();
      expect(await first).toBe(0);
    } finally {
      controller.abort();
      await first.catch(() => 1);
      lines.restore();
    }

    // Only the owner released: after the first call returned, the home is free.
    expect(await readPidLock(pidPath(home))).toEqual(ABSENT);
  });

  test("a port already in use rejects the start and leaves no pid file, no watcher and a usable home", async () => {
    const home = newHome();
    const repo = newRepo();
    register(home, [repo]);

    // The other listener: a real report server on an ephemeral port, closed when the case ends.
    const holder: ReportServer = await serveReport({ home, port: 0 });
    const watchers = watcherSpy();
    let ticks = 0;
    let failure: unknown;

    try {
      await start(home, watchers, {
        signal: new AbortController().signal,
        port: holder.port,
        onTick: async (): Promise<void> => {
          ticks += 1;
        },
      });
    } catch (error) {
      failure = error;
    } finally {
      await holder.close();
    }

    // The bind is what failed, and it surfaced rather than being swallowed into a running daemon.
    expect(failure).toBeInstanceOf(Error);
    expect((failure as { code?: string }).code).toBe("EADDRINUSE");

    // The `finally` ran even though `server` never existed: no pid file, so the next start is not
    // blocked by a lock nobody holds.
    expect(existsSync(pidPath(home))).toBe(false);
    expect(await readPidLock(pidPath(home))).toEqual(ABSENT);

    // Nothing was ever watched, because the loop only starts after the bind succeeds — so the
    // stopped reconciler had nothing to end, and the shutdown still ran in order.
    expect(watchers.opened.length).toBe(0);
    expect(ticks).toBe(0);

    // The home is usable again: the failure left no lock, no listener and no timer behind.
    const retryWatchers = watcherSpy();
    const controller = new AbortController();
    const oneTick = deferred();
    const retry = start(home, retryWatchers, {
      signal: controller.signal,
      onTick: async (): Promise<void> => {
        oneTick.resolve();
      },
    });
    try {
      await oneTick.promise;
      expect(retryWatchers.opened.length).toBe(1);
      controller.abort();
      expect(await retry).toBe(0);
    } finally {
      controller.abort();
      await retry.catch(() => 1);
    }

    expect(await readPidLock(pidPath(home))).toEqual(ABSENT);
    expect(retryWatchers.opened.map((watcher) => watcher.stops())).toEqual([1]);
  });
});
