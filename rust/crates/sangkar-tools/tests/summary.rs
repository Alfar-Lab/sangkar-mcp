//! `summary` over a hand-written `GraphReader` and the captured TypeScript answers: no database, no
//! network, and one temporary working tree per test.
//!
//! Ported from `tests/tools-summary.test.ts`. What is asserted is the tool's own policy — one
//! envelope and three payloads, the two filed-bug behaviours (an empty change set claims no test
//! coverage; a rename is one entry with `from`), the census that explains an unresolved total, the
//! area scope's `dependents_total`, and the scope-specific `next_tools` — plus the three captured
//! goldens (`rust/tests/golden/polyglot/summary*.json`) and the page golden's own cursor token.
//!
//! The goldens are the oracle: the Rust envelope is compared field by field with what the TypeScript
//! CLI wrote, after applying the two post-conditions the composition root owns (`next_tools` from the
//! routing table, `read_status.decisive`) — the handler sets neither, and `tests/trace.rs` removes the
//! same field for the same reason.
//!
//! The change scope runs the real `sangkar_domain::diff` over a temporary directory. The golden's
//! tree is not a git repository, so the diff degrades to the hash comparison — the same path the
//! capture's git-less archive takes — and every stored digest matches the bytes on disk, which is
//! what makes the change set empty. The rename case is the one fact only git can supply, so that test
//! builds a real repository and skips (with a message naming the test) when `git` is not on `PATH`.
//!
//! Nothing here opens `.sangkar/graph.db`: the reader is a value, and every filesystem write goes
//! under the OS temp directory this suite creates and removes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::config::load_config;
use sangkar_core::cursor::Cursor;
use sangkar_core::envelope::{Envelope, Status, TestCensus, TestCensusEntry};
use sangkar_core::errors::AppError;
use sangkar_core::instructions::next_tools_for;
use sangkar_domain::diff::sha256_hex;
use sangkar_store::read::context::{CursorFacade, Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};
use serde_json::Value;

use sangkar_tools::summary::handler::{args_hash_of, handle};
use sangkar_tools::summary::manifest::{
    DEFAULT_BUDGET_TOKENS, DESCRIPTION, EXAMPLE, ExampleValue, NAME, PARAMETERS, Param,
    ParamDefault, ParamType, SCOPES, Scope, SummaryArgs,
};

/// The generation the goldens were captured in, and its publish time (which is `indexed_at`).
const GENERATION: &str = "g-4b56df";
const INDEXED_AT: &str = "2026-11-24T04:44:41.233Z";

/// 39 bytes, above the cursor module's 32-byte key floor.
const KEY: &str = "summary-test-cursor-key-0123456789abcdef";

/// The fixed key every captured golden was minted with (`rust/tests/parity/README.md`).
const PARITY_KEY: &str = "sangkar-parity-fixed-cursor-key-0123456789abcdef";
/// The canonical root the polyglot goldens were captured from; the cursor stamps it into the token.
const PARITY_REPO: &str = "/tmp/sangkar-parity-capture/polyglot/polyglot";

/// The clock each capture ran under. `stale_checked_at` is the one field that differs between the
/// four goldens of one generation, so each test pins its own.
const NOW_REPO: &str = "2026-11-24T04:44:41.806Z";
const NOW_AREA: &str = "2026-11-24T04:44:41.952Z";
const NOW_CHANGE: &str = "2026-11-24T04:44:42.097Z";
const NOW_PAGE: &str = "2026-11-24T04:44:42.246Z";
/// The clock the non-golden tests read; any instants work, none of them is compared.
const NOW_TEST: &str = "2026-11-24T05:00:00.000Z";

/// The polyglot fixture's 15 indexed files and their languages, in path order — which is the order
/// `rows_for_paths` returns and therefore the order the `area` surface shows.
///
/// The languages are the capture's twelve: `typescript` covers four files (the three under `src` plus
/// the test), every other language exactly one.
const POLYGLOT_FILES: [(&str, &str); 15] = [
    ("src/Account.cs", "csharp"),
    ("src/Controller.php", "php"),
    ("src/Service.java", "java"),
    ("src/broken-import.ts", "typescript"),
    ("src/chain.ts", "typescript"),
    ("src/duplicate.ts", "typescript"),
    ("src/legacy.js", "javascript"),
    ("src/lib.rs", "rust"),
    ("src/matrix.cpp", "cpp"),
    ("src/scalar.c", "c"),
    ("src/service.go", "go"),
    ("src/shapes.py", "python"),
    ("src/widget.tsx", "tsx"),
    ("src/worker.rb", "ruby"),
    ("tests/chain.test.ts", "typescript"),
];

/// The file node ids are offset from the file ids so a metric row cannot be confused with a file.
const FILE_NODE_BASE: i64 = 1_000;

/// The captured hotspot ranking: the ten rows `hotspots(10)` returned, in order, with their degrees.
const HOTSPOTS: [(&str, i64, i64); 10] = [
    ("src/chain.ts::chain.ts", 7, 5),
    ("tests/chain.test.ts::chain.test.ts", 0, 11),
    ("src/chain.ts::alpha", 8, 1),
    ("src/duplicate.ts::duplicate.ts", 2, 4),
    ("src/lib.rs::lib.rs", 0, 5),
    ("src/shapes.py::shapes", 1, 4),
    ("src/Account.cs::Account", 2, 2),
    ("src/Controller.php::Controller", 2, 2),
    ("src/Service.java::Service", 2, 2),
    ("src/chain.ts::parseInput", 3, 1),
];

