//! `impact` against a hand-written reader: the payload, the digest, the change set and the page.
//!
//! Ported from `tests/tools-impact.test.ts` (915 lines). Two kinds of fixture are enough for
//! everything this tool decides: a graph described as values (files, nodes, inbound edges, metrics,
//! unresolved rows), and a temporary directory that the change-set comparison runs against — one
//! plain tree for the hash comparison, one real `git` repository for the `base` revision.
//!
//! Every test builds its own tree under the OS temp directory and removes it on drop. Nothing here
//! touches this checkout: the tool's change-set path runs `git` against the fixture root only, and
//! `SANGKAR_*` is never read.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::budget::budget_floor_of;
use sangkar_core::config::load_config;
use sangkar_core::cursor::Cursor;
use sangkar_core::envelope::{Envelope, Status, TestCensus, TestCensusEntry};
use sangkar_core::errors::AppError;
use sangkar_core::instructions::next_tools_for;
use sangkar_domain::diff::sha256_hex;
use sangkar_domain::risk::{HIGH_FROM, MEDIUM_FROM};
use sangkar_store::read::context::{CursorFacade, Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::{EdgeRow, NeighbourEdge};
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};
use serde_json::Value;

use sangkar_tools::impact::handler::{args_hash_of, handle};
use sangkar_tools::impact::manifest::{
    DEFAULT_BUDGET_TOKENS, DEFAULT_DEPTH, DESCRIPTION, EXAMPLE, ExampleValue, ImpactArgs,
    MAX_DEPTH, MIN_DEPTH, NAME, PARAMETERS, Param, ParamDefault, ParamType,
};

/// The generation the goldens were captured in, and its publish time (which is `indexed_at`).
const GENERATION: &str = "g-4b56df";
const INDEXED_AT: &str = "2026-11-24T04:44:41.233Z";

/// 47 bytes, above the cursor module's 32-byte key floor.
const KEY: &str = "impact-test-cursor-key-0123456789abcdefghij";

/// The clock each capture ran under: `stale_checked_at` is the one field that differs between the
/// three `impact` goldens of one generation.
const NOW_EXPLICIT: &str = "2026-11-24T04:44:43.225Z";
const NOW_BASE: &str = "2026-11-24T04:44:43.364Z";
const NOW_PAGE: &str = "2026-11-24T04:44:43.507Z";
/// The clock the non-golden tests read; any instants work, none of them is compared.
const NOW_TEST: &str = "2026-11-24T05:00:00.000Z";

const NOW_EXPLICIT_CLOCK: fn() -> String = || NOW_EXPLICIT.to_owned();
const NOW_BASE_CLOCK: fn() -> String = || NOW_BASE.to_owned();
const NOW_PAGE_CLOCK: fn() -> String = || NOW_PAGE.to_owned();
const NOW_TEST_CLOCK: fn() -> String = || NOW_TEST.to_owned();

/// The digest the captured `impact.page` token carries for
/// `{paths: ["src/chain.ts"], base: "", depth: 2, budget: 500}` — the assertion that pins the bare,
/// 32-character spelling and the key order in one value (`rust/tests/golden/polyglot/impact.page.json`).
const GOLDEN_ARGS_HASH: &str = "26d2c2429fb6e780a9d37cc3851850ad";

/// A temporary directory that removes itself, so the suite leaves no fixture and no repository behind.
struct TempTree(PathBuf);

impl TempTree {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-impact-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Writes one fixture file, creating its parent directories.
fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create the fixture's parent directory");
    }
    fs::write(&path, contents).expect("write the fixture file");
}

