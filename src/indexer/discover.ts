import { createHash } from "node:crypto";
import type { Dirent } from "node:fs";
import { open, readdir, readFile, stat } from "node:fs/promises";
import { join } from "node:path";
import { log } from "#shared/log";
import { detectLanguage } from "./lang/registry";
import type { LangId } from "./lang/types";

/**
 * Discovery decides what the graph can and cannot contain, so it answers two questions in one
 * pass: which files are indexed (with the hash the incremental planner keys on) and why every
 * other candidate was left out. A file that is dropped without a recorded reason becomes an
 * invisible hole in coverage, which the envelope contract forbids.
 *
 * Output is a pure function of the tree's bytes and names: no timestamps, no filesystem readdir
 * order, no locale collation. Two runs over the same tree produce byte-identical JSON.
 */

/** Directories never walked, at any depth, whatever the ignore files say. */
export const ALWAYS_EXCLUDED: ReadonlySet<string> = new Set([
  ".git",
  "node_modules",
  ".sangkar",
  "dist",
  "vendor",
]);

/** Files larger than this are not read, not hashed and not indexed. */
export const SIZE_CAP_BYTES = 512 * 1024 * 1024;

/** The NUL probe window; a byte zero anywhere in it makes the file binary. */
const BINARY_PROBE_BYTES = 8192;

const HASH_CHUNK_BYTES = 1024 * 1024;

/** The two control files, read relative to the repository root only, plus the caller's extras. */
const CONTROL_FILES: readonly string[] = [".gitignore", ".sangkarignore"];

/** Why a path the walk saw is not part of the graph. The set is closed on purpose. */
export type SkipReason =
  | "ignored"
  | "binary"
  | "size_cap"
  | "unsupported_ext"
  /**
   * A directory the process could not open. It is the one reason whose path is a directory: a hole
   * nobody can see is worse than a hole that is reported, so an unreadable directory is a coverage
   * row rather than the end of the walk.
   */
  | "unreadable";

/**
 * The errors that mean "this directory cannot be read from here" rather than "the walk is broken".
 * `ENOENT` belongs with them: a directory that vanished between the parent's `readdir` and this call
 * could not be read either, and the next run sees the settled tree.
 */
const UNREADABLE_CODES: ReadonlySet<string> = new Set(["EACCES", "EPERM", "ENOENT"]);

/** One indexed file. `path` is repo-relative, forward-slashed, with no leading `./`. */
export interface DiscoveredFile {
  path: string;
  lang: LangId;
  /** Bytes on disk at discovery time; the parser reads exactly this file. */
  size: number;
  /** Lowercase hex sha256 of the whole file, the incremental planner's change key. */
  sha256: string;
}

/** One path that was classified and rejected, with the reason and, when known, its language. */
export interface NotIndexedFile {
  path: string;
  reason: SkipReason;
  /** Known whenever the extension is one of the 12, so a report can say "4 ignored TypeScript files". */
  language?: LangId;
}

export interface DiscoverResult {
  files: DiscoveredFile[];
  notIndexed: NotIndexedFile[];
  /**
   * How many paths the walk refused to index. It equals `notIndexed.length`; it exists so the
   * worker can log a count without materialising the list, and so it is obvious that every drop
   * is accounted for. Directories pruned from the walk are not paths and are not counted; the one
   * exception is a directory that could not be *read*, which is a coverage row with reason
   * `unreadable` because nothing else would report it.
   */
  skipped: number;
}

export interface DiscoverOptions {
  /** Extra gitignore-style patterns, applied after the two control files, so they win. */
  ignorePatterns?: readonly string[];
}

export async function discover(
  repoRoot: string,
  options: DiscoverOptions = {},
): Promise<DiscoverResult> {
  const rootInfo = await stat(repoRoot);
  if (!rootInfo.isDirectory()) {
    throw new Error(`discover: ${repoRoot} is not a directory`);
  }

  const rules = await loadIgnoreRules(repoRoot, options.ignorePatterns ?? []);
  const files: DiscoveredFile[] = [];
  const notIndexed: NotIndexedFile[] = [];

  await walk({ repoRoot, rules, files, notIndexed }, "");

  // Codepoint ordering, not `localeCompare`: a locale must never decide graph order.
  files.sort(compareByPath);
  notIndexed.sort(compareByPath);

  return { files, notIndexed, skipped: notIndexed.length };
}

