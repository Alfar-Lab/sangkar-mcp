//! `expand` over a hand-written `GraphReader` and one real scratch directory: no database, no
//! network, no shared mutable state.
//!
//! Ported from `tests/tools-expand.test.ts`. A fake reader is enough for everything except the one
//! mode that must touch the working tree: `what: "source"` verifies the file's SHA-256 before
//! returning any text, so that expectation is proved against bytes on disk rather than against a
//! stubbed hash function. What is asserted is the tool's own policy — the five `what` payload
//! shapes and their key order, inference that is always disclosed, the folder address that only
//! coverage answers, a tie that never becomes an answer, a bare id that is refused rather than
//! guessed at, the `range` clamp and its inverted pair, and the drift check that keeps stale bytes
//! out of an answer.
//!
//! The fixture is one file (a class with two methods and a signatureless `dup`, a free function,
//! two same-named symbols for the tie) plus a folder whose coverage is half indexed and half
//! skipped, and a second file whose bytes no longer match the digest the index recorded.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use sangkar_core::budget::budget_floor_of;
use sangkar_core::config::load_config;
use sangkar_core::cursor::Cursor;
use sangkar_core::envelope::{Envelope, Status, TestCensus, TruncationReason};
use sangkar_core::errors::AppError;
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
use sha2::{Digest, Sha256};

use sangkar_tools::expand::handler::{args_hash_of, handle};
use sangkar_tools::expand::manifest::{
    DEFAULT_BUDGET_TOKENS, DESCRIPTION, EXAMPLE, EXPAND_WHATS, ExpandArgs, NAME, PARAMETERS,
    ParamDefault, ParamType, What,
};

const GENERATION: &str = "g-2";
const INDEXED_AT: &str = "2026-01-03T00:00:00Z";
const NOW: &str = "2026-01-03T00:05:00.000Z";
/// 37 bytes, above the cursor module's 32-byte key floor.
const KEY: &str = "expand-test-cursor-key-material-32";

const FILE_A: i64 = 1;
const FILE_FOLDER: i64 = 2;
const FILE_DRIFTED: i64 = 3;
const FILE_GONE: i64 = 4;
const FILE_LEAK: i64 = 5;

const NODE_FILE_A: i64 = 1;
const NODE_WIDGET: i64 = 10;
const NODE_RENDER: i64 = 11;
const NODE_DISPOSE: i64 = 12;
const NODE_HELPER: i64 = 13;
const NODE_DUP_METHOD: i64 = 14;
const NODE_DUP_FUNCTION: i64 = 15;
const NODE_DRIFTED: i64 = 20;
const NODE_GONE: i64 = 21;
const NODE_LEAK: i64 = 22;

const A_PATH: &str = "src/a.ts";
const FOLDER_PREFIX: &str = "src/widgets";
const DRIFTED_PATH: &str = "src/drifted.ts";
const GONE_PATH: &str = "src/gone.ts";
const LEAK_PATH: &str = "../secrets.ts";

/// The indexed version of `src/drifted.ts`; the bytes on disk are [`DRIFTED_NOW`].
const DRIFTED_INDEXED: &str = "function drifted() {\n  return 1;\n}\n";
const DRIFTED_NOW: &str = "function drifted() {\n  return 2;\n}\n";

/// Forty lines, so a window in the middle of the file is unambiguous.
fn a_source() -> String {
    let mut text = String::new();
    for number in 1..=40 {
        text.push_str(&format!("const v{number} = {number};\n"));
    }
    text
}

fn sha256_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut hex = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// One scratch tree per test, removed on drop. The name carries the process id so two test binaries
/// never share a directory; tests inside one binary differ by `name`.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        let mut root = std::env::temp_dir();
        root.push(format!("sangkar-expand-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create the scratch root");
        Self { root }
    }

    /// Writes `text` at the repo-relative `rel`, creating its parent directory.
    fn write(&self, rel: &str, text: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the file's directory");
        }
        std::fs::write(path, text).expect("write the scratch file");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The symbol half of a `qn`, and its file half.
fn path_of(qn: &str) -> &str {
    qn.split_once("::").map_or(qn, |(path, _)| path)
}

/// A node with the fixture's defaults: file 1, no parent, no signature, not a test.
fn row(id: i64, qn: &str, name: &str, kind: &str, start: i64, end: i64) -> NodeRow {
    NodeRow {
        id,
        file_id: FILE_A,
        path: path_of(qn).to_owned(),
        parent_id: None,
        kind: kind.to_owned(),
        subkind: None,
        name: name.to_owned(),
        qn: qn.to_owned(),
        start_line: start,
        end_line: end,
        signature: None,
        docstring: None,
        is_test: false,
        props: "{}".to_owned(),
    }
}

fn edge(id: i64, src_id: i64, dst_id: i64, kind: &str) -> EdgeRow {
    EdgeRow {
        id,
        src_id,
        dst_id,
        kind: kind.to_owned(),
        tier: "static".to_owned(),
        file_id: FILE_A,
        line: 7,
        local_name: String::new(),
        evidence: None,
    }
}

fn neighbour(edge: EdgeRow, node: NodeRow) -> NeighbourEdge {
    NeighbourEdge { edge, node }
}

fn metrics(node_id: i64, in_degree: i64, out_degree: i64) -> NodeMetrics {
    NodeMetrics {
        node_id,
        in_degree,
        out_degree,
        is_entry: false,
        tested_depth: Some(0),
        reachable_count: None,
    }
}

fn file_row(id: i64, path: &str, sha256: &str, line_count: i64) -> IndexedFileRow {
    IndexedFileRow {
        id,
        path: path.to_owned(),
        language: "typescript".to_owned(),
        sha256: sha256.to_owned(),
        size: 512,
        line_count,
        parse_status: "ok".to_owned(),
        parse_error_ranges: None,
        indexed_generation: GENERATION.to_owned(),
    }
}

/// Six skipped paths no requested prefix covers, plus the folder's own: this fixture's graph count
/// (7) deliberately exceeds what `rows_for_paths` can reach (1), because a fixture whose two counts
/// coincide cannot tell the repo-wide total from the scope's count.
fn outside_skips() -> Vec<SkippedPathRow> {
    [
        ("docs/legacy.xslt", "grammar_unavailable"),
        ("docs/notes.markdown", "grammar_unavailable"),
        ("scripts/build.sh", "unsupported_ext"),
        ("vendor/min.js", "size_cap"),
        ("assets/logo.svg", "unsupported_ext"),
        ("config/settings.yaml", "unsupported_ext"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (path, reason))| SkippedPathRow {
        id: 30 + index as i64,
        path: path.to_owned(),
        reason: reason.to_owned(),
        detail: None,
        language: None,
    })
    .collect()
}

