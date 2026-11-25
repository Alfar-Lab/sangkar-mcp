//! The telemetry record, identity and sink contract, executable.
//!
//! Mirrors `tests/telemetry.test.ts`: the exact bytes of one record of each kind (which pin the field
//! order, the always-present `null`s and the JavaScript number spelling), the one value this crate
//! reads for itself under its unchanged `bun_version` name, the ratio and rounding rules, the install
//! id's day rotation and its create-exclusive race, and a daily file that is only ever appended to.
//!
//! No tool handler and no database: a record is built from a hand-written context and spec, and the
//! identity and sink from a `TempDir`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use sangkar_core::envelope::{Coverage, Envelope, EnvelopeSpec, ReadStatus, Status, ok};
use sangkar_core::runtime::runtime_version;
use sangkar_telemetry::identity::{
    InstallIdentity, new_session_id, resolve_install_id, utc_date_of,
};
use sangkar_telemetry::record::{
    CallSpec, Counts, IndexMode, IndexRunSpec, Kind, RECORD_VERSION, RecordContext, RecordStatus,
    RecordedStaleReason, RepoSizeBucket, RootKind, SessionFacts, SessionMode, call_record_of,
    index_run_record_of, repo_size_bucket_of, root_kind_of, session_record_of,
};
use sangkar_telemetry::sink::{TelemetrySink, jsonl_sink, null_sink};

const FIXED_TS: &str = "2026-11-18T10:00:00.000Z";
const FIXED_DATE: &str = "2026-11-18";
const INDEXED_AT: &str = "2026-11-18T09:00:00.000Z";
const SERVER_VERSION: &str = "0.6.0-beta.1";

/// `"0f".repeat(16)`, the fixture id the TypeScript suite uses.
const INSTALL_ID: &str = "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f";
/// `"1a".repeat(16)`.
const SESSION_ID: &str = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a";

/// The field order of plan.approach.md §3, in serialisation order; `v` sits second so a reader
/// checks it first.
const CALL_FIELDS: [&str; 31] = [
    "ts",
    "v",
    "kind",
    "install_id",
    "session_id",
    "seq",
    "tool",
    "status",
    "error_code",
    "duration_ms",
    "arg_names_set",
    "budget_tokens",
    "payload_tokens",
    "budget_used_ratio",
    "truncated",
    "truncation_reason",
    "total_relation",
    "coverage_complete",
    "coverage_indexed",
    "coverage_skipped",
    "coverage_failed",
    "unresolved_edges",
    "stale",
    "stale_reason",
    "generation_age_ms",
    "result_counts",
    "server_version",
    "bun_version",
    "os",
    "arch",
    "graph_schema_version",
];

const INDEX_RUN_FIELDS: [&str; 21] = [
    "ts",
    "v",
    "kind",
    "install_id",
    "session_id",
    "mode",
    "exit_code",
    "duration_ms",
    "was_forced",
    "generation_advanced",
    "files_seen",
    "files_indexed",
    "files_skipped",
    "files_failed",
    "nodes_by_kind",
    "edges_by_kind_tier",
    "unresolved_count",
    "unresolved_ratio",
    "languages",
    "db_size_bytes",
    "files_changed_in_run",
];

const SESSION_FIELDS: [&str; 14] = [
    "ts",
    "v",
    "kind",
    "install_id",
    "session_id",
    "session_mode",
    "server_version",
    "bun_version",
    "os",
    "arch",
    "is_first_run",
    "repo_size_bucket",
    "languages",
    "root_kind",
];

/// A temporary directory that removes itself, so the suite leaves no telemetry directory behind.
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

