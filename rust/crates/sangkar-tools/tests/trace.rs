//! `trace` over a hand-written `GraphReader`: no database, no filesystem, no network.
//!
//! Ported from `tests/tools-trace.test.ts`. What is asserted is the tool's own policy — the fixed
//! group order, `tier` hoisted onto the group, `hop` per item, ambiguity that never becomes an
//! answer, near misses instead of a bare zero, the census that explains a flat count, and a cursor
//! page boundary that loses and repeats nothing — plus one golden comparison against the captured
//! TypeScript answer (`rust/tests/golden/polyglot/trace.page.json`), which is the only place this
//! suite claims byte parity.
//!
//! The fixture is deliberately dense enough to truncate at a small budget: one seed with two inbound
//! relations and twelve outbound ones, a second hop, an external node, a test node and one unbound
//! call site, so every claim the payload makes has a row behind it.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use sangkar_core::config::load_config;
use sangkar_core::cursor::{Cursor, CursorExpectation};
use sangkar_core::envelope::{Envelope, Status, TestCensus};
use sangkar_core::errors::AppError;
use sangkar_domain::resolve::CANDIDATE_CAP;
use sangkar_domain::unresolved::UNRESOLVED_BUCKETS;
use sangkar_store::read::context::{CursorFacade, Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, UnresolvedRef};
use sangkar_store::read::edges::{EdgeRow, NeighbourEdge};
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};
use serde_json::Value;

use sangkar_tools::trace::handler::{args_hash_of, handle};
use sangkar_tools::trace::manifest::{
    DEFAULT_BUDGET_TOKENS, DEFAULT_DEPTH, DESCRIPTION, DIRECTIONS, MAX_DEPTH, MIN_DEPTH, NAME,
    NODE_KINDS, PARAMETERS, Param, ParamDefault, ParamType, TraceArgs,
};

const GENERATION: &str = "g-1";
/// 39 bytes, above the cursor module's 32-byte key floor.
const KEY: &str = "trace-test-cursor-key-0123456789abcdef";
/// The fixed key every captured golden was minted with (`rust/tests/parity/README.md`).
const PARITY_KEY: &str = "sangkar-parity-fixed-cursor-key-0123456789abcdef";
/// The canonical root the polyglot goldens were captured from; the cursor stamps it into the token.
const PARITY_REPO: &str = "/tmp/sangkar-parity-capture/polyglot/polyglot";
const NOW: &str = "2026-01-04T00:00:00.000Z";
const INDEXED_AT: &str = "2026-01-03T00:00:00Z";

/// 600 tokens is smaller than this fixture's whole payload; the first page cannot hold it all.
const SMALL_BUDGET: u64 = 600;

fn node(id: i64, qn: &str, kind: &str, name: &str) -> NodeRow {
    let path = qn
        .split_once("::")
        .map_or_else(|| qn.to_owned(), |(path, _)| path.to_owned());
    NodeRow {
        id,
        file_id: id,
        path,
        parent_id: None,
        kind: kind.to_owned(),
        subkind: None,
        name: name.to_owned(),
        qn: qn.to_owned(),
        start_line: 1,
        end_line: 2,
        signature: None,
        docstring: None,
        is_test: kind == "test",
        props: "{}".to_owned(),
    }
}

fn edge(id: i64, src: i64, dst: i64, kind: &str, tier: &str, line: i64) -> EdgeRow {
    EdgeRow {
        id,
        src_id: src,
        dst_id: dst,
        kind: kind.to_owned(),
        tier: tier.to_owned(),
        file_id: 1,
        line,
        local_name: String::new(),
        evidence: None,
    }
}

fn neighbour(edge: EdgeRow, node: NodeRow) -> NeighbourEdge {
    NeighbourEdge { edge, node }
}

fn alpha() -> NodeRow {
    NodeRow {
        start_line: 3,
        end_line: 9,
        signature: Some("function alpha(): void".to_owned()),
        ..node(1, "src/a.ts::alpha", "function", "alpha")
    }
}

fn run_b() -> NodeRow {
    node(2, "src/b.ts::run", "function", "run")
}

fn run_c() -> NodeRow {
    node(3, "src/c.ts::run", "method", "run")
}

fn ghost() -> NodeRow {
    node(4, "src/ghost.ts::missing", "function", "missing")
}

fn caller() -> NodeRow {
    node(10, "src/d.ts::caller", "function", "caller")
}

fn test_caller() -> NodeRow {
    node(
        11,
        "tests/a.test.ts::alpha_is_stable",
        "test",
        "alpha_is_stable",
    )
}

fn callee() -> NodeRow {
    node(20, "src/e.ts::callee", "function", "callee")
}

fn builtin() -> NodeRow {
    node(21, "builtins.dict", "function", "dict")
}

fn deep() -> NodeRow {
    node(22, "src/h.ts::deep", "function", "deep")
}

fn referenced() -> NodeRow {
    node(30, "src/f.ts::shape", "class", "shape")
}

fn imported() -> NodeRow {
    node(40, "src/g.ts::helpers", "module", "helpers")
}

/// Enough same-group neighbours that a small budget has to page instead of fitting everything.
fn helpers() -> Vec<NodeRow> {
    (0..8)
        .map(|index| {
            node(
                100 + index,
                &format!("src/helpers.ts::helper_{index}"),
                "function",
                &format!("helper_{index}"),
            )
        })
        .collect()
}

/// Seven same-named symbols in seven files: wider than [`CANDIDATE_CAP`], which is the case where the
/// matched total and the returned count are different numbers and both have to be reported.
fn duplicates() -> Vec<NodeRow> {
    (0..7)
        .map(|index| {
            node(
                300 + index,
                &format!("src/dup{index}.ts::dup"),
                "function",
                "dup",
            )
        })
        .collect()
}

fn file_row(indexed_generation: &str) -> IndexedFileRow {
    IndexedFileRow {
        id: 1,
        path: "src/a.ts".to_owned(),
        language: "typescript".to_owned(),
        sha256: "a".repeat(64),
        size: 200,
        line_count: 40,
        parse_status: "ok".to_owned(),
        parse_error_ranges: None,
        indexed_generation: indexed_generation.to_owned(),
    }
}

fn unbound(id: i64, line: i64) -> UnresolvedRef {
    UnresolvedRef {
        id,
        file_id: 1,
        file: "src/a.ts".to_owned(),
        line,
        raw_target: "mystery".to_owned(),
        evidence_kind: Some("receiver".to_owned()),
        evidence: Some("receiver type unknown".to_owned()),
    }
}

/// One row per bucket of [`UNRESOLVED_BUCKETS`], all in the traced file: a census and a bucket filter
/// need a scope where the breakdown and the flat count are different numbers.
fn mixed_unbound() -> Vec<UnresolvedRef> {
    vec![
        UnresolvedRef {
            id: 1,
            line: 11,
            raw_target: "fs".to_owned(),
            evidence: Some("import:fs".to_owned()),
            ..unbound(1, 11)
        },
        UnresolvedRef {
            id: 2,
            line: 12,
            raw_target: "lodash".to_owned(),
            evidence: Some("import:lodash".to_owned()),
            ..unbound(2, 12)
        },
        UnresolvedRef {
            id: 3,
            line: 13,
            raw_target: "./util.js".to_owned(),
            evidence: Some("import:./util.js".to_owned()),
            ..unbound(3, 13)
        },
        UnresolvedRef {
            id: 4,
            line: 14,
            raw_target: "obj[0]".to_owned(),
            evidence_kind: Some("dynamic".to_owned()),
            evidence: Some("call:obj[0]".to_owned()),
            ..unbound(4, 14)
        },
        UnresolvedRef {
            id: 5,
            line: 15,
            raw_target: "render".to_owned(),
            evidence_kind: Some("receiver_type_unknown".to_owned()),
            evidence: Some("call:render".to_owned()),
            ..unbound(5, 15)
        },
        UnresolvedRef {
            id: 6,
            line: 16,
            raw_target: "messageOf".to_owned(),
            evidence_kind: Some("ambiguous".to_owned()),
            evidence: Some("call:messageOf".to_owned()),
            ..unbound(6, 16)
        },
    ]
}