/// The fake graph: the store queries this tool makes, over values a test can read back.
struct Fake {
    nodes: Vec<NodeRow>,
    metrics: Vec<NodeMetrics>,
    inbound: Vec<(i64, Vec<NeighbourEdge>)>,
    outbound: Vec<(i64, Vec<NeighbourEdge>)>,
    files: Vec<IndexedFileRow>,
    rows: Vec<(String, PathCoverage)>,
    skips: Vec<SkippedPathRow>,
    /// Unbound call sites per file; one of them is what makes a scope's relation `gte`.
    unresolved: Vec<UnresolvedRef>,
    generation: Option<String>,
    published_at: String,
}

impl Fake {
    /// `a_sha` is the digest of the file on disk and `drifted_sha` the one the index recorded; the
    /// two differ on purpose in the drift tests.
    fn new(a_sha: &str, drifted_sha: &str) -> Self {
        let widget = NodeRow {
            parent_id: Some(NODE_FILE_A),
            ..row(NODE_WIDGET, "src/a.ts::Widget", "Widget", "class", 3, 20)
        };
        let render = NodeRow {
            parent_id: Some(NODE_WIDGET),
            signature: Some("render(): void".to_owned()),
            ..row(
                NODE_RENDER,
                "src/a.ts::Widget.render",
                "render",
                "method",
                5,
                8,
            )
        };
        let dispose = NodeRow {
            parent_id: Some(NODE_WIDGET),
            signature: Some("dispose(): void".to_owned()),
            ..row(
                NODE_DISPOSE,
                "src/a.ts::Widget.dispose",
                "dispose",
                "method",
                10,
                12,
            )
        };
        let helper = NodeRow {
            parent_id: Some(NODE_FILE_A),
            ..row(
                NODE_HELPER,
                "src/a.ts::helper",
                "helper",
                "function",
                25,
                30,
            )
        };
        let dup_method = NodeRow {
            parent_id: Some(NODE_WIDGET),
            ..row(
                NODE_DUP_METHOD,
                "src/a.ts::Widget.dup",
                "dup",
                "method",
                14,
                16,
            )
        };
        let dup_function = NodeRow {
            parent_id: Some(NODE_FILE_A),
            ..row(
                NODE_DUP_FUNCTION,
                "src/a.ts::dup",
                "dup",
                "function",
                33,
                35,
            )
        };
        let drifted = NodeRow {
            file_id: FILE_DRIFTED,
            ..row(
                NODE_DRIFTED,
                "src/drifted.ts::drifted",
                "drifted",
                "function",
                1,
                3,
            )
        };
        let gone = NodeRow {
            file_id: FILE_GONE,
            ..row(NODE_GONE, "src/gone.ts::gone", "gone", "function", 1, 2)
        };
        let leak = NodeRow {
            file_id: FILE_LEAK,
            ..row(NODE_LEAK, "../secrets.ts::leak", "leak", "function", 1, 2)
        };
        let file_a = row(NODE_FILE_A, "src/a.ts::a.ts", "a.ts", "file", 1, 40);

        let folder_file = file_row(FILE_FOLDER, "src/widgets/x.ts", &"b".repeat(64), 12);
        let skipped = SkippedPathRow {
            id: 9,
            path: format!("{FOLDER_PREFIX}/skip.xslt"),
            reason: "grammar_unavailable".to_owned(),
            detail: None,
            language: None,
        };
        let files = vec![
            file_row(FILE_A, A_PATH, a_sha, 40),
            folder_file.clone(),
            file_row(FILE_DRIFTED, DRIFTED_PATH, drifted_sha, 3),
            file_row(FILE_GONE, GONE_PATH, drifted_sha, 2),
            file_row(FILE_LEAK, LEAK_PATH, &"c".repeat(64), 2),
        ];
        let rows = vec![
            (
                A_PATH.to_owned(),
                PathCoverage {
                    files: vec![file_row(FILE_A, A_PATH, a_sha, 40)],
                    not_indexed: Vec::new(),
                    indexed: 1,
                    skipped: 0,
                },
            ),
            (
                FOLDER_PREFIX.to_owned(),
                PathCoverage {
                    files: vec![folder_file],
                    not_indexed: vec![skipped.clone()],
                    indexed: 1,
                    skipped: 1,
                },
            ),
            (
                DRIFTED_PATH.to_owned(),
                PathCoverage {
                    files: vec![file_row(FILE_DRIFTED, DRIFTED_PATH, drifted_sha, 3)],
                    not_indexed: Vec::new(),
                    indexed: 1,
                    skipped: 0,
                },
            ),
        ];

        let mut skips = outside_skips();
        skips.push(skipped);

        Self {
            nodes: vec![
                file_a,
                widget,
                render.clone(),
                dispose,
                helper.clone(),
                dup_method,
                dup_function,
                drifted,
                gone,
                leak,
            ],
            metrics: vec![metrics(NODE_WIDGET, 0, 2), metrics(NODE_RENDER, 2, 1)],
            inbound: vec![(
                NODE_RENDER,
                vec![neighbour(
                    edge(1, NODE_HELPER, NODE_RENDER, "calls"),
                    helper.clone(),
                )],
            )],
            outbound: vec![(
                NODE_RENDER,
                vec![neighbour(
                    edge(2, NODE_RENDER, NODE_HELPER, "calls"),
                    helper,
                )],
            )],
            files,
            rows,
            skips,
            unresolved: Vec::new(),
            generation: Some(GENERATION.to_owned()),
            published_at: INDEXED_AT.to_owned(),
        }
    }
}

/// The source order the store's own queries produce: `ORDER BY start_line, qn`.
fn source_order(nodes: &[NodeRow], keep: impl Fn(&NodeRow) -> bool) -> Vec<NodeRow> {
    let mut rows: Vec<NodeRow> = nodes.iter().filter(|node| keep(node)).cloned().collect();
    rows.sort_by(|left, right| {
        left.start_line
            .cmp(&right.start_line)
            .then_with(|| left.qn.cmp(&right.qn))
    });
    rows
}

impl GraphReader for Fake {
    fn find_by_qn(&self, qn: &str) -> Result<Option<NodeRow>, AppError> {
        Ok(self.nodes.iter().find(|node| node.qn == qn).cloned())
    }