/// Runs one git command inside `dir` with a fully isolated configuration: no system or user config to
/// inherit, an identity so `commit` cannot fail, and no terminal prompt.
fn git(dir: &Path, args: &[&str]) -> String {
    let home = dir.join("home");
    fs::create_dir_all(&home).expect("create the fixture HOME");
    let output = Command::new("git")
        .args([
            "--no-pager",
            "--no-optional-locks",
            "-c",
            "credential.helper=",
            "-c",
            "core.askpass=",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Sangkar Test")
        .env("GIT_AUTHOR_EMAIL", "test@sangkar.invalid")
        .env("GIT_COMMITTER_NAME", "Sangkar Test")
        .env("GIT_COMMITTER_EMAIL", "test@sangkar.invalid")
        .env("HOME", &home)
        .output()
        .unwrap_or_else(|error| panic!("git {} could not run: {error}", args.join(" ")));
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A repository with one commit, so a change set can be measured against a real revision.
fn git_repo() -> TempTree {
    let tree = TempTree::new("git");
    write_file(tree.path(), "src/a.ts", "export function alpha() {}\n");
    git(tree.path(), &["init", "-q", "-b", "main"]);
    git(tree.path(), &["add", "-A"]);
    git(tree.path(), &["commit", "-q", "-m", "init"]);
    tree
}

/// The commit id of `HEAD`, so a `base` is a revision rather than a symbolic name.
fn head_of(root: &Path) -> String {
    git(root, &["rev-parse", "HEAD"])
}

/// One inbound edge as the graph would store it: who points at the node, and with which kind/tier.
struct Inbound {
    from: i64,
    kind: &'static str,
    tier: &'static str,
    line: i64,
}

/// A hand-written reader: every method present, and the ones this tool must never call panic rather
/// than answer, so an accidental query is a loud failure instead of a silent default.
///
/// `work` counts the reads that belong to *answering* — the change set, the seeds, the walk — and
/// not the two identity reads every branch performs. That is what makes "the cursor was refused
/// before any work was done" an assertion about the reader rather than about the clock.
struct Fixture {
    files: Vec<IndexedFileRow>,
    nodes: Vec<NodeRow>,
    inbound: BTreeMap<i64, Vec<Inbound>>,
    metrics: BTreeMap<i64, NodeMetrics>,
    unresolved: Vec<UnresolvedRef>,
    tests: TestCensus,
    generation: Option<String>,
    base_commit: Option<String>,
    work: Rc<Cell<usize>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            files: Vec::new(),
            nodes: Vec::new(),
            inbound: BTreeMap::new(),
            metrics: BTreeMap::new(),
            unresolved: Vec::new(),
            tests: TestCensus {
                nodes: 3,
                languages: vec![TestCensusEntry {
                    language: "typescript".to_owned(),
                    nodes: 3,
                }],
            },
            generation: Some(GENERATION.to_owned()),
            base_commit: None,
            work: Rc::new(Cell::new(0)),
        }
    }

    /// One indexed file, with no bytes of its own: an explicit change set is never compared to the
    /// working tree, so a fixture that asks about paths explicitly needs no disk state at all.
    fn file(&mut self, path: &str, language: &str) -> i64 {
        let text = format!("// {path}\n");
        self.file_with_digest(path, language, &text)
    }

    /// One node in the file at `path`, named so a test can read the payload's `qn`.
    fn node(&mut self, id: i64, path: &str, kind: &str, name: &str) -> i64 {
        let file_id = self
            .files
            .iter()
            .find(|file| file.path == path)
            .map_or(0, |file| file.id);
        self.nodes.push(NodeRow {
            id,
            file_id,
            path: path.to_owned(),
            parent_id: None,
            kind: kind.to_owned(),
            subkind: None,
            name: name.to_owned(),
            qn: format!("{path}::{name}"),
            start_line: 1,
            end_line: 2,
            signature: None,
            docstring: None,
            is_test: kind == "test",
            props: "{}".to_owned(),
        });
        id
    }

    /// An edge `from -> to`, so a walk from `to` reaches `from` at the hop the walk is on.
    fn inbound_edge(&mut self, to: i64, from: i64, kind: &'static str, tier: &'static str) -> i64 {
        let line = i64::try_from(self.inbound.get(&to).map_or(0, Vec::len)).unwrap_or(0) + 1;
        self.inbound.entry(to).or_default().push(Inbound {
            from,
            kind,
            tier,
            line,
        });
        from
    }

    /// A metric row. `tested` is `tested_depth`; `entry` is `is_entry`.
    fn metrics(&mut self, id: i64, in_degree: i64, tested: Option<i64>, entry: bool) {
        self.metrics.insert(
            id,
            NodeMetrics {
                node_id: id,
                in_degree,
                out_degree: 1,
                is_entry: entry,
                tested_depth: tested,
                reachable_count: None,
            },
        );
    }

    fn node_of(&self, id: i64) -> &NodeRow {
        self.nodes
            .iter()
            .find(|node| node.id == id)
            .expect("a node with that id")
    }

    /// The captures' graph: `src/chain.ts` with five symbols, eleven direct dependants across five
    /// files and one second-hop dependant. Every id, name and hop is `impact.json`'s.
    fn polyglot() -> Self {
        let mut fixture = Self::new();
        fixture.file("src/chain.ts", "typescript");
        for (offset, name) in ["chain.ts", "alpha", "beta", "gamma", "parseInput"]
            .iter()
            .enumerate()
        {
            let id = i64::try_from(offset).unwrap_or(0) + 1;
            let kind = if offset == 0 { "file" } else { "function" };
            fixture.node(id, "src/chain.ts", kind, name);
            // Three of the five are covered, so the sentence's numerator is 2 after the walk.
            fixture.metrics(id, 0, (offset < 3).then_some(0), false);
        }
        // The eleven direct dependants, in the goldens' own ids and paths.
        let hop_one: [(i64, &str, &str, &str); 11] = [
            (19, "src/broken-import.ts", "file", "broken-import.ts"),
            (20, "src/broken-import.ts", "function", "usesGhost"),
            (26, "src/duplicate.ts", "file", "duplicate.ts"),
            (27, "src/duplicate.ts", "function", "parseInput"),
            (29, "src/legacy.js", "file", "legacy.js"),
            (30, "src/legacy.js", "function", "legacyEntry"),
            (65, "src/widget.tsx", "file", "widget.tsx"),
            (70, "src/widget.tsx", "function", "Widget"),
            (78, "tests/chain.test.ts", "file", "chain.test.ts"),
            (79, "tests/chain.test.ts", "test", "testAlpha"),
            (80, "tests/chain.test.ts", "test", "testParseInput"),
        ];
        for (id, path, kind, name) in hop_one {
            if fixture.files.iter().all(|file| file.path != path) {
                fixture.file(path, "typescript");
            }
            fixture.node(id, path, kind, name);
            // A file node is reached by an import, a symbol by a call.
            let via = if kind == "file" { "imports" } else { "calls" };
            fixture.inbound_edge(1, id, via, "extracted");
            // Two of the twelve are covered, and the widest impact is a single-hop node, so the
            // second hop cannot own `top_qn`.
            fixture.metrics(id, 1, (id == 27).then_some(1), false);
        }
        // `src/duplicate.ts::encodeTwice`, reached only through `parseInput`, so the walk's second
        // hop is what the payload has to report — and its in-degree is the widest.
        fixture.node(28, "src/duplicate.ts", "function", "encodeTwice");
        fixture.inbound_edge(27, 28, "calls", "extracted");
        fixture.metrics(28, 3, Some(1), false);
        fixture.unresolved.push(UnresolvedRef {
            id: 1,
            file_id: 1,
            file: "src/chain.ts".to_owned(),
            line: 12,
            raw_target: "bag[0]".to_owned(),
            evidence_kind: Some("dynamic".to_owned()),
            evidence: Some("call:bag[0]".to_owned()),
        });
        fixture
    }

    /// The capture's clean tree: `src/chain.ts` exists with the bytes its digest was taken from, so
    /// the change set is empty. Only the `file` row is needed — nothing is a seed.
    fn clean_tree(root: &Path) -> Self {
        let mut fixture = Self::new();
        write_file(root, "src/chain.ts", "export function alpha() {}\n");
        fixture.file_with_digest("src/chain.ts", "typescript", "export function alpha() {}\n");
        fixture
    }

    /// A file row whose digest is the digest of `text`, without writing anything: the fixture decides
    /// separately whether the path exists in the working tree.
    fn file_with_digest(&mut self, path: &str, language: &str, text: &str) -> i64 {
        let id = i64::try_from(self.files.len()).unwrap_or(0) + 1;
        self.files.push(IndexedFileRow {
            id,
            path: path.to_owned(),
            language: language.to_owned(),
            sha256: sha256_hex(text.as_bytes()),
            size: i64::try_from(text.len()).unwrap_or(0),
            line_count: 1,
            parse_status: "ok".to_owned(),
            parse_error_ranges: None,
            indexed_generation: GENERATION.to_owned(),
        });
        id
    }

    /// The small change set the TypeScript suite calls `baseGraph`: one changed symbol reached by a
    /// tested caller and, one hop further, an untested entry point.
    fn small() -> Self {
        let mut fixture = Self::new();
        fixture.file("src/a.ts", "typescript");
        fixture.node(1, "src/a.ts", "function", "alpha");
        fixture.metrics(1, 0, Some(0), false);
        fixture.file("src/b.ts", "typescript");
        fixture.node(2, "src/b.ts", "function", "beta");
        fixture.metrics(2, 1, Some(1), false);
        fixture.inbound_edge(1, 2, "calls", "extracted");
        fixture.file("src/c.ts", "typescript");
        fixture.node(3, "src/c.ts", "function", "gamma");
        fixture.metrics(3, 1, None, true);
        fixture.inbound_edge(2, 3, "references", "inferred");
        fixture
    }

    /// A wide change set: `symbols` changed declarations in one file, each reached by `dependants`
    /// callers spread over four files, every caller an entry point and untested. The facts saturate
    /// the risk formula's terms, so the band is what a large change must report.
    fn wide(symbols: usize, dependants: usize) -> Self {
        let mut fixture = Self::new();
        fixture.file("src/a.ts", "typescript");
        for index in 0..symbols {
            let id = i64::try_from(index).unwrap_or(0) + 1;
            fixture.node(id, "src/a.ts", "function", &format!("symbol_{index:02}"));
            // Only the first symbol is covered, so the untested term saturates.
            fixture.metrics(id, 0, (index == 0).then_some(0), false);
        }
        for index in 0..dependants {
            let id = 1_000 + i64::try_from(index).unwrap_or(0);
            let path = format!("src/caller_{:02}.ts", index % 4);
            if fixture.files.iter().all(|file| file.path != path) {
                fixture.file(&path, "typescript");
            }
            fixture.node(id, &path, "function", &format!("caller_{index:02}"));
            fixture.metrics(id, 5, None, true);
            let seed = i64::try_from(index % symbols).unwrap_or(0) + 1;
            fixture.inbound_edge(seed, id, "calls", "extracted");
        }
        fixture
    }
}

impl GraphReader for Fixture {
    fn find_by_qn(&self, _qn: &str) -> Result<Option<NodeRow>, AppError> {
        unreachable!("impact reaches nodes by edge, never by qualified name")
    }

    fn find_by_name(
        &self,
        _name: &str,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("impact resolves no names")
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("impact reads a file's nodes, never a node's members")
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        self.work.set(self.work.get() + 1);
        Ok(self
            .nodes
            .iter()
            .filter(|node| node.file_id == file_id)
            .cloned()
            .collect())
    }

    fn inbound(
        &self,
        node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        self.work.set(self.work.get() + 1);
        Ok(self
            .inbound
            .get(&node_id)
            .map(|edges| {
                edges
                    .iter()
                    .map(|edge| NeighbourEdge {
                        edge: EdgeRow {
                            id: edge.line,
                            src_id: edge.from,
                            dst_id: node_id,
                            kind: edge.kind.to_owned(),
                            tier: edge.tier.to_owned(),
                            file_id: self.node_of(edge.from).file_id,
                            line: edge.line,
                            local_name: String::new(),
                            evidence: None,
                        },
                        node: self.node_of(edge.from).clone(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    fn outbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        unreachable!("impact walks inbound only; outbound reach is trace's question")
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        Ok(self.metrics.get(&node_id).cloned())
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        self.work.set(self.work.get() + 1);
        let files: Vec<IndexedFileRow> = self
            .files
            .iter()
            .filter(|file| {
                prefixes
                    .iter()
                    .any(|prefix| sangkar_core::paths::is_under_prefix(&file.path, prefix))
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

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        Ok(self
            .unresolved
            .iter()
            .filter(|row| file_ids.contains(&row.file_id))
            .cloned()
            .collect())
    }

    fn file_by_path(&self, path: &str) -> Result<Option<IndexedFileRow>, AppError> {
        self.work.set(self.work.get() + 1);
        Ok(self.files.iter().find(|file| file.path == path).cloned())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        self.work.set(self.work.get() + 1);
        Ok(self.files.clone())
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        unreachable!("impact publishes no language census")
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        Ok(self.tests.clone())
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: self.generation.clone(),
            generation_seq: Some(1),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("impact fixture".to_owned()),
            project_root: None,
            project_branch: None,
            project_remote: None,
            embed_dim: None,
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        let Some(generation) = &self.generation else {
            return Ok(None);
        };
        Ok(Some(GenerationRow {
            id: generation.clone(),
            seq: 1,
            published_at: INDEXED_AT.to_owned(),
            base_commit: self.base_commit.clone(),
            worktree_dirty: false,
            files_indexed: i64::try_from(self.files.len()).unwrap_or(0),
            nodes: 80,
            edges: 114,
            duration_ms: 12,
            mode: "fast".to_owned(),
            status: "ok".to_owned(),
        }))
    }

    fn removed_symbols_for(&self, _since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        Ok(Vec::new())
    }

    fn entry_points(&self, _limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        unreachable!("impact reads the entry flag per node, never the ranking")
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        unreachable!("impact ranks dependants, never hotspots")
    }

    fn test_gaps(&self, _ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        unreachable!("impact reports `tested` per node from the metric row, never the gap list")
    }

    fn read_node_fts(&self, _match_query: &str, _limit: usize) -> Result<Vec<FtsHit>, AppError> {
        unreachable!("impact resolves no free text")
    }
}

/// The context one call runs in: the reader, the cursor, a silent log sink, the injected clock and
/// the temporary tree the change-set comparison runs against.
fn context(reader: Fixture, root: &Path, now: fn() -> String, cursor: Cursor) -> ToolContext {
    let cursor: Rc<dyn CursorFacade> = Rc::new(cursor);
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        // `changed_since` logs a degraded git read; the sink keeps a test's stderr readable.
        log: Logger::with_sink("impact", |_, _, _| {}),
        now,
        config: Rc::new(
            load_config(
                &[
                    "--repo-root".to_owned(),
                    root.to_string_lossy().into_owned(),
                ],
                &|_key: &str| None,
                root,
            )
            .expect("resolve the fixture config"),
        ),
        db_size_bytes: 0,
    }
}

/// How many reads that belong to answering a call were made. The two identity reads every branch
/// performs are not counted, so a refusal can be asserted to cost no work at all.
fn work_done(work: &Rc<Cell<usize>>) -> usize {
    work.get()
}

/// The unbound cursor every non-golden test uses.
fn cursor() -> Cursor {
    Cursor::new(KEY.as_bytes().to_vec(), None).expect("bind the test cursor key")
}

fn payload_of(envelope: &Envelope<Value>) -> &Value {
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    envelope
        .payload
        .as_ref()
        .expect("an ok envelope has a payload")
}

/// The keys of a payload object, in wire order, so an assertion reads as the documented set.
fn keys_of(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("a payload object")
        .keys()
        .cloned()
        .collect()
}

/// The `id` of every impacted node a page delivered, in payload order.
fn impacted_ids(envelope: &Envelope<Value>) -> Vec<i64> {
    payload_of(envelope)["impacted"]
        .as_array()
        .expect("an impacted list")
        .iter()
        .map(|row| row["id"].as_i64().expect("a node id"))
        .collect()
}

/// The envelope as the capture recorded it: with the root's routing promise in `next_tools` and
/// without `read_status.decisive`, the two fields a composition root sets and a handler may not.
fn as_captured(envelope: &Envelope<Value>) -> Value {
    let mut value = serde_json::to_value(envelope).expect("the envelope is JSON");
    value["next_tools"] = serde_json::json!(next_tools_for(NAME));
    if let Some(status) = value["read_status"].as_object_mut() {
        status.remove("decisive");
    }
    value
}

/// The same two transformations on the captured side, so the comparison is apples to apples.
fn golden_as_captured(mut captured: Value) -> Value {
    if let Some(status) = captured["read_status"].as_object_mut() {
        status.remove("decisive");
    }
    captured
}

/// One captured golden's envelope, from the golden bank.
macro_rules! captured {
    ($name:literal) => {{
        let golden: Value = serde_json::from_str(include_str!(concat!(
            "../../../tests/golden/polyglot/",
            $name
        )))
        .expect("the captured golden is JSON");
        serde_json::from_str(
            golden["stdout"]
                .as_str()
                .expect("the golden's stdout is a string"),
        )
        .expect("the golden's stdout is an envelope")
    }};
}

#[test]
fn the_manifest_keeps_the_description_verbatim_and_the_documented_arguments() {
    assert_eq!(NAME, "impact");
    // The first sentence is the spec's, verbatim: it is the only text a model routes on.
    assert!(
        DESCRIPTION.starts_with(
            "Given a change set, list what it can reach: dependents, affected entry points, and the \
             tests that cover it, with a risk score."
        ),
        "the first sentence is the spec's, verbatim: {DESCRIPTION}"
    );
    assert!(DESCRIPTION.ends_with("Call before editing, or before merging."));

    let names: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(
        names,
        vec!["paths", "base", "depth", "budget", "cursor", "repo"]
    );
    // Nothing is required: the default scope is the working-tree diff.
    assert!(PARAMETERS.iter().all(|param| !param.required));

    let depth: &Param = PARAMETERS
        .iter()
        .find(|param| param.name == "depth")
        .expect("a depth row");
    assert_eq!(depth.ty, ParamType::Integer);
    assert_eq!(depth.default, Some(ParamDefault::Integer(DEFAULT_DEPTH)));
    assert_eq!(depth.min, Some(MIN_DEPTH));
    assert_eq!(depth.max, Some(MAX_DEPTH));
    // The clamp is the walk's own ceiling, not a second copy of it.
    assert_eq!(MAX_DEPTH, 4);
    assert_eq!(DEFAULT_DEPTH, 2);

    let budget: &Param = PARAMETERS
        .iter()
        .find(|param| param.name == "budget")
        .expect("a budget row");
    assert_eq!(
        budget.default,
        Some(ParamDefault::Integer(DEFAULT_BUDGET_TOKENS as i64))
    );
    assert_eq!(DEFAULT_BUDGET_TOKENS, 2_500);

    let paths: &Param = PARAMETERS
        .iter()
        .find(|param| param.name == "paths")
        .expect("a paths row");
    assert_eq!(paths.ty, ParamType::Array(&[]));
    // No default: an absent `paths` is the working-tree diff, which is a different call.
    assert_eq!(paths.default, None);
    assert_eq!(EXAMPLE, [("depth", ExampleValue::Integer(DEFAULT_DEPTH))]);
}

#[test]
fn the_args_hash_is_32_bare_hex_characters_and_ignores_the_cursor() {
    let paths = vec!["src/chain.ts".to_owned()];
    let digest = args_hash_of(&paths, "", 2, 500);

    // Three properties, all contract: bare (no `sha256:` prefix), lowercase hex, 32 characters.
    assert_eq!(digest.len(), 32, "the digest is truncated to 32 characters");
    assert!(
        digest
            .chars()
            .all(|character| character.is_ascii_hexdigit()),
        "every character is hex: {digest}"
    );
    assert!(
        !digest.contains(':'),
        "there is no `sha256:` prefix: {digest}"
    );
    assert_eq!(digest, GOLDEN_ARGS_HASH);
    // The full digest is longer, so the truncation is a cut and not the whole value.
    assert!(
        sha256_hex(br#"{"paths":["src/chain.ts"],"base":"","depth":2,"budget":500}"#)
            .starts_with(&digest),
        "the 32 characters are the head of the full digest"
    );

    // Every argument is part of the identity: a different budget, a different base or a different
    // path list is a different call, and a cursor minted by one must not resume another.
    assert_ne!(
        args_hash_of(&paths, "", 2, 500),
        args_hash_of(&paths, "", 2, 2_500)
    );
    assert_ne!(
        args_hash_of(&paths, "", 2, 500),
        args_hash_of(&paths, "HEAD", 2, 500)
    );
    assert_ne!(
        args_hash_of(&paths, "", 2, 500),
        args_hash_of(
            &["src/chain.ts".to_owned(), "src/a.ts".to_owned()],
            "",
            2,
            500
        )
    );
    // The function hashes the list it is handed, so the order is part of the hashed bytes. The
    // handler resolves first — normalised, deduplicated and sorted — which is why two spellings of
    // one change set verify the same cursor (`a_truncated_page_continues_where_it_stopped`).
    assert_ne!(
        args_hash_of(&["a.ts".to_owned(), "b.ts".to_owned()], "", 2, 500),
        args_hash_of(&["b.ts".to_owned(), "a.ts".to_owned()], "", 2, 500)
    );
}

#[test]
fn an_explicit_change_set_reports_hop_via_and_tier_per_dependant() {
    let tree = TempTree::new("explicit");
    let ctx = context(Fixture::small(), tree.path(), NOW_TEST_CLOCK, cursor());

    let result = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned(), "docs/notes.md".to_owned()]),
            depth: Some(2),
            ..ImpactArgs::default()
        },
    );

    assert_eq!(result.status, Status::Ok, "{:?}", result.error);
    let payload = payload_of(&result);
    // The documented payload, in the documented key order.
    assert_eq!(
        keys_of(payload),
        vec![
            "changed",
            "impacted",
            "unmatched_files",
            "risk",
            "unresolved"
        ]
    );
    assert_eq!(
        payload["changed"],
        serde_json::json!([
            {"path": "src/a.ts", "state": "modified", "symbols": 1, "entry_point": false}
        ])
    );
    // A path the graph does not know is reported with a reason, never dropped — and never counted
    // as a changed file.
    assert_eq!(
        payload["unmatched_files"],
        serde_json::json!([{"path": "docs/notes.md", "reason": "not_indexed"}])
    );
    assert_eq!(
        payload["impacted"],
        serde_json::json!([
            {
                "id": 2,
                "qn": "src/b.ts::beta",
                "kind": "function",
                "hop": 1,
                "via": "calls",
                "tier": "extracted",
                "tested": true
            },
            {
                "id": 3,
                "qn": "src/c.ts::gamma",
                "kind": "function",
                "hop": 2,
                "via": "references",
                "tier": "inferred",
                "tested": false,
                "entry_point": true
            }
        ])
    );
    // Absent, not false: the documented payload only carries the flag when it is true.
    assert!(payload["impacted"][0].get("entry_point").is_none());

    assert_eq!(result.read_status.generation, GENERATION);
    assert_eq!(result.read_status.indexed_at, INDEXED_AT);
    assert_eq!(
        result.read_status.stale_checked_at.as_deref(),
        Some(NOW_TEST)
    );
    // An explicit change set was never compared to the working tree, so freshness is not claimed.
    assert!(result.read_status.stale);
    assert!(
        result
            .read_status
            .stale_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("explicitly"))
    );
    assert_eq!(
        result.totals,
        sangkar_core::envelope::Totals::new(sangkar_core::envelope::TotalRelation::Gte)
            .with_total("impacted", Some(2))
            .with_unresolved_material(0)
    );
    assert!(!result.truncated);
    assert_eq!(result.next, None);
    assert_eq!(result.next_tools, vec!["trace", "expand"]);
    assert!(
        result
            .summary
            .contains("1 of 2 requested paths reach 2 nodes in 2 files")
    );
    assert!(result.summary.contains("1 entry points affected"));
    assert!(result.summary.contains("1 of 2 covered by tests"));

    // The risk number is the shared formula's, not a second opinion: 2 dependants in 2 files, one
    // changed symbol, none untested.
    assert_eq!(payload["risk"]["band"], serde_json::json!("low"));
    assert_eq!(payload["risk"]["score"], serde_json::json!(0.05));
    assert_eq!(
        payload["risk"]["drivers"][0],
        serde_json::json!("2 dependents sit inside the expanded radius")
    );
}

#[test]
fn an_unindexed_and_an_empty_path_are_reported_with_their_own_reasons() {
    let tree = TempTree::new("unmatched");
    let mut fixture = Fixture::small();
    // An indexed file whose publish wrote no nodes: it seeds nothing, so it cannot stay silent.
    fixture.file("src/empty.ts", "typescript");
    let ctx = context(fixture, tree.path(), NOW_TEST_CLOCK, cursor());

    let result = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["CHANGELOG.md".to_owned(), "src/empty.ts".to_owned()]),
            ..ImpactArgs::default()
        },
    );

    let payload = payload_of(&result);
    assert_eq!(payload["changed"], serde_json::json!([]));
    assert_eq!(payload["impacted"], serde_json::json!([]));
    assert_eq!(
        payload["unmatched_files"],
        serde_json::json!([
            {"path": "CHANGELOG.md", "reason": "not_indexed"},
            {"path": "src/empty.ts", "reason": "no_nodes_in_graph"}
        ])
    );
    assert!(
        result
            .summary
            .contains("2 changed paths could not be matched")
    );
    // Nothing was matched, so nothing was measured: the census is present and zero, never absent.
    assert_eq!(
        payload["unresolved"],
        serde_json::json!({
            "total": 0,
            "material": 0,
            "buckets": {
                "builtin_stdlib": 0,
                "external_package": 0,
                "internal_module": 0,
                "dynamic_property_call": 0,
                "receiver_type_unknown": 0,
                "ambiguous": 0
            }
        })
    );
}

#[test]
fn the_risk_band_separates_a_small_change_from_a_large_one() {
    let tree = TempTree::new("bands");

    let small = Fixture::small();
    let small_ctx = context(small, tree.path(), NOW_TEST_CLOCK, cursor());
    let small_result = handle(
        &small_ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            ..ImpactArgs::default()
        },
    );
    let small_payload = payload_of(&small_result);
    assert_eq!(small_payload["risk"]["band"], serde_json::json!("low"));
    assert!(
        small_payload["risk"]["score"]
            .as_f64()
            .is_some_and(|score| score < MEDIUM_FROM)
    );

    // Twenty changed symbols, eighty untested dependants in four files: the terms that carry weight
    // saturate, which is what a large change has to look like.
    let large = Fixture::wide(20, 80);
    let large_ctx = context(large, tree.path(), NOW_TEST_CLOCK, cursor());
    let large_result = handle(
        &large_ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            depth: Some(1),
            budget: Some(20_000),
            ..ImpactArgs::default()
        },
    );
    let large_payload = payload_of(&large_result);
    assert_eq!(large_payload["risk"]["band"], serde_json::json!("high"));
    // The band's floor is the risk module's, so this asserts the boundary rather than the arithmetic.
    assert!(
        large_payload["risk"]["score"]
            .as_f64()
            .is_some_and(|score| score >= HIGH_FROM)
    );
    // The band is the formula's, and the drivers name the numbers behind it.
    assert!(
        large_payload["risk"]["drivers"][0].as_str().is_some_and(
            |driver| driver.starts_with("80 dependents sit inside the expanded radius")
        )
    );
}

