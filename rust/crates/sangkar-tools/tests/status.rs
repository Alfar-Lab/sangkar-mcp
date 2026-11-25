//! `status` against a fake reader and against a real migrated database: the trust answer's contract
//! (`TOOLS-SPEC.md` §2), ported from `tests/tools-status.test.ts`.
//!
//! The tool's whole value is answering the trust question *including for paths that were never
//! indexed*, so the required cases are the three ways a requested path can come back: indexed,
//! skipped with the indexer's reason, and unknown to the index entirely. A fourth covers the census
//! rule the spec calls out — a language whose grammar never loaded must read as `parsed: 0` plus a
//! note, never as an empty language.
//!
//! Three seams are exercised, and each is a different kind of evidence:
//!
//! - a hand-written [`GraphReader`] over plain vectors, for every shape the payload can take. No
//!   database, no protocol, no environment: the two directory paths are temp dirs, one holding the
//!   `graph.db` whose recorded size the payload reports.
//! - a real `graph.db` written by `sangkar_store`'s own migration and raw `INSERT`s, for the two
//!   facts a fake cannot prove: the identity block a *publish* records in `meta`, and
//!   `health.statement_cache_entries`, which is the process-wide count of a live handle rather than
//!   a number a fixture can choose.
//! - the captured golden `tests/golden/polyglot/status.json`, for the payload's key set **and key
//!   order** and every stable value, with the per-run fields `tests/parity/README.md` lists masked.
//!
//! The fixture's own reader is a fake rather than the daemon's `.sangkar/graph.db`: that file is
//! rewritten by the TypeScript daemon while this suite runs, so every row here is one this suite
//! wrote and can name.

use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::config::{AppConfig, load_config};
use sangkar_core::cursor::{Cursor, CursorExpectation, CursorResult};
use sangkar_core::envelope::{Status, TestCensus, TestCensusEntry, TruncationReason};
use sangkar_domain::unresolved::UNRESOLVED_BUCKETS;
use sangkar_store::db::{open_reader, open_writer};
use sangkar_store::migrate::migrate;
use sangkar_store::read::context::{Logger, ToolContext};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, SqliteReader,
    TombstoneRow,
};
use sangkar_tools::status::handler::{StatusEnvelope, args_hash_of, handle};
use sangkar_tools::status::manifest::{
    DESCRIPTION, EXAMPLE, ExampleValue, NAME, PARAMETERS, ParamDefault, ParamType,
    STATUS_DEFAULT_TOKENS, STATUS_RETRY_AFTER_MS, StatusArgs,
};
use serde_json::Value;

// -------------------------------------------------------------------------------------------
// The fixture's clocks, identities and directories
// -------------------------------------------------------------------------------------------

const CURSOR_KEY: &str = "status-test-cursor-key-0123456789ab";
const NOW: &str = "2026-11-17T14:02:00.000Z";
const GENERATION: &str = "g-7f3a91";
const OLDER_GENERATION: &str = "g-700001";
const PUBLISHED_AT: &str = "2026-11-17T13:50:15Z";
const BASE_COMMIT: &str = "bbdda4b9";
const DEFAULT_PROJECT_NAME: &str = "sangkar";
const DEFAULT_PROJECT_BRANCH: &str = "main";
const DEFAULT_PROJECT_REMOTE: &str = "git@example.invalid:acme/sangkar.git";

/// The clock a context hands out. `status` dates its *bootstrap* answer from it; the freshness
/// verdict carries the domain module's own clock (see the handler's module note).
fn clock() -> String {
    NOW.to_owned()
}

/// A temporary directory that removes itself, so the suite leaves no graph behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-status-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The two directories a context needs: the data dir `graph.db` lives in, and the working tree the
/// answer is about. Two subdirectories of one temp dir, so a test never touches the real tree.
struct Dirs {
    _root: TempDir,
    data: PathBuf,
    repo: PathBuf,
}

impl Dirs {
    fn new(label: &str) -> Self {
        let root = TempDir::new(label);
        let data = root.0.join("data");
        let repo = root.0.join("repo");
        fs::create_dir_all(&data).expect("create the data dir");
        fs::create_dir_all(&repo).expect("create the repo dir");
        Self {
            _root: root,
            data,
            repo,
        }
    }

    /// The graph file's path: `<dataDir>/graph.db`, the one path the payload derives.
    fn db_path(&self) -> PathBuf {
        self.data.join("graph.db")
    }

    fn db_path_text(&self) -> String {
        self.db_path().display().to_string()
    }

    fn repo_text(&self) -> String {
        self.repo.display().to_string()
    }
}

/// The seam every tool task shares: a reader, a cursor, a logger, a clock, a config and the size
/// recorded for the file the reader opened.
fn context_with(
    reader: Rc<dyn GraphReader>,
    dirs: &Dirs,
    db_size_bytes: u64,
    cursor: Cursor,
) -> ToolContext {
    ToolContext {
        reader,
        cursor: Rc::new(cursor),
        log: Logger::new("status"),
        now: clock,
        config: test_config(dirs),
        db_size_bytes,
    }
}

/// A resolved configuration whose machine home and cursor key are this test's, never the
/// developer's: `SANGKAR_HOME` is a directory under the temp data dir, so no `config.json` on the
/// machine can change what the answer says about where it came from.
fn test_config(dirs: &Dirs) -> Rc<AppConfig> {
    let argv = vec![
        "--repo-root".to_owned(),
        dirs.repo_text(),
        "--data-dir".to_owned(),
        dirs.data.display().to_string(),
    ];
    let home = dirs.data.join("home").display().to_string();
    let env = move |name: &str| match name {
        "SANGKAR_HOME" => Some(home.clone()),
        "SANGKAR_CURSOR_KEY" => Some(CURSOR_KEY.to_owned()),
        _ => None,
    };
    Rc::new(load_config(&argv, &env, &dirs.repo).expect("the test configuration"))
}

fn test_cursor() -> Cursor {
    Cursor::new(CURSOR_KEY.as_bytes(), None).expect("a test cursor key")
}

// -------------------------------------------------------------------------------------------
// The fake reader's fixture
// -------------------------------------------------------------------------------------------

/// A 64-character stand-in digest: the `file.sha256` column is text, and only equality is asserted.
fn digest(id: i64) -> String {
    let mut text = format!("sha-{id}");
    while text.len() < 64 {
        text.push('0');
    }
    text
}

/// One `file` row; `parse_status` and `indexed_generation` default to a clean publish.
fn file_row(
    id: i64,
    path: &str,
    language: &str,
    parse_status: &str,
    generation: &str,
) -> IndexedFileRow {
    IndexedFileRow {
        id,
        path: path.to_owned(),
        language: language.to_owned(),
        sha256: digest(id),
        size: 100 * id,
        line_count: 10 * id,
        parse_status: parse_status.to_owned(),
        parse_error_ranges: (parse_status != "ok").then(|| "12-12".to_owned()),
        indexed_generation: generation.to_owned(),
    }
}

fn skipped_row(id: i64, path: &str, reason: &str, language: Option<&str>) -> SkippedPathRow {
    SkippedPathRow {
        id,
        path: path.to_owned(),
        reason: reason.to_owned(),
        detail: None,
        language: language.map(str::to_owned),
    }
}

fn unresolved_row(
    id: i64,
    file_id: i64,
    path: &str,
    raw_target: &str,
    evidence_kind: Option<&str>,
    evidence: Option<&str>,
) -> UnresolvedRef {
    UnresolvedRef {
        id,
        file_id,
        file: path.to_owned(),
        line: id,
        raw_target: raw_target.to_owned(),
        evidence_kind: evidence_kind.map(str::to_owned),
        evidence: evidence.map(str::to_owned),
    }
}

fn generation_row(nodes: i64, edges: i64) -> GenerationRow {
    GenerationRow {
        id: GENERATION.to_owned(),
        seq: 7,
        published_at: PUBLISHED_AT.to_owned(),
        base_commit: Some(BASE_COMMIT.to_owned()),
        worktree_dirty: false,
        files_indexed: 2,
        nodes,
        edges,
        duration_ms: 41_200,
        mode: "incremental".to_owned(),
        status: "published".to_owned(),
    }
}

/// A hand-written reader's fixture. Every field is what the store would report for those rows, so
/// the handler is never handed numbers a real reader could not produce.
#[derive(Clone)]
struct Fixture {
    files: Vec<IndexedFileRow>,
    not_indexed: Vec<SkippedPathRow>,
    /// Symbols per file id; absent means the file defines none.
    symbols: Vec<(i64, usize)>,
    unresolved: Vec<UnresolvedRef>,
    /// `None` models a database that has never published.
    generation: Option<String>,
    generation_row: Option<GenerationRow>,
    indexer_state: Option<String>,
    consecutive_failures: i64,
    project_name: Option<String>,
    /// `None` takes the served tree; `Some(None)` models a store that recorded no root.
    project_root: Option<Option<String>>,
    project_branch: Option<String>,
    project_remote: Option<String>,
    tests_census: Option<TestCensus>,
}