    fn find_by_name(&self, name: &str, kinds: Option<&[String]>) -> Result<Vec<NodeRow>, AppError> {
        let rows = source_order(&self.nodes, |node| node.name == name);
        Ok(match kinds {
            Some(kinds) if !kinds.is_empty() => rows
                .into_iter()
                .filter(|node| kinds.contains(&node.kind))
                .collect(),
            _ => rows,
        })
    }

    fn members(&self, parent_id: i64) -> Result<Vec<NodeRow>, AppError> {
        Ok(source_order(&self.nodes, |node| {
            node.parent_id == Some(parent_id)
        }))
    }

    fn nodes_by_file(&self, file_id: i64) -> Result<Vec<NodeRow>, AppError> {
        Ok(source_order(&self.nodes, |node| node.file_id == file_id))
    }

    fn inbound(
        &self,
        node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(self
            .inbound
            .iter()
            .find(|(id, _)| *id == node_id)
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default())
    }

    fn outbound(
        &self,
        node_id: i64,
        _kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        Ok(self
            .outbound
            .iter()
            .find(|(id, _)| *id == node_id)
            .map(|(_, rows)| rows.clone())
            .unwrap_or_default())
    }

    fn metrics_for(&self, node_id: i64) -> Result<Option<NodeMetrics>, AppError> {
        Ok(self
            .metrics
            .iter()
            .find(|metrics| metrics.node_id == node_id)
            .cloned())
    }

    fn rows_for_paths(&self, prefixes: &[String]) -> Result<PathCoverage, AppError> {
        for (prefix, entry) in &self.rows {
            if prefixes
                .iter()
                .any(|wanted| sangkar_core::paths::is_under_prefix(prefix, wanted))
            {
                return Ok(entry.clone());
            }
        }
        Ok(PathCoverage {
            files: Vec::new(),
            not_indexed: Vec::new(),
            indexed: 0,
            skipped: 0,
        })
    }

    fn not_indexed_all(&self) -> Result<Vec<SkippedPathRow>, AppError> {
        Ok(self.skips.clone())
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        Ok(self.skips.len() as i64)
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
        Ok(self.files.iter().find(|file| file.path == path).cloned())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        Ok(self.files.clone())
    }

    fn census_by_language(&self) -> Result<Vec<LanguageCensus>, AppError> {
        unreachable!("expand never renders a language census")
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        unreachable!("expand does not opt into the test census")
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
            files_indexed: self.files.len() as i64,
            nodes: self.nodes.len() as i64,
            edges: 0,
            duration_ms: 0,
            mode: "fast".to_owned(),
            status: "ok".to_owned(),
        }))
    }

    fn removed_symbols_for(&self, _since_generation: &str) -> Result<Vec<TombstoneRow>, AppError> {
        unreachable!("expand never asks for tombstones")
    }

    fn entry_points(&self, _limit: usize) -> Result<Vec<EntryPointRow>, AppError> {
        unreachable!("expand never ranks entry points")
    }

    fn hotspots(&self, _limit: usize) -> Result<Vec<HotspotRow>, AppError> {
        unreachable!("expand never ranks hotspots")
    }

    fn test_gaps(&self, _ids: &[i64]) -> Result<Vec<TestGapRow>, AppError> {
        unreachable!("expand is not a coverage report")
    }

    fn read_node_fts(&self, _match_query: &str, _limit: usize) -> Result<Vec<FtsHit>, AppError> {
        unreachable!("expand resolves a handle, it does not search")
    }
}

fn clock() -> String {
    NOW.to_owned()
}

/// A context over `reader`, answering as if the repository root were `root`. `source` is the only
/// mode that touches it.
fn context(reader: Fake, root: &Path) -> ToolContext {
    let cursor: Rc<dyn CursorFacade> =
        Rc::new(Cursor::new(KEY.as_bytes().to_vec(), None).expect("bind the test cursor key"));
    ToolContext {
        reader: Rc::new(reader),
        cursor,
        log: Logger::with_sink("expand", |_, _, _| {}),
        now: clock,
        config: Rc::new(
            load_config(&[], &|_key: &str| None, root).expect("resolve the fixture config"),
        ),
        db_size_bytes: 0,
    }
}

/// The fixture's graph, whose file digests describe the bytes the source tests write.
fn graph() -> Fake {
    Fake::new(
        &sha256_hex(a_source().as_bytes()),
        &sha256_hex(DRIFTED_INDEXED.as_bytes()),
    )
}

fn payload_of(envelope: &Envelope<Value>) -> &Value {
    assert_eq!(envelope.status, Status::Ok, "{:?}", envelope.error);
    envelope
        .payload
        .as_ref()
        .expect("an ok envelope has a payload")
}

fn keys_of(payload: &Value) -> Vec<String> {
    payload
        .as_object()
        .expect("a payload object")
        .keys()
        .cloned()
        .collect()
}

fn array_of<'a>(payload: &'a Value, key: &str) -> &'a Vec<Value> {
    payload[key].as_array().expect("a payload list")
}

/// One test per declared argument: the table is the schema, so it is read positionally.
fn declared(name: &str) -> &'static sangkar_tools::expand::manifest::Param {
    PARAMETERS
        .iter()
        .find(|param| param.name == name)
        .expect("the parameter is declared")
}

#[test]
fn the_manifest_keeps_the_description_verbatim_and_declares_no_cursor() {
    assert_eq!(NAME, "expand");
    assert!(
        DESCRIPTION.starts_with("Fetch more detail for one item from a previous result."),
        "{DESCRIPTION}"
    );
    assert!(DESCRIPTION.contains("says which it chose"), "{DESCRIPTION}");
    assert!(
        DESCRIPTION.contains("Pass the id, qualified name or path exactly as it appeared."),
        "{DESCRIPTION}"
    );

    // Six arguments, in the TypeScript literal's order, and `target` is the only required one.
    let names: Vec<&str> = PARAMETERS.iter().map(|param| param.name).collect();
    assert_eq!(
        names,
        vec!["target", "what", "path", "range", "budget", "repo"],
        "the declared surface, in `tools/list` order"
    );
    assert_eq!(
        PARAMETERS.iter().filter(|param| param.required).count(),
        1,
        "at most one required argument per tool"
    );
    assert!(declared("target").required);

    assert_eq!(declared("what").ty, ParamType::Enum(&EXPAND_WHATS));
    assert_eq!(
        EXPAND_WHATS,
        ["members", "edges", "source", "coverage", "outline"]
    );
    assert_eq!(declared("what").default, None, "an absent what is inferred");
    assert_eq!(
        declared("path").ty,
        ParamType::Str,
        "the prefix filter trace takes"
    );
    assert_eq!(
        declared("range").ty,
        ParamType::IntegerArray,
        "a pair of integers, which the handler clamps"
    );
    assert_eq!(
        declared("budget").default,
        Some(ParamDefault::Integer(2_500))
    );
    assert_eq!(DEFAULT_BUDGET_TOKENS, 2_500);
    assert_eq!(
        declared("repo").default,
        None,
        "an absent repo means unselected, so it has no default"
    );

    // The Done-when: a deliberately truncated answer is finished, not resumable, so the schema must
    // not offer a continuation the handler cannot accept back.
    assert!(
        !names.contains(&"cursor"),
        "expand declares no cursor: {names:?}"
    );

    assert_eq!(
        EXAMPLE[1].1,
        sangkar_tools::expand::manifest::ExampleValue::Text("outline")
    );
}