const NOW_REPO_CLOCK: fn() -> String = || NOW_REPO.to_owned();
const NOW_AREA_CLOCK: fn() -> String = || NOW_AREA.to_owned();
const NOW_CHANGE_CLOCK: fn() -> String = || NOW_CHANGE.to_owned();
const NOW_PAGE_CLOCK: fn() -> String = || NOW_PAGE.to_owned();
const NOW_TEST_CLOCK: fn() -> String = || NOW_TEST.to_owned();

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
            "sangkar-summary-{label}-{}-{nanos}-{}",
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

/// The bytes one fixture file is written with: unique per path, so a digest cannot match by accident.
fn fixture_bytes(relative: &str) -> String {
    format!("// {relative}\n")
}

/// Whether the `git` binary exists. The check runs in the OS temp directory, never in this checkout.
fn git_is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .is_ok_and(|output| output.status.success())
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

/// A hand-written reader: every method present, and the ones this tool must never call panic rather
/// than answer, so an accidental query is a loud failure instead of a silent default.
struct Polyglot {
    files: Vec<IndexedFileRow>,
    nodes: Vec<NodeRow>,
    unresolved: Vec<UnresolvedRef>,
    generation: Option<String>,
    published_at: String,
    /// Whether `test_gaps` reports every requested node as untested.
    gaps: bool,
}

impl Polyglot {
    /// The polyglot fixture: each path's bytes are written under `root` first, so the stored digest
    /// is the working tree's own and the change set is empty.
    fn fixture(root: &Path) -> Self {
        Self::of(root, &POLYGLOT_FILES)
    }

    fn of(root: &Path, files: &[(&str, &str)]) -> Self {
        let mut rows = Vec::with_capacity(files.len());
        let mut nodes = Vec::with_capacity(files.len());
        for (index, (path, language)) in files.iter().enumerate() {
            let bytes = fixture_bytes(path);
            write_file(root, path, &bytes);
            let id = i64::try_from(index).unwrap_or(0) + 1;
            rows.push(IndexedFileRow {
                id,
                path: (*path).to_owned(),
                language: (*language).to_owned(),
                sha256: sha256_hex(bytes.as_bytes()),
                size: i64::try_from(bytes.len()).unwrap_or(0),
                line_count: 1,
                parse_status: "ok".to_owned(),
                parse_error_ranges: None,
                indexed_generation: GENERATION.to_owned(),
            });
            let name = path.rsplit('/').next().unwrap_or(path);
            nodes.push(NodeRow {
                id: FILE_NODE_BASE + id,
                file_id: id,
                path: (*path).to_owned(),
                parent_id: None,
                kind: "file".to_owned(),
                subkind: None,
                name: name.to_owned(),
                qn: format!("{path}::{name}"),
                start_line: 1,
                end_line: 2,
                signature: None,
                docstring: None,
                is_test: false,
                props: "{}".to_owned(),
            });
        }
        let unresolved = unresolved_rows(&rows);
        Self {
            files: rows,
            nodes,
            unresolved,
            generation: Some(GENERATION.to_owned()),
            published_at: INDEXED_AT.to_owned(),
            gaps: true,
        }
    }

    /// A modified file with twelve declarations on top of its file node: enough ranked items for a
    /// budget to cut the `changed_symbols` list, which is what a continuation resumes.
    ///
    /// The fixture reports no test gap on purpose, so `changed_symbols` is unambiguously the largest
    /// list and a page boundary is exactly the number of symbols served — a supporting list the cut
    /// had also shortened would make the next position resume slightly early (the caveat
    /// `handler.ts:857-864` documents), and that is not what this test is about.
    fn paging(root: &Path) -> Self {
        let mut reader = Self::of(root, &[("src/a.ts", "typescript")]);
        reader.gaps = false;
        // The stored digest is not the bytes on disk, which is exactly how the indexer reports a
        // change: the fixture never has to run git.
        reader.files[0].sha256 = "0".repeat(64);
        let file = reader.files[0].clone();
        for index in 0..12 {
            let name = format!("symbol_{index:02}");
            reader.nodes.push(NodeRow {
                id: 2_000 + index,
                file_id: file.id,
                path: file.path.clone(),
                parent_id: None,
                kind: "function".to_owned(),
                subkind: None,
                name: name.clone(),
                qn: format!("{}::{name}", file.path),
                start_line: index + 1,
                end_line: index + 1,
                signature: None,
                docstring: None,
                is_test: false,
                props: "{}".to_owned(),
            });
        }
        reader
    }

    /// A database with no published generation: the bootstrap answer's fixture.
    fn unpublished(mut self) -> Self {
        self.generation = None;
        self
    }

    fn file_of(&self, path: &str) -> Option<&IndexedFileRow> {
        self.files.iter().find(|file| file.path == path)
    }

    fn node_of(&self, node_id: i64) -> Option<&NodeRow> {
        self.nodes.iter().find(|node| node.id == node_id)
    }
}

