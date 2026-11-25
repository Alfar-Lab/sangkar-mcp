//! The report's last step: one aggregate in, one HTML document out, with no network in between. The
//! stylesheet, the data and the only script are inline, so the file opens from a USB stick on a
//! machine with the radio off and still draws its chart.
//!
//! Ported from `src/shared/telemetry/html.ts`. Two properties the caller relies on, and both are
//! cheap to lose by accident:
//! - Determinism. No clock, no randomness, no `toLocaleString`, no locale-aware ordering.
//!   `generated_at` is a parameter for exactly that reason: the caller reads the clock once, this
//!   module never does, so two renders of one directory differ only when the directory did.
//! - Structural safety. Every free string goes through [`escape_html`], and the data blob has its
//!   `<` spelled `\u003c` before it is substituted, so a tool named `</script>` cannot close the tag
//!   that carries it.
//!
//! The two number formats are hand-written for the same reason [`escape_html`] is: `Intl` and
//! `toFixed` have no equivalent that both formats and groups identically on every host, and a report
//! is diffed against the previous one. They reproduce the TypeScript's output rather than a nicer
//! one — grouping is `fmtInt`'s, and a ratio is `(Math.round(v * 100) / 100).toFixed(2)`, which
//! rounds halves up (towards +infinity) where Rust's `f64::round` rounds them away from zero.
//!
//! The stylesheet below is copied byte for byte from `html.ts`; the two files must not drift, so a
//! colour change belongs in both or in neither.
//!
//! [`render_body`] draws the panels and [`render_html`] wraps them in the one document, so a panel is
//! never spelled twice and the machine-wide page (`html_multi.rs`) draws the same panels.

use std::collections::BTreeMap;

use sangkar_core::log;

use crate::report::{
    ArgNameCount, Coverage, IndexRunRow, Relations, ReportData, SessionBreakdown, ToolStats,
    Totals, TrendPoint, TruncationSummary,
};

/// The substitution for a number that carries no value: `mean_budget_used_ratio` is `None` when no
/// call supplied a budget, and a zero there would read as "the budget went unused".
const NO_VALUE: &str = "—";

const TOOL_HEADERS: [&str; 12] = [
    "tool",
    "calls",
    "status mix",
    "error codes",
    "p50 ms",
    "p95 ms",
    "mean ms",
    "max ms",
    "truncated",
    "mean budget used",
    "eq",
    "gte",
];

const RUN_HEADERS: [&str; 12] = [
    "recorded at",
    "mode",
    "exit code",
    "duration ms",
    "files seen",
    "files indexed",
    "files skipped",
    "files failed",
    "files changed",
    "unresolved ratio",
    "db bytes",
    "generation",
];

/// The one stylesheet, copied verbatim from `src/shared/telemetry/html.ts` (including the
/// `prefers-color-scheme: dark` block). There is no second source of styles: `html_multi.rs` borrows
/// this block through [`style_block`], so the two pages cannot disagree about a colour.
const STYLE_CSS: &str = r#":root {
  --paper: #fbfaf7;
  --ink: #1d1b18;
  --muted: #6b6559;
  --line: #ddd7c9;
  --accent: #a2502a;
  --track: #e8e3d6;
}
@media (prefers-color-scheme: dark) {
  :root {
    --paper: #171614;
    --ink: #ece7dd;
    --muted: #a09a8d;
    --line: #35322c;
    --accent: #d98a5c;
    --track: #2a2724;
  }
}
* {
  box-sizing: border-box;
}
body {
  margin: 0;
  padding: 2rem 1.5rem 4rem;
  background: var(--paper);
  color: var(--ink);
  font-family: ui-sans-serif, system-ui, "Segoe UI", Helvetica, Arial, sans-serif;
  font-size: 15px;
  line-height: 1.5;
  -webkit-text-size-adjust: 100%;
}
main {
  display: grid;
  gap: 1.5rem;
  max-width: 84rem;
  margin: 0 auto;
}
.panel {
  overflow-x: auto;
  border-top: 2px solid var(--ink);
  padding-top: 0.75rem;
}
h1 {
  margin: 0 0 0.25rem;
  font-size: 1.5rem;
  letter-spacing: -0.01em;
}
h2 {
  margin: 0 0 0.6rem;
  color: var(--muted);
  font-size: 0.8rem;
  font-weight: 600;
  letter-spacing: 0.09em;
  text-transform: uppercase;
}
p {
  margin: 0 0 0.6rem;
}
.meta {
  color: var(--muted);
  font-size: 0.9rem;
}
.note {
  color: var(--muted);
  font-size: 0.85rem;
}
.totals {
  display: flex;
  flex-wrap: wrap;
  gap: 0.75rem 1.75rem;
  margin: 0.75rem 0 0;
}
.totals div {
  display: flex;
  flex-direction: column;
}
.totals dt {
  color: var(--muted);
  font-size: 0.72rem;
  letter-spacing: 0.08em;
  text-transform: uppercase;
}
.totals dd {
  margin: 0;
  font-size: 1.35rem;
  font-weight: 600;
  font-variant-numeric: tabular-nums;
}
table {
  width: 100%;
  border-collapse: collapse;
  font-size: 0.875rem;
  font-variant-numeric: tabular-nums;
}
th,
td {
  border-bottom: 1px solid var(--line);
  padding: 0.4rem 0.6rem;
  text-align: left;
  vertical-align: top;
}
thead th {
  color: var(--muted);
  font-size: 0.72rem;
  font-weight: 600;
  letter-spacing: 0.07em;
  text-transform: uppercase;
  white-space: nowrap;
}
tbody th {
  font-weight: 600;
  white-space: nowrap;
}
.num {
  text-align: right;
  white-space: nowrap;
}
td.empty {
  color: var(--muted);
  font-style: italic;
}
.track {
  display: block;
  overflow: hidden;
  min-width: 7rem;
  height: 0.6rem;
  background: var(--track);
  border-radius: 2px;
}
.bar {
  display: block;
  width: 0;
  height: 100%;
  background: var(--accent);
}
.empty-state {
  border-top-color: var(--accent);
}
@media (max-width: 640px) {
  body {
    padding: 1rem 0.75rem 3rem;
  }
  main {
    gap: 1.25rem;
  }
  h1 {
    font-size: 1.25rem;
  }
  th,
  td {
    padding: 0.35rem 0.45rem;
  }
  .track {
    min-width: 4rem;
  }
}"#;