fn context() -> RecordContext {
    RecordContext {
        ts: FIXED_TS.to_owned(),
        install_id: INSTALL_ID.to_owned(),
        session_id: SESSION_ID.to_owned(),
        seq: 1,
        server_version: SERVER_VERSION.to_owned(),
        os: "linux".to_owned(),
        arch: "x86_64".to_owned(),
        graph_schema_version: 1,
    }
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn clean_coverage() -> Coverage {
    Coverage {
        complete: true,
        indexed: 12,
        skipped: 1,
        failed: 0,
        unresolved_edges: 0,
        not_indexed: Vec::new(),
        tests: None,
    }
}

fn read_status() -> ReadStatus {
    ReadStatus::new("g-1", INDEXED_AT)
}

fn read_status_with(reason: Option<&str>) -> ReadStatus {
    ReadStatus {
        stale: true,
        stale_reason: reason.map(str::to_owned),
        ..read_status()
    }
}

fn envelope(payload: Value) -> Envelope<Value> {
    ok(
        payload,
        EnvelopeSpec::new(
            "trace handleRequest: 2 callers, 1 callee (2 shown)",
            read_status(),
            clean_coverage(),
        ),
    )
}

fn envelope_with(payload: Value, status: ReadStatus) -> Envelope<Value> {
    ok(
        payload,
        EnvelopeSpec::new("trace handleRequest: 2 callers", status, clean_coverage()),
    )
}

fn call_spec() -> CallSpec {
    CallSpec {
        tool: "trace".to_owned(),
        args: args(&[("query", json!("handleRequest")), ("budget", json!(3200))]),
        supplied_names: vec!["query".to_owned()],
        envelope: None,
        status: RecordStatus::Ok,
        error_code: None,
        duration_ms: 12,
    }
}

fn index_run_spec() -> IndexRunSpec {
    IndexRunSpec {
        mode: IndexMode::Full,
        exit_code: 2,
        duration_ms: 5_432,
        was_forced: false,
        files_seen: 10,
        files_indexed: 9,
        files_skipped: 1,
        files_failed: 0,
        nodes_by_kind: Counts::from_iter([("function", 20), ("class", 2)]),
        edges_by_kind_tier: Counts::from_iter([("calls/extracted", 30)]),
        unresolved_count: 3,
        languages: Counts::from_iter([("typescript", 10), ("python", 4)]),
        db_size_bytes: 4_096,
        files_changed_in_run: 4,
    }
}

fn session_facts() -> SessionFacts {
    SessionFacts {
        session_mode: SessionMode::Cli,
        is_first_run: true,
        repo_size_bucket: RepoSizeBucket::S,
        languages: vec!["typescript".to_owned()],
        root_kind: RootKind::Git,
    }
}

fn line_of<T: serde::Serialize>(record: &T) -> String {
    serde_json::to_string(record).expect("the record serialises")
}

fn wire<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("the value serialises")
}

fn keys_of(line: &str) -> Vec<String> {
    let parsed: Value = serde_json::from_str(line).expect("the line parses");
    let object = parsed.as_object().expect("the line is an object");
    object.keys().cloned().collect()
}

/// The serialised text of one field, so an assertion is about a value and not about a substring of a
/// longer one. The scan steps over nested arrays, objects and strings, so a comma inside an array or a
/// string does not end the field.
fn field_text<'a>(line: &'a str, name: &str) -> &'a str {
    let needle = format!("\"{name}\":");
    let start = line
        .find(&needle)
        .unwrap_or_else(|| panic!("{name} is not in {line}"))
        + needle.len();
    let rest = &line[start..];

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in rest.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                if depth == 0 {
                    return &rest[..offset];
                }
                depth -= 1;
            }
            b',' if depth == 0 => return &rest[..offset],
            _ => {}
        }
    }
    rest
}

/// `runtime_version()` is the string the compiled binary reports; the record must carry it verbatim.
fn runtime_text() -> String {
    serde_json::to_string(&runtime_version())
        .expect("the runtime version serialises")
        .trim_matches('"')
        .to_owned()
}

