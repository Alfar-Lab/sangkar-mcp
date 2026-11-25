//! The report contract, executable.
//!
//! Mirrors `tests/telemetry-report.test.ts` for the two halves that need no HTML: the arithmetic a
//! reader trusts — a count, a percentile, a ratio, a trend point, a table order — and the skip rule
//! that keeps a torn line from becoming a record.
//!
//! Fixtures are JSONL lines rather than built records: the reader is half of what is under test, and
//! a line written by hand is also a line written by the other implementation (`bun_version` and
//! `x64` below are its own spellings, and the reader must not care).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use serde_json::{Map, Value, json};

use sangkar_telemetry::record::{
    CallSpec, Kind, RecordContext, RecordStatus, RepoSizeBucket, RootKind, SessionFacts,
    SessionMode, call_record_of, session_record_of,
};
use sangkar_telemetry::report::{
    ArgNameCount, Coverage, DurationStats, IndexRunRow, Relations, ReportData, TelemetryRecord,
    ToolStats, Totals, TruncationSummary, aggregate, is_since_literal, parse_since, read_records,
};

/// The fixture stamp the TypeScript suite uses.
const FIXED_TS: &str = "2026-11-18T10:00:00.000Z";
/// `"0f".repeat(16)`.
const INSTALL_ID: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
/// `"1a".repeat(16)`.
const SESSION_ID: &str = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a";

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// One call line as the recorder would have written it, with `patch`'s keys replaced.
fn call_line(patch: Value) -> String {
    line_of(
        json!({
            "ts": FIXED_TS,
            "v": 1,
            "kind": "call",
            "install_id": INSTALL_ID,
            "session_id": SESSION_ID,
            "seq": 1,
            "tool": "status",
            "status": "ok",
            "error_code": null,
            "duration_ms": 10,
            "arg_names_set": [],
            "budget_tokens": 1_000,
            "payload_tokens": 100,
            "budget_used_ratio": 0.1,
            "truncated": false,
            "truncation_reason": null,
            "total_relation": "gte",
            "coverage_complete": true,
            "coverage_indexed": 1,
            "coverage_skipped": 0,
            "coverage_failed": 0,
            "unresolved_edges": 0,
            "stale": false,
            "stale_reason": null,
            "generation_age_ms": null,
            "result_counts": {},
            "server_version": "0.6.0-beta.1",
            "bun_version": "1.4.4",
            "os": "linux",
            "arch": "x64",
            "graph_schema_version": 1,
        }),
        patch,
    )
}

/// One index-run line, with `patch`'s keys replaced.
fn index_run_line(patch: Value) -> String {
    line_of(
        json!({
            "ts": FIXED_TS,
            "v": 1,
            "kind": "index_run",
            "install_id": INSTALL_ID,
            "session_id": SESSION_ID,
            "mode": "full",
            "exit_code": 0,
            "duration_ms": 5_000,
            "was_forced": false,
            "generation_advanced": true,
            "files_seen": 10,
            "files_indexed": 9,
            "files_skipped": 1,
            "files_failed": 0,
            "nodes_by_kind": {},
            "edges_by_kind_tier": {},
            "unresolved_count": 0,
            "unresolved_ratio": 0,
            "languages": {},
            "db_size_bytes": 4_096,
            "files_changed_in_run": 4,
        }),
        patch,
    )
}

/// One session line, with `patch`'s keys replaced.
fn session_line(patch: Value) -> String {
    line_of(
        json!({
            "ts": FIXED_TS,
            "v": 1,
            "kind": "session",
            "install_id": INSTALL_ID,
            "session_id": SESSION_ID,
            "session_mode": "cli",
            "server_version": "0.6.0-beta.1",
            "bun_version": "1.4.4",
            "os": "linux",
            "arch": "x64",
            "is_first_run": true,
            "repo_size_bucket": "s",
            "languages": ["typescript"],
            "root_kind": "git",
        }),
        patch,
    )
}