/// The `unresolved` rows the capture's census reports: 26 in total, 11 material, split 16 under
/// `src` and 10 in the test file.
///
/// The split is deliberate — the `area` scope asks about `src` alone, so its coverage has to report
/// 16 rows and 8 material from the same population the repository scope reports 26 and 11 from. The
/// bucket shapes are one row per rule: a dynamic property call, a receiver the resolver could not
/// type, two indistinguishable candidates, and the four import outcomes.
fn unresolved_rows(files: &[IndexedFileRow]) -> Vec<UnresolvedRef> {
    let src: Vec<&IndexedFileRow> = files
        .iter()
        .filter(|file| file.path.starts_with("src/"))
        .collect();
    let tests = files.iter().find(|file| file.path.starts_with("tests/"));

    let mut rows: Vec<UnresolvedRef> = Vec::new();
    let push = |file: &IndexedFileRow,
                evidence_kind: Option<&str>,
                evidence: String,
                raw_target: String,
                rows: &mut Vec<UnresolvedRef>| {
        let id = i64::try_from(rows.len()).unwrap_or(0) + 1;
        rows.push(UnresolvedRef {
            id,
            file_id: file.id,
            file: file.path.clone(),
            line: id,
            raw_target,
            evidence_kind: evidence_kind.map(str::to_owned),
            evidence: Some(evidence),
        });
    };

    // Eight dynamic property calls and eight untyped receivers under `src`: 16 rows, 8 material.
    // A fixture with no `src` file (the rename case) has no src rows, and no row may index an empty
    // list.
    for index in 0..8 {
        let Some(file) = src.get(index % src.len().max(1)).copied() else {
            break;
        };
        push(
            file,
            Some("dynamic"),
            format!("call:bag[{index}]"),
            format!("bag[{index}]"),
            &mut rows,
        );
    }
    for index in 0..8 {
        let Some(file) = src.get((index + 3) % src.len().max(1)).copied() else {
            break;
        };
        push(
            file,
            None,
            format!("call:widget{index}.act"),
            format!("widget{index}.act"),
            &mut rows,
        );
    }
    // Ten rows in the test file: the rest of the dynamic calls, one untyped receiver, the two
    // ambiguous candidates, and the four import outcomes (stdlib, two packages, one broken relative).
    if let Some(tests) = tests {
        for index in 0..3 {
            push(
                tests,
                Some("dynamic"),
                format!("call:items[{index}]"),
                format!("items[{index}]"),
                &mut rows,
            );
        }
        push(
            tests,
            None,
            "call:helper.act".to_owned(),
            "helper.act".to_owned(),
            &mut rows,
        );
        push(
            tests,
            Some("ambiguous"),
            "call:parseInput candidates: src/chain.ts | src/duplicate.ts".to_owned(),
            "parseInput".to_owned(),
            &mut rows,
        );
        push(
            tests,
            Some("ambiguous"),
            "call:chain candidates: src/chain.ts | src/lib.rs".to_owned(),
            "chain".to_owned(),
            &mut rows,
        );
        push(
            tests,
            None,
            "import:fs".to_owned(),
            "fs".to_owned(),
            &mut rows,
        );
        push(
            tests,
            None,
            "import:lodash".to_owned(),
            "lodash".to_owned(),
            &mut rows,
        );
        push(
            tests,
            None,
            "import:left-pad".to_owned(),
            "left-pad".to_owned(),
            &mut rows,
        );
        push(
            tests,
            None,
            "import:./ghost-module".to_owned(),
            "./ghost-module".to_owned(),
            &mut rows,
        );
    }
    rows
}

impl GraphReader for Polyglot {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        Ok(self.nodes.iter().find(|node| node.qn == qn).cloned())
    }

    fn find_by_name(
        &self,
        name: &str,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, AppError> {
        Ok(self
            .nodes
            .iter()
            .filter(|node| node.name == name)
            .cloned()
            .collect())
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        unreachable!("summary reads a file's nodes, never a node's members")
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        Ok(self
            .nodes
            .iter()
            .filter(|node| node.file_id == file_id)
            .cloned()
            .collect())
    }

    fn inbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        unreachable!("summary reads precomputed degrees, never the inbound edges")
    }

    fn outbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        // Zero files at the first call hop: no flow is reported because no entry point is measured.
        Ok(Vec::new())
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        let indegree = match node_id - FILE_NODE_BASE {
            // `src/chain.ts` is file 5 and `src/lib.rs` file 8: 7 + 2 dependents across two files is
            // the 9 the captured `area` answer reports, with 7 as the widest single impact.
            5 => 7,
            8 => 2,
            _ => 0,
        };
        Ok(Some(NodeMetrics {
            node_id,
            in_degree: indegree,
            out_degree: 1,
            is_entry: false,
            tested_depth: Some(0),
            reachable_count: None,
        }))
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
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
        Ok(self.file_of(path).cloned())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        Ok(self.files.clone())
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        // Grouped and sorted by language, which is the order the captured `languages` list shows.
        let mut counts: BTreeMap<String, i64> = BTreeMap::new();
        for file in &self.files {
            *counts.entry(file.language.clone()).or_insert(0) += 1;
        }
        Ok(counts
            .into_iter()
            .map(|(language, files)| LanguageCensus {
                language: Some(language),
                files,
                parsed: files,
                failed: 0,
            })
            .collect())
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        Ok(TestCensus {
            nodes: 3,
            languages: vec![TestCensusEntry {
                language: "typescript".to_owned(),
                nodes: 3,
            }],
        })
    }

    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: self.generation.clone(),
            generation_seq: Some(1),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some("polyglot".to_owned()),
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
            published_at: self.published_at.clone(),
            // The captured archive is a git-less copy, so the generation has no base commit and the
            // resolved base is the empty string the page token hashes.
            base_commit: None,
            worktree_dirty: false,
            files_indexed: i64::try_from(self.files.len()).unwrap_or(0),
            // The two identity counts are the capture's own: `summary(repo)` copies them from the
            // publish row rather than re-counting the graph.
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
        // The indexer never writes `node_metrics.is_entry`, so a real graph answers none — the
        // captured repository answer says "0 entry points" for the same reason.
        Ok(Vec::new())
    }

    fn hotspots(&self, limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        Ok(HOTSPOTS
            .iter()
            .take(limit)
            .map(|(qn, in_degree, out_degree)| HotspotRow {
                id: 1,
                qn: (*qn).to_owned(),
                kind: "function".to_owned(),
                file: qn
                    .split_once("::")
                    .map_or((*qn).to_owned(), |(path, _)| path.to_owned()),
                lines: sangkar_store::read::reader::LineRange { start: 1, end: 2 },
                in_degree: *in_degree,
                out_degree: *out_degree,
            })
            .collect())
    }

    fn test_gaps(&self, ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        if !self.gaps {
            return Ok(Vec::new());
        }
        Ok(ids
            .iter()
            .filter_map(|id| self.node_of(*id))
            .map(|node| TestGapRow {
                id: node.id,
                qn: node.qn.clone(),
                kind: node.kind.clone(),
                file: node.path.clone(),
                lines: sangkar_store::read::reader::LineRange {
                    start: node.start_line,
                    end: node.end_line,
                },
                tested_depth: None,
            })
            .collect())
    }

    fn read_node_fts(&self, _match_query: &str, _limit: usize) -> Result<Vec<FtsHit>, AppError> {
        unreachable!("summary resolves no free text, so it never searches")
    }
}

