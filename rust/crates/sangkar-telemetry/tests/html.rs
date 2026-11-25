//! The rendered page, read as text: the panel order the contract fixes, the escaping a hostile record
//! field depends on, the number formats the TypeScript owns, and the machine-wide page's one section
//! per repository.
//!
//! The fixture is a [`ReportData`] literal rather than JSONL lines: the reader and the fold have their
//! own suite (`tests/report.rs`), so what is under test here is one function of one aggregate in, one
//! document out.

use std::collections::BTreeMap;

use sangkar_telemetry::html::{escape_html, render_body, render_html};
use sangkar_telemetry::html_multi::{ReportSection, render_multi_page, render_multi_report};
use sangkar_telemetry::report::{
    ArgNameCount, Coverage, DurationStats, IndexRunRow, Relations, ReportData, SessionBreakdown,
    ToolStats, Totals, TrendPoint, TruncationSummary, Window, aggregate,
};

/// One clock reading, supplied rather than taken: the renderer must never read the clock itself.
const GENERATED_AT: &str = "2026-11-18T10:00:00.000Z";

/// A tool name that closes the script tag holding the data blob, then opens a live element.
const HOSTILE_TOOL: &str = "</script><img onerror=\"alert('x')\">";
/// An argument name with markup in it, which the argument-names panel interpolates unquoted.
const HOSTILE_ARG: &str = "<b>depth</b>";

/// The panel order the contract fixes: the header, then these eight sections, and `empty` only for an
/// empty window.
const POPULATED_ORDER: [&str; 9] = [
    "header",
    "tools",
    "truncation",
    "relations",
    "coverage",
    "argument-names",
    "trend",
    "index-runs",
    "sessions",
];

fn mix(pairs: &[(&str, u64)]) -> BTreeMap<String, u64> {
    pairs
        .iter()
        .map(|(key, count)| ((*key).to_owned(), *count))
        .collect()
}

// -------------------------------------------------------------------------------------------
// Fixtures
// -------------------------------------------------------------------------------------------

/// One populated window with every panel exercised, one hostile tool and one hostile argument name.
fn fixture() -> ReportData {
    ReportData {
        totals: Totals {
            records: 9,
            calls: 6,
            index_runs: 2,
            sessions: 1,
            unparsed: 0,
            install_ids: 1,
            days: 2,
        },
        tools: vec![
            ToolStats {
                tool: "status".to_owned(),
                calls: 1_234,
                statuses: mix(&[("error", 4), ("ok", 1_230)]),
                error_codes: mix(&[("E_TIMEOUT", 4)]),
                duration_ms: DurationStats {
                    p50: 12.0,
                    p95: 40.0,
                    mean: 12.3456,
                    max: 87.0,
                },
                truncated: 0,
                mean_budget_used_ratio: Some(0.5),
                eq: 1_000,
                gte: 234,
            },
            ToolStats {
                tool: HOSTILE_TOOL.to_owned(),
                calls: 2,
                statuses: mix(&[("ok", 2)]),
                error_codes: BTreeMap::new(),
                duration_ms: DurationStats {
                    p50: 1.5,
                    p95: 2.0,
                    mean: 1.75,
                    max: 2.0,
                },
                truncated: 1,
                mean_budget_used_ratio: None,
                eq: 2,
                gte: 0,
            },
        ],
        status_mix: mix(&[("error", 1), ("ok", 5)]),
        error_mix: mix(&[("E_TIMEOUT", 1)]),
        truncation: TruncationSummary {
            count: 1,
            rate: 0.1667,
            reasons: mix(&[("budget", 1)]),
        },
        relations: Relations {
            eq: 1_002,
            gte: 234,
            // A quarter is the one ratio whose hundredth lands exactly on a half, so the panel proves
            // `toFixed`'s round-half-up rather than Rust's round-half-to-even.
            eq_ratio: 0.125,
        },
        coverage: Coverage {
            stale_calls: 1,
            mean_unresolved_edges: 1.25,
        },
        arg_names: vec![
            ArgNameCount {
                name: "depth".to_owned(),
                calls: 5,
            },
            ArgNameCount {
                name: HOSTILE_ARG.to_owned(),
                calls: 1,
            },
        ],
        index_runs: vec![
            IndexRunRow {
                ts: "2026-11-17T09:00:00.000Z".to_owned(),
                mode: "full".to_owned(),
                exit_code: 0,
                duration_ms: 1_234.0,
                files_seen: 100,
                files_indexed: 90,
                files_skipped: 9,
                files_failed: 1,
                files_changed_in_run: 90,
                unresolved_ratio: 0.5,
                db_size_bytes: 2_500_000,
                generation_advanced: true,
            },
            IndexRunRow {
                ts: "2026-11-18T09:00:00.000Z".to_owned(),
                mode: "incremental".to_owned(),
                exit_code: 1,
                duration_ms: 99.0,
                files_seen: 10,
                files_indexed: 9,
                files_skipped: 1,
                files_failed: 0,
                files_changed_in_run: 9,
                unresolved_ratio: 0.25,
                db_size_bytes: 2_600_000,
                generation_advanced: false,
            },
        ],
        unresolved_trend: vec![
            TrendPoint {
                date: "2026-11-17".to_owned(),
                ratio: 0.5,
                runs: 1,
            },
            TrendPoint {
                date: "2026-11-18".to_owned(),
                ratio: 0.25,
                runs: 1,
            },
        ],
        sessions: SessionBreakdown {
            by_mode: mix(&[("stdio", 1)]),
            by_root_kind: mix(&[("repo", 1)]),
            by_size_bucket: mix(&[("small", 1)]),
        },
        window: Window {
            since: Some("7d".to_owned()),
            first_ts: Some("2026-11-17T09:00:00.000Z".to_owned()),
            last_ts: Some("2026-11-18T10:00:00.000Z".to_owned()),
        },
    }
}

