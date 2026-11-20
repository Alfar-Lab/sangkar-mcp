import { mkdirSync, readFileSync, realpathSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { AppError } from "#shared/errors";
import { type Logger, log } from "#shared/log";

/**
 * The global repo registry (`<home>/sangkar.json`): the list of trees this installation indexes.
 * The file is hand-editable and shared by every process on the machine, so every read is
 * defensive — a corrupt registry must degrade to "nothing registered", never to a crash in the
 * server, the daemon or the CLI.
 */

export const REGISTRY_VERSION = 1;

export interface Registry {
  readonly version: number;
  readonly repos: readonly string[];
}

/** Frozen because this instance is shared: a caller appending to it would corrupt every reader. */
export const EMPTY_REGISTRY: Registry = Object.freeze({
  version: REGISTRY_VERSION,
  repos: Object.freeze([]) as readonly string[],
});

export interface CanonicalRootOptions {
  /** The home directory `~/` expands against; injected so callers need not read `$HOME`. */
  readonly home?: string;
}

/**
 * One spelling for a repo root, so a path compared against the registry matches regardless of how
 * the caller wrote it: `~` expanded, relative forms resolved, and symlinks followed for a path that
 * exists. A path that does not exist keeps its resolved form — a deleted tree is still a registry
 * entry that other code reports, not grounds to rewrite the entry out of existence.
 */
export function canonicalRoot(path: string, options: CanonicalRootOptions = {}): string {
  // `os.homedir()` is `$HOME`, but this file must not read the environment: `#shared/config.ts`
  // is the only reader (plan constraint), and it hands its resolved home to every caller that
  // cares. Reading the variable here would make this a second settings reader.
  const home = options.home ?? homedir();
  const expanded = path.startsWith("~/") ? join(home, path.slice(2)) : path;
  const resolved = resolve(expanded);
  try {
    // `realpath` itself is the existence test: it rejects a missing path, which is the case that
    // keeps the resolved form (and there is no check-then-use race).
    return realpathSync(resolved);
  } catch {
    return resolved;
  }
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function warnLogger(options: { readonly log?: Logger }): Logger {
  return options.log ?? log;
}

/**
 * The registry on disk, or `EMPTY_REGISTRY` when the file is absent, unreadable, not the shape
 * this version writes, or otherwise untrustworthy. Each rejection logs exactly one warning that
 * names the reason, so a broken file is diagnosable from stderr without ever throwing.
 */
export function readRegistry(path: string, options: { readonly log?: Logger } = {}): Registry {
  const logger = warnLogger(options);
  const ignore = (reason: string): Registry => {
    logger.warn("registry ignored, using an empty registry", { path, reason });
    return EMPTY_REGISTRY;
  };

  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch (e) {
    return ignore(`unreadable: ${messageOf(e)}`);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch (e) {
    return ignore(`invalid JSON: ${messageOf(e)}`);
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    return ignore("not a JSON object");
  }

  const record = parsed as { version?: unknown; repos?: unknown };
  if (record.version !== REGISTRY_VERSION) {
    return ignore(`unsupported version ${JSON.stringify(record.version)}`);
  }
  if (!Array.isArray(record.repos)) {
    return ignore("repos is not an array");
  }

  const repos: string[] = [];
  let dropped = 0;
  for (const entry of record.repos) {
    if (typeof entry !== "string") {
      dropped += 1;
      continue;
    }
    // First occurrence keeps its position: the file's order is the operator's preference.
    const root = canonicalRoot(entry);
    if (!repos.includes(root)) {
      repos.push(root);
    }
  }
  if (dropped > 0) {
    logger.warn("registry entries dropped: not a path", { path, dropped });
  }

  return { version: REGISTRY_VERSION, repos };
}

/**
 * Replaces the registry atomically: the temp file plus a rename means a reader (the daemon polls
 * this file) sees either the previous list or the new one, never a truncated one.
 */
export function writeRegistry(path: string, registry: Registry): void {
  mkdirSync(dirname(path), { recursive: true });
  const temp = `${path}.tmp`;
  writeFileSync(temp, JSON.stringify({ version: REGISTRY_VERSION, repos: registry.repos }));
  renameSync(temp, path);
}

export interface RegisterResult {
  readonly registry: Registry;
  readonly added: boolean;
}

/**
 * Appends one canonicalised root. A failure to write is logged and swallowed — registering is a
 * convenience for the next call, so it must not take down the call that is already working.
 */
export function registerRepo(
  path: string,
  repoPath: string,
  options: { readonly log?: Logger } = {},
): RegisterResult {
  const logger = warnLogger(options);
  const registry = readRegistry(path, options);
  const root = canonicalRoot(repoPath);
  if (registry.repos.includes(root)) {
    return { registry, added: false };
  }
  const next: Registry = { version: REGISTRY_VERSION, repos: [...registry.repos, root] };
  try {
    writeRegistry(path, next);
  } catch (e) {
    logger.warn("registry not updated", { path, reason: messageOf(e) });
    return { registry, added: false };
  }
  return { registry: next, added: true };
}

export interface RemoveResult {
  readonly registry: Registry;
  readonly removed: boolean;
}

/** The inverse of `registerRepo`, with the same read-modify-write and swallowed-IO-error shape. */
export function removeRepo(
  path: string,
  repoPath: string,
  options: { readonly log?: Logger } = {},
): RemoveResult {
  const logger = warnLogger(options);
  const registry = readRegistry(path, options);
  const root = canonicalRoot(repoPath);
  const repos = registry.repos.filter((entry) => entry !== root);
  if (repos.length === registry.repos.length) {
    return { registry, removed: false };
  }
  const next: Registry = { version: REGISTRY_VERSION, repos };
  try {
    writeRegistry(path, next);
  } catch (e) {
    logger.warn("registry not updated", { path, reason: messageOf(e) });
    return { registry, removed: false };
  }
  return { registry: next, removed: true };
}

export interface Selection {
  readonly ok: true;
  readonly repoRoot: string;
}

export interface SelectionFailure {
  readonly ok: false;
  readonly error: AppError;
}

export interface SelectRepoInput {
  readonly registry: Registry;
  /** What the caller asked for by name or path: outranks the pin and every implicit default. */
  readonly requested?: string | undefined;
  readonly cwd: string;
  /** `--repo-root`: a process-level default, not a barrier that hides a bad `requested`. */
  readonly pinnedRoot?: string | null;
}

/** The one-line-per-repo hint every refusal carries, so the caller can act without guessing. */
function registryHint(registry: Registry): string {
  if (registry.repos.length === 0) {
    return "sangkar repo add <path>";
  }
  return ["Registered repos:", ...registry.repos.map((repo) => `  ${repo}`)].join("\n");
}

/** A request is a path unless it starts with `~`, which `canonicalRoot` must expand itself. */
function againstCwd(root: string, cwd: string): string {
  return root.startsWith("~/") ? root : resolve(cwd, root);
}

/**
 * Resolves the repository a call is about. Order is the whole contract:
 *
 * 1. a supplied `requested` — matched as a path, then as a basename — always wins, because an
 *    explicit request that silently fell back to a pin is a wrong answer, not a default;
 * 2. otherwise a non-empty `pinnedRoot` wins without reading the registry, which keeps ad-hoc
 *    invocations (`--repo-root <unregistered tree>`) working;
 * 3. otherwise the working directory, then the only registered repo, then a refusal.
 *
 * Both refusals are `not_found` / `ambiguous` — the existing closed union, no new code — with a
 * hint listing the registry.
 */
export function selectRepo(input: SelectRepoInput): Selection | SelectionFailure {
  const { registry, pinnedRoot } = input;
  const requested = input.requested;
  const hint = registryHint(registry);

  if (requested !== undefined && requested.trim() !== "") {
    const requestedRoot = canonicalRoot(againstCwd(requested, input.cwd));
    if (registry.repos.includes(requestedRoot)) {
      return { ok: true, repoRoot: requestedRoot };
    }
    const name = basename(requestedRoot);
    const byName = registry.repos.filter((repo) => basename(repo) === name);
    const single = byName[0];
    if (byName.length === 1 && single !== undefined) {
      return { ok: true, repoRoot: single };
    }
    if (byName.length > 1) {
      return {
        ok: false,
        error: new AppError("ambiguous", `${byName.length} registered repos are named "${name}"`, {
          hint,
        }),
      };
    }
    return {
      ok: false,
      error: new AppError("not_found", `repo "${requested}" is not registered`, { hint }),
    };
  }

  if (pinnedRoot !== undefined && pinnedRoot !== null && pinnedRoot.trim() !== "") {
    return { ok: true, repoRoot: canonicalRoot(pinnedRoot) };
  }

  const cwdRoot = canonicalRoot(input.cwd);
  if (registry.repos.includes(cwdRoot)) {
    return { ok: true, repoRoot: cwdRoot };
  }
  const only = registry.repos[0];
  if (registry.repos.length === 1 && only !== undefined) {
    return { ok: true, repoRoot: only };
  }
  if (registry.repos.length === 0) {
    return {
      ok: false,
      error: new AppError("not_found", "no repository is registered", { hint }),
    };
  }
  return {
    ok: false,
    error: new AppError(
      "ambiguous",
      `several repositories are registered and none matches ${cwdRoot}`,
      { hint },
    ),
  };
}
