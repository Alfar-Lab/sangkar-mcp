//! `summary`'s manifest: the description a model routes on and the six arguments the handler parses
//! (`TECH-SPEC.md` §6), ported from `src/tools/summary/manifest.ts`.
//!
//! The description is reproduced verbatim, first sentence included: `TOOLS-SPEC.md` §4 opens with
//! "Summarize the current situation in a few hundred tokens", and an agent decides whether to call
//! the hot path from those bytes. A paraphrase would change the routing behaviour the spec was
//! written to produce.
//!
//! Three scopes, one enum, deliberately: six behaviours (three scopes × two sizes) would be the
//! `detail_level` knob the spec rejects. [`Scope::Change`] is the default because "what did I just
//! do" is the most common first question and the diff is the cheapest scope to answer it from.
//!
//! ## The parameter vocabulary is `trace`'s
//!
//! [`Param`], [`ParamType`], [`ParamDefault`] and [`ExampleValue`] are declared here with
//! [`crate::trace::manifest`]'s shape — an `Enum` variant carrying its values, a `min`/`max` pair, a
//! `Text`/`Integer` default and a `(key, value)` example — and not `status`'s narrower table, which
//! has neither bounds nor an enum. The registry (TASK-034) has to publish one `tools/list` schema
//! from all seven manifests, so every table must speak the richest vocabulary the seven need.
//!
//! The declarations are local rather than imported because tool isolation is a wall, not a
//! preference: `sangkar-boundary`'s `every_source_rule_holds_for_the_real_tree` fails the build when
//! one tool module names another ("shared machinery lives in sangkar-core, sangkar-store and
//! sangkar-domain"). That is also why `status` and `trace` already carry a copy each. The
//! reconciliation TASK-034 owes is therefore a *move* of these four declarations into one shared
//! type that the seven tables re-export — the field names, variants and order below are
//! `trace`'s exactly, so that move rewrites no data and no call site.
//!
//! Only `scope` carries a default here that the handler reads back, so the manifest is the single
//! owner of the hot path's 1200-token budget: `repo` is the cold scope and must not raise it
//! (`TOOLS-SPEC.md` §4).

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "enum"` with `values`, and `type: "array"` with `items`; JSON
/// Schema has no enum type (it is `type: "string"` plus `enum`) and no "array of enum" (it is
/// `items.enum`), so the conversion is the registry's, not this file's. An empty `values` slice on
/// [`ParamType::Array`] is the plain string array `status`'s `paths` declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    /// An array whose every item is one of the set; empty means "any string".
    Array(&'static [&'static str]),
}

/// A declared default. Two shapes are all this table carries, and [`ParamDefault::Text`] is the only
/// one an enum default can use.
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

/// The two scalar shapes a declared example uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    Text(&'static str),
    Integer(i64),
}

/// The tool name, as `tools/list` and a cursor's `tool` member spell it.
pub const NAME: &str = "summary";

/// `TOOLS-SPEC.md` §4, verbatim, in the manifest's sentence order.
pub const DESCRIPTION: &str = concat!(
    "Summarize the current situation in a few hundred tokens. ",
    "`scope=\"change\"` (default) describes the working-tree diff with risk, the top blast radius ",
    "and test gaps; `scope=\"repo\"` describes the repository; `scope=\"area\"` describes one path. ",
    "Call this first in any task to decide what to do next."
);

/// Which slice of the graph one call summarizes.
///
/// One enum for three payloads: the envelope is identical and only `payload` varies, so an agent
/// that has read one answer has read the shape of the other two.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// The working-tree diff against a base revision: the hot path.
    #[default]
    Change,
    /// The whole repository: size, languages, entry points, coupling and flows.
    Repo,
    /// One path's surface and how much depends on it.
    Area,
}

impl Scope {
    /// The three, in the order [`SCOPES`] publishes them.
    pub const ALL: [Self; 3] = [Self::Change, Self::Repo, Self::Area];

