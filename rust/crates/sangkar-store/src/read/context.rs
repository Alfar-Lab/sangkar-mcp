//! The context every tool handler receives (`TECH-SPEC.md` §6, `plan.constraint_invariant.md`).
//!
//! Ported from `src/shared/store/read/context.ts`. It is the whole reason a handler is testable
//! without a database and without a server: the reader, the cursor pair, the logger, the clock, the
//! resolved config and the size recorded for the file the reader opened all arrive as values, so a
//! handler never opens a connection, never reads the environment and never mints an envelope from
//! ambient state. `handle(ctx, args)` is therefore the same signature in every tool, and a
//! [`ToolContext`] built from a fake reader is a complete unit-test seam.
//!
//! This module imports nothing from the tool layer and nothing from the composition root: a shared
//! module that reached for the root would make the reader impossible to fake. Nothing here opens a
//! connection, reads a file or consults the environment; the one side effect is [`Logger`]'s default
//! sink, which delegates to [`sangkar_core::log`] — the workspace's only stderr writer.
//!
//! Two porting decisions are visible in the types:
//!
//! - **The clock is `fn() -> String`**, in the ISO-8601-with-milliseconds form
//!   `new Date(millis).toISOString()` produces (`2024-02-29T12:34:56.789Z`), not a
//!   `DateTime<Utc>`. No date/time crate is in `RUST-MIGRATION.md` §5, and `sangkar_core::log`
//!   hand-rolls exactly this string for the same reason. The string is the form both sides of a
//!   staleness comparison already use — `generation.published_at`, `meta` values and the
//!   `stale_checked_at`/`indexed_at` payload fields are all that spelling — and inside the format
//!   lexicographic order *is* chronological order, so `now > published_at` needs no parser. The one
//!   TypeScript call site that subtracted dates (`index/handler.ts`'s elapsed milliseconds) becomes
//!   a `std::time::Instant` pair in the tools phase, which is the right instrument for a duration
//!   and needs no formatting at all.
//! - **`cursor` is `Rc<dyn CursorFacade>`**, like `reader`. Both are seams a handler must be able to
//!   fake; the TypeScript gets that from structural typing, Rust needs the trait object.
//!
//! `db_size_bytes` is a `u64` rather than the TypeScript's optional `number | null`: a context is
//! only built around an open handle, and [`crate::db::open_reader`] refuses a missing file, so
//! "no file was served" is a state with no context instead of a field with no value.

use std::rc::Rc;

use sangkar_core::config::AppConfig;
use sangkar_core::cursor::{Cursor, CursorExpectation, CursorPayload, CursorResult};
use sangkar_core::log::Level;

use crate::read::reader::GraphReader;

/// The cursor seam: sign a page token, verify a resumed one.
///
/// Structurally identical to [`sangkar_core::cursor::Cursor`], which is what the composition root
/// binds to one key and one repository, so a handler can mint and verify a cursor but cannot reach
/// the key behind it. The generation is part of [`CursorExpectation`], so expiry is not the
/// handler's decision to make: a token from another generation verifies to `cursor_expired`, never
/// to a wrong page (`DATABASE.md` §8).
pub trait CursorFacade {
    fn sign(&self, payload: &CursorPayload) -> String;
    /// `expected` is `{generation, tool, args_hash}`: what the resuming call claims to be.
    fn verify(&self, token: &str, expected: &CursorExpectation) -> CursorResult;
}

/// The real facade is the bound [`Cursor`] itself, so the root hands one over without an adapter.
impl CursorFacade for Cursor {
    fn sign(&self, payload: &CursorPayload) -> String {
        Cursor::sign(self, payload)
    }

    fn verify(&self, token: &str, expected: &CursorExpectation) -> CursorResult {
        Cursor::verify(self, token, expected)
    }
}

/// One structured field of a log line.
///
/// A store-owned enum rather than `serde_json::Value` because this crate depends on neither `serde`
/// nor `serde_json` (`nodes.rs` and `coverage.rs` document the same boundary for their rows): the
/// three variants are the scalar shapes every call site in the TypeScript logger passes, and
/// [`Logger`] narrows them to the core writer's own values at the boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogField {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl From<&str> for LogField {
    fn from(value: &str) -> Self {
        Self::Str(value.to_owned())
    }
}

