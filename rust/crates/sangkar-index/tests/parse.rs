//! Provenance for the parser entry point: a tree comes back for every file, the error ranges match
//! the text the TypeScript stored, and the parser pool stays bounded under concurrency.
//!
//! Ported from `tests/parse.test.ts`. The twelve grammars are linked into this test binary, so the
//! failures this suite can observe are the ones a file can cause; a grammar the runtime refuses is a
//! build problem and is covered by the unit tests in `src/parse.rs` instead.

use std::collections::BTreeSet;

use sangkar_index::lang::types::LangId;
use sangkar_index::parse::{
    MAX_CACHED_LANGUAGES, MAX_ERROR_RANGES, ParseFailureReason, ParseResult, ParserPool, parse_file,
};
use tree_sitter::Node;

/// The ranges of a file that must parse, or a failure naming why it did not.
fn ranges_of(source: &str, lang: LangId) -> Vec<String> {
    match parse_file(source.as_bytes(), lang) {
        ParseResult::Ok { error_ranges, .. } => error_ranges,
        ParseResult::Err { reason, detail } => {
            panic!("{lang} {source:?} failed: {reason}: {detail}")
        }
    }
}

/// The tree and the ranges of a file that must parse.
fn tree_and_ranges(source: &str, lang: LangId) -> (tree_sitter::Tree, Vec<String>) {
    match parse_file(source.as_bytes(), lang) {
        ParseResult::Ok { tree, error_ranges } => (tree, error_ranges),
        ParseResult::Err { reason, detail } => {
            panic!("{lang} {source:?} failed: {reason}: {detail}")
        }
    }
}

/// `"12"` or `"12-14"`: the stored form, with no leading zero and no column.
fn is_line_range(range: &str) -> bool {
    let (start, end) = match range.split_once('-') {
        Some((start, end)) => (start, Some(end)),
        None => (range, None),
    };
    let digits_only = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| byte.is_ascii_digit())
            && !(part.len() > 1 && part.starts_with('0'))
    };
    digits_only(start) && end.is_none_or(digits_only)
}

/// One file per language, the smallest one that parses with no error: the pool must attach every
/// grammar in the table, not the three the fixtures below happen to exercise.
const WELL_FORMED: [(LangId, &str); 12] = [
    (LangId::Python, "def f():\n    return 1\n"),
    (LangId::TypeScript, "const x: number = 1;\n"),
    (LangId::Tsx, "const x = <div />;\n"),
    (LangId::JavaScript, "const x = 1;\n"),
    (LangId::Go, "package main\n\nfunc main() {}\n"),
    (LangId::Java, "class A {}\n"),
    (LangId::Rust, "fn main() {}\n"),
    (LangId::C, "int main(void) { return 0; }\n"),
    (LangId::Cpp, "int main() { return 0; }\n"),
    (LangId::CSharp, "class A {}\n"),
    (LangId::Php, "<?php\n$a = 1;\n"),
    (LangId::Ruby, "x = 1\n"),
];

/// Counts the nodes this crate reports on, independently of how it walks them.
fn error_node_count(tree: &tree_sitter::Tree) -> usize {
    fn walk(node: Node<'_>, count: &mut usize) {
        if node.is_error() || node.is_missing() {
            *count += 1;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(child, count);
        }
    }
    let mut count = 0;
    walk(tree.root_node(), &mut count);
    count
}

