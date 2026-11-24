//! The change set and the freshness verdict against real repositories: every git invocation in
//! this file targets a temporary directory this suite created, never the checkout the suite runs
//! from. The fixture is built with `git init` and one commit, then each case opens exactly the
//! difference it is about — a modified file, an added one, a deletion, a rename, an untracked path,
//! and a path no grammar covers — so a failure names the fact that broke.
//!
//! The freshness half uses a hand-written [`FakeReader`] for the parts a database would provide:
//! the verdict is a pure function of the reader and the injected clock, which is the whole reason
//! `check_freshness` takes a reader rather than a connection. `meta_for`, `latest_generation` and
//! `all_files` are the only methods it ever calls; the rest return `internal` so an accidental
//! call is a loud test failure rather than a silent default.
//!
//! The suite skips only the cases that need the `git` binary, with a message naming the case, so a
//! machine without git still reports the rest rather than one opaque failure.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::envelope::TestCensus;
use sangkar_core::errors::AppError;
use sangkar_domain::diff::{
    ChangeEntry, ChangeSet, ChangeState, ChangedSinceOptions, DigestSource, FileDigest,
    UnmatchedChange, UnmatchedReason, changed_since, sha256_hex,
};
use sangkar_domain::freshness::{
    FreshnessOptions, STALE_REASONS, WATCHER_STALL_MS, check_freshness, head_commit_of, health,
};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};
use sangkar_store::read::edges::NeighbourEdge;
use sangkar_store::read::files::LanguageCensus;
use sangkar_store::read::metrics::NodeMetrics;
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::{
    EntryPointRow, FtsHit, GenerationRow, GraphMeta, GraphReader, HotspotRow, TestGapRow,
    TombstoneRow,
};

/// A temporary directory that removes itself, so the suite leaves no repository behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-diff-freshness-{label}-{}-{nanos}-{}",
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

impl Drop for TempDir {
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

/// Whether the `git` binary exists. The check runs in the OS temp directory, never in this
/// checkout: `--version` reads no repository, but the rule here is that no invocation starts
/// anywhere the session's own tree could be reached.
fn git_is_available() -> bool {
    Command::new("git")
        .arg("--version")
        .current_dir(std::env::temp_dir())
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Runs one git command inside `dir` with a fully isolated configuration: no system or user config
/// to inherit, an identity so `commit` cannot fail on a machine that never set one, and no terminal
/// prompt. The failure message quotes stderr, so a broken fixture names the command that broke.
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

/// The database half of the change set, written by hand: the fixture's indexed state.
struct FakeDigests(Vec<FileDigest>);

impl DigestSource for FakeDigests {
    fn all_files(&self) -> Result<Vec<FileDigest>, AppError> {
        Ok(self.0.clone())
    }
}

/// The digest of a working-tree file, as the indexer would have stored it.
fn digest_of(root: &Path, relative: &str) -> FileDigest {
    FileDigest {
        path: relative.to_owned(),
        sha256: sha256_hex(&fs::read(root.join(relative)).expect("read the fixture file")),
    }
}

/// One committed repository with the five change kinds staged on top of `HEAD`.
fn committed_fixture(label: &str) -> (TempDir, String) {
    let temp = TempDir::new(label);
    let root = temp.path();
    write_file(root, "keep.rs", "alpha\n");
    write_file(root, "gone.rs", "gone\n");
    write_file(root, "old_name.rs", "moved body\n");
    write_file(root, "stable.rs", "stable\n");
    write_file(root, "notes.txt", "notes\n");
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "base"]);
    let head = git(root, &["rev-parse", "HEAD"]);
    (temp, head)
}

/// `ChangeSet` entry shapes, flattened so an assertion failure shows the whole list.
fn entry_shapes(changes: &ChangeSet) -> Vec<(String, ChangeState, Option<String>, bool)> {
    changes
        .entries
        .iter()
        .map(|entry: &ChangeEntry| {
            (
                entry.path.clone(),
                entry.state,
                entry.from.clone(),
                entry.sha256.is_some(),
            )
        })
        .collect()
}