fn line_of(base: Value, patch: Value) -> String {
    let Value::Object(mut fields) = base else {
        panic!("a fixture base is an object");
    };
    let Value::Object(patched) = patch else {
        panic!("a fixture patch is an object");
    };
    for (key, value) in patched {
        fields.insert(key, value);
    }
    Value::Object(fields).to_string()
}

/// One field dropped: a record missing a field the table requires is a line the reader must skip.
fn without_key(line: &str, key: &str) -> String {
    let parsed: Value = serde_json::from_str(line).expect("a fixture line is JSON");
    let Value::Object(mut fields) = parsed else {
        panic!("a fixture line is an object");
    };
    fields.remove(key);
    Value::Object(fields).to_string()
}

/// `name` holding `lines`, with the trailing newline an append always ends with.
fn write_jsonl(dir: &Path, name: &str, lines: &[String]) {
    fs::write(dir.join(name), format!("{}\n", lines.join("\n"))).expect("write the daily file");
}

fn tally(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
        .iter()
        .map(|(key, count)| ((*key).to_owned(), *count))
        .collect()
}

fn sorted_tools(data: &ReportData) -> Vec<&str> {
    data.tools.iter().map(|tool| tool.tool.as_str()).collect()
}

// -------------------------------------------------------------------------------------------
// Per-tool aggregation
// -------------------------------------------------------------------------------------------

#[test]
fn three_tools_produce_the_exact_counts_and_mixes() {
    let dir = TempDir::new("report-tools");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace" })),
            call_line(json!({ "tool": "trace" })),
            call_line(json!({
                "tool": "trace",
                "status": "error",
                "error_code": "invalid_args",
            })),
            call_line(json!({ "tool": "search" })),
            call_line(json!({ "tool": "search", "status": "not_found" })),
            call_line(json!({
                "tool": "status",
                "status": "absent",
                "error_code": "bootstrapping",
            })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.files, 1);
    assert_eq!(read.unparsed, 0);
    assert_eq!(
        read.data.totals,
        Totals {
            records: 6,
            calls: 6,
            index_runs: 0,
            sessions: 0,
            unparsed: 0,
            install_ids: 1,
            days: 1,
        }
    );

    // Sorted by tool name, so a stray row or a dropped one fails the case.
    assert_eq!(sorted_tools(&read.data), ["search", "status", "trace"]);
    let search = &read.data.tools[0];
    assert_eq!(search.calls, 2);
    assert_eq!(search.statuses, tally(&[("not_found", 1), ("ok", 1)]));
    assert_eq!(search.error_codes, tally(&[]));
    assert_eq!(search.mean_budget_used_ratio, Some(0.1));
    assert_eq!((search.eq, search.gte), (0, 2));

    let status = &read.data.tools[1];
    assert_eq!(status.calls, 1);
    assert_eq!(status.statuses, tally(&[("absent", 1)]));
    assert_eq!(status.error_codes, tally(&[("bootstrapping", 1)]));
    assert_eq!((status.eq, status.gte), (0, 1));

    let trace = &read.data.tools[2];
    assert_eq!(trace.calls, 3);
    assert_eq!(trace.statuses, tally(&[("error", 1), ("ok", 2)]));
    assert_eq!(trace.error_codes, tally(&[("invalid_args", 1)]));
    assert_eq!(
        trace.duration_ms,
        DurationStats {
            p50: 10.0,
            p95: 10.0,
            mean: 10.0,
            max: 10.0,
        }
    );
    assert_eq!(trace.truncated, 0);
    assert_eq!((trace.eq, trace.gte), (0, 3));

    assert_eq!(
        read.data.status_mix,
        tally(&[("absent", 1), ("error", 1), ("not_found", 1), ("ok", 3)])
    );
    assert_eq!(
        read.data.error_mix,
        tally(&[("bootstrapping", 1), ("invalid_args", 1)])
    );
}

// -------------------------------------------------------------------------------------------
// Latency
// -------------------------------------------------------------------------------------------

