//! Provenance for the site half of the extractor: the `calls` edges with the receiver each one was
//! written with, the two inheritance kinds, the six value positions that become `references`
//! candidates, and the sites that have no name to bind.
//!
//! Ported from `tests/extract.test.ts`: the TypeScript suite's Python fixture is copied here byte for
//! byte, so the values it pins are checked against the same source in both implementations —
//! `self.paint` with receiver `self`, `widget.render` with receiver `widget`, `Base` as the class's
//! only base, the `decorator_argument` that belongs to the class rather than the method, and the six
//! site kinds. One snippet per remaining family follows: Java supplies `implements` and a
//! `new`/object creation, Ruby the call-shaped import that is a call as well, and a Python subscript
//! callee the one site shape with no name at all.
//!
//! The grammars are `LANGUAGE` constants linked into this binary, so nothing here reaches the network
//! or a WASM directory.

use std::collections::BTreeSet;

use sangkar_index::extract::{
    EXTRACTED_CANDIDATE, ExtractResult, NODE_KINDS, RAW_EDGE_KINDS, REFERENCE_SITES, RawEdge,
    UnresolvedSite, extract_file,
};
use sangkar_index::lang::registry::spec_for;
use sangkar_index::lang::types::LangId;
use sangkar_index::parse::{ParseResult, parse_file};

/// The TypeScript suite's Python fixture, byte for byte, so its assertions apply here too.
const PYTHON_FIXTURE_PATH: &str = "tests/test_widget.py";
const PYTHON_FIXTURE: &str = r#"import os
from pathlib import Path


class Base:
    def label(self):
        return "base"


class Widget(Base):
    def render(self, title):
        prefix = os.sep
        parts = [prefix, title]
        options = {"title": title, "prefix": prefix}
        self.paint(parts, options)
        return prefix

    @route("/items", handler=render)
    def paint(self, parts, options):
        return Path("index.html")


def build(count):
    prefix = str(count)
    return Widget(prefix)


def test_render():
    widget = Widget("x")
    return widget.render("t")
"#;

/// A receiver and a bare callee, one class, so the two `calls` shapes sit side by side.
const PYTHON_RECEIVER_PATH: &str = "src/mod.py";
const PYTHON_RECEIVER: &str = r#"def f():
    return 1


def g():
    a = Widget()
    a.b()
    f()
    return a
"#;

/// A callee with no nameable tail: a subscript is not a name chain, so the site has no name to bind.
const PYTHON_DYNAMIC_PATH: &str = "src/dispatch.py";
const PYTHON_DYNAMIC: &str = r#"handlers = {}


def dispatch(key):
    handlers[key]()
"#;

/// A class, its base, one call and the two value positions after it: the sequence one declaration
/// emits, in source order.
const PYTHON_ORDER_PATH: &str = "src/mod.py";
const PYTHON_ORDER: &str = r#"class Widget(Base):
    def paint(self):
        a.b(parts)
        return a
"#;

/// A superclass, an interface list and a `new`: the shapes Python's grammar has no node for.
const JAVA_PATH: &str = "src/main/java/demo/Widget.java";
const JAVA: &str = r#"package demo;

public class Widget extends Base implements Renderer {
  public void paint() {
    Base fresh = new Base();
    helper.run(fresh);
  }
}
"#;

/// A callee whose object is not a name at all: it still names the member being called, so the site
/// is a `calls` edge rather than an unresolved row.
const TS_NON_NULL_PATH: &str = "src/rows.ts";
const TS_NON_NULL: &str = r#"export function paint(rows: Rows, draft: Draft) {
  return rows!.paint(draft);
}
"#;

/// A call-shaped import: `require "json"` is an `imports` edge and a `calls` edge at once.
const RUBY_PATH: &str = "lib/widget.rb";
const RUBY: &str = r#"require "json"

class Widget
  def render
    helper.paint
  end