/// The fixture's graph, as the fake reader answers it.
struct Fixture {
    /// Omit the unbound call site, so the scope may claim an exact total.
    clean: bool,
    /// An indexed file written by another publish: the answer is about a stale file.
    indexed_generation: Option<String>,
    /// How many unbound call sites the file scope holds; enough of them out-bid a page.
    unresolved_rows: Option<usize>,
    /// Verbatim rows, for a fixture whose buckets have to differ from one another.
    unresolved_set: Option<Vec<UnresolvedRef>>,
    /// A database with no published generation, for the bootstrapping answer.
    unpublished: bool,
    /// The generation row's `published_at`, which the read status reports as `indexed_at`.
    published_at: Option<String>,
    /// Make the scope read fail, so a failed read must surface as an error rather than `not_found`.
    broken_coverage: bool,
}

impl Fixture {
    const fn new() -> Self {
        Self {
            clean: false,
            indexed_generation: None,
            unresolved_rows: None,
            unresolved_set: None,
            unpublished: false,
            published_at: None,
            broken_coverage: false,
        }
    }

    const fn clean(mut self) -> Self {
        self.clean = true;
        self
    }

    fn indexed_generation(mut self, generation: &str) -> Self {
        self.indexed_generation = Some(generation.to_owned());
        self
    }

    const fn unresolved_rows(mut self, rows: usize) -> Self {
        self.unresolved_rows = Some(rows);
        self
    }

    fn unresolved_set(mut self, rows: Vec<UnresolvedRef>) -> Self {
        self.unresolved_set = Some(rows);
        self
    }

    const fn unpublished(mut self) -> Self {
        self.unpublished = true;
        self
    }

    const fn broken_coverage(mut self) -> Self {
        self.broken_coverage = true;
        self
    }
}

struct FakeReader {
    by_qn: HashMap<String, NodeRow>,
    by_name: HashMap<String, Vec<NodeRow>>,
    inbound: HashMap<i64, Vec<NeighbourEdge>>,
    outbound: HashMap<i64, Vec<NeighbourEdge>>,
    file: IndexedFileRow,
    unresolved: Vec<UnresolvedRef>,
    generation: Option<String>,
    published_at: String,
    broken_coverage: bool,
}

impl FakeReader {
    fn new(fixture: Fixture) -> Self {
        let alpha = alpha();
        let callee = callee();
        let nodes = vec![
            alpha.clone(),
            run_b(),
            run_c(),
            ghost(),
            caller(),
            test_caller(),
            callee.clone(),
            builtin(),
            deep(),
            referenced(),
            imported(),
        ];
        let helpers = helpers();
        let duplicates = duplicates();
        let by_qn: HashMap<String, NodeRow> = nodes
            .iter()
            .chain(helpers.iter())
            .chain(duplicates.iter())
            .map(|node| (node.qn.clone(), node.clone()))
            .collect();

        let mut by_name: HashMap<String, Vec<NodeRow>> = HashMap::new();
        by_name.insert("alpha".to_owned(), vec![alpha.clone()]);
        by_name.insert("run".to_owned(), vec![run_b(), run_c()]);
        by_name.insert("dup".to_owned(), duplicates.clone());
        by_name.insert("missing".to_owned(), vec![ghost()]);
        by_name.insert("caller".to_owned(), vec![caller()]);

        let mut inbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
        inbound.insert(
            alpha.id,
            vec![
                neighbour(
                    edge(1, caller().id, alpha.id, "calls", "extracted", 12),
                    caller(),
                ),
                neighbour(
                    edge(2, test_caller().id, alpha.id, "tested_by", "extracted", 4),
                    test_caller(),
                ),
            ],
        );

        let mut outbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
        outbound.insert(
            callee.id,
            vec![neighbour(
                edge(3, callee.id, deep().id, "calls", "extracted", 5),
                deep(),
            )],
        );
        let mut alpha_out = vec![
            neighbour(
                edge(4, alpha.id, callee.id, "calls", "extracted", 20),
                callee.clone(),
            ),
            neighbour(
                edge(5, alpha.id, builtin().id, "calls", "inferred", 21),
                builtin(),
            ),
        ];
        alpha_out.extend(helpers.iter().enumerate().map(|(index, helper)| {
            neighbour(
                edge(
                    200 + index as i64,
                    alpha.id,
                    helper.id,
                    "calls",
                    "extracted",
                    30 + index as i64,
                ),
                helper.clone(),
            )
        }));
        alpha_out.push(neighbour(
            edge(50, alpha.id, referenced().id, "references", "extracted", 40),
            referenced(),
        ));
        alpha_out.push(neighbour(
            edge(51, alpha.id, imported().id, "imports", "extracted", 41),
            imported(),
        ));
        outbound.insert(alpha.id, alpha_out);

        let unresolved = if fixture.clean {
            Vec::new()
        } else if let Some(rows) = fixture.unresolved_set {
            rows
        } else if let Some(count) = fixture.unresolved_rows {
            (0..count)
                .map(|index| unbound(index as i64 + 1, 7 + index as i64))
                .collect()
        } else {
            vec![unbound(1, 7)]
        };

        Self {
            by_qn,
            by_name,
            inbound,
            outbound,
            file: file_row(fixture.indexed_generation.as_deref().unwrap_or(GENERATION)),
            unresolved,
            generation: if fixture.unpublished {
                None
            } else {
                Some(GENERATION.to_owned())
            },
            published_at: fixture
                .published_at
                .unwrap_or_else(|| INDEXED_AT.to_owned()),
            broken_coverage: fixture.broken_coverage,
        }
    }
}

impl GraphReader for FakeReader {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        Ok(self.by_qn.get(qn).cloned())
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        let rows = self.by_name.get(name).cloned().unwrap_or_default();
        Ok(match kinds {
            Some(kinds) if !kinds.is_empty() => rows
                .into_iter()
                .filter(|node| kinds.contains(&node.kind))
                .collect(),
            _ => rows,
        })
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("trace never asks for a node's members")
    }

    fn nodes_by_file(&self, _file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("trace never asks for a file's nodes")
    }

    fn inbound(
        &self,
        node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(self.inbound.get(&node_id).cloned().unwrap_or_default())
    }

    fn outbound(
        &self,
        node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(self.outbound.get(&node_id).cloned().unwrap_or_default())
    }

    fn metrics_for(
        &self,
        _node_id: i64,
    ) -> Result<Option<sangkar_store::read::metrics::NodeMetrics>, AppError> {
        unreachable!("trace never reads a node's metrics")
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        if self.broken_coverage {
            return Err(AppError::internal("the coverage read failed"));
        }
        let files: Vec<IndexedFileRow> = prefixes
            .iter()
            .filter(|prefix| sangkar_core::paths::is_under_prefix(&self.file.path, prefix))
            .map(|_| self.file.clone())
            .collect();
        Ok(PathCoverage {
            indexed: files.len(),
            skipped: 0,
            files,
            not_indexed: Vec::new(),
        })
    }

    fn not_indexed_all(
        &self,
    ) -> Result<Vec<sangkar_store::read::coverage::SkippedPathRow>, AppError> {
        unreachable!("trace asks for one file's coverage, never the whole graph's skips")
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        Ok(0)
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        Ok(self
            .unresolved
            .iter()
            .filter(|row| file_ids.contains(&row.file_id))
            .cloned()
            .collect())
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        Ok((path == self.file.path).then(|| self.file.clone()))
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        unreachable!("trace asks for its own file, not the staleness manifest")
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        unreachable!("trace never renders a language census")
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        unreachable!("trace does not opt into the test census")
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: self.generation.clone(),
            generation_seq: self.generation.as_ref().map(|_| 1),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("fixture".to_owned()),
            project_root: Some("/tmp/fixture".to_owned()),
            project_branch: None,
            project_remote: None,
            embed_dim: None,
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        Ok(self.generation.as_ref().map(|id| GenerationRow {
            id: id.clone(),
            seq: 1,
            published_at: self.published_at.clone(),
            base_commit: None,
            worktree_dirty: false,
            files_indexed: 1,
            nodes: self.by_qn.len() as i64,
            edges: 0,
            duration_ms: 0,
            mode: "fast".to_owned(),
            status: "ok".to_owned(),
        }))
    }

    fn removed_symbols_for(&self, _since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        unreachable!("trace never asks for tombstones")
    }

    fn entry_points(&self, _limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        unreachable!("trace never ranks entry points")
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        unreachable!("trace never ranks hotspots")
    }

    fn test_gaps(&self, _ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        unreachable!("trace is not a coverage report")
    }

    fn read_node_fts(&self, _match_query: &str, _limit: usize) -> Result<Vec<FtsHit>, AppError> {
        unreachable!("trace resolves a name, it does not search")
    }
}

