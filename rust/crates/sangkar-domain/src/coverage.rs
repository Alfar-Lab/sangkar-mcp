//! Coverage assembly: the absence contract's arithmetic (`DESIGN.md` §6, `DATABASE.md` §3.2).
//!
//! Ported from `src/shared/domain/coverage.ts`. The question this answers is *"nothing calls X — are
//! you sure?"*, and it is answered with four outcomes that must never collapse: absent (`eq`),
//! unresolved (`gte`), out-of-scope (`gte`) and stale (`gte`). [`assemble`] is the one place that
//! decides which of them holds, so two tools cannot disagree about whether an empty list is a fact.
//!
//! `eq` requires all three conditions this module can see: complete coverage, no unresolved row in
//! the scope, and a fresh generation — where fresh also means every file in the scope was written by
//! the generation being served. Truncation is the fourth and belongs to `budget::emit`; that is why
//! the relation is returned beside the coverage the envelope's
//! [`sangkar_core::envelope::relation_for`] already understands, rather than baked into a payload.
//!
//! The returned [`Coverage`] is `sangkar_core::envelope`'s, not a copy of it: one contract, one
//! definition. Every row type here is `sangkar_store`'s for the same reason.
//!
//! **The counts are the lists.** `coverage.indexed`, `.skipped` and `.unresolved_edges` are read off
//! the vectors this call returns, in the same pass that built them, so an answer cannot report a
//! number that disagrees with the rows it renders — `unresolved_edges == unresolved.len()` is a
//! property of the return value, not a convention callers have to honour.
//!
//! The prefix predicate is re-applied here rather than trusted to the source. A source is allowed to
//! answer a superset (a handler that memoised a wider query, the `summary` path in
//! `tools/summary/handler.ts`), and a scope that asked about `src` must not inherit a claim about
//! `src-old`; the trimmed lists are what the counts are read from, so the trim cannot be observed in
//! `indexed` but not in `not_indexed`.
//!
//! Two skip counts travel together because they answer different questions and neither implies the
//! other: `coverage.skipped` counts the paths *this scope* skipped, while
//! [`CoverageAssembly::not_indexed_total`] is the whole graph's count that a scoped answer
//! (`summary`, `expand`) publishes as `not_indexed_total`, so a narrow scope cannot understate what
//! the repository never read.

use sangkar_core::envelope::{Coverage, NotIndexedEntry, TestCensus, TotalRelation};
use sangkar_core::errors::AppError;
use sangkar_core::paths::is_under_prefix;
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, UnresolvedRef};
use sangkar_store::read::reader::GraphReader;

/// What a query is about. `paths` are repo-relative prefixes, `generation` is the generation the
/// answer will be served from, and `stale` is the freshness verdict of that generation against the
/// working tree (`DESIGN.md` §10.4) — a domain module does not read git.
///
/// `paths: None` is the whole graph: the source answers every row and there is nothing to trim.
/// `paths: Some(vec![])` is the empty scope — the coverage a bootstrapping answer carries, which
/// reads "nothing was indexed", never "nothing is there".
///
/// `file_ids` lets a caller narrow which files' unresolved call sites count; `None` means "every
/// file indexed under `paths`", which is the whole scope. `include_tests` is the caller's opt-in to
/// the test census: the query behind it is not free, so a tool that does not render the census does
/// not pay for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoverageScope {
    pub paths: Option<Vec<String>>,
    pub generation: String,
    pub stale: bool,
    pub file_ids: Option<Vec<i64>>,
    pub include_tests: bool,
}

/// The two halves of a scope question, with the connection already bound.
///
/// [`coverage_source`] is the `GraphReader`-backed implementation; a test supplies a hand-written
/// object over an in-memory fixture, so the `eq`/`gte` matrix is provable without a database. The
/// methods return `Result` because the store-backed one runs SQL: an assembly built from a failed
/// query would report `indexed: 0`, which is a wrong absence claim rather than a missing one.
///
/// `prefixes: None` asks for the whole graph (`all_files` plus `not_indexed_all`); `Some(&[])` asks
/// for nothing and answers with [`PathCoverage`]'s empty value, which is what "no prefix was asked
/// about" means.
pub trait CoverageSource {
    fn rows_for_paths(&self, prefixes: Option<&[String]>) -> Result<PathCoverage, AppError>;
    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError>;
    /// The repo-wide skipped-path count (`reader.not_indexed_count()`).
    fn not_indexed_count(&self) -> Result<i64, AppError>;
    /// Indexed test symbols. Only called when [`CoverageScope::include_tests`] is set.
    fn tests_census(&self) -> Result<TestCensus, AppError>;
}

/// The assembled answer: the envelope's [`Coverage`], the unresolved rows the scope's call sites did
/// not bind (the tool decides how to show them) and the relation those two support.
#[derive(Clone, Debug, PartialEq)]
pub struct CoverageAssembly {
    pub coverage: Coverage,
    pub unresolved: Vec<UnresolvedRef>,
    /// `eq` only when the coverage is complete, nothing is unresolved in scope and the generation is
    /// fresh for every file in it; anything else is a lower bound and says `gte`.
    pub relation: TotalRelation,
    /// `coverage.skipped` is scoped to [`CoverageScope::paths`]; this is the whole graph. The source
    /// is what keeps it at least as large as `coverage.not_indexed.len()`.
    pub not_indexed_total: u64,
}

