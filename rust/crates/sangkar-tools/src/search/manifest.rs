//! `search`'s manifest: the description a model sees and the arguments the handler parses
//! (`TOOLS-SPEC.md` §5, `TECH-SPEC.md` §6).
//!
//! Ported from `src/tools/search/manifest.ts`. The description is the spec text verbatim, because it
//! is the entire routing prompt an agent sees for this tool: "declarations with file:line and their
//! in/out degree — not source" is what separates `search` from a file reader, and "hybrid keyword
//! (BM25) with optional semantic rerank" is softened only where the payload can say so —
//! `payload.mode` is `bm25` here.
//!
//! The five-argument surface is §10's decision, not a local choice: at most one required argument (a
//! second required arg is where CRG's 11.2 % argument-error rate came from), and no
//! `provider`/`model`/`regex`/`source_max_lines`. `kind` is optional because a wrong `kind` hides the
//! right answer, which is worse than a noisy list; `path` exists because "find this name under
//! tests/" has no other expression once every tool lost its structured arguments.
//!
//! ## Why the parameter descriptor is declared here
//!
//! [`Param`] is the vocabulary the registry converts into JSON Schema, and it is declared in this
//! module rather than shared. A tool module may not name another tool's module (`sangkar-boundary`'s
//! tool-isolation rule), and `sangkar-cli` may not be named from here (the crate DAG), so the three
//! copies are the only shape that compiles today. The registry task moves the descriptor to one
//! crate-root module and deletes them; until then this table matches `trace`'s field for field and
//! carries only the two type shapes `search` declares.
//!
//! [`SearchArgs`] is the Rust mirror of the declared arguments and the one place a default is
//! applied: a registry that re-derived `2000` would be a second copy of the contract.

/// The tool name as the cursor pins it: a token minted by one tool must not resume another.
pub const NAME: &str = "search";

/// `TOOLS-SPEC.md` §5, verbatim. Four sentences, in this order, because a model routes on them.
pub const DESCRIPTION: &str = concat!(
    "Find symbols or text in the indexed code. Hybrid keyword (BM25) with optional semantic rerank. ",
    "Returns declarations with file:line and their in/out degree — not source. Use when you know ",
    "roughly what it is called but not where it lives."
);

/// The kinds a caller may ask for. `file` is a node kind (a file declares itself) and `decorator` is
/// recorded as a symbol because it is a name someone searches for; `test` is absent even though §5's
/// parameter table mentions "only test", because the index stores that as `node.is_test`, not as a
/// kind — a toggled flag, not a value that can be filtered on.
pub const SEARCH_KINDS: [&str; 5] = ["function", "method", "class", "file", "decorator"];

/// The default token budget, kept here because it is a property of the schema the root advertises;
/// the handler reads it so the advertised default and the fallback can never disagree
/// (`TOOLS-SPEC.md` §9: about twenty hits with their degrees).
pub const DEFAULT_BUDGET_TOKENS: u64 = 2_000;

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// The TypeScript table spells `type: "enum"` with `values`; JSON Schema has no enum type (it is
/// `type: "string"` plus `enum`), so the conversion is the registry's, not this file's. The shape is
/// `trace`'s, which is the crate-wide descriptor this module will hand over unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    Integer,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
}

/// A declared default. `search` declares exactly one, and it is a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamDefault {
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

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = concat!(
    "Registered repository to answer from — its path or basename. ",
    "Omit it to use the working directory's repository, or the only registered one."
);

/// The argument table, in the order `tools/list` publishes the properties.
///
/// `query` appears with `required: true` exactly once — TOOLS-SPEC.md §1's at-most-one-required-arg
/// rule — and it is declared first because it is the one argument the tool cannot do without.
pub const PARAMETERS: [Param; 6] = [
    Param {
        name: "query",
        ty: ParamType::Str,
        required: true,
        default: None,
        min: None,
        max: None,
        description: "Free text to find: an identifier, a substring or a phrase.",
    },
    Param {
        name: "kind",
        ty: ParamType::Enum(&SEARCH_KINDS),
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Return only declarations of this kind, when you already know it.",
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
        name: "budget",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64)),
        min: None,
        max: None,
        description: "Token ceiling for this result; exceeding it truncates and sets `next`.",
    },
    Param {
        name: "cursor",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: "Continue this same search from the `next` cursor of a previous page.",
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

/// One call a caller can paste verbatim: the schema says which arguments exist, not what a real one
/// looks like, and an agent with no example reaches for a file reader instead.
///
/// A `(key, value)` pair list rather than a JSON object so this module needs no JSON dependency; the
/// registry renders it into the schema's `examples` array.
pub const EXAMPLE: [(&str, ExampleValue); 2] = [
    ("query", ExampleValue::Text("publish")),
    ("kind", ExampleValue::Text("function")),
];

/// The one scalar shape the declared example uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    Text(&'static str),
}

/// The arguments the handler parses, in the TypeScript's shape (`SearchArgs`): every field but
/// `query` is optional, and an absent field means "take the declared default" rather than "the
/// caller chose the default".
///
/// `kind` is a plain string and not an enum. The TypeScript types it as a five-value union, but the
/// check that rejects a sixth value is the argument parser's, not the handler's: an unknown kind
/// here simply matches no row and the answer says no declaration matched under the requested kind,
/// which is the honest reply for a value this tool did not receive from a validating parser.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SearchArgs {
    pub query: String,
    pub kind: Option<String>,
    /// Repo-relative prefix, `trace`'s and `status`'s semantics: the path itself and everything under
    /// it, never a sibling that merely shares the characters (`src` never matches `src-old/`).
    pub path: Option<String>,
    /// An `i64` because it arrives from a JSON integer, which may be negative: the sign has to be
    /// representable before it is judged, and [`Self::budget_tokens`] is where it is judged.
    pub budget: Option<i64>,
    pub cursor: Option<String>,
    pub repo: Option<String>,
}

impl SearchArgs {
    /// The one required argument; everything else is absent until a caller says otherwise.
    #[must_use]
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            kind: None,
            path: None,
            budget: None,
            cursor: None,
            repo: None,
        }
    }

    /// The token ceiling this call asked for: the declared default when the caller did not ask.
    ///
    /// A non-positive budget becomes `0`, which no answer can fit, so the call is refused with
    /// `budget_too_large` instead of being quietly upgraded to the default — the same refusal the
    /// TypeScript produces for a negative number, because the number decides what fits either way.
    #[must_use]
    pub fn budget_tokens(&self) -> u64 {
        match self.budget {
            Some(tokens) if tokens > 0 => u64::try_from(tokens).unwrap_or(DEFAULT_BUDGET_TOKENS),
            Some(_) => 0,
            None => DEFAULT_BUDGET_TOKENS,
        }
    }
}
