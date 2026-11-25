//! The extractor's shared surface: the row types both halves of the walk fill, the extractor's own
//! state, and the one traversal that drives them.
//!
//! Ported from `src/indexer/extract.ts`, whose 946 lines are split here at the half a file
//! contributes to the graph: `declare.rs` emits the `file` node, the `module` node, every class,
//! function and method with its `contains` edge, the `signature`/`is_test`/`props` payload, and the
//! `imports` edges. The second half — calls, inheritance clauses and reference sites — is
//! `sites.rs`, called once from the same walk.
//!
//! This module owns what neither half may define for itself: the types the persist pass reads, the
//! qn bookkeeping that makes `node.qn` unique, and the loop. It names no language: which grammar
//! node is a class, a function or an import is the language row's `class_types`, `function_types`
//! and `import_types`, and the two grammar shapes no list can express are
//! [`EXTRACTION_EXCEPTIONS`].
//!
//! Nothing here is resolved. Every raw edge carries `tier: "extracted_candidate"` and a textual
//! target; the bind pass either resolves it to a node or records an `unresolved` row.

use std::collections::HashSet;

use regex::Regex;
use sangkar_core::{log, paths};
use serde_json::Value;
use tree_sitter::{Node, Tree};

use crate::lang::types::{LangId, LangRow};

mod declare;
pub mod sites;

/// Every node kind the graph declares. `endpoint` is declared and produced by no rule here: a route
/// node needs the route props (method, path) of a specific framework, which the endpoint pass's
/// business is, and emitting one from a decorator's mere presence would be exactly the
/// language-specific guess the table-driven walk exists to avoid.
pub const NODE_KINDS: [&str; 7] = [
    "file", "module", "class", "function", "method", "test", "endpoint",
];

/// The edge kinds a file can produce before anything is bound. `declare.rs` produces `contains` and
/// `imports`; the other four are the second half's.
pub const RAW_EDGE_KINDS: [&str; 6] = [
    "contains",
    "calls",
    "imports",
    "inherits",
    "implements",
    "references",
];

/// The only tier a raw edge has: nothing has been resolved yet, so nothing is "extracted".
pub const EXTRACTED_CANDIDATE: &str = "extracted_candidate";

/// The six value positions that name something and therefore become `references` candidates. Order
/// is documentation, not significance: `object_element` and `array_element` are two sites because
/// the grammar shapes differ. Produced by the second half.
pub const REFERENCE_SITES: [&str; 6] = [
    "call_argument",
    "assignment_rhs",
    "object_element",
    "array_element",
    "decorator_argument",
    "return_expression",
];

/// The grammar shapes a list of node types cannot express, so the walk implements them by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractionRule {
    /// One file is one module: the root emits a `module` node named after the file stem, which is
    /// the file's own namespace and adds no qn segment.
    FileModule,
    /// A call-shaped import: the row lists the node type as both an import and a call, so only a
    /// `require`, `require_relative` or `load` callee is an import.
    CallImport,
}

/// A grammar shape no list of node types can express, implemented under `rule` for `lang`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtractionException {
    pub lang: LangId,
    pub rule: ExtractionRule,
    pub reason: &'static str,
}

/// The complete set of language-specific branches. Capped at three by `tests/extract_declare.rs`; a
/// fourth entry means the walk grew a language condition instead of a table.
pub const EXTRACTION_EXCEPTIONS: [ExtractionException; 2] = [
    ExtractionException {
        lang: LangId::Python,
        rule: ExtractionRule::FileModule,
        reason: "the grammar has no module declaration, so one file is one module: the root emits a \
                 `module` node named after the file stem, which is the file's own namespace and \
                 adds no qn segment",
    },
    ExtractionException {
        lang: LangId::Ruby,
        rule: ExtractionRule::CallImport,
        reason: "the row lists `call` as both an import and a call, because `require` is an \
                 ordinary call: only a `require`, `require_relative` or `load` callee is an import",
    },
];