fn clock() -> String {
    NOW.to_owned()
}

fn context(reader: FakeReader) -> ToolContext {
    let cursor: Rc<dyn CursorFacade> =
        Rc::new(Cursor::new(KEY.as_bytes().to_vec(), None).expect("bind the test cursor key"));
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        // The truncation log line goes nowhere: the sink is what keeps a test's stderr readable.
        log: Logger::with_sink("trace", |_, _, _| {}),
        now: clock,
        config: Rc::new(
            load_config(&[], &|_key: &str| None, Path::new("/tmp/fixture"))
                .expect("resolve the fixture config"),
        ),
        db_size_bytes: 0,
    }
}

fn ctx() -> ToolContext {
    context(FakeReader::new(Fixture::new()))
}

fn payload_of(envelope: &Envelope<Value>) -> &Value {
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    envelope
        .payload
        .as_ref()
        .expect("an ok envelope has a payload")
}

/// The neighbours a page carries, in sequence order, as `(id, hop)`.
fn items_of(payload: &Value) -> Vec<(i64, i64)> {
    let mut items = Vec::new();
    for side in ["in", "out"] {
        for group in payload[side].as_array().expect("a group list") {
            for item in group["items"].as_array().expect("an item list") {
                items.push((
                    item["id"].as_i64().expect("an id"),
                    item["hop"].as_i64().expect("a hop"),
                ));
            }
        }
    }
    items
}

fn ids_of(payload: &Value) -> Vec<i64> {
    items_of(payload).into_iter().map(|(id, _)| id).collect()
}

fn group_edges(payload: &Value) -> Vec<String> {
    let mut edges = Vec::new();
    for side in ["in", "out"] {
        for group in payload[side].as_array().expect("a group list") {
            edges.push(group["edge"].as_str().expect("an edge kind").to_owned());
        }
    }
    edges
}

fn declared(name: &str) -> &'static Param {
    PARAMETERS
        .iter()
        .find(|param| param.name == name)
        .expect("the parameter is declared")
}

#[test]
fn the_manifest_keeps_the_description_verbatim_and_the_documented_defaults() {
    assert_eq!(NAME, "trace");
    assert!(
        DESCRIPTION.starts_with(
            "Show what is connected to one symbol: callers, callees, implementers, importers and tests."
        ),
        "{DESCRIPTION}"
    );
    assert!(DESCRIPTION.contains("Does not return source code."));
    assert!(DESCRIPTION.contains("`status: \"ambiguous\"` with candidates"));

    let query = declared("query");
    assert!(query.required, "query is the only required argument");
    assert_eq!(query.ty, ParamType::Str);
    assert_eq!(
        query.description,
        "The symbol to look up: a name, dotted name, path or `@decorator`."
    );
    assert_eq!(
        PARAMETERS.iter().filter(|param| param.required).count(),
        1,
        "at most one required argument per tool"
    );

    assert_eq!(
        declared("direction").ty,
        ParamType::Enum(&DIRECTIONS),
        "the direction enum is the module's own list"
    );
    assert_eq!(
        declared("direction").default,
        Some(ParamDefault::Text("all"))
    );
    assert_eq!(
        declared("kinds").ty,
        ParamType::Array(&NODE_KINDS),
        "the node-kind narrowing is all-or-nothing"
    );
    let depth = declared("depth");
    assert_eq!(depth.ty, ParamType::Integer);
    assert_eq!(depth.default, Some(ParamDefault::Integer(DEFAULT_DEPTH)));
    assert_eq!((depth.min, depth.max), (Some(MIN_DEPTH), Some(MAX_DEPTH)));
    assert_eq!(MAX_DEPTH, 4, "the hop ceiling is expand's own clamp");
    assert_eq!(
        declared("budget").default,
        Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64))
    );
    assert_eq!(DEFAULT_BUDGET_TOKENS, 3_200);
    let unresolved = declared("unresolved");
    assert_eq!(
        unresolved.ty,
        ParamType::Enum(&UNRESOLVED_BUCKETS),
        "the bucket filter's values are the taxonomy's own list, never a copy"
    );
    assert_eq!(unresolved.default, None, "a filter has no default bucket");
    assert_eq!(
        declared("cursor").description,
        "Continue this same query from the `next` cursor of a previous page."
    );
    assert!(
        declared("repo")
            .description
            .contains("Registered repository"),
        "an absent repo means unselected, so it has no default"
    );
}

#[test]
fn the_depth_is_clamped_to_the_contract_and_the_hash_keeps_the_callers_number() {
    assert_eq!(TraceArgs::new("alpha").depth(), 2, "the declared default");
    assert_eq!(
        TraceArgs {
            depth: Some(9),
            ..TraceArgs::new("alpha")
        }
        .depth(),
        4,
        "a wider depth is clamped, never walked"
    );
    assert_eq!(
        TraceArgs {
            depth: Some(0),
            ..TraceArgs::new("alpha")
        }
        .depth(),
        1,
        "a zero depth still walks one hop"
    );
    // The hash is the call's identity, so it keeps the number the caller sent: `depth: 9` and
    // `depth: 4` walk the same four hops and are still two different calls.
    assert_ne!(
        args_hash_of(&TraceArgs {
            depth: Some(9),
            ..TraceArgs::new("alpha")
        }),
        args_hash_of(&TraceArgs {
            depth: Some(4),
            ..TraceArgs::new("alpha")
        })
    );
}

#[test]
fn an_implicit_default_hashes_like_an_explicit_one_and_like_the_typescript_golden() {
    assert_eq!(
        args_hash_of(&TraceArgs::new("alpha")),
        args_hash_of(&TraceArgs {
            direction: Some(sangkar_domain::neighbors::Direction::All),
            depth: Some(DEFAULT_DEPTH),
            budget: Some(DEFAULT_BUDGET_TOKENS),
            ..TraceArgs::new("alpha")
        })
    );
    assert_ne!(
        args_hash_of(&TraceArgs::new("alpha")),
        args_hash_of(&TraceArgs {
            direction: Some(sangkar_domain::neighbors::Direction::In),
            ..TraceArgs::new("alpha")
        })
    );
    assert!(
        args_hash_of(&TraceArgs::new("alpha")).starts_with("sha256:"),
        "the digest carries the contract's `sha256:` prefix"
    );
    assert_eq!(args_hash_of(&TraceArgs::new("alpha")).len(), 7 + 64);

    // The golden call: `sangkar trace --query alpha --depth 2 --budget 500` over `polyglot`. The
    // digest is over the compact JSON of the seven effective arguments in declaration order:
    // {"query":"alpha","direction":"all","kinds":null,"path":null,"unresolved":null,"depth":2,"budget":500}
    assert_eq!(
        args_hash_of(&TraceArgs {
            depth: Some(2),
            budget: Some(500),
            ..TraceArgs::new("alpha")
        }),
        "sha256:9faebf6b682b24fe58ce7ed2361cc01c08521a1815d77d1a84f36ea5f02ab914"
    );
}

