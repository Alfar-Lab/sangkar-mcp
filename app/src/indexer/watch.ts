/**
 * The watcher: it decides *when* an index run is due and never runs one itself.
 *
 * Why a poll and not `fs.watch`: TECH-SPEC §8 picks a portable poll of `git HEAD` + `git status`
 * because inotify and FSEvents differ per platform, and because a `git checkout`, `stash` or
 * `rebase` rewrites the tree through a path that a watch event may not describe usefully. One
 * string per observation — the "signature" — and a difference is the only reason to spawn.
 *
 * Why the worker is a child and never an import: the writer is one process (TECH-SPEC §8). A
 * parser panic in a WASM grammar has to kill a child, not the MCP session, and the write lock is
 * only meaningful if a second process can contend for it. So this module reads the tree and spawns
 * `bin/codegraph.ts worker`; it never opens `graph.db`, never takes the lock and never imports the
 * writer module.
 *
 * What the signature contains, and why it is not the same everywhere: in a git checkout it is HEAD
 * plus the porcelain status, which already names every path whose bytes or tracked status differ.
 * An mtime scan on top of that would only turn a bare `touch` into a spawn the worker answers with
 * exit 2 — work for the graph's readers to wait on, for no change. A directory git cannot read
 * degrades to the mtimes of the files the graph could contain, because that is the only signal
 * left there (`#shared/domain/diff` degrades the same way, for the same reason).
 *
 * The child's only report is its exit code, and this module treats the four codes the worker
 * defines (`WORKER_EXIT` in `./worker`, spelled out here because importing it is forbidden by the
 * process split) as follows:
 * - 0 published / 2 nothing to do: the observed state is now the baseline, and any failure streak
 *   is over;
 * - 75 lock held: another indexer is running, which is normal, so it logs and retries on the next
 *   tick without counting a failure — a contended repo must not slow its own polling down;
 * - anything else: a failure. The baseline is *not* advanced, so the change is retried, and the
 *   polling delay backs off to `min(interval << failures, 300 s)` so a run that dies on a broken
 *   tree cannot be respawned every two seconds forever.
 */
import { createHash } from "node:crypto";
import { readdir, stat } from "node:fs/promises";
import { isAbsolute, join, relative, resolve } from "node:path";
import { type LogFields, log } from "#shared/log";
import { normalizeRel } from "#shared/paths";
import { ALWAYS_EXCLUDED } from "./discover";
import { detectLanguage } from "./lang/registry";

/** `TECH-SPEC.md` §8: poll every 2 s — cheap enough that a change is noticed while the editor saves. */
export const DEFAULT_INTERVAL_MS = 2_000;

/** `TECH-SPEC.md` §8: coalesce a save that writes the file more than once. */
export const DEFAULT_DEBOUNCE_MS = 500;

/** The ceiling on the failure backoff, so a repo that cannot index still gets retried. */
export const MAX_BACKOFF_MS = 300_000;

/** One poll's git call must not outlive several intervals, or the next poll starts late. */
const GIT_TIMEOUT_MS = 10_000;

/** How long `stop()` waits after `kill()` before giving up on the child's exit code. */
const KILL_GRACE_MS = 5_000;

/** The CLI's own dispatch is the process boundary: `bun run <bin> worker`. */
const BIN_PATH = resolve(import.meta.dir, "../../bin/codegraph.ts");

/** `WORKER_EXIT` in `./worker`, restated: 75 is `EX_TEMPFAIL`, "another indexer holds the lock". */
const EXIT_PUBLISHED = 0;
const EXIT_NOTHING_TO_DO = 2;
const EXIT_LOCKED = 75;

/** The minimum a spawned worker must offer; Bun's `Subprocess` satisfies it structurally. */
export interface WatchChild {
  /** Resolves with the worker's exit code when the child ends. */
  readonly exited: Promise<number>;
  /** Ends the child. Safe on a child that has already exited. */
  kill(): void;
}

/** What the child needs to know that the CLI cannot read from its own working directory. */
export interface SpawnTarget {
  /** The tree to index; also forwarded as `CODEGRAPH_REPO_ROOT`. */
  readonly repoRoot: string;
  /** Where the graph, the lock and the data dir's `.gitignore` live; also `CODEGRAPH_DATA_DIR`. */
  readonly dataDir: string;
}

