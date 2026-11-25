//! Provenance for the language table: what `registry.rs` claims is checked against the grammar this
//! binary actually loads, not against a node-types listing read while writing the table.
//!
//! Ported from `tests/lang.test.ts`. Two probes per language. First, every declared node type must be
//! a named kind in the loaded grammar. Second, one snippet per language is parsed and must contain a
//! node of every declared type — that is what makes the five lists a claim about code rather than a
//! vocabulary. The grammars are `LANGUAGE` constants compiled into the test binary, so nothing here
//! reaches the network or a WASM directory.
//!
//! One part of the TypeScript suite is not executed here: it ran each row's `testFileRegex` against a
//! test path and a source path. This crate has no regex engine, so the row carries the pattern as a
//! string and `the_test_file_pattern_of_every_row_is_the_ported_one` checks that string against an
//! independent copy of the TypeScript literal instead. Executing it belongs to the extractor, which
//! matches `test_file_regex` against the path to set `is_test`.

use std::collections::{BTreeMap, BTreeSet};

use sangkar_index::lang::registry::{LANGS, detect_language, spec_for, tree_sitter_language};
use sangkar_index::lang::types::{LangId, LangRow};
use tree_sitter::{Language, Node, Parser};

/// Extensions the MVP promises to index, independent of how the table is written.
const REQUIRED_EXTENSIONS: &[(&str, LangId)] = &[
    (".py", LangId::Python),
    (".ts", LangId::TypeScript),
    (".tsx", LangId::Tsx),
    (".js", LangId::JavaScript),
    (".jsx", LangId::JavaScript),
    (".mjs", LangId::JavaScript),
    (".cjs", LangId::JavaScript),
    (".go", LangId::Go),
    (".java", LangId::Java),
    (".rs", LangId::Rust),
    (".c", LangId::C),
    (".h", LangId::C),
    (".cpp", LangId::Cpp),
    (".cc", LangId::Cpp),
    (".hpp", LangId::Cpp),
    (".cs", LangId::CSharp),
    (".php", LangId::Php),
    (".rb", LangId::Ruby),
];

/// The `testFileRegex` of every row, written as the TypeScript table writes it with the `/i` flag
/// folded into the inline `(?i)` group — Go is the one row that had no `/i`, so no `(?i)`.
///
/// This is a second, independent copy of the literals in `src/indexer/lang/registry.ts`. It is the
/// check a copy can fail without noticing: a dropped `[^/]*` or a `\.` typed as `.` still reads fine
/// in the row it was copied into.
const TEST_FILE_PATTERNS: &[(LangId, &str)] = &[
    (
        LangId::Python,
        r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.py$",
    ),
    (
        LangId::TypeScript,
        r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.(?:ts|mts|cts)$",
    ),
    (
        LangId::Tsx,
        r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.tsx$",
    ),
    (
        LangId::JavaScript,
        r"(?i)(^|/)(?:__tests__/[^/]*|[^/]*\.(?:test|spec))\.(?:js|jsx|mjs|cjs)$",
    ),
    (LangId::Go, r"(^|/)[^/]*_test\.go$"),
    (
        LangId::Java,
        r"(?i)(^|/)(?:src/test/[^/]*|[^/]*(?:Test|Tests|IT))\.java$",
    ),
    (LangId::Rust, r"(?i)(^|/)(?:tests?/[^/]*|[^/]*_test)\.rs$"),
    (
        LangId::C,
        r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.(?:c|h)$",
    ),
    (
        LangId::Cpp,
        r"(?i)(^|/)(?:tests?/[^/]*|test_[^/]*|[^/]*_test)\.(?:cc|cpp|cxx|hpp|hh)$",
    ),
    (
        LangId::CSharp,
        r"(?i)(^|/)(?:[^/]*\.Tests/[^/]*|[^/]*(?:Test|Tests))\.cs$",
    ),
    (LangId::Php, r"(?i)(^|/)(?:tests?/[^/]*|[^/]*Test)\.php$"),
    (
        LangId::Ruby,
        r"(?i)(^|/)(?:specs?/[^/]*|[^/]*(?:_spec|_test))\.rb$",
    ),
];

/// One file per language, exercising every type the row declares in each of the five lists.
struct Probe {
    id: LangId,
    snippet: &'static str,
}

