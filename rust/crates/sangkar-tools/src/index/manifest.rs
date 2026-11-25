//! `index`'s manifest: the admin description and the four arguments a caller may pass
//! (`TOOLS-SPEC.md` §8).
//!
//! Ported from `src/tools/index/manifest.ts`. The description is §8's text verbatim — an admin
//! client reads it before calling the one tool in this server that rewrites the entire graph, so
//! paraphrasing it would be a behaviour change, not an edit. [`PARAMETERS`] is data, not a
//! validator: the registry (TASK-034) turns it into the `tools/list` schema, and [`IndexArgs`] is
//! its Rust mirror, so the handler never restates a default.
//!
//! There is deliberately no `budget` argument (`TOOLS-SPEC.md` §10): a repair call publishes counts,
//! not a list, so it has no payload worth paging.
//!
//! The descriptor types at the bottom are a **local** copy of `trace`'s vocabulary rather than an
//! import of it: `sangkar-boundary`'s tool-isolation rule forbids one tool module naming another,
//! and `index` needs one shape `trace` never declares — a boolean parameter. Field for field the
//! shapes match, so TASK-034's reconciliation is a move of the descriptor into one crate-root
//! module plus the deletion of these copies, never a second derivation.

use serde::{Deserialize, Serialize};

/// The tool name as the cursor pins it: a token minted by one tool must not resume another.
pub const NAME: &str = "index";

/// `TOOLS-SPEC.md` §8, verbatim. Two sentences, in this order, because the second is the one that
/// keeps a routine call from becoming a rebuild.
pub const DESCRIPTION: &str = concat!(
    "Admin: force a full or fast index, or repair a broken one. ",
    "Not for normal use — the server keeps the graph current automatically."
);

/// The `generation.mode` values the worker accepts; `full` rebuilds, `fast` republishes changes.
pub const MODES: [&str; 2] = ["full", "fast"];

/// `force` is off by default: the default repair is a normal run, not a rebuild from an empty stage.
pub const DEFAULT_FORCE: bool = false;

/// `register` is on by default, because the run is what makes the tree worth watching: an index
/// nobody can find again is a call the operator has to repeat.
pub const DEFAULT_REGISTER: bool = true;

/// The mode a call that names none publishes under.
pub const DEFAULT_MODE: IndexMode = IndexMode::Full;

/// The run's mode. `full` rebuilds the graph from an empty stage; `fast` republishes only what
/// changed. The wire spelling is the lowercase enum value, which is also the `--mode` argument the
/// worker is started with and the `payload.mode` this tool answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMode {
    #[default]
    Full,
    Fast,
}

impl IndexMode {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Fast => "fast",
        }
    }

    /// A spelling as a mode, for the child's own `mode` field and for argv; `None` for a value the
    /// manifest does not declare, which the caller keeps rather than coercing. The spellings come
    /// from [`Self::as_str`], so there is one place they are written down.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        [Self::Full, Self::Fast]
            .into_iter()
            .find(|mode| mode.as_str() == raw)
    }
}

impl std::fmt::Display for IndexMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A declared parameter's type, in the vocabulary `src/index.ts:227-275` turns into JSON Schema.
///
/// `Bool` is this manifest's one addition to `trace`'s three variants; JSON Schema spells it
/// `type: "boolean"`. The registry owns that conversion, not this file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    Str,
    /// A closed set of strings.
    Enum(&'static [&'static str]),
    Bool,
}

/// A declared default. Only the two shapes this tool's arguments have.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamDefault {
    Text(&'static str),
    Bool(bool),
}

/// One declared argument. Field order is the TypeScript literal's, so the table reads as the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Param {
    pub name: &'static str,
    pub ty: ParamType,
    pub required: bool,
    pub default: Option<ParamDefault>,
    /// Included in the schema as `min`; only an integer declares one, and nothing here is one.
    pub min: Option<i64>,
    /// Included in the schema as `max`; see [`Self::min`].
    pub max: Option<i64>,
    pub description: &'static str,
}

