//! The one change-set computation: what differs from `base`, what is gone, and what could not be
//! attributed to a file row. `summary(change)` and `impact` both read this and neither recomputes
//! it (plan.constraint_invariant.md: one change-set, never one per caller).
//!
//! Ported from `src/shared/domain/diff.ts`. Three reads are unioned, because none is right alone:
//!
//! - `git diff --name-status` knows what changed against a commit, including deletions and renames,
//!   and it still sees paths the indexer never parsed (a modified `README.md`). It never sees
//!   untracked files.
//! - `git status --porcelain -z --untracked-files=all` is the second git read, and it exists only
//!   because the diff cannot see a file git does not track yet: it contributes untracked paths and
//!   nothing else (the diff carries every tracked state). `git status` accepts neither `--relative`
//!   nor `--no-ext-diff` (git 2.53.0 rejects both as unknown options), so the sub-root scope comes
//!   from `rev-parse --show-prefix`.
//! - hashing every `file` row's working-tree bytes against `file.sha256` — the column the indexer
//!   already stores — is the only source in a directory git cannot read, and the only one that
//!   notices a changed indexed file that git ignores.
//!
//! Three traps are closed once here, exactly as the TypeScript closes them:
//!
//! - a git failure (no repository, no binary, timeout) degrades to the hash comparison with
//!   [`ChangeSet::degraded`] set rather than throwing: the integration fixture is a plain temp
//!   directory;
//! - this module writes nothing: git runs with `--no-optional-locks` so it cannot take the
//!   optional index lock, `--no-ext-diff` so a target repo's own config cannot run a command, and
//!   a closed stdin plus cleared credential/askpass helpers so it cannot prompt. The only
//!   filesystem calls are reads of working-tree bytes, so the target repo's tracked files are
//!   never touched;
//! - the environment is never read here. The child inherits the parent environment (the
//!   TypeScript's `Bun.spawn` default) and is kept non-interactive by the two `-c` overrides, so no
//!   caller has to supply one.
//!
//! Two shapes deliberately differ from the TypeScript's:
//!
//! - [`ChangeSet`] carries `degraded: bool` but not the TypeScript's `degradedReason`: no payload
//!   renders the reason, so it goes to [`sangkar_core::log`] where a maintainer reads it and the
//!   contract stays one flag.
//! - [`ChangeEntry`] carries `sha256`: the digest of the working-tree bytes this call read at
//!   `path`, so a tool can show what the bytes are now without a second pass. It is `None` for a
//!   deleted path and for a file that could not be read.
//!
//! `sha256` is hand-rolled ([`sha256_hex`]) because this crate may not gain a dependency and the
//! comparison is against digests the indexer wrote; a hashing crate added for it would be the
//! wrong dependency, the same call `sangkar-store`'s publish tests already made.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use sangkar_core::errors::AppError;
use sangkar_core::log;
use sangkar_core::paths::normalize_rel;
use sangkar_store::read::reader::GraphReader;

/// The hard bound on one `git` invocation: a hung git must never hang a tool call.
pub const GIT_TIMEOUT_MS: u64 = 30_000;

/// The argv every git read is prefixed with, copied verbatim from `diff.ts`.
///
/// This is a security decision as much as a parity one: `--no-pager` and `--no-optional-locks`
/// keep the target repo's config from starting a pager or taking the index lock, and the two `-c`
/// overrides leave the child no credential helper or askpass program to prompt through. `-C <root>`
/// is what makes a relative root resolve the way the caller meant.
const GIT_PREFIX: [&str; 7] = [
    "--no-pager",
    "--no-optional-locks",
    "-c",
    "credential.helper=",
    "-c",
    "core.askpass=",
    "-C",
];

/// How often the wait loop asks whether the child has exited. A git read answers in milliseconds,
/// so the cost is a handful of `try_wait` calls; the poll is what gives the timeout teeth without a
/// `wait-timeout` dependency.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long a pipe reader may take to report after the child was reaped. A `git` that exited has
/// closed its own descriptors, so the collection is immediate unless a grandchild is holding the
/// pipe; the grace turns that pathological case into a degraded answer instead of a hang.
const COLLECT_GRACE: Duration = Duration::from_secs(1);

