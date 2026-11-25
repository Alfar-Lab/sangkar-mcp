//! `status` as the protocol layer sees it (`TOOLS-SPEC.md` §2), ported from
//! `src/tools/status/manifest.ts`.
//!
//! The description is model-facing text, so it is reproduced verbatim: the wording is what tells an
//! agent to call this *before* claiming that something does not exist, and a paraphrase would
//! quietly drop that instruction. The em dash and the trailing "call it freely" are part of it for
//! the same reason.
//!
//! Both constants are here because a tool owns its own defaults (`TECH-SPEC.md` §5.2): 800 tokens is
//! the budget the spec measured, and the retry interval is short because `status` is the cheap,
//! idempotent call an agent polls while the index is still being built — a long interval would make
//! the poll cost more than the answer.
//!
//! ## Why the parameter table is declared here
//!
//! `sangkar-cli` owns the argument *parser* and its `InputLiteral`/`InputField` types, and this
//! crate may not depend on it (the DAG in `sangkar-boundary` forbids the edge: the CLI is the only
//! crate that names the tool layer, never the reverse). [`PARAMETERS`] therefore declares the same
//! three arguments `src/tools/status/manifest.ts` declares — type, requiredness, default, the item
//! set of an array and the description — in the vocabulary the type conversion in
//! `src/index.ts:227-275` consumes, so the registry can build one `tools/list` schema from all seven
//! tables and the parser can check a call against the same data.
//!
//! [`StatusArgs`] is the Rust mirror of the declared arguments and the one place a default is
//! applied: a registry that re-derived `800` would be a second copy of the contract.

/// The tool name, as `tools/list` and a cursor's `tool` member spell it.
pub const NAME: &str = "status";

/// The model-facing description, byte for byte `manifest.ts`'s `description`.
pub const DESCRIPTION: &str = concat!(
    "Report whether the graph is current and what it covers. Call this before telling the user ",
    "that something does not exist, and whenever a previous answer looked incomplete. Returns ",
    "the index generation, how stale the working tree is, per-path coverage, and the language ",
    "census. Cheap — call it freely."
);

/// Default token budget of a `status` call (`TOOLS-SPEC.md` §2).
pub const STATUS_DEFAULT_TOKENS: u64 = 800;

/// How long an unpublished database asks the agent to wait before asking again.
pub const STATUS_RETRY_AFTER_MS: u64 = 5_000;

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "array"` with `items`, and an enum item as `items.values`;
/// JSON Schema has no enum type (it is `type: "string"` plus `enum`) and no "array of enum" (it is
/// `items.enum`), so the conversion is the registry's, not this file's. An empty `values` slice on
/// [`ParamType::Array`] is the plain string array `paths` declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// An array whose every item is one of the set; empty means "any string".
    Array(&'static [&'static str]),
}

/// A declared default. Three shapes are all the seven manifests carry: a word, a number, and a
/// list (an absent `paths` is the empty list, not the repository root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamDefault {
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
    pub description: &'static str,
}

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = concat!(
    "Registered repository to answer from — its path or basename. ",
    "Omit it to use the working directory's repository, or the only registered one."
);

/// Three optional arguments, in the order `tools/list` publishes the properties — no argument here
/// is required, which is what makes `status` the call an agent can make before it knows anything.
///
/// `paths` being absent means "the whole repository", so its default is the empty list rather than
/// the root path (a root path would have to be normalised against the repository root and would then
/// be indistinguishable from an explicit request for that one path). `repo` has no default: an
/// absent `repo` means "unselected", which the root answers from the working directory; a default
/// here would make that indistinguishable from a named repository.
pub const PARAMETERS: [Param; 3] = [
    Param {
        name: "paths",
        ty: ParamType::Array(&[]),
        required: false,
        default: Some(ParamDefault::List(&[])),
        description: "Paths whose per-path coverage and skip count this answer reports.",
    },
    Param {
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(STATUS_DEFAULT_TOKENS as i64)),
        description: "Token ceiling for the coverage report, so forty paths cannot blow it up.",
    },
    Param {
        name: "repo",
        ty: ParamType::Str,
        required: false,
        default: None,
        description: REPO_DESCRIPTION,
    },
];

/// A call a caller can paste: the one path it is unsure the index covers. A bare `status` asks the
/// repository-wide question, which the retry interval already makes cheap to repeat.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 1] = [("paths", ExampleValue::List(&["src"]))];

/// The two scalar shapes the one declared example uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    List(&'static [&'static str]),
}

/// The arguments the handler parses, in the TypeScript's shape (`StatusArgs`): both fields are
/// optional, and an absent field means "take the declared default" rather than "the caller chose the
/// default".
///
/// Optionality is load-bearing beyond convenience: the caller's argv passes straight through, and
/// `budget: 800` typed by a caller must hash the same as an absent one ([`Self::budget`] is what the
/// handler hashes), so the defaults are applied here and nowhere else.
///
/// `budget` is an `i64` and not a `u64` because it arrives from a JSON integer, which may be
/// negative: an out-of-range budget is the manifest's default rather than a caller error, so the sign
/// has to be representable before it is judged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusArgs {
    pub paths: Option<Vec<String>>,
    pub budget: Option<i64>,
}

impl StatusArgs {
    /// The token ceiling this call asked for: a positive integer, else the declared default.
    #[must_use]
    pub fn budget(&self) -> u64 {
        match self.budget {
            Some(tokens) if tokens > 0 => u64::try_from(tokens).unwrap_or(STATUS_DEFAULT_TOKENS),
            _ => STATUS_DEFAULT_TOKENS,
        }
    }
}
