/**
 * Freshness: is the generation a reader serves still true of the tree on disk?
 *
 * Every answer a tool gives about the graph is a claim about a working tree, and the claim is only
 * as good as the distance between the two. This module is the one place that distance is measured,
 * so `status` does not have to guess and no two tools can disagree about what "stale" means.
 *
 * Four ways a served generation can stop matching its tree, in the order they are checked because
 * a later check cannot be trusted once an earlier one fails:
 *
 * - `root_mismatch`: `meta.project_root` is not the root the caller is answering about. Every
 *   other comparison is meaningless against the wrong directory — hashing `src/a.ts` under a
 *   different tree answers a different question — so the digest comparison is not even attempted
 *   and the reason says why.
 * - `base_commit_moved`: the recorded `generation.base_commit` is no longer `HEAD`. A commit change
 *   means the tree the index was built from is behind, even when every indexed file still hashes
 *   the same.
 * - `watcher_stalled`: the indexer records itself as busy and nothing has been published for longer
 *   than {@link WATCHER_STALL_MS}. `meta` carries no "busy since" column, so the last publish time
 *   is the only publish-side clock there is; a run recorded as busy across a stretch that long is
 *   the observable form of a watcher that died mid-run.
 * - `files_changed`: an indexed file's current bytes hash differently from the `file.sha256` the
 *   indexer stored.
 *
 * Two degradations are deliberate, because a freshness verdict that throws would take `status`
 * down with it:
 *
 * - `git` is never invoked. {@link headCommitOf} reads `.git/HEAD`, the ref file it names and
 *   `packed-refs` — three reads, no child process, no lock, nothing a target repo's config could
 *   make this module execute. A directory that is not a checkout yields `null`, and
 *   `base_commit_moved` is then simply not claimed: "unknown" must never be reported as "stale".
 * - a file that cannot be read is not counted as changed. It has no bytes to compare, so it is
 *   neither equal nor different; `#shared/domain/diff.ts` is the module that names removals for the
 *   change-set tools, and `expand` reports an unreadable path at the point of use. `status` says so
 *   in its own summary rather than inventing a count here.
 *
 * The reader is the parameter, never a connection: a handler holds a `GraphReader` and nothing
 * else, so freshness has to be answerable from that interface alone. `now` and the `HEAD` probe are
 * injectable for the same reason the clock is injected everywhere else — the ten-minute threshold
 * and the commit comparison are facts a test must be able to place exactly.
 */
import { createHash } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { isAbsolute, join, resolve, sep } from "node:path";
import type { GraphMeta, GraphReader } from "#shared/store/read/reader";

/** Ten minutes of "busy" with no publish: the bound `plan` sets for a stalled watcher. */
export const WATCHER_STALL_MS = 10 * 60 * 1000;

/**
 * The `meta.indexer_state` values that claim a run is in flight. `failed` is not one of them: a
 * failed run is reported by `health.consecutive_failures`, not as a stall.
 */
export const BUSY_INDEXER_STATES: readonly string[] = ["running", "busy"];

/**
 * Every value {@link Freshness.stale_reason} can take. `none` is a value rather than an absent
 * field so a caller maps one closed set, and `status` is the one place that decides a fresh answer
 * carries no `stale_reason` key at all.
 */
export const STALE_REASONS = [
  "none",
  "files_changed",
  "root_mismatch",
  "base_commit_moved",
  "watcher_stalled",
] as const;
export type StaleReason = (typeof STALE_REASONS)[number];

/** The verdict, with the evidence that produced it. */
export interface Freshness {
  readonly stale: boolean;
  /** One of {@link STALE_REASONS}; `none` exactly when {@link Freshness.stale} is `false`. */
  readonly stale_reason: StaleReason;
  /**
   * Indexed files whose current bytes no longer match their recorded `sha256`. `0` when the
   * comparison did not run (a root mismatch makes it meaningless) and for a file that is gone,
   * which has no bytes to compare.
   */
  readonly files_changed_since: number;
  /** When the verdict was computed, ISO-8601: the claim is dated, never absolute. */
  readonly checked_at: string;
}