/// Whether `lang` is one of the languages a rule is implemented for.
#[must_use]
pub fn has_rule(lang: LangId, rule: ExtractionRule) -> bool {
    EXTRACTION_EXCEPTIONS
        .iter()
        .any(|entry| entry.lang == lang && entry.rule == rule)
}

/// `node.props`, JSON text in the order the props were collected: `serde_json`'s `preserve_order`
/// feature makes the stored JSON match the TypeScript object literal's key order rather than a
/// sorted one, which is what a parity test on the column can assert.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeProps(serde_json::Map<String, Value>);

impl NodeProps {
    /// No props: the shape every declaration without decorators has.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether nothing was collected, which the persist pass stores as the column's `{}` default.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Sets `key` to a list of strings, the shape a decorator list has.
    pub fn set_strings(&mut self, key: &str, values: &[String]) {
        let items = values
            .iter()
            .cloned()
            .map(Value::String)
            .collect::<Vec<_>>();
        self.0.insert(key.to_owned(), Value::Array(items));
    }

    /// The JSON text `node.props` holds, `{}` for a propless node.
    #[must_use]
    pub fn to_json(&self) -> String {
        // A map of `String` keys to `Value` has no unrepresentable state, so the fallback is
        // unreachable rather than a silent loss; it keeps the method total without an unwrap.
        serde_json::to_string(&self.0).unwrap_or_else(|_| "{}".to_owned())
    }
}

/// One declared node, ready for the persist pass. `qn` is the key everything else refers to.
#[derive(Clone, Debug, PartialEq)]
pub struct ExtractedNode {
    pub kind: String,
    pub name: String,
    /// `relPath::Scope.name`, from the shared formatter so the key form has one definition.
    pub qn: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub is_test: bool,
    pub props: NodeProps,
}

/// One site, unresolved by construction. `src` is the qn of the declaration the site sits in (the
/// file node for top-level sites), `target` is the last name segment the bind pass looks up and
/// `receiver` is the qualifier as written (`self`, `os.path`), so a receiver-sensitive binding does
/// not have to re-parse `evidence`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEdge {
    pub kind: String,
    pub src: String,
    pub target: String,
    pub receiver: Option<String>,
    pub line: u32,
    pub tier: String,
    /// Why the site exists: `contains`, `import:<as written>`, `call:<as written>`, the edge kind,
    /// or the reference site.
    pub evidence: Option<String>,
    /// The name an import binds locally; `""` for every other kind.
    pub local_name: String,
}

impl RawEdge {
    /// A raw edge at the only tier this pass produces, carrying the evidence that made it. The tier
    /// is set here rather than by the caller, so no visitor can spell it another way.
    #[must_use]
    pub fn candidate(kind: &str, src: String, target: String, line: u32, evidence: &str) -> Self {
        Self {
            kind: kind.to_owned(),
            src,
            target,
            receiver: None,
            line,
            tier: EXTRACTED_CANDIDATE.to_owned(),
            evidence: Some(evidence.to_owned()),
            local_name: String::new(),
        }
    }
}

/// A site with no name to bind, recorded so absence is never mistaken for "no such call". Only the
/// `dynamic` evidence kind exists: the source offered no name to resolve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnresolvedSite {
    pub line: u32,
    pub raw_target: String,
    pub evidence_kind: String,
    pub evidence: String,
}

/// Everything one file contributes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ExtractResult {
    pub nodes: Vec<ExtractedNode>,
    pub raw_edges: Vec<RawEdge>,
    pub unresolved: Vec<UnresolvedSite>,
}

/// The structural roles a scope can have, which decide `method` versus `function` for a nested
/// declaration and whether a class's own module may prefix a qn. Reportable kinds such as `test`
/// never reach the scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    File,
    Module,
    Class,
    Function,
}

