//! The machine-wide report: one document, one stylesheet, one section per telemetry directory and no
//! asset that needs a network — the same file the daemon serves can be carried on a USB stick and
//! still draws itself.
//!
//! Ported from `src/shared/telemetry/html-multi.ts`. The panels are not re-implemented here:
//! [`crate::html::render_body`] draws them, so a column added to the single-repository report shows
//! up in every section of this page without an edit in this file, and the two documents cannot
//! disagree about what a report looks like. The stylesheet is borrowed the same way, through
//! [`crate::html::style_block`], so the workspace holds exactly one source of styles.
//!
//! Two entry points, because the plan's one-argument signature and the TypeScript's four inputs are
//! not the same shape:
//! - [`render_multi_page`] is the faithful port of `renderMultiReport`: the machine total (a fold
//!   over every repository's records), the clock reading and the window label are arguments, which is
//!   what `serve.ts`'s `reportSections` has in hand. The served page and the file it mirrors are this
//!   document.
//! - [`render_multi_report`] is the plan's `render_multi_report(sections: &[ReportSection])`. A slice
//!   of per-repository aggregates cannot carry the machine fold, the clock or the window label — the
//!   first is a fold over records no section kept, the other two are not in [`ReportData`] at all —
//!   so its document is the head, the navigation and one section per repository, with no
//!   machine-total panel. See the function's own note.
//!
//! Panel ids are the ones `html.rs` gives them, so a page with several sections repeats `id="tools"`
//! and friends. Nothing links to those ids on either page — the nav targets `repo-N`, which is unique
//! here — so a repeated id costs nothing that is actually used.

use std::path::Path;

use crate::html::{escape_html, render_body, style_block};
use crate::report::ReportData;

/// One registered repository's section of the machine-wide page.
///
/// The three strings are the pieces the TypeScript carries as `label` and `sourceDir`; the label is
/// composed here from `basename` and `root` instead of being passed ready-made, so the separator and
/// the order cannot differ between two callers.
#[derive(Clone, Debug, PartialEq)]
pub struct ReportSection {
    /// The registered root as the registry spells it — absolute, and the last piece of the label.
    pub root: String,
    /// The final path component of `root`, kept beside it so the label needs no second parse.
    pub basename: String,
    /// The telemetry directory this section's records were read from; shown under the label, escaped.
    pub source_dir: String,
    /// One repository's fold, from [`crate::report::aggregate`] or [`crate::report::aggregate_with`].
    pub data: ReportData,
}

impl ReportSection {
    /// Builds a section with the basename taken from `root`, which is what a caller has.
    ///
    /// `Path::file_name` has no answer for a filesystem root (`/`), and the TypeScript's
    /// `basename("/")` answers the empty string there; a registry root is a real directory, so the
    /// fallback is the root itself rather than a label that begins with the separator.
    #[must_use]
    pub fn new(root: &str, source_dir: &str, data: ReportData) -> Self {
        Self {
            root: root.to_owned(),
            basename: basename_of(root),
            source_dir: source_dir.to_owned(),
            data,
        }
    }

    /// `<basename> — <root>`, raw: every interpolation escapes at render time.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{} — {}", self.basename, self.root)
    }
}

fn basename_of(root: &str) -> String {
    Path::new(root).file_name().map_or_else(
        || root.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Said once under the machine total when the registry is empty, so the page explains itself instead
/// of looking broken.
const NO_SECTIONS: &str = "no registered repository has recorded anything yet";

/// The machine total comes first because it is the only section that always exists: with nothing
/// registered it still states the window, the clock and the reason the page looks empty.
fn machine_section(
    machine: &ReportData,
    generated_at: &str,
    since: Option<&str>,
    section_count: usize,
) -> String {
    let window_label = since.unwrap_or("all records");
    let empty_line = if section_count == 0 {
        format!("\n<p class=\"note\">{NO_SECTIONS}</p>")
    } else {
        String::new()
    };
    format!(
        "<section id=\"machine\" class=\"panel\">\n<h2>machine total</h2>\n<p class=\"meta\">window <strong>{}</strong> · generated <span>{}</span> · repositories <span>{section_count}</span> · records <span>{}</span></p>{empty_line}\n{}\n</section>",
        escape_html(window_label),
        escape_html(generated_at),
        machine.totals.records,
        render_body(machine),
    )
}

/// Omitted when there is nothing to link to: an empty navigation landmark is noise, not a feature.
fn nav_of(sections: &[ReportSection]) -> String {
    if sections.is_empty() {
        return String::new();
    }
    let links: Vec<String> = sections
        .iter()
        .enumerate()
        .map(|(index, section)| {
            format!(
                "<a href=\"#repo-{}\">{}</a>",
                index + 1,
                escape_html(&section.label())
            )
        })
        .collect();
    format!(
        "<nav aria-label=\"repositories\">{}</nav>",
        links.join(" · ")
    )
}

fn section_of(section: &ReportSection, index: usize) -> String {
    format!(
        "<section id=\"repo-{}\" class=\"panel\">\n<h2>{}</h2>\n<p class=\"meta\">source directory <code>{}</code></p>\n{}\n</section>",
        index + 1,
        escape_html(&section.label()),
        escape_html(&section.source_dir),
        render_body(&section.data),
    )
}

/// The document around the sections; `machine` is the machine-total panel, or the empty string for
/// the sections-only variant.
fn document_of(machine: &str, sections: &[ReportSection]) -> String {
    let sections_html = sections
        .iter()
        .enumerate()
        .map(|(index, section)| section_of(section, index))
        .collect::<Vec<String>>()
        .join("\n");
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>sangkar telemetry report</title>\n{}\n</head>\n<body>\n<main>\n{machine}\n{}\n{sections_html}\n</main>\n</body>\n</html>\n",
        style_block(),
        nav_of(sections),
    )
}

/// The whole machine-wide page: the faithful port of `renderMultiReport`, which is what the report
/// server renders per request (`serve.ts` calls it with the machine fold, the clock and the window).
///
/// `since` is `None` for "all records" and `generated_at` is the one clock reading the caller took, so
/// this stays a pure function of its arguments.
#[must_use]
pub fn render_multi_page(
    machine: &ReportData,
    generated_at: &str,
    since: Option<&str>,
    sections: &[ReportSection],
) -> String {
    document_of(
        &machine_section(machine, generated_at, since, sections.len()),
        sections,
    )
}

/// The plan's one-argument entry point: one `<section id="repo-N">` per registered repository,
/// labelled `<basename> — <root>`, each reusing [`crate::html::render_body`].
///
/// The machine-total panel is the one panel this document does not have, and it is a signature limit
/// rather than a choice: the section counts come from one fold over every repository's *records*
/// (which [`ReportData`] no longer holds), and the clock and window label are inputs [`ReportData`]
/// never carried. A caller that has all three — the server does — calls [`render_multi_page`] and gets
/// the same document with the machine total first.
#[must_use]
pub fn render_multi_report(sections: &[ReportSection]) -> String {
    document_of("", sections)
}
