//! The unresolved-edge taxonomy: one row, one bucket, and a census that sums them.
//!
//! Ported from `src/shared/domain/unresolved.ts`. `unresolved` is what keeps an absence answer
//! honest, but a bare total is not actionable — most rows in a real graph are a stdlib call the
//! indexer will never bind, not a caller it missed. A bucket names *why* the row exists; the
//! `material` count is the narrower population that could still be user-defined code, which is the
//! only part an absence claim has to answer for.
//!
//! The rule order is the contract. An `import:` evidence is bucketed from its specifier and from
//! nothing else, so a writer that records an import failure as `ambiguous` can only move the row
//! into `material`, never between buckets. After that the writer's `evidence_kind` is read before
//! any text: an `ambiguous` row has no single target to name, so it is bucketed as
//! [`UnresolvedBucket::ReceiverTypeUnknown`] instead of by a `call:` target it never bound.
//!
//! The TypeScript module publishes six labels; this port's census is the five below, because
//! `ambiguous` describes the candidate set rather than the target and survives as the material
//! predicate instead of a bucket every payload would have to explain twice.

use sangkar_store::read::coverage::UnresolvedRef;

/// The five labels a census reports, in the order it reports them.
pub const UNRESOLVED_BUCKETS: [&str; 6] = [
    "builtin_stdlib",
    "external_package",
    "internal_module",
    "dynamic_property_call",
    "receiver_type_unknown",
    "ambiguous",
];

/// The six labels, as values a classifier can return.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnresolvedBucket {
    BuiltinStdlib,
    ExternalPackage,
    InternalModule,
    DynamicPropertyCall,
    ReceiverTypeUnknown,
    Ambiguous,
}

impl UnresolvedBucket {
    /// The six, in [`UNRESOLVED_BUCKETS`] order.
    pub const ALL: [Self; 6] = [
        Self::BuiltinStdlib,
        Self::ExternalPackage,
        Self::InternalModule,
        Self::DynamicPropertyCall,
        Self::ReceiverTypeUnknown,
        Self::Ambiguous,
    ];

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BuiltinStdlib => "builtin_stdlib",
            Self::ExternalPackage => "external_package",
            Self::InternalModule => "internal_module",
            Self::DynamicPropertyCall => "dynamic_property_call",
            Self::ReceiverTypeUnknown => "receiver_type_unknown",
            Self::Ambiguous => "ambiguous",
        }
    }

    /// The position in [`UNRESOLVED_BUCKETS`], so a census can order without sorting.
    const fn slot(self) -> usize {
        match self {
            Self::BuiltinStdlib => 0,
            Self::ExternalPackage => 1,
            Self::InternalModule => 2,
            Self::DynamicPropertyCall => 3,
            Self::ReceiverTypeUnknown => 4,
            Self::Ambiguous => 5,
        }
    }
}

impl std::fmt::Display for UnresolvedBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Receivers that make a dotted call site stdlib by construction.
///
/// This pair is a recall trade-off, not a truth: a member name is a property name, so
/// `cache.get` and `watchers.get` are the same string to a static index, and an entry that is too
/// broad hides a real user-defined caller — the failure this taxonomy exists to prevent. Both
/// lists are data: a missing entry costs recall in `builtin_stdlib` and is fixed here without
/// touching [`bucket_of`].
pub const BUILTIN_RECEIVERS: [&str; 20] = [
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
];

/// Members that make a call site stdlib by construction, copied from the TypeScript set in order.
///
/// `join` is listed twice in the source, which is a `Set` there, so the repeat has no effect here
/// either; it is kept untidied because the list is data, not judgement.
pub const BUILTIN_MEMBERS: [&str; 39] = [
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
];

/// Node and Bun modules that are builtins even without a `node:`/`bun:` prefix (`fs`, `path`,
/// `crypto`). Same trade-off as the allowlists above: the list is a seed, so an unlisted builtin
/// lands in `external_package` and stays visible rather than disappearing.
const BUILTIN_MODULES: [&str; 43] = [
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
];

const IMPORT_PREFIX: &str = "import:";
const CALL_PREFIX: &str = "call:";

/// The specifier is the text before the first space; the competing candidates appended after it
/// (`import:./x.js candidates: a.ts | b.ts`) must not be read as part of the module path.
fn specifier_of(evidence: &str) -> &str {
    match evidence.split_once(' ') {
        Some((specifier, _)) => specifier,
        None => evidence,
    }
}

/// A specifier's own shape decides the bucket, so no later rule can move an import.
fn import_bucket(specifier: &str) -> UnresolvedBucket {
    if specifier.starts_with("./") || specifier.starts_with("../") || specifier.starts_with('#') {
        return UnresolvedBucket::InternalModule;
    }
    if is_builtin_module(specifier) {
        UnresolvedBucket::BuiltinStdlib
    } else {
        UnresolvedBucket::ExternalPackage
    }
}