#[test]
fn the_args_hash_is_the_typescripts_canonical_text() {
    // `JSON.stringify({target, what})`: two keys, that order, no whitespace, `null` for an inferred
    // call. The digests below are `sha256sum` of exactly those byte strings, which is what the
    // TypeScript's `Bun.CryptoHasher` sees (`RUST-MIGRATION.md` §8 R4).
    assert_eq!(
        args_hash_of("src/a.ts::Widget", None),
        "sha256:80c445a4d19d4b91b85c74f08af89ac5d5a5fca4a1590dbd6ee03964bb87daed"
    );
    // A requested mode is hashed, so a resumed call cannot silently address a different view.
    assert_eq!(
        args_hash_of("src/a.ts::Widget", Some(What::Edges)),
        "sha256:1dfd46c9093df9e74d30825ed3b7bf4e14952c30fc138ed18f07f875b6fd9ed3"
    );
    assert_ne!(
        args_hash_of("src/a.ts::Widget", None),
        args_hash_of("src/a.ts::Widget", Some(What::Edges))
    );
}

#[test]
fn every_what_answers_with_its_own_payload_shape_in_the_contracts_key_order() {
    let scratch = Scratch::new("shapes");
    scratch.write(A_PATH, &a_source());
    scratch.write(DRIFTED_PATH, DRIFTED_INDEXED);
    let ctx = context(graph(), &scratch.root);

    let cases = [
        (
            ExpandArgs {
                what: Some(What::Members),
                ..ExpandArgs::new("src/a.ts::Widget")
            },
            What::Members,
            vec!["target", "what", "why", "members"],
        ),
        (
            ExpandArgs {
                what: Some(What::Edges),
                ..ExpandArgs::new("src/a.ts::Widget.render")
            },
            What::Edges,
            vec!["target", "what", "why", "direction", "depth", "in", "out"],
        ),
        (
            ExpandArgs {
                what: Some(What::Source),
                ..ExpandArgs::new("src/a.ts::Widget.render")
            },
            What::Source,
            vec![
                "target",
                "what",
                "why",
                "path",
                "source_drift",
                "window",
                "content",
            ],
        ),
        (
            ExpandArgs {
                what: Some(What::Coverage),
                ..ExpandArgs::new("src/a.ts::Widget")
            },
            What::Coverage,
            vec!["target", "what", "why", "prefix", "files", "not_indexed"],
        ),
        (
            ExpandArgs {
                what: Some(What::Outline),
                ..ExpandArgs::new("src/a.ts::Widget")
            },
            What::Outline,
            vec!["target", "what", "why", "path", "members"],
        ),
    ];

    for (args, what, keys) in cases {
        let envelope = handle(&ctx, &args);
        let payload = payload_of(&envelope);
        assert_eq!(payload["what"], Value::String(what.as_str().to_owned()));
        assert_eq!(keys_of(payload), keys, "the {} payload", what.as_str());
        // The address is restated in full, so an agent expanding a stale handle sees the node.
        assert!(
            payload["target"]["qn"].is_string() && payload["target"]["id"].is_i64(),
            "the {} payload restates the address",
            what.as_str()
        );
        assert_eq!(envelope.next_tools, vec!["trace".to_owned()]);
        assert!(!envelope.truncated, "this fixture fits the default budget");
    }
}

#[test]
fn a_class_target_infers_members_and_says_so() {
    let scratch = Scratch::new("class");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new("src/a.ts::Widget"));
    let payload = payload_of(&envelope);

    assert_eq!(payload["what"], Value::String("members".to_owned()));
    assert_eq!(
        payload["why"],
        Value::String("inferred: class -> members".to_owned())
    );
    assert_eq!(
        payload["target"],
        serde_json::json!({
            "id": NODE_WIDGET,
            "qn": "src/a.ts::Widget",
            "kind": "class",
            "lines": [3, 20],
        })
    );
    // Source order, one level only: the members are the class's own methods.
    let members = array_of(payload, "members");
    let names: Vec<&str> = members
        .iter()
        .map(|member| member["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(names, vec!["render", "dispose", "dup"]);
    assert_eq!(members[0]["lines"], serde_json::json!([5, 8]));
    // `metrics_for` never measured `dispose` or `dup`, so their degree is unknown, not zero.
    assert_eq!(members[0]["degree"], serde_json::json!({"in": 2, "out": 1}));
    assert_eq!(members[1]["degree"], Value::Null);
    assert_eq!(envelope.totals.get("members_total"), Some(&Value::from(3)));
    assert_eq!(envelope.summary, "members of src/a.ts::Widget: 3.");
    assert_eq!(envelope.read_status.generation, GENERATION);
    assert!(!envelope.read_status.stale);
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::String("eq".to_owned())),
        "a complete, fresh, fully-bound scope may claim an exact total"
    );
}

#[test]
fn a_file_target_infers_members_of_its_top_level_symbols() {
    let scratch = Scratch::new("file");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new(A_PATH));
    let payload = payload_of(&envelope);

    assert_eq!(
        payload["why"],
        Value::String("inferred: file -> members".to_owned())
    );
    assert_eq!(payload["target"]["kind"], Value::String("file".to_owned()));
    let mut names: Vec<&str> = array_of(payload, "members")
        .iter()
        .map(|member| member["name"].as_str().expect("a name"))
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["Widget", "dup", "helper"]);
}