// -------------------------------------------------------------------------------------------
// Readers
// -------------------------------------------------------------------------------------------

/// Every panel opening tag in document order: the header is `header`, a section is its `id`.
fn panel_order(html: &str) -> Vec<String> {
    let mut found: Vec<(usize, String)> = Vec::new();
    let mut cursor = 0;
    const OPEN: &str = "<section id=\"";
    while let Some(offset) = html[cursor..].find(OPEN) {
        let start = cursor + offset + OPEN.len();
        let end = start + html[start..].find('"').expect("a section id is closed");
        found.push((cursor + offset, html[start..end].to_owned()));
        cursor = end;
    }
    if let Some(offset) = html.find("<header class=\"panel\">") {
        found.push((offset, String::from("header")));
    }
    found.sort_by_key(|(offset, _)| *offset);
    found.into_iter().map(|(_, id)| id).collect()
}

/// The slice from one panel's opening tag to its own closing tag, so an assertion cannot be
/// satisfied by a different panel's number and a nested section is included.
fn panel<'a>(html: &'a str, id: &str) -> &'a str {
    let opener = format!("<section id=\"{id}\" class=\"panel\"");
    let start = html.find(&opener).expect("the panel is on the page");
    let mut depth = 1_usize;
    let mut cursor = start + opener.len();
    while depth > 0 {
        let next_open = html[cursor..]
            .find("<section ")
            .map(|offset| cursor + offset);
        let next_close = html[cursor..]
            .find("</section>")
            .map(|offset| cursor + offset);
        match (next_open, next_close) {
            (Some(open), Some(close)) if open < close => {
                depth += 1;
                cursor = open + "<section ".len();
            }
            (_, Some(close)) => {
                depth -= 1;
                cursor = close + "</section>".len();
            }
            (Some(open), None) => {
                depth += 1;
                cursor = open + "<section ".len();
            }
            (None, None) => return &html[start..],
        }
    }
    &html[start..cursor]
}

/// The top-level panels only: a repository section carries `render_body`'s nested section ids inside
/// it, and those are not sections of the machine-wide page.
fn top_level_panels(html: &str) -> Vec<String> {
    panel_order(html)
        .into_iter()
        .filter(|id| id == "machine" || id.starts_with("repo-"))
        .collect()
}

/// The six characters the TypeScript's `escapeHtml` replaces, in the order that cannot double-escape.
#[test]
fn escape_html_covers_the_five_characters() {
    assert_eq!(
        escape_html("&<>\"'"),
        "&amp;&lt;&gt;&quot;&#39;",
        "each of the five is replaced, `&` first so the others are not escaped twice"
    );
    assert_eq!(escape_html("sangkar telemetry"), "sangkar telemetry");
}

// -------------------------------------------------------------------------------------------
// The single-repository page
// -------------------------------------------------------------------------------------------

#[test]
fn panel_ids_follow_the_documented_order() {
    let page = render_html(&fixture(), GENERATED_AT, Some("7d"));

    assert_eq!(
        panel_order(&page),
        POPULATED_ORDER,
        "the header comes first, then the eight panels in their documented order"
    );
    assert!(
        !page.contains("<section id=\"empty\""),
        "a window with records has no empty panel"
    );
    assert!(page.contains("<h1>sangkar telemetry</h1>"));
    assert!(page.contains("window <strong>7d</strong>"));
    assert!(page.contains("generated <span>2026-11-18T10:00:00.000Z</span>"));
}