interface WalkState {
  readonly repoRoot: string;
  readonly rules: readonly IgnoreRule[];
  readonly files: DiscoveredFile[];
  readonly notIndexed: NotIndexedFile[];
}

async function walk(state: WalkState, relDir: string): Promise<void> {
  const absDir = relDir === "" ? state.repoRoot : join(state.repoRoot, relDir);
  const entries = await readDirectory(state, relDir, absDir);
  if (entries === null) {
    return;
  }
  entries.sort((left, right) => compareStrings(left.name, right.name));

  for (const entry of entries) {
    const rel = relDir === "" ? entry.name : `${relDir}/${entry.name}`;

    // Symlinks are not followed: a link can point outside the tree or create a cycle, and the
    // indexed bytes must belong to the repository being indexed.
    if (entry.isSymbolicLink()) {
      continue;
    }

    if (entry.isDirectory()) {
      if (ALWAYS_EXCLUDED.has(entry.name) || isIgnored(rel, true, state.rules)) {
        continue;
      }
      await walk(state, rel);
      continue;
    }

    if (!entry.isFile()) {
      continue;
    }

    // The control files describe the walk itself; they are configuration, not content, so they
    // are neither indexed nor reported as a coverage gap.
    if (relDir === "" && CONTROL_FILES.includes(entry.name)) {
      continue;
    }

    await classifyFile(state, rel, join(state.repoRoot, rel));
  }
}

/**
 * One directory's entries, or `null` when a subdirectory could not be read. A `0700` directory owned
 * by another user is a hole in coverage, not a reason to abandon a walk that may already have hashed
 * gigabytes: it is recorded with reason `unreadable`, one line names it, and the walk continues. The
 * root is never skipped — `discover` has statted it already, so failing there is the caller's error.
 */
async function readDirectory(
  state: WalkState,
  relDir: string,
  absDir: string,
): Promise<Dirent[] | null> {
  try {
    return await readdir(absDir, { withFileTypes: true });
  } catch (error) {
    const code = (error as { code?: string }).code ?? "";
    if (relDir === "" || !UNREADABLE_CODES.has(code)) {
      throw error;
    }
    state.notIndexed.push({ path: relDir, reason: "unreadable" });
    log.warn("discover.unreadable", { path: relDir, code });
    return null;
  }
}

/**
 * Classification order is fixed and cheapest-first: ignore rules (a path decision, no I/O), then
 * the extension (no I/O), then size (one stat, before any read so a 1 GiB file is never opened),
 * then the binary probe, then hashing. The first decisive check wins, so the same file always gets
 * the same reason regardless of what else is wrong with it.
 */
async function classifyFile(state: WalkState, rel: string, abs: string): Promise<void> {
  const language = detectLanguage(rel);

  if (isIgnored(rel, false, state.rules)) {
    state.notIndexed.push(record(rel, "ignored", language));
    return;
  }

  if (language === null) {
    state.notIndexed.push(record(rel, "unsupported_ext", null));
    return;
  }

  const info = await stat(abs);
  if (info.size > SIZE_CAP_BYTES) {
    state.notIndexed.push(record(rel, "size_cap", language));
    return;
  }

  const sha256 = await hashUnlessBinary(abs);
  if (sha256 === null) {
    state.notIndexed.push(record(rel, "binary", language));
    return;
  }

  state.files.push({ path: rel, lang: language, size: info.size, sha256 });
}

/** A NUL byte in the first 8 KiB, or the sha256 of the whole file; `null` means binary. */
async function hashUnlessBinary(abs: string): Promise<string | null> {
  const handle = await open(abs, "r");
  try {
    const probe = Buffer.allocUnsafe(BINARY_PROBE_BYTES);
    const { bytesRead } = await handle.read(probe, 0, BINARY_PROBE_BYTES, 0);
    const head = probe.subarray(0, bytesRead);
    if (head.includes(0)) {
      return null;
    }

    const hash = createHash("sha256");
    hash.update(head);

    const chunk = Buffer.allocUnsafe(HASH_CHUNK_BYTES);
    let position = bytesRead;
    for (;;) {
      const read = await handle.read(chunk, 0, HASH_CHUNK_BYTES, position);
      if (read.bytesRead === 0) {
        break;
      }
      hash.update(chunk.subarray(0, read.bytesRead));
      position += read.bytesRead;
    }

    return hash.digest("hex");
  } finally {
    await handle.close();
  }
}