end
"#;

/// Extracts one file, or fails with the parse error that stopped it.
fn extract(path: &str, source: &str, lang: LangId) -> ExtractResult {
    let spec = spec_for(lang);
    match parse_file(source.as_bytes(), lang) {
        ParseResult::Ok { tree, .. } => extract_file(&tree, source.as_bytes(), spec, path),
        ParseResult::Err { reason, detail } => panic!("{path} did not parse: {reason}: {detail}"),
    }
}

fn edges<'a>(result: &'a ExtractResult, kind: &str) -> Vec<&'a RawEdge> {
    result
        .raw_edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .collect()
}

/// Every `calls` edge as `target@receiver`, in emission order, `target` alone when no receiver was
/// written.
fn calls(result: &ExtractResult) -> Vec<String> {
    edges(result, "calls")
        .into_iter()
        .map(|edge| match &edge.receiver {
            None => edge.target.clone(),
            Some(receiver) => format!("{}@{receiver}", edge.target),
        })
        .collect()
}

/// Every `references` edge as `site:target@receiver`, in emission order.
fn references(result: &ExtractResult) -> Vec<String> {
    edges(result, "references")
        .into_iter()
        .map(|edge| {
            let site = edge.evidence.as_deref().unwrap_or_default();
            match &edge.receiver {
                None => format!("{site}:{}", edge.target),
                Some(receiver) => format!("{site}:{}@{receiver}", edge.target),
            }
        })
        .collect()
}

/// The targets of every edge of `kind`, in emission order.
fn targets(result: &ExtractResult, kind: &str) -> Vec<String> {
    edges(result, kind)
        .into_iter()
        .map(|edge| edge.target.clone())
        .collect()
}

/// The one edge of `kind` whose target is `target`, or a failure naming how many there were.
fn edge<'a>(result: &'a ExtractResult, kind: &str, target: &str) -> &'a RawEdge {
    let found = edges(result, kind)
        .into_iter()
        .filter(|edge| edge.target == target)
        .collect::<Vec<_>>();
    match found.as_slice() {
        [only] => only,
        _ => panic!(
            "{kind} to {target}: {} edges, expected one; got {:?}",
            found.len(),
            targets(result, kind)
        ),
    }
}

#[test]
fn a_method_call_records_its_receiver_and_a_bare_call_records_none() {
    let result = extract(PYTHON_RECEIVER_PATH, PYTHON_RECEIVER, LangId::Python);
    assert_eq!(calls(&result), ["Widget", "b@a", "f"]);

    // `a.b()`: the callee is a name chain, so the text before the last dot is the receiver.
    let chained = edge(&result, "calls", "b");
    assert_eq!(chained.receiver.as_deref(), Some("a"));
    assert_eq!(chained.src, "src/mod.py::g");
    assert_eq!(chained.line, 7);
    assert_eq!(chained.evidence.as_deref(), Some("call:a.b"));
    assert_eq!(chained.tier, EXTRACTED_CANDIDATE);
    assert_eq!(chained.local_name, "");

    // `f()`: the callee is one name, so there is no receiver to record.
    let bare = edge(&result, "calls", "f");
    assert_eq!(bare.receiver, None);
    assert_eq!(bare.evidence.as_deref(), Some("call:f"));
    assert_eq!(bare.line, 8);
    assert_eq!(bare.tier, EXTRACTED_CANDIDATE);
    assert_eq!(bare.local_name, "");

    // The construction is a call like any other: its callee is the type it names.
    assert_eq!(edge(&result, "calls", "Widget").receiver, None);
}