#[test]
fn a_method_target_infers_edges_in_the_grouped_neighbour_shape() {
    let scratch = Scratch::new("edges");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new("Widget.render"));
    let payload = payload_of(&envelope);

    assert_eq!(payload["what"], Value::String("edges".to_owned()));
    assert_eq!(
        payload["why"],
        Value::String("inferred: method -> edges".to_owned())
    );
    assert_eq!(payload["depth"], Value::from(1));
    assert_eq!(payload["direction"], Value::String("all".to_owned()));
    for side in ["in", "out"] {
        let groups = array_of(payload, side);
        assert_eq!(groups.len(), 1, "{side} holds one edge kind");
        assert_eq!(groups[0]["edge"], Value::String("calls".to_owned()));
        assert_eq!(groups[0]["tier"], Value::String("static".to_owned()));
        // `external` is always present in this payload: the domain row always carries it.
        assert_eq!(
            groups[0]["items"],
            serde_json::json!([{
                "id": NODE_HELPER,
                "qn": "src/a.ts::helper",
                "kind": "function",
                "file": "src/a.ts",
                "line": 25,
                "hop": 1,
                "external": false,
            }])
        );
    }
    assert_eq!(envelope.totals.get("edges_total"), Some(&Value::from(2)));
    assert_eq!(
        envelope.summary,
        "neighbours of src/a.ts::Widget.render at depth 1: 2."
    );
}

#[test]
fn a_folder_target_infers_coverage_of_the_prefix_with_both_halves() {
    let scratch = Scratch::new("folder");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new(FOLDER_PREFIX));
    let payload = payload_of(&envelope);

    assert_eq!(payload["what"], Value::String("coverage".to_owned()));
    assert_eq!(
        payload["why"],
        Value::String("inferred: folder -> coverage".to_owned())
    );
    // A folder is an address, not a node: the three node fields are null rather than invented.
    assert_eq!(
        payload["target"],
        serde_json::json!({"id": null, "qn": null, "kind": "folder", "lines": null})
    );
    assert_eq!(payload["prefix"], Value::String(FOLDER_PREFIX.to_owned()));
    assert_eq!(
        array_of(payload, "files")[0]["path"],
        Value::String(format!("{FOLDER_PREFIX}/x.ts"))
    );
    assert_eq!(
        payload["not_indexed"],
        serde_json::json!([{
            "path": format!("{FOLDER_PREFIX}/skip.xslt"),
            "reason": "grammar_unavailable",
            "detail": null,
        }])
    );
    // The envelope's own coverage keeps the same skip, so payload and contract agree.
    assert_eq!(
        envelope.coverage.not_indexed,
        vec![sangkar_core::envelope::NotIndexedEntry::new(
            format!("{FOLDER_PREFIX}/skip.xslt"),
            "grammar_unavailable"
        )]
    );
    assert!(!envelope.coverage.complete);
    assert_eq!(envelope.totals.get("files_total"), Some(&Value::from(1)));
    // The total is the graph's, so it exceeds this prefix's own count instead of echoing it.
    assert_eq!(
        envelope.totals.get("not_indexed_total"),
        Some(&Value::from(7))
    );
    assert_eq!(envelope.coverage.skipped, 1);
    assert_eq!(
        envelope.summary,
        format!("coverage for {FOLDER_PREFIX}: 1 files, 1 not indexed.")
    );
}

#[test]
fn an_explicit_what_wins_and_is_echoed_as_requested() {
    let scratch = Scratch::new("explicit");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Edges),
            ..ExpandArgs::new("src/a.ts::Widget")
        },
    );
    let payload = payload_of(&envelope);
    assert_eq!(payload["why"], Value::String("requested: edges".to_owned()));
}

#[test]
fn coverage_on_a_node_answers_for_that_nodes_file() {
    let scratch = Scratch::new("node-coverage");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Coverage),
            ..ExpandArgs::new("src/a.ts::Widget")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(
        payload["why"],
        Value::String("requested: coverage".to_owned())
    );
    assert_eq!(payload["prefix"], Value::String(A_PATH.to_owned()));
    assert_eq!(payload["target"]["kind"], Value::String("class".to_owned()));
    assert_eq!(
        array_of(payload, "files")[0]["path"],
        Value::String(A_PATH.to_owned())
    );
    // The file's own digest is on the wire, which is how a caller can trust the source mode.
    assert_eq!(
        array_of(payload, "files")[0]["sha256"],
        Value::String(sha256_hex(a_source().as_bytes()))
    );
}

#[test]
fn outline_lists_a_classs_members_in_source_order_with_signature() {
    let scratch = Scratch::new("outline");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Outline),
            ..ExpandArgs::new("src/a.ts::Widget")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(
        payload["why"],
        Value::String("requested: outline".to_owned())
    );
    assert_eq!(payload["path"], Value::String(A_PATH.to_owned()));
    assert_eq!(
        payload["members"],
        serde_json::json!([
            {
                "qn": "src/a.ts::Widget.render",
                "kind": "method",
                "lines": [5, 8],
                "signature": "render(): void",
            },
            {
                "qn": "src/a.ts::Widget.dispose",
                "kind": "method",
                "lines": [10, 12],
                "signature": "dispose(): void",
            },
            // `dup` carries no signature in the fixture, so the field is null rather than invented.
            {"qn": "src/a.ts::Widget.dup", "kind": "method", "lines": [14, 16], "signature": null},
        ])
    );
    assert_eq!(envelope.totals.get("members_total"), Some(&Value::from(3)));
    assert_eq!(envelope.summary, "outline of src/a.ts::Widget: 3.");
}

#[test]
fn outline_of_a_file_lists_every_node_it_holds_and_never_itself() {
    let scratch = Scratch::new("outline-file");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Outline),
            ..ExpandArgs::new(A_PATH)
        },
    );
    let payload = payload_of(&envelope);

    let qns: Vec<&str> = array_of(payload, "members")
        .iter()
        .map(|member| member["qn"].as_str().expect("a qn"))
        .collect();
    assert_eq!(
        qns,
        vec![
            "src/a.ts::Widget",
            "src/a.ts::Widget.render",
            "src/a.ts::Widget.dispose",
            "src/a.ts::Widget.dup",
            "src/a.ts::helper",
            "src/a.ts::dup",
        ],
        "source order over nodes_by_file, one flat table of contents"
    );
    // The file's own row is the target, not one of its members.
    assert!(!qns.contains(&"src/a.ts::a.ts"));
    assert_eq!(envelope.totals.get("members_total"), Some(&Value::from(6)));
}