#[test]
fn the_change_set_reports_modified_added_deleted_renamed_and_untracked() {
    if !git_is_available() {
        eprintln!(
            "skipping the_change_set_reports_modified_added_deleted_renamed_and_untracked: git is not on PATH"
        );
        return;
    }
    let (temp, _head) = committed_fixture("change-set");
    let root = temp.path();
    let indexed = vec![
        digest_of(root, "keep.rs"),
        digest_of(root, "gone.rs"),
        digest_of(root, "old_name.rs"),
        digest_of(root, "stable.rs"),
    ];

    // The difference from `HEAD`: one of each state, plus a path no grammar covers.
    write_file(root, "keep.rs", "alpha changed\n");
    write_file(root, "added.rs", "added\n");
    git(root, &["add", "added.rs"]);
    git(root, &["rm", "-q", "gone.rs"]);
    git(root, &["mv", "old_name.rs", "new_name.rs"]);
    write_file(root, "untracked.rs", "untracked\n");
    write_file(root, "notes.txt", "notes changed\n");

    let changes = changed_since(
        root,
        None,
        &FakeDigests(indexed),
        &ChangedSinceOptions::default(),
    );

    assert!(
        !changes.degraded,
        "git answered in a real repository: {changes:?}"
    );
    assert_eq!(
        changes.changed,
        vec![
            "added.rs".to_owned(),
            "keep.rs".to_owned(),
            "new_name.rs".to_owned(),
            "untracked.rs".to_owned()
        ],
        "one entry per path that differs from the base or from its stored digest"
    );
    assert_eq!(
        changes.removed,
        vec!["gone.rs".to_owned(), "old_name.rs".to_owned()],
        "the deletion and the vanished side of the rename"
    );

    let states: Vec<(String, ChangeState, Option<String>)> = changes
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.state, entry.from.clone()))
        .collect();
    assert_eq!(
        states,
        vec![
            ("added.rs".to_owned(), ChangeState::Added, None),
            ("gone.rs".to_owned(), ChangeState::Deleted, None),
            ("keep.rs".to_owned(), ChangeState::Modified, None),
            (
                "new_name.rs".to_owned(),
                ChangeState::Renamed,
                Some("old_name.rs".to_owned())
            ),
            ("old_name.rs".to_owned(), ChangeState::Deleted, None),
            ("untracked.rs".to_owned(), ChangeState::Untracked, None),
        ],
        "a rename is ONE entry carrying both paths; a deletion is a state, not an absence"
    );
    assert_eq!(
        changes.entries.len(),
        changes.changed.len() + changes.removed.len(),
        "entries are derived from the two path lists and never drift from them"
    );
    assert_eq!(
        changes.unmatched,
        vec![UnmatchedChange {
            path: "notes.txt".to_owned(),
            reason: UnmatchedReason::NoFileRowAndUnparseableLanguage,
        }],
        "a modified path with no file row and no grammar is named, never dropped and never counted"
    );

    let by_path = |path: &str| {
        changes
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap_or_else(|| panic!("{path} has an entry"))
    };
    assert_eq!(
        by_path("keep.rs").sha256.as_deref(),
        Some(sha256_hex(b"alpha changed\n").as_str()),
        "the entry carries the digest of the bytes now in the tree"
    );
    assert_eq!(
        by_path("untracked.rs").sha256.as_deref(),
        Some(sha256_hex(b"untracked\n").as_str()),
        "a path only git reported still gets its current digest"
    );
    assert_eq!(
        by_path("gone.rs").sha256,
        None,
        "a deleted path has no bytes to digest"
    );
    assert!(
        entry_shapes(&changes)
            .iter()
            .all(|(path, _, _, has_digest)| path == "gone.rs"
                || path == "old_name.rs"
                || *has_digest),
        "every surviving path reports a digest"
    );
}