#[test]
fn a_java_class_inherits_a_base_and_implements_an_interface() {
    let result = extract(JAVA_PATH, JAVA, LangId::Java);

    // `extends Base` — the clause is `superclass`, which names no interface.
    let inherits = edge(&result, "inherits", "Base");
    assert_eq!(inherits.src, "src/main/java/demo/Widget.java::Widget");
    assert_eq!(inherits.line, 3);
    assert_eq!(inherits.receiver, None);
    assert_eq!(inherits.evidence.as_deref(), Some("inherits"));
    assert_eq!(inherits.tier, EXTRACTED_CANDIDATE);

    // `implements Renderer` — the clause's own grammar shape (`super_interfaces`) decides the kind.
    let implements = edge(&result, "implements", "Renderer");
    assert_eq!(implements.src, "src/main/java/demo/Widget.java::Widget");
    assert_eq!(implements.line, 3);
    assert_eq!(implements.receiver, None);
    assert_eq!(implements.evidence.as_deref(), Some("implements"));
    assert_eq!(implements.tier, EXTRACTED_CANDIDATE);

    // Two clauses, one base each, and no third clause mistaken for one of them.
    assert_eq!(targets(&result, "inherits"), ["Base"]);
    assert_eq!(targets(&result, "implements"), ["Renderer"]);

    // `new Base()` is an `object_creation_expression`, a call type: the type it names is the callee.
    let created = edge(&result, "calls", "Base");
    assert_eq!(created.evidence.as_deref(), Some("call:Base"));
    assert_eq!(created.line, 5);
    assert_eq!(created.receiver, None);
    // `helper.run(fresh)` names the method in its own field, which is the callee the row finds.
    assert_eq!(calls(&result), ["Base", "run"]);
    // The created value is a call argument, the only value position this snippet holds.
    assert_eq!(references(&result), ["call_argument:fresh"]);
}

#[test]
fn a_ruby_require_is_an_import_and_a_call_at_once() {
    let result = extract(RUBY_PATH, RUBY, LangId::Ruby);
    // The import half already placed the module; the call half must not drop the site on the floor.
    let imports = edges(&result, "imports")
        .into_iter()
        .map(|edge| format!("{}:{}", edge.target, edge.local_name))
        .collect::<Vec<_>>();
    assert_eq!(imports, ["json:"]);
    assert_eq!(calls(&result), ["require", "paint"]);
    let require = edge(&result, "calls", "require");
    assert_eq!(require.line, 1);
    assert_eq!(require.evidence.as_deref(), Some("call:require"));
}

#[test]
fn a_callee_whose_object_is_not_a_name_keeps_its_member_and_its_object() {
    let result = extract(TS_NON_NULL_PATH, TS_NON_NULL, LangId::TypeScript);
    // `rows!.paint` is not a name chain (the object is an assertion), but it does name `paint`, so
    // the member is the target and the object is the receiver as written — never an unresolved row.
    assert_eq!(calls(&result), ["paint@rows!"]);
    let chained = edge(&result, "calls", "paint");
    assert_eq!(chained.receiver.as_deref(), Some("rows!"));
    assert_eq!(chained.evidence.as_deref(), Some("call:rows!.paint"));
    assert_eq!(chained.src, "src/rows.ts::paint");
    assert!(result.unresolved.is_empty());
    assert_eq!(references(&result), ["call_argument:draft"]);
}

#[test]
fn a_dynamic_call_becomes_one_unresolved_row_with_evidence_kind_dynamic() {
    let result = extract(PYTHON_DYNAMIC_PATH, PYTHON_DYNAMIC, LangId::Python);
    // The unresolved census is a payload contract: a callee with no nameable tail is recorded, never
    // dropped, and `rawTarget` keeps the site as it was written.
    assert_eq!(
        result.unresolved,
        [UnresolvedSite {
            line: 5,
            raw_target: "handlers[key]".to_owned(),
            evidence_kind: "dynamic".to_owned(),
            evidence: "call".to_owned(),
        }]
    );
    assert!(edges(&result, "calls").is_empty());
}

