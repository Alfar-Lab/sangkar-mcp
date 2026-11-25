//! Provenance for the declaration half of the extractor: node kinds, qn shape, `contains` edges,
//! `is_test`, `signature`, `props`, and the `imports` edges with the name each one binds.
//!
//! Ported from `tests/extract.test.ts`, whose Python fixture and its assertions are copied here
//! verbatim, so the values the TypeScript suite pins — `Widget.render`'s signature and line range,
//! the decorator props, the import targets and the containment parents — are checked against the
//! same source in both implementations. One snippet per remaining language family follows, because
//! the walk is table-driven: a language that needed a branch of its own would fail one of these
//! without any test naming a rule.
//!
//! The grammars are `LANGUAGE` constants linked into this binary, so nothing here reaches the
//! network or a WASM directory.

use sangkar_index::extract::{
    EXTRACTED_CANDIDATE, EXTRACTION_EXCEPTIONS, ExtractResult, ExtractedNode, ExtractionRule,
    NODE_KINDS, RAW_EDGE_KINDS, RawEdge, extract_file, has_rule,
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

/// A non-test path, so the `test` kind cannot come from the file name alone.
const PYTHON_SOURCE_PATH: &str = "src/widget.py";
const PYTHON_SOURCE: &str = r#"class Widget(Base):
    def render(self, title):
        return title


def test_smoke():
    return Widget(None)
"#;

/// The smallest Python file with a class and a method: the snippet the task's "done when" names.
const PYTHON_MOD_PATH: &str = "mod.py";
const PYTHON_MOD: &str = r#"class Class(Base):
    def method(self):
        return 1
"#;

const TSX_PATH: &str = "src/panel.tsx";
const TSX: &str = r#"export class Panel {
  render() {
    return <div />;
  }
}
"#;

const TYPESCRIPT_PATH: &str = "src/canvas.ts";
const TYPESCRIPT: &str = r#"import { paint as draw } from "./canvas";

export function outline(width: number): void {
  draw(width);
}
"#;

const JAVA_PATH: &str = "src/main/java/app/Box.java";
const JAVA: &str = r#"package app;

import java.util.List;

class Box {
    void put(String value) {
    }
}
"#;

const GO_PATH: &str = "internal/serve/handler.go";
const GO: &str = r#"package serve

import "fmt"

type Server struct{}

func (s *Server) Serve() {
	fmt.Println("serve")
}
"#;

const RUST_PATH: &str = "src/lib.rs";
const RUST: &str = r#"use std::fmt;

pub struct Widget;

impl Widget {
    pub fn paint(&self) -> u8 {
        0
    }
}
"#;

const RUBY_PATH: &str = "app/widget.rb";
const RUBY: &str = r#"require "json"

class Widget
  def render(title)
    title
  end
end
"#;

/// Extracts one file, or fails with the parse error that stopped it.
fn extract(path: &str, source: &str, lang: LangId) -> ExtractResult {
    extract_bytes(path, source.as_bytes(), lang)
}

fn extract_bytes(path: &str, source: &[u8], lang: LangId) -> ExtractResult {
    let spec = spec_for(lang);
    match parse_file(source, lang) {
        ParseResult::Ok { tree, .. } => extract_file(&tree, source, spec, path),
        ParseResult::Err { reason, detail } => panic!("{path} did not parse: {reason}: {detail}"),
    }
}

/// The node with this qn, or a failure listing the qns that were emitted instead.
fn node<'a>(result: &'a ExtractResult, qn: &str) -> &'a ExtractedNode {
    match result.nodes.iter().find(|node| node.qn == qn) {
        Some(node) => node,
        None => {
            let emitted = result
                .nodes
                .iter()
                .map(|node| node.qn.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            panic!("no node with qn {qn}; got {emitted}")
        }
    }
}

fn qns(result: &ExtractResult) -> Vec<&str> {
    result.nodes.iter().map(|node| node.qn.as_str()).collect()
}

fn edges<'a>(result: &'a ExtractResult, kind: &str) -> Vec<&'a RawEdge> {
    result
        .raw_edges
        .iter()
        .filter(|edge| edge.kind == kind)
        .collect()
}

/// Every `imports` edge as `target:local_name`, in emission order.
fn imports(result: &ExtractResult) -> Vec<String> {
    edges(result, "imports")
        .iter()
        .map(|edge| format!("{}:{}", edge.target, edge.local_name))
        .collect()
}

