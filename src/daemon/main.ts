/**
 * The daemon's composition root: the one process an operator starts.
 *
 * Three slices, each already correct on its own, are joined here and nowhere else — the singleton
 * (`#shared/pidlock`), the reconciler (`./reconcile`) and the report server
 * (`#shared/telemetry/serve`). Keeping the join in a single small module is what makes the order of
 * the three operations reviewable: an operator gets one process that holds the lock, keeps one
 * watcher per registered repository and serves the report, or gets an error and no leftovers.
 *
 * The order is the contract:
 *
 * 1. **The lock first, outside the `try`.** An `index_locked` refusal therefore has nothing to undo:
 *    no watcher was started, no port was bound, and the pid file belongs to the daemon that is still
 *    running, so this call must not remove it.
 * 2. **The reconciler next.** It is passive — it owns no timer — so starting it opens nothing.
 * 3. **The server third, into a nullable handle**, so a bind failure still reaches the `finally`
 *    below with `server === null` and releases the lock.
 * 4. **The loop last.** `runDaemon` adopts the lock we hold and never releases it, which is why the
 *    release lives here, exactly once, in the `finally`. The loop calls `reconcile.tick()` once per
 *    interval: one scheduler for the whole process, never a second one inside the reconciler.
 *
 * What this module is not: it holds no MCP surface, opens no database, imports nothing from the
 * persistence layer (`#shared/store`), and reads no environment variable. `home` arrives as an
 * argument from the one module allowed to read the environment (`#shared/config.ts`), and every
 * diagnostic goes to `#shared/log` on stderr, because stdout is the MCP protocol.
 */
import { join } from "node:path";
import { log } from "#shared/log";
import { acquirePidLock, releasePidLock } from "#shared/pidlock";
import { type ReportServer, serveReport } from "#shared/telemetry/serve";
import type { SpawnWorker, WatchHandle } from "../indexer/watch";
import { runDaemon } from "./lifecycle";
import { RECONCILE_INTERVAL_MS, startReconcile, type WatcherOptions } from "./reconcile";

/** The pid file, spelled as `#shared/config.ts` and `./lifecycle` spell it. */
const PID_FILE = "daemon.pid";

/** Everything the foreground daemon needs. Every seam is optional; production passes only `home`. */
export interface ForegroundOptions {
  /** The machine-level home: `daemon.pid`, `sangkar.json` and the report live in it. */
  readonly home: string;
  /** `0` asks the kernel for a port, which is how a test reads the bound one back. */
  readonly port?: number;
  /** Loopback only; `serveReport` refuses anything else before it binds. */
  readonly host?: string;
  /** `startWatch`'s spawn seam, forwarded unchanged; tests stub it, the daemon does not. */
  readonly spawn?: SpawnWorker;
  /** `startReconcile`'s watcher factory; tests pass a spy so no watcher or child exists. */
  readonly openWatcher?: (options: WatcherOptions) => WatchHandle;
  /** Daemon tick period; defaults to {@link RECONCILE_INTERVAL_MS}, the reconciler's own period. */
  readonly intervalMs?: number;
  /** Called after each reconcile tick, so a test can count passes without polling a clock. */
  readonly onTick?: () => Promise<void>;
  /** The report's clock, forwarded so a rendered page's timestamp is the caller's to control. */
  readonly now?: () => Date;
  /** Test and caller seam: an abort stops the loop, the server and the watchers, and resolves 0. */
  readonly signal?: AbortSignal;
}

/**
 * Runs the whole daemon in the foreground and resolves 0 when it is stopped.
 *
 * `exactOptionalPropertyTypes` is on, so an absent seam is left out of the option objects rather
 * than passed as `undefined` — the same rule `./reconcile` follows at its own seams.
 */
export async function runDaemonForeground(options: ForegroundOptions): Promise<number> {
  // First, and before the `try`: a refused start must leave this call with nothing to undo.
  const lock = await acquirePidLock(join(options.home, PID_FILE));

  const reconcile = startReconcile({
    home: options.home,
    ...(options.spawn === undefined ? {} : { spawn: options.spawn }),
    ...(options.openWatcher === undefined ? {} : { openWatcher: options.openWatcher }),
  });

  let server: ReportServer | null = null;
  try {
    server = await serveReport({
      home: options.home,
      ...(options.port === undefined ? {} : { port: options.port }),
      ...(options.host === undefined ? {} : { host: options.host }),
      ...(options.now === undefined ? {} : { now: options.now }),
    });
    // The ready line carries the bound url, which is the only way `port: 0` is read back.
    log.info("daemon.ready", { home: options.home, url: server.url, port: server.port });

    return await runDaemon({
      home: options.home,
      lock,
      intervalMs: options.intervalMs ?? RECONCILE_INTERVAL_MS,
      log,
      ...(options.signal === undefined ? {} : { signal: options.signal }),
      onTick: async (): Promise<void> => {
        const report = await reconcile.tick();
        await options.onTick?.();
        log.info("daemon.tick", {
          watched: report.watched.length,
          missing: report.missing.length,
        });
      },
    });
  } finally {
    // One shutdown, in the order a stop has to happen: stop opening work, close what is serving,
    // and only then drop the lock — so nobody can take the home while this process still holds a
    // watcher or a socket. The nested `try` keeps the release unconditional: a bind failure or a
    // close that throws must not leave a pid file that makes the next start look locked.
    try {
      await reconcile.stop();
      await server?.close();
    } finally {
      await releasePidLock(lock);
    }
  }
}
