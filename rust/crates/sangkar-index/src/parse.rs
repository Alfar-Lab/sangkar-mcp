//! The indexer's single parser entry point: one parser per language, a bounded pool, and the
//! syntax-error ranges a stored `parse_error_ranges` row is built from.
//!
//! Ported from `src/indexer/parse.ts`, minus the WASM loader that file exists to work around. There,
//! a grammar was a `.wasm` blob loaded on first use through `web-tree-sitter`, its `dylink` header
//! rewritten before Emscripten would accept it, and the whole thing serialised through one FIFO
//! queue because a `Parser` was mutable wasm state. Here a grammar is a `LANGUAGE` constant linked
//! into the binary, so the loader, the runtime initialisation and the queue are gone. What callers
//! depend on remains:
//!
//! - a [`ParserPool`] holds at most [`MAX_CACHED_LANGUAGES`] parsers, least-recently-used first, and
//!   a hit moves its parser to the most-recently-used end;
//! - [`parse_file`] is infallible for a syntax error: a tree always comes back, with up to
//!   [`MAX_ERROR_RANGES`] one-based line ranges saying where the grammar had to recover;
//! - a grammar that cannot be attached to a parser is a structured [`ParseResult::Err`] carrying
//!   [`ParseFailureReason::GrammarUnavailable`], never a panic, so the indexer records a coverage
//!   reason and keeps indexing the rest of the repository.
//!
//! The pool is `Mutex`-guarded and the entry point takes `&self`, so a caller may parse from several
//! threads: the lock is held for the whole parse, which is exactly the one-parse-at-a-time rule the
//! TypeScript queue enforced, but without the queue. No language-inspection or highlighting API is
//! exposed; the extractor needs the tree and nothing else.

use std::collections::{HashSet, VecDeque};
use std::sync::{Mutex, MutexGuard};

use sangkar_core::log;
use tree_sitter::{LanguageError, Parser, Point, Tree};

use crate::lang::registry::tree_sitter_language;
use crate::lang::types::LangId;

/// How many languages keep a parser alive. The pool's queue is the LRU order: least recently used at
/// the front, and a hit is re-appended at the back.
pub const MAX_CACHED_LANGUAGES: usize = 6;

/// How many syntax-error ranges are reported before the rest are summarised as a count, so one
/// pathological file cannot fill a row.
pub const MAX_ERROR_RANGES: usize = 20;

/// Why a file produced no tree. Both values are facts the indexer records, not crashes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseFailureReason {
    /// No parser could be given the language's grammar. With all twelve grammars linked this is
    /// reachable only when the tree-sitter runtime rejects the grammar when it is attached.
    GrammarUnavailable,
    /// The language was attached and the parser still returned no tree. The pool drops that parser,
    /// so a parser that has failed once is never trusted with a second file.
    ParseFailed,
}

impl ParseFailureReason {
    /// The spelling the indexer stores and logs, which is the TypeScript's own.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GrammarUnavailable => "grammar_unavailable",
            Self::ParseFailed => "parse_failed",
        }
    }
}

impl std::fmt::Display for ParseFailureReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of one file: the tree the extractor walks and where it had to recover, or the reason
/// there is no tree.
pub enum ParseResult {
    Ok {
        tree: Tree,
        /// One 1-based line range per syntax error, in tree order: `"12"` or `"12-14"`. At most
        /// [`MAX_ERROR_RANGES`] ranges; when there are more, the last element is
        /// `"+N more ranges"`, so a consumer joining with `", "` gets the capped list and its count
        /// in one string.
        error_ranges: Vec<String>,
    },
    /// No tree. `reason` is the coverage fact; `detail` is for the indexer log, not for a payload.
    Err {
        reason: ParseFailureReason,
        detail: String,
    },
}

/// The parsers held per language, least-recently-used first.
///
/// The lock is held across the parse, not just across the bookkeeping: a `Parser` carries the
/// previous file's state, so two files never share one. `Parser` is `Sync`, but serialising is the
/// ported rule and it costs nothing — parsing one file is the unit of work either way.
pub struct ParserPool {
    cache: Mutex<VecDeque<(LangId, Parser)>>,
}