#[test]
fn a_resolved_trace_groups_both_directions_in_the_contracts_order() {
    let envelope = handle(&ctx(), &TraceArgs::new("alpha"));
    let payload = payload_of(&envelope);

    assert_eq!(
        payload["resolution"],
        serde_json::json!({
            "query": "alpha",
            "matched": "src/a.ts::alpha",
            "kind": "function",
            "how": "exact name, 1 of 1 candidate",
            "confidence": "extracted",
        })
    );
    assert_eq!(
        payload["node"],
        serde_json::json!({
            "id": 1,
            "qn": "src/a.ts::alpha",
            "kind": "function",
            "file": "src/a.ts",
            "lines": [3, 9],
            "signature": "function alpha(): void",
            "is_test": false,
        })
    );

    // `calls` before `tested_by`: the fixed order, not alphabetical.
    let inbound: Vec<(String, String)> = payload["in"]
        .as_array()
        .expect("in groups")
        .iter()
        .map(|group| {
            (
                group["edge"].as_str().expect("edge").to_owned(),
                group["tier"].as_str().expect("tier").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        inbound,
        vec![
            ("calls".to_owned(), "extracted".to_owned()),
            ("tested_by".to_owned(), "extracted".to_owned()),
        ]
    );
    // `calls` before `references` before `imports` is the contract's order, not alphabetical.
    assert_eq!(
        group_edges(payload),
        vec![
            "calls",
            "tested_by",
            "calls",
            "calls",
            "references",
            "imports"
        ]
    );

    let items = items_of(payload);
    assert!(items.iter().all(|(_, hop)| *hop >= 1));
    assert_eq!(
        items.iter().find(|(id, _)| *id == 22).map(|(_, hop)| *hop),
        Some(2),
        "the second hop is reached only through callee"
    );
    assert_eq!(
        items.iter().find(|(id, _)| *id == 20).map(|(_, hop)| *hop),
        Some(1)
    );

    // The builtin is an `inferred` call while the plain callee is an `extracted` one, so they sit in
    // two groups that share an edge kind: the tier, not the edge, is what separates them.
    let inferred = payload["out"]
        .as_array()
        .expect("out groups")
        .iter()
        .find(|group| group["tier"] == "inferred")
        .expect("an inferred group");
    let builtin = inferred["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["id"] == 21)
        .expect("the builtin");
    assert_eq!(builtin["external"], true, "a qn without `::` is external");
    assert!(builtin.get("is_test").is_none());

    let tested = payload["in"]
        .as_array()
        .expect("in groups")
        .iter()
        .find(|group| group["edge"] == "tested_by")
        .expect("the tested_by group");
    assert_eq!(tested["items"][0]["is_test"], true);
    assert!(tested["items"][0].get("external").is_none());
    let plain = payload["in"][0]["items"][0].clone();
    assert!(plain.get("is_test").is_none(), "a false flag costs bytes");
    assert!(plain.get("external").is_none());

    assert_eq!(envelope.totals.get("callers_total"), Some(&Value::from(2)));
    assert_eq!(envelope.totals.get("callees_total"), Some(&Value::from(13)));
    assert_eq!(envelope.read_status.generation, GENERATION);
    assert_eq!(envelope.read_status.indexed_at, INDEXED_AT);
    assert!(!envelope.read_status.stale);
    assert_eq!(
        envelope.read_status.stale_checked_at.as_deref(),
        Some(NOW),
        "the handler records that freshness was checked"
    );
    assert!(!envelope.truncated);
    assert!(envelope.next.is_none());
    assert_eq!(envelope.next_tools, vec!["expand".to_owned()]);
    assert!(envelope.summary.contains("2 callers, 13 callees"));
}

#[test]
fn the_unresolved_rows_explain_the_scope_and_an_unbound_site_makes_totals_a_lower_bound() {
    let envelope = handle(&ctx(), &TraceArgs::new("alpha"));
    let payload = payload_of(&envelope);

    assert_eq!(
        payload["unresolved"],
        serde_json::json!([{
            "file": "src/a.ts",
            "line": 7,
            "name": "mystery",
            "bucket": "receiver_type_unknown",
            "evidence": "receiver type unknown",
            "tier": "unresolved",
        }])
    );
    assert_eq!(payload["not_indexed"], serde_json::json!([]));
    assert_eq!(envelope.coverage.indexed, 1);
    assert_eq!(envelope.coverage.unresolved_edges, 1);
    assert!(envelope.coverage.complete);
    // An unbound call site makes an exact caller count a lie, so the relation is a lower bound.
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
    assert_eq!(payload["unresolved_census"]["total"], 1);
    assert_eq!(payload["unresolved_census"]["material"], 1);
}

#[test]
fn a_clean_fresh_scope_is_allowed_to_claim_an_exact_total() {
    let clean = context(FakeReader::new(Fixture::new().clean()));
    let envelope = handle(&clean, &TraceArgs::new("alpha"));
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("eq"))
    );
    assert_eq!(payload_of(&envelope)["unresolved"], serde_json::json!([]));
}

#[test]
fn a_file_written_by_another_publish_makes_the_answer_stale() {
    let stale = context(FakeReader::new(Fixture::new().indexed_generation("g-0")));
    let envelope = handle(&stale, &TraceArgs::new("alpha"));
    assert!(envelope.read_status.stale);
    assert!(
        envelope
            .read_status
            .stale_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("was last written by generation g-0")),
        "{:?}",
        envelope.read_status.stale_reason
    );
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
}

#[test]
fn a_direction_that_was_not_walked_is_not_a_zero() {
    let inbound = handle(
        &ctx(),
        &TraceArgs {
            direction: Some(sangkar_domain::neighbors::Direction::In),
            ..TraceArgs::new("alpha")
        },
    );
    let payload = payload_of(&inbound);
    assert_eq!(payload["out"], serde_json::json!([]));
    assert_eq!(payload["in"].as_array().map(Vec::len), Some(2));
    assert_eq!(inbound.totals.get("callers_total"), Some(&Value::from(2)));
    assert!(
        inbound.totals.get("callees_total").is_none(),
        "`callees_total: 0` would read as `nothing calls this`"
    );
    assert!(inbound.summary.contains("2 callers"));
    assert!(!inbound.summary.contains("callees"));

    let outbound = handle(
        &ctx(),
        &TraceArgs {
            direction: Some(sangkar_domain::neighbors::Direction::Out),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(payload_of(&outbound)["in"], serde_json::json!([]));
    assert_eq!(outbound.totals.get("callees_total"), Some(&Value::from(13)));
    assert!(outbound.totals.get("callers_total").is_none());
}

#[test]
fn depth_one_stops_at_the_direct_neighbours() {
    let envelope = handle(
        &ctx(),
        &TraceArgs {
            depth: Some(1),
            ..TraceArgs::new("alpha")
        },
    );
    let payload = payload_of(&envelope);
    let ids = ids_of(payload);
    assert!(!ids.contains(&22), "deep is two hops away");
    assert_eq!(
        items_of(payload)
            .into_iter()
            .find(|(id, _)| *id == 20)
            .map(|(_, hop)| hop),
        Some(1)
    );
}

#[test]
fn every_unresolved_row_carries_the_bucket_the_store_names() {
    let mixed = context(FakeReader::new(
        Fixture::new().unresolved_set(mixed_unbound()),
    ));
    let envelope = handle(&mixed, &TraceArgs::new("alpha"));
    let payload = payload_of(&envelope);
    let buckets: Vec<&str> = payload["unresolved"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["bucket"].as_str().expect("a bucket"))
        .collect();
    assert_eq!(buckets, UNRESOLVED_BUCKETS.to_vec());
    assert!(
        buckets
            .iter()
            .all(|bucket| UNRESOLVED_BUCKETS.contains(bucket)),
        "the closed taxonomy only"
    );
}

#[test]
fn the_census_total_is_the_coverage_count_and_its_buckets_sum_to_it() {
    let mixed = context(FakeReader::new(
        Fixture::new().unresolved_set(mixed_unbound()),
    ));
    let envelope = handle(&mixed, &TraceArgs::new("alpha"));
    let census = &payload_of(&envelope)["unresolved_census"];
    assert_eq!(
        census["total"].as_u64(),
        Some(envelope.coverage.unresolved_edges)
    );
    let sum: u64 = UNRESOLVED_BUCKETS
        .iter()
        .map(|bucket| census["buckets"][*bucket].as_u64().expect("a count"))
        .sum();
    assert_eq!(Some(sum), census["total"].as_u64());
    assert_eq!(census["total"], 6);
    // `material` is the taxonomy's own predicate, not a bucket name: the unknown receiver and the
    // ambiguous row are the two a user-defined symbol could still be hiding behind.
    assert_eq!(census["material"], 2);
    assert_eq!(census["buckets"]["builtin_stdlib"], 1);
    assert_eq!(census["buckets"]["ambiguous"], 1);
}

#[test]
fn a_bucket_filter_lists_that_bucket_only_and_leaves_the_census_whole() {
    let mixed = context(FakeReader::new(
        Fixture::new().unresolved_set(mixed_unbound()),
    ));
    let unfiltered = payload_of(&handle(&mixed, &TraceArgs::new("alpha"))).clone();
    let filtered = handle(
        &mixed,
        &TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::ExternalPackage),
            ..TraceArgs::new("alpha")
        },
    );
    let payload = payload_of(&filtered);
    let buckets: Vec<&str> = payload["unresolved"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["bucket"].as_str().expect("a bucket"))
        .collect();
    assert_eq!(buckets, vec!["external_package"]);
    // The filter chooses what to read, not which call sites failed to bind.
    assert_eq!(
        payload["unresolved_census"],
        unfiltered["unresolved_census"]
    );
    assert_eq!(filtered.coverage.unresolved_edges, 6);
}

