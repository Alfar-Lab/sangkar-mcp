//! The shipped guide and the `next_tools` table, pinned against the TypeScript oracle.
//!
//! Every expectation here was read off `src/shared/instructions.ts` and its test
//! (`tests/instructions.test.ts`), not re-derived: the guide is one deliverable published twice, so
//! a silent edit to either copy is the whole failure mode these assertions exist to catch.

use sangkar_core::instructions::{INSTRUCTIONS_TEXT, TOOL_NAMES, next_tools_for};

/// `wc -c src/prompts/INSTRUCTIONS.md` — the byte count of the file `INSTRUCTIONS_TEXT` embeds,
/// trailing newline included. Pinned as a literal so an edit to either side fails the build.
const INSTRUCTIONS_BYTES: usize = 4087;

/// The same text counted in characters. A byte count alone would accept a swap of a two-byte dash
/// for two ASCII bytes; the pair of pins closes the cheap way to keep the byte count and lose the
/// guide. `python3 -c 'print(len(open("src/prompts/INSTRUCTIONS.md").read()))'` gives this number.
const INSTRUCTIONS_CHARS: usize = 4075;

#[test]
fn the_guide_embeds_the_pinned_bytes() {
    // `wc -c src/prompts/INSTRUCTIONS.md` and this constant have to agree, so the file cannot grow
    // or shrink without a test going red on both sides of the port.
    assert_eq!(
        INSTRUCTIONS_TEXT.len(),
        INSTRUCTIONS_BYTES,
        "INSTRUCTIONS_TEXT no longer matches src/prompts/INSTRUCTIONS.md; re-run \
         `wc -c src/prompts/INSTRUCTIONS.md` and update this pin only if the TypeScript side moved too"
    );
    assert_eq!(
        INSTRUCTIONS_TEXT.chars().count(),
        INSTRUCTIONS_CHARS,
        "INSTRUCTIONS_TEXT has the pinned byte count but not the pinned character count"
    );
}

#[test]
fn the_guide_is_the_published_text_and_nothing_else() {
    // The TypeScript constant is the same text with the trailing newline trimmed
    // (`tests/instructions.test.ts` calls `.trimEnd()`), so the include carries exactly one
    // newline at the end and the guide opens on its title.
    assert!(
        INSTRUCTIONS_TEXT.starts_with("# Using the graph: five rules, seven tools"),
        "the guide starts with its title"
    );
    assert!(
        INSTRUCTIONS_TEXT.ends_with("next, always from this list.\n"),
        "the guide ends with its closing sentence and one newline"
    );
    assert_eq!(
        INSTRUCTIONS_TEXT.trim_end(),
        &INSTRUCTIONS_TEXT[..INSTRUCTIONS_BYTES - 1],
        "exactly one byte is trailing whitespace"
    );
}

#[test]
fn the_seven_tools_are_the_published_surface_in_order() {
    assert_eq!(
        TOOL_NAMES,
        [
            "status", "summary", "trace", "search", "impact", "expand", "index"
        ]
    );
}

#[test]
fn next_tools_routes_each_of_the_seven_tools() {
    // Element by element, in the order `src/shared/instructions.ts` declares them: an envelope that
    // reordered or dropped a suggestion would be a different routing promise.
    assert_eq!(next_tools_for("status"), ["summary", "search"]);
    assert_eq!(next_tools_for("summary"), ["impact", "trace", "expand"]);
    assert_eq!(next_tools_for("trace"), ["expand"]);
    assert_eq!(next_tools_for("search"), ["trace", "expand"]);
    assert_eq!(next_tools_for("impact"), ["trace", "expand"]);
    assert_eq!(next_tools_for("expand"), ["trace"]);
    assert_eq!(next_tools_for("index"), ["status"]);
}

#[test]
fn every_tool_routes_within_the_surface_and_never_to_itself() {
    let surface: Vec<&str> = TOOL_NAMES.to_vec();
    for tool in TOOL_NAMES {
        let next = next_tools_for(tool);
        // A tool added to `TOOL_NAMES` without a route would arrive here as an empty list, which is
        // the case the TypeScript's exhaustive `Record<ToolName, …>` makes a type error.
        assert!(!next.is_empty(), "{tool} suggests nothing");
        let outside: Vec<&String> = next
            .iter()
            .filter(|suggestion| !surface.contains(&suggestion.as_str()))
            .collect();
        assert!(
            outside.is_empty(),
            "{tool} names tools outside the surface: {outside:?}"
        );
        assert!(
            !next.iter().any(|suggestion| suggestion == tool),
            "{tool} suggests itself"
        );
    }
}

#[test]
fn index_is_the_follow_up_after_a_repair() {
    assert_eq!(next_tools_for("index"), ["status"]);
}

#[test]
fn a_tool_outside_the_surface_gets_no_suggestion_rather_than_a_guess() {
    for unknown in ["", "index_repository", "Status", "status ", "summar"] {
        assert_eq!(
            next_tools_for(unknown),
            Vec::<String>::new(),
            "{unknown:?} is not a published tool"
        );
    }
}

#[test]
fn every_call_returns_a_fresh_vector_so_one_envelope_cannot_mutate_the_table() {
    let mut first = next_tools_for("summary");
    first.push("index".to_owned());
    first.clear();
    assert_eq!(next_tools_for("summary"), ["impact", "trace", "expand"]);
}