/// Why a path is in [`ChangeSet::unmatched`]: we know it changed but cannot say what changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UnmatchedReason {
    /// Git reported it, there is no `file` row, and no MVP grammar covers its extension.
    NoFileRowAndUnparseableLanguage,
    /// An indexed path whose working-tree bytes could not be read while hashing.
    UnreadableFile,
}

impl UnmatchedReason {
    /// The wire spelling, copied from the TypeScript union.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoFileRowAndUnparseableLanguage => "no_file_row_and_unparseable_language",
            Self::UnreadableFile => "unreadable_file",
        }
    }
}

/// A changed path that is neither in the graph nor parseable, kept instead of dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnmatchedChange {
    pub path: String,
    pub reason: UnmatchedReason,
}

/// What happened to one path in the change set, in the vocabulary a payload can render.
///
/// `Untracked` is the fifth state and only `git status` can report it: the bytes are in the working
/// tree and git records nothing for the path, so the diff has no other side to compare against. It
/// is a state of its own rather than `Added`, because `Added` means "git has this in its index and
/// `base` did not", which is a different fact from "git has never seen this".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeState {
    Modified,
    Added,
    Deleted,
    Renamed,
    Untracked,
}

impl ChangeState {
    /// The wire spelling, copied from the TypeScript union.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Modified => "modified",
            Self::Added => "added",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
            Self::Untracked => "untracked",
        }
    }
}

/// One path of the change set with its state.
///
/// A rename is a single entry — the destination path plus where it came from — because git's two
/// halves of a rename are one fact, and a payload that rendered them as "one added, one deleted"
/// would lose that. A copy is [`ChangeState::Added`]: its source still exists, so there is nothing
/// to attribute away from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeEntry {
    pub state: ChangeState,
    /// The destination path: the new name of a rename, the new file of a copy. Repo-relative.
    pub path: String,
    /// The path a rename came from; `None` for every other state. Repo-relative.
    pub from: Option<String>,
    /// The digest of the bytes this call read at `path`; `None` for a deleted or unreadable path.
    pub sha256: Option<String>,
}

/// The change set both change-aware tools read; the arrays are sorted and repo-relative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSet {
    /// Differ from `base`, or no longer match `file.sha256`, or are in the working tree with
    /// nothing in git's index (`untracked`); sorted, forward slashes.
    pub changed: Vec<String>,
    /// Had a `file` row and their bytes are gone from the working tree; sorted.
    pub removed: Vec<String>,
    /// Every path in `changed` and `removed` with its state, sorted by path; the one view that can
    /// say "renamed from". A path in `unmatched` has no entry — it is not attributed to a file
    /// yet, and its reason lives in `unmatched`. A path outside the repo root appears in neither.
    pub entries: Vec<ChangeEntry>,
    pub unmatched: Vec<UnmatchedChange>,
    /// `true` when git was unusable, so the set rests on the hash comparison — and on nothing at
    /// all when the digest source failed too. A digest-read failure is reported by the log, not by
    /// this flag: the flag is the one fact a payload renders.
    pub degraded: bool,
}

/// Every knob the change set has. One field, because the module reads no environment and no clock:
/// `git_timeout_ms` defaults to [`GIT_TIMEOUT_MS`] and is never unbounded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChangedSinceOptions {
    pub git_timeout_ms: Option<u64>,
}

/// The digest half of the change set, with its source already bound.
///
/// [`GraphReader`] satisfies this through [`ReaderDigests`], which is what lets a tool handler run
/// this module at all: a handler holds a reader and never a connection. The method is fallible
/// because the reader's own is; [`changed_since`] answers without the digest half (and says so in
/// the log) rather than pretending the graph has no files.
pub trait DigestSource {
    /// Every indexed path with the digest the indexer stored for it.
    fn all_files(&self) -> Result<Vec<FileDigest>, AppError>;
}

/// One indexed path and the digest the indexer stored for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDigest {
    pub path: String,
    pub sha256: String,
}

/// The reader-backed digest source: `GraphReader::all_files()` already carries every `path` and
/// `sha256` the hash comparison needs, so no second query exists.
pub struct ReaderDigests<'a> {
    pub reader: &'a dyn GraphReader,
}

