//! The v1 language table and the two lookups built on it.
//!
//! Ported from `src/indexer/lang/registry.ts`. Row order is the extension map's build order; within a
//! row the extension order is the one a user sees first. `grammarWasm`, which named a file inside
//! `tree-sitter-wasms/out`, becomes [`tree_sitter_language`], which returns the crate's `LANGUAGE`
//! constant — so the grammar the binary loads and the grammar this table is proven against are the
//! same object, not two files that happen to agree.
//!
//! The node-type lists follow the reference parser's tables (code-review-graph's `_CLASS_TYPES`,
//! `_FUNCTION_TYPES`, `_IMPORT_TYPES`, `_CALL_TYPES`) where this grammar still has the node name.
//! `tests/lang.rs` proves every name against the compiled grammar and against a parsed snippet rather
//! than against a node-types listing; a missing name is a test failure, not a comment.

use std::path::Path;

use crate::lang::types::{LangId, LangRow};

/// The v1 language table, in declaration order.
///
/// A `static`, not a `const`: `spec_for` hands out a reference to the one row, so the table must have
/// one address (`tests/lang.rs` asserts the row it returns is the row itself).
pub static LANGS: [LangRow; 12] = [
    LangRow {
        id: LangId::Python,
        extensions: &[".py"],
        class_types: &["class_definition"],
        function_types: &["function_definition"],
        import_types: &["import_statement", "import_from_statement"],
        call_types: &["call"],
        // `class Widget(Base)` keeps the bases in the class_definition's argument_list.
        inheritance_types: &["argument_list"],
        test_file_regex: Some(r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.py$"),
    },
    LangRow {
        id: LangId::TypeScript,
        extensions: &[".ts", ".mts", ".cts"],
        // Interfaces, type aliases and enums are what callers depend on, so they are class nodes too.
        class_types: &[
            "class_declaration",
            "abstract_class_declaration",
            "class",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        function_types: &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "abstract_method_signature",
            "arrow_function",
        ],
        import_types: &["import_statement"],
        call_types: &["call_expression", "new_expression"],
        inheritance_types: &["class_heritage", "extends_type_clause"],
        test_file_regex: Some(
            r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.(?:ts|mts|cts)$",
        ),
    },
    LangRow {
        id: LangId::Tsx,
        extensions: &[".tsx"],
        class_types: &[
            "class_declaration",
            "abstract_class_declaration",
            "class",
            "interface_declaration",
            "type_alias_declaration",
            "enum_declaration",
        ],
        function_types: &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "abstract_method_signature",
            "arrow_function",
        ],
        import_types: &["import_statement"],
        call_types: &["call_expression", "new_expression"],
        inheritance_types: &["class_heritage", "extends_type_clause"],
        test_file_regex: Some(r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.tsx$"),
    },
    LangRow {
        id: LangId::JavaScript,
        extensions: &[".js", ".jsx", ".mjs", ".cjs"],
        class_types: &["class_declaration", "class"],
        function_types: &[
            "function_declaration",
            "generator_function_declaration",
            "method_definition",
            "arrow_function",
        ],
        import_types: &["import_statement"],
        call_types: &["call_expression", "new_expression"],
        inheritance_types: &["class_heritage"],
        test_file_regex: Some(
            r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.(?:js|jsx|mjs|cjs)$",
        ),
    },
    LangRow {
        id: LangId::Go,
        extensions: &[".go"],
        class_types: &["type_declaration"],
        function_types: &["function_declaration", "method_declaration"],
        import_types: &["import_declaration"],
        call_types: &["call_expression"],
        // Go has no superclass: embedding is a field list, so the clause lives in the type's body.
        inheritance_types: &["struct_type", "interface_type"],
        // The one row the TypeScript table declared without the `/i` flag, so there is no `(?i)`
        // here: a `.GO` test file with a `_test` suffix is not a match, exactly as before.
        test_file_regex: Some(r"(^|/)[^/]*_test\.go$"),
    },
    LangRow {
        id: LangId::Java,
        extensions: &[".java"],
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
        ],
        function_types: &["method_declaration", "constructor_declaration"],
        import_types: &["import_declaration"],
        call_types: &[
            "method_invocation",
            "object_creation_expression",
            "method_reference",
        ],
        inheritance_types: &["superclass", "super_interfaces"],
        test_file_regex: Some(r"(?i)(^|/)(?:src/test/[^/]*|[^/]*(?:Test|Tests|IT))\.java$"),
    },
    LangRow {
        id: LangId::Rust,
        extensions: &[".rs"],
        // `impl_item` is a scope for methods, not a second type definition, so it is not listed here.
        class_types: &["struct_item", "enum_item", "trait_item"],
        function_types: &["function_item", "function_signature_item"],
        import_types: &["use_declaration"],
        call_types: &["call_expression", "macro_invocation"],
        // Rust's only superclass-like clause is a trait's supertraits (`trait A: B`). `impl B for A`
        // is an implementation clause on a top-level item and belongs to the parser's own dispatch.
        inheritance_types: &["trait_bounds"],
        test_file_regex: Some(r"(?i)(^|/)(?:tests?/[^/]*|[^/]*_test)\.rs$"),
    },
    LangRow {
        id: LangId::C,
        extensions: &[".c", ".h"],
        class_types: &["struct_specifier", "type_definition"],
        function_types: &["function_definition"],
        import_types: &["preproc_include"],
        call_types: &["call_expression"],
        // C has no inheritance clause in the grammar; the empty list is the fact, not an oversight.
        inheritance_types: &[],
        test_file_regex: Some(r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.(?:c|h)$"),
    },
    LangRow {
        id: LangId::Cpp,
        extensions: &[".cpp", ".cc", ".cxx", ".hpp", ".hh"],
        class_types: &["class_specifier", "struct_specifier"],
        // Declarations are listed because a C++ out-of-line or header-only definition is one until
        // the parser sees its body; the reference parser counts them the same way.
        function_types: &["function_definition", "declaration", "field_declaration"],
        import_types: &["preproc_include"],
        call_types: &["call_expression"],
        inheritance_types: &["base_class_clause"],
        test_file_regex: Some(
            r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.(?:cc|cpp|cxx|hpp|hh)$",
        ),
    },
    LangRow {
        id: LangId::CSharp,
        extensions: &[".cs"],
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "enum_declaration",
            "struct_declaration",
            "record_declaration",
        ],
        function_types: &["method_declaration", "constructor_declaration"],
        import_types: &["using_directive"],
        call_types: &["invocation_expression", "object_creation_expression"],
        // `class W : Base, IShape` keeps both clauses in one base_list.
        inheritance_types: &["base_list"],
        test_file_regex: Some(r"(?i)(^|/)(?:[^/]*\.Tests/[^/]*|[^/]*(?:Test|Tests))\.cs$"),
    },
    LangRow {
        id: LangId::Php,
        extensions: &[".php"],
        class_types: &[
            "class_declaration",
            "interface_declaration",
            "trait_declaration",
            "enum_declaration",
        ],
        function_types: &["function_definition", "method_declaration"],
        import_types: &[
            "namespace_use_declaration",
            "include_expression",
            "include_once_expression",
            "require_expression",
            "require_once_expression",
        ],
        call_types: &[
            "function_call_expression",
            "member_call_expression",
            "scoped_call_expression",
            "nullsafe_member_call_expression",
            "object_creation_expression",
        ],
        inheritance_types: &["base_clause", "class_interface_clause"],
        test_file_regex: Some(r"(?i)(^|/)(?:tests?/[^/]*|[^/]*Test)\.php$"),
    },
    LangRow {
        id: LangId::Ruby,
        extensions: &[".rb"],
        class_types: &["class", "module"],
        function_types: &["method", "singleton_method"],
        // `require` is an ordinary call node; the parser filters which calls are imports.
        import_types: &["call"],
        call_types: &["call"],
        inheritance_types: &["superclass"],
        test_file_regex: Some(r"(?i)(^|/)(?:specs?/[^/]*|[^/]*(?:_spec|_test))\.rb$"),
    },
];

