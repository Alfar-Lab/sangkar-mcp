//! The unresolved taxonomy as a pure function over reader-shaped rows: no database, no extractor,
//! so every rule is provable from one literal.
//!
//! Two things are asserted beyond "each bucket is reachable". First the rule *order*: an import is
//! bucketed by its specifier before `evidence_kind` is read, and an `ambiguous` kind beats the
//! allowlisted call tail, because a bucket that moved with the writer's kind would rewrite the
//! census. Second `material`, which is a separate predicate and must never be inferred from a
//! bucket name: only an `ambiguous` kind or the `receiver_type_unknown` bucket can raise it, and a
//! `dynamic_property_call` never does.

use sangkar_domain::unresolved::{
    BUILTIN_MEMBERS, BUILTIN_RECEIVERS, BucketCount, UNRESOLVED_BUCKETS, UnresolvedBucket,
    UnresolvedCensus, bucket_census, bucket_of,
};
use sangkar_store::read::coverage::UnresolvedRef;

use UnresolvedBucket::{
    Ambiguous, BuiltinStdlib, DynamicPropertyCall, ExternalPackage, InternalModule,
    ReceiverTypeUnknown,
};

const FILE: &str = "src/server/global/whatsapp/registry.ts";

/// One `unresolved` row as `unresolved_for_files` would hand it over. The classifier reads three
/// of the seven fields, so the rest stay constant here.
fn row(raw_target: &str, evidence_kind: Option<&str>, evidence: Option<&str>) -> UnresolvedRef {
    UnresolvedRef {
        id: 0,
        file_id: 1,
        file: FILE.to_owned(),
        line: 0,
        raw_target: raw_target.to_owned(),
        evidence_kind: evidence_kind.map(str::to_owned),
        evidence: evidence.map(str::to_owned),
    }
}

/// A call site: the evidence the writer records is `call:<target as written>`.
fn call_row(target: &str, evidence_kind: Option<&str>) -> UnresolvedRef {
    let evidence = format!("call:{target}");
    row(target, evidence_kind, Some(&evidence))
}

/// An import site: the evidence the writer records is `import:<specifier>`.
fn import_row(specifier: &str, evidence_kind: Option<&str>) -> UnresolvedRef {
    let evidence = format!("import:{specifier}");
    row(specifier, evidence_kind, Some(&evidence))
}

fn count(bucket: UnresolvedBucket, count: usize) -> BucketCount {
    BucketCount { bucket, count }
}

/// Builds a census through the one entry point and asserts the invariants every fixture shares.
fn census(rows: &[UnresolvedRef]) -> UnresolvedCensus {
    let census = bucket_census(rows);
    let counted: usize = census.buckets.iter().map(|entry| entry.count).sum();
    assert_eq!(census.total, rows.len(), "total is the row count");
    assert_eq!(counted, census.total, "the listed buckets sum to `total`");
    assert!(
        census.material <= census.total,
        "material ({}) exceeds total ({})",
        census.material,
        census.total
    );
    assert!(
        census.buckets.iter().all(|entry| entry.count > 0),
        "a bucket with no rows is never listed"
    );
    census
}

#[test]
fn the_closed_set_is_the_six_labels_in_taxonomy_order() {
    assert_eq!(
        UnresolvedBucket::ALL.map(UnresolvedBucket::as_str),
        UNRESOLVED_BUCKETS
    );
}

#[test]
fn cross_product_of_writer_evidence_kinds_and_call_shapes() {
    // The writer can record four kinds; the call tail is where the allowlists and the kind
    // disagree. An allowlisted receiver or member beats `dynamic`, and `ambiguous` beats both
    // because the row never bound a single target — it gets its own bucket, not the unknown one.
    let cases: [(&str, &str, UnresolvedBucket); 12] = [
        ("receiver_type_unknown", "Map.has", BuiltinStdlib),
        ("receiver_type_unknown", "push", BuiltinStdlib),
        ("receiver_type_unknown", "legacyThing", ReceiverTypeUnknown),
        ("ambiguous", "Map.has", Ambiguous),
        ("ambiguous", "push", Ambiguous),
        ("ambiguous", "legacyThing", Ambiguous),
        ("dynamic", "Map.has", BuiltinStdlib),
        ("dynamic", "push", BuiltinStdlib),
        ("dynamic", "legacyThing", DynamicPropertyCall),
        ("missing_module", "Map.has", BuiltinStdlib),
        ("missing_module", "push", BuiltinStdlib),
        ("missing_module", "legacyThing", ReceiverTypeUnknown),
    ];

    for (evidence_kind, target, expected) in cases {
        assert_eq!(
            bucket_of(&call_row(target, Some(evidence_kind))),
            expected,
            "call:{target} recorded {evidence_kind}"
        );
    }

    // The same cross product as one census: half of it lands in `builtin_stdlib` because the kind
    // is not `ambiguous`, and the rows whose target stayed unknown or tied are the material half.
    let rows: Vec<UnresolvedRef> = cases
        .iter()
        .map(|(evidence_kind, target, _)| call_row(target, Some(*evidence_kind)))
        .collect();
    let census = census(&rows);
    assert_eq!(census.total, 12);
    assert_eq!(census.count(BuiltinStdlib), 6);
    assert_eq!(census.count(ReceiverTypeUnknown), 2);
    assert_eq!(census.count(DynamicPropertyCall), 1);
    // Two unknown plus three ambiguous: the ambiguous rows are material by their own predicate.
    assert_eq!(census.count(Ambiguous), 3);
    assert_eq!(census.material, 5);
}