impl<'a> ReaderDigests<'a> {
    #[must_use]
    pub fn new(reader: &'a dyn GraphReader) -> Self {
        Self { reader }
    }
}

impl DigestSource for ReaderDigests<'_> {
    fn all_files(&self) -> Result<Vec<FileDigest>, AppError> {
        let rows = self.reader.all_files()?;
        Ok(rows
            .into_iter()
            .map(|row| FileDigest {
                path: row.path,
                sha256: row.sha256,
            })
            .collect())
    }
}

/// Extensions the MVP grammars cover, mapped to their lang id.
///
/// Copied from `diff.ts`: until a grammar registry exists in the Rust tree, a changed path counts
/// as parseable here if and only if the indexer could have parsed it, so the two lists must agree
/// when they meet.
const LANGUAGE_BY_EXTENSION: [(&str, &str); 18] = [
    (".py", "python"),
    (".ts", "typescript"),
    (".tsx", "tsx"),
    (".js", "javascript"),
    (".jsx", "javascript"),
    (".mjs", "javascript"),
    (".cjs", "javascript"),
    (".go", "go"),
    (".java", "java"),
    (".rs", "rust"),
    (".c", "c"),
    (".h", "c"),
    (".cpp", "cpp"),
    (".cc", "cpp"),
    (".hpp", "cpp"),
    (".cs", "csharp"),
    (".php", "php"),
    (".rb", "ruby"),
];

/// The MVP lang id for a path, or `None` when no shipped grammar could parse it.
fn language_for(path: &str) -> Option<&'static str> {
    let dot = path.rfind('.')?;
    // A dot that only belongs to a directory name (`src/v1.2/README`) names no extension.
    if Some(dot) <= path.rfind('/') {
        return None;
    }
    let extension = path.get(dot..)?.to_ascii_lowercase();
    LANGUAGE_BY_EXTENSION
        .iter()
        .find(|(candidate, _)| *candidate == extension)
        .map(|(_, language)| *language)
}

