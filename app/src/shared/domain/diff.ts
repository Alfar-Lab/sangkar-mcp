/**
 * The one change-set computation: what differs from `base`, what is gone, and what could not be
 * attributed to a file row. `summary(change)` and `impact` both read this and neither recomputes
 * it (plan.constraint_invariant.md: one change-set, never one per caller).
 *
 * Two sources are unioned, because neither is right alone:
 * - `git diff --name-status` knows what changed against a commit, including deletions, and it
 *   still sees paths the indexer never parsed (a modified `README.md`). It never sees untracked
 *   files.
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

/** The change set both change-aware tools read; the arrays are sorted and repo-relative. */
export interface ChangeSet {
  /** Differ from `base`, or no longer match `file.sha256`; sorted, forward slashes. */
  readonly changed: readonly string[];
  /** Had a `file` row and their bytes are gone from the working tree; sorted. */
  readonly removed: readonly string[];
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

/** `git diff --name-status` against `base`, bounded by `timeoutMs` and never interactive. */
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
    "--no-renames",
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
  /** The `--name-status` letter: `M`, `A`, `D`, `T`, … */
  readonly status: string;
  /** As git reported it, before containment and normalisation. */
  readonly path: string;
}

/**
 * `-z` writes `STATUS\0path\0` per entry, and `R`/`C` add a second path (the destination). We pass
 * `--no-renames`, so a rename pair would mean git ignored the flag: `null` reports unparseable
 * output rather than letting a silently dropped path look like "no change".
 */
function parseNameStatus(stdout: string): readonly GitEntry[] | null {
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
    const rename = status.startsWith("R") || status.startsWith("C");
    const path = tokens[index + (rename ? 1 : 0)];
    index += rename ? 2 : 1;
    if (path === undefined) {
      return null;
    }
    entries.push({ status: status.slice(0, 1), path });
  }
  return entries;
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
 * The read handle is the third parameter on purpose: `shared/domain` decides what is true but
 * never opens a database (the caller passes the reader it already holds), and the hash comparison
 * cannot be done without the indexed digests.
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
  const timeoutMs = options.gitTimeoutMs ?? GIT_TIMEOUT_MS;
  const root = resolve(repoRoot);

  const digests = indexedDigests(conn);
  const indexed = new Set<string>();
  const changed = new Set<string>();
  const removed = new Set<string>();
  const unmatched = new Map<string, UnmatchedChange>();

  const classify = (path: string): boolean => indexed.has(path) || languageFor(path) !== null;
  const unclassified = (path: string, reason: UnmatchedReason): void => {
    unmatched.set(path, { path, reason });
  };

  // Source 1: git. `base` empty means "the last commit", the usual change scope for a summary.
  const outcome = await gitNameStatus(root, base === "" ? "HEAD" : base, timeoutMs, options.env);
  let degradedReason: string | null = null;
  if (outcome.ok) {
    const entries = parseNameStatus(outcome.stdout);
    if (entries === null) {
      degradedReason = "git diff --name-status -z produced output this build cannot parse";
    } else {
      for (const entry of entries) {
        const path = repoRelative(root, entry.path);
        if (path === null) {
          continue;
        }
        if (entry.status === "D") {
          if (classify(path)) {
            removed.add(path);
          } else {
            unclassified(path, "no_file_row_and_unparseable_language");
          }
          continue;
        }
        // `M`, `A`, `T`, `U`, … all mean the working tree holds different bytes than `base`.
        if (classify(path)) {
          changed.add(path);
        } else {
          unclassified(path, "no_file_row_and_unparseable_language");
        }
      }
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
  return {
    changed: [...changed].sort(byPath),
    removed: [...removed].sort(byPath),
    unmatched: [...unmatched.values()].sort((a, b) => byPath(a.path, b.path)),
    degraded: degradedReason !== null,
    degradedReason,
  };
}
