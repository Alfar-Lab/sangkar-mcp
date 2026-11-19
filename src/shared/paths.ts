/**
 * Repo-relative path with forward slashes and no leading `./`, `../` kept as-is because
 * collapsing it silently would make an escaping path look contained. The one place that
 * turns a filesystem path into the form stored in `node.file` and `node.qn`.
 */
export function normalizeRel(p: string): string {
  const kept: string[] = [];
  for (const segment of p.replace(/\\/g, "/").split("/")) {
    if (segment === "" || segment === ".") {
      continue;
    }
    kept.push(segment);
  }
  return kept.join("/");
}

/**
 * `path::Scope.name`, the qualified name of a symbol. A top-level symbol passes an empty
 * scope and gets `path::name` rather than the malformed `path::.name`.
 */
export function qn(relPath: string, scope: string, name: string): string {
  const path = normalizeRel(relPath);
  return scope === "" ? `${path}::${name}` : `${path}::${scope}.${name}`;
}

/**
 * `./x.js` from `src/a/b.ts` is `src/a/x`; `../c` from `src/a/b.ts` is `src/c`. Collapses `.` and
 * `..` and strips a trailing extension, then returns `null` when the specifier climbs above the
 * repository root — a module string is not a stored path, so `normalizeRel`'s `..`-preserving rule
 * does not apply. Absolute specifiers return `null`.
 */
export function resolveFrom(importer: string, specifier: string): string | null {
  const spec = specifier.replace(/\\/gu, "/");
  // A rooted specifier (POSIX or a Windows drive) is a filesystem path, and an empty one names
  // nothing: neither is a module inside this tree.
  if (spec === "" || spec.startsWith("/") || /^[A-Za-z]:\//u.test(spec)) {
    return null;
  }
  const from = normalizeRel(importer);
  const slash = from.lastIndexOf("/");
  const segments = slash === -1 ? [] : from.slice(0, slash).split("/");
  for (const segment of spec.split("/")) {
    if (segment === "" || segment === ".") {
      continue;
    }
    if (segment === "..") {
      // Nothing left to climb into: the specifier leaves the tree, which is not a stored path.
      if (segments.length === 0) {
        return null;
      }
      segments.pop();
      continue;
    }
    segments.push(segment);
  }
  if (segments.length === 0) {
    return null;
  }
  const last = segments.length - 1;
  const name = segments[last] ?? "";
  const dot = name.lastIndexOf(".");
  // `dot > 0` keeps a dotfile (`.eslintrc`) intact; only an extension after a name is stripped.
  if (dot > 0) {
    segments[last] = name.slice(0, dot);
  }
  return segments.join("/");
}

export interface PathPrefixBounds {
  /** The prefix itself, which sorts before `low` and is therefore matched by equality only. */
  exact: string;
  /** Exclusive lower bound of the subtree range: `exact + "/"`. */
  low: string;
  /** Exclusive upper bound: `exact + "0"`, the successor of `/` (0x2f) in ASCII. */
  high: string;
}

/**
 * The only correct bounds for "path is under this prefix": `path === exact || (path > low &&
 * path < high)`. The naive `path >= exact && path < high` also matches siblings such as
 * `src-old`, because `-` (0x2d) sorts below `/` (0x2f) — the lower bound must be `exact + "/"`,
 * not `exact`.
 */
export function prefixBounds(prefix: string): PathPrefixBounds {
  const exact = normalizeRel(prefix);
  return { exact, low: `${exact}/`, high: `${exact}0` };
}

/** The membership test for `prefixBounds`, so callers cannot re-derive it wrongly. */
export function isUnderPrefix(path: string, prefix: string): boolean {
  const bounds = prefixBounds(prefix);
  return path === bounds.exact || (path > bounds.low && path < bounds.high);
}