/// The change set for `repo_root` against `base`, as the union of `git diff --name-status` and a
/// sha256 comparison of the `file` table against the working tree.
///
/// `base` is `None` (or an empty string) for "the last commit", the usual change scope for a
/// summary. The digest source is a parameter on purpose: `sangkar-domain` decides what is true but
/// never opens a database (the caller passes the reader it already holds), and the hash comparison
/// cannot be done without the indexed digests.
///
/// A path git calls deleted but which still exists in the working tree is reported as `changed`,
/// not as `removed`: the bytes are present, so "gone" would be the wrong fact.
///
/// A git failure never throws: a missing git, a directory that is not a repository and a timeout
/// all fall back to the hash comparison with `degraded: true`, because the integration fixture is a
/// plain temp directory.
#[must_use]
pub fn changed_since(
    repo_root: &Path,
    base: Option<&str>,
    digests: &dyn DigestSource,
    opts: &ChangedSinceOptions,
) -> ChangeSet {
    let timeout_ms = opts.git_timeout_ms.unwrap_or(GIT_TIMEOUT_MS);
    let root = normalize_absolute(repo_root);
    let base = match base {
        Some(base) if !base.is_empty() => base,
        _ => "HEAD",
    };

    let digest_rows = match digests.all_files() {
        Ok(rows) => rows,
        Err(error) => {
            log::warn(
                &format!(
                    "the indexed digests could not be read, so the change set names only git facts: {error}"
                ),
                &[],
            );
            Vec::new()
        }
    };

    let mut degraded = false;
    let mut indexed: HashSet<String> = HashSet::new();
    let mut changed: HashSet<String> = HashSet::new();
    let mut removed: HashSet<String> = HashSet::new();
    let mut unmatched: BTreeMap<String, UnmatchedChange> = BTreeMap::new();
    // What git said about a path that reached `changed`: a rename keeps its source, and an
    // untracked path keeps its own state, so one entry can carry both halves once the hash
    // comparison has had its say.
    let mut git_state: HashMap<String, ChangeEntry> = HashMap::new();

    // Source 1: git. The two reads run together so a hung git costs one bound rather than two, as
    // the TypeScript's `Promise.all` does. The untracked read is applied after the hash comparison
    // (it needs the complete `indexed` set), but its failure is recorded here so both causes reach
    // `degraded`.
    let (name_status, untracked) = thread::scope(|scope| {
        let diff = scope.spawn(|| git_name_status(&root, base, timeout_ms));
        let status = scope.spawn(|| git_untracked(&root, timeout_ms));
        let diff = diff
            .join()
            .unwrap_or_else(|_| Err("git diff did not answer".to_owned()));
        let status = status
            .join()
            .unwrap_or_else(|_| Err("git status did not answer".to_owned()));
        (diff, status)
    });
    if let Err(reason) = &untracked {
        degraded = true;
        log::warn(
            &format!(
                "git status could not name the untracked paths under {}: {reason}",
                root.display()
            ),
            &[],
        );
    }

    match &name_status {
        Ok(stdout) => {
            for entry in parse_name_status(stdout) {
                let Some(path) = repo_relative(&root, &entry.path) else {
                    continue;
                };
                let from = match entry.from.as_deref() {
                    Some(from) => match repo_relative(&root, from) {
                        Some(from) => Some(from),
                        // One half of a rename pair outside the root makes the pair
                        // unattributable, so the record goes rather than being re-bucketed.
                        None => continue,
                    },
                    None => None,
                };
                let state = state_for(entry.status);
                if state == ChangeState::Deleted {
                    if classify(&path, &indexed) {
                        removed.insert(path);
                    } else {
                        unclassified(&mut unmatched, path);
                    }
                    continue;
                }
                // `M`, `A`, `T`, `U`, … all mean the working tree holds different bytes than
                // `base`, and a rename contributes its destination only: the source path is gone,
                // so counting it too would double-count one change.
                if !classify(&path, &indexed) {
                    unclassified(&mut unmatched, path);
                    continue;
                }
                changed.insert(path.clone());
                // `from` belongs to a rename. A copy's source still exists, so its destination is
                // a plain `added` entry and carrying the source would read as a rename.
                let from = if state == ChangeState::Renamed {
                    from
                } else {
                    None
                };
                git_state.insert(
                    path.clone(),
                    ChangeEntry {
                        state,
                        path,
                        from,
                        sha256: None,
                    },
                );
            }
        }
        Err(reason) => {
            degraded = true;
            log::warn(
                &format!(
                    "git diff could not answer for {} against {base}: {reason}",
                    root.display()
                ),
                &[],
            );
        }
    }

    // Source 2: the file table against the working tree. Always run, so a change git ignores (an
    // indexed path listed in `.gitignore`) is still found, and so a non-git directory still works.
    let mut current_digests: HashMap<String, String> = HashMap::new();
    for row in &digest_rows {
        let Some(path) = repo_relative(&root, &row.path) else {
            continue;
        };
        indexed.insert(path.clone());
        let absolute = root.join(&path);
        if !absolute.exists() {
            removed.insert(path);
            continue;
        }
        match fs::read(&absolute) {
            Ok(bytes) => {
                let digest = sha256_hex(&bytes);
                if digest != row.sha256 {
                    changed.insert(path.clone());
                    current_digests.insert(path, digest);
                }
            }
            Err(error) => {
                log::warn(
                    &format!("{path} could not be read while hashing: {error}"),
                    &[],
                );
                unmatched.insert(
                    path.clone(),
                    UnmatchedChange {
                        path,
                        reason: UnmatchedReason::UnreadableFile,
                    },
                );
            }
        }
    }

    // Present bytes outrank a deletion reported by an index: the path exists, so it is not gone.
    for path in &changed {
        removed.remove(path);
        unmatched.remove(path);
    }

    // Source 1b, applied after the hash comparison so `indexed` is complete: a path git does not
    // track is drift even when the graph has never seen it, which is the case the diff alone got
    // wrong. It goes through the same classify gate as every other path, so a path with no `file`
    // row and no parseable language is named in `unmatched` instead of being attributed to nothing.
    if let Ok(paths) = &untracked {
        for path in paths {
            if changed.contains(path) || removed.contains(path) || git_state.contains_key(path) {
                // The diff or the hash comparison already owns this path, and its state is the
                // more specific fact: `untracked` only describes a path no other source spoke
                // about.
                continue;
            }
            if !classify(path, &indexed) {
                unclassified(&mut unmatched, path.clone());
                continue;
            }
            // `changed` carries it as well as `entries`, so a drift count computed from `changed`
            // grows with the change set it describes: an untracked file is the tree moving on.
            changed.insert(path.clone());
            git_state.insert(
                path.clone(),
                ChangeEntry {
                    state: ChangeState::Untracked,
                    path: path.clone(),
                    from: None,
                    sha256: None,
                },
            );
        }
    }

    // Entries are derived from the two fields above, never the other way round, so a path cannot
    // be in one and missing from the other. A path only the hash comparison found has no git
    // letter: its bytes differ (`modified`) or its row outlived them (`deleted`), and a path only
    // `git status` found is `untracked`. Sorting after the merge is what makes two runs agree.
    let mut entries: Vec<ChangeEntry> = Vec::with_capacity(changed.len() + removed.len());
    for path in &changed {
        entries.push(git_state.remove(path).unwrap_or_else(|| ChangeEntry {
            state: ChangeState::Modified,
            path: path.clone(),
            from: None,
            sha256: None,
        }));
    }
    for path in &removed {
        entries.push(ChangeEntry {
            state: ChangeState::Deleted,
            path: path.clone(),
            from: None,
            sha256: None,
        });
    }
    for entry in &mut entries {
        if entry.state == ChangeState::Deleted {
            continue;
        }
        if let Some(digest) = current_digests.get(&entry.path) {
            entry.sha256 = Some(digest.clone());
            continue;
        }
        // A path only git spoke about still gets its current digest: one read, and `summary`
        // never has to reopen the file to say what its bytes are.
        if let Ok(bytes) = fs::read(root.join(&entry.path)) {
            entry.sha256 = Some(sha256_hex(&bytes));
        }
    }
    entries.sort_by(|left, right| by_path(&left.path, &right.path));

    let mut changed: Vec<String> = changed.into_iter().collect();
    changed.sort_by(|left, right| by_path(left, right));
    let mut removed: Vec<String> = removed.into_iter().collect();
    removed.sort_by(|left, right| by_path(left, right));
    let mut unmatched: Vec<UnmatchedChange> = unmatched.into_values().collect();
    unmatched.sort_by(|left, right| by_path(&left.path, &right.path));

    ChangeSet {
        changed,
        removed,
        entries,
        unmatched,
        degraded,
    }
}

