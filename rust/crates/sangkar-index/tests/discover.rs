//! Discovery's contract is a claim about a tree, so every fixture is a real tree in a temporary
//! directory and the assertions are about what the walk reports for it: one accepted file with a
//! digest this suite states rather than recomputes, and one entry per rejected candidate naming its
//! reason. Ported from `tests/discover.test.ts`.
//!
//! Nothing here writes inside the repository or the reference corpus, and nothing touches the
//! network.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_index::discover::{
    ALWAYS_EXCLUDED, DiscoverOptions, DiscoveredFile, NotIndexedFile, SIZE_CAP_BYTES, SkipReason,
    discover,
};
use sangkar_index::lang::types::LangId;

/// A temporary directory that removes itself, so the suite leaves no tree behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-discover-{label}-{}-{nanos}-{}",
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

    fn join(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// Writes a fixture file, creating the directories above it.
    fn write(&self, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        write_file(&self.0, rel, contents)
    }

    fn mkdir(&self, rel: &str) -> PathBuf {
        mkdir_all(&self.0, rel)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_file(root: &Path, rel: &str, contents: impl AsRef<[u8]>) -> PathBuf {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create the parent directory");
    }
    fs::write(&path, contents).expect("write the fixture file");
    path
}

fn mkdir_all(root: &Path, rel: &str) -> PathBuf {
    let path = root.join(rel);
    fs::create_dir_all(&path).expect("create the fixture directory");
    path
}

/// A directory sealed with mode `000` that reopens itself when it goes out of scope, so a failed
/// assertion still leaves a tree the temp-dir guard can delete.
#[cfg(unix)]
struct Sealed(PathBuf);

#[cfg(unix)]
impl Sealed {
    fn new(path: &Path) -> Self {
        set_mode(path, 0o000);
        Self(path.to_path_buf())
    }
}

#[cfg(unix)]
impl Drop for Sealed {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::metadata(&self.0) {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o755);
            let _ = fs::set_permissions(&self.0, permissions);
        }
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    let mut permissions = fs::metadata(path)
        .expect("stat the fixture directory")
        .permissions();
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).expect("set the fixture directory mode");
}

/// A `.ts` file whose bytes are a NUL probe hit: the shape of a compiled artefact left in place.
const BINARY_BYTES: [u8; 8] = [0x63, 0x6f, 0x64, 0x65, 0x00, 0x01, 0x02, 0x00];

/// `sha256sum` of `export const kept = 1;\n`, the one accepted file below.
const KEPT_SHA256: &str = "cdc4de2ad91aee05d41997e02aa58e2c2478a04a1ec2f6c46b8df3b2e55ebe27";

fn indexed(path: &str, lang: LangId, size: u64, sha256: &str) -> DiscoveredFile {
    DiscoveredFile {
        path: path.to_owned(),
        lang,
        size,
        sha256: sha256.to_owned(),
    }
}

fn rejected(path: &str, reason: SkipReason, language: Option<LangId>) -> NotIndexedFile {
    NotIndexedFile {
        path: path.to_owned(),
        reason,
        language,
    }
}

/// The fixture the first four tests share: one accepted file, one binary, one ignored, one with an
/// unknown extension, and a pruned directory from each control file plus every always-exclude name.
fn seed_fixture(root: &Path) {
    write_file(root, ".gitignore", "ignored.ts\nbuild/\n");
    write_file(root, ".sangkarignore", "generated/\n");
    write_file(root, "keep.ts", "export const kept = 1;\n");
    write_file(root, "ignored.ts", "export const hidden = 2;\n");
    write_file(root, "data.ts", BINARY_BYTES);
    write_file(root, "mystery.weird", "plain text, unknown extension\n");
    for rel in ["build/nested.ts", "generated/nested.ts"] {
        write_file(root, rel, "export const nested = 3;\n");
    }
    for name in ALWAYS_EXCLUDED {
        write_file(
            root,
            &format!("{name}/hidden.ts"),
            "export const nested = 3;\n",
        );
    }
}