#[test]
fn a_filtered_answer_says_how_many_rows_the_filter_hid() {
    let mixed = context(FakeReader::new(
        Fixture::new().unresolved_set(mixed_unbound()),
    ));
    let envelope = handle(
        &mixed,
        &TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::InternalModule),
            ..TraceArgs::new("alpha")
        },
    );
    assert!(
        envelope.summary.contains("6 unresolved call sites"),
        "{}",
        envelope.summary
    );
    assert!(
        envelope.summary.contains("5 filtered out"),
        "{}",
        envelope.summary
    );
    assert!(envelope.summary.contains("unresolved: internal_module"));
}

#[test]
fn a_filter_that_matches_nothing_still_reports_the_scopes_total() {
    let envelope = handle(
        &ctx(),
        &TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::Ambiguous),
            ..TraceArgs::new("alpha")
        },
    );
    let payload = payload_of(&envelope);
    assert_eq!(payload["unresolved"], serde_json::json!([]));
    assert_eq!(payload["unresolved_census"]["total"], 1);
    assert_eq!(payload["unresolved_census"]["buckets"]["ambiguous"], 0);
    // An empty list under a filter must never read as "nothing was unresolved".
    assert!(
        envelope
            .summary
            .contains("1 unresolved call site in src/a.ts, 1 filtered out"),
        "{}",
        envelope.summary
    );
    assert!(envelope.summary.contains("unresolved: ambiguous"));
}

#[test]
fn the_filter_is_part_of_the_call_identity() {
    assert_ne!(
        args_hash_of(&TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::Ambiguous),
            ..TraceArgs::new("alpha")
        }),
        args_hash_of(&TraceArgs::new("alpha"))
    );
    assert_ne!(
        args_hash_of(&TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::Ambiguous),
            ..TraceArgs::new("alpha")
        }),
        args_hash_of(&TraceArgs {
            unresolved: Some(sangkar_domain::unresolved::UnresolvedBucket::InternalModule),
            ..TraceArgs::new("alpha")
        })
    );
}

#[test]
fn a_tie_is_ambiguous_with_a_capped_candidate_list_and_no_body() {
    let envelope = handle(&ctx(), &TraceArgs::new("run"));
    assert_eq!(envelope.status, Status::Ambiguous);
    let payload = envelope.payload.as_ref().expect("a candidates payload");
    let qns: Vec<&str> = payload["candidates"]
        .as_array()
        .expect("candidates")
        .iter()
        .map(|candidate| candidate["qn"].as_str().expect("a qn"))
        .collect();
    assert_eq!(qns, vec!["src/b.ts::run", "src/c.ts::run"]);
    // The one float the payload carries renders as the shortest round-trip `0.9`, the same bytes
    // `JSON.stringify` writes: `serde_json`'s ryu and JavaScript agree on every score the resolution
    // can produce (1.0, 0.9, 0.85, 0.8, 0.75, 0.7, 0.6, 0.3), and 1.0 can never be a candidate's score
    // because `exact_qn`'s single hit is never a tie.
    assert_eq!(payload["candidates"][0]["score"], 0.9);
    // No answer body: an ambiguous envelope must not also carry a node or neighbours.
    assert!(payload.get("node").is_none());
    assert!(payload.get("in").is_none());
    assert!(payload.get("out").is_none());
    assert!(envelope.summary.contains("2 candidates"));
}

#[test]
fn kinds_narrow_resolution_so_a_tie_across_kinds_can_resolve() {
    let tied = handle(&ctx(), &TraceArgs::new("run"));
    assert_eq!(tied.status, Status::Ambiguous);
    let narrowed = handle(
        &ctx(),
        &TraceArgs {
            kinds: Some(vec!["method".to_owned()]),
            ..TraceArgs::new("run")
        },
    );
    assert_eq!(narrowed.status, Status::Ok);
    assert_eq!(
        payload_of(&narrowed)["node"]["qn"],
        Value::from("src/c.ts::run")
    );
}

#[test]
fn a_tie_wider_than_the_cap_reports_the_matched_total_and_the_shown_count() {
    let envelope = handle(&ctx(), &TraceArgs::new("dup"));
    assert_eq!(envelope.status, Status::Ambiguous);
    let payload = envelope.payload.as_ref().expect("a candidates payload");
    let shown = payload["candidates"].as_array().map(Vec::len);
    assert_eq!(shown, Some(CANDIDATE_CAP));
    assert_eq!(payload["candidates_total"], 7);
    assert_eq!(payload["candidates_shown"], CANDIDATE_CAP as u64);
    assert!(
        payload["candidates_total"].as_u64() > payload["candidates_shown"].as_u64(),
        "the tie is wider than the list this payload carries"
    );
    // One number per name: the totals, the payload and the prose cannot disagree.
    assert_eq!(
        envelope.totals.get("candidates_total"),
        Some(&Value::from(7))
    );
    assert_eq!(
        envelope.totals.get("candidates_shown"),
        Some(&Value::from(CANDIDATE_CAP as u64))
    );
    assert!(envelope.summary.contains("7 candidates matched"));
    assert!(!envelope.summary.contains("5 candidates matched"));
    for candidate in payload["candidates"].as_array().expect("candidates") {
        assert!(
            candidate["why"]
                .as_str()
                .expect("a why")
                .contains("of 7 candidates"),
            "{candidate}"
        );
    }
}

#[test]
fn every_candidate_carries_the_call_that_answers_it() {
    let envelope = handle(
        &ctx(),
        &TraceArgs {
            direction: Some(sangkar_domain::neighbors::Direction::In),
            ..TraceArgs::new("dup")
        },
    );
    let payload = envelope.payload.as_ref().expect("a candidates payload");
    let candidates = payload["candidates"].as_array().expect("candidates");
    assert!(!candidates.is_empty());
    for candidate in candidates {
        assert_eq!(candidate["next_call"]["tool"], "trace");
        assert_eq!(candidate["next_call"]["query"], candidate["qn"]);
        assert_eq!(
            candidate["next_call"]["direction"], "in",
            "the caller's own direction is echoed"
        );
        let qn = candidate["next_call"]["query"].as_str().expect("a query");
        let answer = handle(
            &ctx(),
            &TraceArgs {
                direction: Some(sangkar_domain::neighbors::Direction::In),
                ..TraceArgs::new(qn)
            },
        );
        assert_eq!(answer.status, Status::Ok);
        assert_eq!(payload_of(&answer)["node"]["qn"], candidate["qn"]);
    }
}

#[test]
fn an_omitted_direction_is_reported_as_all_so_the_offered_call_repeats_this_one() {
    let envelope = handle(&ctx(), &TraceArgs::new("run"));
    let payload = envelope.payload.as_ref().expect("a candidates payload");
    for candidate in payload["candidates"].as_array().expect("candidates") {
        assert_eq!(candidate["next_call"]["direction"], "all");
    }
}

#[test]
fn path_narrows_a_tie_to_one_candidate_and_answers_with_the_node() {
    let narrowed = handle(
        &ctx(),
        &TraceArgs {
            path: Some("src/b.ts".to_owned()),
            ..TraceArgs::new("run")
        },
    );
    assert_eq!(narrowed.status, Status::Ok);
    assert_eq!(payload_of(&narrowed)["node"]["qn"], "src/b.ts::run");
    assert_eq!(narrowed.totals.get("callers_total"), Some(&Value::from(0)));
    // A prefix is a directory or the exact file: `src/dup3` is neither, `src/dup3.ts` is the file.
    let one = handle(
        &ctx(),
        &TraceArgs {
            path: Some("src/dup3.ts".to_owned()),
            ..TraceArgs::new("dup")
        },
    );
    assert_eq!(one.status, Status::Ok);
    assert_eq!(payload_of(&one)["node"]["qn"], "src/dup3.ts::dup");
}