/// `git diff --name-status` against `base`, bounded by `timeout_ms` and never interactive. Rename
/// detection is left on (git's default): a rename has to arrive as one `R` record, because only
/// then can the change set say "this moved" instead of "one path appeared, another vanished".
fn git_name_status(repo_root: &Path, base: &str, timeout_ms: u64) -> Result<Vec<u8>, String> {
    run_git(
        repo_root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-ext-diff",
            // Paths relative to the repository root handed in, not the repository's toplevel, so
            // a subdirectory root works.
            "--relative",
            base,
            "--",
        ],
        "git diff",
        timeout_ms,
    )
}

/// `rev-parse --show-prefix` then `status --porcelain -z --untracked-files=all`, the second git
/// read.
///
/// It exists because `git diff` cannot see a file git does not track yet — a freshly written file
/// has no `base` side, so `diff --name-status` omits it and a change set built from the diff alone
/// answers "nothing changed" for the commonest agent action. `--untracked-files=all` is what makes
/// two files inside a new directory two records instead of one directory record.
fn git_untracked(repo_root: &Path, timeout_ms: u64) -> Result<Vec<String>, String> {
    let prefix = run_git(
        repo_root,
        &["rev-parse", "--show-prefix"],
        "git rev-parse",
        timeout_ms,
    )?;
    let status = run_git(
        repo_root,
        &["status", "--porcelain", "-z", "--untracked-files=all"],
        "git status",
        timeout_ms,
    )?;
    Ok(parse_status_porcelain(
        &String::from_utf8_lossy(&status),
        String::from_utf8_lossy(&prefix).trim(),
    ))
}