#[test]
fn a_well_formed_file_per_language_parses_with_no_error_ranges() {
    let mut problems = Vec::new();
    for (lang, source) in WELL_FORMED {
        match parse_file(source.as_bytes(), lang) {
            ParseResult::Ok { tree, error_ranges } => {
                if tree.root_node().has_error() {
                    problems.push(format!("{lang}: the tree reports an error"));
                }
                if !error_ranges.is_empty() {
                    problems.push(format!("{lang}: error ranges {error_ranges:?}"));
                }
                if tree.root_node().end_position().row == 0 && source.lines().count() > 1 {
                    problems.push(format!("{lang}: the tree has no lines"));
                }
            }
            ParseResult::Err { reason, detail } => {
                problems.push(format!("{lang}: {reason}: {detail}"));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "well-formed files failed:\n{}",
        problems.join("\n")
    );
}

#[test]
fn a_syntax_error_is_a_tree_with_the_stored_range_text() {
    // A missing `)` is a MISSING node, which is one line of the file.
    assert_eq!(ranges_of("def f(:\n  pass\n", LangId::Python), ["1"]);

    // An extra `:` gives an ERROR node that runs to the next line and ends at its column 0, so the
    // range names line 2 without naming line 3: the column-0 correction, end to end.
    assert_eq!(ranges_of("def f(::\n  pass\n", LangId::Python), ["1-2"]);

    // The same file twice: the second parse reuses the pooled parser and must agree with the first.
    let first = ranges_of("def f(::\n  pass\n", LangId::Python);
    assert_eq!(first, ranges_of("def f(::\n  pass\n", LangId::Python));

    // A file that parses with errors is still `Ok`, and every entry is the stored form.
    let (tree, ranges) = tree_and_ranges("def f(::\n  pass\n", LangId::Python);
    assert!(tree.root_node().has_error(), "the tree carries the errors");
    assert!(!ranges.is_empty(), "at least one range is reported");
    assert!(
        ranges.iter().any(|range| range.contains('-')),
        "a range that covers more than one line prints as `a-b`: {ranges:?}"
    );
    for range in &ranges {
        assert!(is_line_range(range), "not the stored form: {range:?}");
    }
}

#[test]
fn the_same_range_from_two_nodes_on_one_line_is_reported_once() {
    // Two ERROR nodes, both on line 1: the dedupe is over the formatted text, not the node ids, so
    // one line is one entry.
    let source = "x = ) + )\n";
    let (tree, ranges) = tree_and_ranges(source, LangId::Python);
    assert_eq!(
        error_node_count(&tree),
        2,
        "the fixture has two error nodes"
    );
    assert_eq!(ranges, ["1"]);
}

#[test]
fn twenty_five_errors_report_twenty_ranges_and_a_count_of_the_rest() {
    // One bad statement per line, so the fixture has 25 distinct error lines by construction.
    let source: String = (1..=25)
        .map(|index| format!("let value{index} = ;\n"))
        .collect();
    let (tree, ranges) = tree_and_ranges(&source, LangId::Rust);
    assert_eq!(
        error_node_count(&tree),
        25,
        "the fixture has 25 error nodes"
    );

    assert_eq!(
        ranges.len(),
        MAX_ERROR_RANGES + 1,
        "20 ranges plus the count: {ranges:?}"
    );
    let expected: Vec<String> = (1..=MAX_ERROR_RANGES)
        .map(|line| line.to_string())
        .collect();
    assert_eq!(
        &ranges[..MAX_ERROR_RANGES],
        &expected[..],
        "the first 20 are in tree order"
    );
    assert_eq!(ranges.last().map(String::as_str), Some("+5 more ranges"));
    for range in &ranges[..MAX_ERROR_RANGES] {
        assert!(is_line_range(range), "not the stored form: {range:?}");
    }
}

#[test]
fn every_language_in_the_table_has_a_grammar_to_parse_with() {
    // The twelve ids all map to a linked grammar, so `GrammarUnavailable` is reachable only through a
    // runtime refusal — the route `src/parse.rs` unit-tests on the real `LanguageError`. What this
    // test can assert is the variant's identity and that no table row takes that route.
    assert_eq!(
        ParseFailureReason::GrammarUnavailable.as_str(),
        "grammar_unavailable"
    );
    assert_eq!(ParseFailureReason::ParseFailed.as_str(), "parse_failed");
    assert_eq!(
        ParseFailureReason::GrammarUnavailable.to_string(),
        "grammar_unavailable"
    );
    assert_ne!(
        ParseFailureReason::GrammarUnavailable,
        ParseFailureReason::ParseFailed
    );

    for lang in LangId::ALL {
        assert!(
            matches!(parse_file(b"", lang), ParseResult::Ok { .. }),
            "{lang} has no grammar"
        );
    }
}

#[test]
fn a_binary_blob_is_a_result_not_a_panic() {
    // Not source text in any of the twelve languages. The contract is only that nothing panics: the
    // tree may carry errors and the ranges must still be the stored form.
    for lang in LangId::ALL {
        match parse_file(b"\x00\x01\x02\x03", lang) {
            ParseResult::Ok { error_ranges, .. } => {
                for range in &error_ranges {
                    assert!(
                        is_line_range(range),
                        "{lang}: not the stored form: {range:?}"
                    );
                }
            }
            ParseResult::Err { reason, detail } => {
                assert_eq!(reason, ParseFailureReason::ParseFailed, "{lang}: {detail}");
            }
        }
    }
}

#[test]
fn the_pool_keeps_six_languages_and_moves_a_hit_to_the_most_recently_used_end() {
    let pool = ParserPool::new();
    for lang in [
        LangId::Python,
        LangId::Go,
        LangId::Rust,
        LangId::C,
        LangId::Java,
        LangId::Cpp,
    ] {
        assert!(matches!(pool.parse_file(b"", lang), ParseResult::Ok { .. }));
    }
    assert_eq!(
        pool.cached_languages(),
        [
            LangId::Python,
            LangId::Go,
            LangId::Rust,
            LangId::C,
            LangId::Java,
            LangId::Cpp
        ]
    );

    // A hit moves its entry to the end rather than evicting anything.
    assert!(matches!(
        pool.parse_file(b"", LangId::Python),
        ParseResult::Ok { .. }
    ));
    assert_eq!(
        pool.cached_languages(),
        [
            LangId::Go,
            LangId::Rust,
            LangId::C,
            LangId::Java,
            LangId::Cpp,
            LangId::Python
        ]
    );

    // The next miss evicts the least recently used entry, which is `Go` and no longer `Python`.
    assert!(matches!(
        pool.parse_file(b"", LangId::Ruby),
        ParseResult::Ok { .. }
    ));
    assert_eq!(
        pool.cached_languages(),
        [
            LangId::Rust,
            LangId::C,
            LangId::Java,
            LangId::Cpp,
            LangId::Python,
            LangId::Ruby
        ]
    );
    assert_eq!(pool.cached_languages().len(), MAX_CACHED_LANGUAGES);
}

#[test]
fn many_threads_share_one_pool_and_every_file_parses() {
    // The entry point takes `&self`, so the pool is the one shared object here and the mutex is the
    // only thing keeping the parsers apart. Twelve threads over twelve languages means evictions and
    // a grammar built under contention, which is where a `&mut self` signature would have deadlocked.
    let pool = std::sync::Arc::new(ParserPool::new());
    let handles: Vec<_> = (0..LangId::ALL.len() * 3)
        .map(|index| {
            let pool = std::sync::Arc::clone(&pool);
            let (lang, source) = WELL_FORMED[index % WELL_FORMED.len()];
            std::thread::spawn(move || match pool.parse_file(source.as_bytes(), lang) {
                ParseResult::Ok { error_ranges, .. } => error_ranges,
                ParseResult::Err { reason, detail } => panic!("{lang}: {reason}: {detail}"),
            })
        })
        .collect();

    let mut failures = Vec::new();
    for (index, handle) in handles.into_iter().enumerate() {
        let ranges = handle.join().expect("a parse thread panicked");
        if !ranges.is_empty() {
            let (lang, _) = WELL_FORMED[index % WELL_FORMED.len()];
            failures.push(format!("{lang}: error ranges {ranges:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "parses under contention disagreed:\n{}",
        failures.join("\n")
    );

    let cached = pool.cached_languages();
    assert!(
        cached.len() <= MAX_CACHED_LANGUAGES,
        "the pool grew past its bound: {cached:?}"
    );
    let unique: BTreeSet<LangId> = cached.iter().copied().collect();
    assert_eq!(
        unique.len(),
        cached.len(),
        "a language was cached twice: {cached:?}"
    );
}