#[test]
fn sorts_one_accepted_file_and_three_rejections_by_path_with_their_reasons() {
    let root = TempDir::new("fixture");
    seed_fixture(root.path());

    let result = discover(root.path(), &DiscoverOptions::default());

    assert_eq!(
        result.files,
        vec![indexed(
            "keep.ts",
            LangId::TypeScript,
            "export const kept = 1;\n".len() as u64,
            KEPT_SHA256,
        )]
    );
    // Sorted by path, so `data.ts` precedes `ignored.ts` precedes `mystery.weird`; the pruned
    // directories contribute no entries at all.
    assert_eq!(
        result.not_indexed,
        vec![
            rejected("data.ts", SkipReason::Binary, Some(LangId::TypeScript)),
            rejected("ignored.ts", SkipReason::Ignored, Some(LangId::TypeScript)),
            rejected("mystery.weird", SkipReason::UnsupportedExt, None),
        ]
    );
    assert_eq!(result.skipped, result.not_indexed.len());
}

#[test]
fn is_a_pure_function_of_the_tree() {
    let root = TempDir::new("pure");
    seed_fixture(root.path());

    let first = discover(root.path(), &DiscoverOptions::default());
    let second = discover(root.path(), &DiscoverOptions::default());

    assert_eq!(second, first);
}

#[test]
fn reports_repo_relative_forward_slash_paths_with_no_leading_dot_slash() {
    let root = TempDir::new("paths");
    root.write("src/deep/leaf.ts", "export const leaf = 1;\n");

    let result = discover(root.path(), &DiscoverOptions::default());

    let paths: Vec<&str> = result
        .files
        .iter()
        .map(|file| file.path.as_str())
        .chain(result.not_indexed.iter().map(|entry| entry.path.as_str()))
        .collect();
    assert_eq!(paths, vec!["src/deep/leaf.ts"]);
    for path in paths {
        assert!(!path.starts_with('/'));
        assert!(!path.starts_with("./"));
        assert!(!path.contains('\\'));
        assert!(!path.starts_with(root.path().to_str().expect("a utf-8 temp path")));
    }
}

#[test]
fn caps_by_stat_before_reading_so_an_oversized_file_is_never_opened() {
    let root = TempDir::new("size-cap");
    let oversized = root.write("huge.ts", "");
    // Sparse, so the size is real to `stat` without 512 MiB of writes.
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&oversized)
        .expect("open the oversized fixture");
    file.set_len(SIZE_CAP_BYTES + 1)
        .expect("truncate the fixture");
    drop(file);

    let result = discover(root.path(), &DiscoverOptions::default());

    assert!(result.files.is_empty());
    assert_eq!(
        result.not_indexed,
        vec![rejected(
            "huge.ts",
            SkipReason::SizeCap,
            Some(LangId::TypeScript)
        )]
    );
}

#[test]
fn honours_negation_and_applies_caller_patterns_after_the_control_files() {
    let root = TempDir::new("negation");
    root.write(".gitignore", "keep.ts\n!notes.ts\n");
    root.write("keep.ts", "export const hidden = 1;\n");
    root.write("notes.ts", "export const kept = 2;\n");
    root.write("extra.ts", "export const extra = 3;\n");

    // `keep.ts` is dropped by the pattern, `notes.ts` is re-included by the later negation.
    let from_control_file = discover(root.path(), &DiscoverOptions::default());
    assert_eq!(
        from_control_file
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["extra.ts", "notes.ts"]
    );
    assert_eq!(
        from_control_file.not_indexed,
        vec![rejected(
            "keep.ts",
            SkipReason::Ignored,
            Some(LangId::TypeScript)
        )]
    );

    // The caller's patterns come last, so they win over both control files.
    let with_option = discover(
        root.path(),
        &DiscoverOptions {
            ignore_patterns: vec!["extra.ts".to_owned()],
        },
    );
    assert_eq!(
        with_option
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["notes.ts"]
    );
    assert_eq!(
        with_option.not_indexed,
        vec![
            rejected("extra.ts", SkipReason::Ignored, Some(LangId::TypeScript)),
            rejected("keep.ts", SkipReason::Ignored, Some(LangId::TypeScript)),
        ]
    );
}