/// Where the walk currently is, so a declaration knows its qn chain and its containment parent.
#[derive(Clone, Debug)]
pub struct Scope<'tree> {
    /// qn segments of the enclosing declarations, outermost first.
    pub names: Vec<String>,
    /// qn of the enclosing declaration: the file node at the top, then the innermost declaration.
    pub parent_qn: String,
    /// Structural role of the enclosing declaration.
    pub parent_role: Role,
    /// The enclosing declaration node itself, for clauses that must be a *direct* child of it.
    pub parent: Node<'tree>,
    /// Decorators of the enclosing `decorated_definition`, handed to the declaration it wraps.
    pub decorators: Vec<String>,
}

/// Which branch of the walk consumed a node, so the second half knows whether it still applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Declared {
    /// A class, function or module declaration: the node declared a symbol, and no call or
    /// inheritance clause can be the same node.
    Declaration,
    /// An import; `call` is `true` when the same node is also a call (`require "x"`), which the
    /// second half still has to emit.
    Import { call: bool },
    /// Not a declaration and not an import: calls and inheritance clauses still apply.
    None,
}

/// What `declare::visit` did with one node.
#[derive(Clone, Debug)]
pub struct Visited<'tree> {
    /// The scope the node's children are walked in.
    pub inner: Scope<'tree>,
    /// The branch that consumed the node, for the second half.
    pub declared: Declared,
}

/// One declaration as a half hands it to `Extractor::emit`: an [`ExtractedNode`] without the
/// `qn`, which only `Extractor::emit` may build.
#[derive(Clone, Debug)]
pub struct Draft {
    pub kind: String,
    pub name: String,
    pub scope: String,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: Option<String>,
    pub is_test: bool,
    pub props: NodeProps,
}

impl Draft {
    /// A declaration with no signature and no props; whichever half emits it fills in both.
    #[must_use]
    pub fn new(kind: &str, name: String, scope: String, start_line: u32, end_line: u32) -> Self {
        Self {
            kind: kind.to_owned(),
            name,
            scope,
            start_line,
            end_line,
            signature: None,
            is_test: false,
            props: NodeProps::new(),
        }
    }
}

/// Extracts one file. `tree` comes from [`crate::parse`], `source` is the same bytes it was parsed
/// from, `spec` is the resolved language row and `rel_path` is the repo-relative path every qn is
/// built from. The file node is always `nodes[0]`.
#[must_use]
pub fn extract_file(
    tree: &Tree,
    source: &[u8],
    spec: &'static LangRow,
    rel_path: &str,
) -> ExtractResult {
    Extractor::new(spec, source, rel_path).run(tree)
}

/// The state one file's walk accumulates: the row, the bytes text slices come from, the qns already
/// claimed, and the result the halves push into.
struct Extractor<'src> {
    spec: &'static LangRow,
    /// Repo-relative, already normalised, so every qn is built from one form.
    rel_path: String,
    /// The bytes the tree was parsed from: every text slice is taken from here.
    source: &'src [u8],
    /// The row's `test_file_regex` compiled once and matched once against `rel_path`. Every node's
    /// `is_test` starts from this answer, so the pattern is neither recompiled nor re-run per
    /// declaration.
    is_test_file: bool,
    /// qns already emitted for this file. The graph keys symbols by qn (`UNIQUE (qn)`), so a second
    /// declaration of the same name — an overload, a redefinition — is the same symbol: the first
    /// declaration keeps the qn and the later one adds no node, because a line suffix would invent a
    /// symbol no caller can name.
    taken: HashSet<String>,
    result: ExtractResult,
}

impl<'src> Extractor<'src> {
    fn new(spec: &'static LangRow, source: &'src [u8], rel_path: &str) -> Self {
        let rel_path = paths::normalize_rel(rel_path);
        // The row holds the pattern source, which already carries its own `(?i)` fold and anchors,
        // so `Regex::new` is the whole translation of the TypeScript row's `RegExp`.
        let is_test_file = match spec.test_file_regex.map(Regex::new) {
            None => false,
            Some(Ok(pattern)) => pattern.is_match(&rel_path),
            Some(Err(error)) => {
                log::warn(
                    "test_file_regex did not compile; no path is a test file for this row",
                    &[
                        ("language", Value::String(spec.id.as_str().to_owned())),
                        ("error", Value::String(error.to_string())),
                    ],
                );
                false
            }
        };
        Self {
            spec,
            rel_path,
            source,
            is_test_file,
            taken: HashSet::new(),
            result: ExtractResult::default(),
        }
    }

