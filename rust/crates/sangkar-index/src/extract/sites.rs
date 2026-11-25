//! The site half of the extractor: every call with the receiver it was written with, the two
//! inheritance edges, and the six value positions that become `references` candidates.
//!
//! Ported from `#emitCall`, `calleeSite`, `#emitInheritance`, `clauseBases`, `clauseKind`,
//! `#emitReferenceSites`, `referenceSite` and `siteValues` of `src/indexer/extract.ts`. [`visit`] is
//! called once per node by [`super::Extractor::visit`], so the whole second half costs no traversal
//! of its own: no subtree is re-walked and no ancestor is searched more than the one clause lookup
//! already needs.
//!
//! No rule here produces an `endpoint` node. A route node needs the route props (method, path) of a
//! specific framework, which the endpoint pass owns; deriving one from a decorator's mere presence
//! would be exactly the language-specific guess the table-driven walk exists to avoid (see
//! [`super::NODE_KINDS`]).
//!
//! Nothing here names a language either: which grammar node is a call or an inheritance clause is
//! the row's `call_types` and `inheritance_types`, and the value positions are the grammar shapes
//! [`super::REFERENCE_SITES`] declares.

use tree_sitter::Node;

use crate::lang::types::LangRow;

use super::{
    CHAIN_TYPES, Declared, Extractor, NAME_TYPES, RawEdge, Role, Scope, UnresolvedSite, callee_of,
    clean, is_decorator_type, line_of, name_chain, split_qualified, text_of, text_or_null,
};

/// Argument/parenthesis containers, shared by call arguments and decorator arguments.
const ARGUMENT_CONTAINERS: [&str; 4] = [
    "argument_list",
    "arguments",
    "value_arguments",
    "value_argument_list",
];

/// The array-literal shapes of the twelve grammars.
const ARRAY_LITERALS: [&str; 7] = [
    "list",
    "tuple",
    "set",
    "array",
    "array_expression",
    "list_expression",
    "set_expression",
];

/// The object/map-literal shapes. A `pair` inside one contributes its `value`, never its key.
const OBJECT_LITERALS: [&str; 4] = [
    "dictionary",
    "object",
    "object_expression",
    "map_expression",
];

/// The statement shapes that hold one returned value.
const RETURN_TYPES: [&str; 2] = ["return_statement", "return_expression"];

/// The assignments whose right-hand side is a value position.
const ASSIGNMENT_TYPES: [&str; 3] = [
    "assignment",
    "augmented_assignment",
    "assignment_expression",
];

/// Node fields that hold the member being called, in the order the grammars spell it. Consulted only
/// when the callee is not a name chain at all (`Path("x").joinpath`, `row!.paint`).
const CALLEE_MEMBER_FIELDS: [&str; 3] = ["attribute", "property", "field"];

/// Node fields that hold the object a callee is reached through, in the order the grammars spell it.
const CALLEE_OBJECT_FIELDS: [&str; 2] = ["object", "value"];

/// How far below its clause a base name may sit: a clause that wraps a field list (Go's struct body)
/// must contribute nothing rather than every field's type.
const CLAUSE_BASE_DEPTH: u8 = 2;

/// Handles one node for this half, in the order `extract.ts` did: the call or the inheritance clause
/// the declaration half left unhandled, then the node's own value positions.
///
/// `declared` is what the declaration half already consumed — a `Declaration` node declared a symbol,
/// `Import { call: false }` was a plain import statement, and `None` leaves the call and inheritance
/// branches here. A call-shaped import (`require "x"`) arrives as `Import { call: true }` so its call
/// is still emitted while a plain import statement contributes none.
///
/// `scope` is the node's *outer* scope, never the scope it opens: a site belongs to the declaration it
/// sits in, never to itself.
pub(super) fn visit<'tree>(
    extract: &mut Extractor<'_>,
    node: Node<'tree>,
    scope: &Scope<'tree>,
    declared: Declared,
) {
    match declared {
        Declared::Declaration | Declared::Import { call: false } => {}
        Declared::Import { call: true } => emit_call(extract, node, scope),
        Declared::None => {
            let kind = node.kind();
            let spec = extract.spec;
            // A node type in both lists is a call first: `extract.ts`'s `else if` chain reached
            // `emitCall` before `emitInheritance` for every node that is not a declaration.
            if spec.call_types.contains(&kind) {
                emit_call(extract, node, scope);
            } else if spec.inheritance_types.contains(&kind) {
                emit_inheritance(extract, node, scope);
            }
        }
    }
    // A value position can sit anywhere, including inside a declaration's own header.
    emit_reference_sites(extract, node, scope);
}