/// Five characters, `'` included because a value can land in a single-quoted attribute tomorrow.
///
/// One pass rather than five `replace` calls: `&` has to be replaced before the ampersands the other
/// four introduce are escaped a second time, and a single pass cannot get that order wrong.
#[must_use]
pub fn escape_html(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// `fmtInt`: group by thousands and truncate towards zero, with no locale in the answer.
///
/// `NaN` and `Infinity` are spelled and grouped the way the TypeScript's template literal spells
/// them; neither is reachable from [`ReportData`], which keeps non-finite numbers out of every field.
fn fmt_int(value: f64) -> String {
    let magnitude = value.abs().trunc();
    let digits = if magnitude.is_nan() {
        String::from("NaN")
    } else if magnitude.is_infinite() {
        String::from("Infinity")
    } else {
        // The data arrives rounded, and `{:.0}` prints integral doubles as digits, which is what
        // `${Math.trunc(...)}` does below 1e21.
        format!("{magnitude:.0}")
    };
    let grouped = group_thousands(&digits);
    if value < 0.0 {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// Commas every three digits from the right. The input is ASCII in every path above, so the byte
/// slicing cannot split a character.
fn group_thousands(digits: &str) -> String {
    let mut groups: Vec<&str> = Vec::new();
    let mut end = digits.len();
    while end > 0 {
        let start = end.saturating_sub(3);
        groups.push(&digits[start..end]);
        end = start;
    }
    groups.reverse();
    groups.join(",")
}

/// `Math.round`: the nearest integer, halves resolved towards +infinity, so `-2.5` is `-2`.
fn js_round(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

/// `fmtRatio`: two decimals of `(Math.round(v * 100) / 100).toFixed(2)`.
///
/// The value is first snapped to a whole hundredth, so the digits below the two printed decimals are
/// zero and the tie-breaking rule of `toFixed` (halves towards the larger integer) never decides a
/// digit. A negative value rounds to "-0.00" only when the pre-rounded hundredth is nonzero, exactly
/// as `(-0.0001).toFixed(2)` and `(-0).toFixed(2)` differ in the TypeScript.
fn fmt_ratio(value: f64) -> String {
    if value.is_nan() {
        return String::from("NaN");
    }
    if value.is_infinite() {
        return if value.is_sign_positive() {
            String::from("Infinity")
        } else {
            String::from("-Infinity")
        };
    }
    let hundredths = js_round(value * 100.0);
    if hundredths == 0.0 {
        return String::from("0.00");
    }
    let digits = format!("{:.0}", hundredths.abs());
    let padded = if digits.len() < 3 {
        format!("{}{digits}", "0".repeat(3 - digits.len()))
    } else {
        digits
    };
    let split = padded.len() - 2;
    let text = format!("{}.{}", &padded[..split], &padded[split..]);
    if hundredths < 0.0 {
        format!("-{text}")
    } else {
        text
    }
}

/// `optionalRatio`: `null` is meaningful on `mean_budget_used_ratio`, so it is not zero.
fn optional_ratio(value: Option<f64>) -> String {
    value.map_or_else(|| String::from(NO_VALUE), fmt_ratio)
}

/// `key: count` with the keys ascending.
///
/// `BTreeMap`'s order is the byte order of the keys, which is the code-unit order
/// `Object.keys().sort()` answers for every key telemetry writes (statuses, error codes, truncation
/// reasons, session modes, root kinds and size buckets are all ASCII).
fn pairs_of(mix: &BTreeMap<String, u64>) -> String {
    if mix.is_empty() {
        return String::from(NO_VALUE);
    }
    mix.iter()
        .map(|(key, count)| format!("{}: {}", escape_html(key), fmt_int(*count as f64)))
        .collect::<Vec<String>>()
        .join(", ")
}

fn row_of(cells: &[String]) -> String {
    format!("<tr>{}</tr>", cells.concat())
}

fn cell(value: &str) -> String {
    format!("<td>{value}</td>")
}

fn num_cell(value: &str) -> String {
    format!("<td class=\"num\">{value}</td>")
}

fn label_cell(label: &str) -> String {
    format!("<th scope=\"row\">{label}</th>")
}

/// One table, with its body replaced by a single explanatory row when it has nothing to show. The
/// message says which kind of record is missing rather than repeating "no data", because a window
/// with calls and no index runs is a different finding from a window with nothing at all.
fn table_of(headers: &[&str], rows: &[String], empty_message: &str) -> String {
    let head: String = headers
        .iter()
        .map(|header| format!("<th scope=\"col\">{header}</th>"))
        .collect();
    let body = if rows.is_empty() {
        row_of(&[format!(
            "<td class=\"empty\" colspan=\"{}\">{empty_message}</td>",
            headers.len()
        )])
    } else {
        rows.join("\n")
    };
    format!("<table>\n<thead>\n<tr>{head}</tr>\n</thead>\n<tbody>\n{body}\n</tbody>\n</table>")
}

fn panel_of(id: &str, title: &str, body: &str) -> String {
    format!("<section id=\"{id}\" class=\"panel\">\n<h2>{title}</h2>\n{body}\n</section>")
}

fn totals_entry(label: &str, value: u64) -> String {
    format!(
        "<div><dt>{label}</dt><dd>{}</dd></div>",
        fmt_int(value as f64)
    )
}

fn header_panel(totals: &Totals, window_label: &str, generated_at: &str) -> String {
    let entries = [
        totals_entry("records", totals.records),
        totals_entry("calls", totals.calls),
        totals_entry("index runs", totals.index_runs),
        totals_entry("sessions", totals.sessions),
        totals_entry("unparsed lines", totals.unparsed),
        totals_entry("install ids", totals.install_ids),
        totals_entry("days", totals.days),
    ]
    .join("\n");
    format!(
        "<header class=\"panel\">\n<h1>sangkar telemetry</h1>\n<p class=\"meta\">window <strong>{}</strong> · generated <span>{}</span></p>\n<dl class=\"totals\">\n{entries}\n</dl>\n</header>",
        escape_html(window_label),
        escape_html(generated_at),
    )
}

fn tools_panel(tools: &[ToolStats], empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        tools
            .iter()
            .map(|tool| {
                row_of(&[
                    cell(&escape_html(&tool.tool)),
                    num_cell(&fmt_int(tool.calls as f64)),
                    cell(&pairs_of(&tool.statuses)),
                    cell(&pairs_of(&tool.error_codes)),
                    num_cell(&fmt_int(tool.duration_ms.p50)),
                    num_cell(&fmt_int(tool.duration_ms.p95)),
                    num_cell(&fmt_ratio(tool.duration_ms.mean)),
                    num_cell(&fmt_int(tool.duration_ms.max)),
                    num_cell(&fmt_int(tool.truncated as f64)),
                    num_cell(&optional_ratio(tool.mean_budget_used_ratio)),
                    num_cell(&fmt_int(tool.eq as f64)),
                    num_cell(&fmt_int(tool.gte as f64)),
                ])
            })
            .collect()
    };
    panel_of(
        "tools",
        "per tool",
        &table_of(&TOOL_HEADERS, &rows, "no calls in this window"),
    )
}

fn truncation_panel(truncation: &TruncationSummary, empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        vec![
            row_of(&[
                label_cell("truncated calls"),
                num_cell(&fmt_int(truncation.count as f64)),
            ]),
            row_of(&[
                label_cell("truncation rate"),
                num_cell(&fmt_ratio(truncation.rate)),
            ]),
            row_of(&[label_cell("reasons"), cell(&pairs_of(&truncation.reasons))]),
        ]
    };
    panel_of(
        "truncation",
        "truncation",
        &table_of(&["measure", "value"], &rows, "no calls in this window"),
    )
}

fn relations_panel(relations: &Relations, empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        vec![
            row_of(&[
                label_cell("eq (total is complete)"),
                num_cell(&fmt_int(relations.eq as f64)),
            ]),
            row_of(&[
                label_cell("gte (total is a lower bound)"),
                num_cell(&fmt_int(relations.gte as f64)),
            ]),
            row_of(&[
                label_cell("eq ratio of calls"),
                num_cell(&fmt_ratio(relations.eq_ratio)),
            ]),
        ]
    };
    panel_of(
        "relations",
        "relations",
        &table_of(&["measure", "value"], &rows, "no calls in this window"),
    )
}

fn coverage_panel(coverage: &Coverage, empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        vec![
            row_of(&[
                label_cell("stale calls"),
                num_cell(&fmt_int(coverage.stale_calls as f64)),
            ]),
            row_of(&[
                label_cell("mean unresolved edges per call"),
                num_cell(&fmt_ratio(coverage.mean_unresolved_edges)),
            ]),
        ]
    };
    panel_of(
        "coverage",
        "coverage",
        &table_of(&["measure", "value"], &rows, "no calls in this window"),
    )
}