#[test]
fn imports_are_bucketed_by_specifier_before_evidence_kind_is_read() {
    let simple: [(&str, UnresolvedBucket); 5] = [
        ("node:fs", BuiltinStdlib),
        ("bun:test", BuiltinStdlib),
        ("fs/promises", BuiltinStdlib),
        ("react", ExternalPackage),
        ("#shared/paths", InternalModule),
    ];
    for (specifier, expected) in simple {
        assert_eq!(
            bucket_of(&import_row(specifier, Some("dynamic"))),
            expected,
            "import:{specifier}"
        );
    }

    // The competing candidates the writer appends after the specifier must not turn a relative
    // import into a package.
    let tied = row(
        "./x.js",
        Some("ambiguous"),
        Some("import:./x.js candidates: a.ts | b.ts"),
    );
    assert_eq!(bucket_of(&tied), InternalModule);

    // The same row is material through its kind, while its bucket stays the specifier's.
    let census = census(&[tied]);
    assert_eq!(census.count(InternalModule), 1);
    assert_eq!(census.material, 1);
}

#[test]
fn a_missing_relative_module_lands_in_internal_module() {
    let missing = row(
        "../types.js",
        Some("missing_module"),
        Some("import:../types.js (no file matched)"),
    );
    assert_eq!(bucket_of(&missing), InternalModule);

    let census = census(&[missing]);
    assert_eq!(census.count(InternalModule), 1);
    // A relative import that matched nothing is a defect in the tree, not a hidden caller.
    assert_eq!(census.material, 0);
}

#[test]
fn every_allowlisted_receiver_buckets_a_dotted_call_as_builtin() {
    assert!(!BUILTIN_RECEIVERS.is_empty());
    for receiver in BUILTIN_RECEIVERS {
        // A member outside the member list, so the receiver is the only path to `builtin_stdlib`,
        // and a `dynamic` kind the allowlist must beat.
        assert_eq!(
            bucket_of(&call_row(
                &format!("{receiver}.someUnlistedMember"),
                Some("dynamic")
            )),
            BuiltinStdlib,
            "receiver {receiver}"
        );
    }
}

#[test]
fn every_allowlisted_member_buckets_a_bare_call_as_builtin() {
    assert!(!BUILTIN_MEMBERS.is_empty());
    for member in BUILTIN_MEMBERS {
        // `receiver_type_unknown` evidence, so the member allowlist is the only path to
        // `builtin_stdlib`.
        assert_eq!(
            bucket_of(&call_row(member, Some("receiver_type_unknown"))),
            BuiltinStdlib,
            "member {member}"
        );
    }
}

#[test]
fn a_mixed_census_reports_non_zero_buckets_in_taxonomy_order() {
    let rows = vec![
        // Recorded `ambiguous` by the import pass, but still bucketed by its specifier.
        row(
            "../types.js",
            Some("ambiguous"),
            Some("import:../types.js candidates: a.ts | b.ts"),
        ),
        import_row("lodash", Some("dynamic")),
        import_row("node:fs", Some("dynamic")),
        call_row("push", Some("dynamic")),
        call_row("cache.get", Some("dynamic")),
        call_row("obj[0]", Some("dynamic")),
        call_row("store.load", None),
        call_row("handler", Some("ambiguous")),
        call_row("legacyThing", Some("missing_module")),
    ];

    let census = census(&rows);
    assert_eq!(census.total, 9);
    assert_eq!(
        census.buckets,
        vec![
            count(BuiltinStdlib, 3),
            count(ExternalPackage, 1),
            count(InternalModule, 1),
            count(DynamicPropertyCall, 1),
            count(ReceiverTypeUnknown, 2),
            count(Ambiguous, 1),
        ]
    );
    // The ambiguous import is material through its kind; `handler` is material by kind and sits in
    // the ambiguous bucket; the two unknown-receiver rows are material by bucket; `obj[0]` and the
    // two non-material imports are not.
    assert_eq!(census.material, 4);
}

#[test]
fn dynamic_property_calls_never_contribute_to_material() {
    let rows = vec![
        call_row("obj[0]", Some("dynamic")),
        call_row("store.legacyThing", Some("dynamic")),
        call_row("thing.someUnlistedMember", Some("dynamic")),
    ];
    for row in &rows {
        assert_eq!(bucket_of(row), DynamicPropertyCall, "{}", row.raw_target);
    }

    let census = census(&rows);
    assert_eq!(census.count(DynamicPropertyCall), rows.len());
    assert_eq!(census.material, 0);
    assert!(census.total > 0);
}

#[test]
fn no_rows_is_an_empty_census_not_an_absent_one() {
    let census = census(&[]);
    assert_eq!(census.total, 0);
    assert_eq!(census.material, 0);
    assert!(census.buckets.is_empty());
}

#[test]
fn evidence_that_is_not_import_ambiguous_or_call_is_receiver_type_unknown() {
    let rows = [
        row("Base", None, Some("inherits")),
        row("render", None, Some("call")),
        row("render", None, Some("import")),
        row("render", None, None),
        row("store.legacyThing", None, Some("call:store.legacyThing")),
    ];
    for row in &rows {
        assert_eq!(bucket_of(row), ReceiverTypeUnknown, "{}", row.raw_target);
    }
}