#[test]
fn duration_ms_one_to_a_hundred_gives_p50_50_p95_95_mean_50_5_and_max_100() {
    let dir = TempDir::new("report-percentile-hundred");
    let lines: Vec<String> = (1..=100)
        .map(|duration| call_line(json!({ "tool": "trace", "duration_ms": duration })))
        .collect();
    write_jsonl(dir.path(), "2026-11-18.jsonl", &lines);

    let read = read_records(dir.path(), None);
    assert_eq!(read.data.tools.len(), 1);
    assert_eq!(read.data.tools[0].calls, 100);
    assert_eq!(
        read.data.tools[0].duration_ms,
        DurationStats {
            p50: 50.0,
            p95: 95.0,
            mean: 50.5,
            max: 100.0,
        }
    );
}

#[test]
fn the_percentile_is_nearest_rank_and_never_interpolated() {
    let dir = TempDir::new("report-percentile-rank");
    // 1,2,3,4 in read order: an interpolating percentile would answer 2.5 and 3.85 here.
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace", "duration_ms": 4 })),
            call_line(json!({ "tool": "trace", "duration_ms": 2 })),
            call_line(json!({ "tool": "trace", "duration_ms": 1 })),
            call_line(json!({ "tool": "trace", "duration_ms": 3 })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(
        read.data.tools[0].duration_ms,
        DurationStats {
            p50: 2.0,
            p95: 4.0,
            mean: 2.5,
            max: 4.0,
        }
    );
}

#[test]
fn two_tools_report_their_own_p50_and_p95() {
    let dir = TempDir::new("report-two-tools");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace", "duration_ms": 10 })),
            call_line(json!({ "tool": "search", "duration_ms": 5 })),
            call_line(json!({ "tool": "trace", "duration_ms": 20 })),
            call_line(json!({ "tool": "search", "duration_ms": 100 })),
            call_line(json!({ "tool": "trace", "duration_ms": 30 })),
            call_line(json!({ "tool": "trace", "duration_ms": 40 })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(sorted_tools(&read.data), ["search", "trace"]);
    let search = &read.data.tools[0];
    assert_eq!(search.duration_ms.p50, 5.0);
    assert_eq!(search.duration_ms.p95, 100.0);
    assert_eq!(search.duration_ms.mean, 52.5);
    let trace = &read.data.tools[1];
    assert_eq!(trace.duration_ms.p50, 20.0);
    assert_eq!(trace.duration_ms.p95, 40.0);
    assert_eq!(trace.duration_ms.mean, 25.0);
}

// -------------------------------------------------------------------------------------------
// Truncation, relations and coverage
// -------------------------------------------------------------------------------------------

#[test]
fn two_truncated_of_four_calls_is_rate_half_and_the_eq_ratio_matches_the_fixture() {
    let dir = TempDir::new("report-truncation");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({
                "tool": "trace",
                "truncated": true,
                "truncation_reason": "budget",
                "unresolved_edges": 3,
                "stale": true,
            })),
            call_line(json!({
                "tool": "trace",
                "truncated": true,
                "truncation_reason": "item_cap",
                "unresolved_edges": 1,
            })),
            call_line(json!({ "tool": "trace", "total_relation": "eq" })),
            call_line(json!({ "tool": "search" })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(
        read.data.truncation,
        TruncationSummary {
            count: 2,
            rate: 0.5,
            reasons: tally(&[("budget", 1), ("item_cap", 1)]),
        }
    );
    assert_eq!(
        read.data.relations,
        Relations {
            eq: 1,
            gte: 3,
            eq_ratio: 0.25,
        }
    );
    // `unresolved_edges` sums over every call, not over the ones that carried a graph.
    assert_eq!(
        read.data.coverage,
        Coverage {
            stale_calls: 1,
            mean_unresolved_edges: 1.0,
        }
    );

    let search: &ToolStats = &read.data.tools[0];
    assert_eq!((search.truncated, search.eq, search.gte), (0, 0, 1));
    let trace: &ToolStats = &read.data.tools[1];
    assert_eq!((trace.truncated, trace.eq, trace.gte), (2, 1, 2));
}

// -------------------------------------------------------------------------------------------
// The unresolved trend and the run table
// -------------------------------------------------------------------------------------------

#[test]
fn two_utc_dates_give_two_trend_points_in_chronological_order() {
    let dir = TempDir::new("report-trend");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            index_run_line(json!({
                "ts": "2026-11-18T12:00:00.000Z",
                "unresolved_ratio": 0.5,
            })),
            index_run_line(json!({
                "ts": "2026-11-17T09:00:00.000Z",
                "unresolved_ratio": 0.25,
            })),
            index_run_line(json!({
                "ts": "2026-11-18T18:00:00.000Z",
                "unresolved_ratio": 0.75,
            })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.data.unresolved_trend.len(), 2);
    assert_eq!(read.data.unresolved_trend[0].date, "2026-11-17");
    assert_eq!(read.data.unresolved_trend[0].ratio, 0.25);
    assert_eq!(read.data.unresolved_trend[0].runs, 1);
    assert_eq!(read.data.unresolved_trend[1].date, "2026-11-18");
    assert_eq!(read.data.unresolved_trend[1].ratio, 0.625);
    assert_eq!(read.data.unresolved_trend[1].runs, 2);
    assert_eq!(read.data.totals.index_runs, 3);
    assert_eq!(read.data.totals.days, 2);
}

#[test]
fn the_run_table_is_chronological_and_carries_the_file_counters() {
    let dir = TempDir::new("report-runs");
    let earlier = index_run_line(json!({
        "ts": "2026-11-17T08:00:00.000Z",
        "mode": "full",
        "exit_code": 0,
        "duration_ms": 5_000,
        "files_seen": 12,
        "files_indexed": 12,
        "files_skipped": 0,
        "files_failed": 0,
        "files_changed_in_run": 12,
        "unresolved_ratio": 0.2,
        "db_size_bytes": 4_096,
        "generation_advanced": true,
    }));
    let later = index_run_line(json!({
        "ts": "2026-11-19T08:00:00.000Z",
        "mode": "fast",
        "exit_code": 3,
        "duration_ms": 1_200,
        "files_seen": 10,
        "files_indexed": 7,
        "files_skipped": 1,
        "files_failed": 2,
        "files_changed_in_run": 5,
        "unresolved_ratio": 0.4,
        "db_size_bytes": 8_192,
        "generation_advanced": false,
    }));
    // Read order is the reverse of recorded order: only `ts` may decide the table.
    write_jsonl(dir.path(), "2026-11-19.jsonl", &[later, earlier]);

    let read = read_records(dir.path(), None);
    assert_eq!(
        read.data.index_runs,
        vec![
            IndexRunRow {
                ts: "2026-11-17T08:00:00.000Z".to_owned(),
                mode: "full".to_owned(),
                exit_code: 0,
                duration_ms: 5_000.0,
                files_seen: 12,
                files_indexed: 12,
                files_skipped: 0,
                files_failed: 0,
                files_changed_in_run: 12,
                unresolved_ratio: 0.2,
                db_size_bytes: 4_096,
                generation_advanced: true,
            },
            IndexRunRow {
                ts: "2026-11-19T08:00:00.000Z".to_owned(),
                mode: "fast".to_owned(),
                exit_code: 3,
                duration_ms: 1_200.0,
                files_seen: 10,
                files_indexed: 7,
                files_skipped: 1,
                files_failed: 2,
                files_changed_in_run: 5,
                unresolved_ratio: 0.4,
                db_size_bytes: 8_192,
                generation_advanced: false,
            },
        ]
    );
}

// -------------------------------------------------------------------------------------------
// The skip rule
// -------------------------------------------------------------------------------------------

#[test]
fn three_good_and_one_malformed_line_reports_three_records_and_one_unparsed() {
    let dir = TempDir::new("report-done-when");
    let torn = "{\"ts\":\"2026-11-18T11:00:00.000Z\",\"v\":1,\"kind\":\"call\",\"tool\":\"tor";
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "seq": 1 })),
            call_line(json!({ "seq": 2 })),
            call_line(json!({ "seq": 3 })),
            torn.to_owned(),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.files, 1);
    assert_eq!(read.records.len(), 3);
    assert_eq!(read.unparsed, 1);
    assert_eq!(read.data.totals.records, 3);
    assert_eq!(read.data.totals.calls, 3);
    assert_eq!(read.data.totals.unparsed, 1);
}

