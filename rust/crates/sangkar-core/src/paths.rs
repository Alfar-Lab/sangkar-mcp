//! Repo-relative path formatting and the half-open prefix range.
//!
//! Ported from `src/shared/paths.ts`: the one place that turns a filesystem path into the form
//! stored in `node.file` and `node.qn`, and the only correct "is this path under that prefix" test.

/// Repo-relative path with forward slashes and no leading `./`.
///
/// `..` is kept as-is, because collapsing it silently would make an escaping path look contained.
#[must_use]
pub fn normalize_rel(path: &str) -> String {
    let slashed = path.replace('\\', "/");
    let mut kept: Vec<&str> = Vec::new();
    for segment in slashed.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        kept.push(segment);
    }
    kept.join("/")
}

/// `path::Scope.name`, the qualified name of a symbol.
///
/// A top-level symbol passes an empty scope and gets `path::name` rather than the malformed
/// `path::.name`.
#[must_use]
pub fn qn(rel_path: &str, scope: &str, name: &str) -> String {
    let path = normalize_rel(rel_path);
    if scope.is_empty() {
        format!("{path}::{name}")
    } else {
        format!("{path}::{scope}.{name}")
    }
}

/// `./x.js` from `src/a/b.ts` is `src/a/x`; `../c` from `src/a/b.ts` is `src/c`.
///
/// Collapses `.` and `..` and strips a trailing extension, then returns `None` when the specifier
/// climbs above the repository root — a module string is not a stored path, so [`normalize_rel`]'s
/// `..`-preserving rule does not apply. Absolute specifiers return `None`.
///
/// The answer is mechanical for a bare or `node:`-style specifier (`lodash` becomes
/// `src/a/lodash`): the resolver only reaches this function for a relative specifier, guarded by
/// the `is_relative_specifier` test, which is what keeps a package name out of a stored path.
#[must_use]
pub fn resolve_from(importer: &str, specifier: &str) -> Option<String> {
    let spec = specifier.replace('\\', "/");
    // A rooted specifier (POSIX or a Windows drive) is a filesystem path, and an empty one names
    // nothing: neither is a module inside this tree.
    if spec.is_empty() || spec.starts_with('/') || is_drive_rooted(&spec) {
        return None;
    }

    let from = normalize_rel(importer);
    let mut segments: Vec<&str> = match from.rfind('/') {
        Some(slash) => from[..slash].split('/').collect(),
        None => Vec::new(),
    };
    for segment in spec.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            // Nothing left to climb into: the specifier leaves the tree, which is not a stored path.
            if segments.is_empty() {
                return None;
            }
            segments.pop();
            continue;
        }
        segments.push(segment);
    }
    if segments.is_empty() {
        return None;
    }

    let last = segments.len() - 1;
    let name = segments[last];
    if let Some(dot) = name.rfind('.')
        && dot > 0
    {
        // `dot > 0` keeps a dotfile (`.eslintrc`) intact; only an extension after a name is stripped.
        segments[last] = &name[..dot];
    }
    Some(segments.join("/"))
}

fn is_drive_rooted(spec: &str) -> bool {
    let bytes = spec.as_bytes();
    bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/'
}

/// The half-open bounds of "path is under this prefix".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixBounds {
    /// The prefix itself, which sorts before `low` and is therefore matched by equality only.
    pub exact: String,
    /// Exclusive lower bound of the subtree range: `exact + "/"`.
    pub low: String,
    /// Exclusive upper bound: `exact + "0"`, the successor of `/` (0x2f) in ASCII.
    pub high: String,
}

/// The only correct bounds for "path is under this prefix": `path == exact || (path > low &&
/// path < high)`.
///
/// The naive `path >= exact && path < high` also matches siblings such as `src-old`, because `-`
/// (0x2d) sorts below `/` (0x2f) — the lower bound must be `exact + "/"`, not `exact`.
#[must_use]
pub fn prefix_bounds(prefix: &str) -> PrefixBounds {
    let exact = normalize_rel(prefix);
    let low = format!("{exact}/");
    // '/' + 1 is '0', so `high` is the first string the subtree range cannot contain.
    let high = format!("{exact}0");
    PrefixBounds { exact, low, high }
}

/// The membership test for [`prefix_bounds`], so callers cannot re-derive it wrongly.
#[must_use]
pub fn is_under_prefix(path: &str, prefix: &str) -> bool {
    let bounds = prefix_bounds(prefix);
    path == bounds.exact || (path > bounds.low.as_str() && path < bounds.high.as_str())
}