fn arg_names_panel(arg_names: &[ArgNameCount], empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        arg_names
            .iter()
            .map(|entry| {
                row_of(&[
                    cell(&escape_html(&entry.name)),
                    num_cell(&fmt_int(entry.calls as f64)),
                ])
            })
            .collect()
    };
    panel_of(
        "argument-names",
        "argument names",
        &format!(
            "<p class=\"note\">Names only: a call record keeps the argument names it was given, never their values.</p>\n{}",
            table_of(
                &["name", "calls"],
                &rows,
                "no argument names in this window"
            ),
        ),
    )
}

fn trend_panel(points: &[TrendPoint], empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        points
            .iter()
            .map(|point| {
                row_of(&[
                    cell(&escape_html(&point.date)),
                    num_cell(&fmt_int(point.runs as f64)),
                    num_cell(&fmt_ratio(point.ratio)),
                    format!(
                        "<td><span class=\"track\"><span class=\"bar\" data-day=\"{}\"></span></span></td>",
                        escape_html(&point.date),
                    ),
                ])
            })
            .collect()
    };
    panel_of(
        "trend",
        "unresolved edges per day",
        &format!(
            "<p class=\"note\">One bar per day, scaled to the highest ratio in this window by the script below; the ratio column is the same number with the script off.</p>\n{}",
            table_of(
                &["day", "index runs", "ratio", "trend"],
                &rows,
                "no index runs in this window"
            ),
        ),
    )
}