impl Fixture {
    fn clean() -> Self {
        Self {
            files: vec![
                file_row(1, "src/a.ts", "typescript", "ok", GENERATION),
                file_row(2, "src/b.py", "python", "ok", GENERATION),
            ],
            not_indexed: Vec::new(),
            symbols: vec![(1, 2), (2, 1)],
            unresolved: Vec::new(),
            generation: Some(GENERATION.to_owned()),
            generation_row: Some(generation_row(13_955, 130_044)),
            indexer_state: Some("idle".to_owned()),
            consecutive_failures: 0,
            project_name: Some(DEFAULT_PROJECT_NAME.to_owned()),
            project_root: None,
            project_branch: Some(DEFAULT_PROJECT_BRANCH.to_owned()),
            project_remote: Some(DEFAULT_PROJECT_REMOTE.to_owned()),
            tests_census: None,
        }
    }

    /// A graph with an outright parse failure and three skipped paths, two of them under `docs`.
    fn messy() -> Self {
        Self {
            files: vec![
                file_row(1, "src/a.ts", "typescript", "ok", GENERATION),
                file_row(2, "src/b.py", "python", "ok", GENERATION),
                file_row(3, "src/c.py", "python", "failed", GENERATION),
            ],
            not_indexed: vec![
                skipped_row(10, "assets/logo.png", "binary", None),
                skipped_row(11, "docs/legacy.xslt", "grammar_unavailable", Some("xslt")),
                skipped_row(12, "docs/older.xslt", "grammar_unavailable", Some("xslt")),
            ],
            unresolved: vec![
                unresolved_row(91, 1, "src/a.ts", "mystery", None, None),
                unresolved_row(92, 2, "src/b.py", "other", None, None),
            ],
            ..Self::clean()
        }
    }

    /// One row per bucket of the closed taxonomy, so the census is asserted against every reason a
    /// call site can fail to bind rather than against one shape.
    fn bucketed() -> Self {
        Self {
            files: vec![
                file_row(1, "src/a.ts", "typescript", "ok", GENERATION),
                file_row(2, "src/b.py", "python", "ok", GENERATION),
            ],
            unresolved: vec![
                // builtin_stdlib: an import of a Node builtin.
                unresolved_row(
                    93,
                    1,
                    "src/a.ts",
                    "readFileSync",
                    Some("import"),
                    Some("import:node:fs"),
                ),
                // external_package: an import of a package that is not in the graph.
                unresolved_row(
                    94,
                    1,
                    "src/a.ts",
                    "lodash",
                    Some("import"),
                    Some("import:lodash"),
                ),
                // internal_module: a relative import whose target is missing.
                unresolved_row(
                    95,
                    2,
                    "src/b.py",
                    "helpers",
                    Some("import"),
                    Some("import:./helpers.js"),
                ),
                // dynamic_property_call: a call the extractor recorded with no declared target.
                unresolved_row(
                    96,
                    2,
                    "src/b.py",
                    "self.handler",
                    Some("dynamic"),
                    Some("call:self.handler"),
                ),
                // ambiguous: two candidates the index could not choose between.
                unresolved_row(
                    97,
                    1,
                    "src/a.ts",
                    "src/a.ts | src/b.ts",
                    Some("ambiguous"),
                    None,
                ),
                // receiver_type_unknown: a bare call with no evidence at all.
                unresolved_row(98, 2, "src/b.py", "mystery", None, None),
            ],
            ..Self::clean()
        }
    }

    /// Two of these three skipped paths sit under `src`, so the scope's count and the graph's differ
    /// — the case that used to report `skipped: 3` beside a two-row `not_indexed` list.
    fn scoped() -> Self {
        Self {
            files: vec![
                file_row(1, "src/a.ts", "typescript", "ok", GENERATION),
                file_row(2, "docs/guide.ts", "typescript", "ok", GENERATION),
            ],
            not_indexed: vec![
                skipped_row(21, "src/gen/bundle.js", "size_limit", Some("javascript")),
                skipped_row(
                    22,
                    "src/vendor/lib.min.js",
                    "size_limit",
                    Some("javascript"),
                ),
                skipped_row(23, "docs/legacy.xslt", "grammar_unavailable", Some("xslt")),
            ],
            tests_census: Some(TestCensus {
                nodes: 3,
                languages: vec![TestCensusEntry {
                    language: "typescript".to_owned(),
                    nodes: 3,
                }],
            }),
            ..Self::clean()
        }
    }
}

/// The census the store would return for these rows.
///
/// `files.rs` folds `not_indexed` in — a skipped path is one more file of its language that failed —
/// and orders languages ascending with the unattributed bucket last. That identity is what the
/// handler's `skipped` (and its `complete: false`) arithmetic depends on, so the fake reproduces the
/// store's answer rather than handing the handler numbers that could not come out of it.
fn census_for(files: &[IndexedFileRow], skipped: &[SkippedPathRow]) -> Vec<LanguageCensus> {
    fn slot(
        buckets: &mut Vec<(Option<String>, LanguageCensus)>,
        language: Option<String>,
    ) -> usize {
        match buckets.iter().position(|(key, _)| *key == language) {
            Some(index) => index,
            None => {
                buckets.push((
                    language.clone(),
                    LanguageCensus {
                        language,
                        files: 0,
                        parsed: 0,
                        failed: 0,
                    },
                ));
                buckets.len() - 1
            }
        }
    }

    let mut buckets: Vec<(Option<String>, LanguageCensus)> = Vec::new();
    for file in files {
        let index = slot(&mut buckets, Some(file.language.clone()));
        buckets[index].1.files += 1;
        if file.parse_status == "failed" {
            buckets[index].1.failed += 1;
        } else {
            buckets[index].1.parsed += 1;
        }
    }
    for row in skipped {
        let index = slot(&mut buckets, row.language.clone());
        buckets[index].1.files += 1;
        buckets[index].1.failed += 1;
    }
    // Ascending by language, the unattributed bucket last: the store's own comparator.
    buckets.sort_by(|(left, _), (right, _)| match (left, right) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(left), Some(right)) => left.cmp(right),
    });
    buckets.into_iter().map(|(_, row)| row).collect()
}

fn node_row(id: i64, file: &IndexedFileRow) -> NodeRow {
    NodeRow {
        id,
        file_id: file.id,
        path: file.path.clone(),
        parent_id: None,
        kind: "function".to_owned(),
        subkind: None,
        name: format!("symbol_{id}"),
        qn: format!("{}::symbol_{id}", file.path),
        start_line: 1,
        end_line: 2,
        signature: None,
        docstring: None,
        is_test: false,
        props: "{}".to_owned(),
    }
}

/// A hand-written reader; every method present, and only `#shared/*` types in its signatures.
struct FakeReader {
    fixture: Fixture,
    repo_root: String,
}

impl FakeReader {
    fn new(fixture: Fixture, repo_root: &Path) -> Self {
        Self {
            fixture,
            repo_root: repo_root.display().to_string(),
        }
    }

    /// `not_indexed_all()` orders by `path, reason`; the fake has to reproduce it to be a reader.
    fn sorted_skipped(&self) -> Vec<SkippedPathRow> {
        let mut rows = self.fixture.not_indexed.clone();
        rows.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.reason.cmp(&right.reason))
        });
        rows
    }
}

impl GraphReader for FakeReader {
    fn find_by_qn(&self, _qn: &str) -> Result<Option<NodeRow>, sangkar_core::errors::AppError> {
        Ok(None)
    }