#[test]
fn render_body_is_the_panels_without_the_document() {
    let body = render_body(&fixture());

    assert!(
        body.starts_with("<section id=\"tools\" class=\"panel\">"),
        "the body opens on the first panel"
    );
    assert!(
        !body.contains("<html") && !body.contains("<style>"),
        "the document and its stylesheet are the wrapper's job"
    );
    assert_eq!(panel_order(&body), POPULATED_ORDER[1..].to_vec());
}

#[test]
fn an_empty_window_appends_the_empty_panel_last() {
    let page = render_html(&aggregate(&[]), GENERATED_AT, None);

    assert_eq!(
        panel_order(&page),
        [
            "header",
            "tools",
            "truncation",
            "relations",
            "coverage",
            "argument-names",
            "trend",
            "index-runs",
            "sessions",
            "empty",
        ],
        "the empty panel is appended after the eight tables"
    );
    assert!(page.contains("<h2>nothing recorded yet</h2>"));
    assert!(
        page.contains("window <strong>all records</strong>"),
        "no `--since` is the whole directory, and the label says so"
    );
    assert!(
        page.contains("<div><dt>records</dt><dd>0</dd></div>"),
        "the totals still state zero rather than disappearing"
    );
    assert!(page.contains("no calls in this window"));
}

#[test]
fn a_hostile_tool_name_is_escaped_and_never_becomes_markup() {
    let page = render_html(&fixture(), GENERATED_AT, Some("7d"));

    assert!(page.contains("&lt;/script&gt;&lt;img onerror=&quot;alert(&#39;x&#39;)&quot;&gt;"));
    assert!(
        !page.contains("<img"),
        "a tool name must not open an element"
    );
    assert_eq!(
        page.matches("<script>").count(),
        1,
        "the trend chart's script tag is the only one"
    );
    assert_eq!(
        page.matches("</script>").count(),
        1,
        "a tool named `</script>` must not close the tag that carries the data"
    );
    assert!(
        page.contains("\\u003c/script>"),
        "the embedded data blob spells `<` as `\\u003c`"
    );
    assert!(
        !page.contains("</script><"),
        "the data blob cannot end the script element early"
    );
    assert!(
        page.contains("&lt;b&gt;depth&lt;/b&gt;"),
        "an argument name is escaped too"
    );
}

#[test]
fn numbers_use_the_typescript_formats() {
    let page = render_html(&fixture(), GENERATED_AT, Some("7d"));
    let tools = panel(&page, "tools");
    let relations = panel(&page, "relations");
    let runs = panel(&page, "index-runs");

    assert!(
        tools.contains("<td class=\"num\">1,234</td>"),
        "fmtInt groups thousands: {tools}"
    );
    assert!(
        tools.contains("<td class=\"num\">12.35</td>"),
        "a mean duration is two decimals"
    );
    assert!(
        tools.contains("<td class=\"num\">12</td>"),
        "p50 is truncated to an integer, not rounded"
    );
    assert!(
        tools.contains("<td class=\"num\">—</td>"),
        "a missing budget ratio is an em dash, not 0.00"
    );
    assert!(tools.contains("<td class=\"num\">1.75</td>"));
    assert!(
        relations.contains("<td class=\"num\">0.13</td>"),
        "0.125 rounds up as `toFixed` does, not to 0.12 as Rust's half-to-even would"
    );
    assert!(
        runs.contains("<td class=\"num\">2,500,000</td>"),
        "db bytes are grouped"
    );
}

#[test]
fn the_page_is_one_stylesheet_with_no_external_asset() {
    let page = render_html(&fixture(), GENERATED_AT, Some("7d"));

    assert_eq!(page.matches("<style>").count(), 1);
    assert!(page.contains("@media (prefers-color-scheme: dark)"));
    assert!(page.contains("--accent: #d98a5c;"));
    for absent in ["http://", "https://", "<link", "@import", "url(", "src="] {
        assert!(
            !page.contains(absent),
            "a report carried on a USB stick cannot reference {absent}"
        );
    }
    assert!(page.ends_with("</html>\n"));
}

#[test]
fn the_fixture_page_stays_under_the_size_bound() {
    let size = render_html(&fixture(), GENERATED_AT, Some("7d")).len();

    assert!(
        size < 200 * 1024,
        "the fixture page is {size} bytes, the bound is 204800"
    );
}

// -------------------------------------------------------------------------------------------
// The machine-wide page
// -------------------------------------------------------------------------------------------