#[test]
fn no_match_returns_near_misses_and_never_a_bare_zero() {
    let envelope = handle(&ctx(), &TraceArgs::new("missing.thing"));
    assert_eq!(envelope.status, Status::NotFound);
    let payload = envelope.payload.as_ref().expect("a near-miss payload");
    let qns: Vec<&str> = payload["near_misses"]
        .as_array()
        .expect("near misses")
        .iter()
        .map(|miss| miss["qn"].as_str().expect("a qn"))
        .collect();
    assert_eq!(qns, vec!["src/ghost.ts::missing"]);
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
    assert!(!envelope.coverage.complete);
}

#[test]
fn a_filtered_miss_stays_a_miss_and_its_near_misses_are_already_path_filtered() {
    // Unfiltered, the dotted tail finds `caller`; under `src/b.ts` the same query has no answer.
    let unfiltered = handle(&ctx(), &TraceArgs::new("run.caller"));
    assert_eq!(unfiltered.status, Status::Ok);
    assert_eq!(payload_of(&unfiltered)["node"]["qn"], "src/d.ts::caller");

    let filtered = handle(
        &ctx(),
        &TraceArgs {
            path: Some("src/b.ts".to_owned()),
            ..TraceArgs::new("run.caller")
        },
    );
    assert_eq!(filtered.status, Status::NotFound);
    let payload = filtered.payload.as_ref().expect("a near-miss payload");
    assert_eq!(payload["near_misses"][0]["qn"], "src/b.ts::run");
    assert_eq!(
        payload["near_misses"][0]["next_call"]["query"],
        "src/b.ts::run"
    );
    assert_eq!(payload["near_misses"][0]["next_call"]["direction"], "all");

    let answer = handle(&ctx(), &TraceArgs::new("src/b.ts::run"));
    assert_eq!(answer.status, Status::Ok);
    assert_eq!(payload_of(&answer)["node"]["qn"], "src/b.ts::run");
}

#[test]
fn a_path_that_matches_nothing_answers_not_found_with_no_near_miss_from_outside_it() {
    let envelope = handle(
        &ctx(),
        &TraceArgs {
            path: Some("tests/".to_owned()),
            ..TraceArgs::new("run")
        },
    );
    assert_eq!(envelope.status, Status::NotFound);
    assert_eq!(
        envelope.payload.as_ref().expect("a payload")["near_misses"],
        serde_json::json!([])
    );
    assert_eq!(
        envelope.totals.get("near_misses_total"),
        Some(&Value::from(0))
    );
    assert!(envelope.summary.contains("no symbol matched"));
}