fn is_hex_32(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("the directory lists")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_call_record_is_the_plan_bytes_in_the_plan_order() {
    let record = call_record_of(&context(), &call_spec());
    let line = line_of(&record);

    assert_eq!(keys_of(&line), CALL_FIELDS);
    assert_eq!(record.v, RECORD_VERSION);
    assert_eq!(record.kind, Kind::Call);

    // `JSON.stringify(record)` for the same facts under Bun 1.4.4: every field present, the two
    // ratios' nulls and the zero ratio's integral spelling included.
    let template = concat!(
        r#"{"ts":"2026-11-18T10:00:00.000Z","v":1,"kind":"call","install_id":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","#,
        r#""session_id":"1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","seq":1,"tool":"trace","status":"ok","#,
        r#""error_code":null,"duration_ms":12,"arg_names_set":["query"],"budget_tokens":3200,"#,
        r#""payload_tokens":0,"budget_used_ratio":0,"truncated":false,"truncation_reason":null,"#,
        r#""total_relation":"gte","coverage_complete":false,"coverage_indexed":0,"coverage_skipped":0,"#,
        r#""coverage_failed":0,"unresolved_edges":0,"stale":true,"stale_reason":null,"#,
        r#""generation_age_ms":null,"result_counts":{},"server_version":"0.6.0-beta.1","#,
        r#""bun_version":"RUNTIME","os":"linux","arch":"x86_64","graph_schema_version":1}"#
    );
    assert_eq!(line, template.replace("RUNTIME", &runtime_text()));
}

#[test]
fn an_index_run_record_is_the_plan_bytes_in_the_plan_order() {
    let line = line_of(&index_run_record_of(&context(), &index_run_spec()));

    assert_eq!(keys_of(&line), INDEX_RUN_FIELDS);
    // The four server-identity fields belong to `call`/`session`, not to a run.
    assert!(!line.contains("bun_version"));
    assert!(!line.contains("graph_schema_version"));

    let template = concat!(
        r#"{"ts":"2026-11-18T10:00:00.000Z","v":1,"kind":"index_run","install_id":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","#,
        r#""session_id":"1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","mode":"full","exit_code":2,"duration_ms":5432,"#,
        r#""was_forced":false,"generation_advanced":false,"files_seen":10,"files_indexed":9,"#,
        r#""files_skipped":1,"files_failed":0,"nodes_by_kind":{"function":20,"class":2},"#,
        r#""edges_by_kind_tier":{"calls/extracted":30},"unresolved_count":3,"unresolved_ratio":0.0909,"#,
        r#""languages":{"typescript":10,"python":4},"db_size_bytes":4096,"files_changed_in_run":4}"#
    );
    assert_eq!(line, template);
}

#[test]
fn a_session_record_is_the_plan_bytes_in_the_plan_order() {
    let line = line_of(&session_record_of(&context(), &session_facts()));

    assert_eq!(keys_of(&line), SESSION_FIELDS);

    let template = concat!(
        r#"{"ts":"2026-11-18T10:00:00.000Z","v":1,"kind":"session","install_id":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","#,
        r#""session_id":"1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a","session_mode":"cli","server_version":"0.6.0-beta.1","#,
        r#""bun_version":"RUNTIME","os":"linux","arch":"x86_64","is_first_run":true,"#,
        r#""repo_size_bucket":"s","languages":["typescript"],"root_kind":"git"}"#
    );
    assert_eq!(line, template.replace("RUNTIME", &runtime_text()));
}

#[test]
fn the_runtime_field_keeps_its_v1_name_and_carries_this_binarys_version() {
    let call = line_of(&call_record_of(&context(), &call_spec()));
    let session = line_of(&session_record_of(&context(), &session_facts()));

    // The name is frozen with the record shape, so it is asserted as bytes and not only as a field.
    assert!(call.contains("\"bun_version\":\""));
    // The field text is JSON, so it carries the quotes the value's own serialisation wrote.
    let runtime_json = format!("\"{}\"", runtime_text());
    assert_eq!(field_text(&call, "bun_version"), runtime_json);
    assert_eq!(field_text(&session, "bun_version"), runtime_json);

    // Nothing renamed it after the interpreter retired: a `rust_version` or similar would be a v1
    // shape change, and no reader in the report asks for this field.
    for key in keys_of(&call) {
        assert!(
            !key.contains("rust"),
            "the runtime field must keep its v1 name, found {key}"
        );
    }
    assert!(!runtime_text().is_empty());
}

#[test]
fn payload_tokens_is_the_envelope_json_in_quarters_rounded_up() {
    let env = envelope(json!({"calls": [{"id": 1}, {"id": 2}]}));
    let bytes = u64::try_from(line_of(&env).len()).expect("the envelope has a length");
    assert_ne!(
        bytes % 4,
        0,
        "the fixture must not be a multiple of four bytes, or the rounding is untested"
    );

    let record = call_record_of(
        &context(),
        &CallSpec {
            envelope: Some(env),
            ..call_spec()
        },
    );

    assert_eq!(record.payload_tokens, bytes.div_ceil(4));
    // A measured envelope replaces the no-envelope defaults with what it carries.
    assert_eq!(record.status, RecordStatus::Ok);
    assert!(record.coverage_complete);
    assert_eq!(record.coverage_indexed, 12);
    assert_eq!(record.coverage_skipped, 1);
    assert!(!record.stale);
    assert_eq!(field_text(&line_of(&record), "total_relation"), "\"eq\"");
    assert_eq!(
        field_text(&line_of(&record), "generation_age_ms"),
        "3600000"
    );
    assert_eq!(
        field_text(&line_of(&record), "result_counts"),
        r#"{"calls":2}"#
    );
}

#[test]
fn the_budget_ratio_is_four_decimals_and_null_at_a_zero_budget() {
    let env = envelope(json!({"calls": []}));
    let tokens = call_record_of(
        &context(),
        &CallSpec {
            envelope: Some(env),
            ..call_spec()
        },
    )
    .payload_tokens;
    assert!(tokens > 0, "the fixture must measure something");

    let with_budget = |budget: Option<u64>| {
        let args = match budget {
            Some(budget) => args(&[("query", json!("handleRequest")), ("budget", json!(budget))]),
            None => args(&[("query", json!("handleRequest"))]),
        };
        let record = call_record_of(
            &context(),
            &CallSpec {
                args,
                envelope: Some(envelope(json!({"calls": []}))),
                ..call_spec()
            },
        );
        line_of(&record)
    };

    // `budget_tokens === 0` is "there is nothing to divide by", never a ratio.
    let no_budget = with_budget(None);
    assert_eq!(field_text(&no_budget, "budget_tokens"), "0");
    assert_eq!(field_text(&no_budget, "budget_used_ratio"), "null");

    // `round4` at its three interesting shapes: a repeating decimal, a quarter, and a whole number
    // a JavaScript number prints without a decimal point.
    assert_eq!(
        field_text(&with_budget(Some(tokens * 3)), "budget_used_ratio"),
        "0.3333"
    );
    assert_eq!(
        field_text(&with_budget(Some(tokens * 7)), "budget_used_ratio"),
        "0.1429"
    );
    assert_eq!(
        field_text(&with_budget(Some(tokens * 4)), "budget_used_ratio"),
        "0.25"
    );
    assert_eq!(
        field_text(&with_budget(Some(tokens)), "budget_used_ratio"),
        "1"
    );
}

#[test]
fn the_unresolved_ratio_is_the_count_over_the_count_plus_every_edge_tier() {
    let ratio_of = |unresolved_count: u64, edges: &[(&str, u64)]| {
        let record = index_run_record_of(
            &context(),
            &IndexRunSpec {
                unresolved_count,
                edges_by_kind_tier: Counts::from_iter(edges.iter().copied()),
                ..index_run_spec()
            },
        );
        field_text(&line_of(&record), "unresolved_ratio").to_owned()
    };

    assert_eq!(ratio_of(1, &[("calls/extracted", 2)]), "0.3333");
    assert_eq!(ratio_of(3, &[("calls/extracted", 9)]), "0.25");
    // A denominator of zero is `0`, spelled as the integer it is.
    assert_eq!(ratio_of(0, &[]), "0");
}

#[test]
fn generation_advanced_is_true_exactly_when_the_run_exited_zero() {
    let advanced = |exit_code: i32| {
        index_run_record_of(
            &context(),
            &IndexRunSpec {
                exit_code,
                ..index_run_spec()
            },
        )
        .generation_advanced
    };
    assert!(advanced(0));
    assert!(!advanced(1));
    assert!(!advanced(2));
    assert!(!advanced(-1));
}

#[test]
fn generation_age_ms_is_the_distance_to_the_envelopes_indexed_at() {
    let age_of = |status: Option<ReadStatus>| {
        let envelope = status.map(|status| envelope_with(json!({}), status));
        call_record_of(
            &context(),
            &CallSpec {
                envelope,
                ..call_spec()
            },
        )
        .generation_age_ms
    };

    assert_eq!(age_of(None), None);
    assert_eq!(age_of(Some(read_status())), Some(3_600_000));
    // The same instant is a zero age, not "unknown".
    assert_eq!(age_of(Some(ReadStatus::new("g-1", FIXED_TS))), Some(0));
    // A value that is not the timestamp this workspace writes is unknown rather than guessed at.
    assert_eq!(
        age_of(Some(ReadStatus::new("g-1", "yesterday, roughly"))),
        None
    );
}

#[test]
fn stale_reason_is_a_closed_enum_so_prose_becomes_other() {
    let reason_of = |reason: Option<&str>| {
        let record = call_record_of(
            &context(),
            &CallSpec {
                envelope: Some(envelope_with(json!({}), read_status_with(reason))),
                ..call_spec()
            },
        );
        (record.stale, line_of(&record))
    };

    let (stale, line) = reason_of(Some("\"src/core.ts\" was last written by generation 3"));
    assert!(stale);
    assert_eq!(field_text(&line, "stale_reason"), "\"other\"");
    // The prose is the envelope's, and it carries a repository path: it must not reach the line.
    assert!(!line.contains("core.ts"));
    assert!(!line.contains("generation 3"));

    for reason in [
        "none",
        "files_changed",
        "root_mismatch",
        "base_commit_moved",
        "watcher_stalled",
    ] {
        let (_, line) = reason_of(Some(reason));
        assert_eq!(field_text(&line, "stale_reason"), format!("\"{reason}\""));
    }

    // No envelope at all: the answer is "not known", never a copy of anything.
    let (stale, line) = (
        call_record_of(&context(), &call_spec()).stale,
        line_of(&call_record_of(&context(), &call_spec())),
    );
    assert!(stale);
    assert_eq!(field_text(&line, "stale_reason"), "null");
    assert_eq!(call_record_of(&context(), &call_spec()).stale_reason, None);
}

#[test]
fn no_argument_value_or_payload_value_reaches_a_call_record() {
    let secret_name = "handleRequest";
    let secret_path = "/home/x/secret-repo/src/core.ts";
    let env = envelope(json!({
        "query": secret_name,
        "node": {"path": secret_path, "qn": "src/core.ts::handleRequest"},
        "calls": [{"path": secret_path, "name": secret_name}],
    }));
    let record = call_record_of(
        &context(),
        &CallSpec {
            args: args(&[
                ("query", json!(secret_name)),
                ("file", json!(secret_path)),
                ("budget", json!(3200)),
            ]),
            supplied_names: vec!["query".to_owned(), "file".to_owned(), "query".to_owned()],
            envelope: Some(env),
            ..call_spec()
        },
    );
    let line = line_of(&record);

    for forbidden in [secret_name, secret_path, "secret-repo", "core.ts"] {
        assert!(!line.contains(forbidden), "{forbidden} reached {line}");
    }
    // The names survive, sorted and deduplicated; only the values are gone.
    assert_eq!(field_text(&line, "arg_names_set"), r#"["file","query"]"#);
}

#[test]
fn the_enums_spell_the_typescript_strings() {
    let statuses = [
        RecordStatus::Ok,
        RecordStatus::Ambiguous,
        RecordStatus::NotFound,
        RecordStatus::Error,
        RecordStatus::Bootstrapping,
        RecordStatus::Absent,
    ];
    let spellings: Vec<Value> = statuses.iter().map(wire).collect();
    assert_eq!(
        spellings,
        vec![
            json!("ok"),
            json!("ambiguous"),
            json!("not_found"),
            json!("error"),
            json!("bootstrapping"),
            json!("absent"),
        ]
    );

    // The five envelope statuses map onto their recorded twin, and `absent` is this module's own.
    for (status, recorded) in Status::ALL.iter().zip(statuses) {
        assert_eq!(RecordStatus::from(*status), recorded);
    }

    assert_eq!(wire(&Kind::Call), json!("call"));
    assert_eq!(wire(&Kind::IndexRun), json!("index_run"));
    assert_eq!(wire(&Kind::Session), json!("session"));
    assert_eq!(wire(&RecordedStaleReason::Other), json!("other"));
    assert_eq!(wire(&SessionMode::Worker), json!("worker"));
    assert_eq!(wire(&IndexMode::Fast), json!("fast"));
    assert_eq!(wire(&RepoSizeBucket::Xl), json!("xl"));
    assert_eq!(wire(&RootKind::Plain), json!("plain"));
}

#[test]
fn the_repo_size_bucket_thresholds_are_inclusive_low() {
    const MIB: u64 = 1024 * 1024;
    let cases: [(u64, Value); 7] = [
        (0, json!("none")),
        (1, json!("xs")),
        (MIB - 1, json!("xs")),
        (MIB, json!("s")),
        (10 * MIB, json!("m")),
        (100 * MIB, json!("l")),
        (1024 * MIB, json!("xl")),
    ];
    for (bytes, expected) in cases {
        assert_eq!(wire(&repo_size_bucket_of(bytes)), expected, "bytes {bytes}");
    }
    assert_eq!(wire(&root_kind_of(true)), json!("git"));
    assert_eq!(wire(&root_kind_of(false)), json!("plain"));
}

#[test]
fn the_utc_day_is_the_first_ten_characters_of_the_timestamp() {
    assert_eq!(utc_date_of(FIXED_TS), Some(FIXED_DATE));
    assert_eq!(utc_date_of("2026-11-19T00:00:01.000Z"), Some("2026-11-19"));

    // A value that does not begin with a day names none, rather than guessing one: this string also
    // becomes a file name, so a refusal is the only safe answer.
    for refused in ["2026", "", "yesterday at ten", "../escape/x yz"] {
        assert_eq!(utc_date_of(refused), None, "{refused}");
    }
}

#[test]
fn one_install_id_per_utc_day_stable_within_it_and_rotated_on_the_next() {
    let dir = TempDir::new("identity-day");

    let first = resolve_install_id(dir.path(), FIXED_DATE);
    assert!(first.is_first_run);
    assert!(is_hex_32(&first.install_id));

    // The file is exactly `JSON.stringify({install_id, date})` plus the newline, which is the shape a
    // reader on either side parses.
    let file = dir.path().join("install_id.json");
    assert_eq!(
        fs::read_to_string(&file).expect("the file reads"),
        format!(
            "{{\"install_id\":\"{}\",\"date\":\"{FIXED_DATE}\"}}\n",
            first.install_id
        )
    );

    let later = resolve_install_id(dir.path(), FIXED_DATE);
    assert_eq!(later.install_id, first.install_id);
    assert!(!later.is_first_run);

    let tomorrow = resolve_install_id(dir.path(), "2026-11-19");
    assert!(tomorrow.is_first_run);
    assert!(is_hex_32(&tomorrow.install_id));
    assert_ne!(tomorrow.install_id, first.install_id);

    // One file, replaced in place: a rotation is a delete and a create, never a second file.
    assert_eq!(file_names(dir.path()), vec!["install_id.json".to_owned()]);
    assert_eq!(
        fs::read_to_string(&file).expect("the file reads"),
        format!(
            "{{\"install_id\":\"{}\",\"date\":\"2026-11-19\"}}\n",
            tomorrow.install_id
        )
    );
}

#[test]
fn a_torn_or_foreign_file_is_rotated_rather_than_trusted() {
    let dir = TempDir::new("identity-torn");
    let file = dir.path().join("install_id.json");

    for torn in [
        "not json at all",
        "[]",
        "{\"install_id\":\"\",\"date\":\"2026-11-18\"}",
        "{\"install_id\":42,\"date\":\"2026-11-18\"}",
        "{\"install_id\":\"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\"}",
    ] {
        fs::write(&file, torn).expect("the fixture writes");
        let resolved = resolve_install_id(dir.path(), FIXED_DATE);
        assert!(resolved.is_first_run, "{torn} must not be trusted");
        assert!(is_hex_32(&resolved.install_id));
        assert_eq!(
            fs::read_to_string(&file).expect("the file reads"),
            format!(
                "{{\"install_id\":\"{}\",\"date\":\"{FIXED_DATE}\"}}\n",
                resolved.install_id
            )
        );
    }

    // A file from another day is replaced, not kept beside the new one.
    fs::write(
        &file,
        "{\"install_id\":\"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f\",\"date\":\"2026-11-17\"}\n",
    )
    .expect("the fixture writes");
    let rotated = resolve_install_id(dir.path(), FIXED_DATE);
    assert!(rotated.is_first_run);
    assert_ne!(rotated.install_id, "0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f");
}

#[test]
fn processes_that_start_together_share_one_id_and_exactly_one_creates_it() {
    let dir = TempDir::new("identity-race");
    let workers = 8;
    let barrier = Arc::new(Barrier::new(workers));

    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let path = dir.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            // Start every caller in the same instant: the exclusive create is what decides between
            // them, not the order the scheduler happened to pick.
            barrier.wait();
            resolve_install_id(&path, FIXED_DATE)
        }));
    }
    let resolved: Vec<InstallIdentity> = handles
        .into_iter()
        .map(|handle| handle.join().expect("the worker finishes"))
        .collect();

    for identity in &resolved {
        assert!(is_hex_32(&identity.install_id), "{identity:?}");
    }
    // Someone must have won the create: a start that reads no file always attempts one.
    assert!(
        resolved.iter().any(|identity| identity.is_first_run),
        "no caller created the day's file: {resolved:?}"
    );
    // Whatever the interleaving, the file that is left holds a usable id — the one the next process
    // will adopt.
    let raw = fs::read_to_string(dir.path().join("install_id.json")).expect("the file reads");
    let written: Value = serde_json::from_str(&raw).expect("the file parses");
    let written_id = written["install_id"]
        .as_str()
        .expect("the file records an id");
    assert!(is_hex_32(written_id), "{raw}");
    assert_eq!(written["date"], json!(FIXED_DATE));

    // A caller that reads the day's file adopts it and never claims the first run.
    let adopted = resolve_install_id(dir.path(), FIXED_DATE);
    assert!(!adopted.is_first_run);
    assert!(raw.contains(&adopted.install_id));
}