impl From<String> for LogField {
    fn from(value: String) -> Self {
        Self::Str(value)
    }
}

impl From<i64> for LogField {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<bool> for LogField {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

/// Where a [`Logger`]'s line goes. The default is [`sangkar_core::log::log`], the workspace's only
/// stderr writer; a test installs a sink and reads its own lines back.
pub type LogSink = fn(Level, &str, &[(&str, LogField)]);

/// The logger a handler holds: `sangkar_core::log`'s four levels, pre-scoped to the tool name so a
/// log line is attributable without every call site repeating it.
///
/// A concrete struct with an injectable sink rather than four free functions, because a handler
/// test has to be able to see what was logged without capturing the process's stderr.
#[derive(Clone, Debug)]
pub struct Logger {
    tool: String,
    sink: LogSink,
}

impl Logger {
    /// The production logger for `tool`.
    pub fn new(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            sink: stderr_sink,
        }
    }

    /// A logger whose lines go to `sink`, for a test that asserts what was reported.
    #[must_use]
    pub fn with_sink(tool: impl Into<String>, sink: LogSink) -> Self {
        Self {
            tool: tool.into(),
            sink,
        }
    }

    /// The scope this logger stamps into every line.
    #[must_use]
    pub fn tool(&self) -> &str {
        &self.tool
    }

    pub fn debug(&self, msg: &str, fields: &[(&str, LogField)]) {
        self.emit(Level::Debug, msg, fields);
    }

    pub fn info(&self, msg: &str, fields: &[(&str, LogField)]) {
        self.emit(Level::Info, msg, fields);
    }

    pub fn warn(&self, msg: &str, fields: &[(&str, LogField)]) {
        self.emit(Level::Warn, msg, fields);
    }

    pub fn error(&self, msg: &str, fields: &[(&str, LogField)]) {
        self.emit(Level::Error, msg, fields);
    }

    /// The scope goes first, so a line carries it even when a call site passes no fields.
    fn emit(&self, level: Level, msg: &str, fields: &[(&str, LogField)]) {
        let mut all: Vec<(&str, LogField)> = Vec::with_capacity(fields.len() + 1);
        all.push(("tool", LogField::Str(self.tool.clone())));
        all.extend(fields.iter().cloned());
        (self.sink)(level, msg, &all);
    }
}

/// Attaches the scope and hands the line to [`sangkar_core::log`], which owns the `ts`, the level
/// spelling, the escaping and the single stderr write.
///
/// The value type is the core writer's, and inference names it through the call below, so this
/// module never has to write `serde_json::Value` down (it has no such dependency).
fn stderr_sink(level: Level, msg: &str, fields: &[(&str, LogField)]) {
    let mut all = Vec::with_capacity(fields.len());
    for (key, field) in fields {
        let value = match field {
            LogField::Str(text) => text.as_str().into(),
            LogField::Int(number) => (*number).into(),
            LogField::Bool(flag) => (*flag).into(),
        };
        all.push((*key, value));
    }
    sangkar_core::log::log(level, msg, &all);
}

/// The clock a context hands out: the current instant as an ISO-8601 string with milliseconds.
pub type Clock = fn() -> String;

/// Everything a handler may know about the process. The six fields are the entire seam:
///
/// - `reader` is the aggregate [`GraphReader`]; there is no connection anywhere in this type, so a
///   tool cannot query SQL even by accident;
/// - `cursor` is bound to one key and one repository shape;
/// - `log` is pre-scoped to the tool name, and nothing is ever written to stdout (that is the MCP
///   protocol);
/// - `now` is injected, so staleness and cursor expiry are testable rather than wall-clock;
/// - `config` is the resolved configuration, so no handler reads the environment;
/// - `db_size_bytes` is the size recorded for the file this reader opened — read once from the file
///   fingerprint and never re-measured, because a publish renames a new file over `graph.db` and a
///   fresh `stat` can describe a file this handle is not serving.
pub struct ToolContext {
    pub reader: Rc<dyn GraphReader>,
    pub cursor: Rc<dyn CursorFacade>,
    pub log: Logger,
    pub now: Clock,
    pub config: Rc<AppConfig>,
    pub db_size_bytes: u64,
}
