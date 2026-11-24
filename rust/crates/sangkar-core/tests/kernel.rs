//! The kernel's three primitives: the closed error-code union, the stderr line, and the path
//! helpers.
//!
//! Every expectation here was taken from the TypeScript oracle (`src/shared/{errors,log,paths}.ts`
//! run under Bun 1.4.4), not re-derived from the ported code.

use std::process::Command;

use sangkar_core::errors::{
    APP_ERROR_CODES, AppError, ErrorCode, ErrorPayload, to_error_payload, to_internal_payload,
};
use sangkar_core::log::{self, Level};
use sangkar_core::paths::{is_under_prefix, normalize_rel, prefix_bounds, qn, resolve_from};
use serde_json::{Value, json};

#[test]
fn app_error_codes_are_the_twelve_in_published_order() {
    assert_eq!(
        APP_ERROR_CODES,
        [
            "wrong_project_db",
            "unsupported_filesystem",
            "schema_too_new",
            "cursor_invalid",
            "cursor_expired",
            "cursor_mismatch",
            "budget_too_large",
            "bootstrapping",
            "not_found",
            "ambiguous",
            "index_locked",
            "internal",
        ]
    );

    // The newtype the rest of the workspace constructs has to publish the same union, in the same
    // order, or `APP_ERROR_CODES` and `ErrorCode` could drift apart without a failing build.
    let spellings: Vec<&str> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
    assert_eq!(spellings, APP_ERROR_CODES.to_vec());
}

