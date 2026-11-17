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