/// The context one call runs in: the reader, the cursor, a silent log sink, the injected clock and
/// the temporary tree the change scope diffs against.
fn context(reader: Polyglot, root: &Path, now: fn() -> String, cursor: Cursor) -> ToolContext {
    let cursor: Rc<dyn CursorFacade> = Rc::new(cursor);
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        // The truncation log line goes nowhere: the sink keeps a test's stderr readable.
        log: Logger::with_sink("summary", |_, _, _| {}),
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

/// The unbound cursor every non-parity test uses.
fn cursor() -> Cursor {
    Cursor::new(KEY.as_bytes().to_vec(), None).expect("bind the test cursor key")
}

/// The parity cursor: the captured key and the captured canonical root, so a token is reproducible.
fn parity_cursor() -> Cursor {
    Cursor::new(PARITY_KEY.as_bytes().to_vec(), Some(PARITY_REPO.to_owned()))
        .expect("bind the parity cursor key")
}

fn payload_of(envelope: &Envelope<Value>) -> &Value {
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    envelope
        .payload
        .as_ref()
        .expect("an ok envelope has a payload")
}

/// The keys of a payload object, sorted, so an assertion reads as the documented set.
fn keys_of(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value
        .as_object()
        .expect("a payload object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    keys
}

/// The `id` of every changed symbol a page delivered, in payload order.
fn symbol_ids(envelope: &Envelope<Value>) -> Vec<i64> {
    payload_of(envelope)["changed_symbols"]
        .as_array()
        .expect("a changed-symbol list")
        .iter()
        .map(|row| row["id"].as_i64().expect("a symbol id"))
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
fn the_manifest_keeps_the_description_verbatim_and_the_documented_defaults() {
    assert_eq!(NAME, "summary");
    assert!(
        DESCRIPTION.starts_with("Summarize the current situation in a few hundred tokens. "),
        "the first sentence is the spec's, verbatim: {DESCRIPTION}"
    );
    assert!(
        DESCRIPTION.ends_with("Call this first in any task to decide what to do next."),
        "the manifest keeps the routing sentence: {DESCRIPTION}"
    );

    let declared: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(
        declared,
        vec!["scope", "target", "base", "budget", "cursor", "repo"],
        "the properties are published in the TypeScript literal's order"
    );

    // The vocabulary is the rich one — an `Enum` carrying its values and, where a bound exists, a
    // `min`/`max` pair — not `status`'s narrower table, so TASK-034 can derive one `tools/list`
    // schema from every manifest. A direct import of `trace`'s table is impossible here:
    // `sangkar-boundary` fails the build when one tool module names another.
    let bounded = Param {
        name: "depth",
        ty: ParamType::Integer,
        required: false,
        default: Some(ParamDefault::Integer(1)),
        min: Some(1),
        max: Some(4),
        description: "a bounded integer is expressible in this vocabulary",
    };
    assert_eq!(bounded.max, Some(4));
    assert_eq!(bounded.min, Some(1));
    let enumerated = Param {
        ty: ParamType::Enum(&SCOPES),
        ..bounded
    };
    assert_eq!(enumerated.ty, ParamType::Enum(&["change", "repo", "area"]));

    let scope = &PARAMETERS[0];
    assert_eq!(scope.ty, ParamType::Enum(&SCOPES));
    assert_eq!(scope.default, Some(ParamDefault::Text("change")));
    assert!(!scope.required);
    assert_eq!(SCOPES, ["change", "repo", "area"]);

    let budget = PARAMETERS
        .iter()
        .find(|param| param.name == "budget")
        .expect("budget is declared");
    assert_eq!(budget.ty, ParamType::Integer);
    assert_eq!(
        budget.default,
        Some(ParamDefault::Integer(
            i64::try_from(DEFAULT_BUDGET_TOKENS).expect("the default budget is a small number")
        ))
    );
    assert_eq!(DEFAULT_BUDGET_TOKENS, 1_200);

    // `target`, `base`, `cursor` and `repo` are strings with no default: an absent `repo` means
    // "unselected", and an absent `base` means "the generation's own base commit".
    for name in ["target", "base", "cursor", "repo"] {
        let param = PARAMETERS
            .iter()
            .find(|param| param.name == name)
            .expect("the parameter is declared");
        assert_eq!(param.ty, ParamType::Str, "{name} is a string");
        assert_eq!(param.default, None, "{name} declares no default");
        assert!(!param.required, "no summary argument is required");
    }

    assert_eq!(EXAMPLE.len(), 1);
    assert_eq!(EXAMPLE[0].0, "scope");
    assert_eq!(EXAMPLE[0].1, ExampleValue::Text("change"));

    // The scope enum's spellings are what a payload and a cursor carry.
    assert_eq!(
        Scope::ALL.map(Scope::as_str),
        ["change", "repo", "area"],
        "the three scopes in declaration order"
    );
    assert_eq!(Scope::default(), Scope::Change);
}

#[test]
fn args_hash_hashes_the_resolved_identity_and_matches_the_captured_token() {
    // The digest inside `rust/tests/golden/polyglot/summary.page.json`'s own `next` token, decoded:
    // `["repo","","",400]`, where the base is the git-less archive's empty base commit.
    assert_eq!(
        args_hash_of(Scope::Repo, "", "", 400),
        "sha256:d9066b1dbe6e24411d0d35e4013882e2d5b2c8e25ed00b66bb4026cfaa64815f"
    );

    // An omitted budget and the declared default are the same call, which is what lets a cursor
    // resume a page the caller asked for by default.
    let implicit = SummaryArgs {
        scope: Some(Scope::Change),
        ..SummaryArgs::default()
    };
    let explicit = SummaryArgs {
        scope: Some(Scope::Change),
        budget: Some(DEFAULT_BUDGET_TOKENS),
        ..SummaryArgs::default()
    };
    assert_eq!(implicit.budget_or_default(), explicit.budget_or_default());
    let hash = args_hash_of(
        implicit.scope_or_default(),
        "",
        "",
        implicit.budget_or_default(),
    );
    assert_eq!(
        hash,
        args_hash_of(
            explicit.scope_or_default(),
            "",
            "",
            explicit.budget_or_default()
        )
    );

    // A different scope, target or base is a different call.
    assert_ne!(
        hash,
        args_hash_of(Scope::Repo, "", "", DEFAULT_BUDGET_TOKENS)
    );
    assert_ne!(
        hash,
        args_hash_of(Scope::Change, "src", "", DEFAULT_BUDGET_TOKENS)
    );
    assert_ne!(
        hash,
        args_hash_of(Scope::Change, "", "HEAD", DEFAULT_BUDGET_TOKENS)
    );
}

#[test]
fn the_change_scope_publishes_the_documented_keys() {
    let tree = TempTree::new("change-keys");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(&ctx, &SummaryArgs::default());
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    let payload = payload_of(&envelope);
    assert_eq!(
        keys_of(payload),
        vec![
            "base",
            "changed_files",
            "changed_symbols",
            "removed_symbols",
            "risk",
            "suggested_next",
            "test_gaps",
            "unresolved",
        ]
    );
    assert_eq!(keys_of(&payload["risk"]), vec!["band", "drivers", "score"]);
    assert_eq!(
        keys_of(&payload["unresolved"]),
        vec!["buckets", "material", "total"]
    );
    // The six buckets are always present, zeros included: a client reads the block by key.
    let buckets = payload["unresolved"]["buckets"]
        .as_object()
        .expect("a bucket object");
    assert_eq!(
        buckets.keys().cloned().collect::<Vec<String>>(),
        sangkar_domain::unresolved::UNRESOLVED_BUCKETS.to_vec(),
        "the taxonomy's own order"
    );
    // The census explains the flat count instead of restating it: one number, one row set.
    assert_eq!(
        payload["unresolved"]["total"].as_u64(),
        Some(envelope.coverage.unresolved_edges)
    );
    assert_eq!(payload["unresolved"]["total"].as_u64(), Some(26));
    assert_eq!(payload["unresolved"]["material"].as_u64(), Some(11));

    // Every scope's `next_tools` is its own promise; a repository overview points at `search`, a
    // change summary at the blast radius first.
    assert_eq!(envelope.next_tools, vec!["impact", "trace", "expand"]);
    assert_eq!(envelope.read_status.generation, GENERATION);
    assert_eq!(envelope.read_status.indexed_at, INDEXED_AT);
    assert!(!envelope.read_status.stale);
    assert!(!envelope.truncated);
    assert_eq!(
        envelope.totals.get("changed_files_total"),
        Some(&Value::from(0_u64))
    );
    assert_eq!(
        envelope.totals.get("changed_symbols_total"),
        Some(&Value::from(0_u64))
    );
    // The default 1200-token budget carries the whole payload: only a smaller one pages it.
    assert_eq!(envelope.next, None);
}

#[test]
fn an_empty_change_set_claims_no_test_coverage() {
    let tree = TempTree::new("empty-change");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(&ctx, &SummaryArgs::default());
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    let payload = payload_of(&envelope);

    // Nothing changed: no files, no symbols, no gaps — and a clean tree is not a stale one.
    assert_eq!(payload["changed_files"], serde_json::json!([]));
    assert_eq!(payload["changed_symbols"], serde_json::json!([]));
    assert_eq!(payload["test_gaps"], serde_json::json!([]));
    assert!(!envelope.read_status.stale);

    // The filed bug (#10): an empty change set has nothing to assess, so it may not claim coverage of
    // anything. `null` says "not applicable"; a zero would say "assessed, nothing missing".
    assert_eq!(
        envelope.totals.get("test_gaps_total"),
        Some(&Value::Null),
        "an empty change set reports `test_gaps_total: null`, never 0"
    );

    // The other half of the same bug: the fact drivers survive — a risk answer is never a bare
    // number — and no driver sentence mentions tests or coverage.
    let drivers = payload["risk"]["drivers"]
        .as_array()
        .expect("a driver list");
    assert_eq!(drivers.len(), 2, "the two fact drivers: {drivers:?}");
    assert_eq!(
        drivers[0],
        Value::from("no dependents were found inside the expanded radius")
    );
    assert_eq!(drivers[1], Value::from("0 changed symbols span 0 files"));
    for driver in drivers {
        let text = driver
            .as_str()
            .expect("a driver is a sentence")
            .to_lowercase();
        assert!(
            !text.contains("test") && !text.contains("coverage"),
            "an empty change set may not talk about testing: {driver}"
        );
    }

    // The summary says the same thing in one sentence, and drops the coverage line altogether.
    assert!(envelope.summary.contains("no changed symbols to assess."));
    let summary = envelope.summary.to_lowercase();
    assert!(!summary.contains("test") && !summary.contains("coverage"));

    // A repository/area risk score can still be a bare zero; the sentence it arrives in is not.
    assert_eq!(payload["risk"]["score"].as_u64(), Some(0));
    assert_eq!(payload["risk"]["band"], Value::from("low"));
}

#[test]
fn a_zero_score_is_written_as_an_integer() {
    let tree = TempTree::new("zero-score");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(&ctx, &SummaryArgs::default());
    let payload = payload_of(&envelope);
    // `JSON.stringify(0)` is `0`; an `f64` written by serde_json's shortest-round-trip writer would
    // be `0.0`, which is not the byte the capture recorded.
    assert_eq!(payload["risk"]["score"], serde_json::json!(0));
    assert!(
        payload["risk"]["score"].is_u64(),
        "an integral score is a JSON integer, not 0.0"
    );
    let text = serde_json::to_string(&envelope).expect("the envelope is JSON");
    assert!(
        text.contains("\"risk\":{\"score\":0,\"band\":\"low\""),
        "the serialised bytes carry `\"score\":0`: {text}"
    );
    assert!(
        !text.contains("\"score\":0.0"),
        "no `.0` may reach the wire"
    );

    // An area whose surface is empty scores zero the same way (the other integral score the tool
    // produces, and the one the `empty` fixture captured).
    let area = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Area),
            target: Some("docs".to_owned()),
            ..SummaryArgs::default()
        },
    );
    let area_payload = payload_of(&area);
    assert_eq!(area_payload["risk"]["score"], serde_json::json!(0));
    assert!(area_payload["risk"]["score"].is_u64());
    assert!(
        area.summary.ends_with("risk 0 (low)."),
        "the summary sentence renders the same spelling: {}",
        area.summary
    );
}

