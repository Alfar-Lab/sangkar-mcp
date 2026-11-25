//! Which files the graph can contain, and why every other candidate was left out.
//!
//! Ported from `src/indexer/discover.ts`. Discovery decides coverage, so one walk answers both
//! questions: the indexed files (with the digest the incremental planner keys on) and one row per
//! rejected path naming its reason. A path dropped without a reason would be an invisible hole in
//! coverage, which the payload contract forbids.
//!
//! The result is a pure function of the tree's bytes and names: no timestamps, no `readdir` order,
//! no locale collation. Two walks over the same tree produce identical output.
//!
//! Two documented divergences from the TypeScript come from an infallible signature: the
//! TypeScript throws for a repo root that is not a directory and for a `stat` or `read` that fails
//! unexpectedly, while [`discover`] reports the root as an error line and an empty result, and
//! records an unreadable file as a coverage row instead of abandoning a walk that may already have
//! hashed gigabytes.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use sangkar_core::log;
use sha2::{Digest, Sha256};

use crate::lang::registry::detect_language;
use crate::lang::types::LangId;

/// Directories never walked, at any depth, whatever the ignore files say.
pub const ALWAYS_EXCLUDED: [&str; 5] = [".git", "node_modules", ".sangkar", "dist", "vendor"];

/// Files larger than this are not read, not hashed and not indexed.
pub const SIZE_CAP_BYTES: u64 = 512 * 1024 * 1024;

/// The NUL probe window; a byte zero anywhere in it makes the file binary.
const BINARY_PROBE_BYTES: usize = 8 * 1024;

/// The read size the digest is streamed in, so a 500 MiB file never becomes a 500 MiB allocation.
const HASH_CHUNK_BYTES: usize = 1024 * 1024;

/// The two control files, read relative to the repository root only.
const CONTROL_FILES: [&str; 2] = [".gitignore", ".sangkarignore"];

/// Why a path the walk saw is not part of the graph. The set is closed on purpose.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// A rule from a control file (or the caller's extras) matched, last match winning.
    Ignored,
    /// A NUL byte in the first `BINARY_PROBE_BYTES`, so the bytes are not source text.
    Binary,
    /// `stat` said the file is larger than [`SIZE_CAP_BYTES`]; it was never opened.
    SizeCap,
    /// No extension in the twelve-language table, so there is no grammar to parse it with.
    UnsupportedExt,
    /// A directory that could not be listed, or a file that could not be statted or read. It is the
    /// one reason whose path is usually a directory: a hole nobody can see is worse than a hole
    /// that is reported, so an unreadable directory is a coverage row rather than the end of the
    /// walk.
    Unreadable,
}

impl SkipReason {
    /// The wire spelling, identical to the TypeScript union's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Binary => "binary",
            Self::SizeCap => "size_cap",
            Self::UnsupportedExt => "unsupported_ext",
            Self::Unreadable => "unreadable",
        }
    }
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One indexed file. `path` is repo-relative, forward-slashed, with no leading `./`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredFile {
    /// Repo-relative path with forward slashes.
    pub path: String,
    /// The language whose grammar parses it, as detected from the extension.
    pub lang: LangId,
    /// Bytes on disk at discovery time; the parser reads exactly this file.
    pub size: u64,
    /// Lowercase hex sha256 of the whole file, the incremental planner's change key.
    pub sha256: String,
}

/// One path that was classified and rejected, with the reason and, when known, its language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotIndexedFile {
    /// Repo-relative path with forward slashes; a directory only for [`SkipReason::Unreadable`].
    pub path: String,
    /// Why the path is not in the graph.
    pub reason: SkipReason,
    /// Known whenever the extension is one of the twelve, so a report can say "4 ignored
    /// TypeScript files".
    pub language: Option<LangId>,
}

/// Everything one walk has to say about a tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverResult {
    /// Indexed files, sorted by path.
    pub files: Vec<DiscoveredFile>,
    /// Rejected paths, sorted by path.
    pub not_indexed: Vec<NotIndexedFile>,
    /// How many paths the walk refused to index. It always equals `not_indexed.len()`; it exists so
    /// a caller can log a count without materialising the list, and so it is obvious that every
    /// drop is accounted for. Directories pruned from the walk are not paths and are not counted;
    /// the one exception is a directory that could not be *read*, which is a coverage row with
    /// reason `unreadable` because nothing else would report it.
    pub skipped: usize,
}

/// The knobs a caller has over the walk.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiscoverOptions {
    /// Extra gitignore-style patterns, applied after the two control files, so they win.
    pub ignore_patterns: Vec<String>,
}