/// The `src` of the single edge that contains `target`.
fn parent_of<'a>(result: &'a ExtractResult, target: &str) -> &'a str {
    let parents = edges(result, "contains")
        .into_iter()
        .filter(|edge| edge.target == target)
        .map(|edge| edge.src.as_str())
        .collect::<Vec<_>>();
    match parents.as_slice() {
        [only] => only,
        _ => panic!(
            "{target} has {} containing edges, expected one",
            parents.len()
        ),
    }
}

#[test]
fn the_file_node_is_first_and_its_qn_is_the_path_plus_the_file_name() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let file = result.nodes.first().expect("a file node");
    assert_eq!(file.kind, "file");
    assert_eq!(file.qn, "tests/test_widget.py::test_widget.py");
    assert_eq!(file.name, "test_widget.py");
    assert_eq!((file.start_line, file.end_line), (1, 30));
    // The row's `test_file_regex` matched the path, so every node in the file is a test.
    assert!(file.is_test);
    assert_eq!(file.signature, None);
    assert_eq!(file.props.to_json(), "{}");
    assert!(file.props.is_empty());
}

#[test]
fn every_node_kind_the_fixture_can_produce_appears_exactly_once_per_declaration() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let kinds = result
        .nodes
        .iter()
        .map(|node| node.kind.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        kinds.into_iter().collect::<Vec<_>>(),
        ["class", "file", "function", "method", "module", "test"]
    );
    // `endpoint` is declared by the union and produced by no rule yet: a route needs framework
    // props that no node-type table carries, and inventing one would be a language branch.
    assert!(NODE_KINDS.contains(&"endpoint"));
    assert!(!result.nodes.iter().any(|node| node.kind == "endpoint"));
    for node in &result.nodes {
        assert!(
            NODE_KINDS.contains(&node.kind.as_str()),
            "{} is not a declared kind",
            node.kind
        );
    }
}

#[test]
fn a_methods_qn_is_path_class_method_and_a_top_level_function_has_no_scope() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);

    let render = node(&result, "tests/test_widget.py::Widget.render");
    assert_eq!(render.kind, "method");
    assert_eq!(render.name, "render");
    assert_eq!(
        render.signature.as_deref(),
        Some("def render(self, title):")
    );
    assert_eq!((render.start_line, render.end_line), (11, 16));

    let label = node(&result, "tests/test_widget.py::Base.label");
    assert_eq!(label.kind, "method");

    let build = node(&result, "tests/test_widget.py::build");
    assert_eq!(build.kind, "function");
    assert_eq!(build.signature.as_deref(), Some("def build(count):"));
    assert_eq!((build.start_line, build.end_line), (23, 25));
}

#[test]
fn the_class_node_carries_its_header_and_the_module_sits_under_the_file() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);

    let widget = node(&result, "tests/test_widget.py::Widget");
    assert_eq!(widget.kind, "class");
    assert_eq!(widget.signature.as_deref(), Some("class Widget(Base):"));
    assert_eq!((widget.start_line, widget.end_line), (10, 20));

    // The file's own module is its namespace, not a qn prefix: `fileModule`.
    let module = node(&result, "tests/test_widget.py::test_widget");
    assert_eq!(module.kind, "module");
    assert_eq!(module.name, "test_widget");
    assert_eq!(
        parent_of(&result, &module.qn),
        "tests/test_widget.py::test_widget.py"
    );
}

#[test]
fn a_decorator_becomes_the_declarations_props_not_part_of_its_signature() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let paint = node(&result, "tests/test_widget.py::Widget.paint");
    assert_eq!(
        paint.signature.as_deref(),
        Some("def paint(self, parts, options):")
    );
    // The JSON text, not just the value: `preserve_order` is what makes a stored `node.props`
    // column compare equal to the TypeScript object literal's.
    assert_eq!(
        paint.props.to_json(),
        r#"{"decorators":["route(\"/items\", handler=render)"]}"#
    );
}

