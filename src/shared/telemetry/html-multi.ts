/**
 * The machine-wide report: one document, one stylesheet, one section per telemetry directory and no
 * asset that needs a network — the same file the daemon serves can be carried on a USB stick and
 * still draws itself.
 *
 * The panels are not re-implemented here. {@link renderBody} from `html.ts` draws them, so a column
 * added to the single-repo report shows up in every section of this page without an edit in this
 * file, and the two documents cannot disagree about what a report looks like.
 */
import { escapeHtml, renderBody, renderHtml } from "./html";
import type { ReportData } from "./report";
import { aggregate } from "./report";

export interface ReportSectionView {
  /** The repo's basename plus its canonical path; raw from the caller, escaped at render time. */
  readonly label: string;
  /** The telemetry directory this record set was read from. */
  readonly sourceDir: string;
  readonly data: ReportData;
}

/**
 * `html.ts` owns the only stylesheet, and its contract allows exactly one new export, so the
 * borrowed block is taken out of a rendered document instead of copied: a colour changed there
 * reaches this page on the next render, and the two files can never hold two stylesheets.
 *
 * The render is of an empty window, which is the cheapest deterministic document the module can
 * produce; the assertion below is a compile-time mistake caught at import, not a runtime branch.
 */
const STYLE_BLOCK = (() => {
  const html = renderHtml(aggregate({ records: [], unparsed: 0, files: [], since: null }), {
    generated_at: "",
    since: null,
  });
  const block = /<style>\n[\s\S]*?\n<\/style>/.exec(html);
  if (block === null) {
    throw new Error("html.ts rendered no <style> block for the multi-repo page to reuse");
  }
  return block[0];
})();

const NO_SECTIONS = "no registered repository has recorded anything yet";

/**
 * The machine total comes first because it is the only section that always exists: with nothing
 * registered it still states the window, the clock and the reason the page looks empty.
 */
function machineSection(
  machine: ReportData,
  since: string | null,
  generatedAt: string,
  sectionCount: number,
): string {
  const windowLabel = since === null ? "all records" : since;
  const emptyLine = sectionCount === 0 ? `\n<p class="note">${NO_SECTIONS}</p>` : "";
  return `<section id="machine" class="panel">
<h2>machine total</h2>
<p class="meta">window <strong>${escapeHtml(windowLabel)}</strong> · generated <span>${escapeHtml(generatedAt)}</span> · repositories <span>${sectionCount}</span> · records <span>${machine.totals.records}</span></p>${emptyLine}
${renderBody(machine)}
</section>`;
}

/** Omitted when there is nothing to link to: an empty navigation landmark is noise, not a feature. */
function navOf(sections: readonly ReportSectionView[]): string {
  if (sections.length === 0) {
    return "";
  }
  const links = sections.map(
    (section, index) => `<a href="#repo-${index + 1}">${escapeHtml(section.label)}</a>`,
  );
  return `<nav aria-label="repositories">${links.join(" · ")}</nav>`;
}

/**
 * The panels keep the ids `html.ts` gives them, so a page with several sections repeats
 * `id="tools"` and friends. Nothing links to those ids on either page — the nav targets `repo-N`,
 * which is unique here — so a repeated id costs nothing that is actually used.
 */
function sectionOf(section: ReportSectionView, index: number): string {
  return `<section id="repo-${index + 1}" class="panel">
<h2>${escapeHtml(section.label)}</h2>
<p class="meta">source directory <code>${escapeHtml(section.sourceDir)}</code></p>
${renderBody(section.data)}
</section>`;
}

export function renderMultiReport(input: {
  readonly machine: ReportData;
  readonly sections: readonly ReportSectionView[];
  readonly generated_at: string;
  readonly since: string | null;
}): string {
  const sectionsHtml = input.sections.map(sectionOf).join("\n");
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>sangkar telemetry report</title>
${STYLE_BLOCK}
</head>
<body>
<main>
${machineSection(input.machine, input.since, input.generated_at, input.sections.length)}
${navOf(input.sections)}
${sectionsHtml}
</main>
</body>
</html>
`;
}