/// Walks `repo_root` and classifies every candidate: files to index, and paths not to.
///
/// The result is sorted by path in byte-wise string order — never `localeCompare`, because a locale
/// must never decide graph order. A root that is not a directory is reported as an error line and
/// yields an empty result rather than a panic: the caller has already resolved the root, and a
/// daemon that unwound here would take the session with it.
#[must_use]
pub fn discover(repo_root: &Path, opts: &DiscoverOptions) -> DiscoverResult {
    match fs::metadata(repo_root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            log::error(
                &format!("discover.not_a_directory: root={}", repo_root.display()),
                &[],
            );
            return DiscoverResult::default();
        }
        Err(error) => {
            log::error(
                &format!(
                    "discover.root_unreadable: root={} error={error}",
                    repo_root.display()
                ),
                &[],
            );
            return DiscoverResult::default();
        }
    }

    let mut walk = Walk {
        repo_root,
        rules: load_ignore_rules(repo_root, &opts.ignore_patterns),
        files: Vec::new(),
        not_indexed: Vec::new(),
        probe: vec![0; BINARY_PROBE_BYTES],
        chunk: vec![0; HASH_CHUNK_BYTES],
    };
    walk.walk_dir("");

    let mut result = DiscoverResult {
        files: walk.files,
        not_indexed: walk.not_indexed,
        skipped: 0,
    };
    result
        .files
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
        .not_indexed
        .sort_by(|left, right| left.path.cmp(&right.path));
    result.skipped = result.not_indexed.len();
    result
}

/// One walk's accumulating state, so the digest buffers are allocated once per tree rather than
/// once per file.
struct Walk<'a> {
    repo_root: &'a Path,
    rules: Vec<IgnoreRule>,
    files: Vec<DiscoveredFile>,
    not_indexed: Vec<NotIndexedFile>,
    probe: Vec<u8>,
    chunk: Vec<u8>,
}

