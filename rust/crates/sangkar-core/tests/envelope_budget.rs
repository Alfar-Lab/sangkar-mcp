//! The envelope contract and the single truncation point.
//!
//! Every case of `tests/budget.test.ts` is transcribed here, plus the wire-shape assertions the
//! TypeScript side gets for free from its interface and a golden byte count. One expectation could
//! not be expressed in Rust and is marked where it occurs: the check that an emitted envelope is
//! the very object it was handed (Rust moves it).

use serde::Serialize;
use serde_json::{Value, json};

use sangkar_core::budget::{BYTES_PER_TOKEN, MAX_BUDGET_TOKENS, budget_floor_of, emit};
use sangkar_core::cursor::{Cursor, CursorExpectation, CursorPayload, CursorResult};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, DEFAULT_BOOTSTRAP_MESSAGE, Envelope, EnvelopeSpec,
    NotIndexedEntry, ReadStatus, Status, TotalRelation, Totals, TruncationReason, ambiguous,
    bootstrapping, fail, not_found, ok, relation_for, with_payload, with_totals,
};
use sangkar_core::errors::{AppError, ErrorCode};

/// The budget every coverage-trim case uses; `budget_byte_allowance` is `tokens * 4` inside the
/// ceiling, so the allowance is written that way rather than reaching into the module's helper.
const BUDGET_TOKENS: u64 = 800;
const ALLOWANCE: u64 = BUDGET_TOKENS * BYTES_PER_TOKEN;

fn fresh() -> ReadStatus {
    ReadStatus::new("g-7f3a91", "2026-11-17T13:50:15Z")
}

fn clean() -> Coverage {
    Coverage::complete(530)
}

fn base_spec() -> EnvelopeSpec {
    EnvelopeSpec::new("showing N of M", fresh(), clean())
}

/// `Buffer.byteLength(JSON.stringify(value))`: the module's unit of account.
fn bytes<T: Serialize + ?Sized>(value: &T) -> u64 {
    serde_json::to_vec(value)
        .expect("the fixture is serialisable")
        .len() as u64
}

fn object_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .expect("an object")
        .keys()
        .cloned()
        .collect()
}

fn items(count: usize, prefix: &str) -> Vec<Value> {
    (0..count)
        .map(|index| {
            json!({
                "id": index + 1,
                "qn": format!("{prefix}/mod{index}.py::fn_{index}"),
                "kind": "function",
                "file": format!("{prefix}/mod{index}.py"),
                "line": 10 * index + 3,
            })
        })
        .collect()
}

fn mulberry32(seed: u32) -> impl FnMut() -> f64 {
    let mut state = seed;
    move || {
        state = state.wrapping_add(0x6d2b_79f5);
        let mut t = state;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        f64::from(t ^ (t >> 14)) / 4_294_967_296.0
    }
}

fn is_prefix_of(candidate: &[Value], original: &[Value]) -> bool {
    candidate.len() <= original.len() && candidate.iter().zip(original).all(|(a, b)| a == b)
}

fn is_prefix_of_entries(candidate: &[NotIndexedEntry], original: &[NotIndexedEntry]) -> bool {
    candidate.len() <= original.len() && candidate.iter().zip(original).all(|(a, b)| a == b)
}

fn paths_of(entries: &[NotIndexedEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.clone()).collect()
}

fn array_of<'a>(value: &'a Value, pointer: &str) -> &'a [Value] {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("{pointer} is an array"))
}

// ---------------------------------------------------------------------------------------------
// emit: budget property
// ---------------------------------------------------------------------------------------------

