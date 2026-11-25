//! `search` over a hand-written `GraphReader`: no database, no filesystem, no network.
//!
//! Ported from `tests/tools-search.test.ts`. The fake returns hits in the order the real reader
//! guarantees — `bm25` ascending, then `in_degree` descending, then `id` ascending — so the
//! assertions are about what the *tool* does with that order: preserve it, slice it at a cursor
//! position, and never re-sort. The highest-degree node is deliberately the *worst* score in the
//! fixture, because a tool that sorted by degree would still pass a fixture where the two agree.
//!
//! Five groups, and each answers a different question:
//!
//! - the payload's key set, key **order** and the degrees a caller judges a hit by;
//! - the two filters: `kind`, and `path` where `src` must not match `src-old/`;
//! - the FTS5 match expression [`match_query_of`] builds, which is the caller's text VERBATIM: a
//!   `*` must widen like a prefix query and `OR` must disjoin, because that is what
//!   `src/tools/search/handler.ts:301` does. The fake models a real FTS5 in the one way that matters
//!   here: the expression it was told to expect returns the matched rows, and *any other* expression
//!   returns the wider set a prefix query or a bare operator would have matched;
//! - the refusals: an unpublished index, a cursor that is not this call's, a budget below the floor,
//!   and a read that failed (an error envelope, never `not_found`);
//! - the captured TypeScript answers (`rust/tests/golden/polyglot/search.json` and `search.page.json`),
//!   compared byte for byte. The page golden is the Done-when: its `next` token is reproduced exactly,
//!   which pins the generation, the repo, the page position and the argument digest at once.
//!
//! The fixture's reader is a fake rather than the daemon's `.sangkar/graph.db`: that file is rewritten
//! by the TypeScript daemon while this suite runs, so every row here is one this suite wrote.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use sangkar_core::budget::BYTES_PER_TOKEN;
use sangkar_core::config::load_config;
use sangkar_core::cursor::{Cursor, CursorExpectation, CursorPayload};
use sangkar_core::envelope::{Status, TestCensus, TruncationReason};
use sangkar_core::errors::AppError;
use sangkar_core::paths::is_under_prefix;
use sangkar_store::read::context::{CursorFacade, Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, LineRange,
    TestGapRow, TombstoneRow,
};
use sangkar_tools::search::handler::{
    READER_BOUND, SearchDegree, SearchEnvelope, SearchResult, args_hash_of, handle, match_query_of,
};
use sangkar_tools::search::manifest::{
    DEFAULT_BUDGET_TOKENS, DESCRIPTION, EXAMPLE, ExampleValue, NAME, PARAMETERS, ParamDefault,
    ParamType, SEARCH_KINDS, SearchArgs,
};
use serde_json::{Value, json};

const QUERY: &str = "widget";
const GENERATION: &str = "g-search-1";
const INDEXED_AT: &str = "2026-01-05T00:00:00Z";
const NOW: &str = "2026-01-05T00:00:00.000Z";
/// 39 bytes, above the cursor module's 32-byte key floor.
const KEY: &str = "search-test-key-0123456789abcdef-0123456789";
/// The fixed key every captured golden was minted with (`rust/tests/parity/README.md`).
const PARITY_KEY: &str = "sangkar-parity-fixed-cursor-key-0123456789abcdef";
/// The canonical root the polyglot goldens were captured from; the cursor stamps it into the token.
const PARITY_REPO: &str = "/tmp/sangkar-parity-capture/polyglot/polyglot";
/// The generation every polyglot golden was captured in.
const PARITY_GENERATION: &str = "g-4b56df";
/// That generation's `published_at`, which the goldens report as `read_status.indexed_at`.
const PARITY_INDEXED_AT: &str = "2026-11-24T04:44:41.233Z";
/// The fixture's ids, best rank first: the order a tool that re-sorted would break.
const ROW_IDS: [i64; 5] = [11, 12, 13, 14, 15];
const NEXT_TOOLS: [&str; 2] = ["trace", "expand"];

// -------------------------------------------------------------------------------------------
// The fixture
// -------------------------------------------------------------------------------------------

/// One ranked row: the reader's hit plus the metrics and signature a lookup would return.
struct Row {
    hit: FtsHit,
    /// `None` means "the publish never measured this node", which the tool renders as 0/0.
    metrics: Option<NodeMetrics>,
    signature: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn row(
    id: i64,
    qn: &str,
    kind: &str,
    file: &str,
    lines: (i64, i64),
    score: f64,
    degree: Option<(i64, i64)>,
    signature: Option<&str>,
) -> Row {
    Row {
        hit: FtsHit {
            id,
            qn: qn.to_owned(),
            kind: kind.to_owned(),
            file: file.to_owned(),
            lines: LineRange {
                start: lines.0,
                end: lines.1,
            },
            score,
        },
        metrics: degree.map(|(in_degree, out_degree)| NodeMetrics {
            node_id: id,
            in_degree,
            out_degree,
            is_entry: false,
            tested_depth: None,
            reachable_count: None,
        }),
        signature: signature.map(str::to_owned),
    }
}

/// Five hits, best first. `src/core/widget.ts::Widget` is the exact name and the best score, which is
/// what "an exact name ranks first" means here: the reader put it first and the tool kept it.
fn widget_rows() -> Vec<Row> {
    vec![
        row(
            11,
            "src/core/widget.ts::Widget",
            "class",
            "src/core/widget.ts",
            (10, 40),
            -8.21,
            Some((2, 3)),
            Some("export class Widget"),
        ),
        row(
            12,
            "src/core/widget.ts::Widget.size",
            "method",
            "src/core/widget.ts",
            (18, 20),
            -6.4,
            Some((1, 0)),
            Some("size(): number"),
        ),
        row(
            13,
            "tests/widget.test.ts::widget",
            "function",
            "tests/widget.test.ts",
            (1, 12),
            -5.1,
            Some((0, 0)),
            None,
        ),
        row(
            14,
            "src/cli/widget.ts::runWidget",
            "function",
            "src/cli/widget.ts",
            (3, 9),
            -4.02,
            Some((9, 4)),
            Some("export function runWidget()"),
        ),
        row(
            15,
            "src-old/widget.ts::Widget",
            "class",
            "src-old/widget.ts",
            (1, 5),
            -3.3,
            None,
            Some("class Widget"),
        ),
    ]
}

fn indexed_file(id: i64, path: &str, generation: &str) -> IndexedFileRow {
    IndexedFileRow {
        id,
        path: path.to_owned(),
        language: "typescript".to_owned(),
        sha256: format!("{id}").repeat(64),
        size: 100 * id,
        line_count: 40,
        parse_status: "ok".to_owned(),
        parse_error_ranges: None,
        indexed_generation: generation.to_owned(),
    }
}

fn widget_files() -> Vec<IndexedFileRow> {
    vec![
        indexed_file(1, "src/core/widget.ts", GENERATION),
        indexed_file(2, "src/cli/widget.ts", GENERATION),
        indexed_file(3, "tests/widget.test.ts", GENERATION),
        indexed_file(4, "src-old/widget.ts", GENERATION),
    ]
}

/// A hand-written reader; every method present, and only `sangkar_store`'s read types in its
/// signatures.
///
/// [`Self::read_node_fts`] is a miniature FTS5 for the one property this tool must guarantee: the
/// *expected* match expression (`"widget"`, quotes included) returns the rows the query matched, while
/// any other expression returns [`Self::widened`] — which is what a raw `widget*` prefix query, a
/// column filter or an unterminated string would reach a real table with. A handler that stopped
/// quoting therefore sees the widened set (or a miss), never a silent pass.
struct FakeReader {
    rows: Vec<Row>,
    /// The wider set any expression but [`Self::exact`] matches.
    widened: Vec<FtsHit>,
    exact: String,
    files: Vec<IndexedFileRow>,
    generation: Option<String>,
    indexed_at: Option<String>,
    /// Every expression handed to `read_node_fts`, shared with the test so it can be read back after
    /// the reader has moved into a context.
    searches: Rc<RefCell<Vec<String>>>,
    /// Makes the FTS read fail, which a handler must answer with an error envelope.
    broken: bool,
}

impl FakeReader {
    /// The standard fixture: `"widget"` matches the five rows, anything else misses.
    fn widget() -> Self {
        Self {
            rows: widget_rows(),
            widened: Vec::new(),
            exact: match_query_of(QUERY),
            files: widget_files(),
            generation: Some(GENERATION.to_owned()),
            indexed_at: Some(INDEXED_AT.to_owned()),
            searches: Rc::new(RefCell::new(Vec::new())),
            broken: false,
        }
    }