/**
 * The spawn seam. Tests pass a stub so no real index runs; the default runs the CLI. A function
 * that ignores the second argument is accepted, because a test only needs the command line.
 */
export type SpawnWorker = (cmd: readonly string[], target: SpawnTarget) => WatchChild;

export interface SpawnNotice {
  /** `Date.now()` at the spawn, so a caller can log an age without reading another clock. */
  readonly startedAt: number;
  /** Consecutive failures before this attempt, so a caller can tell a retry from a fresh run. */
  readonly failures: number;
}

export interface WatchOptions {
  /** The working tree whose changes are watched; resolved once, here. */
  readonly repoRoot: string;
  /** Forwarded to the child, so the watcher and the worker can never target different graphs. */
  readonly dataDir: string;
  /** Poll period; defaults to {@link DEFAULT_INTERVAL_MS}. */
  readonly intervalMs?: number;
  /** Quiet window after a change before polling again; defaults to {@link DEFAULT_DEBOUNCE_MS}. */
  readonly debounceMs?: number;
  /** Injected by tests; the default spawns `bin/codegraph.ts worker`. */
  readonly spawn?: SpawnWorker;
  /** Called once per spawn, before the child's exit is awaited. */
  readonly onSpawn?: (notice: SpawnNotice) => void;
}

/** A snapshot of the loop's memory, for `health.indexer` and for tests. Never the live object. */
export interface WatchStats {
  /** Observations that produced a signature and were compared. Not the same as timer ticks. */
  readonly polls: number;
  /** Spawn attempts, including the ones that exited 75 or 1. */
  readonly spawns: number;
  /** Consecutive failures; any success or lock contention resets it. */
  readonly failures: number;
  /** The delay currently in force, `0` on the healthy path. */
  readonly backoffMs: number;
  readonly lastExitCode: number | null;
  readonly lastSpawnAt: number | null;
  /** Which detector answered, so a degraded repo is visible instead of merely slower. */
  readonly detector: Detector | null;
}

export interface WatchHandle {
  /** Clears the timer, kills a running child and resolves once the loop has ended. */
  stop(): Promise<void>;
  stats(): WatchStats;
}

/** `git` when the tree is a checkout, `mtime` when it is not (or git is unusable). */
export type Detector = "git" | "mtime";

/** One observation of the tree: the comparable key plus what produced it. */
interface Observed {
  readonly key: string;
  readonly detector: Detector;
}

/**
 * The retry delay after `failures` consecutive failures: the poll interval doubled once per
 * failure, capped at {@link MAX_BACKOFF_MS}. Exported because it is a rule a caller can check
 * without waiting five minutes for the cap.
 */