#[test]
fn a_removed_path_appears_in_changed_with_zero_symbols() {
    let tree = TempTree::new("removed");
    // `src/a.ts` matches its stored digest; `src/b.ts` has no bytes left to match, so the hash
    // comparison is the source that finds the removal — git is not needed for this fact.
    write_file(tree.path(), "src/a.ts", "export function alpha() {}\n");
    let mut fixture = Fixture::new();
    fixture.file_with_digest("src/a.ts", "typescript", "export function alpha() {}\n");
    fixture.file_with_digest("src/b.ts", "typescript", "export function beta() {}\n");
    fixture.node(1, "src/a.ts", "function", "alpha");
    fixture.metrics(1, 0, Some(0), false);
    fixture.node(2, "src/b.ts", "function", "beta");
    fixture.metrics(2, 1, Some(0), false);
    // A dependant behind the deleted file: nothing may be reached from a path that is gone.
    fixture.inbound_edge(2, 3, "calls", "extracted");
    fixture.node(3, "src/c.ts", "function", "gamma");
    fixture.metrics(3, 0, Some(0), false);

    let ctx = context(fixture, tree.path(), NOW_TEST_CLOCK, cursor());
    let result = handle(&ctx, &ImpactArgs::default());

    let payload = payload_of(&result);
    // The filed bug this test pins: before it, a deletion was only `changeSet.removed.length`, so an
    // impact report over a deletion named a blast radius with the deletion itself invisible.
    assert_eq!(
        payload["changed"],
        serde_json::json!([
            {"path": "src/b.ts", "state": "deleted", "symbols": 0, "entry_point": false}
        ])
    );
    // Its bytes are gone, so it seeds nothing: walking the graph's surviving rows would invent a
    // blast radius for a deletion.
    assert_eq!(payload["impacted"], serde_json::json!([]));
    // A deletion is a changed file the graph can explain, not an unmatched one.
    assert_eq!(payload["unmatched_files"], serde_json::json!([]));
    assert_eq!(
        result.read_status.stale_reason.as_deref(),
        Some("1 path removed since index")
    );
    assert!(
        result
            .summary
            .contains("1 changed files reach no dependents within 2 hops")
    );
    // The `removed` fact reaches the risk formula, which is why the driver list names it.
    assert!(
        payload["risk"]["drivers"]
            .as_array()
            .is_some_and(|drivers| drivers
                .iter()
                .any(|driver| driver == "1 removed symbols are still referenced"))
    );
}