/// A nested rule set, spelled entirely in the two root control files: `**/` as "any depth",
/// `**` inside a pattern, an anchored directory, a bare glob, a comment and a cross-file negation.
#[test]
fn honours_a_nested_rule_set_from_the_root_control_files() {
    let root = TempDir::new("nested-rules");
    root.write(
        ".gitignore",
        "# build output, drafts and vendored copies\n**/generated/*.ts\ndocs/**/draft-*.md\n\
         /src/vendored/\n*.min.js\n*.tmp\n",
    );
    root.write(".sangkarignore", "!src/build.min.js\n");

    let sources = [
        "src/app.ts",
        "src/build.min.js",
        "src/generated/api.ts",
        "src/generated/keep.ts",
        "src/deep/generated/api.ts",
        "src/vendored/lib.ts",
        "other/src/vendored/lib.ts",
        "src/cache.tmp",
        "src/notes.txt",
        "docs/guide/draft-a.md",
        "docs/guide/deep/draft-b.md",
        "docs/guide/notes.md",
    ];
    for rel in sources {
        root.write(rel, "export const value = 1;\n");
    }

    let result = discover(root.path(), &DiscoverOptions::default());

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec![
            "other/src/vendored/lib.ts",
            "src/app.ts",
            // `*.min.js` matched it and `.sangkarignore`'s negation re-included it: last one wins.
            "src/build.min.js",
            // `src/vendored/lib.ts` lives under a pruned directory, so it is in neither list.
        ]
    );
    assert_eq!(
        result.not_indexed,
        vec![
            rejected("docs/guide/deep/draft-b.md", SkipReason::Ignored, None),
            rejected("docs/guide/draft-a.md", SkipReason::Ignored, None),
            rejected("docs/guide/notes.md", SkipReason::UnsupportedExt, None),
            // A glob that matches an unparseable name keeps the earlier reason, as the fixed order
            // says: ignore rules are decided before the extension.
            rejected("src/cache.tmp", SkipReason::Ignored, None),
            rejected(
                "src/deep/generated/api.ts",
                SkipReason::Ignored,
                Some(LangId::TypeScript)
            ),
            rejected(
                "src/generated/api.ts",
                SkipReason::Ignored,
                Some(LangId::TypeScript)
            ),
            rejected(
                "src/generated/keep.ts",
                SkipReason::Ignored,
                Some(LangId::TypeScript)
            ),
            // Nothing ignores it, so the extension check is what rejects it.
            rejected("src/notes.txt", SkipReason::UnsupportedExt, None),
        ]
    );
}

/// `bare-name/` must prune exactly that name, never its `src-old`-style sibling.
#[test]
fn a_directory_rule_leaves_its_src_old_sibling_alone() {
    let root = TempDir::new("src-old");
    root.write(".gitignore", "src/\n");
    root.write("src/a.ts", "export const inside = 1;\n");
    root.write("src-old/a.ts", "export const sibling = 2;\n");

    let result = discover(root.path(), &DiscoverOptions::default());

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["src-old/a.ts"]
    );
    assert!(result.not_indexed.is_empty());
}

#[cfg(unix)]
#[test]
fn does_not_follow_a_symlink_loop() {
    let root = TempDir::new("symlink");
    root.write("dir/leaf.ts", "export const leaf = 1;\n");
    std::os::unix::fs::symlink("..", root.join("dir/loop")).expect("link the parent directory");
    std::os::unix::fs::symlink("dir", root.join("alias")).expect("link the sibling directory");
    std::os::unix::fs::symlink("dir/leaf.ts", root.join("leaf-link.ts")).expect("link the file");

    let result = discover(root.path(), &DiscoverOptions::default());

    // The real file is indexed once; no link is followed, so neither no-index rows nor a cycle.
    assert_eq!(
        result
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["dir/leaf.ts"]
    );
    assert!(result.not_indexed.is_empty());
}