const PROBES: [Probe; 12] = [
    Probe {
        id: LangId::Python,
        snippet: r#"import os
from os import path as osp

class Base:
    pass

class Widget(Base):
    def render(self):
        return os.path.join("a", "b")
"#,
    },
    Probe {
        id: LangId::TypeScript,
        snippet: r#"import { helper } from "./helper";

interface Shape extends Base {
  area(): number;
}

type Maybe<T> = T extends string ? T : never;

enum Color {
  Red,
}

class Base {}

abstract class Widget extends Base implements Shape {
  abstract paint(): void;

  area(): number {
    return helper();
  }
}

function make(): Widget {
  return new Widget();
}

function* counter() {
  yield 1;
}

const anon = class {};
const arrow = (): Widget => new Widget();
"#,
    },
    Probe {
        id: LangId::Tsx,
        snippet: r#"import { helper } from "./helper";

interface Props extends Base {
  name: string;
}

type Maybe<T> = T extends string ? T : never;

enum Color {
  Red,
}

class Base {}

abstract class Panel {
  abstract paint(): void;
}

class Widget extends Base implements Props {
  render() {
    return <div onClick={() => helper()}>{this.name}</div>;
  }
}

function make(): Widget {
  return new Widget();
}

function* counter() {
  yield 1;
}

const anon = class {};
const arrow = () => <span />;
"#,
    },
    Probe {
        id: LangId::JavaScript,
        snippet: r#"import { helper } from "./helper.js";

class Base {}

class Widget extends Base {
  run() {
    helper();
    return new Base();
  }
}

function plain() {
  return 1;
}

function* counter() {
  yield 1;
}

const anon = class {};
const arrow = () => helper();
"#,
    },
    Probe {
        id: LangId::Go,
        snippet: r#"package main

import "fmt"

type Base struct{}

type Widget struct {
	Base
}

type Shape interface {
	Area() float64
}

func (w Widget) Area() float64 {
	fmt.Println("area")
	return 1
}

func main() {
	Widget{}.Area()
}
"#,
    },
    Probe {
        id: LangId::Java,
        snippet: r#"import java.util.List;

interface Shape {
  double area();
}

enum Color {
  RED
}

class Base {}

class Widget extends Base implements Shape {
  Widget() {}

  public double area() {
    List<String> names = new java.util.ArrayList<String>();
    Object ref = String::valueOf;
    return size();
  }

  int size() {
    return 1;
  }
}
"#,
    },
    Probe {
        id: LangId::Rust,
        snippet: r#"use std::fmt;

struct Base;

enum Color {
    Red,
}

trait Shape: fmt::Debug {
    fn area(&self) -> f64;
}

impl Shape for Base {
    fn area(&self) -> f64 {
        helper()
    }
}

fn helper() -> f64 {
    println!("area");
    1.0
}
"#,
    },
    Probe {
        id: LangId::C,
        snippet: r#"#include <stdio.h>

typedef struct point {
  int x;
} point_t;

int area(struct point p) {
  return compute(p.x);
}
"#,
    },
    Probe {
        id: LangId::Cpp,
        snippet: r#"#include <string>

struct Base {};

class Widget : public Base {
 public:
  int size_;

  int area() {
    return compute();
  }
};

int count = 0;
"#,
    },
    Probe {
        id: LangId::CSharp,
        snippet: r#"using System;

interface IShape {
  double Area();
}

enum Color {
  Red
}

struct Point {
  public int X;
}

record Money(int Amount);

class Base {}

class Widget : Base, IShape {
  Widget() {}

  public double Area() {
    var list = new System.Collections.Generic.List<int>();
    return Helper();
  }

  double Helper() {
    return 1;
  }
}
"#,
    },
    Probe {
        id: LangId::Php,
        snippet: r#"<?php

namespace App;

use Other\Thing;

include "a.php";
include_once "b.php";
require "c.php";
require_once "d.php";

interface Shape {}

trait Loggable {}

enum Suit {
  case Hearts;
}

class Base {}

function area() {
  return 1;
}

class Widget extends Base implements Shape {
  public function run(?Base $other) {
    $other?->size();
    self::make();
    new Base();
    area();
    return $this->size();
  }

  public function size() {
    return 1;
  }
}
"#,
    },
    Probe {
        id: LangId::Ruby,
        snippet: r#"require "json"

module Helpers
end

class Base
end

class Widget < Base
  def area
    size
  end

  def self.make
    1
  end
end
"#,
    },
];

