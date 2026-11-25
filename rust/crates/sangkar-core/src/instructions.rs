//! The one-screen guide and its routing table, ported from `src/shared/instructions.ts`.
//!
//! Both are strings this server publishes to an agent — [`INSTRUCTIONS_TEXT`] as the MCP
//! `instructions`, [`next_tools_for`] on every routed envelope — so they are data the port
//! reproduces rather than prose it may improve. `tests/instructions.test.ts` pins the same guide on
//! the TypeScript side and `tests/instructions.rs` pins it here; neither copy may drift.
//!
//! The guide is embedded from this crate's own `assets/` directory, the way
//! `sangkar_store::migrations` embeds the SQL: one deliverable, two implementations, one copy of the
//! bytes. It sat in the TypeScript tree's `src/prompts/` until TASK-062 moved it here unchanged.
//!
//! There is no `print_instructions` counterpart to the TypeScript: on this side stdout carries the
//! protocol and [`crate::log`] owns stderr, so whichever CLI mode answers with the guide writes
//! [`INSTRUCTIONS_TEXT`] itself.

/// The seven tools this server publishes (TOOLS-SPEC.md §10). Every suggestion [`next_tools_for`]
/// makes is drawn from this list, so `next_tools` can never name a tool the surface does not have.
///
/// `index` sits last on purpose: measured, it was called 1 081 times and failed 12.9 % of the time
/// for work the server already does itself (TOOLS-SPEC.md §8), so it is a repair, not a query.
pub const TOOL_NAMES: [&str; 7] = [
    "status", "summary", "trace", "search", "impact", "expand", "index",
];

/// The guide, byte for byte `assets/INSTRUCTIONS.md` — the file `tests/instructions.test.ts`
/// compares against the TypeScript constant, trailing newline included.
///
/// This is not a second copy of the text: the file was embedded out of the TypeScript tree while
/// this was a strangler migration, and TASK-062 moved it here with the bytes untouched, so only
/// this argument changed. The byte and character counts pinned in `tests/instructions.rs` are what
/// makes the move provably a no-op.
pub const INSTRUCTIONS_TEXT: &str = include_str!("../assets/INSTRUCTIONS.md");

/// The tools worth calling after `tool`, for the envelope's `next_tools`.
///
/// A name this surface does not know (an older client, a typo) gets an empty vector: suggesting a
/// plausible-looking tool for an unknown one would be a guess, and `next_tools` is a routing
/// promise. The vector is freshly allocated per call, so one envelope cannot mutate the table
/// through another.
#[must_use]
pub fn next_tools_for(tool: &str) -> Vec<String> {
    let next: &[&str] = match tool {
        "status" => &["summary", "search"],
        "summary" => &["impact", "trace", "expand"],
        "trace" => &["expand"],
        "search" => &["trace", "expand"],
        "impact" => &["trace", "expand"],
        "expand" => &["trace"],
        "index" => &["status"],
        _ => &[],
    };

    next.iter().map(|name| (*name).to_owned()).collect()
}
