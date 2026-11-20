/**
 * The reconciler: one pass over the registry that makes the live watchers match it.
 *
 * Why passivity: this module owns no timer. `runDaemon` already has one loop for the whole process,
 * and a second scheduler here would double the poll rate and give `stop()` two things to race
 * against. So `startReconcile` returns a handle whose `tick()` does one full reconciliation and
 * whose `stop()` ends every watcher; {@link RECONCILE_INTERVAL_MS} is the period the daemon's own
 * loop consumes, exported so the two cannot drift apart.
 *
 * Why the watcher map is keyed by root: the identity of a watcher is the tree it watches, so a
 * second entry for the same root would mean two child processes indexing one graph. Keeping the
 * interval next to the handle is what makes a retune a comparison instead of a re-read: a
 * registered repo that nobody touched keeps the handle it already has, so its baseline and its
 * failure streak survive the tick.
 *
 * The decision ladder, per registered root, in order:
 * - the directory is gone: report it `missing`, stop any watcher, log one line per transition —
 *   a tree that stays deleted must not write a warning every five seconds;
 * - `watch: false`: report it `skipped`, stop any watcher, same one-line-per-transition rule;
 * - `watch: true`: exactly one watcher, started for a new root or retuned (stop, then start) when
 *   `watchIntervalMs` changed, and left alone otherwise;
 * - no longer in `sangkar.json`: stopped and forgotten, because an unregistered tree is not ours.
 *
 * What this module never does: spawn a worker or read a registry entry's graph. The child, the
 * backoff and the exit-code ladder live in `../indexer/watch`, and `dataDir` is `<root>/.sangkar`
 * so the watcher and the worker always name the same graph.
 */
import { statSync } from "node:fs";
import { join } from "node:path";
import { type LogFields, type Logger, log } from "#shared/log";
import { readRegistry } from "#shared/registry";
import { readRepoConfig } from "#shared/repo-config";
import { type SpawnWorker, startWatch, type WatchHandle, type WatchStats } from "../indexer/watch";

/** The daemon's reconciliation period, consumed by `runDaemon`'s loop. This module has no timer. */
export const RECONCILE_INTERVAL_MS = 5_000;

/** Mirrors the private `DATA_DIR_NAME` in `#shared/config.ts`, which does not export it. */
const DATA_DIR_NAME = ".sangkar";

/**
 * The subset of `startWatch`'s options the daemon passes. Spelled out rather than imported so a
 * test can inject an `openWatcher` that records calls without building a `WatchOptions`, and so
 * this seam cannot grow a field that `watch.ts` does not accept.
 */
export interface WatcherOptions {
  /** The working tree to watch; the map's key, so it is always the registry's spelling. */
  readonly repoRoot: string;
  /** `<repoRoot>/.sangkar`, forwarded so the watcher and the worker target one graph. */
  readonly dataDir: string;
  /** The repo's own `watch_interval`, already clamped by `readRepoConfig`. */
  readonly intervalMs: number;
  /** `startWatch`'s spawn seam, forwarded unchanged; tests stub it, the daemon does not. */
  readonly spawn?: SpawnWorker;
  /** `startWatch`'s per-attempt notice, forwarded unchanged when a caller has one. */
  readonly onSpawn?: (event: unknown) => void;
}

export interface ReconcileOptions {
  /** The installation's home; `sangkar.json` sits directly in it. Never read from the environment. */
  readonly home: string;
  /** `startWatch`'s own spawn seam, forwarded to every watcher this module opens. */
  readonly spawn?: SpawnWorker;
  /** Defaults to `startWatch`; tests pass a spy so no real watcher and no real child exists. */
  readonly openWatcher?: (options: WatcherOptions) => WatchHandle;
  /** Injected clock, so a log line's timestamp is the caller's to control; defaults to `Date.now`. */
  readonly now?: () => number;
  readonly log?: Logger;
}

/** What one `tick()` changed. Every array is in registry order, so two ticks compare by `toEqual`. */
export interface ReconcileReport {
  /** Roots with a live watcher after this tick, which is the invariant the daemon relies on. */
  readonly watched: readonly string[];
  /** Watchers created by this tick, including the second half of a retune. */
  readonly started: readonly string[];
  /** Watchers ended by this tick: retunes, opt-outs, vanished roots and de-registrations. */
  readonly stopped: readonly string[];
  /** Registered roots whose directory is gone, so `status` can report them instead of watching. */
  readonly missing: readonly string[];
  /** Registered roots deliberately not watched: `watch: false`, or a watcher that would not open. */
  readonly skipped: readonly string[];
}

export interface ReconcileHandle {
  /** One reconciliation. Serialised against any tick already running, so maps never interleave. */
  tick(): Promise<ReconcileReport>;
  /** The watchers' own counters, keyed by root; a snapshot, never the live watchers. */
  stats(): ReadonlyMap<string, WatchStats>;
  /** Ends every watcher; a later `tick()` reports nothing and creates nothing. */
  stop(): Promise<void>;
}

