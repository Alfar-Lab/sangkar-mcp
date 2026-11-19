/**
 * The unresolved-edge taxonomy (#5): one row, one bucket, and a census that sums them.
 *
 * `unresolved` is what keeps an absence answer honest, but a bare total is not actionable — this
 * repository carries 9 166 rows at `g-d975f0`, and most of them are a stdlib call the indexer
 * will never bind, not a caller it missed. A bucket names *why* the row exists; `material` is the
 * narrower population that could still be user-defined code, which is the only part the absence
 * claim has to answer for.
 *
 * The rule order is the contract. A row whose evidence names an import is bucketed from its
 * specifier and from nothing else, so #12 (import failures recorded as `ambiguous` instead of
 * `dynamic`) can only move rows into the material count, never move them between buckets: the
 * specifier already says whether the module is internal, builtin or external.
 */
import type { UnresolvedRef } from "#shared/store/read/coverage";

/** The closed taxonomy a census reports, in the order `status` renders it. */
export const UNRESOLVED_BUCKETS = [
  "builtin_stdlib",
  "external_package",
  "internal_module",
  "dynamic_property_call",
  "receiver_type_unknown",
  "ambiguous",
] as const;
export type UnresolvedBucket = (typeof UNRESOLVED_BUCKETS)[number];

/**
 * Receivers and members that make a call site stdlib by construction.
 *
 * This pair is a recall trade-off, not a truth: a member name is a property name, so `cache.get`
 * and `watchers.get` are the same string to a static index, and an entry that is too broad hides a
 * real user-defined caller — the failure this taxonomy exists to prevent. That is why the material
 * predicate never trusts a bucket name alone, and why both sets are data: a missing entry costs
 * recall in `builtin_stdlib` and is fixed here without touching {@link bucketOf}.
 */
export const BUILTIN_RECEIVERS: ReadonlySet<string> = new Set([
  "Array",
  "Object",
  "Map",
  "Set",
  "Promise",
  "Date",
  "JSON",
  "Math",
  "String",
  "Number",
  "Boolean",
  "RegExp",
  "Error",
  "console",
  "fs",
  "path",
  "os",
  "process",
  "Buffer",
  "URLSearchParams",
]);

export const BUILTIN_MEMBERS: ReadonlySet<string> = new Set([
  "push",
  "pop",
  "shift",
  "unshift",
  "splice",
  "slice",
  "join",
  "map",
  "filter",
  "reduce",
  "forEach",
  "find",
  "includes",
  "indexOf",
  "split",
  "trim",
  "toLowerCase",
  "toUpperCase",
  "toISOString",
  "keys",
  "values",
  "entries",
  "has",
  "get",
  "set",
  "add",
  "delete",
  "existsSync",
  "readdirSync",
  "readFileSync",
  "writeFileSync",
  "mkdirSync",
  "statSync",
  "isDirectory",
  "isFile",
  "resolve",
  "dirname",
  "join",
  "basename",
]);

/**
 * Node and Bun modules that are builtins even without a `node:`/`bun:` prefix (`fs`, `path`,
 * `crypto`). Same trade-off as the allowlists above: the list is a seed, so an unlisted builtin
 * lands in `external_package` and stays visible rather than disappearing.
 */
const BUILTIN_MODULES: ReadonlySet<string> = new Set([
  "assert",
  "async_hooks",
  "buffer",
  "child_process",
  "cluster",
  "console",
  "constants",
  "crypto",
  "dgram",
  "diagnostics_channel",
  "dns",
  "domain",
  "events",
  "fs",
  "http",
  "http2",
  "https",
  "inspector",
  "module",
  "net",
  "os",
  "path",
  "perf_hooks",
  "process",
  "punycode",
  "querystring",
  "readline",
  "repl",
  "sqlite",
  "stream",
  "string_decoder",
  "test",
  "timers",
  "tls",
  "trace_events",
  "tty",
  "url",
  "util",
  "v8",
  "vm",
  "wasi",
  "worker_threads",
  "zlib",
]);