/// One `calls` edge for the callee, carrying the receiver it was written with. A call with no
/// resolvable callee (`table[key]()`, `(f())()`) is not dropped: it becomes a `dynamic` unresolved
/// site, because the unresolved census is a payload contract and absence would read as "no such
/// call".
fn emit_call(extract: &mut Extractor<'_>, node: Node<'_>, scope: &Scope<'_>) {
    let line = line_of(node);
    let callee = callee_of(node);
    let Some((receiver, name)) = callee.and_then(|callee| callee_site(callee, extract.source))
    else {
        let written = callee.map_or_else(
            || clean(&text_of(extract.source, node)),
            |callee| clean(&text_of(extract.source, callee)),
        );
        extract.push_unresolved(UnresolvedSite {
            line,
            raw_target: written,
            evidence_kind: "dynamic".to_owned(),
            evidence: "call".to_owned(),
        });
        return;
    };
    // The evidence keeps the callee as it was written (`call:self.paint`), so a trace shows the site
    // rather than the name the bind pass went looking for.
    let evidence = callee.map_or_else(
        || name.clone(),
        |callee| clean(&text_of(extract.source, callee)),
    );
    let mut edge = RawEdge::candidate(
        "calls",
        scope.parent_qn.clone(),
        name,
        line,
        &format!("call:{evidence}"),
    );
    edge.receiver = receiver;
    extract.push_edge(edge);
}

/// The name and receiver of a callee. A member access whose object is not itself a name
/// (`PurePosixPath(head).is_absolute`, `sqliteReader!.getNode`) still names the member being called,
/// so its trailing name and its object are recorded instead of the whole site going to `unresolved`:
/// only a callee with no nameable tail (a subscript, a computed call) is nameless.
fn callee_site(callee: Node<'_>, source: &[u8]) -> Option<(Option<String>, String)> {
    if let Some(whole) = name_chain(callee, source) {
        return Some(split_qualified(&whole));
    }
    let member = CALLEE_MEMBER_FIELDS
        .iter()
        .find_map(|field| callee.child_by_field_name(field))?;
    let name = name_chain(member, source)?;
    let receiver = CALLEE_OBJECT_FIELDS
        .iter()
        .find_map(|field| callee.child_by_field_name(field))
        .and_then(|object| text_or_null(clean(&text_of(source, object))));
    Some((receiver, split_qualified(&name).1))
}

/// One edge per base of an inheritance clause: `implements` when the clause's own grammar shape says
/// so, `inherits` otherwise.
///
/// A clause only belongs to the class it is a *direct* child of. Python's `argument_list` is a base
/// list and also a decorator's and a call's argument list, so proximity alone would make every
/// decorated method inherit from its arguments.
fn emit_inheritance(extract: &mut Extractor<'_>, clause: Node<'_>, scope: &Scope<'_>) {
    if scope.parent_role != Role::Class
        || clause
            .parent()
            .is_none_or(|parent| parent.id() != scope.parent.id())
    {
        return;
    }
    for base in clause_bases(clause) {
        let Some(written) = name_chain(base, extract.source) else {
            continue;
        };
        let (receiver, name) = split_qualified(&written);
        let kind = clause_kind(base, clause);
        let mut edge = RawEdge::candidate(kind, scope.parent_qn.clone(), name, line_of(base), kind);
        edge.receiver = receiver;
        extract.push_edge(edge);
    }
}

