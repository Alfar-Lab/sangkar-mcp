//! The declaration half of the extractor: the nodes a file's own syntax declares, the containment
//! edges that place them, and the import edges.
//!
//! Ported from the declaration visitors of `src/indexer/extract.ts` — the `file` node, the
//! `fileModule` module node, `#emitDeclaration`, `#kindOf`, `#isImport`, `#emitImports`,
//! `importSites`, `collectImportSites`, `declarationName` and `signatureOf`. The other half (calls,
//! inheritance clauses and reference sites) is TASK-042's `sites.rs`; [`super::Extractor::visit`]
//! has the single call site it plugs into.
//!
//! Nothing here names a language either: the node types come from the row's `class_types`,
//! `function_types` and `import_types`, and the two shapes a list cannot express are the
//! [`super::EXTRACTION_EXCEPTIONS`] entries this module consults through
//! [`super::has_rule`] — `fileModule` decides whether a module node adds a qn segment, `callImport`
//! decides whether a call-shaped node is an import at all.

use std::sync::LazyLock;

use regex::Regex;
use tree_sitter::Node;

use crate::lang::types::LangRow;

use super::{
    Declared, Draft, ExtractionRule, Extractor, NAME_TYPES, RawEdge, Role, Scope, UnresolvedSite,
    Visited, callee_of, clean, clean_capped, has_rule, last_line, line_of, name_chain,
    split_qualified, text_of, text_or_null, unquote,
};

/// A declaration's header is stored up to this many characters: the TypeScript's `SIGNATURE_CAP`,
/// so a signature column written by either implementation holds the same text.
const SIGNATURE_CAP: usize = 200;

/// A wrapper around a declaration whose name has to be found one level down (`type_spec`,
/// `function_declarator`).
static DECLARATOR_TYPES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|_)(?:spec|declarator)$").expect("the declarator pattern is a literal")
});

/// A name that reads as a test: `test_x`, `x_test`, `TestCase`, `handle_spec`.
static TEST_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:^|[._-])(?:test|spec)s?(?:[^a-z]|$)|^Test(?:s|Cases)?(?:[^a-z]|$)")
        .expect("the test-name pattern is a literal")
});

/// `require`-style callees for the `callImport` rule.
const REQUIRE_CALLEES: [&str; 3] = ["require", "require_relative", "load"];

/// Grammar types that declare a namespace rather than a class; Ruby and TS/PHP have one.
const MODULE_TYPES: [&str; 4] = [
    "module",
    "module_declaration",
    "namespace_definition",
    "internal_module",
];

/// One name an import statement binds: the specifier it comes from and the name it binds locally.
struct ImportSite {
    target: String,
    local_name: String,
}

/// Handles one node for this half and reports what it did, so the second half knows whether the
/// node is still its business.
pub(super) fn visit<'tree>(
    extract: &mut Extractor<'_>,
    node: Node<'tree>,
    scope: &Scope<'tree>,
) -> Visited<'tree> {
    let spec = extract.spec;
    let kind = node.kind();
    if spec.class_types.contains(&kind) {
        let role = if is_module_type(kind) {
            Role::Module
        } else {
            Role::Class
        };
        return Visited {
            inner: emit_declaration(extract, node, scope, role),
            declared: Declared::Declaration,
        };
    }
    if spec.function_types.contains(&kind) {
        return Visited {
            inner: emit_declaration(extract, node, scope, Role::Function),
            declared: Declared::Declaration,
        };
    }
    if is_import(extract, node) {
        emit_imports(extract, node, scope);
        // `require "x"` is an import and a call at once; a plain import statement is not a call.
        return Visited {
            inner: scope.clone(),
            declared: Declared::Import {
                call: spec.call_types.contains(&kind),
            },
        };
    }
    Visited {
        inner: scope.clone(),
        declared: Declared::None,
    }
}

