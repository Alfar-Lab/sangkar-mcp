//! The language layer as data.
//!
//! Ported from `src/indexer/lang/types.ts`: language support is a table (TECH-SPEC.md §9) — one row
//! per grammar plus the node-type lists that decide which syntax nodes become graph nodes and which
//! become edges. No parser code branches on a language name; adding a language is a row in
//! [`registry`](crate::lang::registry).

/// The 12 languages of the v1 MVP, in table order.
///
/// The order is the registry's row order, and it is load-bearing: the first row that claims an
/// extension owns it. [`LangId::index`] is defined against that same order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LangId {
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Go,
    Java,
    Rust,
    C,
    Cpp,
    CSharp,
    Php,
    Ruby,
}

impl LangId {
    /// Every id, in table order.
    pub const ALL: [Self; 12] = [
        Self::Python,
        Self::TypeScript,
        Self::Tsx,
        Self::JavaScript,
        Self::Go,
        Self::Java,
        Self::Rust,
        Self::C,
        Self::Cpp,
        Self::CSharp,
        Self::Php,
        Self::Ruby,
    ];

    /// The id as the table spells it: what a log line, a stored row or a test failure prints.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::TypeScript => "typescript",
            Self::Tsx => "tsx",
            Self::JavaScript => "javascript",
            Self::Go => "go",
            Self::Java => "java",
            Self::Rust => "rust",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Php => "php",
            Self::Ruby => "ruby",
        }
    }

    /// This id's index into the registry's rows, which is [`LangId::ALL`]'s index too.
    ///
    /// Total by construction, so `specFor` needs no lookup that can fail; `tests/lang.rs` asserts
    /// that every row sits at its own id's index.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::Python => 0,
            Self::TypeScript => 1,
            Self::Tsx => 2,
            Self::JavaScript => 3,
            Self::Go => 4,
            Self::Java => 5,
            Self::Rust => 6,
            Self::C => 7,
            Self::Cpp => 8,
            Self::CSharp => 9,
            Self::Php => 10,
            Self::Ruby => 11,
        }
    }
}

impl std::fmt::Display for LangId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One language row as the parser consumes it: the shape a `.sangkar/languages.toml` (GRAPH-MODEL.md)
/// can also produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LangRow {
    pub id: LangId,
    /// Source extensions, leading dot and lowercase: `.ts`, never `ts` or `.TS`.
    pub extensions: &'static [&'static str],
    /// Nodes that produce a class node: a type declaration, not only a runtime class.
    pub class_types: &'static [&'static str],
    /// Nodes that produce a function node: functions, methods, constructors, arrow functions.
    pub function_types: &'static [&'static str],
    /// Nodes that produce an import edge.
    pub import_types: &'static [&'static str],
    /// Nodes that produce a call edge; the callee name still has to be resolved later.
    pub call_types: &'static [&'static str],
    /// Grammar nodes that carry a superclass or interface clause. This names the node holding the
    /// clause, not the base name's node: TypeScript wraps `extends_clause`/`implements_clause` in
    /// `class_heritage`, and a row with no such construct in the language records an empty list —
    /// C has no inheritance at all.
    pub inheritance_types: &'static [&'static str],
    /// A pattern matching a repo-relative path (forward slashes) that is a test file for this
    /// language. The TypeScript row held a `RegExp`; here it is the source of one, still to be
    /// compiled by the extractor, with the `/i` flag folded in as the inline `(?i)` group so the
    /// two are the same pattern rather than a case-sensitive look-alike.
    pub test_file_regex: Option<&'static str>,
}