#[test]
fn a_create_that_loses_the_race_without_a_readable_winner_still_yields_an_id() {
    let dir = TempDir::new("identity-lost-race");
    // The path is occupied by something that is not a readable id file: the read finds nothing and
    // neither the removal nor the exclusive create can replace it. A process must still run.
    fs::create_dir(dir.path().join("install_id.json")).expect("the obstruction is created");

    let resolved = resolve_install_id(dir.path(), FIXED_DATE);
    assert!(is_hex_32(&resolved.install_id));
    assert!(!resolved.is_first_run);
}

#[test]
fn a_session_id_is_32_hex_and_differs_between_two_mints() {
    let first = new_session_id();
    let second = new_session_id();
    assert!(is_hex_32(&first), "{first}");
    assert!(is_hex_32(&second), "{second}");
    assert_ne!(first, second);
}

#[test]
fn a_daily_file_is_created_then_only_ever_appended_to() {
    let dir = TempDir::new("sink-append");
    let target = dir.path().join("telemetry");
    let mut sink = jsonl_sink(&target);
    let file = target.join(format!("{FIXED_DATE}.jsonl"));

    sink.write(FIXED_DATE, &["{\"a\":1}".to_owned()])
        .expect("the first append succeeds");
    assert_eq!(
        fs::read_to_string(&file).expect("the file reads"),
        "{\"a\":1}\n"
    );

    // Bytes this process did not write survive an append: a rotation is a delete, never a truncate.
    fs::write(&file, "{\"pre\":0}\n{\"a\":1}\n").expect("the fixture writes");
    sink.write(FIXED_DATE, &["{\"b\":2}".to_owned()])
        .expect("the second append succeeds");
    assert_eq!(
        fs::read_to_string(&file).expect("the file reads"),
        "{\"pre\":0}\n{\"a\":1}\n{\"b\":2}\n"
    );

    // A ready sink is not the only one that appends: a fresh process over the same directory appends
    // too.
    let mut reopened = jsonl_sink(&target);
    reopened
        .write(FIXED_DATE, &["{\"c\":3}".to_owned()])
        .expect("the reopened append succeeds");
    assert_eq!(
        fs::read_to_string(&file).expect("the file reads"),
        "{\"pre\":0}\n{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n"
    );
}