#[test]
fn torn_non_object_and_mistyped_lines_cost_one_unparsed_count_each_and_nothing_else() {
    let dir = TempDir::new("report-skip");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace", "seq": 1 })),
            call_line(json!({ "tool": "trace", "seq": 2 })),
            "{\"ts\":\"2026-11-18T11:00:00.000Z\",\"v\":1,\"kind\":\"call\",\"tool\":\"tor"
                .to_owned(),
            "42".to_owned(),
        ],
    );

    let first = read_records(dir.path(), None);
    assert_eq!(first.files, 1);
    assert_eq!(first.records.len(), 2);
    assert_eq!(first.unparsed, 2);
    assert_eq!(first.records[0].kind(), Kind::Call);

    // The second file adds three unparsed lines and no record.
    write_jsonl(
        dir.path(),
        "2026-11-19.jsonl",
        &[
            call_line(json!({ "tool": "ghost-tool", "seq": 3, "kind": "mystery" })),
            call_line(json!({ "tool": "future-tool", "seq": 4, "v": 2 })),
            without_key(
                &call_line(json!({ "tool": "torn-tool", "seq": 5 })),
                "arg_names_set",
            ),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.files, 2);
    assert_eq!(read.records.len(), 2);
    assert_eq!(read.unparsed, 5);
    assert_eq!(read.data.totals.records, 2);
    assert_eq!(read.data.totals.calls, 2);
    assert_eq!(read.data.totals.unparsed, 5);
    assert_eq!(read.data.totals.install_ids, 1);
    assert_eq!(read.data.totals.days, 1);
    assert_eq!(sorted_tools(&read.data), ["trace"]);
    assert_eq!(read.data.status_mix, tally(&[("ok", 2)]));
    assert_eq!(read.data.truncation.count, 0);
    assert_eq!(read.data.truncation.rate, 0.0);
    assert_eq!(read.data.truncation.reasons, tally(&[]));
    assert_eq!(
        read.data.relations,
        Relations {
            eq: 0,
            gte: 2,
            eq_ratio: 0.0,
        }
    );
    assert_eq!(read.data.arg_names, Vec::new());
}