#[test]
fn a_base_revision_finds_the_modified_path() {
    let tree = git_repo();
    let base = head_of(tree.path());
    write_file(
        tree.path(),
        "src/a.ts",
        "export function alpha() { return 1; }\n",
    );
    let mut fixture = Fixture::new();
    fixture.file_with_digest("src/a.ts", "typescript", "export function alpha() {}\n");
    fixture.node(1, "src/a.ts", "function", "alpha");
    fixture.metrics(1, 0, Some(0), false);

    let ctx = context(fixture, tree.path(), NOW_TEST_CLOCK, cursor());
    let result = handle(
        &ctx,
        &ImpactArgs {
            base: Some(base),
            depth: Some(1),
            ..ImpactArgs::default()
        },
    );

    let payload = payload_of(&result);
    assert_eq!(
        payload["changed"],
        serde_json::json!([
            {"path": "src/a.ts", "state": "modified", "symbols": 1, "entry_point": false}
        ])
    );
    assert!(result.read_status.stale);
    assert_eq!(
        result.read_status.stale_reason.as_deref(),
        Some("1 file changed since index")
    );
    assert!(
        result
            .summary
            .contains("1 changed files reach 0 nodes in 0 files")
            || result
                .summary
                .contains("1 changed files reach no dependents within 1 hops")
    );
}