#[test]
fn the_typescript_fixture_finds_every_call_with_the_receiver_it_was_written_with() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    // Source order: the decorator's call precedes the method body's, and both constructions count.
    assert_eq!(
        calls(&result),
        [
            "paint@self",
            "route",
            "Path",
            "str",
            "Widget",
            "Widget",
            "render@widget",
        ]
    );
    let paint = edge(&result, "calls", "paint");
    assert_eq!(paint.src, "tests/test_widget.py::Widget.render");
    assert_eq!(paint.receiver.as_deref(), Some("self"));
    assert_eq!(paint.evidence.as_deref(), Some("call:self.paint"));
    assert_eq!(paint.line, 15);
    let chained = edge(&result, "calls", "render");
    assert_eq!(chained.receiver.as_deref(), Some("widget"));
    assert_eq!(chained.evidence.as_deref(), Some("call:widget.render"));
    // An import statement is not a call: only the call-shaped import produces a `calls` edge.
    assert!(
        edges(&result, "calls")
            .iter()
            .all(|edge| edge.target != "os" && edge.target != "pathlib")
    );
}

#[test]
fn the_typescript_fixture_inherits_from_its_base_and_implements_nothing() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    assert_eq!(edges(&result, "inherits").len(), 1);
    let base = edge(&result, "inherits", "Base");
    assert_eq!(base.src, "tests/test_widget.py::Widget");
    assert_eq!(base.target, "Base");
    assert_eq!(base.receiver, None);
    assert_eq!(base.line, 10);
    assert_eq!(base.tier, EXTRACTED_CANDIDATE);
    assert_eq!(base.evidence.as_deref(), Some("inherits"));
    assert_eq!(base.local_name, "");

    // `argument_list` is also a decorator's and a call's argument list; only the class's own direct
    // child is a base list, so the decorated method inherits from nothing.
    assert!(edges(&result, "implements").is_empty());
    assert!(!targets(&result, "inherits").contains(&"handler".to_owned()));
}

#[test]
fn the_typescript_fixture_produces_all_six_reference_sites_and_nothing_else() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let sites = edges(&result, "references")
        .into_iter()
        .map(|edge| edge.evidence.clone().unwrap_or_default())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        sites,
        REFERENCE_SITES.map(str::to_owned).into_iter().collect()
    );
    // One snippet, every value position, in the order the walk reaches them: a site container
    // contributes its immediate values only, never the values of a literal nested inside it.
    assert_eq!(
        references(&result),
        [
            "assignment_rhs:sep@os",
            "array_element:prefix",
            "array_element:title",
            "object_element:title",
            "object_element:prefix",
            "call_argument:parts",
            "call_argument:options",
            "return_expression:prefix",
            "decorator_argument:render",
            "call_argument:count",
            "call_argument:prefix",
        ]
    );
    // `os.sep` keeps its qualifier, so the bind pass need not re-parse the site.
    let qualified = edge(&result, "references", "sep");
    assert_eq!(qualified.receiver.as_deref(), Some("os"));
    assert_eq!(qualified.line, 12);
    assert_eq!(qualified.src, "tests/test_widget.py::Widget.render");
    assert_eq!(qualified.tier, EXTRACTED_CANDIDATE);
    // A decorator argument belongs to the class the method is declared in, not to the method.
    let decorated = edge(&result, "references", "render");
    assert_eq!(decorated.evidence.as_deref(), Some("decorator_argument"));
    assert_eq!(decorated.src, "tests/test_widget.py::Widget");
    // A literal or a nested call is not a reference; only the names inside it are.
    let line_24 = edges(&result, "references")
        .into_iter()
        .filter(|edge| edge.line == 24)
        .map(|edge| edge.target.clone())
        .collect::<Vec<_>>();
    assert_eq!(line_24, ["count"]);
    // The TypeScript suite asserts the same fixture produces no unresolved site; a call this half
    // dropped would show up here.
    assert!(result.unresolved.is_empty());
}