    fn with_rows(rows: Vec<Row>) -> Self {
        Self {
            rows,
            ..Self::widget()
        }
    }

    fn unpublished() -> Self {
        Self {
            generation: None,
            indexed_at: None,
            ..Self::widget()
        }
    }

    /// Every file in scope was written by an earlier publish: the graph-side drift `stale` reports.
    fn stale() -> Self {
        let generation = "g-previous";
        Self {
            files: widget_files()
                .into_iter()
                .map(|file| IndexedFileRow {
                    indexed_generation: generation.to_owned(),
                    ..file
                })
                .collect(),
            ..Self::widget()
        }
    }

    fn broken() -> Self {
        Self {
            broken: true,
            ..Self::widget()
        }
    }

    /// The handle a test keeps so it can see what the handler asked the reader for.
    fn searches(&self) -> Rc<RefCell<Vec<String>>> {
        Rc::clone(&self.searches)
    }
}

impl GraphReader for FakeReader {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        let Some(entry) = self.rows.iter().find(|entry| entry.hit.qn == qn) else {
            return Ok(None);
        };
        let name = entry
            .hit
            .qn
            .rsplit_once("::")
            .map_or_else(|| entry.hit.qn.clone(), |(_, name)| name.to_owned());
        Ok(Some(NodeRow {
            id: entry.hit.id,
            file_id: 1,
            path: entry.hit.file.clone(),
            parent_id: None,
            kind: entry.hit.kind.clone(),
            subkind: None,
            name,
            qn: entry.hit.qn.clone(),
            start_line: entry.hit.lines.start,
            end_line: entry.hit.lines.end,
            signature: entry.signature.clone(),
            docstring: None,
            is_test: entry.hit.file.starts_with("tests/"),
            props: "{}".to_owned(),
        }))
    }

    fn find_by_name(
        &self,
        _name: &str,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        Ok(Vec::new())
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        Ok(Vec::new())
    }

    fn nodes_by_file(&self, _file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        Ok(Vec::new())
    }

    fn inbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(Vec::new())
    }

    fn outbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(Vec::new())
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        Ok(self
            .rows
            .iter()
            .find(|entry| entry.hit.id == node_id)
            .and_then(|entry| entry.metrics.clone()))
    }

    /// `rows_for_paths([])` is empty in the real reader too: a repo-wide scope is not a prefix.
    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        if prefixes.is_empty() {
            return Ok(PathCoverage {
                files: Vec::new(),
                not_indexed: Vec::new(),
                indexed: 0,
                skipped: 0,
            });
        }
        let files: Vec<IndexedFileRow> = self
            .files
            .iter()
            .filter(|file| {
                prefixes
                    .iter()
                    .any(|prefix| is_under_prefix(&file.path, prefix))
            })
            .cloned()
            .collect();
        Ok(PathCoverage {
            indexed: files.len(),
            skipped: 0,
            files,
            not_indexed: Vec::new(),
        })
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError> {
        Ok(Vec::new())
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        Ok(0)
    }

    fn unresolved_for_files(&self, _file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        Ok(Vec::new())
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        Ok(self.files.iter().find(|file| file.path == path).cloned())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        Ok(self.files.clone())
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        Ok(Vec::new())
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        Ok(TestCensus {
            nodes: 0,
            languages: Vec::new(),
        })
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: self.generation.clone(),
            generation_seq: Some(7),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("fake".to_owned()),
            project_root: Some("/tmp/fake".to_owned()),
            project_branch: None,
            project_remote: None,
            embed_dim: None,
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        Ok(self.indexed_at.as_ref().map(|published_at| GenerationRow {
            id: self.generation.clone().unwrap_or_default(),
            seq: 7,
            published_at: published_at.clone(),
            base_commit: None,
            worktree_dirty: false,
            files_indexed: 0,
            nodes: 0,
            edges: 0,
            duration_ms: 0,
            mode: "fast".to_owned(),
            status: "ok".to_owned(),
        }))
    }

    fn removed_symbols_for(&self, _since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        Ok(Vec::new())
    }

    fn entry_points(&self, _limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        Ok(Vec::new())
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        Ok(Vec::new())
    }

    fn test_gaps(&self, _ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        Ok(Vec::new())
    }

    fn read_node_fts(&self, match_query: &str, limit: usize) -> Result<Vec<FtsHit>, AppError> {
        self.searches.borrow_mut().push(match_query.to_owned());
        if self.broken {
            return Err(AppError::internal("the FTS mirror is not readable"));
        }
        let source: Vec<FtsHit> = if match_query == self.exact {
            self.rows.iter().map(|entry| entry.hit.clone()).collect()
        } else {
            self.widened.clone()
        };
        Ok(source.into_iter().take(limit).collect())
    }
}