#[test]
fn a_zero_score_renders_as_zero_not_zero_point_zero() {
    let tree = TempTree::new("zero");
    let fixture = Fixture::clean_tree(tree.path());
    let ctx = context(fixture, tree.path(), NOW_BASE_CLOCK, cursor());

    let result = handle(
        &ctx,
        &ImpactArgs {
            base: Some("HEAD".to_owned()),
            ..ImpactArgs::default()
        },
    );

    let payload = payload_of(&result);
    assert_eq!(payload["risk"]["score"], serde_json::json!(0));
    assert_eq!(payload["risk"]["band"], serde_json::json!("low"));
    // `ryu` writes an integral `f64` as `0.0`; `JSON.stringify` writes `0`. The field an agent quotes
    // most is the one that would otherwise differ at the byte level.
    let rendered = serde_json::to_string(payload).expect("the payload is JSON");
    assert!(
        rendered.contains("\"score\":0,"),
        "an integral score renders without a fractional part: {rendered}"
    );
    assert!(
        !rendered.contains("\"score\":0.0"),
        "the score must never render as a float zero: {rendered}"
    );
    // The captured answer says exactly this, so the spelling is the golden's and not a preference.
    assert_eq!(
        golden_as_captured(captured!("impact.base.json"))["payload"]["risk"]["score"],
        serde_json::json!(0)
    );
}