/** One live watcher and the interval it was opened with, which is what a retune compares. */
interface Entry {
  readonly handle: WatchHandle;
  readonly intervalMs: number;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** The directory test and the existence test in one call, so there is no check-then-use window. */
function isDirectory(path: string): boolean {
  try {
    return statSync(path).isDirectory();
  } catch {
    return false;
  }
}

export function startReconcile(options: ReconcileOptions): ReconcileHandle {
  const logger = options.log ?? log;
  const openWatcher = options.openWatcher ?? startWatch;
  const now = options.now ?? Date.now;
  const registryPath = join(options.home, "sangkar.json");
  // `exactOptionalPropertyTypes` is on: a seam that is absent is left out, not passed as undefined.
  const spawnField: { readonly spawn?: SpawnWorker } =
    options.spawn === undefined ? {} : { spawn: options.spawn };

  const watchers = new Map<string, Entry>();

  /** Roots whose *current* refusal has already been logged, so a steady state stays silent. */
  const refused = new Set<string>();
  let stopped = false;
  /** The tail of the tick queue: one reconciliation at a time, in call order. */
  let queue: Promise<void> = Promise.resolve();

  function noteOnce(root: string, msg: string, fields: LogFields): void {
    if (refused.has(root)) {
      return;
    }
    refused.add(root);
    logger.warn(msg, { repoRoot: root, ...fields });
  }

  /** Ends `root`'s watcher, if it has one, and records the stop. The handle is idempotent too. */
  async function end(root: string, stoppedRoots: string[]): Promise<void> {
    const entry = watchers.get(root);
    if (entry === undefined) {
      return;
    }
    watchers.delete(root);
    stoppedRoots.push(root);
    try {
      await entry.handle.stop();
    } catch (error) {
      // A watcher that fails to end is already gone from the map, so it cannot hold the slot; the
      // failure is the child's business and saying so is all this level can do.
      logger.warn("watcher.stop_failed", { repoRoot: root, message: messageOf(error) });
    }
  }

  async function reconcile(): Promise<ReconcileReport> {
    if (stopped) {
      return { watched: [], started: [], stopped: [], missing: [], skipped: [] };
    }
    const at = now();
    const registry = readRegistry(registryPath, { log: logger });
    const started: string[] = [];
    const stoppedRoots: string[] = [];
    const missing: string[] = [];
    const skipped: string[] = [];
    const seen = new Set<string>();

    for (const root of registry.repos) {
      // A `stop()` that lands mid-tick wins: the rest of this pass must not open a watcher that
      // nothing would ever end.
      if (stopped) {
        break;
      }
      seen.add(root);
      const dataDir = join(root, DATA_DIR_NAME);

      if (!isDirectory(root)) {
        missing.push(root);
        noteOnce(root, "registered repo is gone, not watching it", { at });
        await end(root, stoppedRoots);
        continue;
      }

      const config = readRepoConfig(root, { dataDir, log: logger });
      if (!config.watch) {
        skipped.push(root);
        noteOnce(root, "repo is configured with watch: false, not watching it", {
          at,
          watchIntervalMs: config.watchIntervalMs,
        });
        await end(root, stoppedRoots);
        continue;
      }

      refused.delete(root);
      const existing = watchers.get(root);
      if (existing !== undefined && existing.intervalMs === config.watchIntervalMs) {
        // Unchanged: the running watcher keeps its baseline and its failure streak.
        continue;
      }
      if (existing !== undefined) {
        // A retune restarts only this repo, and reports it in both arrays because both happened.
        logger.info("watcher retuned", {
          repoRoot: root,
          fromMs: existing.intervalMs,
          toMs: config.watchIntervalMs,
        });
        await end(root, stoppedRoots);
      }

      try {
        const handle = openWatcher({
          repoRoot: root,
          dataDir,
          intervalMs: config.watchIntervalMs,
          ...spawnField,
        });
        watchers.set(root, { handle, intervalMs: config.watchIntervalMs });
        started.push(root);
      } catch (error) {
        // One tree that cannot be watched is not a reason to stop serving the others.
        skipped.push(root);
        logger.error("watcher could not be started", {
          repoRoot: root,
          dataDir,
          message: messageOf(error),
        });
      }
    }

    for (const root of [...watchers.keys()]) {
      if (seen.has(root)) {
        continue;
      }
      logger.info("repo is no longer registered, watcher stopped", { repoRoot: root, at });
      await end(root, stoppedRoots);
    }

    return {
      watched: [...watchers.keys()],
      started,
      stopped: stoppedRoots,
      missing,
      skipped,
    };
  }

  function tick(): Promise<ReconcileReport> {
    // Queued rather than concurrent: two overlapping passes would both see "no watcher for root"
    // and open two. The queue's own failures are swallowed here and surface on the caller's tick.
    const result = queue.then(reconcile, reconcile);
    queue = result.then(
      () => undefined,
      () => undefined,
    );
    return result;
  }

  async function stop(): Promise<void> {
    stopped = true;
    // A pass already running may be between a decision and its open call; let it finish first.
    await queue;
    // Snapshot before clearing: the map is what a concurrent `stats()` reads, and emptying it
    // first means a later `tick()` can never find a handle this pass is still stopping.
    const entries = [...watchers.entries()];
    watchers.clear();
    refused.clear();
    await Promise.all(
      entries.map(async ([root, entry]) => {
        try {
          await entry.handle.stop();
        } catch (error) {
          logger.warn("watcher.stop_failed", { repoRoot: root, message: messageOf(error) });
        }
      }),
    );
  }

  return {
    tick,
    stats(): ReadonlyMap<string, WatchStats> {
      const snapshot = new Map<string, WatchStats>();
      for (const [root, entry] of watchers) {
        snapshot.set(root, entry.handle.stats());
      }
      return snapshot;
    },
    stop,
  };
}