/// Emits a declaration and its `contains` edge, and returns the scope its children are walked in. A
/// declaration node with no recoverable name (an anonymous class expression) declares nothing
/// nameable, so it is not a node and its children stay in the enclosing scope.
fn emit_declaration<'tree>(
    extract: &mut Extractor<'_>,
    node: Node<'tree>,
    scope: &Scope<'tree>,
    role: Role,
) -> Scope<'tree> {
    let Some(name) = declaration_name(node, extract.source) else {
        return scope.clone();
    };
    let reads_as_test = TEST_NAME.is_match(&name);
    let is_test = extract.is_test_file || reads_as_test;
    let kind = kind_of(role, scope.parent_role, reads_as_test);
    let line = line_of(node);
    let mut draft = Draft::new(
        kind,
        name.clone(),
        scope.names.join("."),
        line,
        last_line(node),
    );
    draft.signature = signature_of(node, extract.source);
    draft.is_test = is_test;
    if !scope.decorators.is_empty() {
        draft.props.set_strings("decorators", &scope.decorators);
    }
    let Some(qn) = extract.emit(draft) else {
        return scope.clone();
    };
    extract.push_edge(RawEdge::candidate(
        "contains",
        scope.parent_qn.clone(),
        qn.clone(),
        line,
        "contains",
    ));
    // A file's own module is its namespace, not a prefix: `tests/test_x.py::test_x`, not
    // `::test_x.test_x`. Every other declaration is a segment of the qn chain.
    let mut names = scope.names.clone();
    if !(role == Role::Module && has_rule(extract.spec.id, ExtractionRule::FileModule)) {
        names.push(name);
    }
    Scope {
        names,
        parent_qn: qn,
        parent_role: role,
        parent: node,
        decorators: Vec::new(),
    }
}

/// The reportable kind of a declaration, from its own role and the role of the declaration it sits
/// in. A module keeps its own kind — it is a namespace, not a symbol a caller searches for — and any
/// other declaration whose name reads as a test is a `test`, whatever its structural role.
fn kind_of(role: Role, parent_role: Role, reads_as_test: bool) -> &'static str {
    if role == Role::Module {
        "module"
    } else if reads_as_test {
        "test"
    } else if role == Role::Class {
        "class"
    } else if parent_role == Role::Class {
        "method"
    } else {
        "function"
    }
}

fn is_module_type(kind: &str) -> bool {
    MODULE_TYPES.contains(&kind)
}

/// Whether a node is an import. A node type the row lists in both `import_types` and `call_types`
/// is only an import when the `callImport` rule applies to the language and the callee is a
/// `require`-style name: without that, `require` would be a call and the argument would never be
/// recorded as the module it names.
fn is_import(extract: &Extractor<'_>, node: Node<'_>) -> bool {
    let spec = extract.spec;
    let kind = node.kind();
    if !spec.import_types.contains(&kind) {
        return false;
    }
    if !spec.call_types.contains(&kind) || !has_rule(spec.id, ExtractionRule::CallImport) {
        return true;
    }
    let Some(callee) = callee_of(node) else {
        return false;
    };
    let Some(name) = name_chain(callee, extract.source) else {
        return false;
    };
    let (_, local) = split_qualified(&name);
    REQUIRE_CALLEES.contains(&local.as_str())
}

/// One `imports` edge per imported binding. An import statement with nothing nameable in it is
/// recorded as unresolved instead: no later pass can recover a name the source never wrote.
fn emit_imports(extract: &mut Extractor<'_>, node: Node<'_>, scope: &Scope<'_>) {
    let line = line_of(node);
    let sites = import_sites(node, extract.source, extract.spec);
    if sites.is_empty() {
        extract.push_unresolved(UnresolvedSite {
            line,
            raw_target: clean(&text_of(extract.source, node)),
            evidence_kind: "dynamic".to_owned(),
            evidence: "import".to_owned(),
        });
        return;
    }
    for site in sites {
        let mut edge = RawEdge::candidate(
            "imports",
            scope.parent_qn.clone(),
            site.target.clone(),
            line,
            &format!("import:{}", site.target),
        );
        edge.local_name = site.local_name;
        extract.push_edge(edge);
    }
}