#[test]
fn a_directory_git_cannot_read_degrades_to_the_digest_comparison() {
    let temp = TempDir::new("gitless");
    let root = temp.path();
    write_file(root, "clean.rs", "clean\n");
    write_file(root, "drifted.rs", "current\n");
    let indexed = vec![
        FileDigest {
            path: "clean.rs".to_owned(),
            sha256: sha256_hex(b"clean\n"),
        },
        FileDigest {
            path: "drifted.rs".to_owned(),
            sha256: sha256_hex(b"indexed\n"),
        },
        FileDigest {
            path: "gone.rs".to_owned(),
            sha256: sha256_hex(b"gone\n"),
        },
        // A row that escapes the root, and an absolute one: both are data from a database and must
        // fail the containment test rather than being read or reported as removed.
        FileDigest {
            path: "../escape.rs".to_owned(),
            sha256: sha256_hex(b"escape\n"),
        },
        FileDigest {
            path: "/etc/passwd".to_owned(),
            sha256: sha256_hex(b"nope\n"),
        },
    ];

    let changes = changed_since(
        root,
        None,
        &FakeDigests(indexed),
        &ChangedSinceOptions::default(),
    );
    assert!(
        changes.degraded,
        "a directory that is not a checkout has no git answer: {changes:?}"
    );
    assert_eq!(
        changes.changed,
        vec!["drifted.rs".to_owned()],
        "the hash comparison is the whole change set here, and a matching digest is not a change"
    );
    assert_eq!(
        changes.removed,
        vec!["gone.rs".to_owned()],
        "neither escaping row is attributed to this root"
    );
    assert_eq!(
        changes.entries,
        vec![
            ChangeEntry {
                state: ChangeState::Modified,
                path: "drifted.rs".to_owned(),
                from: None,
                sha256: Some(sha256_hex(b"current\n")),
            },
            ChangeEntry {
                state: ChangeState::Deleted,
                path: "gone.rs".to_owned(),
                from: None,
                sha256: None,
            },
        ]
    );
    assert_eq!(head_commit_of(root), None, "not a checkout");

    // Freshness keeps answering with no git and no commit: the digest half still speaks.
    let reader = FakeReader {
        meta: meta(Some(root), "idle", Some("gen-1")),
        generation: Some(generation_row(None, PUBLISHED_AT)),
        files: vec![
            indexed_file("clean.rs", &sha256_hex(b"clean\n")),
            indexed_file("drifted.rs", &sha256_hex(b"indexed\n")),
        ],
    };
    let drift = check_freshness(&reader, &FreshnessOptions::default());
    assert!(drift.stale);
    assert_eq!(drift.stale_reason.as_deref(), Some("files_changed"));
    assert_eq!(drift.files_changed_since, 1);

    let clean = FakeReader {
        files: vec![indexed_file("clean.rs", &sha256_hex(b"clean\n"))],
        ..reader
    };
    let fresh = check_freshness(&clean, &FreshnessOptions::default());
    assert!(
        !fresh.stale,
        "a tree whose bytes match their digests is fresh even without a commit"
    );
    assert_eq!(fresh.stale_reason, None);
    assert!(fresh.stale_checked_at.is_some(), "the check is dated");
}

#[test]
fn head_commit_of_reads_every_head_shape_and_missing_git_is_none() {
    if !git_is_available() {
        eprintln!(
            "skipping head_commit_of_reads_every_head_shape_and_missing_git_is_none: git is not on PATH"
        );
        return;
    }
    let (temp, head) = committed_fixture("head-shapes");
    let root = temp.path();

    // Shape 1: `ref: refs/heads/main`, read through the loose ref file.
    assert_eq!(head_commit_of(root), Some(head.clone()));

    // Shape 2: the same ref after `git gc` packed it away.
    git(root, &["pack-refs", "--all"]);
    let loose = root.join(".git").join("refs").join("heads").join("main");
    if loose.exists() {
        // Some gits keep the loose copy; removing it is what a real gc leaves behind and it is the
        // only way to prove the packed-refs read happened rather than the loose one.
        fs::remove_file(&loose).expect("drop the loose ref after packing");
    }
    assert!(
        root.join(".git").join("packed-refs").exists(),
        "the fixture packed the ref"
    );
    assert_eq!(head_commit_of(root), Some(head.clone()));

    // Shape 3: a detached HEAD is the raw object name in `.git/HEAD`.
    git(root, &["checkout", "-q", "--detach"]);
    assert_eq!(head_commit_of(root), Some(head));

    // Shape 4: no `.git` at all is `None`, not an error.
    let plain = TempDir::new("head-missing");
    assert_eq!(head_commit_of(plain.path()), None);
}