#[test]
fn a_cursor_from_another_call_is_refused_before_any_work_is_done() {
    let tree = TempTree::new("cursor");
    let fixture = Fixture::small();
    let work = Rc::clone(&fixture.work);
    let ctx = context(fixture, tree.path(), NOW_TEST_CLOCK, cursor());

    let foreign = ctx.cursor.sign(&sangkar_core::cursor::CursorPayload {
        generation: GENERATION.to_owned(),
        tool: "trace".to_owned(),
        args_hash: "0".repeat(32),
        position: 0,
    });
    let mismatch = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            cursor: Some(foreign),
            ..ImpactArgs::default()
        },
    );

    assert_eq!(mismatch.status, Status::Error);
    assert_eq!(mismatch.payload, None);
    let error = mismatch.error.clone().expect("an error payload");
    assert_eq!(error.code, sangkar_core::errors::ErrorCode::CURSOR_MISMATCH);
    assert_eq!(
        error.hint.as_deref(),
        Some("re-run the original call with the same tool and arguments")
    );
    assert_eq!(mismatch.next, None);
    // Verified before the change set is read and before the graph is walked.
    assert_eq!(work_done(&work), 0, "a refused cursor costs no read");

    let stale = ctx.cursor.sign(&sangkar_core::cursor::CursorPayload {
        generation: "g-old".to_owned(),
        tool: NAME.to_owned(),
        args_hash: "0".repeat(32),
        position: 0,
    });
    let expired = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            cursor: Some(stale),
            ..ImpactArgs::default()
        },
    );
    assert_eq!(
        expired.error.expect("an error payload").code,
        sangkar_core::errors::ErrorCode::CURSOR_EXPIRED
    );
    assert_eq!(work_done(&work), 0, "a stale cursor costs no read either");
}

