//! `expand`'s manifest: the description a model routes on and the six arguments the handler parses
//! (`TECH-SPEC.md` §6), ported from `src/tools/expand/manifest.ts`.
//!
//! The description is verbatim `TOOLS-SPEC.md` §7, first sentence and last clause included: "says
//! which it chose" is the promise the handler keeps with `payload.what` and `payload.why`, so a
//! paraphrase here would be a different contract from the one the spec argues for.
//!
//! The six arguments are the whole surface, and that smallness is the point: `expand` is the one
//! address for detail, which is what lets the other six tools ship summaries instead of a
//! `detail_level` knob. `path` is the same prefix filter `trace` and `search` take, because "the
//! `Widget` under `tests/`" is a handle collision an agent can meet and has no other way to
//! express. `budget` defaults higher than `summary`'s because detail is by definition the expensive
//! part.
//!
//! ## There is no `cursor`, and the absence is the product statement
//!
//! `TOOLS-SPEC.md` §7's argument table has no continuation argument, and neither does this table: a
//! deliberately truncated `expand` answer is *finished*, not resumable — the caller raises `budget`
//! or narrows `what` instead of replaying a token. [`PARAMETERS`] is what `tools/list` publishes, so
//! declaring one here would be the whole difference between "not offered" and "offered and
//! rejected". The handler therefore publishes a cut without a continuation
//! ([`crate::expand::handler`] explains why an empty token is not served either).
//!
//! ## `range` is a pair, and the schema cannot say so
//!
//! `range` exists because a source view is priced in lines: a caller that already knows which lines
//! matter should not pay for the rest, and the tool should not grow a second window argument beside
//! it. It is `[from, to]`, 1-based and inclusive, and it narrows `what: "source"` only. JSON Schema
//! can say "an array of integers" but not "exactly two", so the handler defends itself instead of
//! trusting the parser's width.
//!
//! ## The parameter vocabulary is `trace`'s
//!
//! [`Param`], [`ParamType`], [`ParamDefault`] and [`ExampleValue`] are declared here with
//! [`crate::trace::manifest`]'s shape — an `Enum` variant carrying its values, a `min`/`max` pair, a
//! `Text`/`Integer` default and a `(key, value)` example — plus one variant the other two tables do
//! not need: [`ParamType::IntegerArray`] for `range`, which is `items: {type: "integer"}` and not
//! `trace`'s array-of-strings.
//!
//! The declarations are local rather than imported because tool isolation is a wall, not a
//! preference: `sangkar-boundary`'s `every_source_rule_holds_for_the_real_tree` fails the build when
//! one tool module names another ("shared machinery lives in sangkar-core, sangkar-store and
//! sangkar-domain"). The reconciliation TASK-034 owes is therefore a *move* of these four
//! declarations into one crate-root descriptor the seven tables use — the field names and variants
//! below are `trace`'s exactly apart from the integer-array one, so that move rewrites no call site
//! and deletes the copies.

/// The tool name, as `tools/list` and the mailbox a caller addresses spell it.
pub const NAME: &str = "expand";

/// `TOOLS-SPEC.md` §7, verbatim, in the manifest's sentence order.
pub const DESCRIPTION: &str = concat!(
    "Fetch more detail for one item from a previous result. Pass the id, qualified name or path ",
    "exactly as it appeared. Infers what to expand from the item's kind and says which it chose."
);

/// `TOOLS-SPEC.md` §7's default, and the fallback a direct call (a test, a future in-process
/// caller) uses when it sends none. Per-tool defaults live here, never in the handler: the manifest
/// is the one place a tool's sizing is reviewable (`TECH-SPEC.md` §5.3).
pub const DEFAULT_BUDGET_TOKENS: u64 = 2_500;

/// The five detail modes, in the order `TOOLS-SPEC.md` §7 lists them: the enum row's `values`, the
/// set a parser validates against and the values [`What`] spells are one list.
pub const EXPAND_WHATS: [&str; 5] = ["members", "edges", "source", "coverage", "outline"];

/// Which detail one call answers with.
///
/// `outline` is the one mode never inferred: it is a table of contents for a container whose
/// default detail is already `members`, so the flatter shape has to be asked for by name. Every
/// other mode is what the resolved node's kind implies, and the answer always echoes which one ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    Members,
    Edges,
    Source,
    Coverage,
    Outline,
}

impl What {
    /// The five, in [`EXPAND_WHATS`]' order.
    pub const ALL: [Self; 5] = [
        Self::Members,
        Self::Edges,
        Self::Source,
        Self::Coverage,
        Self::Outline,
    ];