impl Walk<'_> {
    fn walk_dir(&mut self, rel_dir: &str) {
        let abs_dir: PathBuf = if rel_dir.is_empty() {
            self.repo_root.to_path_buf()
        } else {
            self.repo_root.join(rel_dir)
        };
        let read = match fs::read_dir(&abs_dir) {
            Ok(read) => read,
            Err(error) => {
                self.record_unreadable_dir(rel_dir, &error);
                return;
            }
        };

        let mut entries: Vec<(String, bool)> = Vec::new();
        for entry in read {
            let entry = match entry {
                Ok(entry) => entry,
                // Nothing here names a path, so there is no coverage row to write for it.
                Err(error) => {
                    log::warn(
                        &format!("discover.entry_failed: dir={rel_dir} error={error}"),
                        &[],
                    );
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    log::warn(
                        &format!("discover.entry_failed: dir={rel_dir} error={error}"),
                        &[],
                    );
                    continue;
                }
            };

            // Symlinks are not followed: a link can point outside the tree or create a cycle, and
            // the indexed bytes must belong to the repository being indexed.
            if file_type.is_symlink() {
                continue;
            }
            let is_dir = file_type.is_dir();
            if !is_dir && !file_type.is_file() {
                // A fifo, socket or device node is not source text and cannot be read to an end.
                continue;
            }

            // A name that is not UTF-8 goes through the same replacement decoding Node applies, so
            // the row's path matches the TypeScript's; the later `stat` or `open` then fails on it,
            // which records the path as unreadable instead of dropping it silently.
            entries.push((entry.file_name().to_string_lossy().into_owned(), is_dir));
        }

        // `readdir` order is a filesystem detail, so the walk sorts to stay a pure function of the
        // tree. `String`'s ordering is byte-wise, and names are unique within a directory.
        entries.sort_unstable();

        for (name, is_dir) in entries {
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };

            if is_dir {
                if ALWAYS_EXCLUDED.contains(&name.as_str()) || is_ignored(&rel, true, &self.rules) {
                    continue;
                }
                self.walk_dir(&rel);
                continue;
            }

            // The control files describe the walk itself; they are configuration, not content, so
            // they are neither indexed nor reported as a coverage gap.
            if rel_dir.is_empty() && CONTROL_FILES.contains(&name.as_str()) {
                continue;
            }

            self.classify(&rel);
        }
    }

    /// Classification order is fixed and cheapest-first: ignore rules (a path decision, no I/O),
    /// then the extension (no I/O), then size (one `stat`, before any read so a 1 GiB file is never
    /// opened), then the binary probe, then hashing. The first decisive check wins, so the same file
    /// always gets the same reason regardless of what else is wrong with it.
    fn classify(&mut self, rel: &str) {
        let language = detect_language(rel);

        if is_ignored(rel, false, &self.rules) {
            self.record(rel, SkipReason::Ignored, language);
            return;
        }

        let Some(lang) = language else {
            self.record(rel, SkipReason::UnsupportedExt, None);
            return;
        };

        let abs = self.repo_root.join(rel);
        let size = match fs::metadata(&abs) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                log::warn(
                    &format!("discover.stat_failed: path={rel} error={error}"),
                    &[],
                );
                self.record(rel, SkipReason::Unreadable, Some(lang));
                return;
            }
        };
        if size > SIZE_CAP_BYTES {
            self.record(rel, SkipReason::SizeCap, Some(lang));
            return;
        }

        match self.hash_unless_binary(&abs) {
            Ok(Some(sha256)) => self.files.push(DiscoveredFile {
                path: rel.to_owned(),
                lang,
                size,
                sha256,
            }),
            Ok(None) => self.record(rel, SkipReason::Binary, Some(lang)),
            Err(error) => {
                log::warn(
                    &format!("discover.read_failed: path={rel} error={error}"),
                    &[],
                );
                self.record(rel, SkipReason::Unreadable, Some(lang));
            }
        }
    }

    /// A NUL byte in the first `BINARY_PROBE_BYTES`, or the sha256 of the whole file; `Ok(None)`
    /// means binary. The probe bytes are the digest's first bytes too, so no byte is read twice.
    fn hash_unless_binary(&mut self, abs: &Path) -> std::io::Result<Option<String>> {
        let mut file = File::open(abs)?;

        let mut head_len = 0;
        while head_len < BINARY_PROBE_BYTES {
            let read = file.read(&mut self.probe[head_len..])?;
            if read == 0 {
                break;
            }
            head_len += read;
        }
        if self.probe[..head_len].contains(&0) {
            return Ok(None);
        }

        let mut hasher = Sha256::new();
        hasher.update(&self.probe[..head_len]);
        loop {
            let read = file.read(&mut self.chunk)?;
            if read == 0 {
                break;
            }
            hasher.update(&self.chunk[..read]);
        }
        Ok(Some(to_hex(&hasher.finalize())))
    }

    fn record(&mut self, path: &str, reason: SkipReason, language: Option<LangId>) {
        self.not_indexed.push(NotIndexedFile {
            path: path.to_owned(),
            reason,
            language,
        });
    }

    /// The root is never skipped: [`discover`] has statted it already, so failing to list it is the
    /// caller's problem, while a subdirectory that cannot be listed is one coverage row.
    fn record_unreadable_dir(&mut self, rel_dir: &str, error: &std::io::Error) {
        if rel_dir.is_empty() {
            log::error(&format!("discover.root_unreadable: error={error}"), &[]);
            return;
        }
        log::warn(
            &format!("discover.unreadable: path={rel_dir} error={error}"),
            &[],
        );
        self.record(rel_dir, SkipReason::Unreadable, None);
    }
}

/// One parsed ignore line. `anchored` mirrors gitignore: a slash anywhere but the end binds the
/// pattern to the repository root.
struct IgnoreRule {
    anchored: bool,
    dir_only: bool,
    negated: bool,
    glob: Vec<char>,
}

impl IgnoreRule {
    /// The supported glob subset from one line, or `None` for a blank line, a comment or an empty
    /// pattern.
    ///
    /// `*` and `?` stay inside one segment, `**` crosses segments, a leading `/` binds to the root,
    /// a trailing `/` matches directories only, `#` starts a comment and `!` negates (last matching
    /// rule wins). Character classes and backslash escapes are not supported — none of them appears
    /// in the `#`-free patterns a repository actually uses to hide build output.
    fn compile(line: &str) -> Option<Self> {
        let pattern = line.trim_end();
        if pattern.is_empty() || pattern.starts_with('#') {
            return None;
        }

        let (negated, pattern) = match pattern.strip_prefix('!') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };
        let (dir_only, pattern) = match pattern.strip_suffix('/') {
            Some(rest) => (true, rest),
            None => (false, pattern),
        };

        // Anchor before stripping the leading slash, so `/build` stays root-relative.
        let anchored = pattern.contains('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.is_empty() {
            return None;
        }

        Some(Self {
            anchored,
            dir_only,
            negated,
            glob: pattern.chars().collect(),
        })
    }

    fn matches(&self, subject: &[char]) -> bool {
        glob_matches(&self.glob, subject)
    }
}