#[test]
fn a_generation_from_another_commit_is_stale_and_the_served_commit_is_fresh() {
    if !git_is_available() {
        eprintln!(
            "skipping a_generation_from_another_commit_is_stale_and_the_served_commit_is_fresh: git is not on PATH"
        );
        return;
    }
    let (temp, head) = committed_fixture("fresh-commit");
    let root = temp.path();
    let reader = FakeReader {
        meta: meta(Some(root), "idle", Some("gen-1")),
        generation: Some(generation_row(
            Some("0000000000000000000000000000000000000000"),
            PUBLISHED_AT,
        )),
        files: Vec::new(),
    };

    let stale = check_freshness(
        &reader,
        &FreshnessOptions {
            now_ms: Some(PUBLISHED_MS),
            repo_root: None,
        },
    );
    assert!(stale.stale);
    assert_eq!(stale.stale_reason.as_deref(), Some("base_commit_moved"));
    assert_eq!(stale.generation, "gen-1");
    assert_eq!(
        stale.indexed_at, PUBLISHED_AT,
        "indexed_at is the newest publish's published_at, the only timestamp the store carries"
    );
    assert_eq!(stale.stale_checked_at.as_deref(), Some(PUBLISHED_AT));
    assert_eq!(stale.files_changed_since, 0);

    // A short recorded commit that prefixes HEAD names the same commit.
    let same = FakeReader {
        generation: Some(generation_row(Some(&head[..12]), PUBLISHED_AT)),
        ..reader
    };
    let fresh = check_freshness(
        &same,
        &FreshnessOptions {
            now_ms: Some(PUBLISHED_MS),
            repo_root: None,
        },
    );
    assert!(!fresh.stale, "the recorded commit is HEAD: {fresh:?}");
    assert_eq!(fresh.stale_reason, None);
}

#[test]
fn a_busy_indexer_stalls_only_after_the_threshold() {
    let reader = FakeReader {
        meta: meta(None, "running", Some("gen-1")),
        generation: Some(generation_row(None, PUBLISHED_AT)),
        files: Vec::new(),
    };
    let at = |offset_ms: i64| FreshnessOptions {
        now_ms: Some(PUBLISHED_MS + offset_ms),
        repo_root: None,
    };

    let threshold = i64::try_from(WATCHER_STALL_MS).expect("the threshold fits an i64");
    let exactly = check_freshness(&reader, &at(threshold));
    assert!(
        !exactly.stale,
        "the comparison is strictly greater than the budget, as the TypeScript's is"
    );
    assert_eq!(exactly.stale_reason, None);

    let past = check_freshness(&reader, &at(threshold + 1));
    assert!(past.stale);
    assert_eq!(past.stale_reason.as_deref(), Some("watcher_stalled"));

    // `idle` is never a stall, however long ago the publish was.
    let idle = FakeReader {
        meta: meta(None, "idle", Some("gen-1")),
        ..reader
    };
    let not_stalled = check_freshness(&idle, &at(threshold + 1));
    assert!(!not_stalled.stale);
    assert_eq!(not_stalled.stale_reason, None);

    assert_eq!(
        STALE_REASONS,
        [
            "none",
            "files_changed",
            "root_mismatch",
            "base_commit_moved",
            "watcher_stalled"
        ],
        "the wire vocabulary is copied verbatim from the TypeScript"
    );
}

#[test]
fn a_recorded_root_that_differs_from_the_callers_root_is_the_first_check() {
    let recorded = TempDir::new("fresh-root-recorded");
    let elsewhere = TempDir::new("fresh-root-caller");
    let reader = FakeReader {
        meta: meta(Some(recorded.path()), "idle", Some("gen-1")),
        generation: Some(generation_row(None, PUBLISHED_AT)),
        files: Vec::new(),
    };

    let mismatch = check_freshness(
        &reader,
        &FreshnessOptions {
            now_ms: Some(PUBLISHED_MS),
            repo_root: Some(elsewhere.path()),
        },
    );
    assert!(mismatch.stale);
    assert_eq!(mismatch.stale_reason.as_deref(), Some("root_mismatch"));
    assert_eq!(
        mismatch.files_changed_since, 0,
        "the digest comparison is meaningless against the wrong tree"
    );

    // No recorded root is not a mismatch: the graph never claimed one.
    let unclaimed = FakeReader {
        meta: meta(None, "idle", Some("gen-1")),
        ..reader
    };
    let verdict = check_freshness(
        &unclaimed,
        &FreshnessOptions {
            now_ms: Some(PUBLISHED_MS),
            repo_root: Some(elsewhere.path()),
        },
    );
    assert!(!verdict.stale);
    assert_eq!(verdict.stale_reason, None);
}

#[test]
fn health_reports_the_indexer_the_publish_duration_and_the_cache_entries() {
    let reader = FakeReader {
        meta: meta(None, "running", Some("gen-1")),
        generation: Some(generation_row(None, PUBLISHED_AT)),
        files: Vec::new(),
    };
    let report = health(&reader);
    assert_eq!(report.indexer.as_deref(), Some("running"));
    assert_eq!(report.last_publish_ms, 42);
    assert_eq!(report.consecutive_failures, 2);
    assert_eq!(
        report.statement_cache_entries,
        sangkar_store::db::statement_cache_stats().size,
        "the process-wide cache count is the number this payload field reports"
    );
}