fn index_runs_panel(runs: &[IndexRunRow], empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        runs.iter()
            .map(|run| {
                row_of(&[
                    cell(&escape_html(&run.ts)),
                    cell(&escape_html(&run.mode)),
                    num_cell(&fmt_int(run.exit_code as f64)),
                    num_cell(&fmt_int(run.duration_ms)),
                    num_cell(&fmt_int(run.files_seen as f64)),
                    num_cell(&fmt_int(run.files_indexed as f64)),
                    num_cell(&fmt_int(run.files_skipped as f64)),
                    num_cell(&fmt_int(run.files_failed as f64)),
                    num_cell(&fmt_int(run.files_changed_in_run as f64)),
                    num_cell(&fmt_ratio(run.unresolved_ratio)),
                    num_cell(&fmt_int(run.db_size_bytes as f64)),
                    cell(if run.generation_advanced {
                        "advanced"
                    } else {
                        "unchanged"
                    }),
                ])
            })
            .collect()
    };
    panel_of(
        "index-runs",
        "index runs",
        &table_of(&RUN_HEADERS, &rows, "no index runs in this window"),
    )
}

fn sessions_panel(sessions: &SessionBreakdown, empty: bool) -> String {
    let rows: Vec<String> = if empty {
        Vec::new()
    } else {
        vec![
            row_of(&[
                label_cell("by session mode"),
                cell(&pairs_of(&sessions.by_mode)),
            ]),
            row_of(&[
                label_cell("by root kind"),
                cell(&pairs_of(&sessions.by_root_kind)),
            ]),
            row_of(&[
                label_cell("by repo size bucket"),
                cell(&pairs_of(&sessions.by_size_bucket)),
            ]),
        ]
    };
    panel_of(
        "sessions",
        "sessions",
        &table_of(&["group", "counts"], &rows, "no sessions in this window"),
    )
}