#[test]
fn an_unpublished_database_answers_bootstrapping_instead_of_signing_a_cursor() {
    let tree = TempTree::new("boot");
    let mut fixture = Fixture::small();
    fixture.generation = None;
    let ctx = context(fixture, tree.path(), NOW_TEST_CLOCK, cursor());

    let result = handle(&ctx, &ImpactArgs::default());

    assert_eq!(result.status, Status::Bootstrapping);
    let error = result.error.expect("a bootstrapping payload");
    assert_eq!(error.code, sangkar_core::errors::ErrorCode::BOOTSTRAPPING);
    assert_eq!(error.retry_after_ms, Some(1_000));
    assert_eq!(result.payload, None);
    assert_eq!(result.read_status.generation, "");
    assert_eq!(result.coverage.indexed, 0);
}

#[test]
fn a_budget_above_the_ceiling_is_refused_with_the_contract_error() {
    let tree = TempTree::new("budget");
    let ctx = context(Fixture::small(), tree.path(), NOW_TEST_CLOCK, cursor());

    let result = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: Some(40_000),
            ..ImpactArgs::default()
        },
    );

    assert_eq!(result.status, Status::Error);
    let error = result.error.expect("an error payload");
    assert_eq!(
        error.code,
        sangkar_core::errors::ErrorCode::BUDGET_TOO_LARGE
    );
    assert!(
        error.hint.unwrap_or_default().contains("32000"),
        "the refusal names the ceiling to request under"
    );
}

