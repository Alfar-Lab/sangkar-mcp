//! Coverage assembly and the risk score, over fake sources (`src/shared/domain/coverage.ts`,
//! `risk.ts`).
//!
//! Every case of `tests/domain.test.ts`'s coverage and risk blocks is here, in the same order, with
//! the TypeScript's expected values rather than re-derived ones. The sources are hand-written
//! objects, not a database: the `eq`/`gte` matrix is a property of the arithmetic, and a migrated
//! `graph.db` would only add a way for the test to be wrong.
//!
//! Three cases have no TypeScript counterpart and are the ones this port must not lose:
//!
//! - `unresolved_edges` is the length of the rows returned, over the whole matrix;
//! - `not_indexed` is trimmed to the scope's prefixes, including the `src-old` sibling whose `-`
//!   sorts below the `/` of `src/`;
//! - an empty change set earns no test-coverage driver, the filed bug (#10).

use std::cell::{Cell, RefCell};

use sangkar_core::envelope::{
    Coverage, NotIndexedEntry, ReadStatus, TestCensus, TestCensusEntry, TotalRelation, relation_for,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_domain::coverage::{CoverageAssembly, CoverageScope, CoverageSource, assemble};
use sangkar_domain::risk::{
    ChangedFacts, HIGH_FROM, ImpactedFacts, MEDIUM_FROM, RISK_BANDS, RiskBand, band_of, score,
};
use sangkar_store::read::coverage::{IndexedFileRow, PathCoverage, SkippedPathRow, UnresolvedRef};

const GENERATION: &str = "g-2";

fn file_row(id: i64, path: &str, generation: &str, parse_status: &str) -> IndexedFileRow {
    IndexedFileRow {
        id,
        path: path.to_owned(),
        language: "typescript".to_owned(),
        sha256: "a".repeat(64),
        size: 100,
        line_count: 10,
        parse_status: parse_status.to_owned(),
        parse_error_ranges: if parse_status == "ok" {
            None
        } else {
            Some("7-7".to_owned())
        },
        indexed_generation: generation.to_owned(),
    }
}

fn skipped_row(path: &str, reason: &str) -> SkippedPathRow {
    SkippedPathRow {
        id: 1,
        path: path.to_owned(),
        reason: reason.to_owned(),
        detail: None,
        language: None,
    }
}

fn unresolved_row(id: i64, file_id: i64, path: &str, line: i64, raw_target: &str) -> UnresolvedRef {
    UnresolvedRef {
        id,
        file_id,
        file: path.to_owned(),
        line,
        raw_target: raw_target.to_owned(),
        evidence_kind: Some("receiver".to_owned()),
        evidence: Some("receiver type unknown".to_owned()),
    }
}

/// A [`CoverageSource`] over rows the test wrote, which also records every question it was asked.
///
/// The record is what makes two rules of the assembly observable: that the scope's prefixes reach the
/// source unchanged, and that the test census is only queried when the caller opted in.
struct FakeSource {
    files: Vec<IndexedFileRow>,
    skipped: Vec<SkippedPathRow>,
    unresolved: Vec<UnresolvedRef>,
    /// `None` reports the scoped skip count, as `notIndexedCount()` would on a clean fixture.
    not_indexed_total: Option<i64>,
    census: TestCensus,
    asked: RefCell<Vec<Option<Vec<String>>>>,
    census_queries: Cell<usize>,
}

impl FakeSource {
    fn new(
        files: Vec<IndexedFileRow>,
        skipped: Vec<SkippedPathRow>,
        unresolved: Vec<UnresolvedRef>,
    ) -> Self {
        Self {
            files,
            skipped,
            unresolved,
            not_indexed_total: None,
            census: TestCensus {
                nodes: 0,
                languages: Vec::new(),
            },
            asked: RefCell::new(Vec::new()),
            census_queries: Cell::new(0),
        }
    }

    fn with_not_indexed_total(mut self, total: i64) -> Self {
        self.not_indexed_total = Some(total);
        self
    }

    fn with_census(mut self, census: TestCensus) -> Self {
        self.census = census;
        self
    }

    fn asked(&self) -> Vec<Option<Vec<String>>> {
        self.asked.borrow().clone()
    }

    fn census_queries(&self) -> usize {
        self.census_queries.get()
    }
}

impl CoverageSource for FakeSource {
    fn rows_for_paths(&self, prefixes: Option<&[String]>) -> Result<PathCoverage, AppError> {
        self.asked
            .borrow_mut()
            .push(prefixes.map(<[String]>::to_vec));
        Ok(PathCoverage {
            indexed: self.files.len(),
            skipped: self.skipped.len(),
            files: self.files.clone(),
            not_indexed: self.skipped.clone(),
        })
    }

    fn unresolved_for_files(&self, file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
        Ok(self
            .unresolved
            .iter()
            .filter(|row| file_ids.contains(&row.file_id))
            .cloned()
            .collect())
    }

    fn not_indexed_count(&self) -> Result<i64, AppError> {
        Ok(self.not_indexed_total.unwrap_or(self.skipped.len() as i64))
    }

    fn tests_census(&self) -> Result<TestCensus, AppError> {
        self.census_queries.set(self.census_queries.get() + 1);
        Ok(self.census.clone())
    }
}

/// Two files under `src`, nothing skipped, nothing unresolved: the `eq` fixture.
fn complete_source() -> FakeSource {
    FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src/b.ts", GENERATION, "ok"),
        ],
        Vec::new(),
        Vec::new(),
    )
}