const IMPORT_PREFIX = "import:";
const CALL_PREFIX = "call:";

/**
 * The competing candidates #12 appends after the specifier ("import:./x.js candidates: a.ts |
 * b.ts"). The specifier is the text before the first space; without this cut the appended list
 * would be read as part of the module path.
 */
function specifierOf(evidence: string): string {
  const space = evidence.indexOf(" ");
  return space === -1 ? evidence : evidence.slice(0, space);
}

/** A specifier's own shape decides the bucket, so no later rule can move an import. */
function importBucket(specifier: string): UnresolvedBucket {
  if (specifier.startsWith("./") || specifier.startsWith("../") || specifier.startsWith("#")) {
    return "internal_module";
  }
  return isBuiltinModule(specifier) ? "builtin_stdlib" : "external_package";
}

/** `node:fs`, `bun:test`, and a bare `fs` or `fs/promises`: the base segment is the module. */
function isBuiltinModule(specifier: string): boolean {
  if (specifier.startsWith("node:") || specifier.startsWith("bun:")) {
    return true;
  }
  const slash = specifier.indexOf("/");
  return BUILTIN_MODULES.has(slash === -1 ? specifier : specifier.slice(0, slash));
}

/**
 * `call:A.b` reads as receiver `A` and member `b`. A bare `b` has an empty receiver, which no
 * allowlist can match, so only the member is consulted.
 */
function callBucket(target: string, evidenceKind: string | null): UnresolvedBucket {
  const dot = target.lastIndexOf(".");
  const receiver = dot === -1 ? "" : target.slice(0, dot);
  const member = dot === -1 ? target : target.slice(dot + 1);
  if (BUILTIN_RECEIVERS.has(receiver) || BUILTIN_MEMBERS.has(member)) {
    return "builtin_stdlib";
  }
  return evidenceKind === "dynamic" ? "dynamic_property_call" : "receiver_type_unknown";
}

/**
 * Why one row never bound. The import rule runs first on purpose: a bucket names what the
 * specifier *is*, so a changed `evidenceKind` (#12) can never move an import into a different
 * bucket — it only changes whether the row counts as {@link bucketCensus material}.
 */
export function bucketOf(row: UnresolvedRef): UnresolvedBucket {
  const evidence = row.evidence ?? "";
  if (evidence.startsWith(IMPORT_PREFIX)) {
    return importBucket(specifierOf(evidence.slice(IMPORT_PREFIX.length)));
  }
  if (row.evidenceKind === "ambiguous") {
    return "ambiguous";
  }
  if (evidence.startsWith(CALL_PREFIX)) {
    return callBucket(row.rawTarget, row.evidenceKind);
  }
  return "receiver_type_unknown";
}

/**
 * The census every answering payload carries: the per-bucket count and the `material` count.
 *
 * `material` asks "could a user-defined symbol be the real target?" and is computed from its own
 * predicate, never from a bucket name: `evidenceKind === "ambiguous"` (two candidates are
 * indistinguishable, so user code can be one of them) or `receiver_type_unknown`. A relative
 * import that matched nothing is a broken import, not a hidden caller, and `dynamic_property_call`
 * is non-material by construction — the extractor records it exactly when no declared target
 * exists.
 */
export function bucketCensus(rows: readonly UnresolvedRef[]): {
  readonly buckets: Readonly<Record<UnresolvedBucket, number>>;
  readonly material: number;
} {
  const buckets: Record<UnresolvedBucket, number> = {
    builtin_stdlib: 0,
    external_package: 0,
    internal_module: 0,
    dynamic_property_call: 0,
    receiver_type_unknown: 0,
    ambiguous: 0,
  };
  let material = 0;
  for (const row of rows) {
    const bucket = bucketOf(row);
    buckets[bucket] += 1;
    if (row.evidenceKind === "ambiguous" || bucket === "receiver_type_unknown") {
      material += 1;
    }
  }
  return { buckets, material };
}