    fn run(mut self, tree: &Tree) -> ExtractResult {
        let root = tree.root_node();
        let file_name = base_name(&self.rel_path);
        let file_qn = paths::qn(&self.rel_path, "", &file_name);
        self.taken.insert(file_qn.clone());
        self.result.nodes.push(ExtractedNode {
            kind: "file".to_owned(),
            name: file_name.clone(),
            qn: file_qn.clone(),
            start_line: 1,
            end_line: last_line(root),
            signature: None,
            is_test: self.is_test_file,
            props: NodeProps::new(),
        });

        let mut scope = Scope {
            names: Vec::new(),
            parent_qn: file_qn.clone(),
            parent_role: Role::File,
            parent: root,
            decorators: Vec::new(),
        };

        if has_rule(self.spec.id, ExtractionRule::FileModule) {
            let mut draft = Draft::new(
                "module",
                stem_of(&file_name),
                String::new(),
                1,
                last_line(root),
            );
            draft.is_test = self.is_test_file;
            if let Some(module_qn) = self.emit(draft) {
                self.push_edge(RawEdge::candidate(
                    "contains",
                    file_qn,
                    module_qn.clone(),
                    1,
                    "contains",
                ));
                scope = Scope {
                    names: Vec::new(),
                    parent_qn: module_qn,
                    parent_role: Role::Module,
                    parent: root,
                    decorators: Vec::new(),
                };
            }
        }

        self.walk(root, scope);
        self.result
    }

    /// The walk itself. An explicit stack rather than recursion because a grammar nests by
    /// construction: a generated file with ten thousand chained additions is a ten-thousand-deep
    /// tree, and a walk that ran out of call stack would lose the whole file to one expression.
    /// Children are pushed in reverse so nodes and edges still come out in source order.
    fn walk<'tree>(&mut self, root: Node<'tree>, root_scope: Scope<'tree>) {
        let mut stack: Vec<(Node<'tree>, Scope<'tree>)> = vec![(root, root_scope)];
        while let Some((node, scope)) = stack.pop() {
            let inner = self.visit(node, &scope);
            let mut cursor = node.walk();
            let children: Vec<Node<'tree>> = node.named_children(&mut cursor).collect();
            for child in children.into_iter().rev() {
                stack.push((child, inner.clone()));
            }
        }
    }

    /// Handles one node and returns the scope its children are walked in.
    fn visit<'tree>(&mut self, node: Node<'tree>, scope: &Scope<'tree>) -> Scope<'tree> {
        let decorators = decorator_texts(node, self.source);
        let scoped = if decorators.is_empty() {
            scope.clone()
        } else {
            Scope {
                decorators,
                ..scope.clone()
            }
        };

        let visited = declare::visit(self, node, &scoped);
        sites::visit(self, node, &scoped, visited.declared);
        visited.inner
    }

    /// Adds a node, or returns `None` when its qn is already claimed by an earlier declaration.
    fn emit(&mut self, draft: Draft) -> Option<String> {
        let qn = paths::qn(&self.rel_path, &draft.scope, &draft.name);
        if !self.taken.insert(qn.clone()) {
            return None;
        }
        self.result.nodes.push(ExtractedNode {
            kind: draft.kind,
            name: draft.name,
            qn: qn.clone(),
            start_line: draft.start_line,
            end_line: draft.end_line,
            signature: draft.signature,
            is_test: draft.is_test,
            props: draft.props,
        });
        Some(qn)
    }

    fn push_edge(&mut self, edge: RawEdge) {
        self.result.raw_edges.push(edge);
    }

    fn push_unresolved(&mut self, site: UnresolvedSite) {
        self.result.unresolved.push(site);
    }
}