/// Binds the store's coverage lookups to a reader — `coverageSource(conn)` in the TypeScript module,
/// one crate further up.
#[must_use]
pub fn coverage_source(reader: &dyn GraphReader) -> ReaderCoverageSource<'_> {
    ReaderCoverageSource { reader }
}

/// [`CoverageSource`] over a [`GraphReader`].
///
/// The whole-graph read is assembled from the two unscoped primitives the reader publishes, because
/// a prefix query with no prefixes answers nothing — correct for "no scope was asked about", wrong
/// for "the scope is the repository".
pub struct ReaderCoverageSource<'a> {
    reader: &'a dyn GraphReader,
}

impl CoverageSource for ReaderCoverageSource<'_> {
    fn rows_for_paths(&self, prefixes: Option<&[String]>) -> Result<PathCoverage, AppError> {
        let Some(prefixes) = prefixes else {
            let files = self.reader.all_files()?;
            let not_indexed = self.reader.not_indexed_all()?;
            return Ok(PathCoverage {
                indexed: files.len(),
                skipped: not_indexed.len(),
                files,
                not_indexed,
            });
        };
        self.reader.rows_for_paths(prefixes)
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        self.reader.unresolved_for_files(file_ids)
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        self.reader.not_indexed_count()
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        self.reader.tests_census()
    }
}

/// `partial` and `failed` parses are both "we did not fully read this file" (`DESIGN.md` §9.2).
fn is_failed_parse(file: &IndexedFileRow) -> bool {
    file.parse_status != "ok"
}

/// A generation is fresh for this scope when the caller found no working-tree drift *and* every file
/// in the scope was written by the generation being served. A file from an older publish is a stale
/// answer even when the repository itself has not moved.
fn is_fresh(scope: &CoverageScope, indexed: &[IndexedFileRow]) -> bool {
    if scope.stale {
        return false;
    }
    indexed
        .iter()
        .all(|file| file.indexed_generation == scope.generation)
}

/// The half-open prefix test from [`sangkar_core::paths`], once per requested prefix.
fn under_any(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| is_under_prefix(path, prefix))
}

/// The repo-wide number of skipped paths, as the wire's `u64`.
///
/// The conversion is not cosmetic: a `COUNT(*)` is an `i64` and a negative one is a bug in the
/// source, not a total. Reporting it as `0` would turn a broken read into "nothing was skipped".
fn not_indexed_total_of(src: &dyn CoverageSource) -> Result<u64, AppError> {
    let count = src.not_indexed_count()?;
    u64::try_from(count).map_err(|error| {
        AppError::internal(format!(
            "the not_indexed count {count} cannot be a coverage total: {error}"
        ))
    })
}

/// Assembles coverage for one scope and derives the relation it can honestly support.
///
/// `complete` is false when the scope holds a skipped path or a `partial`/`failed` parse, and also
/// when nothing was indexed at all: an absence claim over zero files is unverified, so it must not
/// be the one case that reports `eq` (the empty-scope trap `DESIGN.md` §6 names).
/// `unresolved_edges` counts the rows in *this* scope and is the length of the vector returned
/// beside it, which is what makes `gte` local instead of repo-wide.
///
/// The source's own `indexed`/`skipped` counters are deliberately unused: they describe the answer
/// the source gave, while the counts below describe the scope after the prefix trim.
pub fn assemble(
    src: &dyn CoverageSource,
    scope: &CoverageScope,
) -> Result<CoverageAssembly, AppError> {
    let rows = src.rows_for_paths(scope.paths.as_deref())?;
    let (indexed, skipped) = match scope.paths.as_deref() {
        None => (rows.files, rows.not_indexed),
        Some(prefixes) => (
            rows.files
                .into_iter()
                .filter(|file| under_any(&file.path, prefixes))
                .collect(),
            rows.not_indexed
                .into_iter()
                .filter(|row| under_any(&row.path, prefixes))
                .collect(),
        ),
    };

    let file_ids: Vec<i64> = match &scope.file_ids {
        Some(ids) => ids.clone(),
        None => indexed.iter().map(|file| file.id).collect(),
    };
    let unresolved = src.unresolved_for_files(&file_ids)?;

    let failed = indexed.iter().filter(|file| is_failed_parse(file)).count();
    let complete = !indexed.is_empty() && skipped.is_empty() && failed == 0;
    let fresh = is_fresh(scope, &indexed);
    let tests = if scope.include_tests {
        Some(src.tests_census()?)
    } else {
        None
    };
    let not_indexed_total = not_indexed_total_of(src)?;

    let coverage = Coverage {
        complete,
        indexed: indexed.len() as u64,
        skipped: skipped.len() as u64,
        failed: failed as u64,
        unresolved_edges: unresolved.len() as u64,
        not_indexed: skipped
            .iter()
            .map(|row| NotIndexedEntry::new(row.path.clone(), row.reason.clone()))
            .collect(),
        tests,
    };
    let relation = if complete && unresolved.is_empty() && fresh {
        TotalRelation::Eq
    } else {
        TotalRelation::Gte
    };
    Ok(CoverageAssembly {
        coverage,
        unresolved,
        relation,
        not_indexed_total,
    })
}
