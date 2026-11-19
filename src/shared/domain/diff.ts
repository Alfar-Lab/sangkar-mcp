/**
 * The one change-set computation: what differs from `base`, what is gone, and what could not be
 * attributed to a file row. `summary(change)` and `impact` both read this and neither recomputes
 * it (plan.constraint_invariant.md: one change-set, never one per caller).
 *
 * Two sources are unioned, because neither is right alone:
 * - `git diff --name-status` knows what changed against a commit, including deletions and renames,
 *   and it still sees paths the indexer never parsed (a modified `README.md`). It never sees
 *   untracked files.
 * - hashing every `file` row's working-tree bytes against `file.sha256` — the column the indexer
 *   already stores — is the only source in a directory git cannot read, and the only one that
 *   notices a changed indexed file that git ignores.
 *
 * Three traps closed once here:
 * - the integration fixture is a plain temp directory, so a git failure (no repository, no
 *   binary, timeout) degrades to the hash comparison with `degraded: true` rather than throwing;
 * - this module writes nothing: git runs with `--no-optional-locks` so it cannot take the
 *   optional index lock, and `--no-ext-diff` so a target repo's own config cannot run a command;
 *   the only filesystem calls are reads of working-tree bytes, so the target repo's tracked files
 *   are never touched;
 * - the environment is never read here (that stays in `#shared/config.ts`): `Bun.spawn` inherits
 *   the parent environment implicitly when no `env` is given, and the child is kept
 *   non-interactive by closing its stdin and clearing the credential and askpass helpers.
 */
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { log } from "#shared/log";
import { normalizeRel } from "#shared/paths";
import { type Conn, prepared } from "#shared/store/db";

/** The hard bound on one `git` invocation: a hung git must never hang a tool call. */
export const GIT_TIMEOUT_MS = 30_000;

/** Why a path is in `unmatched`: we know it changed but cannot say what changed. */
export type UnmatchedReason =
  /** Git reported it, there is no `file` row, and no MVP grammar covers its extension. */
  | "no_file_row_and_unparseable_language"
  /** An indexed path whose working-tree bytes could not be read while hashing. */
  | "unreadable_file";

/** A changed path that is neither in the graph nor parseable, kept instead of dropped. */
export interface UnmatchedChange {
  readonly path: string;
  readonly reason: UnmatchedReason;
}

/** What happened to one path in the change set, in the vocabulary a payload can render. */
export type ChangeState = "modified" | "added" | "deleted" | "renamed";

/**
 * One path of the change set with its state. A rename is a single entry — the destination path
 * plus where it came from — because git's two halves of a rename are one fact, and a payload that
 * rendered them as "one added, one deleted" would lose that. A copy is `added`: its source still
 * exists, so there is nothing to attribute away from.
 */
export interface ChangeEntry {
  readonly state: ChangeState;
  /** The destination path: the new name of a rename, the new file of a copy. Repo-relative. */
  readonly path: string;
  /** The path a rename came from; absent for every other state. Repo-relative. */
  readonly from?: string;
}

/** The change set both change-aware tools read; the arrays are sorted and repo-relative. */
export interface ChangeSet {
  /** Differ from `base`, or no longer match `file.sha256`; sorted, forward slashes. */
  readonly changed: readonly string[];
  /** Had a `file` row and their bytes are gone from the working tree; sorted. */
  readonly removed: readonly string[];
  /**
   * Every path in `changed` and `removed` with its state, sorted by path; the one view that can
   * say "renamed from". A path in `unmatched` has no entry — it is not attributed to a file yet,
   * and its reason lives in `unmatched`. A path outside `repoRoot` appears in neither.
   */
  readonly entries: readonly ChangeEntry[];
  readonly unmatched: readonly UnmatchedChange[];
  /** `true` when git was unusable and this set came from the hash comparison alone. */
  readonly degraded: boolean;
  /** Why git was unusable, for `log`/`status`; `null` when git answered. */
  readonly degradedReason: string | null;
}

export interface ChangedSinceOptions {
  /** Bound on one git invocation; defaults to {@link GIT_TIMEOUT_MS} and is never unbounded. */
  readonly gitTimeoutMs?: number;
  /**
   * Environment forwarded to the git child when a caller must supply one. Omitted by default, so
   * `Bun.spawn` inherits the parent environment; this module never reads it to fill this in.
   */
  readonly env?: Readonly<Record<string, string | undefined>>;
}

/**
 * The digest half of the change set, with its source already bound. `GraphReader` from
 * `#shared/store/read/reader.ts` satisfies this structurally through `allFiles()`, which is what
 * lets a tool handler run this module at all: a handler holds a reader and never a connection.
 */