#[test]
fn the_edges_of_one_declaration_come_out_in_source_order() {
    let result = extract(PYTHON_ORDER_PATH, PYTHON_ORDER, LangId::Python);
    // A class declares before it inherits, and the body keeps its own order: each call is followed
    // by the value positions its arguments and its `return` hold.
    let sequence = result
        .raw_edges
        .iter()
        .map(|edge| format!("{} {}", edge.kind, edge.target))
        .collect::<Vec<_>>();
    assert_eq!(
        sequence,
        [
            "contains src/mod.py::mod",
            "contains src/mod.py::Widget",
            "inherits Base",
            "contains src/mod.py::Widget.paint",
            "calls b",
            "references parts",
            "references a",
        ]
    );
}

#[test]
fn no_endpoint_node_is_ever_produced() {
    // `endpoint` is declared by the union and produced by no rule: a route node needs the method and
    // path of a specific framework, which the endpoint pass owns. The Python fixture carries a
    // `@route(...)` decorator — the exact presence a route predicate would be tempted to key on —
    // and still declares a method, not an endpoint.
    assert!(NODE_KINDS.contains(&"endpoint"));
    for (path, source, lang) in [
        (PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python),
        (PYTHON_RECEIVER_PATH, PYTHON_RECEIVER, LangId::Python),
        (PYTHON_DYNAMIC_PATH, PYTHON_DYNAMIC, LangId::Python),
        (JAVA_PATH, JAVA, LangId::Java),
        (RUBY_PATH, RUBY, LangId::Ruby),
    ] {
        let result = extract(path, source, lang);
        for node in &result.nodes {
            assert!(NODE_KINDS.contains(&node.kind.as_str()));
            assert_ne!(node.kind, "endpoint", "{path} produced an endpoint node");
        }
    }
    let decorated = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let paint = decorated
        .nodes
        .iter()
        .find(|node| node.qn == "tests/test_widget.py::Widget.paint")
        .expect("the decorated method");
    assert_eq!(paint.kind, "method");
    assert!(paint.props.to_json().contains("route("));
}

#[test]
fn every_edge_this_half_emits_is_a_candidate_with_a_source_a_target_and_a_line() {
    for (path, source, lang) in [
        (PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python),
        (PYTHON_RECEIVER_PATH, PYTHON_RECEIVER, LangId::Python),
        (PYTHON_DYNAMIC_PATH, PYTHON_DYNAMIC, LangId::Python),
        (JAVA_PATH, JAVA, LangId::Java),
        (RUBY_PATH, RUBY, LangId::Ruby),
    ] {
        let result = extract(path, source, lang);
        for edge in &result.raw_edges {
            assert_eq!(edge.tier, EXTRACTED_CANDIDATE, "{path}: {edge:?}");
            assert!(RAW_EDGE_KINDS.contains(&edge.kind.as_str()), "{path}");
            assert!(!edge.src.is_empty() && !edge.target.is_empty(), "{path}");
            assert!(edge.line > 0, "{path}: {edge:?}");
            assert!(edge.evidence.is_some(), "{path}: {edge:?}");
        }
        for site in &result.unresolved {
            assert_eq!(site.evidence_kind, "dynamic");
            assert!(!site.raw_target.is_empty() && !site.evidence.is_empty());
            assert!(site.line > 0);
        }
    }
    // Across the snippets above the six edge kinds are all produced; `implements` and `object
    // creation` need Java, the rest fit in one Python fixture.
    let python = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let java = extract(JAVA_PATH, JAVA, LangId::Java);
    let produced = python
        .raw_edges
        .iter()
        .chain(java.raw_edges.iter())
        .map(|edge| edge.kind.as_str())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        produced.into_iter().collect::<Vec<_>>(),
        [
            "calls",
            "contains",
            "implements",
            "imports",
            "inherits",
            "references"
        ]
    );
    assert_eq!(RAW_EDGE_KINDS.len(), 6);
}