/// The decorator texts of a node that wraps one or more declarations (`@route(...)` without `@`), so
/// the wrapped declaration carries them as props instead of them being part of its signature.
fn decorator_texts(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    children
        .into_iter()
        .filter(|child| is_decorator_type(child.kind()))
        .map(|child| {
            let text = clean(&text_of(source, child));
            text.strip_prefix('@').unwrap_or(&text).to_owned()
        })
        .collect()
}

fn is_decorator_type(kind: &str) -> bool {
    kind.contains("decorator") || kind == "annotation" || kind == "marker_annotation"
}

/// Node types that carry a name as their whole text.
const NAME_TYPES: [&str; 11] = [
    "identifier",
    "type_identifier",
    "field_identifier",
    "property_identifier",
    "shorthand_property_identifier",
    "constant",
    "name",
    "simple_identifier",
    "this",
    "super",
    "variable_name",
];

/// Node types that are a name chain (`os.path.join`, `A::B`), whose text is the chain.
const CHAIN_TYPES: [&str; 10] = [
    "dotted_name",
    "attribute",
    "member_expression",
    "field_expression",
    "selector_expression",
    "scoped_identifier",
    "scoped_type_identifier",
    "qualified_name",
    "namespace_name",
    "member_access_expression",
];

/// Node fields that hold a callee, in the order the twelve grammars spell it.
const CALLEE_FIELDS: [&str; 5] = ["function", "method", "name", "constructor", "declarator"];

/// Whitespace is collapsed and a value is capped at this many characters before it is stored.
const TEXT_CAP: usize = 120;

/// The source text of a node, decoded lossily: a slice that is not UTF-8 — a file the parser
/// accepted byte-wise — yields replacement characters rather than a panic, which is how the
/// TypeScript's `node.text` decoded it.
fn text_of<'src>(source: &'src [u8], node: Node<'_>) -> std::borrow::Cow<'src, str> {
    String::from_utf8_lossy(source.get(node.byte_range()).unwrap_or_default())
}

/// Whitespace collapsed, trimmed and capped, so a signature is one line and a value stays small.
///
/// The `\s+` collapse is `str::split_whitespace`'s: JavaScript's `\s` also covers U+FEFF, which no
/// grammar's declaration header contains.
fn clean(text: &str) -> String {
    clean_capped(text, TEXT_CAP)
}

fn clean_capped(text: &str, cap: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // `String.prototype.length` and `slice` count UTF-16 code units, so the cap is measured the same
    // way; anything else would keep a different amount of text on a signature holding an astral
    // character. A cut that lands inside a surrogate pair drops the pair instead of storing a lone
    // half, which is the one input this port cannot reproduce: a Rust string has no lone surrogates.
    let units: Vec<u16> = collapsed.encode_utf16().collect();
    if units.len() <= cap {
        return collapsed;
    }
    let mut end = cap;
    if end > 0
        && is_high_surrogate(units[end - 1])
        && units.get(end).is_some_and(|unit| is_low_surrogate(*unit))
    {
        end -= 1;
    }
    String::from_utf16_lossy(&units[..end])
}

const fn is_high_surrogate(unit: u16) -> bool {
    unit >= 0xd800 && unit <= 0xdbff
}

const fn is_low_surrogate(unit: u16) -> bool {
    unit >= 0xdc00 && unit <= 0xdfff
}

/// A quoted specifier without its quotes and an optional at-most-two-letter prefix (`r"..."`,
/// `f'...'`), which is how a string literal's text becomes the module it names.
fn unquote(text: &str) -> String {
    let cleaned = clean(text);
    let mut body = cleaned.as_str();
    let mut prefix = 0;
    while prefix < 2
        && body
            .as_bytes()
            .get(prefix)
            .is_some_and(u8::is_ascii_alphabetic)
    {
        prefix += 1;
    }
    let quoted = |byte: &u8| matches!(*byte, b'"' | b'\'' | b'`');
    if body.as_bytes().get(prefix).is_some_and(quoted) {
        body = &body[prefix + 1..];
    }
    if body.as_bytes().last().is_some_and(quoted) {
        body = &body[..body.len() - 1];
    }
    body.to_owned()
}

