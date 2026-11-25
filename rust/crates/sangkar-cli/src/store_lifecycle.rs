//! The store's file lifecycle: what the handle opened, and when the file became a newer generation.
//!
//! Ported from the store-lifecycle half of `src/index.ts` (`FileFingerprint`, `fingerprintOf`,
//! `sameFile`, `openStore`, `refreshStore`, `dbSizeBytes`). The invariant is the one every
//! stale-payload bug comes from losing: a publish renames a new inode over `graph.db`
//! (`sangkar_store::publish`), so a server that decided from the *store* it already had — or from
//! the path string — keeps answering from a generation nobody can see any more. The decision is a
//! fact about the file, taken on every call.
//!
//! Nothing here opens a database to find out whether it changed: an unchanged file costs one
//! `metadata` and one comparison, which is what makes a per-call refresh affordable in the first
//! place. Only a file that is a different publication is opened, and the replacement is built
//! before the old handle is dropped, so a generation that cannot be served leaves the previous
//! reader answering instead of dropping the session to an error it cannot recover from.

use std::fs::Metadata;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log;
use sangkar_store::db::open_reader;
use sangkar_store::migrations::{read_schema_version, schema_version};
use sangkar_store::pool::DATABASE_FILE;
use sangkar_store::read::reader::SqliteReader;

/// What the store's *file* looked like when the handle was opened.
///
/// A publish renames a new inode over the live path (`sangkar_store::publish`), so `inode` is the
/// signal; `size` and `mtime_ns` are what catch a filesystem that hands the replacement the inode
/// of the file it replaced. `path` is carried so a refresh knows which file to compare against,
/// never so that it can be compared — see [`same_file`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFingerprint {
    /// The file this fingerprint describes, as the caller spelled it when the handle opened.
    pub path: PathBuf,
    /// `st_ino` on unix; `0` where the platform reports no inode (see [`inode_of`]).
    pub inode: u64,
    /// `metadata.len()` at open time.
    pub size: u64,
    /// `mtime` in nanoseconds since the epoch, negative before it. `i128` because the value is
    /// seconds × 1e9: an `i64` wraps in 2262, and a wrapped fingerprint claims a newer file is
    /// unchanged.
    pub mtime_ns: i128,
}

/// The file's identity as the OS reports it, or `None` when there is nothing to fingerprint.
///
/// A missing file is the ordinary "nothing published yet" answer, and an unreadable one is the same
/// answer from this side: neither is a newer generation. Any other `stat` failure is an error the
/// caller must see, exactly as `fingerprintOf` re-throws a code it does not know.
pub fn fingerprint_of(path: &Path) -> Result<Option<FileFingerprint>, AppError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(FileFingerprint {
            path: path.to_path_buf(),
            inode: inode_of(&metadata),
            size: metadata.len(),
            mtime_ns: mtime_ns_of(&metadata),
        })),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::PermissionDenied
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(AppError::internal(format!(
            "cannot stat {}: {error}",
            path.display()
        ))),
    }
}

/// Whether two fingerprints name the same publication.
///
/// The comparison is `inode` + `size` + `mtime_ns` and never the path: the publish step replaces
/// the inode while the path stays the same, so a path comparison reports "unchanged" for exactly
/// the one event this module exists to notice — a path-equality check *is* the stale-payload bug.
/// Size and mtime are the belt to the inode's braces: they catch a filesystem that hands the
/// replacement the inode of the file it replaced.
#[must_use]
pub fn same_file(a: &FileFingerprint, b: &FileFingerprint) -> bool {
    a.inode == b.inode && a.size == b.size && a.mtime_ns == b.mtime_ns
}

/// The file's inode, the one signal a rename-over publish always changes.
///
/// On unix it is `st_ino`. A platform without `MetadataExt` has no portable inode, so the fallback
/// is `0` for every file and `size` plus `mtime_ns` carry the whole decision: that is a documented
/// degradation, not a safe default — a replacement that shares both would be missed — which is
/// acceptable only because this build ships Linux x64 and nothing else.
#[cfg(unix)]
fn inode_of(metadata: &Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;

    metadata.ino()
}

#[cfg(not(unix))]
fn inode_of(_metadata: &Metadata) -> u64 {
    0
}

