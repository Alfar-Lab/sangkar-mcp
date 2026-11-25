//! `impact`'s manifest: the description a model routes on and the six arguments the handler parses
//! (`TOOLS-SPEC.md` §6), ported from `src/tools/impact/manifest.ts`.
//!
//! The description is the spec's text verbatim. Its first sentence is what tells an agent to call
//! this *before* an edit rather than after one, so a paraphrase would be a change to routing
//! behaviour, not to prose.
//!
//! `paths`, `base` and `cursor` carry no declared default because their absence is meaningful: no
//! `paths` means "measure the working-tree diff", no `base` means "the generation's own commit", and
//! no `cursor` means "the first page". A default here would make "the caller chose it" and "the
//! caller said nothing" the same request — which is exactly what [`ImpactArgs`] must keep apart so a
//! cursor binds to the call that minted it.
//!
//! ## The parameter vocabulary is declared here, not imported
//!
//! [`Param`], [`ParamType`], [`ParamDefault`] and [`ExampleValue`] are `trace`'s declarations field
//! for field, with the [`ParamDefault::List`] variant `status` added for an absent list argument.
//! They are duplicated rather than imported because tool isolation is a wall: `sangkar-boundary`'s
//! `every_source_rule_holds_for_the_real_tree` fails the build when one tool module names another
//! (`crate::trace`, `crate::status`), and a tool that reached across would also make its schema
//! depend on a sibling's release. TASK-034 has to publish one `tools/list` schema from all seven
//! tables, so it will move these four declarations into one crate-root module and delete the local
//! copies; the field names, variants and order below are `trace`'s exactly so that move rewrites no
//! data and no call site.

use sangkar_domain::neighbors::{EXPAND_DEPTH_MAX, clamp_depth};

/// The tool name as the cursor pins it: a token minted by one tool must not resume another.
pub const NAME: &str = "impact";

/// `TOOLS-SPEC.md` §6, verbatim, in the manifest's sentence order.
pub const DESCRIPTION: &str = concat!(
    "Given a change set, list what it can reach: dependents, affected entry points, and the tests ",
    "that cover it, with a risk score. Defaults to the working-tree diff. Call before editing, or ",
    "before merging."
);

/// `TOOLS-SPEC.md` §6's default hop count: the depth at which "what breaks" is still an answer.
pub const DEFAULT_DEPTH: i64 = 2;

/// `TOOLS-SPEC.md` §6's default budget: between `summary`'s 1200 and `trace`'s 3200.
pub const DEFAULT_BUDGET_TOKENS: u64 = 2_500;

/// The hop budget floor, declared here so the schema says so too.
pub const MIN_DEPTH: i64 = 1;