#[test]
fn the_area_scope_counts_the_dependents_of_its_own_surface() {
    let tree = TempTree::new("area");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Area),
            target: Some("src".to_owned()),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    let payload = payload_of(&envelope);
    assert_eq!(
        keys_of(payload),
        vec!["dependents_total", "risk", "surface", "target"]
    );
    assert_eq!(payload["target"], Value::from("src"));
    let surface = payload["surface"].as_array().expect("a surface list");
    assert_eq!(
        surface.len(),
        14,
        "one file node per indexed file under `src`"
    );
    assert_eq!(keys_of(&surface[0]), vec!["kind", "qn"]);
    // 7 dependents of `src/chain.ts` plus 2 of `src/lib.rs`; the rest measure zero.
    assert_eq!(payload["dependents_total"].as_u64(), Some(9));
    assert_eq!(
        payload["risk"]["drivers"][1],
        Value::from("the widest single impact is src/chain.ts::chain.ts with 7 direct dependents")
    );
    assert_eq!(
        envelope.totals.get("surface_total"),
        Some(&Value::from(14_u64))
    );
    // The area's own census, not the repository's: the count is scoped like its coverage.
    assert_eq!(
        payload["risk"].get("score").and_then(Value::as_f64),
        Some(0.47)
    );
    assert_eq!(
        envelope.totals.get("unresolved_material"),
        Some(&Value::from(8_u64))
    );
    // A repository overview points at a broader query, not at one node's detail.
    assert_eq!(envelope.next_tools, vec!["trace", "impact", "expand"]);
}