#[test]
fn every_code_serialises_to_its_typescript_spelling() {
    for code in ErrorCode::ALL {
        let payload = to_error_payload(&AppError::new(code, "boom"));
        assert_eq!(payload.code, code);
        assert!(
            payload.code == code.as_str(),
            "a code compares equal to its own spelling: {code}"
        );

        let value = serde_json::to_value(&payload).expect("the payload is serialisable");
        assert_eq!(value["code"], json!(code.as_str()));
        // `code` then `message`, and nothing else when no optional member is set.
        assert_eq!(object_keys(&value), ["code", "message"]);
        assert_eq!(
            serde_json::to_string(&payload).expect("serialisable"),
            format!(r#"{{"code":"{}","message":"boom"}}"#, code.as_str())
        );
    }

    assert_eq!(
        serde_json::to_string(&to_error_payload(&AppError::new(
            ErrorCode::NOT_FOUND,
            "no such symbol"
        )))
        .expect("serialisable"),
        r#"{"code":"not_found","message":"no such symbol"}"#
    );
}

#[test]
fn error_payload_omits_absent_optionals() {
    let payload = to_error_payload(&AppError::new(ErrorCode::AMBIGUOUS, "two units match"));
    assert_eq!(payload.hint, None);
    // `retryable: false` is absent, not `"retryable":false`: the TypeScript only writes it when true.
    assert_eq!(payload.retryable, None);
    assert_eq!(payload.retry_after_ms, None);
    assert_eq!(
        serde_json::to_string(&payload).expect("serialisable"),
        r#"{"code":"ambiguous","message":"two units match"}"#
    );

    let explicitly_false =
        to_error_payload(&AppError::new(ErrorCode::INTERNAL, "bug").with_retryable(false));
    assert_eq!(explicitly_false.retryable, None);
}

#[test]
fn error_payload_serialises_present_optionals_in_order() {
    let error = AppError::new(ErrorCode::INDEX_LOCKED, "the indexer holds the lock")
        .with_hint("retry once `sangkar index` finishes")
        .with_retryable(true)
        .with_retry_after_ms(250);
    assert_eq!(error.payload(), to_error_payload(&error));
    assert_eq!(
        serde_json::to_string(&to_error_payload(&error)).expect("serialisable"),
        concat!(
            r#"{"code":"index_locked","message":"the indexer holds the lock","#,
            r#""hint":"retry once `sangkar index` finishes","retryable":true,"retry_after_ms":250}"#
        )
    );
}

#[test]
fn error_payload_round_trips_and_refuses_an_unknown_code() {
    let payload = to_error_payload(&AppError::new(ErrorCode::CURSOR_EXPIRED, "stale cursor"));
    let line = serde_json::to_string(&payload).expect("serialisable");
    let parsed: ErrorPayload = serde_json::from_str(&line).expect("the payload parses back");
    assert_eq!(parsed, payload);

    // A payload from another process is data: a code outside the union is a contract break, so it
    // is refused at the boundary instead of coerced to `internal`.
    let unknown = serde_json::from_str::<ErrorPayload>(r#"{"code":"not_a_code","message":"x"}"#);
    assert!(unknown.is_err(), "an unknown code must not deserialise");

    // The `toErrorPayload(unknown)` branch: no code of its own means `internal`.
    assert_eq!(
        to_internal_payload("something threw"),
        ErrorPayload {
            code: ErrorCode::INTERNAL,
            message: "something threw".to_owned(),
            hint: None,
            retryable: None,
            retry_after_ms: None,
        }
    );
}

#[test]
fn prefix_bounds_exclude_the_src_old_sibling() {
    let bounds = prefix_bounds("src");
    assert_eq!(bounds.exact, "src");
    assert_eq!(bounds.low, "src/");
    assert_eq!(bounds.high, "src0");

    // `-` (0x2d) sorts below `/` (0x2f), which is why `low` is `exact + "/"` and not `exact`: a
    // naive `path >= exact` lower bound is exactly what lets the sibling in.
    assert!("src-old/a.ts" > "src");
    assert!("src-old/a.ts" < bounds.low.as_str());

    assert!(!is_under_prefix("src-old/a.ts", "src"));
    assert!(is_under_prefix("src/a.ts", "src"));
    assert!(is_under_prefix("src", "src"));
    assert!(is_under_prefix("src/nested/deep/a.ts", "src"));
    assert!(!is_under_prefix("srcx/a.ts", "src"));
    assert!(!is_under_prefix("other/src/a.ts", "src"));
}

#[test]
fn prefix_bounds_normalise_the_prefix() {
    let bounds = prefix_bounds("./src/");
    assert_eq!(
        (
            bounds.exact.as_str(),
            bounds.low.as_str(),
            bounds.high.as_str()
        ),
        ("src", "src/", "src0")
    );

    let nested = prefix_bounds("src\\lib");
    assert_eq!(nested.exact, "src/lib");
    assert_eq!(nested.high, "src/lib0");

    // The empty prefix is the repository root: `high` is "0" and the range is `/` .. `0`.
    let root = prefix_bounds("");
    assert_eq!(
        (root.exact.as_str(), root.low.as_str(), root.high.as_str()),
        ("", "/", "0")
    );
    assert!(is_under_prefix("", ""));
    assert!(!is_under_prefix("a", ""));
}

#[test]
fn normalize_rel_strips_dot_segments_and_keeps_dot_travel() {
    assert_eq!(normalize_rel(""), "");
    assert_eq!(normalize_rel("."), "");
    assert_eq!(normalize_rel("./"), "");
    assert_eq!(normalize_rel("./src/a.ts"), "src/a.ts");
    assert_eq!(normalize_rel("src\\a\\b.ts"), "src/a/b.ts");
    assert_eq!(normalize_rel("a/./b//c/"), "a/b/c");
    assert_eq!(normalize_rel("/abs/a.ts"), "abs/a.ts");
    // `..` is kept: collapsing it would make an escaping path look contained.
    assert_eq!(normalize_rel("../x"), "../x");
    assert_eq!(normalize_rel("a/../b"), "a/../b");
}

#[test]
fn qn_formats_a_dotted_scope() {
    assert_eq!(
        qn("src/a/b.ts", "Class.method", "member"),
        "src/a/b.ts::Class.method.member"
    );
    assert_eq!(qn("./src//a.ts", "Klass", "m"), "src/a.ts::Klass.m");
    // A top-level symbol passes an empty scope and gets `path::name`, never `path::.name`.
    assert_eq!(qn("src/a/b.ts", "", "foo"), "src/a/b.ts::foo");
}

#[test]
fn resolve_from_resolves_relative_and_parent_specifiers() {
    assert_eq!(
        resolve_from("src/a/b.ts", "./x.js").as_deref(),
        Some("src/a/x")
    );
    assert_eq!(
        resolve_from("src/a/b.ts", "./x").as_deref(),
        Some("src/a/x")
    );
    assert_eq!(
        resolve_from("src/a/b.ts", ".\\x.js").as_deref(),
        Some("src/a/x")
    );
    assert_eq!(resolve_from("src/a/b.ts", "../c").as_deref(), Some("src/c"));
    assert_eq!(resolve_from("src/a/b.ts", "../../c").as_deref(), Some("c"));
    assert_eq!(resolve_from("src/a/b.ts", "..").as_deref(), Some("src"));
    // An importer with no directory resolves against the root.
    assert_eq!(resolve_from("a.ts", "./x").as_deref(), Some("x"));
    assert_eq!(resolve_from("", "./x").as_deref(), Some("x"));
    // Only an extension after a name is stripped: a dotfile keeps its leading dot, and only the
    // last `.` starts the extension.
    assert_eq!(
        resolve_from("src/a/b.ts", "./.eslintrc").as_deref(),
        Some("src/a/.eslintrc")
    );
    assert_eq!(
        resolve_from("src/a/b.ts", "./a.b.c").as_deref(),
        Some("src/a/a.b")
    );
    assert_eq!(
        resolve_from("src/a/b.ts", "./dir/").as_deref(),
        Some("src/a/dir")
    );
}

#[test]
fn resolve_from_refuses_rooted_and_escaping_specifiers() {
    assert_eq!(resolve_from("src/a/b.ts", ""), None);
    assert_eq!(resolve_from("src/a/b.ts", "/abs/x"), None);
    assert_eq!(resolve_from("src/a/b.ts", "C:/abs/x"), None);
    // Climbing above the repository root is not a stored path, so it resolves to nothing.
    assert_eq!(resolve_from("src/a/b.ts", "../.."), None);
    assert_eq!(resolve_from("src/a/b.ts", "../../../c"), None);
}

#[test]
fn resolve_from_gives_the_mechanical_answer_for_bare_and_node_specifiers() {
    // The resolver guards with `is_relative_specifier` before calling this, so a package name or a
    // `node:` builtin never arrives here in practice. These two assertions pin the arithmetic that
    // runs if one does: the specifier is appended to the importer's directory, never resolved.
    assert_eq!(
        resolve_from("src/a/b.ts", "lodash").as_deref(),
        Some("src/a/lodash")
    );
    assert_eq!(
        resolve_from("src/a/b.ts", "node:fs").as_deref(),
        Some("src/a/node:fs")
    );
}

/// The child half of [`log_writes_one_compact_json_line_per_call`].
///
/// `#[ignore]` keeps it out of a plain `cargo test`; the parent spawns this same binary with
/// `--ignored --exact` and reads the real stderr, which is the only way to observe the bytes the
/// logger writes without a second implementation of the writer existing for tests.
#[test]
#[ignore = "spawned by log_writes_one_compact_json_line_per_call"]
fn log_child_emits_the_probe_lines() {
    // A field whose name collides with a record key: `ts`/`level`/`msg` keep their positions and
    // the field wins the value, which is what the TypeScript's object spread does.
    log::log(
        Level::Info,
        "kernel probe",
        &[("repo", json!("sangkar")), ("msg", json!("overridden"))],
    );
    log::debug("debug message", &[]);
    log::info("info message", &[]);
    log::warn("warn message", &[]);
    log::error("error message", &[]);
}

#[test]
fn log_writes_one_compact_json_line_per_call() {
    let exe = std::env::current_exe().expect("the test binary's own path");
    let output = Command::new(&exe)
        .args([
            "--ignored",
            "--exact",
            "log_child_emits_the_probe_lines",
            "--nocapture",
        ])
        .output()
        .expect("spawn the test binary again");
    assert!(
        output.status.success(),
        "the child failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(stderr.ends_with('\n'), "the record ends with a newline");
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(
        lines.len(),
        5,
        "one line per log call, and no other stderr writer: {stderr:?}"
    );

    let probe: Value = serde_json::from_str(lines[0]).expect("the line is JSON");
    assert_eq!(object_keys(&probe), ["ts", "level", "msg", "repo"]);
    assert_eq!(probe["level"], json!("info"));
    assert_eq!(probe["msg"], json!("overridden"));
    assert_eq!(probe["repo"], json!("sangkar"));
    assert_eq!(
        lines[0],
        serde_json::to_string(&probe).expect("re-serialisable"),
        "compact JSON: no space after `:` or `,` and no trailing whitespace"
    );
    assert_iso8601_millis(probe["ts"].as_str().expect("ts is a string"));

    for (line, spelling) in lines[1..].iter().zip(["debug", "info", "warn", "error"]) {
        let record: Value = serde_json::from_str(line).expect("the line is JSON");
        assert_eq!(object_keys(&record), ["ts", "level", "msg"]);
        assert_eq!(record["level"], json!(spelling));
        assert_eq!(
            *line,
            serde_json::to_string(&record).expect("re-serialisable")
        );
        assert_iso8601_millis(record["ts"].as_str().expect("ts is a string"));
    }
}

fn object_keys(value: &Value) -> Vec<&str> {
    value
        .as_object()
        .expect("the record is a JSON object")
        .keys()
        .map(String::as_str)
        .collect()
}

/// `new Date().toISOString()`: 24 bytes, `YYYY-MM-DDTHH:MM:SS.sssZ`.
fn assert_iso8601_millis(ts: &str) {
    assert_eq!(ts.len(), 24, "millisecond ISO-8601: {ts}");
    let bytes = ts.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        let separator = match index {
            4 | 7 => Some(b'-'),
            10 => Some(b'T'),
            13 | 16 => Some(b':'),
            19 => Some(b'.'),
            23 => Some(b'Z'),
            _ => None,
        };
        match separator {
            Some(expected) => assert_eq!(*byte, expected, "byte {index} of {ts}"),
            None => assert!(byte.is_ascii_digit(), "byte {index} of {ts} is not a digit"),
        }
    }
    // A timestamp the TypeScript side would have formatted with an offset or a different precision
    // would still be 24 bytes only by accident, so pin the shape one step further: a plausible year.
    let year: i32 = ts[..4].parse().expect("the year is four digits");
    assert!(
        (1970..=9999).contains(&year),
        "the year is in the ISO four-digit range: {ts}"
    );
}