#[test]
fn a_section_is_labelled_with_its_basename_and_its_root() {
    let section = ReportSection::new(
        "/work/alpha",
        "/work/alpha/.sangkar/telemetry",
        aggregate(&[]),
    );

    assert_eq!(section.basename, "alpha");
    assert_eq!(section.root, "/work/alpha");
    assert_eq!(section.label(), "alpha — /work/alpha");
}

#[test]
fn the_multi_page_states_the_machine_total_once_then_one_section_per_repository() {
    let sections = vec![
        ReportSection::new("/work/alpha", "/work/alpha/.sangkar/telemetry", fixture()),
        ReportSection::new(
            "/work/beta",
            "/work/beta/.sangkar/telemetry",
            aggregate(&[]),
        ),
    ];
    let page = render_multi_page(&fixture(), GENERATED_AT, Some("7d"), &sections);

    assert_eq!(
        top_level_panels(&page),
        ["machine", "repo-1", "repo-2"],
        "the machine total is the first section and the repositories follow in registry order"
    );
    assert!(page.contains(
        "window <strong>7d</strong> · generated <span>2026-11-18T10:00:00.000Z</span> · repositories <span>2</span> · records <span>9</span>"
    ));
    assert!(page.contains("<nav aria-label=\"repositories\"><a href=\"#repo-1\">alpha — /work/alpha</a> · <a href=\"#repo-2\">beta — /work/beta</a></nav>"));
    assert!(page.contains("<h2>alpha — /work/alpha</h2>"));
    assert!(page.contains("<h2>beta — /work/beta</h2>"));
    assert!(page.contains("<code>/work/alpha/.sangkar/telemetry</code>"));
    assert!(
        panel(&page, "repo-1").contains("<section id=\"tools\""),
        "each repository section is render_body's panels"
    );
    assert!(
        panel(&page, "repo-2").contains("<section id=\"empty\""),
        "a repository with no records still gets a section that says so"
    );
    assert!(
        !page.contains("<script>"),
        "the TypeScript's machine-wide page carries no script; the ratio column is the reading"
    );
    for absent in ["http://", "https://", "<link", "@import", "url(", "src="] {
        assert!(!page.contains(absent), "no external asset: {absent}");
    }
    assert_eq!(page.matches("<style>").count(), 1);
}

#[test]
fn an_empty_registry_states_the_machine_total_and_says_nothing_is_registered() {
    let page = render_multi_page(&aggregate(&[]), GENERATED_AT, None, &[]);

    assert!(page.contains("<section id=\"machine\" class=\"panel\">"));
    assert!(
        page.contains("<p class=\"note\">no registered repository has recorded anything yet</p>")
    );
    assert!(
        !page.contains("<nav"),
        "an empty navigation landmark is noise, not a feature"
    );
    assert!(page.contains("window <strong>all records</strong>"));
    assert!(page.contains("repositories <span>0</span>"));
    assert!(panel(&page, "machine").contains("<section id=\"empty\""));
}

#[test]
fn a_hostile_root_and_source_directory_are_escaped_in_the_multi_page() {
    let sections = vec![ReportSection::new(
        "/work/<b>alpha",
        "/work/<b>alpha/.sangkar/telemetry<script>",
        aggregate(&[]),
    )];
    let page = render_multi_page(&aggregate(&[]), GENERATED_AT, None, &sections);

    assert!(page.contains("<h2>&lt;b&gt;alpha — /work/&lt;b&gt;alpha</h2>"));
    assert!(page.contains("<code>/work/&lt;b&gt;alpha/.sangkar/telemetry&lt;script&gt;</code>"));
    assert_eq!(
        page.matches("<script>").count(),
        0,
        "a source directory cannot open a script element"
    );
    assert!(!page.contains("<b>alpha"));
}

#[test]
fn the_sections_only_variant_renders_every_panel_and_no_machine_total() {
    let sections = vec![
        ReportSection::new("/work/alpha", "/work/alpha/.sangkar/telemetry", fixture()),
        ReportSection::new(
            "/work/beta",
            "/work/beta/.sangkar/telemetry",
            aggregate(&[]),
        ),
    ];
    let page = render_multi_report(&sections);

    assert_eq!(top_level_panels(&page), ["repo-1", "repo-2"]);
    assert!(page.contains("<nav aria-label=\"repositories\">"));
    assert!(
        panel(&page, "repo-1").contains("<section id=\"tools\" class=\"panel\">"),
        "the sections reuse render_body"
    );
    assert_eq!(page.matches("<style>").count(), 1);
    assert!(page.ends_with("</html>\n"));
}