#[test]
fn the_test_kind_follows_the_declaration_name_not_only_the_file_name() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let declared = node(&result, "tests/test_widget.py::test_render");
    assert_eq!(declared.kind, "test");
    assert!(declared.is_test);

    let source = extract(PYTHON_SOURCE_PATH, PYTHON_SOURCE, LangId::Python);
    let smoke = node(&source, "src/widget.py::test_smoke");
    assert_eq!(smoke.kind, "test");
    assert!(smoke.is_test);
    // A non-test path keeps `is_test` false for everything its file name did not mark.
    assert!(!node(&source, "src/widget.py::Widget").is_test);
    assert_eq!(node(&source, "src/widget.py::Widget.render").kind, "method");
    assert!(!node(&source, "src/widget.py::widget.py").is_test);
}

#[test]
fn every_declaration_has_exactly_one_containing_edge() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    let contains = edges(&result, "contains");
    let mut targets = contains
        .iter()
        .map(|edge| edge.target.as_str())
        .collect::<Vec<_>>();
    targets.sort_unstable();
    let mut declared = result
        .nodes
        .iter()
        .filter(|node| node.kind != "file")
        .map(|node| node.qn.as_str())
        .collect::<Vec<_>>();
    declared.sort_unstable();
    assert_eq!(targets, declared);
    for (edge, _) in contains.iter().zip(&declared) {
        assert_eq!(edge.tier, EXTRACTED_CANDIDATE);
        assert_eq!(edge.evidence.as_deref(), Some("contains"));
        assert_eq!(edge.receiver, None);
        assert_eq!(edge.local_name, "");
    }
}

#[test]
fn containment_runs_from_the_file_or_the_class_never_from_nothing() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    assert_eq!(
        parent_of(&result, "tests/test_widget.py::test_widget"),
        "tests/test_widget.py::test_widget.py"
    );
    assert_eq!(
        parent_of(&result, "tests/test_widget.py::Widget"),
        "tests/test_widget.py::test_widget"
    );
    assert_eq!(
        parent_of(&result, "tests/test_widget.py::Widget.render"),
        "tests/test_widget.py::Widget"
    );
}

#[test]
fn import_statements_become_imports_edges_with_the_local_name_they_bind() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    assert_eq!(imports(&result), ["os:os", "pathlib:Path"]);
    for edge in edges(&result, "imports") {
        assert_eq!(edge.tier, EXTRACTED_CANDIDATE);
        assert_eq!(edge.receiver, None);
        // A top-level import belongs to the file's own module, not to the file node.
        assert_eq!(edge.src, "tests/test_widget.py::test_widget");
        assert_eq!(
            edge.evidence,
            Some(format!("import:{}", edge.target)),
            "the evidence says which site made the edge"
        );
    }
    assert_eq!(
        edges(&result, "imports").first().map(|edge| edge.line),
        Some(1)
    );
}

#[test]
fn the_python_snippet_qn_is_the_class_then_the_method() {
    let result = extract(PYTHON_MOD_PATH, PYTHON_MOD, LangId::Python);
    assert_eq!(
        qns(&result),
        [
            "mod.py::mod.py",
            "mod.py::mod",
            "mod.py::Class",
            "mod.py::Class.method",
        ]
    );
    let method = node(&result, "mod.py::Class.method");
    assert_eq!(method.kind, "method");
    assert_eq!(method.name, "method");
    assert_eq!(parent_of(&result, "mod.py::Class.method"), "mod.py::Class");
    assert_eq!(parent_of(&result, "mod.py::Class"), "mod.py::mod");
    assert_eq!(parent_of(&result, "mod.py::mod"), "mod.py::mod.py");
}

#[test]
fn a_tsx_method_lives_inside_its_class() {
    let result = extract(TSX_PATH, TSX, LangId::Tsx);
    let class = node(&result, "src/panel.tsx::Panel");
    assert_eq!(class.kind, "class");
    let method = node(&result, "src/panel.tsx::Panel.render");
    assert_eq!(method.kind, "method");
    assert_eq!(method.name, "render");
    // The body starts on the declaration's own row, and the header is cut at the body's column in
    // the file line — past the end of this indented row — so the brace stays.
    assert_eq!(method.signature.as_deref(), Some("render() {"));
    assert_eq!(parent_of(&result, "src/panel.tsx::Panel.render"), class.qn);
    // TSX has no `fileModule` rule, so there is no module node between the file and the class.
    assert!(!result.nodes.iter().any(|node| node.kind == "module"));
    assert_eq!(
        parent_of(&result, "src/panel.tsx::Panel"),
        "src/panel.tsx::panel.tsx"
    );
}