/// The language of a path, by extension alone.
///
/// `None` means "not one of the 12", which the indexer records as skipped; content sniffing and
/// shebang detection are the parser's business, not this table's. Extensions are case-insensitive
/// because macOS and Windows checkouts still carry `.PY`. Scanning [`LANGS`] in order is the
/// first-row-wins rule the TypeScript table built its extension map with, and with one source of
/// truth rather than a second table: a duplicate extension is a table edit, not a silent shadow.
#[must_use]
pub fn detect_language(path: &str) -> Option<LangId> {
    // `Path::extension` answers `node:path.extname`'s shapes: no dot, or a leading dot with no other
    // dot, is no extension; the last dot segment wins, so `widget.d.ts` is a `.ts` path.
    let extension = Path::new(path).extension()?.to_str()?;
    if extension.is_empty() {
        return None;
    }
    LANGS.iter().find_map(|row| {
        row.extensions
            .iter()
            .any(|declared| {
                declared
                    .strip_prefix('.')
                    .is_some_and(|bare| bare.eq_ignore_ascii_case(extension))
            })
            .then_some(row.id)
    })
}

/// The resolved row for an id already known to be a language, e.g. from [`detect_language`].
#[must_use]
pub fn spec_for(id: LangId) -> &'static LangRow {
    &LANGS[id.index()]
}

/// The compiled grammar for an id: the row's `grammarWasm` file as the crate that ships it.
///
/// `tree-sitter-typescript` carries both of its grammars and `tree-sitter-php` two dialects; the two
/// rows pick the constants the TypeScript table's `tree-sitter-typescript.wasm` and
/// `tree-sitter-php.wasm` were built from.
#[must_use]
pub fn tree_sitter_language(id: LangId) -> tree_sitter::Language {
    match id {
        LangId::Python => tree_sitter_python::LANGUAGE.into(),
        LangId::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        LangId::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        LangId::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        LangId::Go => tree_sitter_go::LANGUAGE.into(),
        LangId::Java => tree_sitter_java::LANGUAGE.into(),
        LangId::Rust => tree_sitter_rust::LANGUAGE.into(),
        LangId::C => tree_sitter_c::LANGUAGE.into(),
        LangId::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        LangId::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        LangId::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        LangId::Ruby => tree_sitter_ruby::LANGUAGE.into(),
    }
}