/// One git argv run through the same bound and the same failure-as-value shape as its siblings.
///
/// The bound is a poll loop rather than a waiter thread because the child must stay reachable to
/// be killed: `try_wait` reports an exit as soon as it happens, and the deadline calls
/// [`std::process::Child::kill`], which is `SIGKILL` on Unix. Each pipe is drained on its own
/// thread, so a git that writes more than one pipe buffer cannot block on its own output while the
/// loop waits for it to exit.
fn run_git(
    repo_root: &Path,
    args: &[&str],
    label: &str,
    timeout_ms: u64,
) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command
        .args(GIT_PREFIX)
        .arg(repo_root)
        .args(args)
        .current_dir(repo_root)
        // Closed stdin plus piped output: the child has no terminal to prompt on.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("git could not be spawned: {error}"))?;
    let stdout = read_async(child.stdout.take());
    let stderr = read_async(child.stderr.take());
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut killed = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    killed = true;
                    let _ = child.kill();
                    match child.wait() {
                        Ok(status) => break status,
                        Err(error) => {
                            return Err(format!("{label} could not be reaped: {error}"));
                        }
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) => {
                return Err(format!("{label} could not be waited on: {error}"));
            }
        }
    };
    let stdout = collect(stdout).ok_or_else(|| format!("{label} stdout could not be read"))?;
    let stderr = collect(stderr).ok_or_else(|| format!("{label} stderr could not be read"))?;
    if killed {
        return Err(format!("{label} exceeded {timeout_ms} ms and was killed"));
    }
    if !status.success() {
        let code = status
            .code()
            .map_or_else(|| "signal".to_owned(), |code| code.to_string());
        return Err(format!("{label} exited {code}: {}", first_line(&stderr)));
    }
    Ok(stdout)
}

/// Drains one child pipe on its own thread, so the wait loop never has to choose between reading
/// and reaping.
fn read_async<R>(pipe: Option<R>) -> Receiver<std::io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        thread::spawn(move || {
            let mut buffer = Vec::new();
            let result = pipe.read_to_end(&mut buffer).map(|_| buffer);
            // The receiver is gone only when this module already answered; dropping the result then
            // is the same "nobody is listening" a closed pipe means.
            let _ = sender.send(result);
        });
    }
    receiver
}

/// Reads what a pipe reader sent, within [`COLLECT_GRACE`] of the child being reaped.
fn collect(receiver: Receiver<std::io::Result<Vec<u8>>>) -> Option<Vec<u8>> {
    match receiver.recv_timeout(COLLECT_GRACE) {
        Ok(Ok(bytes)) => Some(bytes),
        _ => None,
    }
}

/// The first non-empty stderr line, or `no output`: a git failure is one line in a log record.
fn first_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "no output".to_owned()
    } else {
        line.to_owned()
    }
}

/// `git status --porcelain -z` (v1) writes `XY<SP>PATH\0` per record: two status letters, one
/// space, then the path, with no quoting because `-z` suppresses it.
///
/// Only `??` is read here. Every other `XY` is a change the diff already reports, and taking it
/// from this stream too would put one path in the change set twice. A record outside `prefix`
/// belongs to the rest of the repository and is not this root's change set, so it is dropped rather
/// than re-bucketed: `status` reports repository-root-relative paths even from a subdirectory.
fn parse_status_porcelain(stdout: &str, prefix: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for record in stdout.split('\0') {
        let Some(reported) = record.strip_prefix("?? ") else {
            continue;
        };
        if reported.is_empty() || !reported.starts_with(prefix) {
            continue;
        }
        let path = &reported[prefix.len()..];
        if path.is_empty() {
            // The prefix itself, which names no path: `sub/` for the directory `sub`.
            continue;
        }
        paths.push(path.to_owned());
    }
    paths
}

/// One parsed `--name-status` record.
struct GitEntry {
    /// The `--name-status` letter: `M`, `A`, `D`, `R`, `C`, `T`, …
    status: char,
    /// The destination path: as git reported it, before containment and normalisation.
    path: String,
    /// The source path of an `R`/`C` pair; absent on every single-path record.
    from: Option<String>,
}

