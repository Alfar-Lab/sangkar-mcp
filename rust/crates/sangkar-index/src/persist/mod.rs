//! The persist pass: one unit's rows into the stage, and the journalled removal of what the unit
//! no longer declares (`DATABASE.md` §5).
//!
//! Ported from `src/indexer/persist.ts`. Two decisions that file documents shape this module:
//!
//! - a file's rows are replaced **by `file_id`**, not by cascade: the `file` row is the staleness
//!   manifest (`sha256`, `size`, `line_count`, `parse_status`) and outlives a re-parse, so deleting
//!   it would throw the incremental planner's own facts away;
//! - a symbol that disappears is written to `tombstone` at the moment its node row goes, because
//!   the previous symbol set is visible only before the delete and `summary(change)` must be able to
//!   report a removal without re-parsing the base commit. The row is written *pending* — see
//!   [`PENDING_GENERATION`] — and the publish that names the generation stamps it.
//!
//! The seam is [`sangkar_store::Transaction`], never a connection: the stage is written inside the
//! caller's one transaction, so a failure anywhere is a discard of the whole stage rather than a
//! partial publish. Nothing here opens, closes, commits or rolls back anything.
//!
//! [`persist_unit`] replaces one file's contribution. The types below are what it takes in, what it
//! reports, and the closed sets the `file` and `not_indexed` columns are drawn from alongside it.

use std::fmt;

use crate::discover::SkipReason;
use crate::extract::{ExtractedNode, RawEdge, UnresolvedSite};

pub mod finalize;
mod unit;

pub use unit::persist_unit;

/// The `removed_in_generation` of a tombstone that is not a removal yet, and the marker a symbol
/// which vanished in this run carries until the publish names the generation.
///
/// An empty value cannot collide with a generation id, and the stage is discarded on failure, so a
/// pending row never reaches a published graph.
pub const PENDING_GENERATION: &str = "";

/// `file.parse_status`; the one closed set `DATABASE.md` §3.2 declares for the column.
///
/// Three values, no fourth: "not parsed yet" is the absence of a `file` row, which the incremental
/// planner reads as a new path, and a status spelling that state would give the column two meanings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    /// The grammar parsed the whole file.
    Ok,
    /// The grammar had to recover; the ranges live in `file.parse_error_ranges`.
    Partial,
    /// There is no tree at all, so the file has a `file` row and no nodes.
    Failed,
}

impl ParseStatus {
    /// The wire spelling, identical to the TypeScript union's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Partial => "partial",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for ParseStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a path is not in the graph: the five reasons discovery finds, the two only parsing can
/// produce, and the one a walk that refuses to follow a symbolic link reports (`DATABASE.md` §3.2).
///
/// A closed set rather than the column's free text, because the column is written from this side
/// only and a misspelt reason would make `not_indexed` answer a question nobody asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotIndexedReason {
    /// A rule from a control file (or the caller's extras) matched.
    Ignored,
    /// A NUL byte in the probe window, so the bytes are not source text.
    Binary,
    /// The file is larger than the read cap and was never opened.
    SizeCap,
    /// No extension in the twelve-language table, so no grammar parses it.
    UnsupportedExt,
    /// The file or one of its directories could not be read.
    Unreadable,
    /// The language is in the table but its grammar could not be attached to a parser.
    GrammarUnavailable,
    /// The grammar was attached and still produced no tree.
    ParseFailed,
    /// A path the walk recognised as a symbolic link and did not follow.
    Symlink,
}

impl NotIndexedReason {
    /// The spelling the row stores, identical to the TypeScript union's.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ignored => "ignored",
            Self::Binary => "binary",
            Self::SizeCap => "size_cap",
            Self::UnsupportedExt => "unsupported_ext",
            Self::Unreadable => "unreadable",
            Self::GrammarUnavailable => "grammar_unavailable",
            Self::ParseFailed => "parse_failed",
            Self::Symlink => "symlink",
        }
    }
}

impl fmt::Display for NotIndexedReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A discovery skip is one of the reasons a coverage row can carry, so the walk's own enum widens
/// here instead of the caller matching on eight spellings.
impl From<SkipReason> for NotIndexedReason {
    fn from(reason: SkipReason) -> Self {
        match reason {
            SkipReason::Ignored => Self::Ignored,
            SkipReason::Binary => Self::Binary,
            SkipReason::SizeCap => Self::SizeCap,
            SkipReason::UnsupportedExt => Self::UnsupportedExt,
            SkipReason::Unreadable => Self::Unreadable,
        }
    }
}

/// One coverage row: a path the indexer looked at and did not index, with the reason.
///
/// `detail` and `language` are optional because a reason may need neither — a binary file has a
/// byte offset worth reporting and no language, an `unsupported_ext` path is the reverse.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotIndexedEntry {
    /// Repo-relative path, forward-slashed; normalised by the writer, never trusted from the caller.
    pub path: String,
    /// The closed set above, stored under its wire spelling.
    pub reason: NotIndexedReason,
    /// Why this path, in words a person reads: a byte offset, a size, a parser's message.
    pub detail: Option<String>,
    /// The language the path would have been parsed as, when the extension identified one.
    pub language: Option<String>,
}

impl NotIndexedEntry {
    /// An entry with no detail and no language, the shape most reasons take.
    #[must_use]
    pub fn new(path: impl Into<String>, reason: NotIndexedReason) -> Self {
        Self {
            path: path.into(),
            reason,
            detail: None,
            language: None,
        }
    }
}

/// One unit as the worker hands it over: the extractor's [`crate::resolve::FileUnit`] plus the
/// coverage rows parsing found for this path.
///
/// The `file` row itself is the caller's business — it owns the staleness manifest the incremental
/// planner compares — so the persist pass only reads its id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PersistUnit {
    /// Repo-relative path, already normalised by the extractor; normalised again on the way in.
    pub rel_path: String,
    pub nodes: Vec<ExtractedNode>,
    /// The sites the extractor saw, before binding. Not written here: `resolved` is the only edge
    /// input, because the bind pass decides which site became an edge and at which tier.
    pub raw_edges: Vec<RawEdge>,
    /// The extractor's nameless sites, written as `unresolved` rows before the bind pass's failures.
    pub unresolved: Vec<UnresolvedSite>,
    /// Rows for **this** path only; a repo-wide discovery list belongs to the worker.
    pub not_indexed: Vec<NotIndexedEntry>,
}

/// What one [`persist_unit`] call wrote, for the worker's stderr log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PersistCounts {
    /// `file.id` of the unit's path.
    pub file_id: i64,
    /// Nodes written, after dropping the qns the unit declared twice.
    pub nodes: usize,
    /// Edge rows written for `resolved.edges` whose two ends exist. An insert the unique index
    /// collapses still counts: the site was written, the row was already there.
    pub edges: usize,
    /// Unresolved rows written, the extractor's sites and the bind pass's failures together.
    pub unresolved: usize,
    /// Sites whose other end has no row: dropped rather than written as a dangling edge.
    pub skipped_edges: usize,
    /// qns this unit dropped, recorded as pending tombstones.
    pub pending_tombstones: usize,
}