fn scope_of(paths: Option<&[&str]>) -> CoverageScope {
    CoverageScope {
        paths: paths.map(|prefixes| prefixes.iter().map(|prefix| (*prefix).to_owned()).collect()),
        generation: GENERATION.to_owned(),
        ..CoverageScope::default()
    }
}

fn assembly_of(source: &dyn CoverageSource, scope: &CoverageScope) -> CoverageAssembly {
    assemble(source, scope).expect("the fake source never fails")
}

fn fresh_read_status() -> ReadStatus {
    ReadStatus::new(GENERATION, "2026-11-17T13:50:15Z")
}

// -------------------------------------------------------------------------------------------
// coverage: eq only when complete, unresolved-free and fresh
// -------------------------------------------------------------------------------------------

#[test]
fn a_complete_unresolved_free_fresh_scope_is_eq() {
    let source = complete_source();
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert_eq!(result.relation, TotalRelation::Eq);
    assert_eq!(
        result.coverage,
        Coverage {
            complete: true,
            indexed: 2,
            skipped: 0,
            failed: 0,
            unresolved_edges: 0,
            not_indexed: Vec::new(),
            tests: None,
        }
    );
    assert_eq!(result.unresolved, Vec::new());
    // The envelope must accept this coverage unchanged: one contract, one definition.
    assert_eq!(
        relation_for(&fresh_read_status(), &result.coverage),
        TotalRelation::Eq
    );
    assert_eq!(result.not_indexed_total, 0);
}

#[test]
fn an_unresolved_row_in_scope_makes_it_gte() {
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src/b.ts", GENERATION, "ok"),
        ],
        Vec::new(),
        vec![unresolved_row(1, 1, "src/a.ts", 12, "mystery")],
    );
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert_eq!(result.relation, TotalRelation::Gte);
    assert!(result.coverage.complete);
    assert_eq!(result.coverage.unresolved_edges, 1);
    assert_eq!(
        result
            .unresolved
            .iter()
            .map(|row| format!("{}:{}", row.file, row.line))
            .collect::<Vec<_>>(),
        vec!["src/a.ts:12"]
    );
    assert_eq!(
        relation_for(&fresh_read_status(), &result.coverage),
        TotalRelation::Gte
    );
}

#[test]
fn a_stale_generation_makes_a_complete_scope_gte() {
    let source = complete_source();
    let scope = CoverageScope {
        stale: true,
        ..scope_of(Some(&["src"]))
    };
    let result = assembly_of(&source, &scope);

    assert!(result.coverage.complete);
    assert_eq!(result.relation, TotalRelation::Gte);
}

#[test]
fn a_file_written_by_an_older_publish_makes_the_scope_gte() {
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", "g-1", "ok"),
            file_row(2, "src/b.ts", GENERATION, "ok"),
        ],
        Vec::new(),
        Vec::new(),
    );
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert!(result.coverage.complete);
    assert_eq!(result.relation, TotalRelation::Gte);
}