#[test]
fn the_repo_scope_publishes_the_documented_keys() {
    let tree = TempTree::new("repo-keys");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Repo),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    let payload = payload_of(&envelope);
    assert_eq!(
        keys_of(payload),
        vec![
            "coupling_hotspots",
            "entry_points",
            "flows",
            "languages",
            "not_indexed",
            "size",
            "unresolved",
        ]
    );
    assert_eq!(keys_of(&payload["size"]), vec!["edges", "files", "nodes"]);
    assert_eq!(
        payload["size"],
        serde_json::json!({"files": 15, "nodes": 80, "edges": 114})
    );
    assert_eq!(
        envelope.totals.get("languages_total"),
        Some(&Value::from(12_u64))
    );
    // Ten hotspots is the statement bound, so the list is a leading sample and says so.
    assert!(envelope.truncated);
    assert_eq!(
        envelope.truncation_reason,
        Some(sangkar_core::envelope::TruncationReason::ItemCap)
    );
    // The repository overview's own promise, not the change summary's.
    assert_eq!(envelope.next_tools, vec!["trace", "impact", "search"]);
}

#[test]
fn the_change_scope_matches_the_captured_answer() {
    let tree = TempTree::new("golden-change");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_CHANGE_CLOCK,
        cursor(),
    );
    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Change),
            base: Some("HEAD".to_owned()),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(
        as_captured(&envelope),
        golden_as_captured(captured!("summary.change.json")),
        "the change scope's envelope is the captured answer"
    );
}

