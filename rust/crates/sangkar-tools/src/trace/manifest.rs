//! `trace`'s manifest: the description a model sees and the arguments the handler parses
//! (`TECH-SPEC.md` §6).
//!
//! Ported from `src/tools/trace/manifest.ts`. The description is verbatim from `TOOLS-SPEC.md` §3 —
//! it is the only text a model reads before deciding to call the tool, so paraphrasing it would
//! change the routing behaviour the spec was written to produce. [`PARAMETERS`] is data, not a
//! validator: the registry (TASK-034) turns it into the `tools/list` schema, and [`TraceArgs`] is its
//! Rust mirror, so the handler never re-derives a default.
//!
//! The node-kind list is local on purpose. A tool may not read the indexer's `NODE_KINDS` (the
//! module boundary is `tests/boundary.test.ts` rule 1), so this table is the read-side vocabulary
//! `DATABASE.md` §3 fixes for `node.kind`. The unresolved buckets are *not* copied for the same
//! reason: [`PARAMETERS`] spreads `sangkar_domain::unresolved`'s own list, so a bucket added to the
//! taxonomy is filterable here in the same commit and a stale copy cannot refuse a real bucket.
//!
//! Two declared values are not free choices, so they are read from the module that owns them rather
//! than written down twice: the depth ceiling is `neighbors::EXPAND_DEPTH_MAX` (the clamp `expand`
//! applies) and [`TraceArgs::depth`] delegates to `neighbors::clamp_depth`. A caller can therefore
//! never widen the walk past the contract's `1–4`, and the summary's hop count is the depth actually
//! walked.

use sangkar_domain::neighbors::{Direction, EXPAND_DEPTH_MAX, clamp_depth};
use sangkar_domain::unresolved::{UNRESOLVED_BUCKETS, UnresolvedBucket};

/// The tool name as the cursor pins it: a token minted by one tool must not resume another.
pub const NAME: &str = "trace";

/// `TOOLS-SPEC.md` §3, verbatim. Four sentences, in this order, because a model routes on them.
pub const DESCRIPTION: &str = concat!(
    "Show what is connected to one symbol: callers, callees, implementers, importers and tests. ",
    "Accepts a function or method name, a dotted name, a file path, or `@decorator`. Call this ",
    "instead of reading files to discover relationships. When several symbols match it returns ",
    "`status: \"ambiguous\"` with candidates instead of guessing. Does not return source code."
);

/// `node.kind` values a query may be narrowed to; `kinds` is all-or-nothing like `find_by_name`.
pub const NODE_KINDS: [&str; 7] = [
    "file", "module", "class", "function", "method", "test", "endpoint",
];

/// Which way to walk. `all` is the common question; `in`/`out` exist for when the agent knows.
pub const DIRECTIONS: [&str; 3] = ["all", "in", "out"];

/// `TOOLS-SPEC.md` §3's default hop count: two hops of a dense symbol.
pub const DEFAULT_DEPTH: i64 = 2;

/// `TOOLS-SPEC.md` §3's default budget, CRG's `detect_changes` size.
pub const DEFAULT_BUDGET_TOKENS: u64 = 3_200;

/// The hop budget floor, declared here so the schema says so too.
pub const MIN_DEPTH: i64 = 1;