/// The hop budget ceiling: `expand`'s own clamp, not a second copy of it.
pub const MAX_DEPTH: i64 = EXPAND_DEPTH_MAX as i64;

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "enum"` with `values`, and `type: "array"` with `items`; JSON
/// Schema has no enum type (it is `type: "string"` plus `enum`) and no "array of enum" (it is
/// `items.enum`), so the conversion is the registry's, not this file's. An empty `values` slice on
/// [`ParamType::Array`] is the plain string array `paths` declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    /// An array whose every item is one of the set; empty means "any string".
    Array(&'static [&'static str]),
}

/// A declared default. Three shapes are all the seven manifests carry: a word, a number, and a
/// list (an absent `paths` is the empty list, not the repository root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamDefault {
    Text(&'static str),
    Integer(i64),
    List(&'static [&'static str]),
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

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = concat!(
    "Registered repository to answer from — its path or basename. ",
    "Omit it to use the working directory's repository, or the only registered one."
);

/// The six arguments, in the order `tools/list` publishes the properties and `ImpactArgs` declares
/// them.
///
/// Nothing is required: the cheapest useful call is the empty argument list, because the default
/// scope is the working-tree diff and the default base is the generation's own commit.
pub const PARAMETERS: [Param; 6] = [
    // No default: an absent `paths` is the working-tree diff, which is a different question from
    // "these paths changed" — and the two must not hash the same call.
    Param {
        name: "paths",
        ty: ParamType::Array(&[]),
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Explicit change set to analyse; omit it to measure the working-tree diff.",
    },
    Param {
        name: "base",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "The git revision the change set is diffed against.",
    },
    Param {
        name: "depth",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_DEPTH)),
        min: Some(MIN_DEPTH),
        max: Some(MAX_DEPTH),
        description: "How many caller hops the impact walk follows from the change set.",
    },
    Param {
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64)),
        min: None,
        max: None,
        description: "Token ceiling for the answer; a longer dependents list continues via cursor.",
    },
    Param {
        name: "cursor",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Continuation token a previous truncated answer returned.",
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
        description: REPO_DESCRIPTION,
    },
];

/// A call an agent can paste: the working-tree diff, two hops out, which is the question asked
/// before an edit rather than after one.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 1] = [("depth", ExampleValue::Integer(DEFAULT_DEPTH))];

/// The two scalar shapes a declared example uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    Text(&'static str),
    Integer(i64),
}

/// The arguments the handler parses, in the TypeScript's shape (`ImpactArgs`): every field is
/// optional, and an absent field means "take the declared default" rather than "the caller chose
/// the default".
///
/// Optionality is load-bearing beyond convenience: `src/index.ts` passes the caller's argv straight
/// through, and a `depth: 2` the caller did not type must hash the same as an absent one, so the
/// defaults are applied by the methods below and nowhere else.
///
/// `repo` is declared but never read by this handler: it selects which reader the composition root
/// builds, and a tool that resolved it itself would have to know the registry. It stays in the
/// struct so the parser and the schema agree on one argument list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImpactArgs {
    pub paths: Option<Vec<String>>,
    pub base: Option<String>,
    pub depth: Option<i64>,
    pub budget: Option<u64>,
    pub cursor: Option<String>,
    pub repo: Option<String>,
}

impl ImpactArgs {
    /// The hop count actually walked and hashed: the declared default, clamped to `1–4` by the
    /// module that owns the ceiling.
    ///
    /// The clamp happens before the digest so a `depth: 9` and a `depth: 4` are the same call — the
    /// walk they describe is identical, and a cursor minted by one must resume the other.
    #[must_use]
    pub fn depth(&self) -> u8 {
        clamp_depth(self.depth.unwrap_or(DEFAULT_DEPTH))
    }

    /// The token ceiling: the declared default when the caller did not ask.
    ///
    /// A `0` is passed through rather than replaced: `budget::emit` refuses it with
    /// `budget_too_large` and the floor, which is the honest answer to a ceiling nothing fits under.
    #[must_use]
    pub fn budget_or_default(&self) -> u64 {
        self.budget.unwrap_or(DEFAULT_BUDGET_TOKENS)
    }

    /// The revision the change set is measured against, or `None` when the caller named none — the
    /// handler then falls back to the generation's own base commit, and last of all to `HEAD`.
    #[must_use]
    pub fn base_or_none(&self) -> Option<&str> {
        self.base.as_deref().filter(|base| !base.is_empty())
    }

    /// The continuation token, or `None` for the first page. An empty string is "no cursor": a
    /// client that serialises an absent field as `""` must not be refused for it.
    #[must_use]
    pub fn cursor_or_none(&self) -> Option<&str> {
        self.cursor.as_deref().filter(|token| !token.is_empty())
    }

    /// The caller's `paths`, or the empty slice: `None` and `Some(&[])` both mean "no explicit
    /// change set", because both hash the same call and both ask for the working-tree diff.
    #[must_use]
    pub fn paths_slice(&self) -> &[String] {
        self.paths.as_deref().unwrap_or_default()
    }
}