#[test]
fn a_missing_directory_is_an_empty_window_and_never_an_error() {
    let dir = TempDir::new("report-empty");
    let read = read_records(&dir.path().join("rotated-away"), None);

    assert_eq!(read.files, 0);
    assert!(read.records.is_empty());
    assert_eq!(read.unparsed, 0);
    assert_eq!(read.data.totals.records, 0);
    assert!(read.data.tools.is_empty());
    assert!(read.data.status_mix.is_empty());
    assert_eq!(read.data.window.first_ts, None);
    assert_eq!(read.data.window.last_ts, None);
    assert_eq!(read.data, aggregate(&[]));
}

// -------------------------------------------------------------------------------------------
// --since
// -------------------------------------------------------------------------------------------

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-18T13:45:23.000Z")
        .expect("the fixture clock is RFC 3339")
        .to_utc()
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[test]
fn only_a_calendar_day_or_nd_with_n_at_least_one_is_a_literal() {
    assert!(is_since_literal("2026-11-01"));
    assert!(is_since_literal("7d"));

    for rejected in [
        "2026-13-01",
        "2026-02-30",
        "2026-11-1",
        "0d",
        "7",
        "7days",
        "1w",
        "24h",
        "",
    ] {
        assert!(
            !is_since_literal(rejected),
            "{rejected} is not a since literal"
        );
        assert!(
            parse_since(rejected, now()).is_err(),
            "{rejected} must be refused"
        );
    }
}