#[test]
fn a_skipped_path_makes_coverage_incomplete_and_the_relation_gte() {
    let source = FakeSource::new(
        vec![file_row(1, "src/a.ts", GENERATION, "ok")],
        vec![skipped_row("src/legacy.xslt", "grammar_unavailable")],
        Vec::new(),
    );
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert!(!result.coverage.complete);
    assert_eq!(result.coverage.skipped, 1);
    assert_eq!(
        result.coverage.not_indexed,
        vec![NotIndexedEntry::new(
            "src/legacy.xslt",
            "grammar_unavailable"
        )]
    );
    assert_eq!(result.relation, TotalRelation::Gte);
}

#[test]
fn a_partial_parse_is_a_failure_so_coverage_is_incomplete() {
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src/b.ts", GENERATION, "partial"),
        ],
        Vec::new(),
        Vec::new(),
    );
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert_eq!(result.coverage.failed, 1);
    assert!(!result.coverage.complete);
    assert_eq!(result.relation, TotalRelation::Gte);
}

#[test]
fn an_empty_scope_is_never_eq() {
    let source = FakeSource::new(Vec::new(), Vec::new(), Vec::new());
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    // An absence over zero files is unverified, so it may not be the one case that reports `eq`.
    assert!(!result.coverage.complete);
    assert_eq!(result.coverage.indexed, 0);
    assert_eq!(result.relation, TotalRelation::Gte);
}

#[test]
fn narrowing_to_file_ids_keeps_outside_unresolved_rows_out_of_the_answer() {
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src/b.ts", GENERATION, "ok"),
        ],
        Vec::new(),
        vec![unresolved_row(1, 2, "src/b.ts", 30, "elsewhere")],
    );
    assert_eq!(
        assembly_of(&source, &scope_of(Some(&["src"]))).relation,
        TotalRelation::Gte
    );

    let narrowed = assembly_of(
        &source,
        &CoverageScope {
            file_ids: Some(vec![1]),
            ..scope_of(Some(&["src"]))
        },
    );
    assert_eq!(narrowed.unresolved, Vec::new());
    assert_eq!(narrowed.coverage.unresolved_edges, 0);
    assert_eq!(narrowed.relation, TotalRelation::Eq);
}

#[test]
fn the_scoped_skipped_count_and_the_repo_wide_total_are_reported_separately() {
    // Three skips in the graph, one of them under the queried prefix: the payload must be able to
    // say both, because the scoped count alone reads as "nothing was skipped".
    let source = FakeSource::new(
        vec![file_row(1, "src/a.ts", GENERATION, "ok")],
        vec![skipped_row("src/legacy.xslt", "grammar_unavailable")],
        Vec::new(),
    )
    .with_not_indexed_total(3);
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert_eq!(result.coverage.skipped, 1);
    assert_eq!(result.coverage.not_indexed.len(), 1);
    assert_eq!(result.not_indexed_total, 3);
    assert!(!result.coverage.complete);
    assert_eq!(result.relation, TotalRelation::Gte);
}

// -------------------------------------------------------------------------------------------
// coverage: the count/list coupling and the scope the source may not widen
// -------------------------------------------------------------------------------------------

#[test]
fn unresolved_edges_is_always_the_length_of_the_rows_returned() {
    let empty = FakeSource::new(
        vec![file_row(1, "src/a.ts", GENERATION, "ok")],
        Vec::new(),
        Vec::new(),
    );
    let three = FakeSource::new(
        vec![file_row(1, "src/a.ts", GENERATION, "ok")],
        Vec::new(),
        vec![
            unresolved_row(1, 1, "src/a.ts", 3, "alpha"),
            unresolved_row(2, 1, "src/a.ts", 9, "beta"),
            unresolved_row(3, 1, "src/a.ts", 14, "gamma"),
        ],
    );
    let across_files = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src/b.ts", GENERATION, "ok"),
        ],
        Vec::new(),
        vec![
            unresolved_row(1, 1, "src/a.ts", 3, "alpha"),
            unresolved_row(2, 2, "src/b.ts", 5, "beta"),
        ],
    );

    let cases = [
        (&empty as &dyn CoverageSource, scope_of(Some(&["src"]))),
        (&three as &dyn CoverageSource, scope_of(Some(&["src"]))),
        (
            &across_files as &dyn CoverageSource,
            CoverageScope {
                file_ids: Some(vec![2]),
                ..scope_of(Some(&["src"]))
            },
        ),
        (&across_files as &dyn CoverageSource, scope_of(Some(&[]))),
    ];
    for (source, scope) in cases {
        let result = assembly_of(source, &scope);
        assert_eq!(
            result.coverage.unresolved_edges,
            result.unresolved.len() as u64,
            "coverage.unresolved_edges and the rows returned must be one number"
        );
    }
}