/// The one timestamp the freshness cases fix: 2024-02-29T12:34:56.789Z.
const PUBLISHED_AT: &str = "2024-02-29T12:34:56.789Z";
const PUBLISHED_MS: i64 = 1_709_210_096_789;

fn meta(project_root: Option<&Path>, indexer_state: &str, generation: Option<&str>) -> GraphMeta {
    GraphMeta {
        schema_version: Some(3),
        generation: generation.map(str::to_owned),
        generation_seq: generation.map(|_| 1),
        indexer_state: Some(indexer_state.to_owned()),
        consecutive_failures: 2,
        project_name: Some("fixture".to_owned()),
        project_root: project_root.map(|root| root.display().to_string()),
        project_branch: Some("main".to_owned()),
        project_remote: None,
        embed_dim: None,
    }
}

fn generation_row(base_commit: Option<&str>, published_at: &str) -> GenerationRow {
    GenerationRow {
        id: "gen-1".to_owned(),
        seq: 1,
        published_at: published_at.to_owned(),
        base_commit: base_commit.map(str::to_owned),
        worktree_dirty: false,
        files_indexed: 0,
        nodes: 0,
        edges: 0,
        duration_ms: 42,
        mode: "full".to_owned(),
        status: "ok".to_owned(),
    }
}

fn indexed_file(path: &str, sha256: &str) -> IndexedFileRow {
    IndexedFileRow {
        id: 1,
        path: path.to_owned(),
        language: "rust".to_owned(),
        sha256: sha256.to_owned(),
        size: 0,
        line_count: 0,
        parse_status: "ok".to_owned(),
        parse_error_ranges: None,
        indexed_generation: "gen-1".to_owned(),
    }
}

/// The reader half of `check_freshness` and `health`: three real answers and nothing else.
#[derive(Clone)]
struct FakeReader {
    meta: GraphMeta,
    generation: Option<GenerationRow>,
    files: Vec<IndexedFileRow>,
}

/// A read this suite never expects: returning an error makes a wrong call a failing assertion
/// instead of a plausible empty default.
macro_rules! unsupported {
    ($(fn $name:ident($($arg:ty),*) -> $ret:ty;)*) => {
        $(
            fn $name(&self $(, _: $arg)*) -> Result<$ret, AppError> {
                Err(AppError::internal(concat!(
                    "the diff_freshness fake does not implement ",
                    stringify!($name)
                )))
            }
        )*
    };
}

impl GraphReader for FakeReader {
    fn meta_for(&self) -> Result<GraphMeta, AppError> {
        Ok(self.meta.clone())
    }

    fn latest_generation(&self) -> Result<Option<GenerationRow>, AppError> {
        Ok(self.generation.clone())
    }

    fn all_files(&self) -> Result<Vec<IndexedFileRow>, AppError> {
        Ok(self.files.clone())
    }

    unsupported! {
        fn find_by_qn(&str) -> Option<NodeRow>;
        fn find_by_name(&str, Option<&[String]>) -> Vec<NodeRow>;
        fn members(i64) -> Vec<NodeRow>;
        fn nodes_by_file(i64) -> Vec<NodeRow>;
        fn inbound(i64, Option<&[String]>) -> Vec<NeighbourEdge>;
        fn outbound(i64, Option<&[String]>) -> Vec<NeighbourEdge>;
        fn metrics_for(i64) -> Option<NodeMetrics>;
        fn rows_for_paths(&[String]) -> PathCoverage;
        fn not_indexed_all() -> Vec<SkippedPathRow>;
        fn not_indexed_count() -> i64;
        fn unresolved_for_files(&[i64]) -> Vec<UnresolvedRef>;
        fn file_by_path(&str) -> Option<IndexedFileRow>;
        fn census_by_language() -> Vec<LanguageCensus>;
        fn tests_census() -> TestCensus;
        fn removed_symbols_for(&str) -> Vec<TombstoneRow>;
        fn entry_points(usize) -> Vec<EntryPointRow>;
        fn hotspots(usize) -> Vec<HotspotRow>;
        fn test_gaps(&[i64]) -> Vec<TestGapRow>;
        fn read_node_fts(&str, usize) -> Vec<FtsHit>;
    }
}