export interface DigestSource {
  /** Every indexed path with the digest the indexer stored for it. */
  allFiles(): readonly { readonly path: string; readonly sha256: string }[];
}

/**
 * Extensions the MVP grammars cover, mapped to their lang id. TASK-013's registry owns this list
 * from phase 1 on; until that module exists a changed path counts as parseable here if and only
 * if the indexer could have parsed it, so the two lists must agree when they meet.
 */
const LANGUAGE_BY_EXTENSION: Readonly<Record<string, string>> = {
  ".py": "python",
  ".ts": "typescript",
  ".tsx": "tsx",
  ".js": "javascript",
  ".jsx": "javascript",
  ".mjs": "javascript",
  ".cjs": "javascript",
  ".go": "go",
  ".java": "java",
  ".rs": "rust",
  ".c": "c",
  ".h": "c",
  ".cpp": "cpp",
  ".cc": "cpp",
  ".hpp": "cpp",
  ".cs": "csharp",
  ".php": "php",
  ".rb": "ruby",
};

/** The MVP lang id for a path, or `null` when no shipped grammar could parse it. */
function languageFor(path: string): string | null {
  const dot = path.lastIndexOf(".");
  if (dot <= path.lastIndexOf("/")) {
    // No dot at all, or a dot that only belongs to a directory name (`src/v1.2/README`).
    return null;
  }
  return LANGUAGE_BY_EXTENSION[path.slice(dot).toLowerCase()] ?? null;
}

interface FileDigestSqlRow {
  readonly path: string;
  readonly sha256: string;
}

/** Every indexed path with the digest the indexer stored for it. One query, never one per path. */
function indexedDigests(conn: Conn): readonly FileDigestSqlRow[] {
  return prepared(conn, "diff.fileDigests", (sqlite) =>
    sqlite.query<FileDigestSqlRow, []>("SELECT path, sha256 FROM file ORDER BY path"),
  ).all();
}

/** What one git invocation produced: either its stdout, or the reason it is unusable. */
type GitOutcome =
  | { readonly ok: true; readonly stdout: string }
  | { readonly ok: false; readonly failure: string };

function firstLine(text: string): string {
  const line = text.split("\n", 1)[0]?.trim() ?? "";
  return line === "" ? "no output" : line;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/**
 * `git diff --name-status` against `base`, bounded by `timeoutMs` and never interactive. Rename
 * detection is left on (git's default): a rename has to arrive as one `R` record, because only then
 * can the change set say "this moved" instead of "one path appeared, another vanished".
 */
async function gitNameStatus(
  repoRoot: string,
  base: string,
  timeoutMs: number,
  env: Readonly<Record<string, string | undefined>> | undefined,
): Promise<GitOutcome> {
  const cmd = [
    "git",
    "--no-pager",
    // Refusing the optional index lock is what keeps `.git/index` untouched: no write of any kind.
    "--no-optional-locks",
    // Non-interactive without touching the environment: no credential helper, no askpass program.
    "-c",
    "credential.helper=",
    "-c",
    "core.askpass=",
    "-C",
    repoRoot,
    "diff",
    "--name-status",
    "-z",
    "--no-ext-diff",
    // Paths relative to `repoRoot`, not the repository's toplevel, so a subdirectory root works.
    "--relative",
    base,
    "--",
  ];
  let proc: ReturnType<typeof Bun.spawn<"ignore", "pipe", "pipe">>;
  try {
    proc = Bun.spawn({
      cmd,
      cwd: repoRoot,
      // Closed stdin plus piped output: the child has no terminal to prompt on.
      stdin: "ignore",
      stdout: "pipe",
      stderr: "pipe",
      // Omitted, so Bun inherits the parent environment; a caller that must override it passes
      // `options.env` instead of this module reading the environment.
      ...(env === undefined ? {} : { env }),
    });
  } catch (error) {
    // The binary is missing or the cwd is unreadable: both are "git is unavailable" here.
    return { ok: false, failure: `git could not be spawned: ${messageOf(error)}` };
  }

  // The timeout wins the race rather than being checked after the pipes close: a killed git can
  // leave a child (a clean filter, say) holding the stderr pipe open, and waiting for that EOF
  // would make the bound meaningless.
  let timer: ReturnType<typeof setTimeout> | undefined;
  const expired = new Promise<GitOutcome>((report) => {
    timer = setTimeout(() => {
      proc.kill("SIGKILL");
      report({ ok: false, failure: `git diff exceeded ${timeoutMs} ms and was killed` });
    }, timeoutMs);
  });
  const collected = Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]).then(([stdout, stderr, code]): GitOutcome => {
    if (code !== 0) {
      return { ok: false, failure: `git diff exited ${code}: ${firstLine(stderr)}` };
    }
    return { ok: true, stdout };
  });
  try {
    return await Promise.race([collected, expired]);
  } catch (error) {
    return { ok: false, failure: `git diff failed: ${messageOf(error)}` };
  } finally {
    clearTimeout(timer);
  }
}