#[test]
fn the_first_page_is_cut_mints_a_next_and_keeps_the_relation_a_lower_bound() {
    let first = handle(
        &ctx(),
        &TraceArgs {
            budget: Some(SMALL_BUDGET),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(first.status, Status::Ok);
    assert!(first.truncated);
    assert_eq!(
        first.truncation_reason,
        Some(sangkar_core::envelope::TruncationReason::Budget)
    );
    assert!(first.next.is_some());
    assert_eq!(
        first.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
    // Totals are the relation's totals, not the page's: truncation never rewrites them.
    assert_eq!(first.totals.get("callers_total"), Some(&Value::from(2)));
    assert_eq!(first.totals.get("callees_total"), Some(&Value::from(13)));
    assert!(first.summary.contains("neighbours shown"));
    let shown = items_of(payload_of(&first)).len();
    assert!(shown > 0);
    assert!(shown < 15);
}

#[test]
fn a_cursor_round_trip_continues_the_sequence_without_repeating_or_skipping_a_row() {
    let whole = ids_of(payload_of(&handle(&ctx(), &TraceArgs::new("alpha"))));
    let args = TraceArgs {
        budget: Some(SMALL_BUDGET),
        ..TraceArgs::new("alpha")
    };
    let mut delivered: Vec<i64> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0_usize;
    // Bounded so a cursor that failed to advance fails the test instead of hanging it.
    for _ in 0..20 {
        let request = TraceArgs {
            cursor: cursor.clone(),
            ..args.clone()
        };
        let envelope = handle(&ctx(), &request);
        assert_eq!(envelope.status, Status::Ok);
        let page = ids_of(payload_of(&envelope));
        pages += 1;

        if let Some(token) = envelope.next.clone() {
            // The token a page hands back verifies against this very call and addresses the item
            // after the last one served.
            let verified = ctx().cursor.verify(
                &token,
                &CursorExpectation {
                    generation: GENERATION.to_owned(),
                    tool: NAME.to_owned(),
                    args_hash: args_hash_of(&args),
                },
            );
            let expected = (delivered.len() + page.len()) as u64;
            assert_eq!(
                verified.position(),
                Some(expected),
                "the minted position is start + entries.len()"
            );
            assert!(!page.is_empty(), "a truncated page always advances");
        } else {
            assert!(envelope.payload.is_some());
        }
        delivered.extend(page);
        cursor = envelope.next;
        if cursor.is_none() {
            break;
        }
    }
    assert!(pages > 1, "a small budget must page this fixture");
    assert_eq!(
        delivered.len(),
        whole.len(),
        "every neighbour of the untruncated answer appears exactly once across the pages"
    );
    let mut sorted_delivered = delivered.clone();
    sorted_delivered.sort_unstable();
    sorted_delivered.dedup();
    assert_eq!(sorted_delivered.len(), delivered.len(), "no duplicates");
    let mut expected = whole.clone();
    expected.sort_unstable();
    assert_eq!(
        sorted_delivered, expected,
        "no neighbour skipped between pages"
    );
}

#[test]
fn a_resumed_page_keeps_the_group_order_and_the_per_item_hop() {
    let args = TraceArgs {
        budget: Some(SMALL_BUDGET),
        ..TraceArgs::new("alpha")
    };
    let first = handle(&ctx(), &args);
    let token = first.next.clone().expect("a first page mints a token");
    let second = handle(
        &ctx(),
        &TraceArgs {
            cursor: Some(token),
            ..args
        },
    );
    let payload = payload_of(&second);
    let groups = group_edges(payload);
    let rank = |edge: &str| {
        [
            "calls",
            "inherits",
            "implements",
            "references",
            "tested_by",
            "imports",
        ]
        .iter()
        .position(|known| *known == edge)
        .unwrap_or(6)
    };
    let mut ordered = groups.clone();
    ordered.sort_by_key(|edge| rank(edge));
    assert_eq!(groups, ordered);
    assert!(items_of(payload).iter().all(|(_, hop)| *hop >= 1));
}

#[test]
fn a_cursor_never_repeats_a_row_and_the_second_page_differs_from_the_first() {
    // The dense case in miniature: the evidence list is bigger than the page it competes with, which
    // is what let `emit` empty the neighbour groups and mint a token at the position it was given.
    let dense = context(FakeReader::new(Fixture::new().unresolved_rows(160)));
    let whole = ids_of(payload_of(&handle(&dense, &TraceArgs::new("alpha"))));
    let args = TraceArgs {
        budget: Some(SMALL_BUDGET),
        ..TraceArgs::new("alpha")
    };
    let first = handle(&dense, &args);
    let first_ids = ids_of(payload_of(&first));
    assert!(first.truncated);
    assert!(first.next.is_some());
    assert!(!first_ids.is_empty());

    let second = handle(
        &dense,
        &TraceArgs {
            cursor: first.next.clone(),
            ..args
        },
    );
    let second_ids = ids_of(payload_of(&second));
    assert!(!second_ids.is_empty());
    assert!(second.truncated);

    // The defect: a page whose token repeats the position it was given re-serves itself forever.
    assert!(second.next.is_some());
    assert_ne!(second.next, first.next);
    assert!(
        second_ids.iter().all(|id| !first_ids.contains(id)),
        "no row is served twice"
    );

    // Contiguous, in sequence order: the two pages are the first and second slice of the whole.
    assert_eq!(first_ids.as_slice(), &whole[..first_ids.len()]);
    assert_eq!(
        second_ids.as_slice(),
        &whole[first_ids.len()..first_ids.len() + second_ids.len()]
    );
    assert!(first.summary.contains("neighbours shown"));
    assert!(second.summary.contains("neighbours shown"));
    assert_eq!(
        second.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
}

#[test]
fn unresolved_evidence_is_capped_so_the_neighbour_rows_still_reach_the_page() {
    // 160 unbound call sites in one file: uncapped, the evidence list alone is the whole budget, which
    // is how the neighbour groups were emptied while the payload still claimed to be a page.
    let noisy = context(FakeReader::new(Fixture::new().unresolved_rows(160)));
    let envelope = handle(
        &noisy,
        &TraceArgs {
            budget: Some(SMALL_BUDGET),
            ..TraceArgs::new("alpha")
        },
    );
    let payload = payload_of(&envelope);
    assert_eq!(envelope.coverage.unresolved_edges, 160);
    let listed = payload["unresolved"].as_array().map(Vec::len).unwrap_or(0);
    assert!(listed < 160, "the evidence list is capped");
    assert!(listed > 0);
    // A capped list still arrives with the whole census, so the missing rows are explained.
    assert_eq!(payload["unresolved_census"]["total"], 160);
    assert_eq!(
        payload["unresolved_census"]["buckets"]["receiver_type_unknown"],
        160
    );
    assert_eq!(payload["unresolved_census"]["material"], 160);
    assert!(!ids_of(payload).is_empty());
    assert!(envelope.truncated);
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
}

#[test]
fn a_budget_below_one_neighbour_terminates_honestly_instead_of_minting_a_token() {
    let envelope = handle(
        &ctx(),
        &TraceArgs {
            budget: Some(120),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(envelope.status, Status::Ok);
    // A zero-row page is only honest as the last one: the cut is at the envelope's floor.
    assert!(envelope.truncated);
    assert_eq!(
        envelope.truncation_reason,
        Some(sangkar_core::envelope::TruncationReason::Budget)
    );
    assert!(envelope.next.is_none());
    assert!(items_of(payload_of(&envelope)).is_empty());
    assert!(
        envelope.summary.contains("does not fit budget 120 tokens"),
        "{}",
        envelope.summary
    );
    // The cheaper query is the route it names; `expand` cannot stand in for a neighbour list.
    assert!(envelope.next_tools.contains(&"trace".to_owned()));
    assert!(!envelope.next_tools.contains(&"expand".to_owned()));
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );
}

#[test]
fn a_cursor_is_refused_rather_than_silently_re_served() {
    let tampered = handle(
        &ctx(),
        &TraceArgs {
            cursor: Some("not-a-cursor".to_owned()),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(tampered.status, Status::Error);
    assert_eq!(
        tampered.error.as_ref().map(|error| error.code.as_str()),
        Some("cursor_invalid")
    );
    assert!(
        tampered
            .error
            .as_ref()
            .and_then(|error| error.hint.as_deref())
            .is_some_and(|hint| hint.contains("re-run the original call"))
    );
    assert!(tampered.payload.is_none());

    let context = ctx();
    let stale = context.cursor.sign(&sangkar_core::cursor::CursorPayload {
        generation: "g-0".to_owned(),
        tool: NAME.to_owned(),
        args_hash: args_hash_of(&TraceArgs::new("alpha")),
        position: 0,
    });
    let expired = handle(
        &context,
        &TraceArgs {
            cursor: Some(stale),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(
        expired.error.as_ref().map(|error| error.code.as_str()),
        Some("cursor_expired")
    );
    assert_eq!(
        expired
            .error
            .as_ref()
            .and_then(|error| error.hint.as_deref()),
        Some("re-run the original call")
    );

    let first = handle(
        &context,
        &TraceArgs {
            budget: Some(SMALL_BUDGET),
            ..TraceArgs::new("alpha")
        },
    );
    let mismatched = handle(
        &context,
        &TraceArgs {
            budget: Some(200),
            cursor: first.next.clone(),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(
        mismatched.error.as_ref().map(|error| error.code.as_str()),
        Some("cursor_mismatch")
    );
    assert!(
        mismatched
            .error
            .as_ref()
            .and_then(|error| error.hint.as_deref())
            .is_some_and(|hint| hint.contains("same tool and arguments"))
    );

    let foreign = context.cursor.sign(&sangkar_core::cursor::CursorPayload {
        generation: GENERATION.to_owned(),
        tool: "search".to_owned(),
        args_hash: args_hash_of(&TraceArgs::new("alpha")),
        position: 0,
    });
    let refused = handle(
        &context,
        &TraceArgs {
            cursor: Some(foreign),
            ..TraceArgs::new("alpha")
        },
    );
    assert_eq!(
        refused.error.as_ref().map(|error| error.code.as_str()),
        Some("cursor_mismatch")
    );
}

#[test]
fn an_unpublished_graph_says_so_instead_of_claiming_the_symbol_does_not_exist() {
    let envelope = handle(
        &context(FakeReader::new(Fixture::new().unpublished())),
        &TraceArgs::new("alpha"),
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.as_ref().expect("an error payload");
    assert_eq!(error.code.as_str(), "bootstrapping");
    assert!(error.retry_after_ms.is_some_and(|wait| wait > 0));
    assert!(envelope.payload.is_none());
}

#[test]
fn a_failed_read_is_an_error_envelope_not_a_not_found() {
    let broken = context(FakeReader::new(Fixture::new().broken_coverage()));
    let envelope = handle(&broken, &TraceArgs::new("alpha"));
    assert_eq!(
        envelope.status,
        Status::Error,
        "a failed read is not `not_found`"
    );
    assert_eq!(
        envelope.error.as_ref().map(|error| error.code.as_str()),
        Some("internal")
    );
    assert!(envelope.payload.is_none());
}

/// The polyglot `polyglot/trace.page.json` call: `--query alpha --depth 2 --budget 500`.
///
/// The graph slice is the captured answer's own neighbourhood, hand-written here because the goldens
/// were captured from a database this suite must not open: node 22 (`src/chain.ts::alpha`), its six
/// first-hop callers in the order the captured page shows them, the file node that contains it, the
/// four second-hop file nodes behind those callers, the three second-hop callers of a caller, and the
/// two callees (`beta` at hop 1, `gamma` behind it).
fn polyglot_node(id: i64, qn: &str, kind: &str, line: i64) -> NodeRow {
    NodeRow {
        start_line: line,
        ..node(id, qn, kind, name_of(qn))
    }
}

/// The symbol half of a `qn`: what the indexer writes as `node.name`.
fn name_of(qn: &str) -> &str {
    qn.rsplit_once("::").map_or(qn, |(_, name)| name)
}

fn polyglot() -> FakeReader {
    let alpha = NodeRow {
        start_line: 3,
        end_line: 5,
        // `src/chain.ts` is file 1: the unresolved row and the coverage scope hang off that id.
        file_id: 1,
        signature: Some("function alpha(n: number): number {".to_owned()),
        ..node(22, "src/chain.ts::alpha", "function", "alpha")
    };
    let legacy_entry = polyglot_node(30, "src/legacy.js::legacyEntry", "function", 3);
    let duplicate_parse = polyglot_node(27, "src/duplicate.ts::parseInput", "function", 4);
    let uses_ghost = polyglot_node(20, "src/broken-import.ts::usesGhost", "function", 5);
    let test_alpha = polyglot_node(79, "tests/chain.test.ts::testAlpha", "test", 3);
    let chain_parse = polyglot_node(25, "src/chain.ts::parseInput", "function", 15);
    let widget = polyglot_node(70, "src/widget.tsx::Widget", "class", 3);
    let chain_file = polyglot_node(21, "src/chain.ts::chain.ts", "file", 1);
    let beta = polyglot_node(23, "src/chain.ts::beta", "function", 7);
    let gamma = polyglot_node(24, "src/chain.ts::gamma", "function", 11);

    let encode_twice = polyglot_node(28, "src/duplicate.ts::encodeTwice", "function", 10);
    let test_parse = polyglot_node(80, "tests/chain.test.ts::testParseInput", "test", 12);
    let test_file = polyglot_node(78, "tests/chain.test.ts::chain.test.ts", "file", 1);
    let legacy_file = polyglot_node(29, "src/legacy.js::legacy.js", "file", 1);
    let duplicate_file = polyglot_node(26, "src/duplicate.ts::duplicate.ts", "file", 1);
    let broken_file = polyglot_node(19, "src/broken-import.ts::broken-import.ts", "file", 1);
    let widget_file = polyglot_node(65, "src/widget.tsx::widget.tsx", "file", 1);

    let all = vec![
        alpha.clone(),
        legacy_entry.clone(),
        duplicate_parse.clone(),
        uses_ghost.clone(),
        test_alpha.clone(),
        chain_parse.clone(),
        widget.clone(),
        chain_file.clone(),
        beta.clone(),
        gamma.clone(),
        encode_twice.clone(),
        test_parse.clone(),
        test_file.clone(),
        legacy_file.clone(),
        duplicate_file.clone(),
        broken_file.clone(),
        widget_file.clone(),
    ];
    let by_qn: HashMap<String, NodeRow> = all
        .iter()
        .map(|row| (row.qn.clone(), row.clone()))
        .collect();
    let mut by_name: HashMap<String, Vec<NodeRow>> = HashMap::new();
    by_name.insert("alpha".to_owned(), vec![alpha.clone()]);

    // Hop one, in the captured page's order: the six callers, then the containing file node.
    let mut inbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
    let mut alpha_in = vec![
        neighbour(
            edge(1, legacy_entry.id, alpha.id, "calls", "extracted", 12),
            legacy_entry.clone(),
        ),
        neighbour(
            edge(2, duplicate_parse.id, alpha.id, "calls", "extracted", 13),
            duplicate_parse.clone(),
        ),
        neighbour(
            edge(3, uses_ghost.id, alpha.id, "calls", "extracted", 14),
            uses_ghost.clone(),
        ),
        neighbour(
            edge(4, test_alpha.id, alpha.id, "calls", "extracted", 15),
            test_alpha.clone(),
        ),
        neighbour(
            edge(5, chain_parse.id, alpha.id, "calls", "extracted", 16),
            chain_parse.clone(),
        ),
        neighbour(
            edge(6, widget.id, alpha.id, "calls", "extracted", 17),
            widget.clone(),
        ),
    ];
    alpha_in.push(neighbour(
        edge(7, chain_file.id, alpha.id, "contains", "extracted", 3),
        chain_file.clone(),
    ));
    inbound.insert(alpha.id, alpha_in);
    // Hop two: behind three of the callers, a caller of theirs; behind each caller, its file node.
    inbound.insert(
        legacy_entry.id,
        vec![
            neighbour(
                edge(
                    8,
                    encode_twice.id,
                    legacy_entry.id,
                    "calls",
                    "extracted",
                    20,
                ),
                encode_twice.clone(),
            ),
            neighbour(
                edge(
                    9,
                    legacy_file.id,
                    legacy_entry.id,
                    "contains",
                    "extracted",
                    1,
                ),
                legacy_file.clone(),
            ),
        ],
    );
    inbound.insert(
        duplicate_parse.id,
        vec![
            neighbour(
                edge(
                    10,
                    test_parse.id,
                    duplicate_parse.id,
                    "calls",
                    "extracted",
                    21,
                ),
                test_parse.clone(),
            ),
            neighbour(
                edge(
                    11,
                    duplicate_file.id,
                    duplicate_parse.id,
                    "contains",
                    "extracted",
                    1,
                ),
                duplicate_file.clone(),
            ),
        ],
    );
    inbound.insert(
        uses_ghost.id,
        vec![
            neighbour(
                edge(12, test_file.id, uses_ghost.id, "calls", "extracted", 22),
                test_file.clone(),
            ),
            neighbour(
                edge(
                    13,
                    broken_file.id,
                    uses_ghost.id,
                    "contains",
                    "extracted",
                    1,
                ),
                broken_file.clone(),
            ),
        ],
    );
    inbound.insert(
        widget.id,
        vec![neighbour(
            edge(14, widget_file.id, widget.id, "contains", "extracted", 1),
            widget_file.clone(),
        )],
    );

    let mut outbound: HashMap<i64, Vec<NeighbourEdge>> = HashMap::new();
    outbound.insert(
        alpha.id,
        vec![neighbour(
            edge(15, alpha.id, beta.id, "calls", "extracted", 4),
            beta.clone(),
        )],
    );
    outbound.insert(
        beta.id,
        vec![neighbour(
            edge(16, beta.id, gamma.id, "calls", "extracted", 8),
            gamma.clone(),
        )],
    );

    FakeReader {
        by_qn,
        by_name,
        inbound,
        outbound,
        file: IndexedFileRow {
            id: 1,
            path: "src/chain.ts".to_owned(),
            language: "typescript".to_owned(),
            sha256: "b".repeat(64),
            size: 67,
            line_count: 17,
            parse_status: "ok".to_owned(),
            parse_error_ranges: None,
            indexed_generation: "g-4b56df".to_owned(),
        },
        unresolved: vec![UnresolvedRef {
            id: 1,
            file_id: 1,
            file: "src/chain.ts".to_owned(),
            line: 16,
            raw_target: "parseDuplicate".to_owned(),
            evidence_kind: Some("dynamic".to_owned()),
            evidence: Some("call:parseDuplicate".to_owned()),
        }],
        generation: Some("g-4b56df".to_owned()),
        published_at: "2026-11-24T04:44:41.233Z".to_owned(),
        broken_coverage: false,
    }
}

#[test]
fn the_polyglot_page_golden_mints_the_same_next_token_byte_for_byte() {
    let golden: Value = serde_json::from_str(include_str!(
        "../../../tests/golden/polyglot/trace.page.json"
    ))
    .expect("the captured golden is JSON");
    assert_eq!(
        golden["args"],
        serde_json::json!(["--query", "alpha", "--depth", "2", "--budget", "500"])
    );
    let expected: Value = serde_json::from_str(
        golden["stdout"]
            .as_str()
            .expect("the golden's stdout is a string"),
    )
    .expect("the golden's stdout is an envelope");

    // The captured archive's canonical root and the capture's fixed key: a token is reproducible given
    // its generation, and the generation is inside the golden (`rust/tests/parity/README.md`).
    let cursor: Rc<dyn CursorFacade> = Rc::new(
        Cursor::new(PARITY_KEY.as_bytes().to_vec(), Some(PARITY_REPO.to_owned()))
            .expect("bind the parity cursor key"),
    );
    let context = ToolContext {
        reader: Rc::new(polyglot()),
        cursor,
        log: Logger::with_sink("trace", |_, _, _| {}),
        now: || "2026-11-24T04:44:42.668Z".to_owned(),
        config: Rc::new(
            load_config(&[], &|_key: &str| None, Path::new(PARITY_REPO))
                .expect("resolve the captured archive's config"),
        ),
        db_size_bytes: 0,
    };
    let args = TraceArgs {
        depth: Some(2),
        budget: Some(500),
        ..TraceArgs::new("alpha")
    };
    let envelope = handle(&context, &args);

    // The Done-when: the page-2 token the Rust binary mints is byte-identical to the TypeScript's,
    // which pins the generation, the repo, the argument digest and the page position (3 of 16).
    assert_eq!(
        envelope.next.as_deref(),
        expected["next"].as_str(),
        "the minted cursor is the golden's `next`"
    );
    assert!(envelope.truncated);
    assert_eq!(envelope.summary, expected["summary"]);
    assert_eq!(envelope.next_tools, vec!["expand".to_owned()]);

    // Everything else the capture pins, except `read_status.decisive`: the composition root sets that
    // field, and a handler may not claim the answer is the one that should end a search.
    let mut served = serde_json::to_value(&envelope).expect("the envelope is JSON");
    served["read_status"]
        .as_object_mut()
        .expect("a read_status object")
        .remove("decisive");
    let mut wanted = expected.clone();
    wanted["read_status"]
        .as_object_mut()
        .expect("a read_status object")
        .remove("decisive");
    assert_eq!(
        served, wanted,
        "the served envelope is the golden's, minus the root-owned `read_status.decisive`"
    );

    // The token the golden carries verifies under the same key, the same call and the same repo, and
    // it addresses the third neighbour: the two implementations agree on the position, not just on
    // the bytes.
    let verified = context.cursor.verify(
        expected["next"].as_str().expect("the golden's next"),
        &CursorExpectation {
            generation: "g-4b56df".to_owned(),
            tool: NAME.to_owned(),
            args_hash: args_hash_of(&args),
        },
    );
    assert_eq!(verified.position(), Some(3));
}
