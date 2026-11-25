//! The argument layer and the two renderings, ported from `tests/cli-args.test.ts` and
//! `tests/cli-render.test.ts`.
//!
//! The TypeScript suite reads the seven real manifests, because a field added to a tool without
//! teaching the parser about it has to fail there. The Rust manifests do not exist yet — they are
//! the deliverable of the seven tool tasks that follow this one — so this file carries each
//! manifest's `input` table verbatim as a fixture. When a manifest lands, the fixture is deleted and
//! the test reads `sangkar_tools::<tool>::manifest::input` instead; until then the parser is proven
//! against the same data, not against a smaller one somebody invented here.
//!
//! Three things are pinned beyond the ported cases, because they are this port's own risk:
//!
//! - `render_json`'s bytes against a golden captured from the TypeScript CLI with `--json`
//!   (`rust/tests/golden/polyglot/status.json`). The golden's `stdout` is compact JSON plus one
//!   newline, so a renderer that re-ordered a key, wrote `null` for an absent one or ended in two
//!   newlines fails here rather than at cutover.
//! - the verb usage blocks, character for character, including the `1000..300000` clamp the repo
//!   verb prints and the `"run"`-only note the daemon block carries.
//! - one case per verb flag: the table has to be the complete set the TypeScript CLI accepts today,
//!   and a flag it does not have (`--yes` on `worker`) has to be refused rather than ignored.

use std::fs;

use sangkar_cli::args::{
    ArgValue, CliUsageError, CommandResolution, DefaultValue, GLOBAL_VALUE_FLAGS, Globals,
    InputField, InputLiteral, Items, ParsedCliArgs, ToolArgs, VERB_SPECS, Verb, expectation_of,
    parse_args, parse_cli_args, parse_cli_args_and_names, parse_verb, parse_verb_args,
    positional_field_of, resolve_command, split_globals, usage_of, verb_spec,
};
use sangkar_cli::render::{
    EXIT_FAILURE, EXIT_USAGE, exit_code_of, mcp_text, render_json, render_output, render_toon,
};
use sangkar_core::envelope::{
    BootstrappingSpec, Coverage, Envelope, EnvelopeSpec, ReadStatus, ambiguous, bootstrapping,
    fail, not_found, ok,
};
use sangkar_core::errors::AppError;
use sangkar_core::repo_config::{WATCH_INTERVAL_MAX_MS, WATCH_INTERVAL_MIN_MS};
use serde::Serialize;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// The seven manifests' `input` tables, verbatim. See the module doc for why they live here.
// ---------------------------------------------------------------------------------------------

/// The `repo` description every manifest carries, byte for byte.
const REPO_DESCRIPTION: &str = "Registered repository to answer from — its path or basename. Omit it \
                                to use the working directory's repository, or the only registered \
                                one.";

const SEARCH_KINDS: &[&str] = &["function", "method", "class", "file", "decorator"];

const NODE_KINDS: &[&str] = &[
    "file", "module", "class", "function", "method", "test", "endpoint",
];

const DIRECTIONS: &[&str] = &["all", "in", "out"];

const UNRESOLVED_BUCKETS: &[&str] = &[
    "builtin_stdlib",
    "external_package",
    "internal_module",
    "dynamic_property_call",
    "receiver_type_unknown",
    "ambiguous",
];