/// The seam every tool task shares: a reader, a cursor, a logger, a clock, a config and the size
/// recorded for the file the reader opened.
fn context(reader: FakeReader) -> ToolContext {
    context_with(reader, Rc::new(test_cursor()))
}

fn context_with(reader: FakeReader, cursor: Rc<dyn CursorFacade>) -> ToolContext {
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        log: Logger::with_sink(NAME, |_, _, _| {}),
        now: || NOW.to_owned(),
        config: Rc::new(
            load_config(&[], &|_key: &str| None, Path::new("/tmp/fake"))
                .expect("the test configuration"),
        ),
        db_size_bytes: 0,
    }
}

fn test_cursor() -> Cursor {
    Cursor::new(KEY.as_bytes(), None).expect("a test cursor key")
}

fn results_of(envelope: &SearchEnvelope) -> &[SearchResult] {
    envelope
        .payload
        .as_ref()
        .map_or(&[], |payload| payload.results.as_slice())
}

fn ids_of(envelope: &SearchEnvelope) -> Vec<i64> {
    results_of(envelope)
        .iter()
        .map(|result| result.id)
        .collect()
}

/// A token for this call's shape, with the three parts a refusal is separated by.
fn token(generation: &str, tool: &str, args_hash: &str) -> String {
    test_cursor().sign(&CursorPayload {
        generation: generation.to_owned(),
        tool: tool.to_owned(),
        args_hash: args_hash.to_owned(),
        position: 1,
    })
}

fn with_cursor(args: SearchArgs, cursor: String) -> SearchArgs {
    SearchArgs {
        cursor: Some(cursor),
        ..args
    }
}

// -------------------------------------------------------------------------------------------
// The manifest
// -------------------------------------------------------------------------------------------

#[test]
fn the_manifest_carries_the_spec_description_verbatim_and_query_alone_required() {
    assert_eq!(NAME, "search");
    // The first sentence is the routing prompt; the rest is why an agent would pick this tool.
    assert!(DESCRIPTION.starts_with("Find symbols or text in the indexed code."));
    assert_eq!(
        DESCRIPTION,
        "Find symbols or text in the indexed code. Hybrid keyword (BM25) with optional semantic \
         rerank. Returns declarations with file:line and their in/out degree — not source. Use when \
         you know roughly what it is called but not where it lives."
    );
    assert_eq!(
        SEARCH_KINDS,
        ["function", "method", "class", "file", "decorator"]
    );

    let names: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(names, ["query", "kind", "path", "budget", "cursor", "repo"]);
    let required: Vec<&str> = PARAMETERS
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name)
        .collect();
    assert_eq!(required, ["query"], "at most one required argument");

    let query = &PARAMETERS[0];
    assert_eq!(query.ty, ParamType::Str);
    assert_eq!(query.default, None);
    assert_eq!(
        query.description,
        "Free text to find: an identifier, a substring or a phrase."
    );

    assert_eq!(PARAMETERS[1].ty, ParamType::Enum(&SEARCH_KINDS));
    assert_eq!(PARAMETERS[1].default, None);
    assert_eq!(PARAMETERS[2].name, "path");

    assert_eq!(DEFAULT_BUDGET_TOKENS, 2_000);
    assert_eq!(
        PARAMETERS[3].default,
        Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64))
    );
    assert_eq!(PARAMETERS[3].ty, ParamType::Integer);
    assert_eq!(PARAMETERS[3].min, None);

    assert_eq!(
        EXAMPLE,
        [
            ("query", ExampleValue::Text("publish")),
            ("kind", ExampleValue::Text("function")),
        ]
    );
    assert!(PARAMETERS.iter().all(|param| !param.description.is_empty()));
}

#[test]
fn the_declared_default_is_the_budget_a_call_that_asks_for_none_uses() {
    assert_eq!(
        SearchArgs::new(QUERY).budget_tokens(),
        DEFAULT_BUDGET_TOKENS
    );
    assert_eq!(
        SearchArgs {
            budget: Some(500),
            ..SearchArgs::new(QUERY)
        }
        .budget_tokens(),
        500
    );
    // A non-positive budget is not upgraded to the default: it is a ceiling nothing fits, so the call
    // is refused rather than answered at ten times the allowance the caller asked for.
    assert_eq!(
        SearchArgs {
            budget: Some(-1),
            ..SearchArgs::new(QUERY)
        }
        .budget_tokens(),
        0
    );
}

// -------------------------------------------------------------------------------------------
// Ranking, the payload's keys and the degree join
// -------------------------------------------------------------------------------------------

#[test]
fn a_hit_keeps_the_readers_order_and_carries_the_documented_degrees() {
    let envelope = handle(&context(FakeReader::widget()), &SearchArgs::new(QUERY));
    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(ids_of(&envelope), ROW_IDS);
    assert_eq!(results_of(&envelope)[0].qn, "src/core/widget.ts::Widget");
    // The node with the highest degree (9/4) stays fourth because its score is worse: score first.
    assert_eq!(results_of(&envelope)[3].id, 14);
    assert_eq!(
        results_of(&envelope)[3].degree,
        SearchDegree { in_: 9, out: 4 }
    );
    assert_eq!(results_of(&envelope)[0].degree.in_, 2);
    assert_eq!(results_of(&envelope)[0].degree.out, 3);
    // An unmeasured node still reports the two documented integers.
    assert_eq!(results_of(&envelope)[4].degree.in_, 0);
    assert_eq!(results_of(&envelope)[4].degree.out, 0);

    assert_eq!(
        envelope.summary,
        "Found 5 declarations for 'widget', showing 5 of 5."
    );
    assert_eq!(envelope.totals.as_map()["results_total"], json!(5));
    assert_eq!(envelope.next_tools, NEXT_TOOLS.map(str::to_owned).to_vec());
    assert!(!envelope.truncated);
    assert_eq!(envelope.next, None);
}