#[test]
fn two_hundred_random_payloads_stay_inside_budget_with_totals_preserved() {
    let mut rand = mulberry32(0x5eed);
    let mut violations: Vec<String> = Vec::new();
    let mut truncated_runs = 0_usize;
    let mut fitting_runs = 0_usize;

    for run in 0..200 {
        let calls = items(5 + (rand() * 60.0) as usize, "app");
        let callees = items(5 + (rand() * 30.0) as usize, "lib");
        let calls_len = calls.len();
        let callees_len = callees.len();
        let spec = base_spec().with_totals(
            Totals::new(TotalRelation::Eq)
                .with_total("callers", Some(calls_len as u64))
                .with_total("callees", Some(callees_len as u64)),
        );
        let env = ok(
            json!({"calls": calls.clone(), "callees": callees.clone()}),
            spec,
        );
        let original_bytes = bytes(&env);
        let budget_tokens = 400 + (rand() * 1600.0) as u64;
        let budget_bytes = budget_tokens * BYTES_PER_TOKEN;
        let mut minted: Vec<usize> = Vec::new();
        let out = emit(env, budget_tokens, |position| {
            minted.push(position);
            format!("cur_{position}")
        })
        .expect("the budget is inside the ceiling");

        let where_ = format!("run {run} (original {original_bytes} B, budget {budget_bytes} B)");
        let out_bytes = bytes(&out);
        let should_truncate = original_bytes > budget_bytes;
        let payload = out.payload.as_ref().expect("ok carries a payload");

        if out_bytes * 10 > budget_bytes * 11 {
            violations.push(format!(
                "{where_}: emitted {out_bytes} B, over budget + 10%"
            ));
        }
        if out.totals.get("callers_total") != Some(&json!(calls_len))
            || out.totals.get("callees_total") != Some(&json!(callees_len))
        {
            violations.push(format!(
                "{where_}: a *_total was not preserved ({} B object)",
                bytes(out.totals.as_map())
            ));
        }
        // `total_relation` cannot leave the contract in Rust: `Totals` stores a `TotalRelation`.
        if !is_prefix_of(array_of(payload, "/calls"), &calls)
            || !is_prefix_of(array_of(payload, "/callees"), &callees)
        {
            violations.push(format!(
                "{where_}: truncation reordered or rewrote the surviving items"
            ));
        }
        // The payload arrays `emit` trims are its own copy: `calls`/`callees` were cloned in, so a
        // cut landing on them is a stale-`Value` bug. TS asserts the same two lengths.
        if calls.len() != calls_len || callees.len() != callees_len {
            violations.push(format!("{where_}: the caller's payload was mutated"));
        }

        if should_truncate {
            truncated_runs += 1;
            if !out.truncated {
                violations.push(format!(
                    "{where_}: truncated is false but the payload was cut"
                ));
            }
            if minted.len() != 1 {
                violations.push(format!("{where_}: mint called {} times", minted.len()));
            }
            if out.next != Some(format!("cur_{}", minted[0])) {
                violations.push(format!("{where_}: next is not the minted token"));
            }
            if out.truncation_reason != Some(TruncationReason::Budget) {
                violations.push(format!(
                    "{where_}: truncation_reason is not budget ({:?})",
                    out.truncation_reason
                ));
            }
            if out.totals.relation() != TotalRelation::Gte {
                violations.push(format!("{where_}: a truncated payload still claims eq"));
            }
        } else {
            fitting_runs += 1;
            if out.truncated || out.next.is_some() || !minted.is_empty() {
                violations.push(format!(
                    "{where_}: an uncut payload was reported as truncated"
                ));
            }
            if out.totals.relation() != TotalRelation::Eq {
                violations.push(format!(
                    "{where_}: an uncut, clean payload lost its eq relation"
                ));
            }
        }
    }

    assert!(violations.is_empty(), "{}", violations.join("\n"));
    assert!(truncated_runs > 0, "no run was cut, the sweep is vacuous");
    assert!(fitting_runs > 0, "no run fitted, the sweep is vacuous");
    assert_eq!(truncated_runs + fitting_runs, 200);
}

// ---------------------------------------------------------------------------------------------
// emit: the gte downgrade and the mint callback
// ---------------------------------------------------------------------------------------------