#[test]
fn typescript_binds_the_aliased_name_of_an_import() {
    let result = extract(TYPESCRIPT_PATH, TYPESCRIPT, LangId::TypeScript);
    // The specifier is the target; the name the file can call is the local one.
    assert_eq!(imports(&result), ["./canvas:draw"]);
    assert_eq!(edges(&result, "imports")[0].line, 1);
    let outline = node(&result, "src/canvas.ts::outline");
    assert_eq!(outline.kind, "function");
    assert_eq!(
        outline.signature.as_deref(),
        Some("function outline(width: number): void {")
    );
}

#[test]
fn java_declares_a_class_with_a_method_and_imports_its_name() {
    let result = extract(JAVA_PATH, JAVA, LangId::Java);
    assert_eq!(
        node(&result, "src/main/java/app/Box.java::Box").kind,
        "class"
    );
    let put = node(&result, "src/main/java/app/Box.java::Box.put");
    assert_eq!(put.kind, "method");
    assert_eq!(put.signature.as_deref(), Some("void put(String value) {"));
    assert_eq!(
        parent_of(&result, "src/main/java/app/Box.java::Box.put"),
        "src/main/java/app/Box.java::Box"
    );
    // A scoped import is read through the declaration's `name` field: the tail is what it binds.
    assert_eq!(imports(&result), ["List:List"]);
    assert_eq!(edges(&result, "imports")[0].line, 3);
}

#[test]
fn go_declares_a_type_and_a_receiver_function_and_imports_a_package() {
    let result = extract(GO_PATH, GO, LangId::Go);
    let server = node(&result, "internal/serve/handler.go::Server");
    assert_eq!(server.kind, "class");
    // Go's receiver method is a top-level function node: no class node contains it.
    let serve = node(&result, "internal/serve/handler.go::Serve");
    assert_eq!(serve.kind, "function");
    assert_eq!(serve.signature.as_deref(), Some("func (s *Server) Serve()"));
    assert_eq!(
        parent_of(&result, "internal/serve/handler.go::Serve"),
        "internal/serve/handler.go::handler.go"
    );
    // A path-import binds nothing locally: the module it names is the whole of it.
    assert_eq!(imports(&result), ["fmt:"]);

    // Go is the one row whose test pattern carries no `(?i)`: `_test.go` and nothing else.
    let tested = extract("internal/serve/handler_test.go", GO, LangId::Go);
    assert!(node(&tested, "internal/serve/handler_test.go::handler_test.go").is_test);
    assert!(node(&tested, "internal/serve/handler_test.go::Serve").is_test);
}

#[test]
fn rust_declares_a_struct_and_a_function_and_imports_a_module() {
    let result = extract(RUST_PATH, RUST, LangId::Rust);
    let widget = node(&result, "src/lib.rs::Widget");
    assert_eq!(widget.kind, "class");
    assert_eq!(widget.signature.as_deref(), Some("pub struct Widget;"));
    // `impl_item` is a scope, not a type, so `paint` is a top-level function node.
    let paint = node(&result, "src/lib.rs::paint");
    assert_eq!(paint.kind, "function");
    assert_eq!(
        paint.signature.as_deref(),
        Some("pub fn paint(&self) -> u8 {")
    );
    assert_eq!(
        parent_of(&result, "src/lib.rs::paint"),
        "src/lib.rs::lib.rs"
    );
    assert_eq!(imports(&result), ["fmt:fmt"]);
}

#[test]
fn ruby_declares_a_class_with_a_method_and_a_require_is_an_import() {
    let result = extract(RUBY_PATH, RUBY, LangId::Ruby);
    assert_eq!(node(&result, "app/widget.rb::Widget").kind, "class");
    let render = node(&result, "app/widget.rb::Widget.render");
    assert_eq!(render.kind, "method");
    assert_eq!(render.signature.as_deref(), Some("def render(title)"));
    assert_eq!(
        parent_of(&result, "app/widget.rb::Widget.render"),
        "app/widget.rb::Widget"
    );
    // `require` is an ordinary call node the row lists as an import (`callImport`); its argument
    // names the module and nothing is bound by name.
    assert_eq!(imports(&result), ["json:"]);
    assert!(!result.nodes.iter().any(|node| node.kind == "module"));
}