#[test]
fn a_tie_is_ambiguous_with_candidates_and_no_answer_body() {
    let scratch = Scratch::new("ambiguous");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new("dup"));
    assert_eq!(envelope.status, Status::Ambiguous);
    let payload = envelope
        .payload
        .as_ref()
        .expect("a tie carries its candidates");

    let mut qns: Vec<&str> = array_of(payload, "candidates")
        .iter()
        .map(|candidate| candidate["qn"].as_str().expect("a qn"))
        .collect();
    qns.sort_unstable();
    assert_eq!(qns, vec!["src/a.ts::Widget.dup", "src/a.ts::dup"]);
    assert_eq!(payload["candidates_total"], Value::from(2));
    assert_eq!(payload["candidates_shown"], Value::from(2));
    assert_eq!(
        envelope.totals.get("candidates_total"),
        Some(&Value::from(2)),
        "one tie, one pair of numbers: payload and totals cannot disagree"
    );
    assert_eq!(
        envelope.totals.get("candidates_shown"),
        Some(&Value::from(2))
    );
    assert_eq!(
        envelope.summary,
        "\"dup\" matches 2 nodes; pass the qn of the one you meant"
    );
    for candidate in array_of(payload, "candidates") {
        let qn = candidate["qn"].as_str().expect("a qn");
        assert_eq!(
            candidate["next_call"],
            serde_json::json!({"tool": "expand", "target": qn}),
            "every candidate offers the call that expands exactly it"
        );
        // A score is a float on the wire, and 0.9 is written the way `JSON.stringify` writes it.
        assert_eq!(candidate["score"], serde_json::json!(0.9));
    }
}

#[test]
fn an_unknown_target_is_not_found_with_the_accepted_forms() {
    let scratch = Scratch::new("not-found");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new("nowhere::nothing"));
    assert_eq!(envelope.status, Status::NotFound);
    let payload = envelope
        .payload
        .as_ref()
        .expect("a refusal names the target");
    assert_eq!(keys_of(payload), vec!["target", "why", "hint"]);
    assert_eq!(
        payload["target"],
        Value::String("nowhere::nothing".to_owned())
    );
    assert_eq!(
        payload["why"],
        Value::String("nothing in the index matches the target".to_owned())
    );
    assert!(
        payload["hint"]
            .as_str()
            .expect("a hint")
            .contains("qualified name"),
        "{payload}"
    );
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::String("gte".to_owned())),
        "an answer over neither a node nor a scope may not claim an exact total"
    );
}

#[test]
fn a_bare_numeric_id_is_refused_with_a_hint_rather_than_guessed_at() {
    let scratch = Scratch::new("bare-id");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new("1006"));
    assert_eq!(envelope.status, Status::NotFound);
    let payload = envelope
        .payload
        .as_ref()
        .expect("a refusal names the target");
    assert_eq!(
        payload["why"],
        Value::String("a bare numeric id cannot be resolved".to_owned())
    );
    let hint = payload["hint"].as_str().expect("a hint");
    assert!(hint.contains("numeric id"), "{hint}");
    assert!(hint.contains("qualified name"), "{hint}");
    assert_eq!(
        envelope.summary,
        "id 1006 is not an address expand can resolve"
    );
}

#[test]
fn a_folder_asked_for_source_is_not_found_naming_coverage() {
    let scratch = Scratch::new("folder-source");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            ..ExpandArgs::new(FOLDER_PREFIX)
        },
    );
    assert_eq!(envelope.status, Status::NotFound);
    let payload = envelope
        .payload
        .as_ref()
        .expect("a refusal names the target");
    assert_eq!(
        payload["hint"],
        Value::String("pass what=coverage for a folder".to_owned())
    );
    assert_eq!(
        payload["why"],
        Value::String(format!(
            "\"{FOLDER_PREFIX}\" is a folder, so only coverage can be expanded from it"
        ))
    );
}

#[test]
fn a_verified_file_returns_its_numbered_line_window() {
    let scratch = Scratch::new("source");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["source_drift"], Value::Bool(false));
    assert_eq!(payload["path"], Value::String(A_PATH.to_owned()));
    assert_eq!(payload["window"], serde_json::json!([5, 8]));
    let content = array_of(payload, "content");
    let numbers: Vec<i64> = content
        .iter()
        .map(|line| line["line"].as_i64().expect("a line number"))
        .collect();
    assert_eq!(numbers, vec![5, 6, 7, 8]);
    assert_eq!(
        content[0]["text"],
        Value::String("const v5 = 5;".to_owned())
    );
    assert!(!envelope.read_status.stale);
    assert_eq!(
        envelope.read_status.stale_checked_at, None,
        "a fresh answer was checked against the index but has nothing stale to date"
    );
    assert_eq!(envelope.summary, format!("lines 5-8 of {A_PATH}."));
}

#[test]
fn a_range_inside_the_node_returns_exactly_those_lines() {
    let scratch = Scratch::new("range-inside");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            range: Some(vec![6, 7]),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    // One effective window: the requested pair, because it sits inside the node's 5-8.
    assert_eq!(payload["window"], serde_json::json!([6, 7]));
    assert_eq!(
        array_of(payload, "content")[0]["text"],
        Value::String("const v6 = 6;".to_owned())
    );
    // Nothing was clamped, so `why` is the plain disclosure it was before the range existed.
    assert_eq!(
        payload["why"],
        Value::String("requested: source".to_owned())
    );
    assert_eq!(envelope.totals.get("lines_total"), Some(&Value::from(2)));
    assert_eq!(envelope.summary, format!("lines 6-7 of {A_PATH}."));
}

#[test]
fn a_range_crossing_the_nodes_boundary_is_clamped_and_why_says_so() {
    let scratch = Scratch::new("range-clamp");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            range: Some(vec![2, 30]),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["window"], serde_json::json!([5, 8]));
    assert_eq!(array_of(payload, "content").len(), 4);
    assert_eq!(
        payload["why"],
        Value::String("requested: source; range 2-30 clamped to the node's extent 5-8".to_owned())
    );
    assert_eq!(envelope.totals.get("lines_total"), Some(&Value::from(4)));
}

#[test]
fn a_range_entirely_outside_the_node_clamps_to_its_nearest_line() {
    let scratch = Scratch::new("range-outside");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            range: Some(vec![100, 200]),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["window"], serde_json::json!([8, 8]));
    assert_eq!(
        array_of(payload, "content")[0]["text"],
        Value::String("const v8 = 8;".to_owned())
    );
    assert_eq!(
        payload["why"],
        Value::String(
            "requested: source; range 100-200 clamped to the node's extent 5-8".to_owned()
        )
    );
}

#[test]
fn an_inverted_range_selects_nothing_without_failing_the_call() {
    let scratch = Scratch::new("range-inverted");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            range: Some(vec![8, 5]),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["source_drift"], Value::Bool(false));
    // An empty list, not null: null is reserved for "the bytes could not be trusted".
    assert_eq!(payload["content"], serde_json::json!([]));
    assert_eq!(payload["window"], serde_json::json!([8, 5]));
    assert_eq!(
        payload["why"],
        Value::String(
            "requested: source; range 8-5 is inverted, so no lines were selected".to_owned()
        )
    );
    assert_eq!(envelope.totals.get("lines_total"), Some(&Value::from(0)));
    assert_eq!(envelope.summary, format!("no lines of {A_PATH} in 8-5."));
}