#[test]
fn not_indexed_is_trimmed_to_the_scope_prefixes() {
    // `src-old/` is the sibling a naive lower bound would also match: `-` (0x2d) sorts below `/`
    // (0x2f), so `path >= "src"` would carry it into a `src` answer.
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "src-old/x.ts", GENERATION, "ok"),
            file_row(3, "lib/c.ts", GENERATION, "ok"),
        ],
        vec![
            skipped_row("src/legacy.xslt", "grammar_unavailable"),
            skipped_row("src-old/legacy.xslt", "grammar_unavailable"),
            skipped_row("lib/gen.xslt", "grammar_unavailable"),
        ],
        Vec::new(),
    );
    let result = assembly_of(&source, &scope_of(Some(&["src"])));

    assert_eq!(source.asked(), vec![Some(vec!["src".to_owned()])]);
    assert_eq!(result.coverage.indexed, 1);
    assert_eq!(result.coverage.skipped, 1);
    assert_eq!(
        result.coverage.not_indexed,
        vec![NotIndexedEntry::new(
            "src/legacy.xslt",
            "grammar_unavailable"
        )]
    );
}

#[test]
fn a_whole_graph_scope_keeps_every_row() {
    let source = FakeSource::new(
        vec![
            file_row(1, "src/a.ts", GENERATION, "ok"),
            file_row(2, "lib/c.ts", GENERATION, "ok"),
        ],
        vec![skipped_row("docs/readme.md", "unsupported_ext")],
        Vec::new(),
    );
    let result = assembly_of(&source, &scope_of(None));

    assert_eq!(source.asked(), vec![None]);
    assert_eq!(result.coverage.indexed, 2);
    assert_eq!(result.coverage.skipped, 1);
    assert_eq!(result.coverage.not_indexed.len(), 1);
}

#[test]
fn an_empty_prefix_list_selects_nothing() {
    let source = complete_source();
    let result = assembly_of(&source, &scope_of(Some(&[])));

    assert_eq!(source.asked(), vec![Some(Vec::new())]);
    assert_eq!(result.coverage.indexed, 0);
    assert!(!result.coverage.complete);
}

#[test]
fn tests_are_carried_only_when_the_caller_asks() {
    let census = TestCensus {
        nodes: 12,
        languages: vec![TestCensusEntry {
            language: "rust".to_owned(),
            nodes: 12,
        }],
    };
    let source = complete_source().with_census(census.clone());

    let quiet = assembly_of(&source, &scope_of(Some(&["src"])));
    assert_eq!(quiet.coverage.tests, None);
    assert_eq!(source.census_queries(), 0);

    let loud = assembly_of(
        &source,
        &CoverageScope {
            include_tests: true,
            ..scope_of(Some(&["src"]))
        },
    );
    assert_eq!(loud.coverage.tests, Some(census));
    assert_eq!(source.census_queries(), 1);
}

#[test]
fn a_failing_source_is_an_error_not_an_absence_claim() {
    struct Failing;

    impl CoverageSource for Failing {
        fn rows_for_paths(&self, _prefixes: Option<&[String]>) -> Result<PathCoverage, AppError> {
            Err(AppError::internal("the file table is locked"))
        }

        fn unresolved_for_files(&self, _file_ids: &[i64]) -> Result<Vec<UnresolvedRef>, AppError> {
            Err(AppError::internal("the file table is locked"))
        }

        fn not_indexed_count(&self) -> Result<i64, AppError> {
            Err(AppError::internal("the file table is locked"))
        }

        fn tests_census(&self) -> Result<TestCensus, AppError> {
            Err(AppError::internal("the file table is locked"))
        }
    }

    // A failed read must not assemble into `indexed: 0`, which reads as a verified absence.
    let error = assemble(&Failing, &scope_of(Some(&["src"]))).expect_err("the read failed");
    assert_eq!(error.code, ErrorCode::INTERNAL);
}