#[test]
fn a_repeated_declaration_is_one_symbol_and_one_edge() {
    let result = extract(
        "src/twice.py",
        "def f():\n    return 1\n\ndef f():\n    return 2\n",
        LangId::Python,
    );
    // `node.qn` is UNIQUE, so the second `f` is the same symbol, not a second row.
    assert_eq!(
        qns(&result),
        [
            "src/twice.py::twice.py",
            "src/twice.py::twice",
            "src/twice.py::f"
        ]
    );
    assert_eq!(edges(&result, "contains").len(), 2);
}

#[test]
fn a_signature_is_collapsed_and_capped_at_two_hundred_characters() {
    let source = format!(
        "function wide(\n{}\n): void {{}}",
        (0..40)
            .map(|index| format!("  parameter{index}: number,"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let result = extract("src/wide.ts", &source, LangId::TypeScript);
    let wide = node(&result, "src/wide.ts::wide");
    let signature = wide.signature.as_deref().expect("a signature");

    // The header up to the body, its rows collapsed to one space: the TypeScript's
    // `clean(rows.join(" "), SIGNATURE_CAP)`.
    let mut rows = vec!["function wide(".to_owned()];
    rows.extend((0..40).map(|index| format!("parameter{index}: number,")));
    let whole = rows
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        whole.chars().count() > 200,
        "the fixture must need truncating"
    );

    assert_ne!(signature, whole);
    assert_eq!(signature, whole.chars().take(200).collect::<String>());
    assert_eq!(signature.chars().count(), 200);
    assert!(!signature.contains('\n'));
    assert!(signature.starts_with("function wide( parameter0: number, parameter1: number,"));
}

#[test]
fn a_body_on_the_signature_row_ends_the_signature() {
    // Python accepts a simple body on the `def` line; the signature stops where the body begins.
    let result = extract("src/one.py", "def alone(): return 1\n", LangId::Python);
    let alone = node(&result, "src/one.py::alone");
    assert_eq!(alone.signature.as_deref(), Some("def alone():"));
}

#[test]
fn a_slice_that_is_not_utf8_is_decoded_lossily_not_panicked_on() {
    // A decorator argument holding a byte that is not UTF-8: the text is decoded with replacement
    // characters, which is what the TypeScript's `node.text` did, rather than a panic.
    let result = extract_bytes(
        "src/odd.py",
        b"@route(\"/x\xffy\")\ndef decorated():\n    return 1\n",
        LangId::Python,
    );
    let decorated = node(&result, "src/odd.py::decorated");
    assert!(
        decorated.props.to_json().contains('\u{fffd}'),
        "{}",
        decorated.props.to_json()
    );
}

#[test]
fn every_raw_edge_is_an_extracted_candidate_of_a_declared_kind() {
    let result = extract(PYTHON_FIXTURE_PATH, PYTHON_FIXTURE, LangId::Python);
    assert!(RAW_EDGE_KINDS.contains(&"contains") && RAW_EDGE_KINDS.contains(&"imports"));
    for edge in &result.raw_edges {
        assert_eq!(edge.tier, EXTRACTED_CANDIDATE);
        assert!(RAW_EDGE_KINDS.contains(&edge.kind.as_str()));
        assert!(edge.line > 0);
        assert!(!edge.src.is_empty());
        assert!(!edge.target.is_empty());
        assert!(edge.evidence.is_some());
    }
}

#[test]
fn the_language_specific_branches_are_a_table_of_two_documented_entries() {
    assert!(EXTRACTION_EXCEPTIONS.len() <= 3);
    assert_eq!(EXTRACTION_EXCEPTIONS.len(), 2);
    for entry in &EXTRACTION_EXCEPTIONS {
        assert!(entry.reason.len() > 20);
        assert!(matches!(
            entry.rule,
            ExtractionRule::FileModule | ExtractionRule::CallImport
        ));
        assert!(has_rule(entry.lang, entry.rule));
    }
    // One file is one module only where the grammar has no module declaration of its own.
    assert!(has_rule(LangId::Python, ExtractionRule::FileModule));
    assert!(!has_rule(LangId::TypeScript, ExtractionRule::FileModule));
    assert!(has_rule(LangId::Ruby, ExtractionRule::CallImport));
    assert!(!has_rule(LangId::Python, ExtractionRule::CallImport));
}