    fn find_by_name(
        &self,
        _name: &str,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NodeRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn members(&self, _parent_id: i64) -> Result<Vec<NodeRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, sangkar_core::errors::AppError> {
        let Some(file) = self.fixture.files.iter().find(|file| file.id == file_id) else {
            return Ok(Vec::new());
        };
        let count = self
            .fixture
            .symbols
            .iter()
            .find(|(id, _)| *id == file_id)
            .map_or(0, |(_, count)| *count);
        Ok((1..=count as i64).map(|id| node_row(id, file)).collect())
    }

    fn inbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn outbound(
        &self,
        _node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn metrics_for(
        &self,
        _node_id: i64,
    ) -> Result<Option<NodeMetrics>, sangkar_core::errors::AppError> {
        Ok(None)
    }

    fn rows_for_paths(
        &self,
        prefixes: &[String],
    ) -> Result<PathCoverage, sangkar_core::errors::AppError> {
        let under = |path: &str| prefixes.iter().any(|prefix| is_under(path, prefix));
        let files: Vec<IndexedFileRow> = self
            .fixture
            .files
            .iter()
            .filter(|file| under(&file.path))
            .cloned()
            .collect();
        let not_indexed: Vec<SkippedPathRow> = self
            .fixture
            .not_indexed
            .iter()
            .filter(|row| under(&row.path))
            .cloned()
            .collect();
        Ok(PathCoverage {
            indexed: files.len(),
            skipped: not_indexed.len(),
            files,
            not_indexed,
        })
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, sangkar_core::errors::AppError> {
        Ok(self.sorted_skipped())
    }

    fn not_indexed_count(&self) -> Result<i64, sangkar_core::errors::AppError> {
        Ok(self.fixture.not_indexed.len() as i64)
    }

    fn unresolved_for_files(
        &self,
        file_ids: &[i64],
    ) -> Result<Vec<UnresolvedRef>, sangkar_core::errors::AppError> {
        Ok(self
            .fixture
            .unresolved
            .iter()
            .filter(|row| file_ids.contains(&row.file_id))
            .cloned()
            .collect())
    }

    fn file_by_path(
        &self,
        path: &str,
    ) -> Result<Option<IndexedFileRow>, sangkar_core::errors::AppError> {
        Ok(self
            .fixture
            .files
            .iter()
            .find(|file| file.path == path)
            .cloned())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, sangkar_core::errors::AppError> {
        Ok(self.fixture.files.clone())
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, sangkar_core::errors::AppError> {
        Ok(census_for(&self.fixture.files, &self.fixture.not_indexed))
    }

    fn tests_census(&self) -> Result<TestCensus, sangkar_core::errors::AppError> {
        Ok(self.fixture.tests_census.clone().unwrap_or(TestCensus {
            nodes: 0,
            languages: Vec::new(),
        }))
    }

    fn meta_for(&self) -> Result<GraphMeta, sangkar_core::errors::AppError> {
        Ok(GraphMeta {
            schema_version: Some(1),
            generation: self.fixture.generation.clone(),
            generation_seq: Some(7),
            indexer_state: self.fixture.indexer_state.clone(),
            consecutive_failures: self.fixture.consecutive_failures,
            project_name: self.fixture.project_name.clone(),
            project_root: match &self.fixture.project_root {
                None => Some(self.repo_root.clone()),
                Some(recorded) => recorded.clone(),
            },
            project_branch: self.fixture.project_branch.clone(),
            project_remote: self.fixture.project_remote.clone(),
            embed_dim: None,
        })
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, sangkar_core::errors::AppError> {
        Ok(if self.fixture.generation.is_none() {
            None
        } else {
            self.fixture.generation_row.clone()
        })
    }

    fn removed_symbols_for(
        &self,
        _since_generation: &str,
    ) -> Result<Vec<TombstoneRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn entry_points(
        &self,
        _limit: usize,
    ) -> Result<Vec<EntryPointRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn test_gaps(
        &self,
        _ids: &[i64],
    ) -> Result<Vec<sangkar_store::read::reader::TestGapRow>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }

    fn read_node_fts(
        &self,
        _match_query: &str,
        _limit: usize,
    ) -> Result<Vec<FtsHit>, sangkar_core::errors::AppError> {
        Ok(Vec::new())
    }
}

/// The half-open prefix test, from the workspace's own module rather than a copy of it.
fn is_under(path: &str, prefix: &str) -> bool {
    sangkar_core::paths::is_under_prefix(path, prefix)
}

/// A context over the fake reader, with the recorded database size the payload reports.
fn fake_context(fixture: Fixture, dirs: &Dirs) -> ToolContext {
    let reader: Rc<dyn GraphReader> = Rc::new(FakeReader::new(fixture, &dirs.repo));
    context_with(reader, dirs, 2048, test_cursor())
}

// -------------------------------------------------------------------------------------------
// Assertions shared by the cases
// -------------------------------------------------------------------------------------------

/// The payload keys `TOOLS-SPEC.md` §2 documents, in that order.
const PAYLOAD_KEYS: [&str; 7] = [
    "project",
    "config",
    "graph",
    "unresolved",
    "languages",
    "paths",
    "health",
];

fn keys_of(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

fn payload_of(envelope: &StatusEnvelope) -> &Value {
    envelope
        .payload
        .as_ref()
        .expect("an ok envelope carries a payload")
}

/// Narrows to the payload, so a wrong status fails with the summary that explains it.
fn expect_ok(envelope: StatusEnvelope) -> Value {
    assert_eq!(
        envelope.status,
        Status::Ok,
        "expected an ok envelope, got {}: {}",
        envelope.status,
        envelope.summary
    );
    assert!(envelope.error.is_none(), "an ok envelope carries no error");
    payload_of(&envelope).clone()
}

/// One `<list>_total` as a number, so a missing or `null` total fails instead of reading as zero.
fn total_of(envelope: &StatusEnvelope, key: &str) -> u64 {
    envelope
        .totals
        .get(key)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("totals.{key} is not a number"))
}

// -------------------------------------------------------------------------------------------
// Complete coverage: the documented payload, field by field
// -------------------------------------------------------------------------------------------

#[test]
fn answers_ok_with_all_nine_fields_and_the_documented_payload() {
    let dirs = Dirs::new("clean");
    let envelope = handle(
        &fake_context(Fixture::clean(), &dirs),
        &StatusArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());
    let _ = &envelope;

    assert!(envelope.error.is_none());
    assert_eq!(
        envelope.summary,
        "2 files indexed, generation g-7f3a91. 1 of 1 requested paths indexed."
    );
    assert!(!envelope.truncated);
    assert_eq!(envelope.truncation_reason, None);
    assert_eq!(envelope.next, None);
    // Complete coverage, no unresolved call sites and a fresh generation is the only state that may
    // claim an exact total.
    assert_eq!(envelope.read_status.generation, GENERATION);
    // `sangkar_store`'s `GraphMeta` carries no timestamp; the served publish's is the record.
    assert_eq!(envelope.read_status.indexed_at, PUBLISHED_AT);
    assert!(!envelope.read_status.stale);
    // A fresh answer carries no `stale_reason` key at all, and its verdict is dated by the domain
    // module's own clock — the one field of this block a test cannot pin to a fixture.
    assert_eq!(envelope.read_status.stale_reason, None);
    assert!(
        envelope
            .read_status
            .stale_checked_at
            .as_deref()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "{:?}",
        envelope.read_status.stale_checked_at
    );
    assert_eq!(
        serde_json::to_value(&envelope.coverage).expect("a coverage"),
        serde_json::json!({
            "complete": true,
            "indexed": 2,
            "skipped": 0,
            "failed": 0,
            "unresolved_edges": 0,
            "not_indexed": [],
            // The graph indexed no test symbol, which the census says with zeros rather than by
            // omitting the key: a reader never has to guess whether this build reports tests.
            "tests": { "nodes": 0, "languages": [] },
        })
    );
    assert_eq!(
        envelope.totals.as_map().clone(),
        serde_json::json!({
            "total_relation": "eq",
            "paths_total": 1,
            "not_indexed_total": 0,
        })
        .as_object()
        .expect("an object")
        .clone()
    );

    assert_eq!(
        payload["project"],
        serde_json::json!({
            "name": DEFAULT_PROJECT_NAME,
            "root": dirs.repo_text(),
            "branch": DEFAULT_PROJECT_BRANCH,
            "remote": DEFAULT_PROJECT_REMOTE,
            "commit": BASE_COMMIT,
            // Derived from `config.data_dir`: the reader deliberately exposes no database path.
            "db_path": dirs.db_path_text(),
        })
    );
    // Which file the three repository settings were read from; this fixture's config carries the
    // machine defaults, because nothing here consults a tree.
    assert_eq!(payload["config"], serde_json::json!({ "source": "global" }));
    assert_eq!(
        payload["graph"],
        serde_json::json!({
            "nodes": 13_955,
            "edges": 130_044,
            "size_bytes": 2048,
            "db_path": dirs.db_path_text(),
        })
    );
    assert_eq!(
        payload["languages"],
        serde_json::json!([
            { "language": "python", "files": 1, "parsed": 1, "failed": 0 },
            { "language": "typescript", "files": 1, "parsed": 1, "failed": 0 },
        ])
    );
    assert_eq!(
        payload["paths"],
        serde_json::json!([{
            "path": "src/a.ts",
            "indexed": true,
            "sha256": digest(1),
            "symbols": 2,
        }])
    );
    // The three fields the fixture decides, and the one it cannot: `statement_cache_entries` is the
    // store's process-wide count of live handles, so this suite's real-database test keeps it above
    // zero and the parity README lists it as per-run. Its source is proved there.
    assert_eq!(payload["health"]["indexer"], serde_json::json!("idle"));
    assert_eq!(
        payload["health"]["last_publish_ms"],
        serde_json::json!(41_200)
    );
    assert_eq!(
        payload["health"]["consecutive_failures"],
        serde_json::json!(0)
    );
    assert!(
        payload["health"]["statement_cache_entries"]
            .as_u64()
            .is_some(),
        "{:?}",
        payload["health"]["statement_cache_entries"]
    );
    assert_eq!(
        keys_of(&payload["health"]),
        vec![
            "indexer",
            "last_publish_ms",
            "consecutive_failures",
            "statement_cache_entries",
        ]
    );
    assert_eq!(keys_of(&payload), PAYLOAD_KEYS.to_vec());
}

#[test]
fn a_request_with_no_paths_answers_about_the_whole_repository() {
    let dirs = Dirs::new("whole");
    let envelope = handle(
        &fake_context(Fixture::clean(), &dirs),
        &StatusArgs::default(),
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(
        envelope.summary,
        "2 files indexed, generation g-7f3a91. Whole repository."
    );
    assert_eq!(payload["paths"], serde_json::json!([]));
    assert_eq!(total_of(&envelope, "paths_total"), 0);
    assert_eq!(total_of(&envelope, "not_indexed_total"), 0);
    assert!(!envelope.truncated);
}

#[test]
fn size_bytes_is_the_size_recorded_at_open() {
    // Issue #13's reproduction, in miniature: the served generation was 172032 bytes while the file
    // on disk was 720896. The recorded value wins over anything the context could re-measure.
    let dirs = Dirs::new("recorded-size");
    let reader: Rc<dyn GraphReader> = Rc::new(FakeReader::new(Fixture::clean(), &dirs.repo));
    let envelope = handle(
        &context_with(reader, &dirs, 172_032, test_cursor()),
        &StatusArgs::default(),
    );
    let payload = expect_ok(envelope);

    assert_eq!(payload["graph"]["size_bytes"], serde_json::json!(172_032));
    assert_eq!(
        payload["graph"]["db_path"],
        serde_json::json!(dirs.db_path_text())
    );
}

#[test]
fn a_directory_request_expands_to_the_files_and_skips_under_it() {
    let dirs = Dirs::new("directory");
    let envelope = handle(
        &fake_context(Fixture::messy(), &dirs),
        &StatusArgs {
            paths: Some(vec!["docs".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());

    // Every entry is a real row: a prefix has no digest and no symbol count of its own, so it is
    // answered at file granularity instead of pretending to be one file.
    assert_eq!(
        payload["paths"],
        serde_json::json!([
            { "path": "docs/legacy.xslt", "indexed": false, "reason": "grammar_unavailable" },
            { "path": "docs/older.xslt", "indexed": false, "reason": "grammar_unavailable" },
        ])
    );
    assert_eq!(
        envelope.summary,
        // Two of the three skipped paths sit under `docs`; the third is named only as the repo-wide
        // total, so the number in front of "skipped" is always the one the list behind it backs.
        "3 files indexed, generation g-7f3a91, 2 skipped of 3 repo-wide, 1 failed to parse. \
         0 of 1 requested paths indexed."
    );
    assert_eq!(envelope.coverage.skipped, 2);
    assert_eq!(envelope.coverage.not_indexed.len(), 2);
    assert_eq!(total_of(&envelope, "not_indexed_total"), 3);
}

// -------------------------------------------------------------------------------------------
// Repository identity
// -------------------------------------------------------------------------------------------

#[test]
fn a_store_published_before_identity_existed_still_answers_with_nulls() {
    let dirs = Dirs::new("legacy-identity");
    let fixture = Fixture {
        project_name: None,
        project_root: Some(None),
        project_branch: None,
        project_remote: None,
        ..Fixture::clean()
    };
    let envelope = handle(&fake_context(fixture, &dirs), &StatusArgs::default());
    let payload = expect_ok(envelope.clone());

    // An unrecorded root is not a refusal and not a `root_mismatch`: the graph claims no tree, so
    // there is nothing to contradict. The answer keeps every other field.
    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(
        payload["project"],
        serde_json::json!({
            "name": null,
            "root": null,
            "branch": null,
            "remote": null,
            "commit": BASE_COMMIT,
            "db_path": dirs.db_path_text(),
        })
    );
    // A payload must spell the absent fact as `null`, and an absent key would vanish from the JSON.
    assert_eq!(
        keys_of(&payload["project"]),
        vec!["name", "root", "branch", "remote", "commit", "db_path"]
    );
    assert_eq!(payload["graph"]["nodes"], serde_json::json!(13_955));
}

// -------------------------------------------------------------------------------------------
// The unresolved census
// -------------------------------------------------------------------------------------------

#[test]
fn the_census_names_every_bucket_and_sums_to_the_count_it_explains() {
    let dirs = Dirs::new("bucketed");
    let envelope = handle(
        &fake_context(Fixture::bucketed(), &dirs),
        &StatusArgs::default(),
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(
        payload["unresolved"]["buckets"],
        serde_json::json!({
            "builtin_stdlib": 1,
            "external_package": 1,
            "internal_module": 1,
            "dynamic_property_call": 1,
            "receiver_type_unknown": 1,
            "ambiguous": 1,
        })
    );
    // `material` is the part of the count that could still be user-defined code: an ambiguous row
    // (two candidates) and a receiver the indexer could not type. Everything else is stdlib, a
    // package that is not in the graph, a broken relative import, or a call with no target.
    assert_eq!(payload["unresolved"]["material"], serde_json::json!(2));
    assert_eq!(payload["unresolved"]["total"], serde_json::json!(6));
    assert_eq!(
        payload["unresolved"]["total"],
        serde_json::json!(envelope.coverage.unresolved_edges)
    );
    let summed: u64 = payload["unresolved"]["buckets"]
        .as_object()
        .expect("an object")
        .values()
        .map(|count| count.as_u64().expect("a count"))
        .sum();
    assert_eq!(summed, 6);
    // The six keys are the taxonomy's, in its order: a bucket added there must appear here.
    assert_eq!(
        keys_of(&payload["unresolved"]["buckets"]),
        UNRESOLVED_BUCKETS.to_vec()
    );
}

#[test]
fn a_graph_with_no_unresolved_rows_reports_a_full_set_of_zeros() {
    let dirs = Dirs::new("no-unresolved");
    let envelope = handle(
        &fake_context(Fixture::clean(), &dirs),
        &StatusArgs::default(),
    );
    let payload = expect_ok(envelope.clone());

    // Every bucket is present at zero rather than the key being absent, so a reader of the payload
    // never has to tell "no rows" apart from "this build does not report buckets".
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
                "ambiguous": 0,
            },
        })
    );
    assert_eq!(envelope.coverage.unresolved_edges, 0);
    // Zero unresolved rows is part of what lets this answer claim an exact total.
    assert_eq!(
        envelope.totals.relation(),
        sangkar_core::envelope::TotalRelation::Eq
    );
}

#[test]
fn the_census_counts_the_rows_the_envelope_counts_whichever_paths_were_requested() {
    // `coverage.unresolved_edges` is repo-wide while `paths` scopes the entries by design; the census
    // follows the coverage count it explains, so the two numbers can never describe two populations.
    let dirs = Dirs::new("census-scope");
    for paths in [
        Vec::new(),
        vec!["src/a.ts".to_owned()],
        vec!["docs/legacy.xslt".to_owned()],
    ] {
        let envelope = handle(
            &fake_context(Fixture::messy(), &dirs),
            &StatusArgs {
                paths: Some(paths.clone()),
                budget: None,
            },
        );
        let payload = payload_of(&envelope);
        assert_eq!(
            payload["unresolved"]["total"],
            serde_json::json!(envelope.coverage.unresolved_edges),
            "paths {paths:?}"
        );
        let summed: u64 = payload["unresolved"]["buckets"]
            .as_object()
            .expect("an object")
            .values()
            .map(|count| count.as_u64().expect("a count"))
            .sum();
        assert_eq!(
            summed, envelope.coverage.unresolved_edges,
            "paths {paths:?}"
        );
    }
}

// -------------------------------------------------------------------------------------------
// Paths that were never parsed
// -------------------------------------------------------------------------------------------

#[test]
fn a_skipped_path_comes_back_with_its_reason_not_as_a_zero() {
    let dirs = Dirs::new("skipped");
    let envelope = handle(
        &fake_context(Fixture::messy(), &dirs),
        &StatusArgs {
            paths: Some(vec!["docs/legacy.xslt".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(
        payload["paths"],
        serde_json::json!([
            { "path": "docs/legacy.xslt", "indexed": false, "reason": "grammar_unavailable" },
        ])
    );
    // The count is the scope's own: `skipped` and `not_indexed` are the same row set read once, so a
    // scoped answer never pairs a skip count with an empty list. The repository's total — all three
    // skipped paths — travels in `totals.not_indexed_total`.
    assert_eq!(
        serde_json::to_value(&envelope.coverage).expect("a coverage"),
        serde_json::json!({
            "complete": false,
            "indexed": 3,
            "skipped": 1,
            "failed": 1,
            "unresolved_edges": 2,
            "not_indexed": [{ "path": "docs/legacy.xslt", "reason": "grammar_unavailable" }],
            "tests": { "nodes": 0, "languages": [] },
        })
    );
    assert_eq!(total_of(&envelope, "not_indexed_total"), 3);
    assert_eq!(
        envelope.totals.relation(),
        sangkar_core::envelope::TotalRelation::Gte
    );
}

#[test]
fn the_census_reports_a_language_that_parsed_nothing_with_a_note() {
    let dirs = Dirs::new("grammar-note");
    let envelope = handle(
        &fake_context(Fixture::messy(), &dirs),
        &StatusArgs {
            paths: Some(vec!["docs/legacy.xslt".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());
    let languages = payload["languages"].as_array().expect("an array");

    assert_eq!(
        languages.iter().find(|row| row["language"] == "xslt"),
        Some(&serde_json::json!({
            "language": "xslt",
            "files": 2,
            "parsed": 0,
            "failed": 2,
            "note": "grammar_unavailable",
        }))
    );
    // The unattributed bucket keeps `null` and gets no note: the note names a language, and a
    // skipped path whose grammar could not be identified has no name to attach it to.
    assert_eq!(
        languages.iter().find(|row| row["language"].is_null()),
        Some(&serde_json::json!({
            "language": null,
            "files": 1,
            "parsed": 0,
            "failed": 1,
        }))
    );
    assert_eq!(
        languages.iter().find(|row| row["language"] == "python"),
        Some(&serde_json::json!({
            "language": "python",
            "files": 2,
            "parsed": 1,
            "failed": 1,
        }))
    );

    // The arithmetic the handler's coverage rests on. The census is repo-wide, so its skipped half is
    // `totals.not_indexed_total`, never the scope's `coverage.skipped`.
    let census_files: u64 = languages
        .iter()
        .map(|row| row["files"].as_u64().expect("a count"))
        .sum();
    let census_failed: u64 = languages
        .iter()
        .map(|row| row["failed"].as_u64().expect("a count"))
        .sum();
    let not_indexed_total = total_of(&envelope, "not_indexed_total");
    assert_eq!(not_indexed_total, 3);
    assert_eq!(census_files, envelope.coverage.indexed + not_indexed_total);
    assert_eq!(census_failed, envelope.coverage.failed + not_indexed_total);
}

#[test]
fn a_path_in_neither_file_nor_not_indexed_is_reported_as_not_indexed_unknown() {
    let dirs = Dirs::new("unknown-path");
    let envelope = handle(
        &fake_context(Fixture::messy(), &dirs),
        &StatusArgs {
            paths: Some(vec!["src/missing.ts".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(
        payload["paths"],
        serde_json::json!([
            { "path": "src/missing.ts", "indexed": false, "reason": "not_indexed_unknown" },
        ])
    );
    assert!(envelope.summary.contains("0 of 1 requested paths indexed"));
    assert!(!envelope.coverage.complete);
    assert_eq!(
        envelope.totals.relation(),
        sangkar_core::envelope::TotalRelation::Gte
    );
}

// -------------------------------------------------------------------------------------------
// The scoped skip count and the repository total
// -------------------------------------------------------------------------------------------

#[test]
fn a_scoped_request_counts_its_own_skipped_paths_and_quotes_the_graph_total() {
    let dirs = Dirs::new("scoped");
    let envelope = handle(
        &fake_context(Fixture::scoped(), &dirs),
        &StatusArgs {
            paths: Some(vec!["src".to_owned()]),
            budget: None,
        },
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(envelope.coverage.skipped, 2);
    assert_eq!(
        envelope.coverage.not_indexed,
        vec![
            sangkar_core::envelope::NotIndexedEntry::new("src/gen/bundle.js", "size_limit"),
            sangkar_core::envelope::NotIndexedEntry::new("src/vendor/lib.min.js", "size_limit"),
        ]
    );
    // The invariant the payload has to keep: a positive skip count always has a list behind it.
    assert_eq!(
        envelope.coverage.skipped as usize,
        envelope.coverage.not_indexed.len()
    );
    assert!(!envelope.coverage.complete);
    // Repo-wide, and larger than this scope skipped: the number the scope's list cannot back.
    assert_eq!(total_of(&envelope, "not_indexed_total"), 3);
    assert!(envelope.summary.contains("2 skipped of 3 repo-wide"));
    // `coverage.tests` is the reader's census passed through unchanged.
    assert_eq!(
        payload["tests"],
        Value::Null,
        "the test census lives on the coverage, not the payload"
    );
    assert_eq!(
        envelope.coverage.tests,
        Some(TestCensus {
            nodes: 3,
            languages: vec![TestCensusEntry {
                language: "typescript".to_owned(),
                nodes: 3,
            }],
        })
    );
}

#[test]
fn a_request_with_no_paths_lists_every_skipped_path_the_graph_holds() {
    let dirs = Dirs::new("no-paths-skips");
    let envelope = handle(
        &fake_context(Fixture::scoped(), &dirs),
        &StatusArgs::default(),
    );
    let payload = expect_ok(envelope.clone());

    assert_eq!(payload["paths"], serde_json::json!([]));
    // Assembling the scope from `not_indexed_all()` is what keeps this call from answering
    // `not_indexed: []` about a repository whose skip list is not empty.
    assert_eq!(envelope.coverage.skipped, 3);
    assert_eq!(
        envelope.coverage.not_indexed,
        vec![
            sangkar_core::envelope::NotIndexedEntry::new("docs/legacy.xslt", "grammar_unavailable"),
            sangkar_core::envelope::NotIndexedEntry::new("src/gen/bundle.js", "size_limit"),
            sangkar_core::envelope::NotIndexedEntry::new("src/vendor/lib.min.js", "size_limit"),
        ]
    );
    assert!(!envelope.coverage.complete);
    // One scope, one count: the graph total and the scope's count are the same measurement here.
    assert_eq!(
        envelope.coverage.skipped,
        total_of(&envelope, "not_indexed_total")
    );
    assert!(envelope.summary.contains("3 skipped"));
    assert!(!envelope.summary.contains("repo-wide"));
    assert!(!envelope.truncated);
}

// -------------------------------------------------------------------------------------------
// Freshness and health
// -------------------------------------------------------------------------------------------

#[test]
fn a_file_row_from_an_earlier_generation_makes_the_answer_stale_dated() {
    let dirs = Dirs::new("stale-generation");
    let fixture = Fixture {
        files: vec![
            file_row(1, "src/a.ts", "typescript", "ok", GENERATION),
            file_row(2, "src/b.py", "python", "ok", OLDER_GENERATION),
        ],
        ..Fixture::clean()
    };
    let envelope = handle(
        &fake_context(fixture, &dirs),
        &StatusArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: None,
        },
    );

    assert!(envelope.read_status.stale);
    assert_eq!(
        envelope.read_status.stale_reason.as_deref(),
        Some("1 of 2 indexed files were written by an earlier generation")
    );
    // A dated claim, not an absolute one: the verdict carries the clock it ran under.
    assert!(envelope.read_status.stale_checked_at.is_some());
    // Stale is a warning, never a failure: the payload is still returned, flagged.
    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(
        envelope.totals.relation(),
        sangkar_core::envelope::TotalRelation::Gte
    );
    assert!(
        envelope
            .summary
            .contains("1 written by an earlier generation")
    );
}

#[test]
fn health_reports_an_unrecorded_indexer_state_and_a_failure_counter() {
    let dirs = Dirs::new("health");
    let fixture = Fixture {
        indexer_state: None,
        consecutive_failures: 3,
        ..Fixture::clean()
    };
    let payload = expect_ok(handle(
        &fake_context(fixture, &dirs),
        &StatusArgs::default(),
    ));

    assert_eq!(payload["health"]["indexer"], Value::Null);
    assert_eq!(
        payload["health"]["last_publish_ms"],
        serde_json::json!(41_200)
    );
    assert_eq!(
        payload["health"]["consecutive_failures"],
        serde_json::json!(3)
    );
    // `statement_cache_entries` is the store's process-wide count, so a fake reader cannot choose
    // it: this suite's real-handle tests keep a live cache, and the parity README lists the field as
    // per-run. Its source is proved in `a_publish_with_recorded_project_meta_rows_…` below.
    assert!(
        payload["health"]["statement_cache_entries"]
            .as_u64()
            .is_some(),
        "{:?}",
        payload["health"]["statement_cache_entries"]
    );
}

// -------------------------------------------------------------------------------------------
// No published generation
// -------------------------------------------------------------------------------------------

#[test]
fn answers_bootstrapping_with_a_retry_interval_and_no_payload() {
    let dirs = Dirs::new("bootstrapping");
    let fixture = Fixture {
        files: Vec::new(),
        generation: None,
        generation_row: None,
        ..Fixture::clean()
    };
    let envelope = handle(
        &fake_context(fixture, &dirs),
        &StatusArgs {
            paths: Some(vec!["src".to_owned()]),
            budget: None,
        },
    );

    assert_eq!(envelope.status, Status::Bootstrapping);
    assert_eq!(envelope.payload, None);
    let error = envelope.error.as_ref().expect("a bootstrapping error");
    assert_eq!(error.code, "bootstrapping");
    assert_eq!(error.retry_after_ms, Some(STATUS_RETRY_AFTER_MS));
    assert_eq!(
        error.message,
        "the index has not published a generation yet"
    );
    // No generation id is invented: the empty one is the truthful value for "nothing published".
    assert_eq!(envelope.read_status.generation, "");
    assert!(envelope.read_status.stale);
    assert_eq!(envelope.coverage.indexed, 0);
    assert!(!envelope.coverage.complete);
    // The injected clock dates this answer, because this module builds its own read_status here.
    assert_eq!(envelope.read_status.stale_checked_at.as_deref(), Some(NOW));
    // `payload` is `null`, not absent: the two statuses that carry none still carry the key.
    let wire = serde_json::to_value(&envelope).expect("an envelope");
    assert_eq!(wire["payload"], Value::Null);
    assert!(wire.get("payload").is_some());
}

// -------------------------------------------------------------------------------------------
// Budget
// -------------------------------------------------------------------------------------------

/// The tightest budget that still cuts and still answers: found, not guessed, because below it the
/// report's own required fields are the floor and `status` refuses instead of overrunning.
fn tightest_cutting_budget(ctx: &ToolContext, args: &StatusArgs) -> u64 {
    let whole = serde_json::to_value(handle(ctx, args))
        .expect("an envelope")
        .to_string()
        .len() as u64;
    let whole_tokens = whole.div_ceil(4);
    let mut budget = whole_tokens.saturating_sub(1);
    while budget > 1 {
        let envelope = handle(
            ctx,
            &StatusArgs {
                budget: Some(i64::try_from(budget).expect("a small budget")),
                ..args.clone()
            },
        );
        if envelope.status == Status::Ok
            && envelope.truncated
            && envelope.truncation_reason == Some(TruncationReason::Budget)
        {
            return budget;
        }
        budget -= 1;
    }
    panic!("no budget between 2 and {whole_tokens} produced a cut");
}

#[test]
fn a_budget_that_cuts_the_no_paths_skip_list_keeps_every_count_exact() {
    let dirs = Dirs::new("cut-skips");
    let fixture = Fixture {
        files: vec![file_row(1, "src/a.ts", "typescript", "ok", GENERATION)],
        not_indexed: (0..60)
            .map(|index| {
                skipped_row(
                    100 + index,
                    &format!("assets/binary_{index:02}.png"),
                    "binary",
                    None,
                )
            })
            .collect(),
        ..Fixture::clean()
    };
    let ctx = fake_context(fixture, &dirs);
    let budget = tightest_cutting_budget(&ctx, &StatusArgs::default());
    let envelope = handle(
        &ctx,
        &StatusArgs {
            budget: Some(i64::try_from(budget).expect("a small budget")),
            ..StatusArgs::default()
        },
    );

    assert_eq!(envelope.status, Status::Ok);
    assert!(envelope.truncated);
    assert_eq!(envelope.truncation_reason, Some(TruncationReason::Budget));
    assert!(envelope.coverage.not_indexed.len() < 60);
    // Truncation cuts the list, never the arithmetic: the counts stay the numbers that were counted.
    assert_eq!(envelope.coverage.skipped, 60);
    assert_eq!(total_of(&envelope, "not_indexed_total"), 60);
}

#[test]
fn the_default_budget_is_serviceable_with_a_repository_stamped_cursor() {
    // The composition root binds the cursor to the repository's canonical path, which puts that path
    // in every minted token — and `emit` fills against a reserved placeholder, so a token longer than
    // the reserve spends the difference after the cut is decided. With the placeholder short of the
    // real token this call answered `budget_too_large` at its own default; `budget.rs`'s 384-byte
    // reserve is what fixes it, and this is the tool-level guard on it.
    let dirs = Dirs::new("stamped-cursor");
    let fixture = Fixture {
        files: vec![file_row(1, "src/a.ts", "typescript", "ok", GENERATION)],
        not_indexed: (0..120)
            .map(|index| {
                skipped_row(
                    200 + index,
                    &format!("assets/generated/binary_{index:03}.png"),
                    "binary",
                    None,
                )
            })
            .collect(),
        ..Fixture::clean()
    };
    let reader: Rc<dyn GraphReader> = Rc::new(FakeReader::new(fixture, &dirs.repo));
    let cursor = Cursor::new(
        CURSOR_KEY.as_bytes(),
        Some("/home/ljosalfar-server/Documents/researchs/codegraph-mcp".to_owned()),
    )
    .expect("a test cursor key");
    let ctx = context_with(reader, &dirs, 2048, cursor);

    let envelope = handle(&ctx, &StatusArgs::default());

    assert_eq!(envelope.status, Status::Ok);
    assert!(envelope.truncated);
    assert!(envelope.next.is_some());
    let measured = serde_json::to_value(&envelope)
        .expect("an envelope")
        .to_string()
        .len() as u64;
    assert!(
        measured <= STATUS_DEFAULT_TOKENS * 4,
        "{measured} bytes is over the default budget"
    );
    // The cut still reports what it left out.
    assert!(envelope.coverage.not_indexed.len() < 120);
    assert_eq!(envelope.coverage.skipped, 120);
}

#[test]
fn a_cut_payload_keeps_every_count_and_mints_a_resumable_next() {
    let dirs = Dirs::new("cut-paths");
    let files: Vec<IndexedFileRow> = (0..40)
        .map(|index| {
            file_row(
                index + 1,
                &format!("src/file_{index:02}.ts"),
                "typescript",
                "ok",
                GENERATION,
            )
        })
        .collect();
    let paths: Vec<String> = files.iter().map(|file| file.path.clone()).collect();
    let ctx = fake_context(
        Fixture {
            files,
            symbols: Vec::new(),
            ..Fixture::clean()
        },
        &dirs,
    );
    let args = StatusArgs {
        paths: Some(paths.clone()),
        budget: None,
    };
    let budget = tightest_cutting_budget(&ctx, &args);
    let envelope = handle(
        &ctx,
        &StatusArgs {
            paths: Some(paths.clone()),
            budget: Some(i64::try_from(budget).expect("a small budget")),
        },
    );

    assert!(envelope.truncated);
    assert_eq!(envelope.truncation_reason, Some(TruncationReason::Budget));
    // The total survives the cut, so "showing N of M" stays computable.
    assert_eq!(total_of(&envelope, "paths_total"), 40);
    let payload = payload_of(&envelope);
    let kept_paths = payload["paths"].as_array().expect("an array").len();
    let kept_languages = payload["languages"].as_array().expect("an array").len();
    assert!(kept_paths < 40);

    // The token is signed over this call's canonical arguments, which is what lets `expand` refuse
    // it for a different generation or a different request.
    let expected = CursorExpectation {
        generation: GENERATION.to_owned(),
        tool: NAME.to_owned(),
        args_hash: args_hash_of(&paths, budget),
    };
    let next = envelope.next.clone().expect("a minted token");
    // `emit` drops from the largest list first: path entries, then the census line once that is the
    // biggest list left. The position is the count of everything it took out.
    let dropped = (40 - kept_paths) + usize::from(kept_languages == 0);
    assert_eq!(
        test_cursor().verify(&next, &expected),
        CursorResult::Accepted {
            position: dropped as u64
        }
    );
}

#[test]
fn a_budget_above_the_ceiling_is_refused_as_an_envelope() {
    let dirs = Dirs::new("ceiling");
    let envelope = handle(
        &fake_context(Fixture::clean(), &dirs),
        &StatusArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: Some(1_000_000),
        },
    );

    assert_eq!(envelope.status, Status::Error);
    assert_eq!(envelope.payload, None);
    assert_eq!(
        envelope.error,
        Some(sangkar_core::errors::ErrorPayload {
            code: sangkar_core::errors::ErrorCode::BUDGET_TOO_LARGE,
            message: "budget 1000000 exceeds the 32000 token ceiling".to_owned(),
            hint: Some("request at most 32000 tokens".to_owned()),
            retryable: None,
            retry_after_ms: None,
        })
    );
    assert_eq!(
        envelope.summary,
        "the requested budget could not be applied"
    );
}

#[test]
fn a_budget_below_the_floor_is_refused_with_budget_too_large() {
    let dirs = Dirs::new("floor");
    let envelope = handle(
        &fake_context(Fixture::clean(), &dirs),
        &StatusArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: Some(1),
        },
    );

    assert_eq!(envelope.status, Status::Error);
    assert_eq!(envelope.payload, None);
    let error = envelope.error.as_ref().expect("a budget refusal");
    assert_eq!(error.code, "budget_too_large");
    assert_eq!(
        error.message,
        "budget 1 is below the floor this report needs"
    );
    assert!(
        error
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("the default is 800")),
        "the hint has to say how to fix the call: {:?}",
        error.hint
    );
    assert_eq!(
        envelope.summary,
        "the requested budget could not be applied"
    );
    // A refusal carries no lists and no payload, but the nine envelope fields are all there.
    let wire = serde_json::to_value(&envelope).expect("an envelope");
    assert_eq!(wire["payload"], Value::Null);
    assert_eq!(wire["status"], "error");
}

// -------------------------------------------------------------------------------------------
// The manifest and the argument digest
// -------------------------------------------------------------------------------------------

#[test]
fn describes_the_tool_verbatim_and_declares_exactly_three_optional_arguments() {
    assert_eq!(NAME, "status");
    assert_eq!(
        DESCRIPTION,
        "Report whether the graph is current and what it covers. Call this before telling the user \
         that something does not exist, and whenever a previous answer looked incomplete. Returns \
         the index generation, how stale the working tree is, per-path coverage, and the language \
         census. Cheap — call it freely."
    );
    let names: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(names, vec!["paths", "budget", "repo"]);
    assert!(PARAMETERS.iter().all(|param| !param.required));
    assert_eq!(PARAMETERS[0].ty, ParamType::Array(&[]));
    assert_eq!(PARAMETERS[0].default, Some(ParamDefault::List(&[])));
    assert_eq!(PARAMETERS[1].ty, ParamType::Integer);
    assert_eq!(
        PARAMETERS[1].default,
        Some(ParamDefault::Integer(STATUS_DEFAULT_TOKENS as i64)),
        "the declared default and the handler's fallback are one number"
    );
    assert_eq!(PARAMETERS[1].default, Some(ParamDefault::Integer(800)));
    // No default: an absent `repo` means "unselected", not "the repository named ''".
    assert_eq!(PARAMETERS[2].default, None);
    assert_eq!(
        EXAMPLE.to_vec(),
        vec![("paths", ExampleValue::List(&["src"]))]
    );
    assert_eq!(STATUS_RETRY_AFTER_MS, 5_000);
}

#[test]
fn the_argument_digest_is_the_typescripts_own_bytes() {
    // The two goldens captured with `--paths src` carry the token `status` minted, and the token's
    // body names the digest of `JSON.stringify({budget, paths})`. Those strings are the TypeScript's
    // own answer, so they pin the canonical form's key order, compactness and digest — not just this
    // module agreeing with itself.
    assert_eq!(
        args_hash_of(&["src".to_owned()], 800),
        "sha256:ccda1335a4c976016f5aa39580281683ae61b84b4e5e47eedc5695fbe5e5c8ae"
    );
    assert_eq!(
        args_hash_of(&["src".to_owned()], 600),
        "sha256:b9cd9a5d1155d8f6b5b79817e8619bc0ca31138786579c453f9b42aab87d7aa8"
    );
    // The no-path call, from `status.json`'s arguments (its answer was untruncated, so no token
    // records this digest; the canonical form is the same one the other two prove).
    assert_eq!(
        args_hash_of(&[], 800),
        "sha256:026e1e94dce78a235816a786aafa2dd530bd2782307aad121cb555a2b8375ae0"
    );
    // Two calls that differ in either half are two pages, never one resumed page.
    assert_ne!(
        args_hash_of(&["src".to_owned()], 800),
        args_hash_of(&["src".to_owned()], 801)
    );
    assert_ne!(
        args_hash_of(&[], 800),
        args_hash_of(&["src".to_owned()], 800)
    );
}

// -------------------------------------------------------------------------------------------
// The captured golden
// -------------------------------------------------------------------------------------------

fn read_golden(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden/polyglot")
        .join(name);
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("a golden document")
}

/// The polyglot capture's graph, rebuilt from the golden's own stable facts: its fifteen files across
/// twelve languages, its twenty-six unresolved call sites (eleven of them material), its test census,
/// its identity and its generation. The database's rowids cannot be reproduced, so the fixture
/// reproduces what the payload reports instead.
fn polyglot() -> Fixture {
    let mut files = Vec::new();
    let languages: [(&str, i64); 12] = [
        ("c", 1),
        ("cpp", 1),
        ("csharp", 1),
        ("go", 1),
        ("java", 1),
        ("javascript", 1),
        ("php", 1),
        ("python", 1),
        ("ruby", 1),
        ("rust", 1),
        ("tsx", 1),
        ("typescript", 4),
    ];
    let mut id = 0_i64;
    for (language, count) in languages {
        for _ in 0..count {
            id += 1;
            files.push(file_row(
                id,
                &format!("src/{language}_{id}.src"),
                language,
                "ok",
                "g-4b56df",
            ));
        }
    }
    let mut unresolved = Vec::new();
    let mut next = 0_i64;
    let mut add = |count: i64,
                   raw_target: &str,
                   evidence_kind: Option<&str>,
                   evidence: Option<&str>,
                   fixture: &mut Vec<UnresolvedRef>| {
        for _ in 0..count {
            next += 1;
            fixture.push(unresolved_row(
                next,
                1 + (next % 15),
                "src/polyglot.src",
                raw_target,
                evidence_kind,
                evidence,
            ));
        }
    };
    // The golden's six bucket counts: 1 stdlib, 2 packages, 1 broken relative import, 11 dynamic
    // calls, 9 untyped receivers, 2 ambiguous candidates — and its `material: 11` is the last two
    // buckets' sum.
    add(
        1,
        "readFileSync",
        Some("import"),
        Some("import:node:fs"),
        &mut unresolved,
    );
    add(
        2,
        "lodash",
        Some("import"),
        Some("import:lodash"),
        &mut unresolved,
    );
    add(
        1,
        "helpers",
        Some("import"),
        Some("import:./helpers.js"),
        &mut unresolved,
    );
    add(
        11,
        "self.handler",
        Some("dynamic"),
        Some("call:self.handler"),
        &mut unresolved,
    );
    add(9, "mystery", None, None, &mut unresolved);
    add(
        2,
        "src/a.ts | src/b.ts",
        Some("ambiguous"),
        None,
        &mut unresolved,
    );

    Fixture {
        files,
        not_indexed: Vec::new(),
        symbols: Vec::new(),
        unresolved,
        generation: Some("g-4b56df".to_owned()),
        generation_row: Some(GenerationRow {
            id: "g-4b56df".to_owned(),
            seq: 7,
            published_at: "2026-11-24T04:44:41.233Z".to_owned(),
            // The archive was captured without a git identity, so the golden's own nulls stand.
            base_commit: None,
            worktree_dirty: false,
            files_indexed: 15,
            nodes: 80,
            edges: 114,
            duration_ms: 246,
            mode: "full".to_owned(),
            status: "published".to_owned(),
        }),
        indexer_state: Some("idle".to_owned()),
        consecutive_failures: 0,
        project_name: Some("polyglot".to_owned()),
        project_root: None,
        project_branch: None,
        project_remote: None,
        tests_census: Some(TestCensus {
            nodes: 3,
            languages: vec![TestCensusEntry {
                language: "typescript".to_owned(),
                nodes: 3,
            }],
        }),
    }
}

/// Replaces `path`'s value with a placeholder in both documents, so a per-run field is compared as a
/// key that exists rather than as a value only one side can predict.
fn mask(document: &mut Value, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut node = document;
    for key in parents {
        node = node.get_mut(*key).expect("the masked path exists");
    }
    let object = node.as_object_mut().expect("an object");
    assert!(object.contains_key(*last), "the masked key exists");
    object.insert((*last).to_owned(), Value::String("<per-run>".to_owned()));
}

/// Asserts that two documents carry the same keys in the same order at every depth.
fn assert_key_order(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => {
            assert_eq!(
                actual.keys().collect::<Vec<_>>(),
                expected.keys().collect::<Vec<_>>(),
                "key set and order under {path}"
            );
            for (key, value) in actual {
                assert_key_order(value, &expected[key], &format!("{path}/{key}"));
            }
        }
        (Value::Array(actual), Value::Array(expected)) => {
            assert_eq!(actual.len(), expected.len(), "array length under {path}");
            for (index, (value, golden)) in actual.iter().zip(expected).enumerate() {
                assert_key_order(value, golden, &format!("{path}/{index}"));
            }
        }
        _ => {}
    }
}

#[test]
fn the_golden_polyglot_answer_matches_key_for_key_and_value_for_value() {
    let golden = read_golden("status.json");
    assert_eq!(golden["args"], serde_json::json!([]));
    assert_eq!(golden["exit_code"], serde_json::json!(0));
    let expected: Value =
        serde_json::from_str(golden["stdout"].as_str().expect("stdout text")).expect("an envelope");

    let dirs = Dirs::new("golden");
    let envelope = handle(&fake_context(polyglot(), &dirs), &StatusArgs::default());
    let actual = serde_json::to_value(&envelope).expect("an envelope");

    // The payload's key set and order, at every depth: the parity bar, because serde's struct order
    // is the contract and a `Value` comparison alone cannot see a re-ordered object.
    assert_key_order(&actual["payload"], &expected["payload"], "payload");
    assert_eq!(keys_of(&actual["payload"]), PAYLOAD_KEYS.to_vec());

    // Every envelope field whose value is a property of the answer rather than of the run.
    for key in [
        "status",
        "summary",
        "coverage",
        "totals",
        "truncated",
        "truncation_reason",
        "next",
        "next_tools",
    ] {
        assert_eq!(actual[key], expected[key], "{key}");
    }
    assert_eq!(
        actual["read_status"]["generation"],
        expected["read_status"]["generation"]
    );
    assert_eq!(
        actual["read_status"]["indexed_at"],
        expected["read_status"]["indexed_at"]
    );
    assert_eq!(
        actual["read_status"]["stale"],
        expected["read_status"]["stale"]
    );
    // Per-run and composition-root fields, both outside the handler's half of the answer:
    // `stale_checked_at` is the freshness verdict's own wall clock (`tests/parity/README.md` lists
    // it as per-run) and `decisive` is filled once by `route()`, which this crate does not own yet.
    assert!(
        actual["read_status"]["stale_checked_at"]
            .as_str()
            .is_some_and(|stamp| stamp.ends_with('Z')),
        "the freshness verdict is dated: {:?}",
        actual["read_status"]["stale_checked_at"]
    );

    // The payload's values, once the per-run and environmental fields the parity README lists are
    // masked on both sides: absolute tree/data paths, the recorded database size, the measured
    // publish duration, and the live statement-cache count.
    let per_run: [&[&str]; 6] = [
        &["project", "root"],
        &["project", "db_path"],
        &["graph", "db_path"],
        &["graph", "size_bytes"],
        &["health", "last_publish_ms"],
        &["health", "statement_cache_entries"],
    ];
    let mut actual_payload = actual["payload"].clone();
    let mut expected_payload = expected["payload"].clone();
    for path in per_run {
        mask(&mut actual_payload, path);
        mask(&mut expected_payload, path);
    }
    assert_eq!(actual_payload, expected_payload);
}

// -------------------------------------------------------------------------------------------
// A real database: the identity a publish records, and the live statement-cache count
// -------------------------------------------------------------------------------------------

/// A context over a real `graph.db`: migrated, optionally seeded with `sql`, then reopened read-only
/// — the same order the composition root uses.
fn real_context(dirs: &Dirs, sql: &str) -> ToolContext {
    let db_path = dirs.db_path();
    {
        let mut writer = open_writer(&db_path).expect("open the writer");
        migrate(writer.connection_mut()).expect("migrate an empty database");
        if !sql.is_empty() {
            writer
                .connection()
                .execute_batch(sql)
                .expect("insert the fixture rows");
        }
        writer.close().expect("close the writer");
    }
    let size = fs::metadata(&db_path).expect("stat the database").len();
    let reader: Rc<dyn GraphReader> = Rc::new(SqliteReader::new(
        open_reader(&db_path).expect("open the reader"),
    ));
    context_with(reader, dirs, size, test_cursor())
}

#[test]
fn an_empty_database_answers_bootstrapping() {
    let dirs = Dirs::new("real-empty");
    let envelope = handle(&real_context(&dirs, ""), &StatusArgs::default());

    assert_eq!(envelope.status, Status::Bootstrapping);
    assert_eq!(envelope.payload, None);
    assert_eq!(
        envelope
            .error
            .as_ref()
            .and_then(|error| error.code.as_str().to_owned().into()),
        Some("bootstrapping".to_owned())
    );
    assert!(!envelope.coverage.complete);
}

#[test]
fn a_publish_with_recorded_project_meta_rows_fills_the_identity_and_a_live_cache_count() {
    let dirs = Dirs::new("real-identity");
    let sql = format!(
        "INSERT INTO generation (id, seq, published_at, base_commit, worktree_dirty, files_indexed, nodes, edges, duration_ms, mode, status) VALUES
           ('g-real', 3, '2026-11-17T13:50:15.000Z', 'bbdda4b9', 0, 1, 80, 114, 246, 'full', 'published');
         INSERT INTO meta (key, value) VALUES
           ('generation', 'g-real'),
           ('project_name', 'sangkar'),
           ('project_root', '{}'),
           ('project_branch', 'main'),
           ('project_remote', 'git@example.invalid:acme/sangkar.git'),
           ('indexer_state', 'idle'),
           ('consecutive_failures', '2');
         INSERT INTO file (id, path, language, sha256, size, line_count, parse_status, parse_error_ranges, indexed_generation) VALUES
           (1, 'src/a.ts', 'typescript', 'sha-a', 900, 32, 'ok', NULL, 'g-real');",
        dirs.repo_text()
    );
    let ctx = real_context(&dirs, &sql);
    let envelope = handle(&ctx, &StatusArgs::default());
    let payload = expect_ok(envelope.clone());

    assert_eq!(envelope.status, Status::Ok);
    assert_eq!(
        envelope.summary,
        "1 files indexed, generation g-real. Whole repository."
    );
    assert_eq!(
        payload["project"],
        serde_json::json!({
            "name": "sangkar",
            "root": dirs.repo_text(),
            "branch": "main",
            "remote": "git@example.invalid:acme/sangkar.git",
            "commit": "bbdda4b9",
            "db_path": dirs.db_path_text(),
        })
    );
    assert_eq!(payload["config"], serde_json::json!({ "source": "global" }));
    assert_eq!(
        payload["graph"],
        serde_json::json!({
            "nodes": 80,
            "edges": 114,
            "size_bytes": ctx.db_size_bytes,
            "db_path": dirs.db_path_text(),
        })
    );
    // The key order the contract fixes, on an answer a real database produced.
    assert_eq!(keys_of(&payload), PAYLOAD_KEYS.to_vec());
    assert_eq!(
        serde_json::to_value(&envelope.coverage).expect("a coverage"),
        serde_json::json!({
            "complete": true,
            "indexed": 1,
            "skipped": 0,
            "failed": 0,
            "unresolved_edges": 0,
            "not_indexed": [],
            "tests": { "nodes": 0, "languages": [] },
        })
    );
    // The health block is read from `meta` and the generation row, and the cache count is the live
    // handle's: the reads above prepared statements, so it cannot be zero.
    assert_eq!(payload["health"]["indexer"], serde_json::json!("idle"));
    assert_eq!(
        payload["health"]["consecutive_failures"],
        serde_json::json!(2)
    );
    assert_eq!(payload["health"]["last_publish_ms"], serde_json::json!(246));
    let entries = payload["health"]["statement_cache_entries"]
        .as_u64()
        .expect("a count");
    assert!(
        entries > 0,
        "a reader that answered prepared statements, so the process-wide cache cannot be empty"
    );

    // A file the request names is answered at file granularity, symbols included.
    let named = handle(
        &ctx,
        &StatusArgs {
            paths: Some(vec!["src/a.ts".to_owned()]),
            budget: None,
        },
    );
    let named_payload = expect_ok(named);
    assert_eq!(
        named_payload["paths"],
        serde_json::json!([
            { "path": "src/a.ts", "indexed": true, "sha256": "sha-a", "symbols": 0 },
        ])
    );
}