// -------------------------------------------------------------------------------------------
// risk: driver sentences, not a bare number
// -------------------------------------------------------------------------------------------

fn change(
    symbols: usize,
    files: usize,
    entry_points: usize,
    untested: usize,
    removed: usize,
) -> ChangedFacts {
    ChangedFacts {
        symbols,
        files,
        entry_points,
        untested,
        removed,
    }
}

fn impact(nodes: usize, files: usize, max_indegree: usize, top_qn: Option<&str>) -> ImpactedFacts {
    ImpactedFacts {
        nodes,
        files,
        max_indegree,
        top_qn: top_qn.map(str::to_owned),
    }
}

#[test]
fn the_published_example_scores_medium_with_driver_sentences() {
    let result = score(
        &change(5, 3, 1, 2, 0),
        &impact(41, 10, 12, Some("code_review_graph/graph.py::GraphStore")),
    );

    assert_eq!(result.score, 0.55);
    assert_eq!(result.band, RiskBand::Medium);
    assert!(!result.drivers.is_empty());
    for driver in &result.drivers {
        assert!(driver.contains(' '), "not prose: {driver}");
        assert!(
            driver.chars().any(|c| c.is_ascii_digit()),
            "no number quoted: {driver}"
        );
    }
    assert!(
        result
            .drivers
            .contains(&"41 dependents sit inside the expanded radius".to_owned())
    );
    assert!(
        result
            .drivers
            .contains(&"2 changed symbols have no reachable test".to_owned())
    );
    assert!(result.drivers.contains(
        &"the widest single impact is code_review_graph/graph.py::GraphStore with 12 direct dependents"
            .to_owned()
    ));
}

#[test]
fn drivers_keep_the_declared_order() {
    let result = score(
        &change(4, 2, 3, 1, 2),
        &impact(7, 2, 4, Some("src/a.ts::Hub")),
    );

    assert_eq!(
        result.drivers,
        vec![
            "7 dependents sit inside the expanded radius".to_owned(),
            "the widest single impact is src/a.ts::Hub with 4 direct dependents".to_owned(),
            "4 changed symbols span 2 files".to_owned(),
            "1 changed symbols have no reachable test".to_owned(),
            "3 changed symbols are entry points".to_owned(),
            "2 removed symbols are still referenced".to_owned(),
        ]
    );
}

#[test]
fn no_dependents_still_yields_driver_sentences() {
    let result = score(&change(1, 1, 0, 0, 0), &impact(0, 0, 0, None));

    assert!(!result.drivers.is_empty());
    assert!(
        result
            .drivers
            .contains(&"no dependents were found inside the expanded radius".to_owned())
    );
    assert!(
        result
            .drivers
            .contains(&"every changed symbol has a test within the tested-depth limit".to_owned())
    );
    assert_eq!(result.band, RiskBand::Low);
}

#[test]
fn the_widest_impact_driver_needs_a_named_dependant() {
    let unnamed = score(
        &change(1, 1, 0, 0, 0),
        &impact(1, 1, 0, Some("src/a.ts::Hub")),
    );
    assert!(
        !unnamed
            .drivers
            .iter()
            .any(|driver| driver.starts_with("the widest single impact"))
    );

    let named = score(
        &change(1, 1, 0, 0, 0),
        &impact(1, 1, 3, Some("src/a.ts::Hub")),
    );
    assert!(named.drivers.contains(
        &"the widest single impact is src/a.ts::Hub with 3 direct dependents".to_owned()
    ));
}

#[test]
fn band_thresholds_cover_low_medium_and_high() {
    let low = score(&change(1, 1, 0, 0, 0), &impact(1, 1, 1, None));
    assert_eq!(low.band, RiskBand::Low);

    let medium = score(&change(5, 3, 1, 2, 0), &impact(41, 10, 12, None));
    assert_eq!(medium.band, RiskBand::Medium);

    let high = score(
        &change(10, 20, 2, 5, 3),
        &impact(50, 20, 50, Some("src/a.ts::Hub")),
    );
    assert_eq!(high.score, 1.0);
    assert_eq!(high.band, RiskBand::High);
}