/// `-z` writes `STATUS\0path\0` per entry, and `R`/`C` write `STATUS\0source\0destination\0`.
///
/// A malformed record — a truncated pair, an empty path — is skipped instead of failing the parse:
/// one odd line must not empty the change set, and a git failure is the only thing that may degrade
/// the result to the hash comparison. A rename's status token is `R100` (with a similarity score),
/// so only its first character is the letter, exactly as the TypeScript reads it.
fn parse_name_status(stdout: &[u8]) -> Vec<GitEntry> {
    let text = String::from_utf8_lossy(stdout);
    let tokens: Vec<&str> = text.split('\0').collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < tokens.len() {
        let status = tokens[index];
        index += 1;
        if status.is_empty() {
            // The trailing NUL leaves one empty token; anything else is a malformed record.
            continue;
        }
        let letter = status.chars().next().unwrap_or(' ');
        let paired = letter == 'R' || letter == 'C';
        let from = tokens.get(index).copied();
        let path = tokens.get(index + usize::from(paired)).copied();
        index += if paired { 2 } else { 1 };
        let Some(path) = path else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        if paired {
            let Some(from) = from.filter(|from| !from.is_empty()) else {
                continue;
            };
            entries.push(GitEntry {
                status: letter,
                path: path.to_owned(),
                from: Some(from.to_owned()),
            });
        } else {
            entries.push(GitEntry {
                status: letter,
                path: path.to_owned(),
                from: None,
            });
        }
    }
    entries
}

/// git's status letter as the state a payload renders.
fn state_for(status: char) -> ChangeState {
    match status {
        'A' => ChangeState::Added,
        'D' => ChangeState::Deleted,
        'R' => ChangeState::Renamed,
        // A copy leaves its source in place, so only the destination is new: `added`, without a
        // `from` a reader would mistake for a rename. `M`, `T` (type change), `U` (unmerged) and
        // `X` (unknown) are all "different bytes at the same path".
        'C' => ChangeState::Added,
        _ => ChangeState::Modified,
    }
}

/// A path counts as attributable when the graph indexed it or a shipped grammar could parse it.
fn classify(path: &str, indexed: &HashSet<String>) -> bool {
    indexed.contains(path) || language_for(path).is_some()
}

/// Keeps a changed path we cannot attribute instead of dropping it silently.
fn unclassified(unmatched: &mut BTreeMap<String, UnmatchedChange>, path: String) {
    unmatched.insert(
        path.clone(),
        UnmatchedChange {
            path,
            reason: UnmatchedReason::NoFileRowAndUnparseableLanguage,
        },
    );
}

/// The path comparison the TypeScript writes as `a < b`: UTF-16 code units, which is what a
/// JavaScript string comparison orders by. It differs from Rust's UTF-8 byte order only for
/// non-ASCII names, and sorting is part of the payload contract.
fn by_path(left: &str, right: &str) -> std::cmp::Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}

/// The repo-relative form of a reported path, or `None` when it is outside `repo_root` — those are
/// dropped. Containment is lexical: `..` is collapsed, so `../x` and an absolute sibling both fail
/// the prefix test instead of being normalised into something that looks contained.
fn repo_relative(root: &Path, reported: &str) -> Option<String> {
    let reported = Path::new(reported);
    let absolute = if reported.is_absolute() {
        normalize_absolute(reported)
    } else {
        normalize_absolute(&root.join(reported))
    };
    if absolute == root {
        return None;
    }
    let relative = absolute.strip_prefix(root).ok()?.to_string_lossy();
    let relative = normalize_rel(&relative);
    if relative.is_empty() || relative == "." {
        None
    } else {
        Some(relative)
    }
}