/// The import shapes of the twelve grammars in one reader: a from-import keeps its module in the
/// `module_name` field (or, for TS and JS, in the `source` string) and binds each remaining name,
/// while a plain import binds each name to the module it names. A call-shaped import (Ruby's
/// `require`) contributes only its argument, never its callee.
fn import_sites(node: Node<'_>, source: &[u8], spec: &LangRow) -> Vec<ImportSite> {
    let module_field = node
        .child_by_field_name("module_name")
        .or_else(|| node.child_by_field_name("source"));
    let module_text = module_field.map(|field| unquote(&text_of(source, field)));
    let callee = spec
        .call_types
        .contains(&node.kind())
        .then(|| callee_of(node))
        .flatten();
    let mut sites = Vec::new();
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in children {
        if module_field.is_some_and(|field| field.id() == child.id()) {
            continue;
        }
        if callee.is_some_and(|called| called.id() == child.id()) {
            continue;
        }
        collect_import_sites(child, module_text.as_deref(), source, &mut sites);
    }
    if sites.is_empty()
        && let Some(text) = module_text
    {
        sites.push(ImportSite {
            target: text,
            local_name: String::new(),
        });
    }
    sites
}

fn collect_import_sites(
    node: Node<'_>,
    module_text: Option<&str>,
    source: &[u8],
    out: &mut Vec<ImportSite>,
) {
    let name_field = node.child_by_field_name("name");
    let alias_field = node.child_by_field_name("alias");
    if let Some(name_node) = name_field {
        let name =
            name_chain(name_node, source).unwrap_or_else(|| clean(&text_of(source, name_node)));
        if !name.is_empty() {
            let local_name = alias_field.map_or_else(
                || split_qualified(&name).1,
                |alias| clean(&text_of(source, alias)),
            );
            out.push(ImportSite {
                target: module_text.map_or_else(|| name.clone(), str::to_owned),
                local_name,
            });
        }
        return;
    }
    if node.kind().contains("string") {
        let text = unquote(&text_of(source, node));
        if !text.is_empty() {
            out.push(ImportSite {
                target: module_text.map_or_else(|| text.clone(), str::to_owned),
                local_name: String::new(),
            });
        }
        return;
    }
    if let Some(chain) = name_chain(node, source) {
        out.push(ImportSite {
            local_name: split_qualified(&chain).1,
            target: module_text.map_or_else(|| chain.clone(), str::to_owned),
        });
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in children {
        collect_import_sites(child, module_text, source, out);
    }
}

/// The name of a declaration: its `name` field, a wrapped spec/declarator, or a single name child.
fn declaration_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    if let Some(named) = node.child_by_field_name("name") {
        let text = name_chain(named, source).unwrap_or_else(|| clean(&text_of(source, named)));
        return text_or_null(text);
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    for child in &children {
        if DECLARATOR_TYPES.is_match(child.kind())
            && let Some(nested) = declaration_name(*child, source)
        {
            return Some(nested);
        }
    }
    let names = children
        .iter()
        .filter(|child| NAME_TYPES.contains(&child.kind()))
        .collect::<Vec<_>>();
    match names.as_slice() {
        [only] => text_or_null(clean(&text_of(source, **only))),
        _ => None,
    }
}

/// A declaration's header: its text up to the body, so a multi-line signature keeps its own rows.
///
/// The cap is [`SIGNATURE_CAP`] characters after the whitespace collapse, which is the TypeScript's
/// `clean(rows.join(" "), SIGNATURE_CAP)`. Characters, not UTF-16 code units: a cap that could cut
/// a surrogate pair in half is a defect, not a parity requirement.
fn signature_of(node: Node<'_>, source: &[u8]) -> Option<String> {
    let text = text_of(source, node);
    let Some(body) = node.child_by_field_name("body") else {
        return text_or_null(clean(text.split('\n').next().unwrap_or_default()));
    };
    let body_row = body
        .start_position()
        .row
        .saturating_sub(node.start_position().row);
    let mut rows: Vec<&str> = text.split('\n').take(body_row.max(1)).collect();
    if body_row == 0
        && let Some(first) = rows.first_mut()
    {
        // The body starts on the signature's own row (`area() { return 1; }`), so the signature ends
        // where the body begins. A byte column only equals a character index on ASCII, so a row with
        // anything outside printable ASCII keeps its whole text rather than a cut inside a
        // character.
        let row = *first;
        if row
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            let cut = body.start_byte().saturating_sub(node.start_byte());
            if let Some(head) = row.get(..cut) {
                *first = head;
            }
        }
    }
    text_or_null(clean_capped(&rows.join(" "), SIGNATURE_CAP))
}