function record(path: string, reason: SkipReason, language: LangId | null): NotIndexedFile {
  return language === null ? { path, reason } : { path, reason, language };
}

/** One parsed ignore line. `anchored` mirrors gitignore: a slash anywhere but the end binds to root. */
interface IgnoreRule {
  readonly anchored: boolean;
  readonly dirOnly: boolean;
  readonly negated: boolean;
  readonly regex: RegExp;
}

async function loadIgnoreRules(
  repoRoot: string,
  extraPatterns: readonly string[],
): Promise<IgnoreRule[]> {
  const rules: IgnoreRule[] = [];
  for (const name of CONTROL_FILES) {
    rules.push(...compilePatterns(await readControlFile(join(repoRoot, name))));
  }
  // Later rules win, so the caller's patterns can re-include a path the control files dropped.
  rules.push(...compilePatterns(extraPatterns));
  return rules;
}

/** Lines of a control file; a missing file is an empty list, not a failure. */
async function readControlFile(abs: string): Promise<string[]> {
  try {
    return (await readFile(abs, "utf8")).split("\n");
  } catch (error) {
    if ((error as { code?: string }).code === "ENOENT") {
      return [];
    }
    throw error;
  }
}

function compilePatterns(lines: readonly string[]): IgnoreRule[] {
  const rules: IgnoreRule[] = [];
  for (const line of lines) {
    const rule = compilePattern(line);
    if (rule !== null) {
      rules.push(rule);
    }
  }
  return rules;
}

/**
 * The supported glob subset: `*` and `?` stay inside one segment, `**` crosses segments, a leading
 * `/` binds to the root, a trailing `/` matches directories only, `#` starts a comment and `!`
 * negates (last matching rule wins). Character classes and backslash escapes are not supported —
 * none of them appears in the `#`-free patterns a repository actually uses to hide build output.
 */
function compilePattern(line: string): IgnoreRule | null {
  let pattern = line.trimEnd();
  if (pattern === "" || pattern.startsWith("#")) {
    return null;
  }

  let negated = false;
  if (pattern.startsWith("!")) {
    negated = true;
    pattern = pattern.slice(1);
  }

  let dirOnly = false;
  if (pattern.endsWith("/")) {
    dirOnly = true;
    pattern = pattern.slice(0, -1);
  }

  // Anchor before stripping the leading slash, so `/build` stays root-relative.
  const anchored = pattern.includes("/");
  if (pattern.startsWith("/")) {
    pattern = pattern.slice(1);
  }
  if (pattern === "") {
    return null;
  }

  return { anchored, dirOnly, negated, regex: new RegExp(`^${globToRegExpSource(pattern)}$`) };
}

function globToRegExpSource(glob: string): string {
  let source = "";
  for (let index = 0; index < glob.length; index += 1) {
    const char = glob.charAt(index);
    if (char === "*") {
      if (glob.charAt(index + 1) === "*") {
        index += 1;
        if (glob.charAt(index + 1) === "/") {
          // `a/**/b` matches `a/b` as well as `a/x/y/b`.
          index += 1;
          source += "(?:[^/]*/)*";
        } else {
          source += ".*";
        }
      } else {
        source += "[^/]*";
      }
    } else if (char === "?") {
      source += "[^/]";
    } else {
      source += char.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    }
  }
  return source;
}

/**
 * A path is ignored when the last matching rule is not a negation. A pattern without a slash
 * matches the entry's own name, which is how `build/` hides `a/b/build` as well as `build`.
 */
function isIgnored(rel: string, isDir: boolean, rules: readonly IgnoreRule[]): boolean {
  const slash = rel.lastIndexOf("/");
  const name = slash === -1 ? rel : rel.slice(slash + 1);
  let ignored = false;
  for (const rule of rules) {
    if (rule.dirOnly && !isDir) {
      continue;
    }
    const subject = rule.anchored ? rel : name;
    if (!rule.regex.test(subject)) {
      continue;
    }
    ignored = !rule.negated;
  }
  return ignored;
}

function compareByPath(left: { path: string }, right: { path: string }): number {
  return compareStrings(left.path, right.path);
}

/** `<` on UTF-16 code units: stable, locale-free and identical on every platform. */
function compareStrings(left: string, right: string): number {
  if (left === right) {
    return 0;
  }
  return left < right ? -1 : 1;
}