#[test]
fn a_truncated_clean_payload_is_downgraded_from_eq_to_gte_and_carries_next_from_mint() {
    let spec = base_spec().with_totals(
        Totals::new(TotalRelation::Eq)
            .with_total("callers", Some(40))
            .with_total("callees", Some(4)),
    );
    let env = ok(
        json!({"calls": items(40, "app"), "callees": items(4, "lib")}),
        spec,
    );
    assert_eq!(env.totals.relation(), TotalRelation::Eq);

    let mut minted: Vec<usize> = Vec::new();
    let out = emit(env, 900, |position| {
        minted.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");

    assert!(out.truncated);
    assert_eq!(out.truncation_reason, Some(TruncationReason::Budget));
    assert_eq!(out.totals.relation(), TotalRelation::Gte);
    assert_eq!(out.totals.get("callers_total"), Some(&json!(40)));
    assert_eq!(out.totals.get("callees_total"), Some(&json!(4)));
    assert_eq!(minted.len(), 1);
    assert_eq!(out.next, Some(format!("cur_{}", minted[0])));
    assert!(minted[0] > 0, "a cut of zero items must not be minted");
    assert!(bytes(&out) * 10 <= 900 * BYTES_PER_TOKEN * 11);
}

#[test]
fn mint_is_called_exactly_once_and_its_position_equals_the_number_of_dropped_items() {
    let env = ok(
        json!({"calls": items(50, "app"), "callees": items(3, "lib")}),
        base_spec(),
    );
    let mut minted: Vec<usize> = Vec::new();
    let out = emit(env, 700, |position| {
        minted.push(position);
        format!("p{position}")
    })
    .expect("inside the ceiling");

    let payload = out.payload.as_ref().expect("payload");
    let kept = array_of(payload, "/calls").len() + array_of(payload, "/callees").len();
    assert_eq!(minted.len(), 1);
    assert_eq!(minted[0], 53 - kept);
    assert_eq!(out.next, Some(format!("p{}", minted[0])));
}

#[test]
fn the_largest_list_is_cut_first_and_the_smaller_list_survives_intact() {
    let calls = items(60, "app");
    let env = ok(
        json!({"calls": calls.clone(), "callees": items(3, "lib")}),
        base_spec(),
    );
    let out = emit(env, 1000, |position| format!("cur_{position}")).expect("inside the ceiling");

    let payload = out.payload.as_ref().expect("payload");
    assert_eq!(array_of(payload, "/callees").len(), 3);
    let kept_calls = array_of(payload, "/calls");
    assert!(kept_calls.len() < 60);
    assert!(!kept_calls.is_empty());
    assert!(is_prefix_of(kept_calls, &calls));
}

#[test]
fn an_envelope_that_fits_is_returned_untouched_and_without_a_mint_call() {
    let env = ok(json!({"calls": items(2, "app")}), base_spec());
    // The TypeScript case asserts object identity (`toBe`); a Rust envelope is moved in, so the
    // observable equivalent is that the answer is deep-equal to what was handed over.
    let snapshot = env.clone();

    let mut calls = 0_usize;
    let out = emit(env, MAX_BUDGET_TOKENS, |_| {
        calls += 1;
        "cur_never".to_owned()
    })
    .expect("inside the ceiling");

    assert_eq!(out, snapshot);
    assert_eq!(calls, 0);
    assert!(!out.truncated);
    assert_eq!(out.next, None);
    assert_eq!(out.totals.relation(), TotalRelation::Eq);
}

#[test]
fn the_callers_payload_arrays_are_never_mutated() {
    let calls = items(30, "app");
    let env = ok(json!({"calls": calls.clone()}), base_spec());
    let snapshot = env.clone();
    let out = emit(env, 400, |position| format!("cur_{position}")).expect("inside the ceiling");

    assert_eq!(calls.len(), 30);
    assert_eq!(
        array_of(snapshot.payload.as_ref().expect("payload"), "/calls").len(),
        30
    );
    assert!(out.truncated);
    assert!(array_of(out.payload.as_ref().expect("payload"), "/calls").len() < 30);
}

#[test]
fn a_caller_flagged_truncation_may_not_keep_eq_even_when_the_payload_fits() {
    let spec = base_spec()
        .with_totals(Totals::new(TotalRelation::Eq).with_total("callers", Some(2)))
        .with_truncation(TruncationReason::Depth, Some("cur_depth".to_owned()));
    let env = ok(json!({"calls": items(2, "app")}), spec);

    let out = emit(env, MAX_BUDGET_TOKENS, |_| "cur_never".to_owned()).expect("inside the ceiling");
    assert_eq!(out.totals.relation(), TotalRelation::Gte);
    assert_eq!(out.truncation_reason, Some(TruncationReason::Depth));
    assert_eq!(out.next.as_deref(), Some("cur_depth"));
    assert_eq!(out.totals.get("callers_total"), Some(&json!(2)));
    assert_eq!(out.status, Status::Ok);
}

#[test]
fn nested_item_lists_are_reachable_by_the_budget() {
    let env = ok(
        json!({
            "groups": [{"name": "calls", "derived": false, "items": items(40, "app")}],
            "callees": items(2, "lib"),
        }),
        base_spec(),
    );
    let out = emit(env, 700, |position| format!("cur_{position}")).expect("inside the ceiling");
    assert!(out.truncated);
    assert_eq!(out.totals.relation(), TotalRelation::Gte);
    assert!(bytes(&out) * 10 <= 700 * BYTES_PER_TOKEN * 11);
}

#[test]
fn a_payload_with_no_lists_to_shrink_is_returned_uncut_rather_than_mislabelled() {
    // The done-when case: an envelope over budget whose drop frees zero bytes comes back unchanged
    // — `truncated == false`, `next == None`, `mint` never called — rather than labelled with a cut
    // that freed nothing.
    let env = ok(
        json!({"note": "the whole answer is one sentence"}),
        base_spec(),
    );
    let snapshot = env.clone();
    assert!(bytes(&env) > 10 * BYTES_PER_TOKEN);

    let mut calls = 0_usize;
    let out = emit(env, 10, |_| {
        calls += 1;
        "cur_none".to_owned()
    })
    .expect("inside the ceiling");

    assert!(!out.truncated);
    assert_eq!(out.next, None);
    assert_eq!(out.truncation_reason, None);
    assert_eq!(calls, 0);
    assert_eq!(out, snapshot);
}

// ---------------------------------------------------------------------------------------------
// emit: the 32 000 token ceiling
// ---------------------------------------------------------------------------------------------

#[test]
fn a_budget_above_the_ceiling_is_refused_with_budget_too_large() {
    let env = ok(json!({"calls": items(5, "app")}), base_spec());

    for budget in [40_000_u64, MAX_BUDGET_TOKENS + 1] {
        let error = emit(env.clone(), budget, |_| "cur".to_owned())
            .expect_err("a budget above the ceiling is refused");
        assert_eq!(error.code, ErrorCode::BUDGET_TOO_LARGE);
        assert_eq!(error.code.as_str(), "budget_too_large");
        assert!(
            error.message.contains(&budget.to_string()),
            "the message names the budget it refused: {}",
            error.message
        );
        assert!(error.hint.is_some(), "the refusal carries a hint");
    }
}

#[test]
fn the_ceiling_itself_is_accepted() {
    let env = ok(json!({"calls": items(5, "app")}), base_spec());
    let out = emit(env, MAX_BUDGET_TOKENS, |_| "cur".to_owned()).expect("the ceiling is allowed");
    assert_eq!(out.status, Status::Ok);
    assert_eq!(MAX_BUDGET_TOKENS * BYTES_PER_TOKEN, 128_000);
}

// ---------------------------------------------------------------------------------------------
// emit: trimming the envelope-level coverage list
// ---------------------------------------------------------------------------------------------

/// Skipped paths in the reader's own order (path ascending), which is what a trim cuts from the end.
fn skipped(count: usize) -> Vec<NotIndexedEntry> {
    (0..count)
        .map(|index| {
            NotIndexedEntry::new(
                format!("code_review_graph/module_{index:03}.xslt"),
                "unsupported_ext",
            )
        })
        .collect()
}

/// Coverage over a scope that skipped `not_indexed.len()` paths: incomplete, and it says so.
fn skipped_spec(entries: &[NotIndexedEntry]) -> EnvelopeSpec {
    EnvelopeSpec::new(
        format!("{} paths were looked at and skipped", entries.len()),
        fresh(),
        Coverage {
            complete: false,
            indexed: 400,
            skipped: entries.len() as u64,
            failed: 0,
            unresolved_edges: 0,
            not_indexed: entries.to_vec(),
            tests: None,
        },
    )
}

#[test]
fn a_large_coverage_not_indexed_is_trimmed_until_the_envelope_fits_the_allowance() {
    let entries = skipped(80);
    let env = ok(
        json!({"base": "HEAD", "risk": {"score": 0, "band": "low", "drivers": ["changed_files", "untested"]}}),
        skipped_spec(&entries),
    );
    // The shape the defect was reproduced on: a 126 B payload inside a 6 KB envelope.
    assert!(bytes(&env) > ALLOWANCE);

    let mut positions: Vec<usize> = Vec::new();
    let out = emit(env, BUDGET_TOKENS, |position| {
        positions.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");

    assert!(bytes(&out) <= ALLOWANCE);
    assert!(out.truncated);
    assert_eq!(out.truncation_reason, Some(TruncationReason::Budget));
    assert_eq!(out.totals.relation(), TotalRelation::Gte);

    // Coverage stays honest: the count is exact, `complete` stays false, and what is left is the
    // head of the reader's path order rather than an arbitrary sample.
    assert_eq!(out.coverage.skipped, 80);
    assert!(!out.coverage.complete);
    assert!(!out.coverage.not_indexed.is_empty());
    assert!(out.coverage.not_indexed.len() < 80);
    assert!(is_prefix_of_entries(&out.coverage.not_indexed, &entries));

    // The payload's own list is drained first, then the coverage list pays for the rest, and the
    // count `emit` reports is everything that left the envelope — rows and paths alike.
    let payload = out.payload.as_ref().expect("payload");
    let drivers = array_of(payload, "/risk/drivers");
    assert!(drivers.is_empty());
    let dropped_rows = 2 - drivers.len();
    let dropped_paths = entries.len() - out.coverage.not_indexed.len();
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0], dropped_rows + dropped_paths);
    assert_eq!(out.next, Some(format!("cur_{}", positions[0])));
}

#[test]
fn a_path_the_payload_answers_about_survives_the_trim_and_the_unpinned_tail_does_not() {
    // Sorts last, so a trim that only knew about paths would drop it first.
    let touched = "code_review_graph/zzz_touched.xslt";
    let mut entries = skipped(80);
    entries.push(NotIndexedEntry::new(touched, "grammar_unavailable"));

    let answering = ok(
        json!({"paths": [{"path": touched, "indexed": false, "reason": "grammar_unavailable"}]}),
        skipped_spec(&entries),
    );
    let blind = ok(json!({"paths": []}), skipped_spec(&entries));

    let kept = emit(answering, BUDGET_TOKENS, |position| {
        format!("cur_{position}")
    })
    .expect("inside the ceiling");
    let cut = emit(blind, BUDGET_TOKENS, |position| format!("cur_{position}"))
        .expect("inside the ceiling");

    let kept_paths = paths_of(&kept.coverage.not_indexed);
    let cut_paths = paths_of(&cut.coverage.not_indexed);

    // The control proves the pin changed the outcome rather than the trim being lenient.
    assert!(kept_paths.iter().any(|path| path == touched));
    assert!(!cut_paths.iter().any(|path| path == touched));
    assert!(kept_paths.len() < entries.len());
    assert_eq!(kept.coverage.skipped as usize, entries.len());
    assert!(bytes(&kept) <= ALLOWANCE);
}

#[test]
fn the_callers_coverage_not_indexed_array_is_never_mutated_by_a_trim() {
    let entries = skipped(80);
    let env = ok(json!({"base": "HEAD"}), skipped_spec(&entries));
    let before = paths_of(&entries);
    let snapshot = env.clone();

    let out =
        emit(env, BUDGET_TOKENS, |position| format!("cur_{position}")).expect("inside the ceiling");

    assert!(out.coverage.not_indexed.len() < before.len());
    // Neither the envelope the caller passed in nor the fixture it was built from lost an entry.
    assert_eq!(paths_of(&snapshot.coverage.not_indexed), before);
    assert_eq!(paths_of(&entries), before);
    assert!(!snapshot.truncated);
    assert_eq!(snapshot.next, None);
    // The trimmed list belongs to the draft's own copy of the coverage object, which is what makes
    // the mutation safe; TS proves the same fact with an identity check on the array.
    assert_ne!(
        out.coverage.not_indexed.len(),
        snapshot.coverage.not_indexed.len()
    );
}

#[test]
fn a_list_a_cut_took_with_its_parent_is_not_counted_as_trimmed() {
    // `groups` holds two groups of 40 items; `spill` is a further 60 items beside it. The budget is
    // small enough that draining `groups` is not enough, so the pass has to reach `spill` — and the
    // 80 items that left with the groups are not lists it may cut a second time.
    let env = ok(
        json!({
            "groups": [
                {"name": "calls", "derived": false, "items": items(40, "app")},
                {"name": "calls", "derived": false, "items": items(40, "lib")},
            ],
            "spill": items(60, "lib"),
        }),
        base_spec(),
    );

    let mut positions: Vec<usize> = Vec::new();
    let out = emit(env, 500, |position| {
        positions.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");

    assert!(bytes(&out) <= 500 * BYTES_PER_TOKEN);
    let payload = out.payload.as_ref().expect("payload");
    assert_eq!(array_of(payload, "/groups").len(), 0);
    let spill = array_of(payload, "/spill").len();
    assert!(spill > 0);
    assert!(spill < 60);

    // Two groups plus the spill rows that went: counting the detached lists would report ~80 more
    // drops than the envelope lost, and a client resuming from that count would skip rows.
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0], 2 + (60 - spill));
}

#[test]
fn every_budget_from_the_envelopes_floor_upwards_is_honoured_with_one_mint_and_kept_totals() {
    let calls = items(40, "app");
    let entries = skipped(30);
    let spec = skipped_spec(&entries).with_totals(
        Totals::new(TotalRelation::Gte)
            .with_total("calls", Some(calls.len() as u64))
            .with_total("skipped", Some(entries.len() as u64)),
    );

    // The floor: every list drained, so what is left is the skeleton, the required fields and the
    // token. Rounding it up to whole tokens is the smallest budget the envelope can honour.
    let floor_env = ok(json!({"calls": []}), skipped_spec(&entries));
    let floor = emit(floor_env, BUDGET_TOKENS, |position| {
        format!("cur_{position}")
    })
    .expect("inside the ceiling");
    let smallest_honoured = bytes(&floor).div_ceil(BYTES_PER_TOKEN);

    let mut violations: Vec<String> = Vec::new();
    let mut cut_runs = 0_usize;
    let mut whole_runs = 0_usize;
    for budget_tokens in [
        smallest_honoured,
        smallest_honoured + 1,
        400,
        600,
        900,
        1200,
        2000,
    ] {
        let env = ok(json!({"calls": calls.clone()}), spec.clone());
        let mut positions: Vec<usize> = Vec::new();
        let out = emit(env, budget_tokens, |position| {
            positions.push(position);
            format!("cur_{position}")
        })
        .expect("inside the ceiling");
        let where_ = format!(
            "{budget_tokens} tokens ({} B)",
            budget_tokens * BYTES_PER_TOKEN
        );
        let allowance = budget_tokens * BYTES_PER_TOKEN;
        let payload = out.payload.as_ref().expect("payload");

        if bytes(&out) > allowance {
            violations.push(format!("{where_}: emitted {} B", bytes(&out)));
        }
        if out.totals.get("calls_total") != Some(&json!(calls.len()))
            || out.totals.get("skipped_total") != Some(&json!(entries.len()))
        {
            violations.push(format!("{where_}: a *_total was not preserved"));
        }
        if !is_prefix_of(array_of(payload, "/calls"), &calls) {
            violations.push(format!(
                "{where_}: the surviving rows are not the ranked head"
            ));
        }
        if !is_prefix_of_entries(&out.coverage.not_indexed, &entries) {
            violations.push(format!(
                "{where_}: the surviving paths are not the path-order head"
            ));
        }

        if out.truncated {
            cut_runs += 1;
            if out.truncation_reason != Some(TruncationReason::Budget) {
                violations.push(format!(
                    "{where_}: truncation_reason is not budget ({:?})",
                    out.truncation_reason
                ));
            }
            if out.totals.relation() != TotalRelation::Gte {
                violations.push(format!("{where_}: a cut envelope still claims eq"));
            }
            if positions.len() != 1 {
                violations.push(format!("{where_}: mint called {} times", positions.len()));
            }
            let dropped_rows = calls.len() - array_of(payload, "/calls").len();
            let dropped_paths = entries.len() - out.coverage.not_indexed.len();
            if positions.first() != Some(&(dropped_rows + dropped_paths)) {
                violations.push(format!(
                    "{where_}: minted {:?} for {} dropped items",
                    positions.first(),
                    dropped_rows + dropped_paths
                ));
            }
            if out.next != Some(format!("cur_{}", positions[0])) {
                violations.push(format!("{where_}: next is not the minted token"));
            }
        } else {
            whole_runs += 1;
            if !positions.is_empty() {
                violations.push(format!("{where_}: mint called without a cut"));
            }
            if out.next.is_some() {
                violations.push(format!("{where_}: next present without a cut"));
            }
        }
    }

    assert!(violations.is_empty(), "{}", violations.join("\n"));
    // Both halves ran, so the sweep is not vacuous.
    assert!(cut_runs > 0, "no budget was cut, the sweep is vacuous");
    assert!(whole_runs > 0, "no budget fitted, the sweep is vacuous");
}

#[test]
fn below_the_envelopes_own_floor_the_envelope_is_returned_untouched_and_unlabelled() {
    let calls = items(40, "app");
    let entries = skipped(20);
    let spec = skipped_spec(&entries)
        .with_totals(Totals::new(TotalRelation::Gte).with_total("calls", Some(40)));
    let env = ok(json!({"calls": calls}), spec);
    let snapshot = env.clone();

    let mut positions: Vec<usize> = Vec::new();
    let out = emit(env, 1, |position| {
        positions.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");

    // Nothing was cut, so nothing may say it was, and no token may point at a page that would be
    // byte-identical to this one.
    assert!(!out.truncated);
    assert_eq!(out.truncation_reason, None);
    assert_eq!(out.next, None);
    assert!(positions.is_empty());
    assert_eq!(out, snapshot);
    assert_eq!(bytes(&out), bytes(&snapshot));

    // A caller-flagged truncation survives the floor untouched, so `emit` never erases the caller's
    // own claim (`SearchEnvelope`-style zero-row pages depend on that).
    let flagged_env = Envelope {
        truncated: true,
        truncation_reason: Some(TruncationReason::Budget),
        ..snapshot.clone()
    };
    let flagged = emit(flagged_env, 1, |position| {
        positions.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");
    assert!(flagged.truncated);
    assert_eq!(flagged.totals.relation(), TotalRelation::Gte);
    assert!(positions.is_empty());

    // The floor is a floor, not a leak: just under it the same payload is cut and minted.
    let smallest_honoured = bytes(&snapshot).div_ceil(BYTES_PER_TOKEN);
    let tight = std::cmp::max(2, smallest_honoured - 2);
    let cut = emit(snapshot, tight, |position| {
        positions.push(position);
        format!("cur_{position}")
    })
    .expect("inside the ceiling");
    assert!(bytes(&cut) <= tight * BYTES_PER_TOKEN);
    assert!(cut.truncated);
    assert_eq!(positions.len(), 1);
}

// ---------------------------------------------------------------------------------------------
// emit: the reserve charged for the continuation token
// ---------------------------------------------------------------------------------------------

/// The cursor a real call mints is longer than this: `cursor.ts` stamps the repository's canonical
/// path into the wire, which takes this repository's token to 298 bytes. The reserve is 384, and
/// charging exactly that many characters is what keeps a filled envelope inside its own ceiling.
const TOKEN_CHARS: usize = 384;

#[test]
fn a_repository_stamped_token_keeps_a_filled_envelope_inside_its_budget() {
    let token = format!("cur_{}", "t".repeat(TOKEN_CHARS - 4));
    assert_eq!(token.chars().count(), TOKEN_CHARS);
    let env = ok(json!({"calls": items(400, "app")}), base_spec());

    let mut cut_runs = 0_usize;
    for budget in [700_u64, 800, 900, 1000, 1200, 2000] {
        let out = emit(env.clone(), budget, |_| token.clone()).expect("inside the ceiling");
        if !out.truncated {
            continue;
        }
        cut_runs += 1;
        // Byte-exact: not budget + 10 %, the ceiling itself.
        assert!(
            bytes(&out) <= budget * BYTES_PER_TOKEN,
            "at {budget} tokens the emitted envelope is {} B",
            bytes(&out)
        );
        // The token in the envelope is the caller's own, which is what the reserve paid for.
        assert_eq!(out.next.as_deref(), Some(token.as_str()));
    }
    assert!(
        cut_runs > 0,
        "no budget was filled, the reserve is untested"
    );
}

#[test]
fn a_cursor_bound_to_a_second_repository_cannot_resume_this_ones_page() {
    // Transcribed from `tests/budget.test.ts`. `emit` delegates the token to `mint`, so the repo
    // binding is enforced by `cursor.rs`; this test exists to pin the seam between the two: the
    // envelope carries the page the token addresses, and a reader holding the same key inside a
    // different repository still cannot resume it.
    let key = "0123456789abcdef0123456789abcdef";
    let here = Cursor::new(key, Some("repo-a".to_owned())).expect("a 32-byte key is enough");
    let elsewhere = Cursor::new(key, Some("repo-b".to_owned())).expect("a 32-byte key is enough");
    let payload = CursorPayload {
        generation: "g-1a2b3c".to_owned(),
        tool: "trace".to_owned(),
        args_hash: format!("sha256:{}", "0".repeat(64)),
        position: 40,
    };
    let expectation = CursorExpectation {
        generation: payload.generation.clone(),
        tool: payload.tool.clone(),
        args_hash: payload.args_hash.clone(),
    };

    let token = here.sign(&payload);
    let env = ok(json!({"calls": items(400, "app")}), base_spec());
    let out = emit(env, 700, |_| token.clone()).expect("inside the ceiling");
    assert!(out.truncated, "700 tokens must cut 400 calls");
    let next = out.next.expect("a cut envelope carries a token");

    assert_eq!(
        here.verify(&next, &expectation),
        CursorResult::Accepted { position: 40 }
    );
    let refusal = elsewhere
        .verify(&next, &expectation)
        .refusal()
        .expect("another repository must be refused");
    assert_eq!(refusal.code, ErrorCode::CURSOR_MISMATCH);
    assert_eq!(
        refusal.hint,
        "re-run the original call with the same tool and arguments"
    );
}

// ---------------------------------------------------------------------------------------------
// the envelope contract itself
// ---------------------------------------------------------------------------------------------

#[test]
fn metadata_keys_are_always_present_in_contract_order() {
    let envelope = ok(json!({"calls": []}), base_spec());
    let value = serde_json::to_value(&envelope).expect("serialisable");
    assert_eq!(
        object_keys(&value),
        [
            "status",
            "summary",
            "read_status",
            "coverage",
            "totals",
            "truncated",
            "truncation_reason",
            "next",
            "next_tools",
            "payload",
        ]
    );
    assert_eq!(value["truncated"], json!(false));
    assert_eq!(value["truncation_reason"], Value::Null);
    assert_eq!(value["next"], Value::Null);

    // A bootstrapping answer is the same ten keys plus `error`, in that position.
    let bootstrapping_env: Envelope<Value> = bootstrapping(BootstrappingSpec::new(
        EnvelopeSpec::new(
            "the index is still being built",
            fresh(),
            Coverage::complete(0),
        ),
        500,
    ));
    let value = serde_json::to_value(&bootstrapping_env).expect("serialisable");
    let keys = object_keys(&value);
    assert_eq!(keys.len(), 11);
    assert_eq!(keys.last().map(String::as_str), Some("error"));
    assert_eq!(value["payload"], Value::Null);
    assert_eq!(value["status"], json!("bootstrapping"));
    assert_eq!(value["error"]["code"], json!("bootstrapping"));
    assert_eq!(value["error"]["message"], json!(DEFAULT_BOOTSTRAP_MESSAGE));
    assert_eq!(value["error"]["retry_after_ms"], json!(500));
    assert_eq!(
        object_keys(&value["error"]),
        ["code", "message", "retry_after_ms"]
    );

    // A failure carries the contract payload and no `retryable` key when it is not retryable.
    let failure: Envelope<Value> = fail(
        AppError::new(ErrorCode::NOT_FOUND, "no function named `parse`"),
        base_spec(),
    );
    let value = serde_json::to_value(&failure).expect("serialisable");
    assert_eq!(value["status"], json!("error"));
    assert_eq!(value["payload"], Value::Null);
    assert_eq!(value["error"]["code"], json!("not_found"));
    assert_eq!(object_keys(&value["error"]), ["code", "message"]);
    assert_eq!(value.as_object().expect("object").len(), 11);
}

#[test]
fn the_statuses_reasons_and_relations_serialise_to_their_typescript_spellings() {
    let statuses: Vec<String> = Status::ALL
        .iter()
        .map(|status| {
            serde_json::to_value(status)
                .expect("serialisable")
                .to_string()
        })
        .map(|text| text.trim_matches('"').to_owned())
        .collect();
    assert_eq!(
        statuses,
        ["ok", "ambiguous", "not_found", "error", "bootstrapping"]
    );

    let reasons: Vec<String> = TruncationReason::ALL
        .iter()
        .map(|reason| {
            serde_json::to_value(reason)
                .expect("serialisable")
                .to_string()
        })
        .map(|text| text.trim_matches('"').to_owned())
        .collect();
    assert_eq!(reasons, ["budget", "item_cap", "depth", "page"]);

    for (relation, spelling) in [
        (TotalRelation::Eq, "\"eq\""),
        (TotalRelation::Gte, "\"gte\""),
    ] {
        assert_eq!(
            serde_json::to_string(&relation).expect("serialisable"),
            spelling
        );
        assert_eq!(
            relation.as_str().trim_matches('"'),
            spelling.trim_matches('"')
        );
    }
}

#[test]
fn relation_for_is_eq_only_for_a_complete_fresh_scope_without_unresolved_edges() {
    assert_eq!(relation_for(&fresh(), &clean()), TotalRelation::Eq);

    let unresolved = Coverage {
        unresolved_edges: 1,
        ..clean()
    };
    assert_eq!(relation_for(&fresh(), &unresolved), TotalRelation::Gte);

    let incomplete = Coverage {
        complete: false,
        ..clean()
    };
    assert_eq!(relation_for(&fresh(), &incomplete), TotalRelation::Gte);

    let stale = ReadStatus {
        stale: true,
        ..fresh()
    };
    assert_eq!(relation_for(&stale, &clean()), TotalRelation::Gte);

    // A derived relation reaches the constructor's totals when the caller does not supply any.
    let spec = EnvelopeSpec::new("x", stale.clone(), clean());
    assert_eq!(ok(json!({}), spec).totals.relation(), TotalRelation::Gte);
    let spec = EnvelopeSpec::new("x", fresh(), unresolved);
    assert_eq!(ok(json!({}), spec).totals.relation(), TotalRelation::Gte);
    let spec = EnvelopeSpec::new("x", fresh(), clean());
    assert_eq!(ok(json!({}), spec).totals.relation(), TotalRelation::Eq);
}

#[test]
fn totals_serialise_with_the_relation_first_and_the_lists_in_insertion_order() {
    let totals = Totals::new(TotalRelation::Eq)
        .with_total("callers", Some(2))
        .with_total("callees", None)
        .with_unresolved_material(3)
        .with_candidates_shown(1);
    let value = serde_json::to_value(&totals).expect("serialisable");
    assert_eq!(
        object_keys(&value),
        [
            "total_relation",
            "callers_total",
            "callees_total",
            "unresolved_material",
            "candidates_shown",
        ]
    );
    // `null` means "this list does not apply to the scope", never zero.
    assert_eq!(value["callees_total"], Value::Null);
    assert_eq!(value["callers_total"], json!(2));

    // Rewriting the relation keeps its first position.
    let downgraded = totals.clone().with_relation(TotalRelation::Gte);
    assert_eq!(
        object_keys(&serde_json::to_value(&downgraded).expect("serialisable"))[0],
        "total_relation"
    );
    assert_eq!(downgraded.relation(), TotalRelation::Gte);
    assert_eq!(downgraded.get("callers_total"), Some(&json!(2)));
}

#[test]
fn the_constructors_keep_the_status_error_and_payload_in_step() {
    let ok_env = ok(json!({"a": 1}), base_spec());
    assert_eq!(ok_env.status, Status::Ok);
    assert_eq!(ok_env.payload, Some(json!({"a": 1})));
    assert!(ok_env.error.is_none());

    let ambiguous_env = ambiguous(json!({"candidates": []}), base_spec());
    assert_eq!(ambiguous_env.status, Status::Ambiguous);
    assert!(ambiguous_env.error.is_none());

    let missing = not_found(json!({"near_misses": []}), base_spec());
    assert_eq!(missing.status, Status::NotFound);
    assert!(missing.error.is_none());

    let failure: Envelope<Value> = fail(AppError::internal("boom"), base_spec());
    assert_eq!(failure.status, Status::Error);
    assert_eq!(failure.payload, None);
    assert_eq!(
        failure.error.as_ref().map(|error| error.code),
        Some(ErrorCode::INTERNAL)
    );

    let retryable: Envelope<Value> = fail(
        AppError::new(ErrorCode::INDEX_LOCKED, "indexing").with_retryable(true),
        base_spec(),
    );
    assert_eq!(
        serde_json::to_value(&retryable).expect("serialisable")["error"]["retryable"],
        json!(true)
    );

    // `with_payload` and `with_totals` copy the envelope instead of rewriting the shape.
    let replaced = with_payload(ok_env.clone(), Some(json!({"b": 2})));
    assert_eq!(replaced.payload, Some(json!({"b": 2})));
    assert_eq!(replaced.summary, ok_env.summary);
    let retotalled = with_totals(ok_env.clone(), Totals::new(TotalRelation::Gte));
    assert_eq!(retotalled.totals.relation(), TotalRelation::Gte);
    assert_eq!(retotalled.status, ok_env.status);
}

#[test]
fn a_fixed_envelope_measures_the_golden_byte_count() {
    // The byte count is the module's unit of account: one compact JSON serialisation of the ten
    // metadata keys and the payload. Pinning it catches a rename, a re-ordered key or a stray
    // optional field, all of which would change every budget decision downstream.
    let env = ok(
        json!({"base": "HEAD", "calls": items(2, "app")}),
        base_spec().with_totals(Totals::new(TotalRelation::Eq).with_total("calls", Some(2))),
    );
    assert_eq!(bytes(&env), GOLDEN_ENVELOPE_BYTES);
    assert_eq!(
        budget_floor_of(&env),
        GOLDEN_ENVELOPE_BYTES.div_ceil(BYTES_PER_TOKEN)
    );
    assert_eq!(
        serde_json::to_string(&env).expect("serialisable").len() as u64,
        GOLDEN_ENVELOPE_BYTES
    );
}

const GOLDEN_ENVELOPE_BYTES: u64 = 557;