/** Reads the commit `HEAD` names for `repoRoot`; `null` when this is not a readable checkout. */
export type HeadReader = (repoRoot: string) => string | null;

export interface FreshnessOptions {
  /** Injected clock; staleness must be testable rather than wall-clock. */
  readonly now?: () => Date;
  /** Injected `HEAD` probe; defaults to {@link headCommitOf} and is never a git invocation. */
  readonly readHead?: HeadReader;
}

/** `sha256` in a comparison form: the indexer writes lowercase hex, a row is data, not a promise. */
const SHA256_HEX = /^[0-9a-f]{7,64}$/i;

/** A path node of the working tree, or `null` when the row escapes `root`. */
function absoluteWithin(root: string, path: string): string | null {
  const absolute = resolve(root, path);
  if (absolute === root) {
    return null;
  }
  return absolute.startsWith(`${root}${sep}`) ? absolute : null;
}

/** `null` when the bytes cannot be read: an unreadable file is not a changed one. */
function sha256Of(absolute: string): string | null {
  try {
    return createHash("sha256").update(readFileSync(absolute)).digest("hex");
  } catch {
    return null;
  }
}

/** Indexed files whose working-tree bytes differ from the digest the indexer stored. */
function countChanged(
  repoRoot: string,
  files: readonly { path: string; sha256: string }[],
): number {
  let changed = 0;
  for (const file of files) {
    const absolute = absoluteWithin(repoRoot, file.path);
    if (absolute === null) {
      continue;
    }
    const actual = sha256Of(absolute);
    if (actual !== null && actual !== file.sha256.toLowerCase()) {
      changed += 1;
    }
  }
  return changed;
}

/** The `.git` directory of `root`, following the `gitdir:` file a worktree or submodule uses. */
function gitDirOf(root: string): string | null {
  const dotGit = join(root, ".git");
  try {
    if (statSync(dotGit).isDirectory()) {
      return dotGit;
    }
    const text = readFileSync(dotGit, "utf8").trim();
    const marker = "gitdir:";
    if (!text.startsWith(marker)) {
      return null;
    }
    const target = text.slice(marker.length).trim();
    return isAbsolute(target) ? target : resolve(root, target);
  } catch {
    return null;
  }
}

/** A ref that has been packed away by `git gc`: one line per ref, `^` lines are peel lines. */
function packedRef(gitDir: string, ref: string): string | null {
  let text: string;
  try {
    text = readFileSync(join(gitDir, "packed-refs"), "utf8");
  } catch {
    return null;
  }
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (trimmed === "" || trimmed.startsWith("#") || trimmed.startsWith("^")) {
      continue;
    }
    const [sha, name] = trimmed.split(/\s+/);
    if (name === ref && sha !== undefined && SHA256_HEX.test(sha)) {
      return sha.toLowerCase();
    }
  }
  return null;
}

/**
 * The commit `HEAD` names, read from the repository's own files: the `HEAD` file, the ref it names
 * (loose, then `packed-refs`), or the raw object name of a detached `HEAD`.
 *
 * `null` is the honest answer for "not a checkout", "no ref yet" and "unreadable": the caller
 * degrades to "no commit comparison", which is why this module never shells out to git. A child
 * process would cost milliseconds, take an optional lock and execute whatever a target repo's
 * `core.pager`/`core.fsmonitor` config asked for, none of which a read-only tool may do.
 */
export function headCommitOf(repoRoot: string): string | null {
  const gitDir = gitDirOf(resolve(repoRoot));
  if (gitDir === null) {
    return null;
  }
  let head: string;
  try {
    head = readFileSync(join(gitDir, "HEAD"), "utf8").trim();
  } catch {
    return null;
  }
  if (!head.startsWith("ref:")) {
    return SHA256_HEX.test(head) ? head.toLowerCase() : null;
  }
  const ref = head.slice("ref:".length).trim();
  if (ref === "") {
    return null;
  }
  try {
    const sha = readFileSync(join(gitDir, ...ref.split("/")), "utf8").trim();
    if (SHA256_HEX.test(sha)) {
      return sha.toLowerCase();
    }
  } catch {
    // A ref `git gc` has packed is not a loose file; fall through rather than call it unknown.
  }
  return packedRef(gitDir, ref);
}