/// The argument table, in the order `tools/list` publishes the properties and the order the
/// TypeScript's `input` literal declares them.
///
/// No argument is required: every one of them has a meaning when it is absent, and the tool's whole
/// job is reachable from an empty argument object.
pub const PARAMETERS: [Param; 4] = [
    Param {
        name: "force",
        ty: ParamType::Bool,
        required: false,
        default: Some(ParamDefault::Bool(DEFAULT_FORCE)),
        min: None,
        max: None,
        description: concat!(
            "Rebuild from an empty stage and publish even when the file hashes say nothing ",
            "changed."
        ),
    },
    Param {
        name: "mode",
        ty: ParamType::Enum(&MODES),
        required: false,
        default: Some(ParamDefault::Text("full")),
        min: None,
        max: None,
        description: concat!(
            "full rebuilds the graph from an empty stage; fast republishes only what changed and ",
            "is the cheaper first pass on a huge repository."
        ),
    },
    // No default: an absent `repo` means "unselected", which the composition root answers from the
    // working directory; a default here would make that indistinguishable from a named repository.
    Param {
        name: "repo",
        ty: ParamType::Str,
        required: false,
        default: None,
        min: None,
        max: None,
        description: concat!(
            "Registered repository to answer from — its path or basename. ",
            "Omit it to use the working directory's repository, or the only registered one."
        ),
    },
    Param {
        name: "register",
        ty: ParamType::Bool,
        required: false,
        default: Some(ParamDefault::Bool(DEFAULT_REGISTER)),
        min: None,
        max: None,
        description: concat!(
            "Add this repository to the sangkar registry so the daemon watches it. ",
            "Use --no-register to index without registering."
        ),
    },
];

/// The one call `TOOLS-SPEC.md` §8 shows: a fast pass is the cheaper repair, and naming the mode is
/// the only argument here that changes what the worker does. `force` stays out because an example
/// that rebuilds from an empty stage is the expensive call, not the representative one.
pub const EXAMPLE: [(&str, ExampleValue); 1] = [("mode", ExampleValue::Text("fast"))];

/// The one scalar shape this tool's example uses; TASK-034 renders it into `examples`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExampleValue {
    Text(&'static str),
}

/// A call's arguments. Every field is optional because the schema carries the defaults and a caller
/// may send none of them; [`Self::force`] and its siblings apply those defaults themselves, so a
/// direct call and a protocol call agree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IndexArgs {
    /// Rebuild from an empty stage even when nothing looks changed.
    pub force: Option<bool>,
    /// The run's mode; `full` when absent.
    pub mode: Option<IndexMode>,
    /// Read by the composition root to pick the tree; the handler indexes the one it is bound to.
    pub repo: Option<String>,
    /// Publish the tree to the machine's registry after a successful run.
    pub register: Option<bool>,
}

impl IndexArgs {
    /// The declared default when the caller did not ask.
    #[must_use]
    pub fn force(&self) -> bool {
        self.force.unwrap_or(DEFAULT_FORCE)
    }

    /// The declared default when the caller did not ask.
    #[must_use]
    pub fn mode(&self) -> IndexMode {
        self.mode.unwrap_or(DEFAULT_MODE)
    }

    /// The declared default when the caller did not ask.
    #[must_use]
    pub fn register(&self) -> bool {
        self.register.unwrap_or(DEFAULT_REGISTER)
    }
}

#[cfg(test)]
mod tests {
    use super::{DESCRIPTION, IndexMode, MODES};

    #[test]
    fn the_mode_round_trips_through_its_spellings() {
        assert_eq!(IndexMode::Full.as_str(), "full");
        assert_eq!(IndexMode::Fast.as_str(), "fast");
        assert_eq!(IndexMode::parse("full"), Some(IndexMode::Full));
        assert_eq!(IndexMode::parse("fast"), Some(IndexMode::Fast));
        assert_eq!(IndexMode::parse("rebuild"), None);
        assert_eq!(IndexMode::parse("Full"), None);
        // The declared list and the enum's own spellings cannot drift apart.
        assert_eq!(MODES, [IndexMode::Full.as_str(), IndexMode::Fast.as_str()]);
    }

    #[test]
    fn the_description_keeps_the_sentence_an_admin_client_reads() {
        assert!(
            DESCRIPTION.starts_with("Admin: force a full or fast index, or repair a broken one. ")
        );
        // ... and the rest of §8's text, which is what keeps the tool a repair tool.
        assert!(DESCRIPTION.ends_with("the server keeps the graph current automatically."));
    }
}