/// `mtime` in nanoseconds since the epoch, negative for a file older than it.
///
/// A platform that cannot answer `modified()` reports `0`, the honest "no timestamp" value. Two
/// such files compare equal on this field, so the inode and the size are what separate them there;
/// no fingerprint is ever invented from a timestamp the OS refused to give.
fn mtime_ns_of(metadata: &Metadata) -> i128 {
    match metadata.modified() {
        Ok(modified) => match modified.duration_since(UNIX_EPOCH) {
            Ok(since) => since.as_nanos() as i128,
            Err(before) => -(before.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

/// One reader handle and the publication it opened.
///
/// The handle owns its connection, so dropping a [`Store`] closes it and replacing one generation
/// with another closes the superseded handle as a side effect of the swap — there is no `close()`
/// for a caller to forget.
#[derive(Debug)]
pub struct Store {
    reader: SqliteReader,
    fingerprint: FileFingerprint,
    schema_version: i64,
    db_size_bytes: u64,
    /// Set when a publish was detected but could not be opened: the reader below is still the
    /// previous generation, and this string is how the next answer says so instead of looking
    /// current. Copied from `refreshStore`'s catch branch (`src/index.ts`), which returns the old
    /// store carrying `staleReason: "a newer generation could not be opened"`.
    stale_reason: Option<String>,
}

impl Store {
    /// The reader this handle serves from. It answers about the generation named by
    /// [`Self::fingerprint`], which is what a refreshed handle must be re-read for.
    #[must_use]
    pub fn reader(&self) -> &SqliteReader {
        &self.reader
    }

    /// The file this handle is serving, so a newer one is a comparison rather than a guess.
    #[must_use]
    pub fn fingerprint(&self) -> &FileFingerprint {
        &self.fingerprint
    }

    /// `meta.schema_version` of the file this handle opened: [`schema_version`] for anything this
    /// build wrote, lower for a database an older build migrated.
    #[must_use]
    pub fn schema_version(&self) -> i64 {
        self.schema_version
    }

    /// The size of the file this handle opened, never a fresh `stat` of the path.
    ///
    /// This is the one payload number that can be read from the wrong file: `status` publishes it,
    /// so it has to describe the generation the reader answers from. A live `stat` would report a
    /// file published after this handle opened.
    #[must_use]
    pub fn db_size_bytes(&self) -> u64 {
        self.db_size_bytes
    }

    /// Why this handle is serving a generation it knows is behind, or `None` when it is current.
    ///
    /// A publish this process detected but could not open leaves the previous reader answering;
    /// without this the next `read_status` would report a current generation over an old file,
    /// which is the silent-staleness failure the fingerprint pair exists to prevent.
    #[must_use]
    pub fn stale_reason(&self) -> Option<&str> {
        self.stale_reason.as_deref()
    }
}

/// Why a cold open did not produce a [`Store`].
///
/// The lifecycle decision stays three-way and path-free; the wire's closed error union is
/// [`Self::code`] and [`Self::error`]'s job, so neither concern has to know the other's vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreFailure {
    /// No database at the path yet (or an empty one): a `bootstrapping` answer, not an error.
    Missing,
    /// The graph was written by a build that ships more migrations than this one.
    SchemaTooNew {
        /// `meta.schema_version` of the file on disk.
        found: i64,
        /// [`schema_version`]: what this build writes and can read.
        built: i64,
    },
    /// The file exists but could not be opened; the message is the underlying failure's.
    Open(String),
}

impl StoreFailure {
    /// The code this failure answers with, taken from the closed union in `sangkar_core::errors`.
    ///
    /// [`Self::Missing`] is `bootstrapping` rather than an error: a first `mcp` session before the
    /// first publish is normal, and every tool already knows how to answer it. This method is the
    /// one place the three lifecycle variants meet the twelve wire codes, so a caller never has to
    /// spell a code itself.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Missing => ErrorCode::BOOTSTRAPPING,
            Self::SchemaTooNew { .. } => ErrorCode::SCHEMA_TOO_NEW,
            Self::Open(_) => ErrorCode::INTERNAL,
        }
    }

    /// The error body a caller attaches to its envelope, or `None` for [`Self::Missing`].
    ///
    /// One message for both open paths, as `newerSchemaError` has it: a generation this build cannot
    /// read is a refusal, never a swap. The hint names the schema this build's indexer writes,
    /// because the reader of a refusal needs to know which side is behind.
    #[must_use]
    pub fn error(&self) -> Option<AppError> {
        match self {
            Self::Missing => None,
            Self::SchemaTooNew { found, built } => Some(
                AppError::new(
                    ErrorCode::SCHEMA_TOO_NEW,
                    format!("the graph is schema v{found}; this build ships v{built}"),
                )
                .with_hint(format!(
                    "upgrade the server, or move the data dir aside and re-index with \
                     `sangkar index`; this build's indexer writes schema v{built}"
                )),
            ),
            Self::Open(reason) => Some(AppError::internal(reason.clone())),
        }
    }
}

/// What a [`refresh_store`] call did, as the caller's decision: keep the handle, or answer from a
/// new one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshOutcome {
    /// A different publication was opened. The reader, the schema version and the size are now the
    /// new file's, all three replaced by one assignment so no caller can observe a mixture.
    Reopened,
    /// The file this handle serves is still the file on disk, or nothing servable replaced it: the
    /// previous reader keeps answering.
    Unchanged,
    /// There is no file at the path any more. The handle keeps serving the generation it opened
    /// (`DATABASE.md` §5), and the next *cold* open is the one that answers `bootstrapping`.
    Missing,
}