/// `resolve()` in Rust: make `path` absolute against the current directory when it is relative, then
/// collapse `.` and `..` lexically.
///
/// Lexical on purpose. Git's own reports and the `file.path` column are both plain paths, and a
/// filesystem walk here would resolve a symlinked root while the containment test still compared
/// the strings — a mismatch that would read as "outside the root".
fn normalize_absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` at the root is the root; below it, drop the segment it names.
                if !normalized.pop() && !normalized.has_root() {
                    normalized.push("..");
                }
            }
            Component::Normal(segment) => normalized.push(segment),
        }
    }
    normalized
}

/// The SHA-256 state: the eight initial hash words.
const SHA256_INITIAL: [u32; 8] = [
    0x6a09_e667,
    0xbb67_ae85,
    0x3c6e_f372,
    0xa54f_f53a,
    0x510e_527f,
    0x9b05_688c,
    0x1f83_d9ab,
    0x5be0_cd19,
];

/// SHA-256's message schedule constants.
const SHA256_K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// The lowercase hex SHA-256 of `bytes`: the form `file.sha256` stores and [`ChangeEntry`] reports.
///
/// Public because the comparison is a contract between this module and whoever wrote the digest:
/// a test fixture, the indexer's file pass and `expand`'s drift check all have to compute it the
/// same way, and a second implementation is how two digests silently stop matching.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    let mut hex = String::with_capacity(64);
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        hex.push(char::from(DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    hex
}

/// FIPS 180-4 SHA-256 over a whole byte slice, padded here rather than streamed.
fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut message = Vec::with_capacity(bytes.len() + 72);
    message.extend_from_slice(bytes);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(bytes.len() as u64).wrapping_mul(8).to_be_bytes());

    let mut state = SHA256_INITIAL;
    for chunk in message.as_chunks::<64>().0 {
        let mut schedule = [0u32; 64];
        for (index, word) in chunk.as_chunks::<4>().0.iter().enumerate() {
            schedule[index] = u32::from_be_bytes(*word);
        }
        for index in 16..64 {
            let small_sigma0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let small_sigma1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(small_sigma0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(small_sigma1);
        }

        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for index in 0..64 {
            let big_sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let first = h
                .wrapping_add(big_sigma1)
                .wrapping_add(choose)
                .wrapping_add(SHA256_K[index])
                .wrapping_add(schedule[index]);
            let big_sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let second = big_sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(first);
            d = c;
            c = b;
            b = a;
            a = first.wrapping_add(second);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    let mut digest = [0u8; 32];
    for (index, word) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use super::{ChangeState, parse_name_status, parse_status_porcelain, sha256_hex, state_for};

    /// FIPS 180-4 / NIST vectors, plus two lengths that cross a padding block boundary.
    #[test]
    fn sha256_matches_the_published_vectors() {
        let cases: [(&[u8], &str); 4] = [
            (
                b"",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
            (
                b"The quick brown fox jumps over the lazy dog",
                "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592",
            ),
            (
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq",
                "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1",
            ),
        ];
        for (bytes, expected) in cases {
            assert_eq!(sha256_hex(bytes), expected, "{bytes:?}");
        }
    }

    /// The `-z` split: a rename is one record carrying its source, a copy is one record, and a
    /// malformed record is skipped without losing its neighbours.
    #[test]
    fn name_status_records_split_on_nul_and_keep_the_rename_source() {
        let stdout = b"M\0src/a.rs\0A\0src/b.rs\0R100\0src/old.rs\0src/new.rs\0C75\0src/c.rs\0src/d.rs\0D\0src/gone.rs\0R100\0truncated\0\0";
        let entries = parse_name_status(stdout);
        let shapes: Vec<(char, &str, Option<&str>)> = entries
            .iter()
            .map(|entry| (entry.status, entry.path.as_str(), entry.from.as_deref()))
            .collect();
        assert_eq!(
            shapes,
            vec![
                ('M', "src/a.rs", None),
                ('A', "src/b.rs", None),
                ('R', "src/new.rs", Some("src/old.rs")),
                ('C', "src/d.rs", Some("src/c.rs")),
                ('D', "src/gone.rs", None),
            ],
            "the truncated pair names no destination and is skipped"
        );
        assert_eq!(state_for('R'), ChangeState::Renamed);
        assert_eq!(state_for('C'), ChangeState::Added);
        assert_eq!(state_for('T'), ChangeState::Modified);
    }

    /// Only `??` records are untracked, and only those under the sub-root scope survive.
    #[test]
    fn status_records_take_untracked_paths_and_apply_the_prefix() {
        let stdout = "?? sub/keep.rs\0 M sub/tracked.rs\0?? outside.rs\0?? sub/\0R  sub/new.rs\0sub/old.rs\0";
        assert_eq!(
            parse_status_porcelain(stdout, "sub/"),
            vec!["keep.rs".to_owned()]
        );
        assert_eq!(
            parse_status_porcelain("?? plain.rs\0", ""),
            vec!["plain.rs".to_owned()]
        );
    }
}