#[test]
fn reports_bm25_mode_and_exactly_the_documented_payload_keys_never_source() {
    let envelope = handle(&context(FakeReader::widget()), &SearchArgs::new(QUERY));
    let payload = envelope.payload.as_ref().expect("a payload");
    assert_eq!(payload.mode, "bm25");

    // Wire order, not just the key set: serde's declaration order is the contract.
    let payload_value = serde_json::to_value(payload).expect("the payload is JSON");
    let keys: Vec<&str> = payload_value
        .as_object()
        .expect("a payload object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["mode", "results"]);

    let first = serde_json::to_value(&payload.results[0]).expect("a result");
    let result_keys: Vec<&str> = first
        .as_object()
        .expect("a result object")
        .keys()
        .map(String::as_str)
        .collect();
    // `signature` is last because `handler.ts` spreads it onto the base object after `score`.
    assert_eq!(
        result_keys,
        [
            "id",
            "qn",
            "kind",
            "file",
            "lines",
            "degree",
            "score",
            "signature"
        ]
    );
    assert_eq!(first["file"], json!("src/core/widget.ts"));
    assert_eq!(first["lines"], json!([10, 40]));
    assert_eq!(first["degree"], json!({"in": 2, "out": 3}));
    assert_eq!(first["score"], json!(-8.21));

    // A row without a signature omits the key instead of writing `null`.
    let unsigned = serde_json::to_value(&payload.results[2]).expect("a result");
    assert_eq!(unsigned["id"], json!(13));
    assert!(
        !unsigned
            .as_object()
            .expect("an object")
            .contains_key("signature")
    );
}

#[test]
fn two_identical_calls_return_an_identical_payload_and_summary() {
    let ctx = context(FakeReader::widget());
    let first = handle(&ctx, &SearchArgs::new(QUERY));
    let second = handle(&ctx, &SearchArgs::new(QUERY));
    assert_eq!(first.payload, second.payload);
    assert_eq!(first.summary, second.summary);
}

#[test]
fn a_score_that_is_an_integer_renders_without_a_trailing_dot_zero() {
    // `JSON.stringify(0)` is `0`, `serde_json`'s writer for `0.0f64` is `0.0`: the difference is
    // invisible to a `Value` comparison and visible on the wire.
    let rows = vec![row(
        21,
        "src/a.ts::a",
        "function",
        "src/a.ts",
        (1, 2),
        0.0,
        Some((0, 0)),
        None,
    )];
    let envelope = handle(
        &context(FakeReader::with_rows(rows)),
        &SearchArgs::new(QUERY),
    );
    let text = serde_json::to_string(&envelope).expect("an envelope");
    assert!(
        text.contains("\"score\":0}"),
        "an integral score is written as an integer: {text}"
    );
    assert!(!text.contains("\"score\":0.0"));
}

// -------------------------------------------------------------------------------------------
// The kind filter
// -------------------------------------------------------------------------------------------

#[test]
fn the_kind_filter_keeps_the_readers_order_and_drops_every_other_kind() {
    let args = SearchArgs {
        kind: Some("class".to_owned()),
        ..SearchArgs::new(QUERY)
    };
    let envelope = handle(&context(FakeReader::widget()), &args);
    assert_eq!(ids_of(&envelope), [11, 15]);
    assert!(
        results_of(&envelope)
            .iter()
            .all(|result| result.kind == "class")
    );
    assert_eq!(envelope.totals.as_map()["results_total"], json!(2));
}

// -------------------------------------------------------------------------------------------
// The path filter
// -------------------------------------------------------------------------------------------

#[test]
fn a_path_prefix_excludes_everything_outside_it_including_a_sibling_that_shares_the_text() {
    let args = SearchArgs {
        path: Some("src".to_owned()),
        ..SearchArgs::new(QUERY)
    };
    let envelope = handle(&context(FakeReader::widget()), &args);
    assert_eq!(ids_of(&envelope), [11, 12, 14]);
    assert!(
        results_of(&envelope)
            .iter()
            .all(|result| is_under_prefix(&result.file, "src"))
    );
    // `src-old/widget.ts` shares the first three characters and must still be excluded.
    assert!(
        !results_of(&envelope)
            .iter()
            .any(|result| result.qn.contains("src-old"))
    );
    assert!(
        !results_of(&envelope)
            .iter()
            .any(|result| result.file.starts_with("tests/"))
    );
    assert_eq!(envelope.totals.as_map()["results_total"], json!(3));
}

#[test]
fn a_fully_indexed_path_scope_may_claim_eq_while_a_repo_wide_scope_may_not() {
    let scoped = handle(
        &context(FakeReader::widget()),
        &SearchArgs {
            path: Some("src".to_owned()),
            ..SearchArgs::new(QUERY)
        },
    );
    assert!(scoped.coverage.complete);
    assert_eq!(scoped.coverage.indexed, 2);
    assert_eq!(
        scoped.totals.as_map()["total_relation"],
        json!("eq"),
        "a complete, unresolved-free, fresh scope is exact"
    );
    // The graph-side check ran over the two files in scope, and it says so.
    assert!(!scoped.read_status.stale);
    assert_eq!(scoped.read_status.stale_checked_at.as_deref(), Some(NOW));
    assert_eq!(scoped.read_status.stale_reason, None);

    let repo_wide = handle(&context(FakeReader::widget()), &SearchArgs::new(QUERY));
    assert!(!repo_wide.coverage.complete);
    assert_eq!(repo_wide.totals.as_map()["total_relation"], json!("gte"));
    // No prefix means no file was compared, so no freshness check may be claimed either.
    assert_eq!(repo_wide.read_status.stale_checked_at, None);
}

#[test]
fn a_file_written_by_an_earlier_generation_makes_the_answer_stale_without_failing() {
    let envelope = handle(
        &context(FakeReader::stale()),
        &SearchArgs {
            path: Some("src".to_owned()),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(envelope.read_status.generation, GENERATION);
    assert!(envelope.read_status.stale);
    let reason = envelope
        .read_status
        .stale_reason
        .as_deref()
        .expect("a stale answer says why");
    assert!(reason.contains("2 of 2 file(s) in scope"), "{reason}");
    assert_eq!(envelope.read_status.stale_checked_at.as_deref(), Some(NOW));
    // A stale answer may never claim an exact total, and the results are still served.
    assert_eq!(envelope.totals.as_map()["total_relation"], json!("gte"));
    assert!(!results_of(&envelope).is_empty());
}

#[test]
fn a_filter_that_empties_a_matching_query_says_which_one_emptied_it() {
    let by_kind = handle(
        &context(FakeReader::widget()),
        &SearchArgs {
            kind: Some("decorator".to_owned()),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(by_kind.status, Status::NotFound);
    assert!(results_of(&by_kind).is_empty());
    assert!(
        by_kind.summary.contains("though 5 matched the query alone"),
        "{}",
        by_kind.summary
    );

    let by_path = handle(
        &context(FakeReader::widget()),
        &SearchArgs {
            path: Some("docs".to_owned()),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(by_path.status, Status::NotFound);
    assert!(by_path.summary.contains("No declarations matched 'widget'"));
}

#[test]
fn a_query_that_matches_nothing_is_not_found_never_a_bare_zero() {
    let reader = FakeReader {
        rows: Vec::new(),
        exact: match_query_of("nomatch"),
        ..FakeReader::widget()
    };
    let envelope = handle(&context(reader), &SearchArgs::new("nomatch"));
    assert_eq!(envelope.status, Status::NotFound);
    assert!(results_of(&envelope).is_empty());
    assert_eq!(envelope.totals.as_map()["results_total"], json!(0));
    assert!(envelope.summary.contains("nomatch"));
    let payload = serde_json::to_value(envelope.payload.expect("a payload")).expect("JSON");
    assert_eq!(payload, json!({"mode": "bm25", "results": []}));
}

#[test]
fn an_empty_query_is_answered_without_asking_the_reader_for_a_match() {
    let reader = FakeReader::widget();
    let searches = reader.searches();
    let envelope = handle(&context(reader), &SearchArgs::new("   "));
    assert_eq!(envelope.status, Status::NotFound);
    assert!(results_of(&envelope).is_empty());
    assert!(
        envelope.summary.contains("empty query"),
        "{}",
        envelope.summary
    );
    assert!(
        searches.borrow().is_empty(),
        "an empty query is answered without a MATCH"
    );
}

// -------------------------------------------------------------------------------------------
// The FTS5 match expression
// -------------------------------------------------------------------------------------------

#[test]
fn the_match_expression_is_the_callers_text_verbatim() {
    // Parity, not sanitising: `src/tools/search/handler.ts:301` binds the trimmed query straight to
    // `MATCH`, so FTS5 reads `widget*` as a prefix query and `a OR b` as a disjunction. Quoting was
    // tried and rejected by the maintainer, because it turned `widget*` into the literal token
    // `widget*` and made the same call return different rows in the two binaries.
    assert_eq!(match_query_of("widget"), "widget");
    assert_eq!(match_query_of("  widget  "), "  widget  ");
    assert_eq!(match_query_of("handle request"), "handle request");

    // Every FTS5 operator reaches SQLite as written, which is what makes the answers the same.
    for raw in [
        "widget*",
        "foo:bar",
        "a-b",
        "(open",
        "close)",
        "*",
        ":",
        "-",
        "AND",
        "NEAR(a b)",
        r#"a"b"#,
        "\"",
        r#"a*"b:c"#,
        "héllo-世界",
    ] {
        assert_eq!(match_query_of(raw), raw, "the expression for {raw:?}");
    }

    // Whitespace is not trimmed here; `handle` trims once, before the empty check and the read, so a
    // whitespace-only query is answered as an empty search and never reaches `MATCH`.
    assert_eq!(match_query_of("   "), "   ");
}

#[test]
fn a_prefix_query_widens_the_match_set_exactly_as_the_typescript_does() {
    // The behaviour the sanitiser used to prevent, restored on purpose: `widget*` is a prefix query,
    // so it answers with every row the statement returns rather than only the exact token.
    let widened: Vec<FtsHit> = widget_rows()
        .iter()
        .map(|entry| entry.hit.clone())
        .collect();
    let reader = FakeReader {
        widened,
        exact: match_query_of("widget*"),
        ..FakeReader::widget()
    };
    let searches = reader.searches();
    let envelope = handle(&context(reader), &SearchArgs::new("widget*"));
    assert_eq!(envelope.status, Status::Ok, "{}", envelope.summary);
    assert!(
        ids_of(&envelope).len() > 1,
        "a prefix query must reach more than the exact token"
    );
    assert_eq!(*searches.borrow(), ["widget*"]);
}

#[test]
fn a_query_sqlite_rejects_becomes_the_same_internal_envelope_the_typescript_returns() {
    // A malformed expression is not intercepted: it reaches the reader, fails there, and surfaces as
    // the `internal` envelope `handler.ts` produces — never a panic, never a raw SQL error escaping.
    let reader = FakeReader {
        broken: true,
        ..FakeReader::widget()
    };
    let envelope = handle(&context(reader), &SearchArgs::new(r#"a*:"b""#));
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.expect("an error payload");
    assert_eq!(error.code.as_str(), "internal");
    assert!(
        error.message.contains("not readable"),
        "the reader's failure is carried through, not swallowed: {}",
        error.message
    );
}

// -------------------------------------------------------------------------------------------
// Pagination
// -------------------------------------------------------------------------------------------

/// The tightest budget that still truncates this fixture's answer, found rather than guessed: the
/// whole answer first, then down one token at a time. The fixture's envelope floor sits just under
/// its payload, so a hard-coded number would either not cut or land below the floor — where `search`
/// refuses instead of paging, because a page that cannot be cut to fit is not a page.
fn tightest_cutting_budget(ctx: &ToolContext) -> i64 {
    let whole = handle(ctx, &SearchArgs::new(QUERY));
    let bytes = serde_json::to_string(&whole)
        .expect("the envelope is JSON")
        .len();
    let whole_tokens = i64::try_from(bytes.div_ceil(BYTES_PER_TOKEN as usize)).expect("a length");
    for budget in (2..whole_tokens).rev() {
        let envelope = handle(
            ctx,
            &SearchArgs {
                budget: Some(budget),
                ..SearchArgs::new(QUERY)
            },
        );
        if envelope.status == Status::Ok && envelope.next.is_some() {
            return budget;
        }
    }
    panic!("no budget truncates this fixture's answer");
}

#[test]
fn a_truncated_pages_next_cursor_returns_the_following_page_with_no_duplicates() {
    let ctx = context(FakeReader::widget());
    let budget = Some(tightest_cutting_budget(&ctx));
    let mut pages: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut calls = 0_usize;

    loop {
        let args = SearchArgs {
            budget,
            cursor: cursor.clone(),
            ..SearchArgs::new(QUERY)
        };
        let envelope = handle(&ctx, &args);
        calls += 1;
        assert_eq!(envelope.status, Status::Ok);
        pages.extend(ids_of(&envelope));

        let Some(next) = envelope.next.clone() else {
            assert!(!envelope.truncated);
            break;
        };
        // A cut page always says why and always carries a continuation.
        assert!(envelope.truncated);
        assert_eq!(envelope.truncation_reason, Some(TruncationReason::Budget));
        assert_eq!(envelope.totals.as_map()["total_relation"], json!("gte"));
        assert!(calls <= ROW_IDS.len(), "every page advances");
        cursor = Some(next);
    }

    assert!(calls > 1, "the budget cuts the answer");
    assert_eq!(pages, ROW_IDS);
}

#[test]
fn a_resumed_page_may_use_a_different_budget_the_list_not_the_page_size_is_the_call() {
    let ctx = context(FakeReader::widget());
    let tight = Some(tightest_cutting_budget(&ctx));
    let first = handle(
        &ctx,
        &SearchArgs {
            budget: tight,
            ..SearchArgs::new(QUERY)
        },
    );
    let next = first.next.clone().expect("a continuation");
    let first_ids = ids_of(&first);
    assert!(!first_ids.is_empty());
    assert_eq!(first_ids, ROW_IDS[..first_ids.len()].to_vec());

    let resumed = handle(
        &ctx,
        &SearchArgs {
            budget: Some(DEFAULT_BUDGET_TOKENS as i64),
            cursor: Some(next),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(resumed.status, Status::Ok);
    let resumed_ids = ids_of(&resumed);
    assert!(!resumed_ids.is_empty());
    // The resumed page starts exactly where the tight one stopped, whatever budget either used.
    assert_eq!(resumed_ids[0], ROW_IDS[first_ids.len()]);
    assert!(!resumed_ids.iter().any(|id| first_ids.contains(id)));
}

// -------------------------------------------------------------------------------------------
// Cursors
// -------------------------------------------------------------------------------------------

#[test]
fn a_cursor_for_other_arguments_is_refused_as_cursor_mismatch_and_returns_no_page() {
    let reader = FakeReader::widget();
    let searches = reader.searches();
    let args = with_cursor(
        SearchArgs::new(QUERY),
        token(GENERATION, NAME, "not-the-hash-of-any-call"),
    );
    let envelope = handle(&context(reader), &args);
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "cursor_mismatch");
    assert_eq!(
        error.hint.as_deref(),
        Some("re-run the original call with the same tool and arguments")
    );
    assert_eq!(envelope.payload, None);
    assert_eq!(envelope.next, None);
    assert!(
        searches.borrow().is_empty(),
        "the cursor is verified before the search runs"
    );
}

#[test]
fn a_cursor_minted_in_another_generation_is_refused_as_cursor_expired() {
    let args = with_cursor(
        SearchArgs::new(QUERY),
        token("g-old", NAME, "not-the-hash-of-any-call"),
    );
    let envelope = handle(&context(FakeReader::widget()), &args);
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "cursor_expired");
    assert_eq!(error.hint.as_deref(), Some("re-run the original call"));
}

#[test]
fn a_cursor_for_another_tool_and_a_malformed_cursor_are_refused_with_the_cursors_own_codes() {
    let other_tool = handle(
        &context(FakeReader::widget()),
        &with_cursor(
            SearchArgs::new(QUERY),
            token(GENERATION, "trace", "not-the-hash-of-any-call"),
        ),
    );
    assert_eq!(
        other_tool.error.as_ref().expect("an error").code.as_str(),
        "cursor_mismatch"
    );

    let malformed = handle(
        &context(FakeReader::widget()),
        &with_cursor(SearchArgs::new(QUERY), "not-a-cursor".to_owned()),
    );
    assert_eq!(malformed.status, Status::Error);
    let error = malformed.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "cursor_invalid");
    assert_eq!(
        error.hint.as_deref(),
        Some("re-run the original call to mint a fresh cursor")
    );
}

#[test]
fn a_cursor_this_call_minted_resumes_at_its_own_position() {
    // The page arithmetic and the digest are one mechanism: a token signed over this call's own
    // `args_hash` is accepted, and its position is the index into the flattened list.
    let ctx = context(FakeReader::widget());
    let signed = test_cursor().sign(&CursorPayload {
        generation: GENERATION.to_owned(),
        tool: NAME.to_owned(),
        args_hash: args_hash_of(QUERY, None, ""),
        position: 3,
    });
    let envelope = handle(&ctx, &with_cursor(SearchArgs::new(QUERY), signed));
    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(ids_of(&envelope), [14, 15]);
    assert!(
        envelope.summary.ends_with("showing 4-5 of 5."),
        "{}",
        envelope.summary
    );
}

// -------------------------------------------------------------------------------------------
// Refusals
// -------------------------------------------------------------------------------------------

#[test]
fn an_unpublished_index_answers_bootstrapping_with_a_retry_interval() {
    let envelope = handle(&context(FakeReader::unpublished()), &SearchArgs::new(QUERY));
    assert_eq!(envelope.status, Status::Bootstrapping);
    assert_eq!(envelope.payload, None);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "bootstrapping");
    assert_eq!(error.retry_after_ms, Some(1_000));
    assert_eq!(
        error.hint.as_deref(),
        Some("re-run once the indexer has published; `status` reports the progress")
    );
    assert_eq!(envelope.read_status.generation, "");
    assert_eq!(envelope.read_status.indexed_at, NOW);
    assert!(envelope.read_status.stale);
}

#[test]
fn a_budget_that_fits_no_result_refuses_instead_of_paging_forever() {
    let envelope = handle(
        &context(FakeReader::widget()),
        &SearchArgs {
            budget: Some(1),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "budget_too_large");
    assert_eq!(envelope.next, None);
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("raise the budget")),
        "{:?}",
        error.hint
    );
}

#[test]
fn a_budget_above_the_ceiling_is_reported_not_thrown() {
    let envelope = handle(
        &context(FakeReader::widget()),
        &SearchArgs {
            budget: Some(40_000),
            ..SearchArgs::new(QUERY)
        },
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "budget_too_large");
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("32000")),
        "{:?}",
        error.hint
    );
}

#[test]
fn a_failed_read_is_an_error_envelope_never_not_found() {
    let envelope = handle(&context(FakeReader::broken()), &SearchArgs::new(QUERY));
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error envelope");
    assert_eq!(error.code.as_str(), "internal");
    assert_eq!(error.message, "the FTS mirror is not readable");
    assert_eq!(envelope.payload, None);
    // A failed read claims nothing: not freshness, and certainly not an absence.
    assert!(envelope.read_status.stale);
    assert_eq!(
        envelope.read_status.stale_reason.as_deref(),
        Some("the read that would have answered it failed")
    );
    assert!(envelope.summary.contains("could not be read"));
}

#[test]
fn a_query_with_more_matches_than_the_reader_bound_says_so_in_the_summary() {
    // The bound is a property of the reader's statement, not a page size: a query with more matches is
    // the one case it — not the budget — ends the list, and the summary has to say which one did.
    let rows: Vec<Row> = (0..(READER_BOUND + 20))
        .map(|index| {
            let id = i64::try_from(index).expect("a small index");
            row(
                id,
                &format!("src/f{id}.ts::f{id}"),
                "function",
                &format!("src/f{id}.ts"),
                (1, 2),
                -(index as f64) / 100.0,
                Some((0, 0)),
                None,
            )
        })
        .collect();
    let envelope = handle(
        &context(FakeReader::with_rows(rows)),
        &SearchArgs::new(QUERY),
    );
    assert_eq!(
        envelope.totals.as_map()["results_total"],
        json!(READER_BOUND as u64)
    );
    assert!(
        envelope.summary.contains("among the 200 best matches"),
        "{}",
        envelope.summary
    );
    // The budget still decides how many of those 200 are shown, and the page still advances.
    assert!(envelope.truncated);
    assert_eq!(envelope.truncation_reason, Some(TruncationReason::Budget));
    assert!(results_of(&envelope).len() < READER_BOUND);
    assert!(envelope.next.is_some());
}

// -------------------------------------------------------------------------------------------
// The argument digest
// -------------------------------------------------------------------------------------------

#[test]
fn the_args_hash_is_bare_hex_over_the_compacted_triple() {
    // The digest inside the captured `search.page.json` token, over `["request",null,""]`: bare
    // lowercase hex, no `sha256:` prefix. That difference from the other tools is the contract.
    let from_golden = args_hash_of("request", None, "");
    assert_eq!(
        from_golden, "d4b4ab8fa947edc7ce68467e97d780a7c437f3ddd4a5ccb6ad12384c76105c4c",
        "the digest the TypeScript minted into the golden token"
    );
    assert_eq!(from_golden.len(), 64);
    assert!(!from_golden.contains(':'), "bare hex, no algorithm prefix");
    assert!(
        from_golden
            .chars()
            .all(|character| character.is_ascii_digit() || ('a'..='f').contains(&character))
    );

    // The three arguments that decide which rows are in the list, and nothing else: a different kind
    // or a different prefix is a different call.
    assert_ne!(from_golden, args_hash_of("request", Some("function"), ""));
    assert_ne!(from_golden, args_hash_of("request", None, "src"));
    assert_eq!(
        args_hash_of("request", Some("function"), "src"),
        "6d34b9c375fec4aba79da719a99f60754fbada75f3b29d8778cfec81a4e59a24"
    );
    assert_eq!(
        args_hash_of("widget", None, ""),
        "b2e904f4012a042d2a84183d06a459bf75ee5d45541d06f3dcbbd54a0e43e671"
    );
    // `null` is JSON's absence, not the empty string: the two digests differ.
    assert_ne!(
        args_hash_of("request", None, ""),
        args_hash_of("request", Some(""), "")
    );
}

// -------------------------------------------------------------------------------------------
// The captured goldens
// -------------------------------------------------------------------------------------------

fn read_golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/polyglot")
        .join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("a golden document")
}

/// The polyglot capture's seven hits, rebuilt from the golden's own stable facts: ids, qualified
/// names, kinds, files, line ranges, degrees, scores and signatures.
///
/// The capture ran against a fixture tree this suite does not index, so the rows are reproduced from
/// the answer that recorded them; what that makes comparable is the whole of the tool's own policy —
/// the order it preserves, the page arithmetic, the budget cut and the minted cursor.
fn polyglot_reader(rows: Vec<Row>) -> FakeReader {
    FakeReader {
        rows,
        widened: Vec::new(),
        exact: match_query_of("request"),
        files: Vec::new(),
        generation: Some(PARITY_GENERATION.to_owned()),
        indexed_at: Some(PARITY_INDEXED_AT.to_owned()),
        searches: Rc::new(RefCell::new(Vec::new())),
        broken: false,
    }
}

fn polyglot_rows() -> Vec<Row> {
    vec![
        row(
            53,
            "src/service.go::respond",
            "function",
            "src/service.go",
            (11, 13),
            -2.169_860_795_147_13,
            Some((2, 0)),
            Some("func respond(request string) string"),
        ),
        row(
            16,
            "src/Service.java::Service.respond",
            "method",
            "src/Service.java",
            (8, 10),
            -2.078_764_598_499_553_5,
            Some((2, 0)),
            Some("protected String respond(String request) {"),
        ),
        row(
            15,
            "src/Service.java::Service.handle",
            "method",
            "src/Service.java",
            (4, 6),
            -2.078_764_598_499_553_5,
            Some((1, 1)),
            Some("public String handle(String request) {"),
        ),
        row(
            10,
            "src/Controller.php::Controller.respond",
            "method",
            "src/Controller.php",
            (11, 14),
            -1.995_009_113_837_008_7,
            Some((2, 0)),
            Some("protected function respond(string $request): string"),
        ),
        row(
            52,
            "src/service.go::Handle",
            "function",
            "src/service.go",
            (7, 9),
            -1.995_009_113_837_008_7,
            Some((2, 1)),
            Some("func (s Server) Handle(request string) string"),
        ),
        row(
            9,
            "src/Controller.php::Controller.handle",
            "method",
            "src/Controller.php",
            (6, 9),
            -1.995_009_113_837_008_7,
            Some((1, 1)),
            Some("public function handle(string $request): string"),
        ),
        row(
            12,
            "src/Controller.php::ApiController.handle",
            "method",
            "src/Controller.php",
            (19, 22),
            -1.995_009_113_837_008_7,
            Some((1, 0)),
            Some("public function handle(string $request): string"),
        ),
    ]
}

/// The captured archive's canonical root and the capture's fixed key: a token is reproducible given
/// its generation, and the generation is inside the golden (`rust/tests/parity/README.md`).
fn parity_context(reader: FakeReader) -> ToolContext {
    let cursor: Rc<dyn CursorFacade> = Rc::new(
        Cursor::new(PARITY_KEY.as_bytes().to_vec(), Some(PARITY_REPO.to_owned()))
            .expect("bind the parity cursor key"),
    );
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        log: Logger::with_sink(NAME, |_, _, _| {}),
        now: || "2026-11-24T04:44:42.668Z".to_owned(),
        config: Rc::new(
            load_config(&[], &|_key: &str| None, Path::new(PARITY_REPO))
                .expect("resolve the captured archive's config"),
        ),
        db_size_bytes: 0,
    }
}

/// The served envelope minus the field the composition root owns.
fn served(envelope: &SearchEnvelope) -> Value {
    let mut value = serde_json::to_value(envelope).expect("the envelope is JSON");
    value["read_status"]
        .as_object_mut()
        .expect("a read_status object")
        .remove("decisive");
    value
}

/// A golden's envelope, with the root-owned field removed on the same terms.
fn wanted(golden: &Value) -> Value {
    let mut value: Value = serde_json::from_str(
        golden["stdout"]
            .as_str()
            .expect("the golden's stdout is a string"),
    )
    .expect("the golden's stdout is an envelope");
    value["read_status"]
        .as_object_mut()
        .expect("a read_status object")
        .remove("decisive");
    value
}

/// Byte comparison, not `Value` equality: a `serde_json::Map` compares as a map, so only the bytes can
/// catch a key that moved or a number that gained a `.0`.
fn same_bytes(envelope: &SearchEnvelope, expected: &Value) {
    assert_eq!(
        serde_json::to_string(&served(envelope)).expect("the served envelope is JSON"),
        serde_json::to_string(expected).expect("the golden's envelope is JSON"),
        "the served envelope is the golden's, minus the root-owned `read_status.decisive`"
    );
}

#[test]
fn the_polyglot_golden_matches_byte_for_byte_including_the_payloads_key_order() {
    let golden = read_golden("search.json");
    assert_eq!(golden["args"], json!(["--query", "request"]));
    assert_eq!(golden["exit_code"], json!(0));
    let expected = wanted(&golden);

    let context = parity_context(polyglot_reader(polyglot_rows()));
    let envelope = handle(&context, &SearchArgs::new("request"));

    same_bytes(&envelope, &expected);
    assert_eq!(
        envelope.summary,
        "Found 7 declarations for 'request', showing 7 of 7."
    );
    assert_eq!(envelope.next, None);
    assert_eq!(envelope.totals.as_map()["results_total"], json!(7));
    assert!(
        !envelope.coverage.complete,
        "a repo-wide scope is not complete"
    );
    assert_eq!(envelope.read_status.stale_checked_at, None);
}

#[test]
fn the_polyglot_page_golden_mints_the_same_next_token_byte_for_byte() {
    let golden = read_golden("search.page.json");
    assert_eq!(
        golden["args"],
        json!(["--query", "request", "--budget", "300"])
    );
    let expected = wanted(&golden);

    let context = parity_context(polyglot_reader(polyglot_rows()));
    let args = SearchArgs {
        budget: Some(300),
        ..SearchArgs::new("request")
    };
    let envelope = handle(&context, &args);

    // The Done-when: the page-2 token the Rust binary mints is byte-identical to the TypeScript's,
    // which pins the generation, the repo, the argument digest and the page position at once.
    assert_eq!(
        envelope.next.as_deref(),
        expected["next"].as_str(),
        "the minted cursor is the golden's `next`"
    );
    assert!(envelope.truncated);
    assert_eq!(envelope.truncation_reason, Some(TruncationReason::Budget));
    assert_eq!(
        envelope.summary,
        "Found 7 declarations for 'request', showing 1-1 of 7."
    );
    same_bytes(&envelope, &expected);

    // The token verifies under the same key and the same call, and it addresses the second hit: the
    // two implementations agree on the position, not just on the bytes.
    let verified = context.cursor.verify(
        expected["next"].as_str().expect("the golden's next"),
        &CursorExpectation {
            generation: PARITY_GENERATION.to_owned(),
            tool: NAME.to_owned(),
            args_hash: args_hash_of("request", None, ""),
        },
    );
    assert_eq!(verified.position(), Some(1));

    // The boundary is the flattened, already-filtered list: the first of the seven is served and the
    // token continues at the second, so nothing is served twice and nothing is skipped.
    assert_eq!(ids_of(&envelope), [53]);
    let second = handle(
        &context,
        &SearchArgs {
            budget: Some(300),
            cursor: envelope.next.clone(),
            ..SearchArgs::new("request")
        },
    );
    assert_eq!(ids_of(&second), [16]);
}

#[test]
fn the_empty_fixtures_answer_to_the_same_query_is_not_found_and_never_an_error() {
    // `rust/tests/golden/broken/search.json`: the same call against the fixture whose index found
    // nothing. An empty result is `not_found` with an empty list, not a SQL error and not a bare
    // absence claim.
    let golden = read_golden("../broken/search.json");
    assert_eq!(golden["args"], json!(["--query", "request"]));
    let expected = wanted(&golden);
    let reader = FakeReader {
        rows: Vec::new(),
        files: Vec::new(),
        generation: Some(
            expected["read_status"]["generation"]
                .as_str()
                .expect("a generation")
                .to_owned(),
        ),
        indexed_at: Some(
            expected["read_status"]["indexed_at"]
                .as_str()
                .expect("a timestamp")
                .to_owned(),
        ),
        ..FakeReader::widget()
    };

    let envelope = handle(&context(reader), &SearchArgs::new("request"));
    assert_eq!(envelope.status, Status::NotFound);
    same_bytes(&envelope, &expected);
    assert_eq!(
        envelope.summary,
        "Found 0 declarations for 'request'; nothing further from position 0."
    );
}