#[test]
fn a_band_boundary_holds_on_each_side() {
    // Each floor and the value one hundredth below it: `0.67`/`0.66` and `0.34`/`0.33`.
    assert_eq!(band_of(HIGH_FROM), RiskBand::High);
    assert_eq!(band_of(0.66), RiskBand::Medium);
    assert_eq!(band_of(MEDIUM_FROM), RiskBand::Medium);
    assert_eq!(band_of(0.33), RiskBand::Low);

    assert_eq!(
        RiskBand::ALL.map(RiskBand::as_str),
        RISK_BANDS,
        "the enum and the published band list must not drift"
    );
}

#[test]
fn removed_symbols_add_a_driver_only_when_they_exist() {
    let with_removed = score(&change(1, 1, 0, 0, 2), &impact(0, 0, 0, None));
    assert!(
        with_removed
            .drivers
            .contains(&"2 removed symbols are still referenced".to_owned())
    );

    let without = score(&change(1, 1, 0, 0, 0), &impact(0, 0, 0, None));
    assert!(
        !without
            .drivers
            .iter()
            .any(|driver| driver.contains("removed"))
    );
}

#[test]
fn the_score_is_deterministic_bounded_and_monotone_in_dependents() {
    let facts = change(3, 2, 0, 1, 0);
    let few = score(&facts, &impact(2, 1, 2, None));
    let many = score(&facts, &impact(40, 1, 9, None));

    assert!(many.score > few.score);
    assert!(few.score >= 0.0);
    assert!(many.score <= 1.0);
    assert_eq!(score(&facts, &impact(40, 1, 9, None)), many);
}

#[test]
fn saturating_counts_cannot_produce_an_unbounded_score() {
    // The Rust side of the TypeScript's `Number.isFinite` guard: a `usize` cannot be `NaN` or
    // negative, and a count past its term's scale is still worth exactly one term.
    let saturated = score(
        &change(usize::MAX, usize::MAX, usize::MAX, usize::MAX, usize::MAX),
        &impact(usize::MAX, usize::MAX, usize::MAX, Some("src/a.ts::Hub")),
    );
    assert_eq!(saturated.score, 1.0);
    assert_eq!(saturated.band, RiskBand::High);
    assert!(!saturated.drivers.is_empty());

    let none = score(&ChangedFacts::default(), &ImpactedFacts::default());
    assert_eq!(none.score, 0.0);
    assert_eq!(none.band, RiskBand::Low);
    assert!(!none.drivers.is_empty());
}

// -------------------------------------------------------------------------------------------
// risk: the filed bug — an empty change set is not a tested one
// -------------------------------------------------------------------------------------------

#[test]
fn an_empty_change_set_emits_no_test_coverage_driver() {
    let result = score(&ChangedFacts::default(), &ImpactedFacts::default());

    assert_eq!(result.score, 0.0);
    assert_eq!(result.band, RiskBand::Low);
    // The two fact sentences remain — the changed-symbols sentence is unconditional so a risk
    // answer is never a bare number — and neither of them claims anything about tests.
    assert_eq!(
        result.drivers,
        vec![
            "no dependents were found inside the expanded radius".to_owned(),
            "0 changed symbols span 0 files".to_owned(),
        ]
    );
    for driver in &result.drivers {
        let lowered = driver.to_lowercase();
        assert!(!lowered.contains("test"), "test-gap claim: {driver}");
        assert!(!lowered.contains("coverage"), "coverage claim: {driver}");
    }
}

#[test]
fn the_test_coverage_sentence_is_gated_on_the_change_set_not_on_the_census() {
    // Zero untested symbols over a real change set is a measurement, so the success sentence stays.
    let covered = score(&change(3, 2, 0, 0, 0), &impact(0, 0, 0, None));
    assert!(
        covered
            .drivers
            .contains(&"every changed symbol has a test within the tested-depth limit".to_owned())
    );

    let gaps = score(&change(3, 2, 0, 2, 0), &impact(0, 0, 0, None));
    assert!(
        gaps.drivers
            .contains(&"2 changed symbols have no reachable test".to_owned())
    );

    // Nothing changed and nothing untested: the same zero, and no sentence.
    let empty = score(&ChangedFacts::default(), &impact(0, 0, 0, None));
    assert!(!empty.drivers.iter().any(|driver| driver.contains("test")));
}