export function backoffMs(intervalMs: number, failures: number): number {
  if (failures <= 0) {
    return 0;
  }
  // `2 ** failures` saturates at Infinity rather than wrapping, so the cap always wins eventually.
  return Math.min(intervalMs * 2 ** failures, MAX_BACKOFF_MS);
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** `true` when `child` is strictly inside `parent`; the data dir must be pruned only when inside. */
function isInside(parent: string, child: string): boolean {
  const rel = relative(parent, child);
  return rel !== "" && !rel.startsWith("..") && !isAbsolute(rel);
}

interface GitOutput {
  readonly ok: boolean;
  readonly stdout: string;
  readonly failure: string | null;
}

/**
 * One bounded git read. The flags mirror `#shared/domain/diff`: no optional index lock (this
 * module must not write), no credential helper, no pager, and a closed stdin so a hook cannot
 * prompt. A missing binary, a missing repository or a timeout is reported, never thrown, because
 * "git is unusable here" is a degradation and not an error.
 */
async function gitRun(repoRoot: string, args: readonly string[]): Promise<GitOutput> {
  const cmd = [
    "git",
    "--no-pager",
    "--no-optional-locks",
    "-c",
    "credential.helper=",
    "-c",
    "core.askpass=",
    "-C",
    repoRoot,
    ...args,
  ];

  let proc: ReturnType<typeof Bun.spawn<"ignore", "pipe", "pipe">>;
  try {
    proc = Bun.spawn({ cmd, cwd: repoRoot, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  } catch (error) {
    return { ok: false, stdout: "", failure: `git could not be spawned: ${messageOf(error)}` };
  }

  // The timeout wins the race: a killed git can leave a grandchild holding the pipes open, which
  // would make a "wait for EOF" bound meaningless.
  let timer: ReturnType<typeof setTimeout> | null = null;
  const expired = new Promise<GitOutput>((report) => {
    timer = setTimeout(() => {
      proc.kill("SIGKILL");
      report({
        ok: false,
        stdout: "",
        failure: `git ${args.join(" ")} exceeded ${GIT_TIMEOUT_MS} ms`,
      });
    }, GIT_TIMEOUT_MS);
    timer.unref();
  });

  const collected = Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]).then(([stdout, stderr, code]): GitOutput => {
    if (code !== 0) {
      const first = stderr.split("\n")[0] ?? "";
      return { ok: false, stdout, failure: `git ${args.join(" ")} exited ${code}: ${first}` };
    }
    return { ok: true, stdout, failure: null };
  });

  try {
    return await Promise.race([collected, expired]);
  } catch (error) {
    return { ok: false, stdout: "", failure: `git ${args.join(" ")} failed: ${messageOf(error)}` };
  } finally {
    if (timer !== null) {
      clearTimeout(timer);
    }
  }
}

/**
 * HEAD plus the porcelain status, or `null` when this directory is not a checkout git can read.
 * The two probes are separate calls on purpose: `rev-parse --git-dir` answers "is this a
 * repository", while `rev-parse HEAD` fails on a branch with no commits yet — a real repository
 * whose working tree still deserves watching.
 */
async function readGitSignature(repoRoot: string): Promise<Observed | null> {
  const dir = await gitRun(repoRoot, ["rev-parse", "--git-dir"]);
  if (!dir.ok) {
    return null;
  }

  const head = await gitRun(repoRoot, ["rev-parse", "HEAD"]);
  const status = await gitRun(repoRoot, ["status", "--porcelain"]);
  if (!status.ok) {
    // The repository exists but git cannot describe its tree; mtimes are the only fallback left.
    return null;
  }

  // An unborn HEAD contributes the empty string, which is still a stable key component.
  return {
    key: `git\n${head.ok ? head.stdout.trim() : ""}\n${status.stdout}`,
    detector: "git",
  };
}

/**
 * The mtime-only signature: a digest over the path, mtime and size of every file the graph could
 * contain. It is deliberately not a hash of contents — that is the worker's job, and re-reading
 * every file on every poll would cost more than the run it would save.
 */
async function readMtimeSignature(repoRoot: string, dataDir: string): Promise<string> {
  // A data dir inside the tree would otherwise look like a source change on every run: the worker
  // writes `graph.db` there, so the watcher would spawn forever. Outside the tree there is nothing
  // to prune.
  const skipDir = isInside(repoRoot, dataDir) ? dataDir : null;
  const rows: string[] = [];
  const pending: string[] = [repoRoot];

  while (pending.length > 0) {
    const dir = pending.pop();
    if (dir === undefined) {
      break;
    }
    const entries = await readdir(dir, { withFileTypes: true });
    entries.sort((left, right) => (left.name < right.name ? -1 : left.name > right.name ? 1 : 0));

    for (const entry of entries) {
      const absolute = join(dir, entry.name);
      // A symlink can point outside the tree or form a cycle; discovery skips them for the same
      // reason, and the signature must describe the same file set the graph can.
      if (entry.isSymbolicLink()) {
        continue;
      }
      if (entry.isDirectory()) {
        if (ALWAYS_EXCLUDED.has(entry.name) || absolute === skipDir) {
          continue;
        }
        pending.push(absolute);
        continue;
      }
      if (!entry.isFile()) {
        continue;
      }

      const rel = normalizeRel(relative(repoRoot, absolute));
      if (detectLanguage(rel) === null) {
        // Not a file any shipped grammar could parse, so its mtime cannot change the graph.
        continue;
      }

      let info: Awaited<ReturnType<typeof stat>>;
      try {
        info = await stat(absolute);
      } catch {
        // Vanished between the readdir and the stat: the next poll sees the settled tree.
        continue;
      }
      rows.push(`${rel}\u0000${Math.floor(info.mtimeMs)}\u0000${info.size}`);
    }
  }

  // Sorted so the digest is a function of the tree, not of readdir or traversal order.
  rows.sort();
  const digest = createHash("sha256").update(rows.join("\n")).digest("hex");
  return `mtime\n${rows.length}\n${digest}`;
}

/**
 * One poll's observation. The mtime path throws when the tree cannot be read at all, and the loop
 * treats that as a degradation rather than a failure (see `pump`).
 */
async function observe(repoRoot: string, dataDir: string): Promise<Observed> {
  const git = await readGitSignature(repoRoot);
  if (git !== null) {
    return git;
  }
  return { key: await readMtimeSignature(repoRoot, dataDir), detector: "mtime" };
}

/** The default spawn: the CLI's worker mode as a real child, rooted through its own environment. */
function spawnCliWorker(cmd: readonly string[], target: SpawnTarget): WatchChild {
  const child = Bun.spawn({
    cmd: [...cmd],
    cwd: target.repoRoot,
    // `#shared/config.ts` is the CLI's only environment reader, so this is the documented way to
    // hand the worker its roots; it keeps the watcher and the worker on one graph.
    env: {
      ...process.env,
      CODEGRAPH_REPO_ROOT: target.repoRoot,
      CODEGRAPH_DATA_DIR: target.dataDir,
    },
    stdin: "ignore",
    // stdout carries the MCP protocol (§11): a child that inherited it could corrupt the session.
    stdout: "ignore",
    stderr: "inherit",
  });
  return { exited: child.exited, kill: () => child.kill() };
}

/**
 * Starts the poll loop. The first poll only records the baseline — the watcher reports *changes*,
 * so a graph that is stale at startup is the caller's index run to make, not a spawn storm here.
 */
export function startWatch(options: WatchOptions): WatchHandle {
  const repoRoot = resolve(options.repoRoot);
  const dataDir = resolve(options.dataDir);
  const intervalMs = Math.max(1, Math.floor(options.intervalMs ?? DEFAULT_INTERVAL_MS));
  const debounceMs = Math.max(0, Math.floor(options.debounceMs ?? DEFAULT_DEBOUNCE_MS));
  const spawnWorker = options.spawn ?? spawnCliWorker;
  const onSpawn = options.onSpawn;

  let polls = 0;
  let spawns = 0;
  let failures = 0;
  let lastExitCode: number | null = null;
  let lastSpawnAt: number | null = null;
  let detector: Detector | null = null;

  /** The last state a successful run covered, or the first observation when nothing has run yet. */
  let baseline: string | null = null;
  let child: WatchChild | null = null;
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | null = null;
  let waker: (() => void) | null = null;

  /** Logs one message per distinct note, so a persistently degraded repo does not flood stderr. */
  let lastNote = "";
  function note(msg: string, fields: LogFields): void {
    if (msg === lastNote) {
      return;
    }
    lastNote = msg;
    log.warn(msg, fields);
  }

  /**
   * A sleep that `stop()` can cut short, and that is unref'd so a pending poll is never the reason
   * a process cannot exit. One timer slot is enough: the debounce wait and the poll wait never
   * overlap.
   */
  function sleep(ms: number): Promise<void> {
    return new Promise<void>((resolveSleep) => {
      waker = resolveSleep;
      const handle = setTimeout(() => {
        timer = null;
        waker = null;
        resolveSleep();
      }, ms);
      handle.unref();
      timer = handle;
    });
  }

  function interrupt(): void {
    if (timer !== null) {
      clearTimeout(timer);
      timer = null;
    }
    const wake = waker;
    waker = null;
    wake?.();
  }

  /** Runs one worker, then folds its exit code into the baseline, the streak and the delay. */
  async function run(key: string): Promise<void> {
    const startedAt = Date.now();
    spawns += 1;
    lastSpawnAt = startedAt;
    onSpawn?.({ startedAt, failures });

    let running: WatchChild;
    try {
      running = spawnWorker([process.execPath, "run", BIN_PATH, "worker"], { repoRoot, dataDir });
    } catch (error) {
      // A binary that cannot be spawned is a failure like any other, and retrying it is right.
      failures += 1;
      note("watch.spawn_failed", { message: messageOf(error), failures });
      return;
    }

    child = running;
    let code = -1;
    try {
      code = await running.exited;
    } catch (error) {
      note("watch.exit_failed", { message: messageOf(error) });
    } finally {
      child = null;
    }
    lastExitCode = code;
    const durationMs = Date.now() - startedAt;

    // A child killed by `stop()` says nothing about the repository, so it changes no state.
    if (stopped) {
      return;
    }

    if (code === EXIT_LOCKED) {
      log.info("watch.worker_locked", { durationMs, retryInMs: intervalMs });
      return;
    }

    if (code === EXIT_PUBLISHED || code === EXIT_NOTHING_TO_DO) {
      if (failures > 0) {
        log.info("watch.worker_recovered", { exitCode: code, priorFailures: failures });
      }
      failures = 0;
      lastNote = "";
      // The state this run saw is now covered; anything newer differs from the new baseline.
      baseline = key;
      log.debug("watch.worker_done", { exitCode: code, durationMs });
      return;
    }

    failures += 1;
    log.warn("watch.worker_failed", {
      exitCode: code,
      durationMs,
      failures,
      retryInMs: backoffMs(intervalMs, failures),
    });
    // The baseline stays put: the change that failed must be retried after the backoff.
  }

  /** One poll: observe, compare with the baseline, debounce, re-check once, then maybe spawn. */
  async function tick(): Promise<void> {
    if (child !== null) {
      // One writer at a time. The running child reads the tree itself, and the next tick re-reads
      // the signature, so nothing is missed by declining to spawn here.
      return;
    }

    const seen = await observe(repoRoot, dataDir);
    if (stopped) {
      return;
    }
    // Counted only once the observation exists: a caller that waits for `polls` is then also
    // waiting for the baseline, which is what makes "the first poll is the baseline" observable.
    polls += 1;
    detector = seen.detector;
    if (baseline === null) {
      baseline = seen.key;
      return;
    }
    if (seen.key === baseline) {
      return;
    }

    // Debounce: a save often writes the file several times. Re-check *once* after the quiet
    // window rather than restarting the window on every write — a change that lands during the
    // wait still differs from the baseline, so it is covered by the single spawn that follows,
    // and anything newer than that shows up on the next tick.
    await sleep(debounceMs);
    if (stopped || child !== null) {
      return;
    }
    const confirmed = await observe(repoRoot, dataDir);
    if (stopped) {
      return;
    }
    detector = confirmed.detector;
    if (confirmed.key === baseline) {
      // The tree settled back to what the graph already has (a save that was undone).
      return;
    }
    await run(confirmed.key);
  }

  /** The poll loop. Nothing is awaited after `stop()`: the loop's own timer is the only wake-up. */
  async function pump(): Promise<void> {
    while (!stopped) {
      await sleep(failures === 0 ? intervalMs : backoffMs(intervalMs, failures));
      if (stopped) {
        return;
      }
      try {
        await tick();
      } catch (error) {
        // An unreadable tree is a degradation, not a worker failure: log it and keep polling.
        note("watch.poll_failed", { message: messageOf(error) });
      }
    }
  }

  const pumped = pump();

  /** Waits for a child's exit code, but never longer than the kill grace. */
  async function settle(exited: Promise<number>): Promise<void> {
    let grace: ReturnType<typeof setTimeout> | null = null;
    try {
      await Promise.race([
        exited.catch(() => undefined),
        new Promise<void>((resolveGrace) => {
          grace = setTimeout(resolveGrace, KILL_GRACE_MS);
          grace.unref();
        }),
      ]);
    } finally {
      if (grace !== null) {
        clearTimeout(grace);
      }
    }
  }

  async function halt(): Promise<void> {
    stopped = true;
    interrupt();
    const running = child;
    if (running !== null) {
      try {
        running.kill();
      } catch (error) {
        log.warn("watch.kill_failed", { message: messageOf(error) });
      }
      await settle(running.exited);
    }
    await pumped;
  }

  let halting: Promise<void> | null = null;

  return {
    stop: (): Promise<void> => {
      halting ??= halt();
      return halting;
    },
    stats: (): WatchStats => ({
      polls,
      spawns,
      failures,
      backoffMs: backoffMs(intervalMs, failures),
      lastExitCode,
      lastSpawnAt,
      detector,
    }),
  };
}