/// `node:fs`, `bun:test`, and a bare `fs` or `fs/promises`: the base segment is the module.
fn is_builtin_module(specifier: &str) -> bool {
    if specifier.starts_with("node:") || specifier.starts_with("bun:") {
        return true;
    }
    let base = match specifier.split_once('/') {
        Some((base, _)) => base,
        None => specifier,
    };
    BUILTIN_MODULES.contains(&base)
}

/// `call:A.b` reads as receiver `A` and member `b`. A bare `b` has an empty receiver, which no
/// allowlist can match, so only the member is consulted.
fn call_bucket(target: &str, evidence_kind: Option<&str>) -> UnresolvedBucket {
    let (receiver, member) = match target.rfind('.') {
        Some(dot) => (&target[..dot], &target[dot + 1..]),
        None => ("", target),
    };
    if BUILTIN_RECEIVERS.contains(&receiver) || BUILTIN_MEMBERS.contains(&member) {
        return UnresolvedBucket::BuiltinStdlib;
    }
    if evidence_kind == Some("dynamic") {
        UnresolvedBucket::DynamicPropertyCall
    } else {
        UnresolvedBucket::ReceiverTypeUnknown
    }
}

/// Why one row never bound.
///
/// The import rule runs first on purpose: a bucket names what the specifier *is*, so a changed
/// `evidence_kind` can never move an import into a different bucket — it only changes whether the
/// row counts as [`bucket_census`] material.
#[must_use]
pub fn bucket_of(row: &UnresolvedRef) -> UnresolvedBucket {
    let evidence = row.evidence.as_deref().unwrap_or("");
    if let Some(specifier) = evidence.strip_prefix(IMPORT_PREFIX) {
        return import_bucket(specifier_of(specifier));
    }
    if row.evidence_kind.as_deref() == Some("ambiguous") {
        return UnresolvedBucket::Ambiguous;
    }
    if evidence.starts_with(CALL_PREFIX) {
        return call_bucket(&row.raw_target, row.evidence_kind.as_deref());
    }
    UnresolvedBucket::ReceiverTypeUnknown
}

/// Whether a row could still name user-defined code.
///
/// This is its own predicate, never a bucket name: an `ambiguous` kind means two candidates are
/// indistinguishable, so user code can be one of them, and a `receiver_type_unknown` row has no
/// target the index could establish. A broken relative import is a defect in the tree rather than
/// a hidden caller, and `dynamic_property_call` is non-material by construction — the extractor
/// records it exactly when no declared target exists.
fn is_material(row: &UnresolvedRef, bucket: UnresolvedBucket) -> bool {
    row.evidence_kind.as_deref() == Some("ambiguous")
        || bucket == UnresolvedBucket::ReceiverTypeUnknown
}

/// One bucket's count, present only when it is non-zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BucketCount {
    pub bucket: UnresolvedBucket,
    pub count: usize,
}

/// The census every answering payload carries beside the unchanged `coverage.unresolved_edges`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedCensus {
    /// The number of rows the census was built from.
    pub total: usize,
    /// Rows that could still be user-defined code; never more than [`Self::total`].
    pub material: usize,
    /// The non-zero buckets only, in [`UNRESOLVED_BUCKETS`] order.
    pub buckets: Vec<BucketCount>,
}

impl UnresolvedCensus {
    /// The count for one bucket, `0` when it is absent from [`Self::buckets`].
    #[must_use]
    pub fn count(&self, bucket: UnresolvedBucket) -> usize {
        self.buckets
            .iter()
            .find(|entry| entry.bucket == bucket)
            .map_or(0, |entry| entry.count)
    }
}

/// The census every answering payload carries: the per-bucket count and the `material` count.
///
/// Both numbers come from one pass over `rows`, so `material <= total` and the listed counts
/// summing to `total` are true by construction rather than re-derived by a caller.
#[must_use]
pub fn bucket_census(rows: &[UnresolvedRef]) -> UnresolvedCensus {
    let mut counts = [0_usize; UNRESOLVED_BUCKETS.len()];
    let mut material = 0_usize;
    for row in rows {
        let bucket = bucket_of(row);
        counts[bucket.slot()] += 1;
        if is_material(row, bucket) {
            material += 1;
        }
    }
    let buckets = UnresolvedBucket::ALL
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|(bucket, count)| BucketCount {
            bucket: *bucket,
            count,
        })
        .collect();
    UnresolvedCensus {
        total: rows.len(),
        material,
        buckets,
    }
}