/// Shown last, and only for an empty window: the tables above already say which record is missing.
fn empty_panel() -> String {
    String::from(
        "<section id=\"empty\" class=\"panel empty-state\">\n<h2>nothing recorded yet</h2>\n<p>No record matches this window. Run the server or the indexer once and the same panels fill in; the layout does not move.</p>\n</section>",
    )
}

/// The one substitution of the data blob. Only `<` is rewritten — as `\u003c` — because that single
/// character is what could close the script tag; escaping the rest would corrupt a valid JSON string
/// for no gain, since the browser never parses this text as HTML.
///
/// A serialisation failure is not a render failure: the chart is an enhancement over the ratio column
/// and a page that lost it still answers. Nothing in [`ReportData`] can fail to serialise — every
/// field is a string, a count or a finite `f64` — so the fallback is a guard, not a branch the tests
/// expect to take.
fn embedded_json(data: &ReportData) -> String {
    let Ok(json) = serde_json::to_string(data) else {
        log::warn("telemetry report: could not embed the report data", &[]);
        return String::from("null");
    };
    json.replace('<', "\\u003c")
}

/// Plain DOM, no framework, no `fetch`: the whole chart is six lines against a blob that is already in
/// the document. A day whose ratio is the window maximum fills its track and every other day scales
/// against it, because a fixed 0..1 axis would leave a healthy graph as an invisible sliver.
fn trend_script(data: &ReportData) -> String {
    let mut script = String::from("(function () {\n  var data = ");
    script.push_str(&embedded_json(data));
    script.push_str(
        r#";
  var trend = data.unresolved_trend || [];
  var byDay = {};
  var max = 0;
  for (var index = 0; index < trend.length; index += 1) {
    var point = trend[index];
    byDay[point.date] = point.ratio;
    if (point.ratio > max) {
      max = point.ratio;
    }
  }
  var bars = document.querySelectorAll(".bar");
  for (var position = 0; position < bars.length; position += 1) {
    var bar = bars[position];
    var ratio = byDay[bar.getAttribute("data-day")];
    if (typeof ratio !== "number" || max <= 0) {
      continue;
    }
    bar.style.width = ((ratio / max) * 100).toFixed(2) + "%";
  }
})();"#,
    );
    script
}

/// The panels of one report, without `<html>`/`<head>`: the multi-repository page reuses them.
///
/// The header panel stays out of here on purpose: it carries the clock and the window label, which
/// belong to the caller, and the machine-wide page states both once for the whole machine.
#[must_use]
pub fn render_body(data: &ReportData) -> String {
    let empty = data.totals.records == 0;
    let panels = [
        tools_panel(&data.tools, empty),
        truncation_panel(&data.truncation, empty),
        relations_panel(&data.relations, empty),
        coverage_panel(&data.coverage, empty),
        arg_names_panel(&data.arg_names, empty),
        trend_panel(&data.unresolved_trend, empty),
        index_runs_panel(&data.index_runs, empty),
        sessions_panel(&data.sessions, empty),
    ]
    .join("\n");
    if empty {
        format!("{panels}\n{}", empty_panel())
    } else {
        panels
    }
}

/// The whole file: everything before the panels — doctype, the one stylesheet and this report's
/// header panel — then [`render_body`], so no panel is spelled twice.
///
/// `since` is `None` for "all records"; the label repeats what the caller typed (`7d`, `2026-11-01`)
/// rather than the date it floored to.
#[must_use]
pub fn render_html(data: &ReportData, generated_at: &str, since: Option<&str>) -> String {
    let window_label = since.unwrap_or("all records");
    let head = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>sangkar telemetry report</title>\n{}\n</head>\n<body>\n<main>\n{}\n",
        style_block(),
        header_panel(&data.totals, window_label, generated_at),
    );
    format!(
        "{head}{}\n</main>\n<script>\n{}\n</script>\n</body>\n</html>\n",
        render_body(data),
        trend_script(data),
    )
}

/// `<style>` and the stylesheet above, exactly once for the whole workspace: `html_multi.rs` wraps
/// this block instead of holding a second copy, which is what the TypeScript does by extracting the
/// block from a rendered document.
pub(crate) fn style_block() -> String {
    format!("<style>\n{STYLE_CSS}\n</style>")
}