const STATUS_INPUT: InputLiteral = &[
    (
        "paths",
        InputField::array(Items::String)
            .with_default(DefaultValue::List(&[]))
            .with_description("Paths whose per-path coverage and skip count this answer reports."),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(800))
            .with_description(
                "Token ceiling for the coverage report, so forty paths cannot blow it up.",
            ),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const SUMMARY_INPUT: InputLiteral = &[
    (
        "scope",
        InputField::enumeration(&["change", "repo", "area"])
            .with_default(DefaultValue::Text("change"))
            .with_description(
                "Which slice to summarize: working-tree diff, whole repository, or one path.",
            ),
    ),
    (
        "target",
        InputField::string()
            .with_description("The path the `area` scope describes; ignored by the other two."),
    ),
    (
        "base",
        InputField::string()
            .with_description("The git revision the working-tree diff is taken against."),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(1200))
            .with_description("Token ceiling for the answer; this hot path must stay small."),
    ),
    (
        "cursor",
        InputField::string()
            .with_description("Continuation token a previous truncated answer returned."),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const TRACE_INPUT: InputLiteral = &[
    (
        "query",
        InputField::string()
            .required()
            .with_description("The symbol to look up: a name, dotted name, path or `@decorator`."),
    ),
    (
        "direction",
        InputField::enumeration(DIRECTIONS)
            .with_default(DefaultValue::Text("all"))
            .with_description("Which way to walk from the symbol: in, out or both."),
    ),
    (
        "kinds",
        InputField::array(Items::Enum(NODE_KINDS)).with_description(
            "Narrow resolution to these node kinds when a name collides across kinds.",
        ),
    ),
    (
        "path",
        InputField::string()
            .with_description("Repo-relative prefix: the path itself and everything under it."),
    ),
    (
        "depth",
        InputField::integer()
            .with_default(DefaultValue::Integer(2))
            .with_bounds(1, 4)
            .with_description("How many hops of callers and callees to walk from the symbol."),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(3200))
            .with_description(
                "Token ceiling for this result; exceeding it truncates and sets `next`.",
            ),
    ),
    (
        "unresolved",
        InputField::enumeration(UNRESOLVED_BUCKETS)
            .with_description("List only this unresolved bucket; the census stays unfiltered."),
    ),
    (
        "cursor",
        InputField::string().with_description(
            "Continue this same query from the `next` cursor of a previous page.",
        ),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const SEARCH_INPUT: InputLiteral = &[
    (
        "query",
        InputField::string()
            .required()
            .with_description("Free text to find: an identifier, a substring or a phrase."),
    ),
    (
        "kind",
        InputField::enumeration(SEARCH_KINDS)
            .with_description("Return only declarations of this kind, when you already know it."),
    ),
    (
        "path",
        InputField::string()
            .with_description("Repo-relative prefix: the path itself and everything under it."),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(2000))
            .with_description(
                "Token ceiling for this result; exceeding it truncates and sets `next`.",
            ),
    ),
    (
        "cursor",
        InputField::string().with_description(
            "Continue this same search from the `next` cursor of a previous page.",
        ),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const IMPACT_INPUT: InputLiteral = &[
    (
        "paths",
        InputField::array(Items::String).with_description(
            "Explicit change set to analyse; omit it to measure the working-tree diff.",
        ),
    ),
    (
        "base",
        InputField::string().with_description("The git revision the change set is diffed against."),
    ),
    (
        "depth",
        InputField::integer()
            .with_default(DefaultValue::Integer(2))
            .with_bounds(1, 4)
            .with_description("How many caller hops the impact walk follows from the change set."),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(2500))
            .with_description(
                "Token ceiling for the answer; a longer dependents list continues via cursor.",
            ),
    ),
    (
        "cursor",
        InputField::string()
            .with_description("Continuation token a previous truncated answer returned."),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const EXPAND_INPUT: InputLiteral = &[
    (
        "target",
        InputField::string().required().with_description(
            "One opaque handle copied verbatim from a prior payload: an id, a qualified name or a \
             path. Detail is addressable here instead of pre-shipped in every other tool's answer.",
        ),
    ),
    (
        "what",
        InputField::enumeration(&["members", "edges", "source", "coverage", "outline"])
            .with_description(
                "Which detail to return — one of `members`, `edges`, `source`, `coverage` or \
                 `outline` — inferred from the target's kind when omitted and always echoed in the \
                 answer. `outline` is the one value never inferred: ask for it by name.",
            ),
    ),
    (
        "path",
        InputField::string().with_description(
            "Repo-relative prefix applied when resolving the target, so the `Widget` under `tests/` \
             can be named. Optional: an unprefixed target is the common case.",
        ),
    ),
    (
        "range",
        InputField::array(Items::Integer).with_description(
            "The 1-based, inclusive line span [from, to] to return for a source request, read \
             inside the resolved node's own extent and clamped to it; the answer says when a \
             request was narrowed. Defaults to the node's whole extent.",
        ),
    ),
    (
        "budget",
        InputField::integer()
            .with_default(DefaultValue::Integer(2500))
            .with_description(
                "Token budget for the answer, larger by default than `summary`'s because detail is \
                 by definition the expensive part.",
            ),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
];

const INDEX_INPUT: InputLiteral = &[
    (
        "force",
        InputField::boolean()
            .with_default(DefaultValue::Bool(false))
            .with_description(
                "Rebuild from an empty stage and publish even when the file hashes say nothing \
                 changed.",
            ),
    ),
    (
        "mode",
        InputField::enumeration(&["full", "fast"])
            .with_default(DefaultValue::Text("full"))
            .with_description(
                "full rebuilds the graph from an empty stage; fast republishes only what changed \
                 and is the cheaper first pass on a huge repository.",
            ),
    ),
    (
        "repo",
        InputField::string().with_description(REPO_DESCRIPTION),
    ),
    (
        "register",
        InputField::boolean()
            .with_default(DefaultValue::Bool(true))
            .with_description(
                "Add this repository to the sangkar registry so the daemon watches it. Use \
                 --no-register to index without registering.",
            ),
    ),
];

/// The seven tools, in the composition root's registration order.
const MANIFESTS: [(&str, InputLiteral); 7] = [
    ("status", STATUS_INPUT),
    ("summary", SUMMARY_INPUT),
    ("trace", TRACE_INPUT),
    ("search", SEARCH_INPUT),
    ("impact", IMPACT_INPUT),
    ("expand", EXPAND_INPUT),
    ("index", INDEX_INPUT),
];

/// The three tools whose one required string argument a bare token may bind.
const POSITIONAL_TOOLS: [(&str, InputLiteral, &str); 3] = [
    ("trace", TRACE_INPUT, "query"),
    ("search", SEARCH_INPUT, "query"),
    ("expand", EXPAND_INPUT, "target"),
];

/// The four tools with no required string argument, so a bare token has nowhere to go.
const NO_POSITIONAL_TOOLS: [(&str, InputLiteral); 4] = [
    ("status", STATUS_INPUT),
    ("summary", SUMMARY_INPUT),
    ("impact", IMPACT_INPUT),
    ("index", INDEX_INPUT),
];

fn argv(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|token| (*token).to_owned()).collect()
}

/// The refusal a call produced, or a panic naming the call that should have refused.
fn usage_error_of(run: impl FnOnce() -> Result<(), CliUsageError>) -> CliUsageError {
    match run() {
        Ok(()) => panic!("expected a CliUsageError"),
        Err(error) => error,
    }
}

/// The protocol refusal a call produced, or a panic naming the call that should have refused.
fn app_error_of(run: impl FnOnce() -> Result<(), AppError>) -> AppError {
    match run() {
        Ok(()) => panic!("expected an AppError"),
        Err(error) => error,
    }
}

/// One argument map as a JSON value, so an assertion reads as the record the handler receives.
fn as_value(args: ToolArgs) -> Value {
    Value::Object(args)
}

// ---------------------------------------------------------------------------------------------
// cli args: globals
// ---------------------------------------------------------------------------------------------

#[test]
fn the_value_flags_are_the_two_the_cli_knows() {
    assert_eq!(GLOBAL_VALUE_FLAGS, ["repo-root", "data-dir"]);
}

#[test]
fn a_verbs_own_flag_is_not_a_global() {
    // `sangkar telemetry --out` names a report path and nothing else, so promoting it to the global
    // set would make every tool accept it. The short list is the assertion.
    assert_eq!(GLOBAL_VALUE_FLAGS, ["repo-root", "data-dir"]);
    let refusal = usage_error_of(|| {
        parse_cli_args("status", STATUS_INPUT, &argv(&["--out", "x"])).map(|_| ())
    });
    assert_eq!(refusal.message, "unknown option \"--out\"");
    assert!(refusal.message.contains("--out"));
}

#[test]
fn switches_and_value_flags_are_removed_and_everything_else_keeps_its_order() {
    let split = split_globals(&argv(&[
        "--json",
        "--repo-root",
        "/tmp/repo",
        "trace",
        "find callers",
        "--data-dir=/tmp/graph",
        "--depth",
        "2",
        "--help",
    ]))
    .expect("every global carries a value");
    assert!(split.json);
    assert!(split.help);
    assert_eq!(split.rest, ["trace", "find callers", "--depth", "2"]);
}

#[test]
fn plain_argv_passes_through_untouched() {
    assert_eq!(
        split_globals(&argv(&[])).expect("empty argv"),
        Globals {
            json: false,
            help: false,
            rest: Vec::new(),
        }
    );
    let split = split_globals(&argv(&["trace", "--depth", "2"])).expect("no globals");
    assert!(!split.json);
    assert!(!split.help);
    assert_eq!(split.rest, ["trace", "--depth", "2"]);
}

#[test]
fn short_help_is_help_and_a_tool_flag_is_never_a_global() {
    let split = split_globals(&argv(&["-h"])).expect("a switch needs no value");
    assert!(split.help);
    assert!(split.rest.is_empty());
    let split = split_globals(&argv(&["--json-budget"])).expect("not a global");
    assert!(!split.json);
    assert!(!split.help);
    assert_eq!(split.rest, ["--json-budget"]);
}

#[test]
fn a_value_flag_with_no_value_is_refused_and_names_the_flag() {
    let cases: [(Vec<&str>, &str); 4] = [
        (vec!["--repo-root"], "--repo-root"),
        (vec!["trace", "--data-dir"], "--data-dir"),
        (vec!["--data-dir", "--json"], "--data-dir"),
        (vec!["--repo-root="], "--repo-root"),
    ];
    for (args, flag) in cases {
        let refusal = usage_error_of(|| split_globals(&argv(&args)).map(|_| ()));
        assert_eq!(refusal.message, format!("{flag} needs a value"));
        assert!(refusal.usage.contains("--repo-root"));
    }
}

// ---------------------------------------------------------------------------------------------
// cli args: where the command name sits
// ---------------------------------------------------------------------------------------------

#[test]
fn a_global_flag_before_the_name_does_not_hide_it() {
    // The entry point needs the name before any config exists, so `sangkar --json status` and
    // `sangkar status --json` have to name the same command.
    let cases: [(Vec<&str>, &str); 4] = [
        (vec!["--json", "status"], "status"),
        (vec!["--repo-root", "/x", "status"], "status"),
        (vec!["--repo-root=/x", "status"], "status"),
        (vec!["--data-dir", "/d", "--json", "trace", "q"], "trace"),
    ];
    for (args, name) in cases {
        let resolution = resolve_command(&argv(&args));
        assert_eq!(resolution.name.as_deref(), Some(name), "argv {args:?}");
    }
    // The tail is every token after the name: the globals before it are the entry point's, the ones
    // after it are the command's own to strip.
    assert_eq!(
        resolve_command(&argv(&["--data-dir", "/d", "--json", "trace", "q"])).rest,
        ["q"]
    );
}

#[test]
fn globals_after_the_name_are_kept_for_the_command_to_consume() {
    assert_eq!(
        resolve_command(&argv(&["status", "--json"])),
        CommandResolution {
            name: Some("status".to_owned()),
            rest: argv(&["--json"]),
            help: false,
        }
    );
    assert_eq!(
        resolve_command(&argv(&["status", "--repo-root", "/x"])).rest,
        ["--repo-root", "/x"]
    );
    // Including a value flag whose value is the last token: nothing is consumed before the name, so
    // the command sees exactly what the user typed and refuses the missing value itself.
    assert_eq!(
        resolve_command(&argv(&["trace", "--data-dir"])).rest,
        ["--data-dir"]
    );
}

#[test]
fn no_name_at_all_resolves_to_none_and_nothing_is_left_over() {
    let cases: [Vec<&str>; 6] = [
        vec![],
        vec!["--json"],
        vec!["--repo-root"],
        vec!["--repo-root="],
        vec!["--help"],
        vec!["-h"],
    ];
    for args in cases {
        let resolution = resolve_command(&argv(&args));
        assert_eq!(resolution.name, None, "argv {args:?} names nothing");
        assert!(resolution.rest.is_empty(), "argv {args:?} leaves nothing");
    }
}

#[test]
fn help_anywhere_sets_help_and_the_name_is_still_found() {
    let cases: [Vec<&str>; 4] = [
        vec!["trace", "--help"],
        vec!["--help"],
        vec!["-h", "trace"],
        vec!["--data-dir", "/d", "--help", "status"],
    ];
    for args in cases {
        assert!(resolve_command(&argv(&args)).help, "argv {args:?}");
    }
    assert_eq!(
        resolve_command(&argv(&["trace", "--help"])).name.as_deref(),
        Some("trace")
    );
    assert_eq!(resolve_command(&argv(&["--help"])).name, None);
}

#[test]
fn a_value_flag_with_nothing_to_consume_is_skipped_rather_than_refused() {
    // This scan runs before a usage block exists, so it may not throw: the spaced form reads the
    // next token as its value when that token is not a flag, and otherwise consumes nothing and lets
    // the following token name the command.
    assert_eq!(
        resolve_command(&argv(&["--repo-root", "--json", "status"])),
        CommandResolution {
            name: Some("status".to_owned()),
            rest: Vec::new(),
            help: false,
        }
    );
    assert_eq!(resolve_command(&argv(&["--repo-root"])).name, None);
    // A value written inline is its own token, so the name follows immediately.
    assert_eq!(
        resolve_command(&argv(&["--repo-root=", "status"])).name,
        Some("status".to_owned())
    );
}

// ---------------------------------------------------------------------------------------------
// cli args: the positional argument
// ---------------------------------------------------------------------------------------------

#[test]
fn a_bare_token_binds_the_tools_one_required_string_argument() {
    for (tool, input, field) in POSITIONAL_TOOLS {
        let split = split_globals(&argv(&["--json", tool, "some name", "--budget", "100"]))
            .expect("globals carry their values");
        let args =
            parse_cli_args(tool, input, &split.rest[1..]).expect("a positional and a budget");
        assert_eq!(
            args.get(field).and_then(Value::as_str),
            Some("some name"),
            "{tool}"
        );
        assert_eq!(
            args.get("budget").and_then(Value::as_i64),
            Some(100),
            "{tool}"
        );
    }
}

#[test]
fn a_second_positional_is_refused() {
    let refusal =
        usage_error_of(|| parse_cli_args("trace", TRACE_INPUT, &argv(&["one", "two"])).map(|_| ()));
    assert_eq!(refusal.message, "unexpected positional argument \"two\"");
    assert_eq!(refusal.usage, usage_of("trace", TRACE_INPUT));
}

#[test]
fn a_positional_already_given_as_a_flag_cannot_be_given_twice() {
    let refusal = usage_error_of(|| {
        parse_cli_args("search", SEARCH_INPUT, &argv(&["--query", "one", "two"])).map(|_| ())
    });
    assert_eq!(refusal.message, "unexpected positional argument \"two\"");
}

#[test]
fn a_positional_is_refused_for_a_tool_with_no_required_string_argument() {
    for (tool, input) in NO_POSITIONAL_TOOLS {
        let refusal =
            usage_error_of(|| parse_cli_args(tool, input, &argv(&["somewhere"])).map(|_| ()));
        assert_eq!(
            refusal.message, "unexpected positional argument \"somewhere\"",
            "{tool}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// cli args: flags
// ---------------------------------------------------------------------------------------------

#[test]
fn name_value_and_name_equals_value_are_both_accepted() {
    let args = parse_cli_args("trace", TRACE_INPUT, &argv(&["q", "--depth", "3"]))
        .expect("a spaced value");
    assert_eq!(args.get("depth").and_then(Value::as_i64), Some(3));
    let args =
        parse_cli_args("trace", TRACE_INPUT, &argv(&["q", "--depth=3"])).expect("an inline value");
    assert_eq!(args.get("depth").and_then(Value::as_i64), Some(3));
    let args = parse_cli_args("trace", TRACE_INPUT, &argv(&["--query", "find calls"]))
        .expect("a spaced string");
    assert_eq!(
        args.get("query").and_then(Value::as_str),
        Some("find calls")
    );
    let args = parse_cli_args("trace", TRACE_INPUT, &argv(&["--query=find calls"]))
        .expect("an inline string");
    assert_eq!(
        args.get("query").and_then(Value::as_str),
        Some("find calls")
    );
}

#[test]
fn integer_flags_are_read_with_numbers_rules() {
    let args =
        parse_cli_args("status", STATUS_INPUT, &argv(&["--budget", "1200"])).expect("a budget");
    assert_eq!(args.get("budget").and_then(Value::as_i64), Some(1200));
    let args = parse_cli_args("status", STATUS_INPUT, &argv(&["--budget=1200"])).expect("a budget");
    assert_eq!(args.get("budget").and_then(Value::as_i64), Some(1200));
    // A single dash is a value, so a negative number survives argv reading.
    let args = parse_cli_args("impact", IMPACT_INPUT, &argv(&["--depth", "-1"])).expect("a depth");
    assert_eq!(args.get("depth").and_then(Value::as_i64), Some(-1));
    // min/max are the handler's business: the parser neither clamps nor refuses.
    let args =
        parse_cli_args("trace", TRACE_INPUT, &argv(&["q", "--depth", "99"])).expect("a depth");
    assert_eq!(args.get("depth").and_then(Value::as_i64), Some(99));
}

#[test]
fn a_non_integer_integer_flag_is_refused() {
    for args in [
        vec!["--budget", "2.5"],
        vec!["--budget", "many"],
        vec!["--budget="],
    ] {
        let refusal =
            usage_error_of(|| parse_cli_args("status", STATUS_INPUT, &argv(&args)).map(|_| ()));
        assert!(
            refusal.message == "--budget must be an integer"
                || refusal.message == "--budget needs a value",
            "argv {args:?} answered {:?}",
            refusal.message
        );
    }
}

#[test]
fn a_flag_with_no_value_is_refused() {
    for args in [
        vec!["q", "--depth"],
        vec!["q", "--depth", "--budget", "5"],
        vec!["--query"],
    ] {
        let refusal =
            usage_error_of(|| parse_cli_args("trace", TRACE_INPUT, &argv(&args)).map(|_| ()));
        assert!(
            refusal.message == "--depth needs a value"
                || refusal.message == "--query needs a value",
            "argv {args:?} answered {:?}",
            refusal.message
        );
    }
}

#[test]
fn boolean_fields_take_the_switch_equals_true_equals_false_and_the_no_prefix() {
    let cases: [(Vec<&str>, bool); 4] = [
        (vec!["--force"], true),
        (vec!["--force=true"], true),
        (vec!["--force=false"], false),
        (vec!["--no-force"], false),
    ];
    for (args, expected) in cases {
        let parsed = parse_cli_args("index", INDEX_INPUT, &argv(&args)).expect("a boolean");
        assert_eq!(
            parsed.get("force").and_then(Value::as_bool),
            Some(expected),
            "argv {args:?}"
        );
    }
}

#[test]
fn a_bad_boolean_value_is_refused() {
    let refusal = usage_error_of(|| {
        parse_cli_args("index", INDEX_INPUT, &argv(&["--force=maybe"])).map(|_| ())
    });
    assert_eq!(refusal.message, "--force must be true or false");
}

#[test]
fn a_boolean_switch_does_not_swallow_the_next_token() {
    let refusal = usage_error_of(|| {
        parse_cli_args("index", INDEX_INPUT, &argv(&["--force", "full"])).map(|_| ())
    });
    assert_eq!(refusal.message, "unexpected positional argument \"full\"");
}

#[test]
fn array_fields_collect_repeats_and_never_split_a_comma() {
    let args = parse_cli_args(
        "impact",
        IMPACT_INPUT,
        &argv(&["--paths", "a.ts", "--paths", "b.ts"]),
    )
    .expect("repeated paths");
    assert_eq!(args.get("paths"), Some(&json!(["a.ts", "b.ts"])));
    let args = parse_cli_args(
        "status",
        STATUS_INPUT,
        &argv(&["--paths=a.ts", "--paths=b.ts"]),
    )
    .expect("repeated paths");
    assert_eq!(args.get("paths"), Some(&json!(["a.ts", "b.ts"])));
    let args = parse_cli_args("status", STATUS_INPUT, &argv(&["--paths", "one,two"]))
        .expect("a comma is a name");
    assert_eq!(args.get("paths"), Some(&json!(["one,two"])));
}

// ---------------------------------------------------------------------------------------------
// cli args: an integer array splits on the colon (`Done when`: `--range 1:5` is `[1, 5]`)
// ---------------------------------------------------------------------------------------------

#[test]
fn an_integer_array_flag_splits_on_the_colon_and_collects_repeats() {
    let args = parse_cli_args(
        "expand",
        EXPAND_INPUT,
        &argv(&["AGENTS.md", "--what", "source", "--range", "1:5"]),
    )
    .expect("a range pair");
    assert_eq!(
        args.get("target").and_then(Value::as_str),
        Some("AGENTS.md")
    );
    assert_eq!(args.get("range"), Some(&json!([1, 5])));

    let args = parse_cli_args(
        "expand",
        EXPAND_INPUT,
        &argv(&["x", "--range", "1:5", "--range", "7:9"]),
    )
    .expect("two range pairs");
    assert_eq!(args.get("range"), Some(&json!([1, 5, 7, 9])));

    // A malformed item is the schema's refusal, not a third wording: the same message a protocol
    // call gets.
    let refusal = usage_error_of(|| {
        parse_cli_args("expand", EXPAND_INPUT, &argv(&["x", "--range", "1:x"])).map(|_| ())
    });
    assert_eq!(
        refusal.message,
        "expand received an invalid \"range\" argument"
    );
    assert_eq!(refusal.usage, usage_of("expand", EXPAND_INPUT));
    assert_eq!(
        app_error_of(|| parse_args(
            "expand",
            EXPAND_INPUT,
            &json!({"target": "x", "range": [1, "x"]})
        )
        .map(|_| ()))
        .message,
        refusal.message
    );
}

// ---------------------------------------------------------------------------------------------
// cli args: refusals keep the schema's own message
// ---------------------------------------------------------------------------------------------

#[test]
fn an_unknown_flag_is_refused_and_named() {
    let refusal = usage_error_of(|| {
        parse_cli_args("search", SEARCH_INPUT, &argv(&["q", "--colour", "red"])).map(|_| ())
    });
    assert_eq!(refusal.message, "unknown option \"--colour\"");
    // `--no-…` is only a switch for a boolean field.
    let refusal =
        usage_error_of(|| parse_cli_args("index", INDEX_INPUT, &argv(&["--no-mode"])).map(|_| ()));
    assert_eq!(refusal.message, "unknown option \"--no-mode\"");
}

#[test]
fn an_enum_outside_the_manifest_reads_exactly_as_parse_args_reads_it() {
    let from_cli = usage_error_of(|| {
        parse_cli_args("search", SEARCH_INPUT, &argv(&["q", "--kind", "nope"])).map(|_| ())
    });
    let from_call = app_error_of(|| {
        parse_args(
            "search",
            SEARCH_INPUT,
            &json!({"query": "q", "kind": "nope"}),
        )
        .map(|_| ())
    });
    assert_eq!(from_cli.message, from_call.message);
    assert_eq!(
        from_cli.message,
        "search received an invalid \"kind\" argument"
    );
    assert_eq!(from_cli.usage, usage_of("search", SEARCH_INPUT));
    assert_eq!(
        from_call.hint.as_deref(),
        Some("\"kind\" must be one of: function, method, class, file, decorator")
    );
    let args = parse_cli_args("search", SEARCH_INPUT, &argv(&["q", "--kind=file"]))
        .expect("a declared kind");
    assert_eq!(args.get("kind").and_then(Value::as_str), Some("file"));
}

#[test]
fn a_missing_required_argument_reads_exactly_as_parse_args_reads_it() {
    let from_cli = usage_error_of(|| {
        parse_cli_args("trace", TRACE_INPUT, &argv(&["--depth", "2"])).map(|_| ())
    });
    let from_call =
        app_error_of(|| parse_args("trace", TRACE_INPUT, &json!({"depth": 2})).map(|_| ()));
    assert_eq!(from_cli.message, from_call.message);
    assert_eq!(
        from_cli.message,
        "trace is missing the required argument \"query\""
    );
    assert_eq!(from_cli.usage, usage_of("trace", TRACE_INPUT));
    assert_eq!(
        from_call.hint.as_deref(),
        Some("\"query\" must be a string")
    );
}

#[test]
fn an_expectation_reports_the_declared_bounds_or_the_safe_integer_pair() {
    let bounded = app_error_of(|| {
        parse_args("trace", TRACE_INPUT, &json!({"query": "q", "depth": "2"})).map(|_| ())
    });
    assert_eq!(
        bounded.hint.as_deref(),
        Some("\"depth\" must be an integer between 1 and 4")
    );
    let unbounded =
        app_error_of(|| parse_args("status", STATUS_INPUT, &json!({"budget": "5"})).map(|_| ()));
    assert_eq!(
        unbounded.hint.as_deref(),
        Some("\"budget\" must be an integer between -9007199254740991 and 9007199254740991")
    );
    // The bounds are a hint, never a refusal: `--depth 99` parsed, and so does a call with 99.
    assert!(
        parse_args("trace", TRACE_INPUT, &json!({"query": "q", "depth": 99})).is_ok(),
        "min/max are the handler's business"
    );
    // The array expectations name the item type rather than the container.
    assert_eq!(
        expectation_of("paths", &STATUS_INPUT[0].1),
        "\"paths\" must be an array of strings"
    );
    assert_eq!(
        expectation_of("range", &EXPAND_INPUT[3].1),
        "\"range\" must be an array of integers"
    );
    assert_eq!(
        expectation_of("kinds", &TRACE_INPUT[2].1),
        "\"kinds\" must be an array of: file, module, class, function, method, test, endpoint"
    );
}

// ---------------------------------------------------------------------------------------------
// cli args: parseArgs is reached, not reimplemented
// ---------------------------------------------------------------------------------------------

#[test]
fn defaults_and_dropped_keys_behave_exactly_as_a_protocol_call() {
    assert_eq!(
        parse_cli_args("index", INDEX_INPUT, &argv(&[])).expect("defaults"),
        parse_args("index", INDEX_INPUT, &json!({})).expect("defaults")
    );
    assert_eq!(
        as_value(parse_cli_args("index", INDEX_INPUT, &argv(&[])).expect("defaults")),
        json!({"force": false, "mode": "full", "register": true})
    );
    assert_eq!(
        as_value(
            parse_cli_args("index", INDEX_INPUT, &argv(&["--no-register"])).expect("the opt-out")
        ),
        json!({"force": false, "mode": "full", "register": false})
    );
    assert_eq!(
        as_value(parse_cli_args("status", STATUS_INPUT, &argv(&[])).expect("defaults")),
        json!({"paths": [], "budget": 800})
    );
    // A key no manifest declares never reaches the handler, whichever layer asks.
    assert_eq!(
        as_value(
            parse_args(
                "search",
                SEARCH_INPUT,
                &json!({"query": "q", "colour": "red"})
            )
            .expect("an undeclared key is dropped")
        ),
        json!({"query": "q", "budget": 2000})
    );
}

#[test]
fn the_supplied_names_are_the_declared_ones_the_caller_wrote_before_defaults() {
    let parsed = parse_cli_args_and_names("status", STATUS_INPUT, &argv(&["--budget", "900"]))
        .expect("one argument");
    assert_eq!(parsed.supplied, ["budget"]);
    assert_eq!(
        as_value(parsed.args),
        json!({"paths": [], "budget": 900}),
        "the default is applied, the supplied set stays a fact about the request"
    );
    let parsed =
        parse_cli_args_and_names("status", STATUS_INPUT, &argv(&[])).expect("no arguments");
    assert!(parsed.supplied.is_empty());
    assert_eq!(
        parsed,
        ParsedCliArgs {
            args: parse_args("status", STATUS_INPUT, &json!({})).expect("defaults"),
            supplied: Vec::new(),
        }
    );
}

// ---------------------------------------------------------------------------------------------
// cli args: usage
// ---------------------------------------------------------------------------------------------

#[test]
fn every_manifest_field_is_named_and_the_global_flags_close_the_block() {
    for (tool, input) in MANIFESTS {
        let usage = usage_of(tool, input);
        let lines: Vec<&str> = usage.split('\n').collect();
        assert!(
            lines[0].starts_with(&format!("sangkar {tool}")),
            "{tool}: {}",
            lines[0]
        );
        assert_eq!(lines.len(), input.len() + 2, "{tool}");
        assert_eq!(
            lines.last().copied(),
            Some(
                "global options: --repo-root <path>, --data-dir <path>, --json (JSON instead of TOON)"
            ),
            "{tool}"
        );
        for (name, _) in input {
            assert!(
                usage.contains(&format!("--{name} <")),
                "{tool} names {name}"
            );
        }
    }
}

#[test]
fn the_header_names_the_positional_the_command_accepts() {
    assert_eq!(
        usage_of("trace", TRACE_INPUT).lines().next(),
        Some("sangkar trace [<query>] [options]")
    );
    assert_eq!(
        usage_of("expand", EXPAND_INPUT).lines().next(),
        Some("sangkar expand [<target>] [options]")
    );
    assert_eq!(
        usage_of("status", STATUS_INPUT).lines().next(),
        Some("sangkar status [options]")
    );
}

#[test]
fn a_required_field_is_marked_and_a_description_is_reproduced() {
    assert!(usage_of("trace", TRACE_INPUT).contains("--query <string> (required)"));
    assert!(
        usage_of("index", INDEX_INPUT).contains(
            "Rebuild from an empty stage and publish even when the file hashes say nothing changed."
        ),
        "an index description is reproduced, so the CLI help and the manifest cannot drift"
    );
    assert_eq!(positional_field_of(TRACE_INPUT), Some("query"));
    assert_eq!(positional_field_of(SUMMARY_INPUT), None);
}

// ---------------------------------------------------------------------------------------------
// verbs: the flag table is data
// ---------------------------------------------------------------------------------------------

#[test]
fn the_verb_table_covers_every_flag_the_typescript_cli_accepts() {
    let names = |verb: Verb| -> Vec<&'static str> {
        verb_spec(verb).flags.iter().map(|flag| flag.name).collect()
    };
    assert!(names(Verb::Mcp).is_empty(), "mcp takes only the globals");
    assert_eq!(names(Verb::Worker), ["mode", "force"]);
    assert_eq!(names(Verb::Index), ["mode", "force", "yes", "no-register"]);
    assert_eq!(
        names(Verb::Telemetry),
        ["out", "serve", "port", "host", "since"]
    );
    assert_eq!(names(Verb::Daemon), ["port", "host"]);
    assert_eq!(names(Verb::Repo), ["watch", "watch-interval", "telemetry"]);
    assert_eq!(
        verb_spec(Verb::Daemon).subcommands,
        ["run", "start", "stop", "status"]
    );
    assert_eq!(
        verb_spec(Verb::Repo).subcommands,
        ["add", "remove", "list", "config"]
    );
    assert!(verb_spec(Verb::Worker).subcommands.is_empty());
    // Every verb name round-trips, which is what dispatch will rely on.
    for verb in Verb::ALL {
        assert_eq!(Verb::from_name(verb.name()), Some(verb));
    }
    assert_eq!(Verb::from_name("status"), None, "a tool is not a verb");
    assert_eq!(VERB_SPECS.len(), Verb::ALL.len());
}

#[test]
fn the_verb_usage_blocks_are_the_typescript_text() {
    // Character for character from `bin/sangkar.ts` (`VERB_USAGE`) and `src/shared/cli/repo.ts`
    // (`USAGE`): a refusal prints the same block `--help` does, and the wording is user-visible.
    assert_eq!(
        verb_spec(Verb::Mcp).usage,
        "usage: sangkar mcp [--repo-root <path>] [--data-dir <path>] [--json]"
    );
    assert_eq!(
        verb_spec(Verb::Worker).usage,
        "usage: sangkar worker [--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>]"
    );
    assert_eq!(
        verb_spec(Verb::Index).usage,
        "usage: sangkar index [--mode full|fast] [--force] [--repo-root <path>] [--data-dir <path>] [--yes]\n  --yes          index the autodetected tree without asking (the answer a script gives)"
    );
    assert_eq!(
        verb_spec(Verb::Telemetry).usage,
        "usage: sangkar telemetry [--out <path>] [--serve] [--port <n>] [--host <addr>] [--since <YYYY-MM-DD|Nd>] [--repo-root <path>] [--data-dir <path>]\n  --serve        serve the report for every registered repository on loopback instead of writing one file\n  --port <n>     the port to bind (default: config.json's daemon.port, else 7317; 0 picks a free port)\n  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)"
    );
    assert_eq!(
        verb_spec(Verb::Daemon).usage,
        "usage: sangkar daemon [run|start|stop|status] [--port <n>] [--host <addr>]\n  run     run the daemon in the foreground until a signal stops it\n  start   start it as a detached child and return once it holds the pid file\n  stop    stop the daemon this home's pid file names\n  status  one header line, then one line per registered repository\n  --port <n>     the report server's port (default: config.json's daemon.port, else 7317; 0 picks a free port)\n  --host <addr>  the address to bind (default: config.json's daemon.host, else 127.0.0.1; loopback only)\n  --repo-root and --data-dir are accepted by every subcommand and change nothing here; --json prints status as one JSON object line, and --port/--host still belong to \"run\"\n  --port and --host belong to \"run\"; start passes neither, so it binds the same defaults"
    );
    // Built with the two constants rather than a literal pair, so the clamp the block advertises and
    // the band the reader enforces cannot drift apart.
    let repo_usage = format!(
        "usage: sangkar repo [add|remove|list|config] <path>\n  add <path>                register a tree and print its canonical path\n  remove <path>             unregister a tree; the tree and its graph are left alone\n  list                      one registered path per line (--json for a JSON document)\n  config <path>             write <path>/.sangkar/config.json and print the effective settings\n  --json                    the registry as {{\"version\":1,\"repos\":[...]}} (list only)\n  --watch <true|false>      whether the daemon watches this tree (config only)\n  --watch-interval <n>      poll period in milliseconds, {WATCH_INTERVAL_MIN_MS}..{WATCH_INTERVAL_MAX_MS} (config only)\n  --telemetry <true|false>  whether this tree's usage may be recorded (config only)\n  --repo-root, --data-dir and --json are accepted by every subcommand; only list changes with --json\n  the <path> may follow the flags, and no token starting with -- is ever read as a path\n  a relative <path> resolves against the current directory; a leading ~/ is expanded"
    );
    assert_eq!(verb_spec(Verb::Repo).usage, repo_usage);
}

#[test]
fn every_worker_and_index_flag_parses_in_its_declared_type() {
    let worker =
        parse_verb_args(Verb::Worker, &argv(&["--mode", "fast", "--force"])).expect("worker flags");
    assert_eq!(worker.subcommand, None);
    assert_eq!(worker.text("mode"), Some("fast"));
    assert!(worker.switch("force"));
    let worker = parse_verb_args(Verb::Worker, &argv(&["--mode=full"])).expect("an inline mode");
    assert_eq!(worker.text("mode"), Some("full"));
    assert!(!worker.switch("force"));

    let index = parse_verb_args(Verb::Index, &argv(&["--yes", "--no-register", "--force"]))
        .expect("index flags");
    assert!(index.switch("yes"), "--yes is the verb's own flag");
    assert!(index.switch("no-register"));
    assert!(index.switch("force"));
    assert_eq!(index.value("yes"), Some(&ArgValue::Bool(true)));
}

#[test]
fn yes_on_index_is_accepted_and_refused_on_worker() {
    assert!(parse_verb_args(Verb::Index, &argv(&["--yes"])).is_ok());
    let refusal = usage_error_of(|| parse_verb_args(Verb::Worker, &argv(&["--yes"])).map(|_| ()));
    assert_eq!(refusal.message, "unknown option \"--yes\"");
    assert_eq!(refusal.usage, verb_spec(Verb::Worker).usage);
}

#[test]
fn every_telemetry_flag_parses_in_its_declared_type() {
    let args = parse_verb_args(
        Verb::Telemetry,
        &argv(&[
            "--out",
            "report.html",
            "--serve",
            "--port",
            "7317",
            "--host",
            "127.0.0.1",
            "--since",
            "7d",
        ]),
    )
    .expect("telemetry flags");
    assert_eq!(args.text("out"), Some("report.html"));
    assert!(args.switch("serve"));
    assert_eq!(args.integer("port"), Some(7317));
    assert_eq!(args.text("host"), Some("127.0.0.1"));
    assert_eq!(args.text("since"), Some("7d"));
    // The port is an integer, not the text `flagValue` would hand over.
    assert_eq!(args.value("port"), Some(&ArgValue::Integer(7317)));
}

#[test]
fn daemon_takes_a_subcommand_and_its_flags_only_on_run() {
    let args = parse_verb_args(
        Verb::Daemon,
        &argv(&["run", "--port", "0", "--host", "127.0.0.1"]),
    )
    .expect("daemon run flags");
    assert_eq!(args.subcommand, Some("run"));
    assert_eq!(args.integer("port"), Some(0), "0 picks a free port");
    assert_eq!(args.text("host"), Some("127.0.0.1"));

    for subcommand in ["start", "stop", "status"] {
        let refusal = usage_error_of(|| {
            parse_verb_args(Verb::Daemon, &argv(&[subcommand, "--port", "7317"])).map(|_| ())
        });
        assert_eq!(
            refusal.message,
            format!("\"daemon {subcommand}\" takes no --port")
        );
        assert_eq!(refusal.usage, verb_spec(Verb::Daemon).usage);
    }
    let refusal = usage_error_of(|| parse_verb_args(Verb::Daemon, &argv(&[])).map(|_| ()));
    assert_eq!(refusal.message, "daemon needs a subcommand");
    let refusal = usage_error_of(|| parse_verb_args(Verb::Daemon, &argv(&["restart"])).map(|_| ()));
    assert_eq!(refusal.message, "unknown subcommand \"restart\"");
    assert_eq!(refusal.usage, verb_spec(Verb::Daemon).usage);
}

#[test]
fn repo_takes_a_subcommand_a_path_and_its_flags_only_on_config() {
    let args = parse_verb_args(
        Verb::Repo,
        &argv(&[
            "config",
            "--watch",
            "false",
            "--watch-interval",
            "2000",
            "--telemetry",
            "true",
            "./",
        ]),
    )
    .expect("repo config flags");
    assert_eq!(args.subcommand, Some("config"));
    assert_eq!(args.boolean("watch"), Some(false));
    assert_eq!(args.integer("watch-interval"), Some(2000));
    assert_eq!(args.boolean("telemetry"), Some(true));
    assert_eq!(args.positional(), Some("./"));

    let args = parse_verb_args(Verb::Repo, &argv(&["add", "./"])).expect("a path");
    assert_eq!(args.subcommand, Some("add"));
    assert_eq!(args.positional(), Some("./"));
    // The path may follow the flags, and the flag's value is never read as the path.
    let args = parse_verb_args(Verb::Repo, &argv(&["config", "--watch", "true", "."]))
        .expect("the path after a flag");
    assert_eq!(args.positional(), Some("."));
    assert_eq!(args.boolean("watch"), Some(true));

    let refusal = usage_error_of(|| {
        parse_verb_args(Verb::Repo, &argv(&["add", "./", "--watch", "true"])).map(|_| ())
    });
    assert_eq!(refusal.message, "\"repo add\" takes no --watch");
    assert_eq!(refusal.usage, verb_spec(Verb::Repo).usage);
    let refusal = usage_error_of(|| {
        parse_verb_args(Verb::Repo, &argv(&["config", "./", "--watch", "maybe"])).map(|_| ())
    });
    assert_eq!(
        refusal.message,
        "--watch must be true or false, not \"maybe\""
    );
    let refusal =
        usage_error_of(|| parse_verb_args(Verb::Repo, &argv(&["config", "--watch"])).map(|_| ()));
    assert_eq!(refusal.message, "--watch needs true or false");
    // A negative spelling is only a switch for a switch; `--watch` takes a value.
    let refusal = usage_error_of(|| {
        parse_verb_args(Verb::Repo, &argv(&["config", "--no-watch"])).map(|_| ())
    });
    assert_eq!(refusal.message, "unknown option \"--no-watch\"");
}

#[test]
fn a_switch_negation_sets_a_verb_flag_false() {
    let args = parse_verb_args(Verb::Worker, &argv(&["--no-force"])).expect("a negated switch");
    assert_eq!(args.boolean("force"), Some(false));
    let args = parse_verb_args(Verb::Worker, &argv(&["--force=false"])).expect("an inline false");
    assert_eq!(args.boolean("force"), Some(false));
    let refusal =
        usage_error_of(|| parse_verb_args(Verb::Worker, &argv(&["--force=maybe"])).map(|_| ()));
    assert_eq!(refusal.message, "--force must be true or false");
}

#[test]
fn a_verb_refuses_a_malformed_value_in_its_declared_type() {
    let refusal =
        usage_error_of(|| parse_verb_args(Verb::Telemetry, &argv(&["--port", "soon"])).map(|_| ()));
    assert_eq!(refusal.message, "--port must be an integer");
    let refusal =
        usage_error_of(|| parse_verb_args(Verb::Telemetry, &argv(&["--out"])).map(|_| ()));
    assert_eq!(refusal.message, "--out needs a value");
    let refusal =
        usage_error_of(|| parse_verb_args(Verb::Worker, &argv(&["--mode", "partial"])).map(|_| ()));
    assert_eq!(refusal.message, "--mode must be one of: full, fast");
}

#[test]
fn every_verb_accepts_the_four_globals_on_either_side() {
    let invocation = parse_verb(
        Verb::Telemetry,
        &argv(&["--json", "--repo-root", "/r", "--out", "x", "--data-dir=/d"]),
    )
    .expect("globals on both sides of the verb's own flags");
    assert!(invocation.globals.json);
    assert_eq!(invocation.args.text("out"), Some("x"));
    assert_eq!(
        invocation.globals.rest,
        ["--out", "x"],
        "the verb's own tokens are what the globals were removed from"
    );

    let invocation = parse_verb(Verb::Index, &argv(&["--yes", "--json"]))
        .expect("a global after the verb's own flag");
    assert!(invocation.globals.json);
    assert!(invocation.args.switch("yes"));

    // `--help` is read by `split_globals` and answered by the caller before the tail is validated.
    assert!(
        split_globals(&argv(&["--help", "--out"]))
            .expect("a switch")
            .help,
        "a verb answers help without satisfying its grammar"
    );
}

#[test]
fn a_dangling_global_on_a_verb_line_carries_the_verbs_own_block() {
    let refusal = usage_error_of(|| parse_verb(Verb::Repo, &argv(&["--repo-root"])).map(|_| ()));
    assert_eq!(refusal.message, "--repo-root needs a value");
    assert_eq!(
        refusal.usage,
        verb_spec(Verb::Repo).usage,
        "the refusal points at the verb the user typed, not at the tool surface"
    );
}

// ---------------------------------------------------------------------------------------------
// render: the shared envelope fixtures
// ---------------------------------------------------------------------------------------------

const FRESH_GENERATION: &str = "g-7f3a91";
const FRESH_INDEXED_AT: &str = "2026-11-17T13:50:15Z";

/// The `status` tool's own summary shape, so the encoding is proven on real wording.
const SUMMARY: &str = "530 files indexed, generation g-7f3a91. Whole repository.";

/// A cursor as the tools spell it: opaque, base64url, and never quoted by the encoder.
const CURSOR: &str = "eyJyZXBvIjoiL3NyYyIsIm9mZiI6MjB9";

/// A payload whose content is the worst case for a line- and comma-oriented notation.
const RAW_SOURCE: &str = "export function f(a, b) { return [a, \"b\"] | c; }\n// the second line \
                           carries a newline into the value";

fn fresh() -> ReadStatus {
    ReadStatus::new(FRESH_GENERATION, FRESH_INDEXED_AT)
}

fn clean() -> Coverage {
    Coverage::complete(530)
}

fn spec(summary: &str) -> EnvelopeSpec {
    EnvelopeSpec::new(summary, fresh(), clean())
}

/// The payload `status` produces, field for field, so the encoding is proven on a real shape.
fn status_payload() -> Value {
    json!({
        "project": {
            "name": "sangkar-mcp",
            "root": "/src/sangkar-mcp",
            "commit": "ce28fe2",
            "db_path": "/data/graph.db",
        },
        "graph": {
            "nodes": 530,
            "edges": 1188,
            "size_bytes": 1234567,
            "db_path": "/data/graph.db",
        },
        "languages": [{"name": "typescript", "files": 41, "symbols": 530}],
        "paths": [],
        "health": {
            "indexer": "idle",
            "last_publish_ms": 1200,
            "consecutive_failures": 0,
            "statement_cache_entries": 3,
        },
    })
}

/// Two fields, one row per item: the shape TOON writes as a table.
fn uniform_skip_list() -> Value {
    json!([
        {"path": "src/a.ts", "reason": "not indexed"},
        {"path": "src/b.ts", "reason": "skipped, \"quoted\""},
    ])
}

/// Every character class an encoder has to escape, plus nulls and an empty object.
fn adversarial_payload() -> Value {
    json!({
        "results": uniform_skip_list(),
        "source": RAW_SOURCE,
        "separators": "a, b | c { brace } [ bracket ] \"quoted\"",
        "skipped": [Value::Null, json!({"path": "src/c.ts", "reason": "binary"}), Value::Null],
        "empty": {},
        "summary": "x".repeat(600),
    })
}

fn status_envelope() -> Envelope<Value> {
    ok(status_payload(), spec(SUMMARY))
}

/// `bootstrapping` needs its retry window; everything else builds through the plain constructors.
fn all_statuses() -> Vec<(&'static str, Envelope<Value>)> {
    vec![
        ("ok", status_envelope()),
        (
            "ambiguous",
            ambiguous(json!({"candidates": []}), spec("two matches")),
        ),
        (
            "not_found",
            not_found(json!({"near_misses": []}), spec("no such symbol")),
        ),
        (
            "error",
            fail(AppError::internal("store refused"), spec("store refused")),
        ),
        (
            "bootstrapping",
            bootstrapping(BootstrappingSpec::new(spec("indexing"), 900)),
        ),
    ]
}

fn envelope_for(status: &str) -> Envelope<Value> {
    all_statuses()
        .into_iter()
        .find(|(name, _)| *name == status)
        .map(|(_, envelope)| envelope)
        .unwrap_or_else(|| panic!("no fixture for {status}"))
}

/// The body of a rendering, with its one trailing newline proven first: a line stream that ended in
/// two newlines would hand a decoder an empty last line, and a caller reading stdout line by line
/// would see a blank answer where there is none.
fn body_of(rendered: &str) -> &str {
    assert!(rendered.ends_with('\n'), "a rendering ends in one newline");
    assert!(
        !rendered[..rendered.len() - 1].ends_with('\n'),
        "and not two"
    );
    &rendered[..rendered.len() - 1]
}

/// What a consumer does with the default rendering: read it back as the same JSON value.
///
/// The decode lands on `Value`, not on `Envelope<T>`, because `serde_toon_format` 0.2.0's decoder
/// forwards `deserialize_enum` to `deserialize_any` and therefore cannot read a unit-variant enum
/// from its string spelling (`status: ok`). That is the decoder's limitation and not the encoder's:
/// the product only ever writes TOON, and the JSON data model a decode produces is the contract the
/// format makes — the same comparison `tests/cli-render.test.ts` makes with `decode()`.
fn decoded(rendered: &str) -> Value {
    serde_toon::from_str(body_of(rendered)).expect("the rendering decodes")
}

/// An envelope, typed as the value a JSON reader speaks: what a round trip is compared against.
fn as_json<T: Serialize>(envelope: &Envelope<T>) -> Value {
    serde_json::to_value(envelope).expect("the envelope serialises")
}

// ---------------------------------------------------------------------------------------------
// renderJson: one line of JSON
// ---------------------------------------------------------------------------------------------

#[test]
fn the_json_output_parses_back_to_the_envelope_it_was_given() {
    let envelope = status_envelope();
    let rendered = render_json(&envelope).expect("a renderable envelope");
    assert_eq!(
        serde_json::from_str::<Value>(&rendered).expect("one JSON document"),
        as_json(&envelope)
    );
}

#[test]
fn the_json_output_has_only_the_trailing_newline() {
    body_of(&render_json(&status_envelope()).expect("a renderable envelope"));
}

#[test]
fn an_envelope_with_no_payload_still_renders_compact_json() {
    let envelope: Envelope<Value> =
        fail(AppError::internal("store refused"), spec("store refused"));
    let output = render_json(&envelope).expect("a renderable envelope");
    assert_eq!(
        output,
        format!(
            "{}\n",
            serde_json::to_string(&envelope).expect("compact JSON")
        )
    );
    assert_eq!(
        serde_json::from_str::<Value>(&output).expect("one JSON document"),
        as_json(&envelope)
    );
}

#[test]
fn a_payload_keeps_the_order_of_its_keys() {
    // `preserve_order` is the load-bearing half of the JSON pin: a sorted map would answer with the
    // same data and different bytes, which is what a caller diffing two captures sees.
    let envelope = ok(json!({"z": 1, "a": 2, "m": 3}), spec(SUMMARY));
    let rendered = render_json(&envelope).expect("a renderable envelope");
    let z = rendered.find("\"z\"").expect("z is present");
    let a = rendered.find("\"a\"").expect("a is present");
    let m = rendered.find("\"m\"").expect("m is present");
    assert!(z < a && a < m, "insertion order survives: {rendered}");
}

// ---------------------------------------------------------------------------------------------
// renderToon: the default rendering
// ---------------------------------------------------------------------------------------------

#[test]
fn a_status_envelope_decodes_back_to_the_value_it_was_given() {
    let envelope = status_envelope();
    let rendered = render_toon(&envelope).expect("a renderable envelope");
    assert_eq!(decoded(&rendered), as_json(&envelope));
}

#[test]
fn the_toon_output_has_only_the_trailing_newline_whatever_the_payload_holds() {
    // A newline inside a value is escaped, so the line count of the rendering stays one answer.
    let envelope = ok(adversarial_payload(), spec(SUMMARY));
    body_of(&render_toon(&envelope).expect("a renderable envelope"));
}

#[test]
fn adversarial_content_survives_the_toon_round_trip_unchanged() {
    let envelope = ok(
        adversarial_payload(),
        EnvelopeSpec {
            next: Some(CURSOR.to_owned()),
            ..spec(SUMMARY)
        },
    );
    assert_eq!(
        decoded(&render_toon(&envelope).expect("a renderable envelope")),
        as_json(&envelope)
    );
}

#[test]
fn a_null_element_a_null_payload_and_an_empty_payload_all_round_trip() {
    // Three absences the envelope contract distinguishes: "no elements that are null", "no item at
    // all" and "an item whose fields are all absent".
    let with_nulls = ok(
        json!({"items": [Value::Null, json!({"path": "a.ts"}), Value::Null]}),
        spec(SUMMARY),
    );
    assert_eq!(
        decoded(&render_toon(&with_nulls).expect("a renderable envelope")),
        as_json(&with_nulls)
    );

    let no_payload: Envelope<Value> =
        fail(AppError::internal("store refused"), spec("store refused"));
    assert!(no_payload.payload.is_none());
    assert_eq!(
        decoded(&render_toon(&no_payload).expect("a renderable envelope")),
        as_json(&no_payload)
    );

    let empty_payload = ok(json!({}), spec(SUMMARY));
    assert_eq!(
        decoded(&render_toon(&empty_payload).expect("a renderable envelope")),
        as_json(&empty_payload)
    );
}

#[test]
fn a_long_summary_is_not_wrapped_into_a_second_line() {
    let summary = "y".repeat(2_000);
    let envelope = ok(json!({}), spec(&summary));
    let rendered = render_toon(&envelope).expect("a renderable envelope");
    assert_eq!(
        decoded(&rendered)["summary"].as_str(),
        Some("y".repeat(2_000).as_str())
    );
    body_of(&rendered);
}

#[test]
fn a_uniform_array_renders_as_one_table_header_and_one_row_per_item() {
    let envelope = ok(json!({"results": uniform_skip_list()}), spec(SUMMARY));
    let rendered = render_toon(&envelope).expect("a renderable envelope");
    let body = body_of(&rendered);
    let lines: Vec<&str> = body.lines().collect();
    // The table form is what makes a uniform list cheap: the field names appear once, not per row.
    let headers: Vec<&&str> = lines
        .iter()
        .filter(|line| line.trim_start() == "results[2]{path,reason}:")
        .collect();
    assert_eq!(headers.len(), 1, "one table header in:\n{body}");
    let header = headers[0];
    let indent = header.len() - header.trim_start().len();
    let rows: Vec<&&str> = lines
        .iter()
        .filter(|line| line.trim_start().starts_with("src/"))
        .collect();
    assert_eq!(rows.len(), 2, "one row per item in:\n{body}");
    for row in rows {
        assert!(
            row.len() - row.trim_start().len() > indent,
            "a row is indented under its header: {row}"
        );
    }
    // The comma inside the second row's reason is quoted rather than splitting that row into a third
    // cell.
    assert!(body.contains("src/a.ts,not indexed"));
}

#[test]
fn every_status_encodes_and_decodes_to_itself() {
    for (status, envelope) in all_statuses() {
        let rendered = render_toon(&envelope).expect("a renderable envelope");
        assert_eq!(
            decoded(&rendered),
            as_json(&envelope),
            "{status} did not round-trip"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// renderOutput: one envelope, two renderings
// ---------------------------------------------------------------------------------------------

#[test]
fn the_json_flag_picks_the_form() {
    let envelope = status_envelope();
    assert_eq!(
        render_output(&envelope, true).expect("JSON"),
        render_json(&envelope).expect("JSON")
    );
    assert_eq!(
        render_output(&envelope, false).expect("TOON"),
        render_toon(&envelope).expect("TOON")
    );
}

#[test]
fn both_forms_carry_the_same_envelope() {
    let envelope = status_envelope();
    let toon = render_output(&envelope, false).expect("TOON");
    let json = render_output(&envelope, true).expect("JSON");
    assert_eq!(
        decoded(&toon),
        serde_json::from_str::<Value>(&json).expect("JSON")
    );
}

// ---------------------------------------------------------------------------------------------
// mcpText: the protocol result, no trailing newline
// ---------------------------------------------------------------------------------------------

#[test]
fn the_default_mcp_text_is_toon_and_parses_to_the_envelope() {
    let envelope = status_envelope();
    let text = mcp_text(&envelope, false).expect("TOON");
    // A protocol result is one string field, not a line stream: the newline `render_toon` adds would
    // be part of the answer text.
    assert!(!text.ends_with('\n'));
    assert_eq!(
        serde_toon::from_str::<Value>(&text).expect("TOON decodes"),
        as_json(&envelope)
    );
}

#[test]
fn the_json_mcp_text_is_json_still_without_a_newline() {
    let envelope = status_envelope();
    let text = mcp_text(&envelope, true).expect("JSON");
    assert!(!text.ends_with('\n'));
    assert_eq!(
        text,
        serde_json::to_string(&envelope).expect("compact JSON")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&text).expect("one JSON document"),
        as_json(&envelope)
    );
}

// ---------------------------------------------------------------------------------------------
// The JSON byte-identity proof: the golden was captured from the TypeScript CLI with `--json`
// ---------------------------------------------------------------------------------------------

/// `scripts/parity/capture-goldens.ts`'s capture of `sangkar status --json` on the polyglot fixture.
const STATUS_GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tests/golden/polyglot/status.json"
);

/// The envelope a golden's `stdout` carries, read back through the Rust contract type.
fn golden_envelope() -> (String, Envelope<Value>) {
    let captured = fs::read_to_string(STATUS_GOLDEN).expect("the captured status golden");
    let captured: Value = serde_json::from_str(&captured).expect("the golden is a JSON document");
    let stdout = captured["stdout"]
        .as_str()
        .expect("the golden captured stdout")
        .to_owned();
    assert_eq!(
        captured["args"],
        json!([]),
        "the status golden was captured with the globals and no tool argument"
    );
    assert_eq!(captured["exit_code"], json!(0));
    let envelope = serde_json::from_str(&stdout).expect("the golden's stdout is an envelope");
    (stdout, envelope)
}

#[test]
fn the_json_rendering_of_the_status_golden_is_byte_identical() {
    let (stdout, envelope) = golden_envelope();
    // The capture wrote `JSON.stringify(envelope)` and the CLI appended one newline; a renderer that
    // re-ordered `totals`, wrote an absent `stale_reason` as `null` or ended in two newlines answers
    // a different byte string and fails here.
    assert_eq!(
        render_json(&envelope).expect("the golden envelope renders"),
        stdout,
        "render_json must reproduce the TypeScript `--json` stdout byte for byte"
    );
    assert!(
        stdout.ends_with('\n'),
        "the golden was captured with --json"
    );
    assert!(!stdout[..stdout.len() - 1].ends_with('\n'));
}

#[test]
fn the_toon_rendering_of_the_status_golden_decodes_back_to_it() {
    let (_, envelope) = golden_envelope();
    assert_eq!(
        decoded(&render_toon(&envelope).expect("the golden envelope renders")),
        as_json(&envelope),
        "the default rendering carries the same envelope the JSON one does"
    );
    assert_eq!(
        mcp_text(&envelope, true).expect("JSON"),
        serde_json::to_string(&envelope).expect("compact JSON"),
        "the protocol text is the same bytes without the newline"
    );
}

// ---------------------------------------------------------------------------------------------
// exitCodeOf: five statuses, two codes
// ---------------------------------------------------------------------------------------------

#[test]
fn ok_ambiguous_and_not_found_exit_zero() {
    for status in ["ok", "ambiguous", "not_found"] {
        assert_eq!(exit_code_of(&envelope_for(status)), 0, "{status}");
    }
}

#[test]
fn error_and_bootstrapping_exit_exit_failure() {
    for status in ["error", "bootstrapping"] {
        assert_eq!(
            exit_code_of(&envelope_for(status)),
            EXIT_FAILURE,
            "{status}"
        );
    }
}

#[test]
fn the_codes_are_the_documented_pair() {
    assert_eq!(EXIT_FAILURE, 1);
    assert_eq!(EXIT_USAGE, 2);
    // `70` belongs to the npm launcher shim, never to an envelope.
    for status in ["ok", "ambiguous", "not_found", "error", "bootstrapping"] {
        let code = exit_code_of(&envelope_for(status));
        assert!(
            code == 0 || code == EXIT_FAILURE,
            "{status} answered {code}"
        );
    }
}