#[test]
fn a_range_that_is_not_a_pair_is_ignored_with_a_note() {
    let scratch = Scratch::new("range-shape");
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            range: Some(vec![6]),
            ..ExpandArgs::new("src/a.ts::Widget.render")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["window"], serde_json::json!([5, 8]));
    assert_eq!(
        payload["why"],
        Value::String(
            "requested: source; range ignored: pass exactly two integers, [from, to]".to_owned()
        )
    );
}

#[test]
fn a_modified_file_reports_source_drift_against_the_observed_sha256() {
    let scratch = Scratch::new("drift");
    // The row records the digest of the version that was indexed; the bytes on disk say otherwise.
    scratch.write(DRIFTED_PATH, DRIFTED_NOW);
    let indexed = sha256_hex(DRIFTED_INDEXED.as_bytes());
    let observed = sha256_hex(DRIFTED_NOW.as_bytes());
    assert_ne!(observed, indexed, "the fixture must actually drift");
    let ctx = context(
        Fake::new(&sha256_hex(a_source().as_bytes()), &indexed),
        &scratch.root,
    );

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            ..ExpandArgs::new("src/drifted.ts::drifted")
        },
    );
    let payload = payload_of(&envelope);

    // The Done-when: the observed digest differs from the indexed one, and the answer says so
    // instead of serving lines the agent would quote as current.
    assert_eq!(
        payload["source_drift"],
        Value::Bool(true),
        "observed sha256 {observed} against indexed {indexed}"
    );
    assert_eq!(payload["content"], Value::Null);
    assert!(envelope.read_status.stale);
    assert_eq!(
        envelope.read_status.stale_reason,
        Some(format!("\"{DRIFTED_PATH}\" changed since the index"))
    );
    assert_eq!(envelope.read_status.stale_checked_at, Some(NOW.to_owned()));
    assert_eq!(
        envelope.summary,
        format!("{DRIFTED_PATH} changed since the index: no source returned.")
    );
    assert_eq!(envelope.totals.get("lines_total"), Some(&Value::from(0)));
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::String("gte".to_owned())),
        "a stale answer may not claim an exact total"
    );
}

#[test]
fn a_file_that_vanished_since_the_index_is_drift_too_not_an_error() {
    let scratch = Scratch::new("vanished");
    // Nothing is written at `src/gone.ts`: the row exists, the path does not.
    scratch.write(A_PATH, &a_source());
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            ..ExpandArgs::new("src/gone.ts::gone")
        },
    );
    let payload = payload_of(&envelope);

    assert_eq!(payload["source_drift"], Value::Bool(true));
    assert_eq!(payload["content"], Value::Null);
    assert_eq!(
        envelope.read_status.stale_reason,
        Some(format!("\"{GONE_PATH}\" is no longer readable on disk"))
    );
}

#[test]
fn a_stored_path_that_escapes_the_repo_root_is_refused_instead_of_read() {
    let scratch = Scratch::new("escape");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Source),
            ..ExpandArgs::new("../secrets.ts::leak")
        },
    );

    assert_eq!(envelope.status, Status::Error);
    let error = envelope
        .error
        .as_ref()
        .expect("an error envelope carries one");
    assert_eq!(error.code.as_str(), "internal");
    assert!(
        error.message.contains("outside the repository root"),
        "{}",
        error.message
    );
    assert!(envelope.payload.is_none());
}

#[test]
fn an_unpublished_graph_is_bootstrapping_not_not_found() {
    let scratch = Scratch::new("bootstrap");
    let mut fake = graph();
    fake.generation = None;
    let ctx = context(fake, &scratch.root);

    let envelope = handle(&ctx, &ExpandArgs::new(A_PATH));
    assert_eq!(envelope.status, Status::Bootstrapping);
    let error = envelope
        .error
        .as_ref()
        .expect("an error envelope carries one");
    assert_eq!(error.code.as_str(), "bootstrapping");
    assert_eq!(error.retry_after_ms, Some(5_000));
    assert_eq!(envelope.read_status.generation, "unpublished");
    assert!(envelope.payload.is_none());
}

#[test]
fn a_budget_above_the_protocol_ceiling_is_an_error_not_a_clamped_page() {
    let scratch = Scratch::new("ceiling");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            budget: Some(50_000),
            ..ExpandArgs::new("src/a.ts::Widget")
        },
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope
        .error
        .as_ref()
        .expect("an error envelope carries one");
    assert_eq!(error.code.as_str(), "budget_too_large");
}

#[test]
fn a_budget_below_the_floor_refuses_with_the_floor_rather_than_serving_an_oversized_answer() {
    let scratch = Scratch::new("floor");
    let ctx = context(graph(), &scratch.root);

    let envelope = handle(
        &ctx,
        &ExpandArgs {
            budget: Some(1),
            ..ExpandArgs::new("src/a.ts::Widget")
        },
    );
    assert_eq!(envelope.status, Status::Error);
    let error = envelope
        .error
        .as_ref()
        .expect("an error envelope carries one");
    assert_eq!(error.code.as_str(), "budget_too_large");
    let hint = error.hint.as_deref().expect("the refusal names the floor");
    assert!(hint.contains("tokens"), "{hint}");
    assert!(hint.contains(&DEFAULT_BUDGET_TOKENS.to_string()), "{hint}");
    // Below the floor no cut can reach the allowance, so nothing was truncated or continued.
    assert!(!envelope.truncated);
    assert_eq!(envelope.next, None);
    assert!(envelope.payload.is_none());
}

