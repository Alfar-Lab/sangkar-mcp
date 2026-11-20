/**
 * The report's last step: one aggregate in, one HTML file out, with no network in between. The CSS,
 * the data and the only script are inline, so the file opens from a USB stick on a machine with the
 * radio off and still draws its chart.
 *
 * Two properties the caller relies on, and both are cheap to lose by accident:
 * - Determinism. No clock, no randomness, no `toLocaleString`, no `localeCompare`. `generated_at`
 *   is a parameter for exactly that reason: the caller reads the clock once, this module never reads
 *   it, so two renders of one directory differ only when the directory did.
 * - Structural safety. Every free string goes through {@link escapeHtml}, and the data blob has its
 *   `<` spelled `\u003c` before it is substituted, so a tool named `</script>` cannot close the tag
 *   that carries it.
 */
import type { ReportData } from "./report";

type ToolStats = ReportData["tools"][number];
type IndexRunRow = ReportData["index_runs"][number];
type TrendPoint = ReportData["unresolved_trend"][number];
type SessionMix = ReportData["sessions"];

const NO_VALUE = "—";

const TOOL_HEADERS = [
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

const RUN_HEADERS = [
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

/**
 * Five characters, `'` included because a value can land in a single-quoted attribute tomorrow.
 * `&` is replaced first or the ampersands introduced by the next four calls would be escaped twice.
 */
export function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

/**
 * Grouping is spelled out rather than delegated to `toLocaleString`: a report is diffed against the
 * previous one, so two hosts with different locales must serialise the same bytes.
 */
function fmtInt(value: number): string {
  const digits = `${Math.trunc(Math.abs(value))}`;
  let grouped = "";
  for (let end = digits.length; end > 0; end -= 3) {
    const start = Math.max(end - 3, 0);
    const group = digits.slice(start, end);
    grouped = grouped === "" ? group : `${group},${grouped}`;
  }
  return value < 0 ? `-${grouped}` : grouped;
}

/** Two decimals: the data arrives rounded to four, and a report column is scanned, not analysed. */
function fmtRatio(value: number): string {
  return (Math.round(value * 100) / 100).toFixed(2);
}

/** `null` is meaningful on `mean_budget_used_ratio` — no call carried the field — so it is not zero. */
function optionalRatio(value: number | null): string {
  return value === null ? NO_VALUE : fmtRatio(value);
}

/**
 * `key: count` with the keys ascending. `Object.keys().sort()` is code-unit order, which is the same
 * order on every host; `localeCompare` answers differently per locale and would break the diff.
 */
function pairsOf(mix: Readonly<Record<string, number>>): string {
  const keys = Object.keys(mix).sort();
  if (keys.length === 0) {
    return NO_VALUE;
  }
  return keys.map((key) => `${escapeHtml(key)}: ${fmtInt(mix[key] ?? 0)}`).join(", ");
}

function rowOf(cells: readonly string[]): string {
  return `<tr>${cells.join("")}</tr>`;
}

function cell(value: string): string {
  return `<td>${value}</td>`;
}

function numCell(value: string): string {
  return `<td class="num">${value}</td>`;
}

function labelCell(label: string): string {
  return `<th scope="row">${label}</th>`;
}

/**
 * One table, with its body replaced by a single explanatory row when it has nothing to show. The
 * message says which kind of record is missing rather than repeating "no data", because a window
 * with calls and no index runs is a different finding from a window with nothing at all.
 */
function tableOf(
  headers: readonly string[],
  rows: readonly string[],
  emptyMessage: string,
): string {
  const head = headers.map((header) => `<th scope="col">${header}</th>`).join("");
  const body =
    rows.length === 0
      ? rowOf([`<td class="empty" colspan="${headers.length}">${emptyMessage}</td>`])
      : rows.join("\n");
  return `<table>
<thead>
<tr>${head}</tr>
</thead>
<tbody>
${body}
</tbody>
</table>`;
}

function panelOf(id: string, title: string, body: string): string {
  return `<section id="${id}" class="panel">
<h2>${title}</h2>
${body}
</section>`;
}

function headerPanel(
  totals: ReportData["totals"],
  windowLabel: string,
  generatedAt: string,
): string {
  return `<header class="panel">
<h1>sangkar telemetry</h1>
<p class="meta">window <strong>${escapeHtml(windowLabel)}</strong> · generated <span>${escapeHtml(generatedAt)}</span></p>
<dl class="totals">
<div><dt>records</dt><dd>${fmtInt(totals.records)}</dd></div>
<div><dt>calls</dt><dd>${fmtInt(totals.calls)}</dd></div>
<div><dt>index runs</dt><dd>${fmtInt(totals.index_runs)}</dd></div>
<div><dt>sessions</dt><dd>${fmtInt(totals.sessions)}</dd></div>
<div><dt>unparsed lines</dt><dd>${fmtInt(totals.unparsed)}</dd></div>
<div><dt>install ids</dt><dd>${fmtInt(totals.install_ids)}</dd></div>
<div><dt>days</dt><dd>${fmtInt(totals.days)}</dd></div>
</dl>
</header>`;
}

function toolsPanel(tools: readonly ToolStats[], empty: boolean): string {
  const rows = empty
    ? []
    : tools.map((tool) =>
        rowOf([
          cell(escapeHtml(tool.tool)),
          numCell(fmtInt(tool.calls)),
          cell(pairsOf(tool.statuses)),
          cell(pairsOf(tool.error_codes)),
          numCell(fmtInt(tool.duration_ms.p50)),
          numCell(fmtInt(tool.duration_ms.p95)),
          numCell(fmtRatio(tool.duration_ms.mean)),
          numCell(fmtInt(tool.duration_ms.max)),
          numCell(fmtInt(tool.truncated)),
          numCell(optionalRatio(tool.mean_budget_used_ratio)),
          numCell(fmtInt(tool.eq)),
          numCell(fmtInt(tool.gte)),
        ]),
      );
  return panelOf("tools", "per tool", tableOf(TOOL_HEADERS, rows, "no calls in this window"));
}

function truncationPanel(truncation: ReportData["truncation"], empty: boolean): string {
  const rows = empty
    ? []
    : [
        rowOf([labelCell("truncated calls"), numCell(fmtInt(truncation.count))]),
        rowOf([labelCell("truncation rate"), numCell(fmtRatio(truncation.rate))]),
        rowOf([labelCell("reasons"), cell(pairsOf(truncation.reasons))]),
      ];
  return panelOf(
    "truncation",
    "truncation",
    tableOf(["measure", "value"], rows, "no calls in this window"),
  );
}

function relationsPanel(relations: ReportData["relations"], empty: boolean): string {
  const rows = empty
    ? []
    : [
        rowOf([labelCell("eq (total is complete)"), numCell(fmtInt(relations.eq))]),
        rowOf([labelCell("gte (total is a lower bound)"), numCell(fmtInt(relations.gte))]),
        rowOf([labelCell("eq ratio of calls"), numCell(fmtRatio(relations.eq_ratio))]),
      ];
  return panelOf(
    "relations",
    "relations",
    tableOf(["measure", "value"], rows, "no calls in this window"),
  );
}

function coveragePanel(coverage: ReportData["coverage"], empty: boolean): string {
  const rows = empty
    ? []
    : [
        rowOf([labelCell("stale calls"), numCell(fmtInt(coverage.stale_calls))]),
        rowOf([
          labelCell("mean unresolved edges per call"),
          numCell(fmtRatio(coverage.mean_unresolved_edges)),
        ]),
      ];
  return panelOf(
    "coverage",
    "coverage",
    tableOf(["measure", "value"], rows, "no calls in this window"),
  );
}

function argNamesPanel(argNames: ReportData["arg_names"], empty: boolean): string {
  const rows = empty
    ? []
    : argNames.map((entry) => rowOf([cell(escapeHtml(entry.name)), numCell(fmtInt(entry.calls))]));
  return panelOf(
    "argument-names",
    "argument names",
    `<p class="note">Names only: a call record keeps the argument names it was given, never their values.</p>
${tableOf(["name", "calls"], rows, "no argument names in this window")}`,
  );
}

function trendPanel(points: readonly TrendPoint[], empty: boolean): string {
  const rows = empty
    ? []
    : points.map((point) =>
        rowOf([
          cell(escapeHtml(point.date)),
          numCell(fmtInt(point.runs)),
          numCell(fmtRatio(point.ratio)),
          `<td><span class="track"><span class="bar" data-day="${escapeHtml(point.date)}"></span></span></td>`,
        ]),
      );
  return panelOf(
    "trend",
    "unresolved edges per day",
    `<p class="note">One bar per day, scaled to the highest ratio in this window by the script below; the ratio column is the same number with the script off.</p>
${tableOf(["day", "index runs", "ratio", "trend"], rows, "no index runs in this window")}`,
  );
}

function indexRunsPanel(runs: readonly IndexRunRow[], empty: boolean): string {
  const rows = empty
    ? []
    : runs.map((run) =>
        rowOf([
          cell(escapeHtml(run.ts)),
          cell(escapeHtml(run.mode)),
          numCell(fmtInt(run.exit_code)),
          numCell(fmtInt(run.duration_ms)),
          numCell(fmtInt(run.files_seen)),
          numCell(fmtInt(run.files_indexed)),
          numCell(fmtInt(run.files_skipped)),
          numCell(fmtInt(run.files_failed)),
          numCell(fmtInt(run.files_changed_in_run)),
          numCell(fmtRatio(run.unresolved_ratio)),
          numCell(fmtInt(run.db_size_bytes)),
          cell(run.generation_advanced ? "advanced" : "unchanged"),
        ]),
      );
  return panelOf(
    "index-runs",
    "index runs",
    tableOf(RUN_HEADERS, rows, "no index runs in this window"),
  );
}

function sessionsPanel(sessions: SessionMix, empty: boolean): string {
  const rows = empty
    ? []
    : [
        rowOf([labelCell("by session mode"), cell(pairsOf(sessions.by_mode))]),
        rowOf([labelCell("by root kind"), cell(pairsOf(sessions.by_root_kind))]),
        rowOf([labelCell("by repo size bucket"), cell(pairsOf(sessions.by_size_bucket))]),
      ];
  return panelOf(
    "sessions",
    "sessions",
    tableOf(["group", "counts"], rows, "no sessions in this window"),
  );
}

/** Shown last, and only for an empty window: the tables above already say which record is missing. */
function emptyPanel(): string {
  return `<section id="empty" class="panel empty-state">
<h2>nothing recorded yet</h2>
<p>No record matches this window. Run the server or the indexer once and the same panels fill in; the layout does not move.</p>
</section>`;
}

/**
 * The one substitution of the data blob. Only `<` is rewritten — as `\u003c` — because that single
 * character is what could close the script tag; escaping the rest would corrupt a valid JSON string
 * for no gain, since the browser never parses this text as HTML.
 */
function embeddedJson(data: ReportData): string {
  return JSON.stringify(data).replaceAll("<", "\\u003c");
}

/**
 * Plain DOM, no framework, no `fetch`: the whole chart is six lines against a blob that is already in
 * the document. A day whose ratio is the window maximum fills its track and every other day scales
 * against it, because a fixed 0..1 axis would leave a healthy graph as an invisible sliver.
 */
function trendScript(data: ReportData): string {
  return `(function () {
  var data = ${embeddedJson(data)};
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
})();`;
}

/**
 * The panels of one report, without <html>/<head>: the multi-repo page reuses them.
 *
 * The header panel stays out of here on purpose: it carries the clock and the window label, which
 * belong to the caller, and the multi-repo page states both once for the whole machine.
 */
export function renderBody(data: ReportData): string {
  const empty = data.totals.records === 0;
  return `${toolsPanel(data.tools, empty)}
${truncationPanel(data.truncation, empty)}
${relationsPanel(data.relations, empty)}
${coveragePanel(data.coverage, empty)}
${argNamesPanel(data.arg_names, empty)}
${trendPanel(data.unresolved_trend, empty)}
${indexRunsPanel(data.index_runs, empty)}
${sessionsPanel(data.sessions, empty)}${empty ? `\n${emptyPanel()}` : ""}`;
}

/**
 * The whole file: `head` is everything before the panels — doctype, the one stylesheet and this
 * report's header panel — and {@link renderBody} draws the rest, so no panel is spelled twice.
 */
export function renderHtml(
  data: ReportData,
  meta: { readonly generated_at: string; readonly since: string | null },
): string {
  const windowLabel = meta.since === null ? "all records" : meta.since;
  const head = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sangkar telemetry report</title>
<style>
:root {
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
}
</style>
</head>
<body>
<main>
${headerPanel(data.totals, windowLabel, meta.generated_at)}
`;
  return `${head}${renderBody(data)}
</main>
<script>
${trendScript(data)}
</script>
</body>
</html>
`;
}