interface GitEntry {
  /** The `--name-status` letter: `M`, `A`, `D`, `R`, `C`, `T`, … */
  readonly status: string;
  /** The destination path: as git reported it, before containment and normalisation. */
  readonly path: string;
  /** The source path of an `R`/`C` pair; absent on every single-path record. */
  readonly from?: string;
}

/**
 * `-z` writes `STATUS\0path\0` per entry, and `R`/`C` write `STATUS\0source\0destination\0`.
 *
 * A malformed record — a truncated pair, an empty path — is skipped instead of failing the parse:
 * one odd line must not empty the change set, and `null` coming back from git is the only thing
 * that may degrade the result to the hash comparison.
 */
function parseNameStatus(stdout: string): readonly GitEntry[] {
  const tokens = stdout.split("\0");
  const entries: GitEntry[] = [];
  let index = 0;
  while (index < tokens.length) {
    const status = tokens[index] ?? "";
    index += 1;
    if (status === "") {
      // The trailing NUL leaves one empty token; anything else is a malformed record.
      continue;
    }
    const letter = status.slice(0, 1);
    const paired = letter === "R" || letter === "C";
    const from = tokens[index];
    const path = tokens[index + (paired ? 1 : 0)];
    index += paired ? 2 : 1;
    if (path === undefined || path === "" || (paired && (from === undefined || from === ""))) {
      continue;
    }
    entries.push(
      paired && from !== undefined ? { status: letter, path, from } : { status: letter, path },
    );
  }
  return entries;
}

/** git's status letter as the state a payload renders. */
function stateFor(status: string): ChangeState {
  switch (status) {
    case "A":
      return "added";
    case "D":
      return "deleted";
    case "R":
      return "renamed";
    // A copy leaves its source in place, so only the destination is new: `added`, without a
    // `from` a reader would mistake for a rename.
    case "C":
      return "added";
    // `M`, `T` (type change), `U` (unmerged), `X` (unknown): different bytes at the same path.
    default:
      return "modified";
  }
}

/**
 * The repo-relative form of a reported path, or `null` when it is outside `repoRoot` — those are
 * dropped. Containment is lexical: `resolve()` collapses `..`, so `../x` and an absolute sibling
 * both fail the prefix test instead of being normalised into something that looks contained.
 */
function repoRelative(repoRoot: string, reported: string): string | null {
  const root = resolve(repoRoot);
  const absolute = isAbsolute(reported) ? resolve(reported) : resolve(root, reported);
  if (absolute !== root && !absolute.startsWith(`${root}${sep}`)) {
    return null;
  }
  const rel = normalizeRel(relative(root, absolute));
  return rel === "" ? null : rel;
}