#[test]
fn seven_d_is_now_minus_seven_days_floored_to_the_utc_day() {
    let since = parse_since("7d", now()).expect("7d is a literal");

    assert_eq!(since.raw(), "7d");
    assert_eq!(iso(since.at()), "2026-11-11T00:00:00.000Z");
    // Seven days plus the part of the day the floor removed: the shift happens before the floor.
    let elapsed = now().signed_duration_since(since.at());
    assert_eq!(
        elapsed,
        TimeDelta::days(7) + TimeDelta::milliseconds((13 * 3_600 + 45 * 60 + 23) * 1_000)
    );

    let day = parse_since("2026-11-01", now()).expect("a calendar day is a literal");
    assert_eq!(iso(day.at()), "2026-11-01T00:00:00.000Z");
    assert_eq!(day.raw(), "2026-11-01");
}

#[test]
fn a_refused_since_value_says_which_forms_are_accepted() {
    let refusal = parse_since("24h", now()).expect_err("24h is refused");
    assert_eq!(refusal.raw(), "24h");
    assert_eq!(
        refusal.to_string(),
        "--since must be YYYY-MM-DD or <n>d, not \"24h\""
    );
}

#[test]
fn since_filters_by_the_records_own_ts() {
    let dir = TempDir::new("report-since");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "ts": "2026-11-10T23:59:59.000Z" })),
            call_line(json!({ "ts": "2026-11-11T00:00:00.000Z" })),
            call_line(json!({ "ts": "2026-11-18T12:00:00.000Z" })),
        ],
    );

    let unfiltered = read_records(dir.path(), None);
    assert_eq!(unfiltered.records.len(), 3);

    let since = parse_since("7d", now()).expect("7d is a literal");
    let read = read_records(dir.path(), Some(since));
    assert_eq!(read.files, 1);
    assert_eq!(read.unparsed, 0);
    assert_eq!(
        read.records
            .iter()
            .map(TelemetryRecord::ts)
            .collect::<Vec<_>>(),
        ["2026-11-11T00:00:00.000Z", "2026-11-18T12:00:00.000Z"]
    );
    assert_eq!(read.data.totals.records, 2);
    // The page labels the window with what the caller typed, not with the date it floored to.
    assert_eq!(read.data.window.since.as_deref(), Some("7d"));
    assert_eq!(
        read.data.window.first_ts.as_deref(),
        Some("2026-11-11T00:00:00.000Z")
    );
}

// -------------------------------------------------------------------------------------------
// Argument names, sessions and the window
// -------------------------------------------------------------------------------------------

#[test]
fn argument_names_are_ordered_by_count_then_by_name() {
    let dir = TempDir::new("report-arg-names");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace", "arg_names_set": ["depth", "query"] })),
            call_line(json!({ "tool": "trace", "arg_names_set": ["query"] })),
            call_line(json!({ "tool": "trace", "arg_names_set": ["depth"] })),
            call_line(json!({ "tool": "search", "arg_names_set": ["alpha"] })),
        ],
    );

    let read = read_records(dir.path(), None);
    // Two calls each for `depth` and `query`, so the tie is broken by name, not by read order.
    assert_eq!(
        read.data.arg_names,
        vec![
            ArgNameCount {
                name: "depth".to_owned(),
                calls: 2,
            },
            ArgNameCount {
                name: "query".to_owned(),
                calls: 2,
            },
            ArgNameCount {
                name: "alpha".to_owned(),
                calls: 1,
            },
        ]
    );
}