/// Opens the published graph under `data_dir`, or says why there is none to serve.
///
/// A missing (or zero-byte) file is [`StoreFailure::Missing`] — "nothing indexed yet" is an answer,
/// not a failure — and so is a file this process cannot stat. A graph whose `meta.schema_version`
/// exceeds [`schema_version`] is [`StoreFailure::SchemaTooNew`]: this build cannot read it, and
/// serving it anyway would publish rows it does not understand.
pub fn open_store(data_dir: &Path) -> Result<Store, StoreFailure> {
    let db_path = data_dir.join(DATABASE_FILE);
    // Fingerprinted before the handle is opened: a file replaced in between is then recorded as
    // *older* than the generation this connection read, so the next call refreshes again. Stamping
    // the newer print would leave the process serving the older graph until the following change.
    let fingerprint = match fingerprint_of(&db_path) {
        Ok(Some(fingerprint)) => fingerprint,
        // No file, or one this process cannot stat: neither is a generation to serve.
        Ok(None) => return Err(StoreFailure::Missing),
        Err(error) => return Err(StoreFailure::Open(error.message)),
    };
    open_publication(&db_path, fingerprint)
}

/// The one sentence a missing graph answers with: whether the file never existed or is the zero-byte
/// stage a torn publish left behind.
///
/// Worded here rather than at each call site because both surfaces say it — the CLI's absence answer
/// and the MCP `bootstrapping` envelope — and a caller comparing two answers must not see the same
/// fact spelled two ways.
#[must_use]
pub fn absent_reason(db_path: &Path) -> String {
    if db_path.exists() {
        format!("the index at {} is empty", db_path.display())
    } else {
        format!("no index at {} yet", db_path.display())
    }
}

/// Re-opens the store when the file on disk is a newer generation.
///
/// The decision is made on the *file*, never on the [`Store`] it is handed: a handle whose file was
/// replaced by a publish is `Reopened` here even though nothing about the handle changed, and a
/// file that is gone is `Missing` rather than an error, because a vanished graph is not a newer
/// generation.
///
/// A replacement is built before anything is dropped, so a generation that cannot be served — a
/// newer schema, a torn file, an unreadable path — leaves the previous reader in place and the
/// outcome `Unchanged`. That is the whole point: the session keeps answering, from a generation it
/// still knows it opened, instead of dying on a refusal that only a re-index can fix.
#[must_use]
pub fn refresh_store(store: &mut Store) -> RefreshOutcome {
    // The path this handle recorded, not a caller's idea of it: the question is whether *this* file
    // changed, and a second spelling of the path would be a different question.
    let now = match fingerprint_of(&store.fingerprint.path) {
        Ok(Some(now)) => now,
        Ok(None) => return RefreshOutcome::Missing,
        // Unreadable is not newer: the handle keeps serving what it already read.
        Err(_) => return RefreshOutcome::Unchanged,
    };
    if same_file(&store.fingerprint, &now) {
        return RefreshOutcome::Unchanged;
    }
    match open_publication(&store.fingerprint.path, now) {
        // One assignment, so the reader, the version and the size change together. The superseded
        // reader is dropped here, and dropping it is what closes its connection. A replacement that
        // opened is by definition current, so any earlier disclosure goes with it.
        Ok(replacement) => {
            *store = replacement;
            RefreshOutcome::Reopened
        }
        // A newer file exists and this process is still serving the old one. Reporting plain
        // `Unchanged` here would be the stale-payload bug this module exists to prevent, so the
        // handle records why it is behind and the words to say: `refreshStore` in `src/index.ts`
        // does the same job by returning the old store with `staleReason` set.
        Err(failure) => {
            let reason = format!("a newer generation could not be opened: {failure:?}");
            // `sangkar-cli` has no `serde_json`, so the reason rides in the message rather than a
            // structured field; `src/index.ts` logs it as `{reason, dbPath}` under the same event
            // name. Log wording may differ; the disclosed fact may not.
            log::warn(&format!("store.refresh_failed: {reason}"), &[]);
            store.stale_reason = Some(reason);
            RefreshOutcome::Unchanged
        }
    }
}

/// Opens `db_path` as the publication `fingerprint` describes.
///
/// The print is passed in rather than taken again so the file that was compared is the file that is
/// opened, and so the recorded size is the size of the generation this handle read.
fn open_publication(db_path: &Path, fingerprint: FileFingerprint) -> Result<Store, StoreFailure> {
    // A zero-byte `graph.db` is a stage that never got renamed into place: `open_reader` would
    // refuse it, and the honest answer is the same one a missing file gets.
    if fingerprint.size == 0 {
        return Err(StoreFailure::Missing);
    }
    let conn = open_reader(db_path).map_err(|error| StoreFailure::Open(error.message))?;
    let found = read_schema_version(conn.connection());
    let built = schema_version();
    if found > built {
        // Refused, never served, and the handle goes before the answer does: a refused graph must
        // not keep a descriptor on the data directory.
        let _ = conn.close();
        return Err(StoreFailure::SchemaTooNew { found, built });
    }
    let db_size_bytes = fingerprint.size;
    Ok(Store {
        stale_reason: None,
        reader: SqliteReader::new(conn),
        fingerprint,
        schema_version: found,
        db_size_bytes,
    })
}