/// The rules from `<root>/.gitignore`, then `<root>/.sangkarignore`, then the caller's extras, which
/// therefore win.
fn load_ignore_rules(repo_root: &Path, extra_patterns: &[String]) -> Vec<IgnoreRule> {
    let mut rules = Vec::new();
    for name in CONTROL_FILES {
        rules.extend(compile_lines(&read_control_file(&repo_root.join(name))));
    }
    rules.extend(compile_lines(extra_patterns));
    rules
}

/// Lines of a control file; a missing file is an empty list, not a failure. The bytes are decoded
/// lossily because that is what Node's `utf8` read does, so a stray byte in a `.gitignore` cannot
/// cost the walk every rule in the file.
fn read_control_file(abs: &Path) -> Vec<String> {
    match fs::read(abs) {
        Ok(bytes) => String::from_utf8_lossy(&bytes)
            .lines()
            .map(ToOwned::to_owned)
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            log::warn(
                &format!(
                    "discover.control_file_unreadable: path={} error={error}",
                    abs.display()
                ),
                &[],
            );
            Vec::new()
        }
    }
}

fn compile_lines<'a>(lines: impl IntoIterator<Item = &'a String>) -> Vec<IgnoreRule> {
    lines
        .into_iter()
        .filter_map(|line| IgnoreRule::compile(line))
        .collect()
}

/// A path is ignored when the last matching rule is not a negation. A pattern without a slash
/// matches the entry's own name, which is how `build/` hides `a/b/build` as well as `build` — and
/// why it leaves the sibling `build-old` alone.
fn is_ignored(rel: &str, is_dir: bool, rules: &[IgnoreRule]) -> bool {
    if rules.is_empty() {
        return false;
    }

    let characters: Vec<char> = rel.chars().collect();
    let name_start = characters
        .iter()
        .rposition(|character| *character == '/')
        .map_or(0, |slash| slash + 1);
    let name = &characters[name_start..];

    let mut ignored = false;
    for rule in rules {
        if rule.dir_only && !is_dir {
            continue;
        }
        let subject = if rule.anchored { &characters[..] } else { name };
        if !rule.matches(subject) {
            continue;
        }
        ignored = !rule.negated;
    }
    ignored
}

/// The glob subset, matched against a whole subject.
///
/// Recursive rather than compiled to a regular expression because the workspace carries no regular
/// expression crate for this crate, and because a pattern here is a few characters long: the
/// backtracking is bounded by the subject's length for every pattern a repository writes.
fn glob_matches(pattern: &[char], subject: &[char]) -> bool {
    let Some(&head) = pattern.first() else {
        return subject.is_empty();
    };

    if head == '*' {
        if pattern.get(1) == Some(&'*') {
            let rest = &pattern[2..];
            if rest.first() == Some(&'/') {
                // `a/**/b` matches `a/b` as well as `a/x/y/b`: zero or more whole segments.
                let rest = &rest[1..];
                return glob_matches(rest, subject)
                    || subject.iter().enumerate().any(|(index, character)| {
                        *character == '/' && glob_matches(rest, &subject[index + 1..])
                    });
            }
            // A bare `**` crosses segment boundaries, so it may consume anything.
            return (0..=subject.len()).any(|index| glob_matches(rest, &subject[index..]));
        }
        // `*` stays inside one segment: the consumed prefix may not contain a slash.
        let rest = &pattern[1..];
        return (0..=subject.len())
            .take_while(|index| *index == 0 || subject[index - 1] != '/')
            .any(|index| glob_matches(rest, &subject[index..]));
    }

    if head == '?' {
        return subject.first().is_some_and(|character| *character != '/')
            && glob_matches(&pattern[1..], &subject[1..]);
    }

    subject.first() == Some(&head) && glob_matches(&pattern[1..], &subject[1..])
}

const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