#[test]
fn a_truncated_page_continues_where_it_stopped() {
    let tree = TempTree::new("page");
    let ctx = context(Fixture::wide(1, 80), tree.path(), NOW_PAGE_CLOCK, cursor());
    let paths = Some(vec!["src/a.ts".to_owned()]);

    let full = handle(
        &ctx,
        &ImpactArgs {
            paths: paths.clone(),
            depth: Some(1),
            budget: Some(20_000),
            ..ImpactArgs::default()
        },
    );
    let all_ids = impacted_ids(&full);
    assert_eq!(all_ids.len(), 80);
    assert!(!full.truncated);

    // The tightest budget that still cuts is found from the whole answer rather than guessed: below
    // the floor the handler refuses instead of serving past the allowance.
    let mut budget = budget_floor_of(&full).saturating_sub(1);
    while budget > 1 {
        let cut = handle(
            &ctx,
            &ImpactArgs {
                paths: paths.clone(),
                depth: Some(1),
                budget: Some(budget),
                ..ImpactArgs::default()
            },
        );
        if cut.status == Status::Ok && cut.truncated {
            break;
        }
        budget -= 1;
    }
    assert!(budget > 1, "an 80-dependant answer must be cuttable");

    let first = handle(
        &ctx,
        &ImpactArgs {
            paths: paths.clone(),
            depth: Some(1),
            budget: Some(budget),
            ..ImpactArgs::default()
        },
    );
    let first_ids = impacted_ids(&first);
    assert!(first.truncated);
    assert_eq!(
        first.truncation_reason,
        Some(sangkar_core::envelope::TruncationReason::Budget)
    );
    assert_eq!(
        first.totals.get("impacted_total"),
        Some(&Value::from(80_u64))
    );
    // A partial page, not an empty one: the fixture is large enough that the budget cuts it.
    assert!(!first_ids.is_empty() && first_ids.len() < 80);
    assert_eq!(first_ids, all_ids[..first_ids.len()]);
    assert!(
        first
            .summary
            .contains(&format!("Showing {} of 80", first_ids.len()))
    );
    let next = first.next.clone().expect("a truncated page mints a cursor");

    let resumed = handle(
        &ctx,
        &ImpactArgs {
            paths,
            depth: Some(1),
            budget: Some(budget),
            cursor: Some(next),
            ..ImpactArgs::default()
        },
    );
    let resumed_ids = impacted_ids(&resumed);
    assert!(!resumed_ids.is_empty());
    // Page N+1 overlaps page N by nothing, and continues exactly where it stopped.
    assert!(resumed_ids.iter().all(|id| !first_ids.contains(id)));
    assert_eq!(resumed_ids[0], all_ids[first_ids.len()]);
    assert_eq!(
        resumed.totals.get("impacted_total"),
        Some(&Value::from(80_u64))
    );
    assert!(resumed.summary.contains("of 80"));
    // A different spelling of the same path list is the same call: the handler resolves
    // (normalises, deduplicates, sorts) before it hashes, so the cursor still verifies.
    let respelled = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec![String::from("./src/a.ts")]),
            depth: Some(1),
            budget: Some(budget),
            cursor: Some(first.next.clone().expect("a truncated page mints a cursor")),
            ..ImpactArgs::default()
        },
    );
    assert_eq!(respelled.status, Status::Ok, "{:?}", respelled.error);
    assert_eq!(impacted_ids(&respelled), resumed_ids);
}

#[test]
fn the_explicit_change_set_matches_the_captured_answer() {
    let tree = TempTree::new("golden-explicit");
    let ctx = context(
        Fixture::polyglot(),
        tree.path(),
        NOW_EXPLICIT_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &ImpactArgs {
            paths: Some(vec!["src/chain.ts".to_owned()]),
            depth: Some(2),
            ..ImpactArgs::default()
        },
    );

    assert_eq!(
        as_captured(&envelope),
        golden_as_captured(captured!("impact.json")),
        "the explicit change set's envelope is the captured answer"
    );
}

#[test]
fn the_empty_change_set_matches_the_captured_answer() {
    let tree = TempTree::new("golden-base");
    let ctx = context(
        Fixture::clean_tree(tree.path()),
        tree.path(),
        NOW_BASE_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &ImpactArgs {
            base: Some("HEAD".to_owned()),
            ..ImpactArgs::default()
        },
    );

    // The whole envelope, `"score":0` included: this is the capture whose risk field would read
    // `0.0` if the score were serialised as a bare `f64`.
    assert_eq!(
        as_captured(&envelope),
        golden_as_captured(captured!("impact.base.json")),
        "the empty change set's envelope is the captured answer"
    );
    assert!(
        serde_json::to_string(&as_captured(&envelope))
            .expect("the envelope is JSON")
            .contains("\"score\":0,")
    );
}