fn text_or_null(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// The text of a name or of a name chain, or `None` when the expression is not a name at all
/// (a literal, a call, a subscript). The chain keeps its own separators; every part must itself be
/// a name, so `f().x` is not a name chain.
///
/// Iterative rather than recursive: the predicate only has to hold for every leaf of the chain, and
/// a generated file is free to be ten thousand `.` deep.
fn name_chain(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut pending = vec![node];
    while let Some(current) = pending.pop() {
        let kind = current.kind();
        if NAME_TYPES.contains(&kind) {
            continue;
        }
        if !CHAIN_TYPES.contains(&kind) {
            return None;
        }
        let mut cursor = current.walk();
        let children: Vec<Node<'_>> = current.named_children(&mut cursor).collect();
        if children.is_empty() {
            return None;
        }
        pending.extend(children);
    }
    text_or_null(clean(&text_of(source, node)))
}

/// `self.paint` → `(Some("self"), "paint")`; `f` → `(None, "f")`. The separators are the twelve
/// grammars' four (`::`, `->`, `.`, `/`); scanning bytes for them is safe because each is ASCII, so
/// every split point is a character boundary.
fn split_qualified(text: &str) -> (Option<String>, String) {
    let bytes = text.as_bytes();
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let width = match bytes[index] {
            b'.' | b'/' => 1,
            b':' if bytes.get(index + 1) == Some(&b':') => 2,
            b'-' if bytes.get(index + 1) == Some(&b'>') => 2,
            _ => {
                index += 1;
                continue;
            }
        };
        parts.push(&text[start..index]);
        index += width;
        start = index;
    }
    parts.push(&text[start..]);
    parts.retain(|part| !part.is_empty());
    match parts.split_last() {
        Some((name, [])) => (None, (*name).to_owned()),
        Some((name, rest)) => (Some(rest.join(".")), (*name).to_owned()),
        None => (None, text.to_owned()),
    }
}

/// The command being called, in the field order the grammars use, else the first named child.
fn callee_of(node: Node<'_>) -> Option<Node<'_>> {
    for field in CALLEE_FIELDS {
        if let Some(child) = node.child_by_field_name(field) {
            return Some(child);
        }
    }
    node.named_child(0)
}

/// The 1-based line a node starts on.
fn line_of(node: Node<'_>) -> u32 {
    node.start_position().row as u32 + 1
}

/// The last line a node covers; a node ending at column 0 does not own the next row.
fn last_line(node: Node<'_>) -> u32 {
    let end = node.end_position();
    if end.column == 0 && end.row > 0 {
        end.row as u32
    } else {
        end.row as u32 + 1
    }
}

fn base_name(rel_path: &str) -> String {
    rel_path.rsplit('/').next().unwrap_or(rel_path).to_owned()
}

fn stem_of(file_name: &str) -> String {
    match file_name.rfind('.') {
        Some(dot) if dot > 0 => file_name[..dot].to_owned(),
        _ => file_name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::clean_capped;

    /// The cap is `String.prototype.length`, not a character count: `slice(0, 3)` of `"a😀😀"` keeps
    /// the `a` and one whole astral character, and a cut that would split the second pair drops it
    /// instead of storing a lone surrogate as U+FFFD.
    #[test]
    fn the_cap_is_counted_in_utf16_code_units() {
        assert_eq!(clean_capped("a😀😀", 3), "a😀");
        assert_eq!(clean_capped("a😀😀", 4), "a😀");
        assert_eq!(clean_capped("a😀😀", 5), "a😀😀");
        assert_eq!(clean_capped("  a \n b  ", 3), "a b");
    }
}