#[test]
fn sessions_are_broken_down_three_ways_and_the_window_spans_every_kind() {
    let dir = TempDir::new("report-sessions");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            session_line(json!({ "ts": "2026-11-17T08:00:00.000Z" })),
            session_line(json!({
                "ts": "2026-11-19T08:00:00.000Z",
                "session_mode": "mcp",
                "root_kind": "plain",
                "repo_size_bucket": "xs",
            })),
            call_line(json!({ "tool": "trace" })),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.data.totals.records, 3);
    assert_eq!(read.data.totals.sessions, 2);
    assert_eq!(read.data.totals.calls, 1);
    assert_eq!(read.data.totals.days, 3);
    assert_eq!(read.data.sessions.by_mode, tally(&[("cli", 1), ("mcp", 1)]));
    assert_eq!(
        read.data.sessions.by_root_kind,
        tally(&[("git", 1), ("plain", 1)])
    );
    assert_eq!(
        read.data.sessions.by_size_bucket,
        tally(&[("s", 1), ("xs", 1)])
    );
    assert_eq!(
        read.data.window.first_ts.as_deref(),
        Some("2026-11-17T08:00:00.000Z")
    );
    assert_eq!(
        read.data.window.last_ts.as_deref(),
        Some("2026-11-19T08:00:00.000Z")
    );
    assert_eq!(read.data.window.since, None);
}

#[test]
fn a_line_the_recorder_writes_is_a_record_the_report_reads() {
    // The fixtures above are the other implementation's spelling. This one is the Rust writer's own, so
    // a reader and a recorder that drifted apart fail here instead of in a report nobody can explain.
    let context = RecordContext {
        ts: FIXED_TS.to_owned(),
        install_id: INSTALL_ID.to_owned(),
        session_id: SESSION_ID.to_owned(),
        seq: 1,
        server_version: "0.6.0-beta.1".to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        graph_schema_version: 1,
    };
    let call = call_record_of(
        &context,
        &CallSpec {
            tool: "trace".to_owned(),
            args: Map::new(),
            supplied_names: vec!["query".to_owned()],
            envelope: None,
            status: RecordStatus::Ok,
            error_code: None,
            duration_ms: 7,
        },
    );
    let session = session_record_of(
        &context,
        &SessionFacts {
            session_mode: SessionMode::Mcp,
            is_first_run: true,
            repo_size_bucket: RepoSizeBucket::S,
            languages: vec!["rust".to_owned()],
            root_kind: RootKind::Git,
        },
    );

    let dir = TempDir::new("report-recorder");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            serde_json::to_string(&call).expect("a call record serialises"),
            serde_json::to_string(&session).expect("a session record serialises"),
        ],
    );

    let read = read_records(dir.path(), None);
    assert_eq!(read.unparsed, 0);
    assert_eq!(read.records.len(), 2);
    assert_eq!(read.records[0].kind(), Kind::Call);
    assert_eq!(read.records[1].kind(), Kind::Session);
    assert_eq!(sorted_tools(&read.data), ["trace"]);
    assert_eq!(read.data.tools[0].duration_ms.p50, 7.0);
    assert_eq!(
        read.data.arg_names,
        vec![ArgNameCount {
            name: "query".to_owned(),
            calls: 1,
        }]
    );
    assert_eq!(read.data.sessions.by_mode, tally(&[("mcp", 1)]));
    assert_eq!(read.data.sessions.by_root_kind, tally(&[("git", 1)]));
    // No envelope: there was no budget to measure against, so the ratio is absent rather than zero.
    assert_eq!(read.data.tools[0].mean_budget_used_ratio, None);
}

#[test]
fn two_folds_of_one_window_agree_with_the_one_argument_aggregate() {
    let dir = TempDir::new("report-determinism");
    write_jsonl(
        dir.path(),
        "2026-11-18.jsonl",
        &[
            call_line(json!({ "tool": "trace", "duration_ms": 12 })),
            call_line(json!({ "tool": "search", "duration_ms": 30, "status": "not_found" })),
            index_run_line(json!({ "unresolved_ratio": 0.25 })),
            session_line(json!({})),
        ],
    );

    let first = read_records(dir.path(), None);
    let second = read_records(dir.path(), None);
    assert_eq!(first.data, second.data);
    // A clean window with no `since` carries no fact the records do not: the two entry points agree.
    assert_eq!(aggregate(&first.records), first.data);
}

// -------------------------------------------------------------------------------------------
// A scratch directory, dropped at the end of the test that made it
// -------------------------------------------------------------------------------------------

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-telemetry-{label}-{}-{nanos}-{}",
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