/// The bases of an inheritance clause: the name nodes at most [`CLAUSE_BASE_DEPTH`] levels down, so a
/// clause that wraps a field list (Go's struct body) contributes nothing rather than every field's
/// type. The dedupe by node id is the TypeScript's: a base a grammar lists twice is one base.
fn clause_bases(clause: Node<'_>) -> Vec<Node<'_>> {
    let mut bases = Vec::new();
    collect_clause_bases(clause, 0, &mut bases);
    bases
}

fn collect_clause_bases<'tree>(node: Node<'tree>, depth: u8, out: &mut Vec<Node<'tree>>) {
    if depth > CLAUSE_BASE_DEPTH {
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
    for child in children {
        let kind = child.kind();
        if NAME_TYPES.contains(&kind) || CHAIN_TYPES.contains(&kind) {
            if !out.iter().any(|seen| seen.id() == child.id()) {
                out.push(child);
            }
            continue;
        }
        collect_clause_bases(child, depth + 1, out);
    }
}

/// Whether a base is an interface: the *clause's* own grammar shape decides (`implements_clause`,
/// `super_interfaces`, …), never the language name. A language with a single mixed clause (C#)
/// therefore reads as `inherits`, which the bind pass can still correct from the target's subkind.
fn clause_kind(base: Node<'_>, clause: Node<'_>) -> &'static str {
    let mut current = base;
    loop {
        let kind = current.kind();
        if kind.contains("implements") || kind.contains("interface") {
            return "implements";
        }
        if current.id() == clause.id() {
            return "inherits";
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return "inherits",
        }
    }
}

/// The value positions of one node. A site container contributes its immediate values only, so a
/// literal nested in another literal is counted by its own site and never twice: `f([a, b])` yields
/// no reference for the list and one per element.
fn emit_reference_sites(extract: &mut Extractor<'_>, node: Node<'_>, scope: &Scope<'_>) {
    let Some(site) = reference_site(node, extract.spec) else {
        return;
    };
    for value in site_values(node, site) {
        let Some(written) = name_chain(value, extract.source) else {
            continue;
        };
        let (receiver, name) = split_qualified(&written);
        let mut edge = RawEdge::candidate(
            "references",
            scope.parent_qn.clone(),
            name,
            line_of(value),
            site,
        );
        edge.receiver = receiver;
        extract.push_edge(edge);
    }
}

/// The reference site a node is a container for, or `None` when it holds no value position.
fn reference_site(node: Node<'_>, spec: &LangRow) -> Option<&'static str> {
    let kind = node.kind();
    if ARGUMENT_CONTAINERS.contains(&kind) {
        let parent = node.parent()?;
        let parent_kind = parent.kind();
        if is_decorator_type(parent_kind) {
            return Some("decorator_argument");
        }
        // A class's base list is an `argument_list` too (Python); it is an inheritance clause, not a
        // value position, so only a call's argument list counts here.
        if !spec.call_types.contains(&parent_kind) {
            return None;
        }
        let decorated = parent
            .parent()
            .is_some_and(|wrapper| is_decorator_type(wrapper.kind()));
        return Some(if decorated {
            "decorator_argument"
        } else {
            "call_argument"
        });
    }
    if ARRAY_LITERALS.contains(&kind) {
        return Some("array_element");
    }
    if OBJECT_LITERALS.contains(&kind) {
        return Some("object_element");
    }
    if RETURN_TYPES.contains(&kind) {
        return Some("return_expression");
    }
    if ASSIGNMENT_TYPES.contains(&kind) && node.child_by_field_name("right").is_some() {
        return Some("assignment_rhs");
    }
    None
}

/// The values of one site: arguments minus keyword names, pair values minus keys, return operands.
fn site_values<'tree>(node: Node<'tree>, site: &str) -> Vec<Node<'tree>> {
    match site {
        "call_argument" | "decorator_argument" | "object_element" => named_children(node)
            .into_iter()
            .map(|child| child.child_by_field_name("value").unwrap_or(child))
            .collect(),
        "assignment_rhs" => {
            let Some(right) = node.child_by_field_name("right") else {
                return Vec::new();
            };
            // An unpacking assignment (`a, b = b, a`) names each element of its right-hand list.
            if right.kind().ends_with("expression_list") {
                named_children(right)
            } else {
                vec![right]
            }
        }
        _ => named_children(node),
    }
}

fn named_children<'tree>(node: Node<'tree>) -> Vec<Node<'tree>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}