impl ParserPool {
    /// An empty pool.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cache: Mutex::new(VecDeque::new()),
        }
    }

    /// Parses one file. A syntax error is a [`ParseResult::Ok`] with ranges; only a grammar that
    /// cannot be attached and a parser that returns no tree produce [`ParseResult::Err`].
    pub fn parse_file(&self, source: &[u8], lang: LangId) -> ParseResult {
        let mut cache = self.lock();

        // A hit is removed and re-appended, which is what makes it the most recently used entry; a
        // miss makes room and builds a new parser. Either way the parser is out of the cache while it
        // is in use and goes back at the end, so the pool holds at most MAX_CACHED_LANGUAGES parsers
        // counting the one being used. Dropping an entry releases its grammar, which is what
        // `disposeLang` did by hand in the TypeScript.
        //
        // A lookup that finds nothing to remove falls through to the same arm as a miss, because both
        // mean the same thing here: there is no parser for this language yet.
        let cached = cache
            .iter()
            .position(|(id, _)| *id == lang)
            .and_then(|index| cache.remove(index));
        let (_, mut parser) = match cached {
            Some(entry) => entry,
            None => {
                while cache.len() >= MAX_CACHED_LANGUAGES {
                    cache.pop_front();
                }
                match new_parser(lang) {
                    Ok(parser) => (lang, parser),
                    Err(failed) => return failed,
                }
            }
        };

        // A previous parse may have stopped early, so the parser is told to start from the beginning
        // rather than resume where it left off.
        parser.reset();

        // A syntax error is a tree, not a failure: only a parser that returns nothing at all leaves
        // this call as an `Err`.
        let Some(tree) = parser.parse(source, None) else {
            // The parser is no longer trusted after a failed parse, so it is dropped rather than
            // cached; the next call for this language builds a fresh one.
            return ParseResult::Err {
                reason: ParseFailureReason::ParseFailed,
                detail: format!("the {lang} parser returned no tree"),
            };
        };

        cache.push_back((lang, parser));
        ParseResult::Ok {
            error_ranges: collect_error_ranges(&tree),
            tree,
        }
    }

    /// The cached languages in LRU order, least recently used first: the pool bound and the
    /// move-to-most-recently-used rule made observable, as `parserPoolStats` was in the TypeScript.
    #[must_use]
    pub fn cached_languages(&self) -> Vec<LangId> {
        self.lock().iter().map(|(lang, _)| *lang).collect()
    }

    /// Takes the cache, rebuilding it empty if a thread panicked while holding it.
    ///
    /// A parse that panicked left its parser mid-file, and a parser mid-file is the one thing this
    /// module must not hand to the next call, so the whole cache is dropped rather than trusted. The
    /// alternative — propagating the poison — would turn one panicked file into a parser that never
    /// works again.
    fn lock(&self) -> MutexGuard<'_, VecDeque<(LangId, Parser)>> {
        match self.cache.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                let mut guard = poisoned.into_inner();
                guard.clear();
                log::warn("parse.pool_recovered", &[]);
                guard
            }
        }
    }
}

impl Default for ParserPool {
    fn default() -> Self {
        Self::new()
    }
}

/// The pool every caller of [`parse_file`] shares.
static DEFAULT_POOL: ParserPool = ParserPool::new();

/// Parses one file with the process-wide pool: the entry point the indexer uses.
pub fn parse_file(source: &[u8], lang: LangId) -> ParseResult {
    DEFAULT_POOL.parse_file(source, lang)
}

/// A parser for `lang`, or the structured failure to return instead of one.
fn new_parser(lang: LangId) -> Result<Parser, ParseResult> {
    let language = tree_sitter_language(lang);
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|error| grammar_unavailable(lang, error))?;
    Ok(parser)
}

/// The one route to [`ParseFailureReason::GrammarUnavailable`].
///
/// Every [`LangId`] maps to a grammar linked into this binary, so in the twelve-language table the
/// only failure left is the runtime refusing one at `Parser::set_language` — an ABI mismatch between
/// the tree-sitter crate and a grammar crate, which is a build problem rather than a file problem.
/// Converting the real [`LanguageError`] here keeps that route the same shape as any other failure
/// and the unit tests below assert the mapping without inventing a reachable branch.
fn grammar_unavailable(lang: LangId, error: LanguageError) -> ParseResult {
    ParseResult::Err {
        reason: ParseFailureReason::GrammarUnavailable,
        detail: format!("the {lang} grammar could not be loaded into a parser: {error:?}"),
    }
}

/// The syntax-error ranges of a tree: every `ERROR` node and every `MISSING` node, formatted as a
/// 1-based line range and deduplicated, capped at [`MAX_ERROR_RANGES`] followed by a count suffix.
///
/// One iterative pre-order walk through a `TreeCursor`, as the TypeScript used: a file with no
/// errors still visits every node, and a file a grammar recovered badly from can be deeply nested,
/// so the walk never recurses and has no depth limit to hit.
fn collect_error_ranges(tree: &Tree) -> Vec<String> {
    let mut ranges: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = tree.walk();

    loop {
        let node = cursor.node();
        // `MISSING` is the other half of tree-sitter's error reporting: the grammar inserted a node
        // the file never had, and `is_error` is false for it.
        if node.is_error() || node.is_missing() {
            let range = format_line_range(node.start_position(), node.end_position());
            // The dedupe is over the formatted string, in tree order: two nodes on one line are one
            // reported range, and the first occurrence is the one that decides the position.
            if !seen.contains(range.as_str()) {
                seen.insert(range.clone());
                ranges.push(range);
            }
        }

        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return capped(ranges);
            }
        }
    }
}