/// The five lists with the names the TypeScript suite used, so a failure reads the same.
fn lists_of(row: &LangRow) -> [(&'static str, &'static [&'static str]); 5] {
    [
        ("class_types", row.class_types),
        ("function_types", row.function_types),
        ("import_types", row.import_types),
        ("call_types", row.call_types),
        ("inheritance_types", row.inheritance_types),
    ]
}

/// Every named kind the grammar can produce, the way `tests/lang.test.ts` read the loaded language's
/// `types` array: all symbol ids that are neither anonymous nor hidden.
fn named_kinds(language: &Language) -> BTreeSet<&str> {
    (0..language.node_kind_count())
        .filter_map(|id| u16::try_from(id).ok())
        .filter(|id| language.node_kind_is_named(*id))
        .filter_map(|id| language.node_kind_for_id(id))
        .collect()
}

/// The kinds present in a tree, over named children only — the same walk the TypeScript suite made.
fn visit<'tree>(node: Node<'tree>, seen: &mut BTreeSet<&'tree str>) {
    seen.insert(node.kind());
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        visit(child, seen);
    }
}

fn parse(id: LangId, snippet: &str) -> tree_sitter::Tree {
    let language = tree_sitter_language(id);
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .unwrap_or_else(|error| panic!("{id}: the row's grammar did not load: {error}"));
    parser
        .parse(snippet, None)
        .unwrap_or_else(|| panic!("{id}: tree-sitter returned no tree"))
}

#[test]
fn the_mvp_extensions_resolve() {
    for (extension, id) in REQUIRED_EXTENSIONS {
        assert_eq!(
            detect_language(&format!("src/widget{extension}")).as_ref(),
            Some(id),
            "{extension} must resolve to {id}"
        );
    }
}

#[test]
fn dot_h_is_c_while_dot_cpp_is_cpp() {
    assert_eq!(detect_language("include/widget.h"), Some(LangId::C));
    assert_eq!(detect_language("src/widget.cpp"), Some(LangId::Cpp));
    assert_eq!(detect_language("src/widget.hpp"), Some(LangId::Cpp));
}

#[test]
fn detect_language_is_extension_only_and_case_insensitive() {
    assert_eq!(detect_language("pkg/WIDGET.PY"), Some(LangId::Python));
    assert_eq!(
        detect_language("src/nested/deep/widget.ts"),
        Some(LangId::TypeScript)
    );
    // No extension at all, which is what `""` becomes after `extname`: a directory-only path, an
    // extensionless name, and a name whose only dot is the leading one.
    assert_eq!(detect_language(""), None);
    assert_eq!(detect_language("Makefile"), None);
    assert_eq!(detect_language("src/widget"), None);
    assert_eq!(detect_language(".py"), None);
    // A language the MVP does not index.
    assert_eq!(detect_language("src/widget.kt"), None);
}

#[test]
fn detect_language_resolves_dot_d_ts_to_typescript() {
    // `extname("widget.d.ts")` is `.ts`, so a declaration file is a TypeScript path — the quirk the
    // TypeScript table relied on and the Rust port has to keep.
    assert_eq!(
        detect_language("types/widget.d.ts"),
        Some(LangId::TypeScript)
    );
    assert_eq!(detect_language(".d.ts"), Some(LangId::TypeScript));
}

#[test]
fn every_declared_extension_maps_back_to_its_own_language_and_nothing_else_is_mapped() {
    let declared = declared_extensions();
    for (extension, id) in &declared {
        assert_eq!(
            detect_language(&format!("src/widget{extension}")),
            Some(*id),
            "{extension} is declared by {id} and must resolve to it"
        );
    }
    // Nothing beyond the declared table is detected: an entry the table does not have would be a
    // second source of truth, so an undeclared extension must be `None` — near misses included.
    for candidate in [
        ".pyw", ".tsv", ".gyp", ".hxx", ".csx", ".php5", ".rake", ".kt",
    ] {
        assert_eq!(
            detect_language(&format!("src/widget{candidate}")),
            None,
            "{candidate} is not declared by any row"
        );
    }
}

#[test]
fn the_extension_map_is_built_in_row_order() {
    // First-row-wins is what scanning `LANGS` in order gives, so folding the declared extensions with
    // `or_insert` and comparing is the rule itself rather than a restatement of it. No two rows claim
    // the same extension today, which is why the length check below is the part that can fail: a
    // shared extension must be a table edit, and this test refuses one that is not.
    let declared = declared_extensions();
    let mut owners: BTreeMap<&str, LangId> = BTreeMap::new();
    for (extension, id) in &declared {
        owners.entry(*extension).or_insert(*id);
    }
    assert_eq!(
        owners.len(),
        declared.len(),
        "an extension is claimed by more than one row: {declared:?}"
    );
    for (extension, id) in &owners {
        assert_eq!(
            detect_language(&format!("src/widget{extension}")),
            Some(*id),
            "{extension} belongs to the first row that claims it, which is {id}"
        );
    }
}