/** sha256 of a working-tree file's bytes, as the indexer computes it for `file.sha256`. */
async function sha256Of(path: string): Promise<string> {
  const bytes = new Uint8Array(await Bun.file(path).arrayBuffer());
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

/**
 * The change set for `repoRoot` against `base`, as the union of `git diff --name-status` and a
 * sha256 comparison of the `file` table against the working tree.
 *
 * The digest source is the third parameter on purpose: `shared/domain` decides what is true but
 * never opens a database (the caller passes the handle or the reader it already holds), and the
 * hash comparison cannot be done without the indexed digests. A tool handler has no connection,
 * so it calls {@link changedSinceFrom} with its injected `GraphReader`.
 *
 * A path git calls deleted but which still exists in the working tree is reported as `changed`,
 * not as `removed`: the bytes are present, so "gone" would be the wrong fact.
 *
 * `degraded` never throws: a missing git, a directory that is not a repository and a timeout all
 * fall back to the hash comparison, because the integration fixture is a plain temp directory.
 */
export async function changedSince(
  repoRoot: string,
  base: string,
  conn: Conn,
  options: ChangedSinceOptions = {},
): Promise<ChangeSet> {
  return changeSetFor(repoRoot, base, indexedDigests(conn), options);
}

/**
 * The same change set, sourced from an injected reader instead of a connection. This is the entry
 * point `summary` and `impact` use: `GraphReader.allFiles()` already carries every `path` and
 * `sha256` the hash comparison needs, so a handler never needs a handle to answer "what changed".
 *
 * Both entries share one implementation, so the git half, the degradation rule and the ordering
 * cannot drift between the worker and the tools.
 */
export async function changedSinceFrom(
  repoRoot: string,
  base: string,
  source: DigestSource,
  options: ChangedSinceOptions = {},
): Promise<ChangeSet> {
  return changeSetFor(repoRoot, base, source.allFiles(), options);
}

async function changeSetFor(
  repoRoot: string,
  base: string,
  digests: readonly FileDigestSqlRow[],
  options: ChangedSinceOptions,
): Promise<ChangeSet> {
  const timeoutMs = options.gitTimeoutMs ?? GIT_TIMEOUT_MS;
  const root = resolve(repoRoot);
  const indexed = new Set<string>();
  const changed = new Set<string>();
  const removed = new Set<string>();
  const unmatched = new Map<string, UnmatchedChange>();
  // What git said about a path that reached `changed`: a rename keeps its source, so one entry can
  // carry both halves once the hash comparison has had its say.
  const gitState = new Map<string, ChangeEntry>();

  const classify = (path: string): boolean => indexed.has(path) || languageFor(path) !== null;
  const unclassified = (path: string, reason: UnmatchedReason): void => {
    unmatched.set(path, { path, reason });
  };

  // Source 1: git. `base` empty means "the last commit", the usual change scope for a summary.
  const outcome = await gitNameStatus(root, base === "" ? "HEAD" : base, timeoutMs, options.env);
  let degradedReason: string | null = null;
  if (outcome.ok) {
    for (const entry of parseNameStatus(outcome.stdout)) {
      const path = repoRelative(root, entry.path);
      const from = entry.from === undefined ? undefined : repoRelative(root, entry.from);
      // One half of a rename pair outside `repoRoot` makes the pair unattributable, so the record
      // goes: a path outside the root is dropped and counted nowhere, not silently re-bucketed.
      if (path === null || from === null) {
        continue;
      }
      const state = stateFor(entry.status);
      if (state === "deleted") {
        if (classify(path)) {
          removed.add(path);
        } else {
          unclassified(path, "no_file_row_and_unparseable_language");
        }
        continue;
      }
      // `M`, `A`, `T`, `U`, … all mean the working tree holds different bytes than `base`, and a
      // rename contributes its destination only: the source path is gone, so counting it too
      // would double-count one change.
      if (!classify(path)) {
        unclassified(path, "no_file_row_and_unparseable_language");
        continue;
      }
      changed.add(path);
      // `from` belongs to a rename. A copy's source still exists, so its destination is a plain
      // `added` entry and carrying the source would read as a rename downstream.
      gitState.set(
        path,
        state === "renamed" && from !== undefined ? { state, path, from } : { state, path },
      );
    }
  } else {
    degradedReason = outcome.failure;
    log.warn("diff.git_unavailable", { repo: root, base, reason: outcome.failure });
  }

  // Source 2: the file table against the working tree. Always run, so a change git ignores (an
  // indexed path listed in `.gitignore`) is still found, and so a non-git directory still works.
  for (const row of digests) {
    const path = repoRelative(root, row.path);
    if (path === null) {
      continue;
    }
    indexed.add(path);
    const absolute = join(root, path);
    if (!(await Bun.file(absolute).exists())) {
      removed.add(path);
      continue;
    }
    try {
      if ((await sha256Of(absolute)) !== row.sha256) {
        changed.add(path);
      }
    } catch (error) {
      log.warn("diff.hash_unreadable", { repo: root, path, reason: messageOf(error) });
      unclassified(path, "unreadable_file");
    }
  }

  // Present bytes outrank a deletion reported by an index: the path exists, so it is not gone.
  for (const path of changed) {
    removed.delete(path);
    unmatched.delete(path);
  }

  const byPath = (a: string, b: string): number => (a < b ? -1 : a > b ? 1 : 0);

  // Entries are derived from the two fields above, never the other way round, so a path cannot be
  // in one and missing from the other. A path only the hash comparison found has no git letter:
  // its bytes differ (`modified`) or its row outlived them (`deleted`).
  const entries: ChangeEntry[] = [];
  for (const path of changed) {
    entries.push(gitState.get(path) ?? { state: "modified", path });
  }
  for (const path of removed) {
    entries.push({ state: "deleted", path });
  }

  return {
    changed: [...changed].sort(byPath),
    removed: [...removed].sort(byPath),
    entries: entries.sort((a, b) => byPath(a.path, b.path)),
    unmatched: [...unmatched.values()].sort((a, b) => byPath(a.path, b.path)),
    degraded: degradedReason !== null,
    degradedReason,
  };
}