#[test]
fn the_repo_scope_matches_the_captured_answer() {
    let tree = TempTree::new("golden-repo");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_REPO_CLOCK,
        cursor(),
    );
    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Repo),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(
        as_captured(&envelope),
        golden_as_captured(captured!("summary.json")),
        "the repository scope's envelope is the captured answer"
    );
}

#[test]
fn the_area_scope_matches_the_captured_answer() {
    let tree = TempTree::new("golden-area");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_AREA_CLOCK,
        cursor(),
    );
    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Area),
            target: Some("src".to_owned()),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(
        as_captured(&envelope),
        golden_as_captured(captured!("summary.area.json")),
        "the area scope's envelope is the captured answer"
    );
}

#[test]
fn the_page_golden_mints_the_same_next_token_byte_for_byte() {
    let expected: Value = captured!("summary.page.json");
    let tree = TempTree::new("golden-page");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_PAGE_CLOCK,
        parity_cursor(),
    );
    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Repo),
            budget: Some(400),
            ..SummaryArgs::default()
        },
    );

    // The parity-critical half: the token pins the generation, the repo the cursor is bound to, the
    // argument digest and the page position. `position` is `min(paged_length, max(start,
    // paged_length - dropped))` = `min(0, max(0, 0 - dropped))` = 0 here, because this scope's paged
    // list is `not_indexed` and it is empty — the budget cut the entry points and the hotspots instead.
    assert_eq!(
        envelope.next.as_deref(),
        expected["next"].as_str(),
        "the minted cursor is the golden's `next`"
    );
    assert!(envelope.truncated);
    assert_eq!(
        envelope.truncation_reason,
        Some(sangkar_core::envelope::TruncationReason::Budget)
    );

    // Then the whole page, so a payload that no longer cuts the way the capture's did fails here
    // rather than silently in a client.
    let served = as_captured(&envelope);
    // The golden's `next` is the same token the handler just minted; comparing the envelopes whole
    // keeps every other field honest.
    let wanted = golden_as_captured(expected);
    assert_eq!(served, wanted, "the page's envelope is the captured answer");
}