#[test]
fn one_write_is_one_joined_append_with_a_trailing_newline() {
    let dir = TempDir::new("sink-lines");
    let target = dir.path().join("telemetry");
    let mut sink = jsonl_sink(&target);

    sink.write(
        FIXED_DATE,
        &["{\"i\":1}".to_owned(), "{\"i\":2}".to_owned()],
    )
    .expect("the append succeeds");

    assert_eq!(
        fs::read_to_string(target.join(format!("{FIXED_DATE}.jsonl"))).expect("the file reads"),
        "{\"i\":1}\n{\"i\":2}\n"
    );
}

#[test]
fn a_zero_line_write_creates_neither_a_directory_nor_a_file() {
    let dir = TempDir::new("sink-empty");
    let target = dir.path().join("telemetry");
    let mut sink = jsonl_sink(&target);

    sink.write(FIXED_DATE, &[])
        .expect("there is nothing to write");

    // A bare newline would be an unparsed (torn) line to the reader, so there is no file at all.
    assert!(!target.exists());
}

#[test]
fn a_flush_that_straddles_midnight_writes_one_file_per_date() {
    let dir = TempDir::new("sink-midnight");
    let mut sink = jsonl_sink(dir.path());

    sink.write("2026-11-18", &["{\"i\":1}".to_owned()])
        .expect("the first append succeeds");
    sink.write("2026-11-19", &["{\"i\":2}".to_owned()])
        .expect("the second append succeeds");
    sink.write("2026-11-18", &["{\"i\":3}".to_owned()])
        .expect("the third append succeeds");

    assert_eq!(
        file_names(dir.path()),
        vec!["2026-11-18.jsonl".to_owned(), "2026-11-19.jsonl".to_owned()]
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("2026-11-18.jsonl")).expect("the file reads"),
        "{\"i\":1}\n{\"i\":3}\n"
    );
}

#[test]
fn the_null_sink_writes_nothing_anywhere() {
    let dir = TempDir::new("sink-null");
    let mut sink = null_sink();
    sink.write(FIXED_DATE, &["{\"i\":1}".to_owned()])
        .expect("discarding a line cannot fail");
    assert!(file_names(dir.path()).is_empty());
}