#[test]
fn langs_holds_exactly_the_twelve_mvp_ids_and_spec_for_returns_the_row() {
    assert_eq!(LANGS.len(), LangId::ALL.len());
    for (index, row) in LANGS.iter().enumerate() {
        assert_eq!(
            row.id,
            LangId::ALL[index],
            "row {index} is out of table order"
        );
        assert_eq!(row.id.index(), index, "{} is not at its own index", row.id);
        let resolved = spec_for(row.id);
        assert!(
            std::ptr::eq(resolved, row),
            "spec_for({}) must return the row itself",
            row.id
        );
        assert_eq!(resolved.id, row.id);
        assert_eq!(
            PROBES[index].id, row.id,
            "probe {index} is out of table order"
        );
    }
}

#[test]
fn the_test_file_pattern_of_every_row_is_the_ported_one() {
    for (id, expected) in TEST_FILE_PATTERNS {
        let row = spec_for(*id);
        let pattern = row
            .test_file_regex
            .unwrap_or_else(|| panic!("{id}: the row declares no test file pattern"));
        assert_eq!(&pattern, expected, "{id}: test file pattern drifted");
        // The anchor is what makes a pattern a whole-path match rather than a substring search, and
        // the `(?i)` group is where the TypeScript `/i` flag went.
        let body = pattern.strip_prefix("(?i)").unwrap_or(pattern);
        assert!(
            body.starts_with("(^|/)"),
            "{id}: a pattern must anchor at the start of a path segment"
        );
        assert!(
            body.ends_with('$'),
            "{id}: a pattern must be anchored at the end"
        );
        assert_eq!(
            pattern.starts_with("(?i)"),
            *id != LangId::Go,
            "{id}: only the Go row was declared without the /i flag"
        );
    }
}

#[test]
fn every_non_inheritance_list_is_non_empty_for_all_twelve_languages() {
    let mut empty = Vec::new();
    for row in &LANGS {
        for (list, types) in lists_of(row) {
            if list != "inheritance_types" && types.is_empty() {
                empty.push(format!("{}.{list}", row.id));
            }
        }
    }
    assert!(empty.is_empty(), "lists with no node types: {empty:?}");
}

#[test]
fn inheritance_is_declared_wherever_the_language_has_the_clause_and_c_declares_none() {
    // C is the one language in the MVP with no superclass or interface clause in its grammar, so its
    // empty list is a fact of the table, not a gap to fill with a look-alike node.
    for row in &LANGS {
        assert_eq!(
            row.inheritance_types.is_empty(),
            row.id == LangId::C,
            "{}: inheritance list is wrong for this language",
            row.id
        );
    }
}

#[test]
fn every_declared_node_type_is_a_named_kind_in_the_loaded_grammar() {
    let mut missing = Vec::new();
    for row in &LANGS {
        let language = tree_sitter_language(row.id);
        let named = named_kinds(&language);
        for (list, types) in lists_of(row) {
            for kind in types {
                if !named.contains(kind) {
                    missing.push(format!("{}.{list}: {kind}", row.id));
                }
            }
        }
    }
    assert!(
        missing.is_empty(),
        "declared node types the loaded grammar does not have:\n{}",
        missing.join("\n")
    );
}

#[test]
fn one_snippet_per_language_parses_cleanly_and_contains_every_declared_node_type() {
    let mut problems = Vec::new();
    let mut probed = 0_usize;
    for probe in &PROBES {
        let row = spec_for(probe.id);
        let tree = parse(probe.id, probe.snippet);
        let root = tree.root_node();
        if root.has_error() {
            problems.push(format!("{}: the snippet contains an ERROR node", probe.id));
        }
        let mut observed = BTreeSet::new();
        visit(root, &mut observed);

        let mut count = 0;
        for (list, types) in lists_of(row) {
            for kind in types {
                count += 1;
                if !observed.contains(kind) {
                    problems.push(format!(
                        "{}.{list}: {kind} is not in the parsed tree",
                        probe.id
                    ));
                }
            }
        }
        probed += count;
        println!("{}: {count} node types probed", probe.id);
    }

    println!(
        "probed {probed} declared node types across {} languages",
        PROBES.len()
    );
    assert!(
        problems.is_empty(),
        "declared node types the snippets do not exercise:\n{}",
        problems.join("\n")
    );
}

/// `(extension, id)` for every extension the table declares, in row and then in-row order.
fn declared_extensions() -> Vec<(&'static str, LangId)> {
    let mut declared = Vec::new();
    for row in &LANGS {
        for extension in row.extensions {
            declared.push((*extension, row.id));
        }
    }
    declared
}