#[test]
fn a_truncated_answer_is_finished_rather_than_resumable() {
    let scratch = Scratch::new("truncated");
    // A dense container: sixty members, so some budget in the middle has to cut the list.
    let mut fake = graph();
    for index in 0..60 {
        let qn = format!("src/a.ts::Widget.m{index}");
        let name = format!("m{index}");
        fake.nodes.push(NodeRow {
            parent_id: Some(NODE_WIDGET),
            ..row(100 + index, &qn, &name, "method", 30, 31)
        });
    }
    let ctx = context(fake, &scratch.root);
    let whole = handle(&ctx, &ExpandArgs::new("src/a.ts::Widget"));
    let whole_tokens = budget_floor_of(&whole);

    let mut observed = None;
    for budget in 1..=whole_tokens {
        let envelope = handle(
            &ctx,
            &ExpandArgs {
                budget: Some(budget),
                ..ExpandArgs::new("src/a.ts::Widget")
            },
        );
        if envelope.truncated {
            observed = Some((budget, envelope));
            break;
        }
    }
    let (budget, envelope) = observed.expect("some budget must cut a sixty-member list");

    assert_eq!(envelope.status, Status::Ok, "budget {budget}");
    assert_eq!(
        envelope.truncation_reason,
        Some(TruncationReason::Budget),
        "the cut is disclosed as a budget cut"
    );
    // No cursor is declared, so none is published: a truncated `expand` answer is finished, and the
    // caller raises the budget or narrows `what` instead of replaying a token.
    assert_eq!(
        envelope.next, None,
        "budget {budget} publishes no continuation"
    );
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::String("gte".to_owned())),
        "a cut answer may not claim an exact total"
    );
    assert!(
        envelope.summary.contains("showing"),
        "the summary says how much of the total was served: {}",
        envelope.summary
    );
    let payload = payload_of(&envelope);
    assert!(
        array_of(payload, "members").len() < 61,
        "the served list is what survived the cut"
    );
}

/// The polyglot `src/Account.cs` slice: the six rows behind the two captured `expand` answers.
///
/// Hand-written because the goldens were captured from a database this suite must not open
/// (`rust/tests/parity/README.md`). The ids, lines, signatures and degrees are the captured ones.
fn polyglot_account() -> Fake {
    let account_file = row(1, "src/Account.cs::Account.cs", "Account.cs", "file", 1, 25);
    let account = NodeRow {
        parent_id: Some(1),
        signature: Some("public class Account".to_owned()),
        ..row(2, "src/Account.cs::Account", "Account", "class", 3, 16)
    };
    let deposit = NodeRow {
        parent_id: Some(2),
        signature: Some("public virtual int Deposit(int amount)".to_owned()),
        ..row(
            3,
            "src/Account.cs::Account.Deposit",
            "Deposit",
            "method",
            7,
            10,
        )
    };
    let adjust = NodeRow {
        parent_id: Some(2),
        signature: Some("protected int Adjust(int amount)".to_owned()),
        ..row(
            4,
            "src/Account.cs::Account.Adjust",
            "Adjust",
            "method",
            12,
            15,
        )
    };
    let savings = NodeRow {
        parent_id: Some(1),
        signature: Some("public class SavingsAccount : Account".to_owned()),
        ..row(
            5,
            "src/Account.cs::SavingsAccount",
            "SavingsAccount",
            "class",
            18,
            24,
        )
    };
    let savings_deposit = NodeRow {
        parent_id: Some(5),
        signature: Some("public override int Deposit(int amount)".to_owned()),
        ..row(
            6,
            "src/Account.cs::SavingsAccount.Deposit",
            "Deposit",
            "method",
            20,
            23,
        )
    };
    let indexed = file_row(1, "src/Account.cs", &"a".repeat(64), 25);
    Fake {
        nodes: vec![
            account_file,
            account,
            deposit,
            adjust,
            savings,
            savings_deposit,
        ],
        metrics: vec![metrics(2, 2, 2), metrics(5, 1, 2)],
        inbound: Vec::new(),
        outbound: Vec::new(),
        rows: vec![(
            "src/Account.cs".to_owned(),
            PathCoverage {
                files: vec![indexed.clone()],
                not_indexed: Vec::new(),
                indexed: 1,
                skipped: 0,
            },
        )],
        files: vec![indexed],
        skips: Vec::new(),
        unresolved: vec![UnresolvedRef {
            id: 1,
            file_id: 1,
            file: "src/Account.cs".to_owned(),
            line: 9,
            raw_target: "_balance".to_owned(),
            evidence_kind: None,
            evidence: None,
        }],
        generation: Some("g-4b56df".to_owned()),
        published_at: "2026-11-24T04:44:41.233Z".to_owned(),
    }
}

/// The canonical root the polyglot goldens were captured from.
const PARITY_REPO: &str = "/tmp/sangkar-parity-capture/polyglot/polyglot";

/// The served envelope as a golden can be compared with it.
///
/// One field is normalised, and it is the composition root's rather than the handler's:
/// `read_status.decisive` is set by `route()` after the handler returns, so a unit test that claimed
/// it would be claiming knowledge of whether the answer ends the agent's search. The generation and
/// `indexed_at` are *not* normalised here: the fixture serves the golden's own publish, so they are
/// the captured values.
fn served(envelope: &Envelope<Value>) -> Value {
    let mut value = serde_json::to_value(envelope).expect("the envelope is JSON");
    value["read_status"]
        .as_object_mut()
        .expect("a read_status object")
        .remove("decisive");
    value
}

fn expected_of(golden: &Value) -> Value {
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

#[test]
fn the_polyglot_members_golden_is_served_byte_for_byte() {
    let golden: Value =
        serde_json::from_str(include_str!("../../../tests/golden/polyglot/expand.json"))
            .expect("the captured golden is JSON");
    assert_eq!(
        golden["args"],
        serde_json::json!(["--target", "src/Account.cs", "--what", "members"])
    );

    let ctx = context(polyglot_account(), Path::new(PARITY_REPO));
    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Members),
            ..ExpandArgs::new("src/Account.cs")
        },
    );

    assert_eq!(
        served(&envelope),
        expected_of(&golden),
        "the served envelope is the golden's, minus the root-owned `read_status.decisive`"
    );
    assert_eq!(
        envelope.summary,
        "members of src/Account.cs::Account.cs: 2."
    );
    assert_eq!(
        envelope.totals.get("total_relation"),
        Some(&Value::String("gte".to_owned())),
        "the captured scope holds one unbound call site"
    );
}

#[test]
fn the_polyglot_outline_golden_is_served_byte_for_byte() {
    let golden: Value = serde_json::from_str(include_str!(
        "../../../tests/golden/polyglot/expand.outline.json"
    ))
    .expect("the captured golden is JSON");
    assert_eq!(
        golden["args"],
        serde_json::json!(["--target", "src/Account.cs", "--what", "outline"])
    );

    let ctx = context(polyglot_account(), Path::new(PARITY_REPO));
    let envelope = handle(
        &ctx,
        &ExpandArgs {
            what: Some(What::Outline),
            ..ExpandArgs::new("src/Account.cs")
        },
    );

    assert_eq!(
        served(&envelope),
        expected_of(&golden),
        "the served envelope is the golden's, minus the root-owned `read_status.decisive`"
    );
    assert_eq!(
        envelope.summary,
        "outline of src/Account.cs::Account.cs: 5."
    );
}