    /// The wire spelling, which is also the argument's accepted value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Change => "change",
            Self::Repo => "repo",
            Self::Area => "area",
        }
    }
}

impl std::fmt::Display for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The declared values of `scope`, in declaration order: the enum row's `values` and the set a
/// parser validates a call against are one list.
pub const SCOPES: [&str; 3] = ["change", "repo", "area"];

/// The default token budget of the hot path (`TOOLS-SPEC.md` §4).
///
/// 1200 and not more: this scope has to fit in "a few hundred tokens" of payload plus the envelope,
/// and the cold `repo` scope must not raise the ceiling every ordinary call pays.
pub const DEFAULT_BUDGET_TOKENS: u64 = 1_200;

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = concat!(
    "Registered repository to answer from — its path or basename. ",
    "Omit it to use the working directory's repository, or the only registered one."
);

/// The six arguments, in the order `tools/list` publishes the properties.
///
/// The order is the TypeScript literal's: `scope` first because it selects the payload, then the one
/// argument that belongs to a single scope (`target`), then `base`, `budget`, `cursor` and `repo`.
/// Nothing is required: the default scope answers from the generation's own base commit, so the
/// cheapest useful call is the empty argument list.
pub const PARAMETERS: [Param; 6] = [
    Param {
        name: "scope",
        ty: ParamType::Enum(&SCOPES),
        required: false,
        default: Some(ParamDefault::Text("change")),
        min: None,
        max: None,
        description: "Which slice to summarize: working-tree diff, whole repository, or one path.",
    },
    // Only meaningful for `area`; one param serving one scope keeps the scopes discoverable
    // together instead of growing a `--area`/`--repo-path` pair the schema would have to explain.
    Param {
        name: "target",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "The path the `area` scope describes; ignored by the other two scopes.",
    },
    // Defaults to the generation's `base_commit`, so the ordinary call spawns no git at all.
    Param {
        name: "base",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "The git revision the working-tree diff is taken against.",
    },
    Param {
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64)),
        min: None,
        max: None,
        description: "Token ceiling for the answer; this hot path must stay small.",
    },
    // Continuation for the scopes whose ranked or unbounded list does not fit one page.
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

/// A call an agent can paste: the default scope, which needs no other argument because the diff is
/// read from the generation's own base commit.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 1] = [("scope", ExampleValue::Text("change"))];

/// The arguments the handler parses, in the TypeScript's shape (`SummaryArgs`): every field is
/// optional, and an absent field means "take the declared default" rather than "the caller chose
/// the default".
///
/// Optionality is load-bearing beyond convenience: `src/index.ts` passes the caller's argv straight
/// through, and a `budget: 1200` the caller did not type must hash the same as an absent one
/// ([`SummaryArgs::budget_or_default`] is what [`crate::summary::handler::args_hash_of`] receives),
/// so the handler's own defaults are applied here and nowhere else.
///
/// `repo` is declared but never read by this handler: it selects which reader the composition root
/// builds, and a tool that resolved it itself would have to know the registry. It stays in the
/// struct so the parser and the schema agree on one argument list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SummaryArgs {
    pub scope: Option<Scope>,
    pub target: Option<String>,
    pub base: Option<String>,
    pub budget: Option<u64>,
    pub cursor: Option<String>,
    pub repo: Option<String>,
}

impl SummaryArgs {
    /// The scope to answer: `change` when the caller did not ask, because it is the hot path.
    #[must_use]
    pub fn scope_or_default(&self) -> Scope {
        self.scope.unwrap_or_default()
    }

    /// The token ceiling: the declared default when the caller did not ask.
    ///
    /// A `0` is passed through rather than replaced: `budget::emit` refuses it with
    /// `budget_too_large` and the floor, which is the honest answer to a ceiling nothing fits under.
    #[must_use]
    pub fn budget_or_default(&self) -> u64 {
        self.budget.unwrap_or(DEFAULT_BUDGET_TOKENS)
    }
}