/// A `0700` directory owned by another user must not end the walk: it is one coverage row. Running
/// as root bypasses the permission bits, so the case is skipped rather than asserted vacuously —
/// with the skip decided by trying to list the directory, so it needs no `getuid` binding.
#[cfg(unix)]
#[test]
fn records_an_unreadable_directory_instead_of_abandoning_the_walk() {
    let root = TempDir::new("unreadable");
    root.write("keep.ts", "export const kept = 1;\n");
    let locked = root.mkdir("locked");
    root.write("locked/secret.ts", "export const secret = 1;\n");

    let sealed = Sealed::new(&locked);
    if fs::read_dir(&locked).is_ok() {
        // Privileged: the mode bits do not apply to this process, so the case cannot be exercised.
        return;
    }

    let result = discover(root.path(), &DiscoverOptions::default());

    assert_eq!(
        result
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        vec!["keep.ts"]
    );
    assert_eq!(
        result.not_indexed,
        vec![rejected("locked", SkipReason::Unreadable, None)]
    );
    assert_eq!(result.skipped, 1);

    // Restored before the temp-dir guard removes the tree: deleting a directory needs to read it.
    drop(sealed);
    assert!(locked.is_dir());
}

/// The closed reason set, all five in one walk, on one tree.
#[cfg(unix)]
#[test]
fn reports_all_five_skip_reasons_in_one_walk() {
    let root = TempDir::new("five-reasons");
    root.write(".gitignore", "ignored.ts\n");
    root.write("ignored.ts", "export const hidden = 1;\n");
    root.write("data.ts", BINARY_BYTES);
    root.write("mystery.weird", "plain text, unknown extension\n");
    let oversized = root.write("huge.ts", "");
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&oversized)
        .expect("open the oversized fixture");
    file.set_len(SIZE_CAP_BYTES + 1)
        .expect("truncate the fixture");
    drop(file);
    let locked = root.mkdir("locked");
    root.write("locked/secret.ts", "export const secret = 1;\n");

    let sealed = Sealed::new(&locked);
    if fs::read_dir(&locked).is_ok() {
        return;
    }

    let result = discover(root.path(), &DiscoverOptions::default());

    let reasons: Vec<SkipReason> = result
        .not_indexed
        .iter()
        .map(|entry| entry.reason)
        .collect();
    assert_eq!(
        reasons,
        vec![
            SkipReason::Binary,
            SkipReason::SizeCap,
            SkipReason::Ignored,
            SkipReason::Unreadable,
            SkipReason::UnsupportedExt,
        ]
    );
    assert_eq!(result.skipped, 5);
    drop(sealed);
}

/// The Done-when: the repository's own `.sangkarignore` keeps `rust/tests/parity/fixtures/` out of a
/// walk of this repository.
///
/// Only `rust/target/` is passed as an extra pattern, and only because it is 5 GiB of build output
/// that `rust/.gitignore` holds and this two-file contract deliberately does not read. Everything
/// else in the tree — including `rust/tests/parity/` — is walked with the repository's real control
/// files, and `rust/tests/parity/README.md` in the result is the proof the walk got that far.
#[test]
fn the_repositorys_sangkarignore_keeps_the_parity_fixtures_out_of_a_walk() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..");
    if !repo_root.join("rust/tests/parity/fixtures").is_dir() {
        // A published crate has no parity corpus; the claim cannot be exercised there.
        return;
    }

    let result = discover(
        &repo_root,
        &DiscoverOptions {
            ignore_patterns: vec!["rust/target/".to_owned()],
        },
    );

    let mut paths: Vec<&str> = result
        .files
        .iter()
        .map(|file| file.path.as_str())
        .chain(result.not_indexed.iter().map(|entry| entry.path.as_str()))
        .collect();
    paths.sort_unstable();

    assert!(
        paths
            .iter()
            .all(|path| !path.starts_with("rust/tests/parity/fixtures/")),
        "no fixture file is indexed or reported: {:?}",
        paths
            .iter()
            .filter(|path| path.starts_with("rust/tests/parity/fixtures"))
            .collect::<Vec<_>>()
    );
    assert!(
        paths.contains(&"rust/tests/parity/README.md"),
        "the walk reached rust/tests/parity/"
    );
    assert!(
        result
            .files
            .iter()
            .any(|file| file.path == "src/indexer/discover.ts"),
        "and indexed the port's source"
    );
}