/// A digest as the lowercase hex the `sha256` field carries.
fn to_hex(digest: &[u8]) -> String {
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{IgnoreRule, SkipReason, glob_matches, is_ignored, to_hex};

    /// The digest of `bytes` as the lowercase hex the walk stores.
    fn hex_of(bytes: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        to_hex(&hasher.finalize())
    }

    /// The digest vectors are `sha256sum` of exactly these bytes, including the two padding
    /// boundaries (55 and 56 bytes) and a payload past one 64-byte block.
    #[test]
    fn sha256_matches_the_reference_digests() {
        let cases: [(&[u8], &str); 6] = [
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"export const kept = 1;\n",
                "cdc4de2ad91aee05d41997e02aa58e2c2478a04a1ec2f6c46b8df3b2e55ebe27",
            ),
            (
                &[b'a'; 55],
                "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318",
            ),
            (
                &[b'a'; 56],
                "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a",
            ),
            (
                &[b'a'; 64],
                "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb",
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(hex_of(bytes), expected, "digest of {} bytes", bytes.len());
        }
    }

    /// A digest fed one byte at a time and one fed a whole 2 MiB buffer in two pieces must agree:
    /// the streaming path is the one the walk takes for a file larger than `HASH_CHUNK_BYTES`.
    #[test]
    fn sha256_is_independent_of_how_the_bytes_are_split() {
        let payload: Vec<u8> = (0..2_000_000_u32)
            .map(|index| (index % 251) as u8)
            .collect();

        let mut whole = Sha256::new();
        whole.update(&payload);

        let mut fed = Sha256::new();
        for byte in &payload {
            fed.update([*byte]);
        }

        assert_eq!(to_hex(&whole.finalize()), to_hex(&fed.finalize()));
    }

    #[test]
    fn to_hex_pads_every_nibble() {
        let mut digest = [0_u8; 32];
        digest[0] = 0x0f;
        digest[31] = 0xa0;
        let hex = to_hex(&digest);
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("0f00"));
        assert!(hex.ends_with("00a0"));
    }

    /// The glob subset: `*` and `?` stay in a segment, `**` crosses, `**/` is zero or more whole
    /// segments.
    #[test]
    fn glob_subset_matches_segment_by_segment() {
        let matches = |pattern: &str, subject: &str| -> bool {
            let pattern: Vec<char> = pattern.chars().collect();
            let subject: Vec<char> = subject.chars().collect();
            glob_matches(&pattern, &subject)
        };

        assert!(matches("*.ts", "keep.ts"));
        assert!(
            !matches("*.ts", "src/keep.ts"),
            "`*` does not cross a slash"
        );
        assert!(matches("?.ts", "a.ts"));
        assert!(!matches("?.ts", "ab.ts"));
        assert!(!matches("?.ts", "/.ts"), "`?` does not match a slash");
        assert!(matches("src/*.ts", "src/a.ts"));
        assert!(
            matches("**/keep.ts", "keep.ts"),
            "`**/` also matches zero segments"
        );
        assert!(matches("**/keep.ts", "a/b/keep.ts"));
        assert!(matches("src/**/keep.ts", "src/keep.ts"));
        assert!(matches("src/**/keep.ts", "src/a/b/keep.ts"));
        assert!(
            matches("src/**", "src/a/b.ts"),
            "a bare `**` consumes anything"
        );
        assert!(matches("*.m?ts", "a.m1ts"));
        // `?` counts characters, not bytes.
        assert!(matches("?.ts", "é.ts"));
    }

    /// Compilation rules: comments, a trailing-space trim, negation, anchoring and directory-only.
    #[test]
    fn rules_are_compiled_the_way_gitignore_reads_them() {
        assert!(IgnoreRule::compile("").is_none());
        assert!(IgnoreRule::compile("   ").is_none());
        assert!(IgnoreRule::compile("# a comment").is_none());
        assert!(IgnoreRule::compile("!").is_none());
        assert!(IgnoreRule::compile("/").is_none());

        let rules: Vec<IgnoreRule> = ["build/", "/root-only", "*.min.js   ", "!keep.min.js"]
            .into_iter()
            .filter_map(IgnoreRule::compile)
            .collect();

        assert!(
            is_ignored("build", true, &rules),
            "a trailing slash prunes the directory"
        );
        assert!(
            !is_ignored("build", false, &rules),
            "and not a file of that name"
        );
        assert!(
            is_ignored("a/b/build", true, &rules),
            "a bare name matches at any depth"
        );
        assert!(
            !is_ignored("build-old", true, &rules),
            "a whole-name match leaves the sibling alone"
        );
        assert!(
            is_ignored("root-only", true, &rules),
            "a leading slash anchors to the entry at the root"
        );
        assert!(
            !is_ignored("a/root-only", true, &rules),
            "an anchored rule does not float to another depth"
        );
        assert!(is_ignored("a/b.min.js", false, &rules));
        assert!(
            !is_ignored("a/keep.min.js", false, &rules),
            "the last matching rule wins, so a negation re-includes"
        );
    }

    #[test]
    fn skip_reason_spellings_are_the_payload_strings() {
        let spellings: [&str; 5] = [
            SkipReason::Ignored.as_str(),
            SkipReason::Binary.as_str(),
            SkipReason::SizeCap.as_str(),
            SkipReason::UnsupportedExt.as_str(),
            SkipReason::Unreadable.as_str(),
        ];
        assert_eq!(
            spellings,
            [
                "ignored",
                "binary",
                "size_cap",
                "unsupported_ext",
                "unreadable"
            ]
        );
    }
}