/// The hop budget ceiling: `expand`'s own clamp, not a second copy of it.
pub const MAX_DEPTH: i64 = EXPAND_DEPTH_MAX as i64;

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "enum"` with `values`, and `type: "array"` with `items`;
/// JSON Schema has no enum type (it is `type: "string"` plus `enum`) and no "array of enum" (it is
/// `items.enum`), so the conversion is the registry's, not this file's. Keeping the declaration in
/// the manifest's own terms is what lets one derivation serve all seven tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    /// An array whose every item is one of the set.
    Array(&'static [&'static str]),
}

/// A declared default. Two shapes are all the schema carries, so the enum has two variants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamDefault {
    Text(&'static str),
    Integer(i64),
}

/// One declared argument. Field order is the TypeScript literal's, so the table reads as the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: &'static str,
    pub ty: ParamType,
    pub required: bool,
    pub default: Option<ParamDefault>,
    /// Included in the schema as `min`; only an integer declares one.
    pub min: Option<i64>,
    /// Included in the schema as `max`; only an integer declares one.
    pub max: Option<i64>,
    pub description: &'static str,
}

/// The argument table, in the order `tools/list` publishes the properties.
///
/// `query` is the only required argument (`TOOLS-SPEC.md` §1: at most one required arg per tool,
/// because argument count is a bigger error source than argument type). `cursor` continues *this*
/// query and replaces `limit`/`offset`/`max_results`. Each description says what the argument
/// selects and nothing else: the type, the enum values and the default are already in the same row,
/// and the substance is §3's `why` column, one clause per argument.
pub const PARAMETERS: [Param; 9] = [
    Param {
        name: "query",
        ty: ParamType::Str,
        required: true,
        default: None,
        min: None,
        max: None,
        description: "The symbol to look up: a name, dotted name, path or `@decorator`.",
    },
    Param {
        name: "direction",
        ty: ParamType::Enum(&DIRECTIONS),
        required: false,
        default: Some(ParamDefault::Text("all")),
        min: None,
        max: None,
        description: "Which way to walk from the symbol: in, out or both.",
    },
    Param {
        name: "kinds",
        ty: ParamType::Array(&NODE_KINDS),
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Narrow resolution to these node kinds when a name collides across kinds.",
    },
    Param {
        name: "path",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Repo-relative prefix: the path itself and everything under it.",
    },
    Param {
        name: "depth",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_DEPTH)),
        min: Some(MIN_DEPTH),
        max: Some(MAX_DEPTH),
        description: "How many hops of callers and callees to walk from the symbol.",
    },
    Param {
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64)),
        min: None,
        max: None,
        description: "Token ceiling for this result; exceeding it truncates and sets `next`.",
    },
    Param {
        name: "unresolved",
        ty: ParamType::Enum(&UNRESOLVED_BUCKETS),
        required: false,
        default: None,
        min: None,
        max: None,
        description: "List only this unresolved bucket; the census stays unfiltered.",
    },
    Param {
        name: "cursor",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Continue this same query from the `next` cursor of a previous page.",
    },
    // No default: an absent `repo` means "unselected", which the root answers from the working
    // directory; a default here would make that indistinguishable from a named repository.
    Param {
        name: "repo",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "Registered repository to answer from — its path or basename. ",
            "Omit it to use the working directory's repository, or the only registered one."
        ),
    },
];

/// One call a caller can paste verbatim: the schema says which arguments exist, not what a real one
/// looks like, and an agent with no example reaches for a file reader instead.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 2] = [
    ("query", ExampleValue::Text("handleRequest")),
    ("depth", ExampleValue::Integer(1)),
];

/// The two scalar shapes the one declared example uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    Text(&'static str),
    Integer(i64),
}

/// The arguments the handler parses, in the TypeScript's shape (`TraceArgs`): every field but
/// `query` is optional, and an absent field means "take the declared default" rather than "the
/// caller chose the default".
///
/// Optionality is load-bearing beyond convenience: `src/index.ts` passes the caller's argv straight
/// through, and a `depth: 2` the caller did not type must hash the same as an absent one
/// ([`crate::trace::handler::args_hash_of`]), so the handler's own defaults are applied here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceArgs {
    pub query: String,
    pub direction: Option<Direction>,
    /// Node-kind narrowing, applied at resolution only — `expand` filters edge kinds, not node kinds.
    pub kinds: Option<Vec<String>>,
    /// Repo-relative prefix, `search`'s semantics: it narrows resolution and the near misses, so a
    /// filtered tie can resolve and a filtered miss never answers from outside the prefix.
    pub path: Option<String>,
    pub depth: Option<i64>,
    pub budget: Option<u64>,
    /// Narrow the `unresolved` list to one bucket. The census describes the unfiltered scope either
    /// way, because a filter chooses what to read, not which call sites failed to bind.
    pub unresolved: Option<UnresolvedBucket>,
    pub cursor: Option<String>,
    pub repo: Option<String>,
}

impl TraceArgs {
    /// The one required argument; everything else is absent until a caller says otherwise.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            direction: None,
            kinds: None,
            path: None,
            depth: None,
            budget: None,
            unresolved: None,
            cursor: None,
            repo: None,
        }
    }

    /// The direction to walk: `all` when the caller did not ask.
    #[must_use]
    pub fn direction_or_all(&self) -> Direction {
        self.direction.unwrap_or(Direction::All)
    }

    /// The hop count actually walked: the declared default, clamped to `1–4` by the module that owns
    /// the ceiling.
    #[must_use]
    pub fn depth(&self) -> u8 {
        clamp_depth(self.depth.unwrap_or(DEFAULT_DEPTH))
    }

    /// The token ceiling: the declared default when the caller did not ask.
    #[must_use]
    pub fn budget_or_default(&self) -> u64 {
        self.budget.unwrap_or(DEFAULT_BUDGET_TOKENS)
    }
}