/// A node's position as the stored range text: `"12"` for one line, `"12-14"` for a span.
///
/// The TypeScript (`parse.ts:348-356`) is followed exactly, including the part a naive port gets
/// wrong: an error that ends at column 0 of the next row does not cover that row, so the end line is
/// the end row itself rather than the row after it. A range whose end line is not past its start
/// line prints as the bare start line.
fn format_line_range(start: Point, end: Point) -> String {
    let start_line = start.row + 1;
    let end_row = end.row;
    let end_line = if end.column == 0 && end_row > start.row {
        end_row
    } else {
        end_row + 1
    };
    if end_line > start_line {
        format!("{start_line}-{end_line}")
    } else {
        start_line.to_string()
    }
}

/// The first [`MAX_ERROR_RANGES`] ranges plus a final count of the ones that did not fit.
fn capped(mut ranges: Vec<String>) -> Vec<String> {
    if ranges.len() <= MAX_ERROR_RANGES {
        return ranges;
    }
    let dropped = ranges.len() - MAX_ERROR_RANGES;
    ranges.truncate(MAX_ERROR_RANGES);
    ranges.push(format!("+{dropped} more ranges"));
    ranges
}

#[cfg(test)]
mod tests {
    //! Unit tests for the two pieces an integration test cannot see: the range formatting, which is
    //! the column-0 correction the stored text depends on, and the conversion that is the only route
    //! to `grammar_unavailable` in a binary with every grammar linked.

    use super::{
        MAX_ERROR_RANGES, ParseFailureReason, ParseResult, capped, format_line_range,
        grammar_unavailable,
    };
    use crate::lang::types::LangId;
    use tree_sitter::{LanguageError, Point};

    fn point(row: usize, column: usize) -> Point {
        Point { row, column }
    }

    #[test]
    fn one_error_on_one_line_is_a_bare_line_number() {
        assert_eq!(format_line_range(point(11, 4), point(11, 9)), "12");
        // An error that starts and ends on its own first column is still one line.
        assert_eq!(format_line_range(point(0, 0), point(0, 0)), "1");
    }

    #[test]
    fn a_span_that_ends_at_column_zero_does_not_name_the_next_row() {
        // Rows 2..=3 of the file, ending where row 4 begins: three lines, not four.
        assert_eq!(format_line_range(point(1, 0), point(3, 0)), "2-3");
        // The same rows ending mid-row do cover row 4.
        assert_eq!(format_line_range(point(1, 0), point(3, 2)), "2-4");
    }

    #[test]
    fn an_end_on_the_start_row_is_one_line_even_at_column_zero() {
        // The correction only applies when the end row is past the start row.
        assert_eq!(format_line_range(point(0, 0), point(0, 0)), "1");
        assert_eq!(format_line_range(point(4, 7), point(4, 0)), "5");
        // One row further, ending at its column 0, still covers only the start row: the correction
        // takes the end row as it is, and `5` is not past `5`.
        assert_eq!(format_line_range(point(4, 0), point(5, 0)), "5");
        // Two rows further is a span, ending on the corrected row.
        assert_eq!(format_line_range(point(4, 0), point(6, 0)), "5-6");
    }

    #[test]
    fn the_cap_keeps_twenty_ranges_and_counts_the_rest() {
        assert_eq!(capped(Vec::new()), Vec::<String>::new());
        let twenty: Vec<String> = (1..=MAX_ERROR_RANGES)
            .map(|line| line.to_string())
            .collect();
        assert_eq!(
            capped(twenty.clone()),
            twenty,
            "the cap is not reached at 20"
        );

        let twenty_five: Vec<String> = (1..=25).map(|line| line.to_string()).collect();
        let capped_twenty_five = capped(twenty_five.clone());
        assert_eq!(capped_twenty_five.len(), MAX_ERROR_RANGES + 1);
        assert_eq!(
            capped_twenty_five.last().map(String::as_str),
            Some("+5 more ranges")
        );
        assert_eq!(
            &capped_twenty_five[..MAX_ERROR_RANGES],
            &twenty_five[..MAX_ERROR_RANGES],
            "the kept ranges are the first 20, in order"
        );
    }

    #[test]
    fn a_language_error_becomes_grammar_unavailable() {
        // The real error type the real path produces, so the variant's detail text is asserted
        // without a grammar crate having to be built against the wrong tree-sitter.
        let refused = grammar_unavailable(LangId::Python, LanguageError::Version(99));
        match refused {
            ParseResult::Err { reason, detail } => {
                assert_eq!(reason, ParseFailureReason::GrammarUnavailable);
                assert_eq!(reason.as_str(), "grammar_unavailable");
                assert!(
                    detail.contains("python"),
                    "detail names the language: {detail}"
                );
                assert!(
                    detail.contains("Version(99)"),
                    "detail carries the runtime's refusal: {detail}"
                );
            }
            ParseResult::Ok { .. } => panic!("a refused grammar produced a tree"),
        }
        assert_eq!(
            ParseFailureReason::ParseFailed.as_str(),
            "parse_failed",
            "the wire spellings are the TypeScript's"
        );
    }
}