    /// The wire spelling, which is also the argument's accepted value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Members => EXPAND_WHATS[0],
            Self::Edges => EXPAND_WHATS[1],
            Self::Source => EXPAND_WHATS[2],
            Self::Coverage => EXPAND_WHATS[3],
            Self::Outline => EXPAND_WHATS[4],
        }
    }

    /// The mode a declared value names; `None` for anything outside the enum.
    ///
    /// The schema refuses an unknown value before it reaches a handler, so this exists for the
    /// parser and for a caller that holds a string it read from a previous answer.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|what| what.as_str() == text.trim())
    }
}

impl std::fmt::Display for What {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "enum"` with `values`, and `type: "array"` with `items`; JSON
/// Schema has no enum type (it is `type: "string"` plus `enum`) and no "array of enum" (it is
/// `items.enum`), so the conversion is the registry's, not this file's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    /// An array whose every item is an integer: `range`'s `[from, to]`.
    IntegerArray,
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
}

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = concat!(
    "Registered repository to answer from — its path or basename. ",
    "Omit it to use the working directory's repository, or the only registered one."
);

/// The six arguments, in the order `tools/list` publishes the properties — and in the TypeScript
/// literal's order, which is the table's own reading order rather than the alphabet.
///
/// `target` is the only required argument (`TOOLS-SPEC.md` §1: at most one required arg per tool,
/// because argument count is a bigger error source than argument type). `repo` has no default: an
/// absent `repo` means "unselected", which the root answers from the working directory, and a
/// default here would make that indistinguishable from a named repository.
pub const PARAMETERS: [Param; 6] = [
    Param {
        name: "target",
        ty: ParamType::Str,
        required: true,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "One opaque handle copied verbatim from a prior payload: an id, a qualified name or a ",
            "path. Detail is addressable here instead of pre-shipped in every other tool's answer."
        ),
    },
    Param {
        name: "what",
        ty: ParamType::Enum(&EXPAND_WHATS),
        required: false,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "Which detail to return — one of `members`, `edges`, `source`, `coverage` or ",
            "`outline` — inferred from the target's kind when omitted and always echoed in the ",
            "answer. `outline` is the one value never inferred: ask for it by name."
        ),
    },
    Param {
        name: "path",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "Repo-relative prefix applied when resolving the target, so the `Widget` under `tests/` ",
            "can be named. Optional: an unprefixed target is the common case."
        ),
    },
    Param {
        name: "range",
        ty: ParamType::IntegerArray,
        required: false,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "The 1-based, inclusive line span [from, to] to return for a source request, read ",
            "inside the resolved node's own extent and clamped to it; the answer says when a ",
            "request was narrowed. Defaults to the node's whole extent."
        ),
    },
    Param {
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64)),
        min: None,
        max: None,
        description: concat!(
            "Token budget for the answer, larger by default than `summary`'s because detail is by ",
            "definition the expensive part."
        ),
    },
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

/// The one call the CLI usage block and the tool documentation show: a file handle plus `outline`
/// answers "what does this file declare" in one row per declaration, which is the cheapest way to
/// pick the next target.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 2] = [
    ("target", ExampleValue::Text("src/indexer/resolve.ts")),
    ("what", ExampleValue::Text("outline")),
];

/// The arguments the handler parses, in the TypeScript's shape (`ExpandArgs`): `target` is the one
/// required field, and every other absent field means "take the declared default" rather than "the
/// caller chose the default".
///
/// `range` is a `Vec` and not a pair because the schema declares an array: the schema cannot say
/// "exactly two", so a call may arrive with zero, one or three items and the handler refuses the
/// shape instead of trusting the parser's width.
///
/// `what` is typed, so "an unknown mode" is a parse refusal and not a handler branch: the declared
/// enum is what a caller is checked against, and a handler that re-validated it would be a second
/// copy of [`EXPAND_WHATS`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpandArgs {
    pub target: String,
    /// The mode the caller asked for; `None` means "infer it from the resolved node's kind".
    pub what: Option<What>,
    /// Repo-relative prefix the resolved node must sit under; absent means the whole graph.
    pub path: Option<String>,
    /// `[from, to]` lines of a `what: "source"` answer, 1-based and inclusive.
    pub range: Option<Vec<i64>>,
    pub budget: Option<u64>,
    /// Declared but never read by this handler: `repo` selects which reader the composition root
    /// builds, and a tool that resolved it itself would have to know the registry.
    pub repo: Option<String>,
}

impl ExpandArgs {
    /// The one required argument; everything else is absent until a caller says otherwise.
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            ..Self::default()
        }
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