/**
 * A short recorded commit and a full `HEAD` are the same commit when one is a prefix of the other,
 * which is what makes `base_commit` written by `git rev-parse --short` comparable at all.
 */
function movedSince(recorded: string, head: string): boolean {
  return !(recorded === head || head.startsWith(recorded) || recorded.startsWith(head));
}

/** `meta.indexer_state` busy and nothing published for longer than the threshold. */
function watcherStalled(meta: GraphMeta, now: Date): boolean {
  const state = (meta.indexerState ?? "").toLowerCase();
  if (!BUSY_INDEXER_STATES.includes(state)) {
    return false;
  }
  const publishedAt = meta.indexedAt === null ? Number.NaN : Date.parse(meta.indexedAt);
  if (!Number.isFinite(publishedAt)) {
    return false;
  }
  return now.getTime() - publishedAt > WATCHER_STALL_MS;
}

/**
 * The freshness verdict for `repoRoot`, computed from the injected reader alone.
 *
 * The comparison is ordered as the module comment describes and the first failing check wins: a
 * mismatched root makes the hash comparison meaningless, and a commit or a stalled watcher explains
 * the drift before the per-file count does. The cost is one `allFiles()` (unbounded by design, from
 * `store/read/files.ts`) and one read per indexed file; the caller owns the decision to ask.
 */
export function checkFreshness(
  reader: GraphReader,
  repoRoot: string,
  options: FreshnessOptions = {},
): Freshness {
  const now = options.now ?? (() => new Date());
  const readHead = options.readHead ?? headCommitOf;
  const at = now();
  const checkedAt = at.toISOString();
  const root = resolve(repoRoot);
  const meta = reader.metaFor();

  const verdict = (stale_reason: StaleReason, files_changed_since: number): Freshness => ({
    stale: stale_reason !== "none",
    stale_reason,
    files_changed_since,
    checked_at: checkedAt,
  });

  // An unrecorded `project_root` is not a mismatch: the graph never claimed a tree, so there is
  // nothing to contradict. Only two recorded roots that differ are a `root_mismatch`.
  if (meta.projectRoot !== null && resolve(meta.projectRoot) !== root) {
    return verdict("root_mismatch", 0);
  }

  // One `allFiles()` and one read per indexed file for the whole verdict, whatever the reason it
  // reports: the count is part of the answer in every branch below.
  const changed = countChanged(root, reader.allFiles());

  const recorded = reader.latestGeneration()?.baseCommit?.trim().toLowerCase() ?? "";
  const head = readHead(root)?.trim().toLowerCase() ?? "";
  if (recorded !== "" && head !== "" && movedSince(recorded, head)) {
    return verdict("base_commit_moved", changed);
  }

  if (watcherStalled(meta, at)) {
    return verdict("watcher_stalled", changed);
  }

  return verdict(changed === 0 ? "none" : "files_changed", changed);
}

/**
 * `status.health`: the indexer's own state and the two costs a caller can act on. `last_publish_ms`
 * is the duration of the publish being served (`0` before the first one), and
 * `statement_cache_entries` is what the reader's prepared-statement cache holds right now.
 *
 * The cache count is optional on `GraphReader` because a hand-written fake in a unit test has no
 * cache to report, and inventing one would make the fake lie. When the member is present the number
 * is that handle's own `statementCacheStats().size` (`store/db.ts`); when it is absent the honest
 * answer is `0`, never a guess.
 */
export interface ToolHealth {
  readonly indexer: string | null;
  readonly last_publish_ms: number;
  readonly consecutive_failures: number;
  readonly statement_cache_entries: number;
}

export function health(reader: GraphReader): ToolHealth {
  const meta = reader.metaFor();
  const generation = reader.latestGeneration();
  return {
    indexer: meta.indexerState,
    last_publish_ms: generation === null ? 0 : generation.durationMs,
    consecutive_failures: meta.consecutiveFailures,
    statement_cache_entries: reader.statementCacheEntries?.() ?? 0,
  };
}