#[test]
fn a_truncated_page_resumes_without_repeating_a_ranked_item() {
    let tree = TempTree::new("paging");
    let ctx = context(
        Polyglot::paging(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );
    // Small enough to cut the ranked list, large enough that the cut cannot drain it: the page then
    // advances by exactly the symbols that were not delivered.
    let budget = 700_u64;

    let first = handle(
        &ctx,
        &SummaryArgs {
            budget: Some(budget),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(first.status, Status::Ok, "{:?}", first.error);
    assert!(first.truncated, "the fixture is larger than the budget");
    let token = first.next.clone().expect("a truncated page mints a token");
    let first_ids = symbol_ids(&first);
    assert!(
        !first_ids.is_empty() && first_ids.len() < 13,
        "the page is a prefix of the ranking: {first_ids:?}"
    );

    // The same budget, so the call identity the cursor pins is unchanged.
    let second = handle(
        &ctx,
        &SummaryArgs {
            budget: Some(budget),
            cursor: Some(token),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(second.status, Status::Ok, "{:?}", second.error);
    let second_ids = symbol_ids(&second);
    assert!(
        !second_ids.is_empty(),
        "the continuation still has items to deliver"
    );
    assert!(
        second_ids.iter().all(|id| !first_ids.contains(id)),
        "a continuation may not repeat a ranked item: {first_ids:?} then {second_ids:?}"
    );
    // `mint` is `min(paged_length, max(start, paged_length - dropped))`, and with `start = 0` and
    // `dropped = paged_length - served` it lands exactly on the first page's length: the two pages
    // together are the whole ranking, with nothing repeated and nothing skipped.
    assert_eq!(
        first_ids.len() + second_ids.len(),
        13,
        "the continuation resumes exactly where the first page stopped"
    );
    // Totals are the whole sequence's, not this page's.
    assert_eq!(
        second.totals.get("changed_symbols_total"),
        Some(&Value::from(13_u64))
    );
    assert_eq!(
        second.totals.get("total_relation"),
        Some(&Value::from("gte"))
    );

    // Draining the pages yields every symbol exactly once.
    let mut seen: Vec<i64> = Vec::new();
    let mut cursor_token: Option<String> = None;
    for _ in 0..20 {
        let args = SummaryArgs {
            budget: Some(budget),
            cursor: cursor_token.clone(),
            ..SummaryArgs::default()
        };
        let page = handle(&ctx, &args);
        seen.extend(symbol_ids(&page));
        cursor_token = page.next;
        if cursor_token.is_none() {
            break;
        }
    }
    let mut unique = seen.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        seen.len(),
        "no symbol is delivered twice: {seen:?}"
    );
    assert_eq!(
        unique,
        vec![
            1_001, 2_000, 2_001, 2_002, 2_003, 2_004, 2_005, 2_006, 2_007, 2_008, 2_009, 2_010,
            2_011
        ],
        "every changed symbol is delivered once"
    );
}

#[test]
fn an_unpublished_database_answers_bootstrapping_instead_of_failing() {
    let tree = TempTree::new("bootstrap");
    let ctx = context(
        Polyglot::fixture(tree.path()).unpublished(),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(&ctx, &SummaryArgs::default());
    assert_eq!(envelope.status, Status::Bootstrapping);
    assert_eq!(envelope.payload, None);
    let error = envelope.error.expect("a bootstrapping answer carries one");
    assert_eq!(error.code, sangkar_core::errors::ErrorCode::BOOTSTRAPPING);
    assert_eq!(error.retry_after_ms, Some(2_000));
    assert!(error.message.contains("published generation"));
    assert_eq!(envelope.read_status.generation, "");
}

#[test]
fn an_unusable_cursor_is_refused_before_any_page_is_built() {
    let tree = TempTree::new("cursor-invalid");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &SummaryArgs {
            cursor: Some("not-a-token".to_owned()),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(envelope.status, Status::Error);
    assert_eq!(envelope.payload, None);
    let error = envelope.error.expect("an error answer carries one");
    assert_eq!(error.code, sangkar_core::errors::ErrorCode::CURSOR_INVALID);
    assert_eq!(
        error.hint.as_deref(),
        Some("re-run the original call to mint a fresh cursor")
    );
    assert!(envelope.read_status.stale);
}

#[test]
fn a_budget_below_the_floor_is_refused_with_the_floor() {
    let tree = TempTree::new("budget-floor");
    let ctx = context(
        Polyglot::fixture(tree.path()),
        tree.path(),
        NOW_TEST_CLOCK,
        cursor(),
    );

    let envelope = handle(
        &ctx,
        &SummaryArgs {
            scope: Some(Scope::Repo),
            budget: Some(1),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope.error.expect("an error answer carries one");
    assert_eq!(
        error.code,
        sangkar_core::errors::ErrorCode::BUDGET_TOO_LARGE
    );
    let hint = error.hint.unwrap_or_default();
    assert!(
        hint.starts_with("raise the budget to at least "),
        "the refusal names the floor: {hint}"
    );
    // The refusal carries no payload and no path list: those entries are what the caller was told to
    // make room for.
    assert_eq!(envelope.payload, None);
    assert!(envelope.coverage.not_indexed.is_empty());
}

#[test]
fn a_rename_is_one_entry_carrying_from_and_counts_as_one_changed_file() {
    if !git_is_available() {
        eprintln!(
            "skipping a_rename_is_one_entry_carrying_from_and_counts_as_one_changed_file: git is not on PATH"
        );
        return;
    }
    let tree = TempTree::new("rename");
    let root = tree.path();
    // The graph indexed the old name: its stored digest is the bytes that are about to move.
    let reader = Polyglot::of(root, &[("src/a.ts", "typescript")]);
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let base = git(root, &["rev-parse", "HEAD"]);
    git(root, &["mv", "src/a.ts", "src/c.ts"]);
    git(root, &["commit", "-q", "-m", "rename"]);

    let ctx = context(reader, root, NOW_TEST_CLOCK, cursor());
    let envelope = handle(
        &ctx,
        &SummaryArgs {
            base: Some(base),
            ..SummaryArgs::default()
        },
    );
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    let payload = payload_of(&envelope);

    // The destination has no `file` row in this fixture, so it has no symbols yet — a count of 0
    // rather than a guess — but it is still one entry with the path it came from.
    assert_eq!(
        payload["changed_files"],
        serde_json::json!([{
            "path": "src/c.ts",
            "state": "renamed",
            "from": "src/a.ts",
            "symbols": 0,
            "entry_point": false,
        }]),
        "a rename is one entry with `from`, not an added path plus a deleted one"
    );
    // The `deleted` half of the pair is what would double-count the move.
    assert!(
        !payload["changed_files"]
            .as_array()
            .expect("a changed-file list")
            .iter()
            .any(|row| row["path"].as_str() == Some("src/a.ts"))
    );
    assert_eq!(
        envelope.totals.get("changed_files_total"),
        Some(&Value::from(1_u64))
    );
    assert!(
        envelope.summary.contains("1 files changed"),
        "one move is one changed file: {}",
        envelope.summary
    );
    // The working tree moved since the publish, so the answer is stale rather than wrong.
    assert!(envelope.read_status.stale);
}
