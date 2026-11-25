//! The default TOON rendering, the `--json` JSON rendering, and the exit-code mapping: everything the
//! two surfaces say about an envelope once a handler has produced it, in one place so the CLI and
//! the MCP answer cannot disagree about the bytes.
//!
//! Ported from `src/shared/cli/render.ts`. TOON is the default because an agent pays for the tokens
//! of every answer; `--json` stays one-line JSON for a caller that parses stdout. The encoder is
//! `serde_toon_format` 0.2.0, whose defaults (`Spaces(2)`, `Comma`) the Phase 0 spike proved render
//! every envelope byte-identically to `@toon-format/toon` 4.4.0 (`rust/spikes/toon/DECISION.md`).
//!
//! Byte discipline is the contract: [`render_json`] ends in exactly one newline, [`mcp_text`] ends
//! in none, and a stray newline on either is a failure rather than a formatting preference.

use sangkar_core::envelope::{Envelope, Status};
use sangkar_core::errors::AppError;
use serde::Serialize;

/// The code a caller returns when the envelope says the run failed.
pub const EXIT_FAILURE: i32 = 1;

/// The code for the argv or config refusal that happens before there is an envelope to read; it is
/// returned by the argument layer, never derived from a status.
pub const EXIT_USAGE: i32 = 2;

/// The envelope as one line of compact JSON plus a terminating newline: the `--json` rendering, and
/// the shape a caller that captures stdout can parse without knowing TOON.
///
/// Key order is the envelope's own declaration order and every payload object keeps the order its
/// handler wrote (`preserve_order`), so the line is byte-identical to
/// `JSON.stringify(envelope)`.
pub fn render_json<T: Serialize>(envelope: &Envelope<T>) -> Result<String, AppError> {
    let mut line = serde_json::to_string(envelope).map_err(|error| {
        AppError::internal(format!(
            "the envelope could not be rendered as JSON: {error}"
        ))
    })?;
    line.push('\n');
    Ok(line)
}

/// The default rendering: the envelope as TOON (`Token-Oriented Object Notation`) plus a terminating
/// newline. TOON spells the same JSON data model in fewer tokens, which is what an agent pays for on
/// every answer; a decoder turns it back into the identical value, so nothing is lost by not being
/// JSON.
pub fn render_toon<T: Serialize>(envelope: &Envelope<T>) -> Result<String, AppError> {
    let mut rendered = serde_toon::to_string(envelope).map_err(|error| {
        AppError::internal(format!(
            "the envelope could not be rendered as TOON: {error}"
        ))
    })?;
    rendered.push('\n');
    Ok(rendered)
}

/// One envelope, in whichever of the two forms the caller asked for. The only place a CLI rendering
/// is chosen, so `--json` cannot mean one thing for the tool commands and another for `mcp`.
pub fn render_output<T: Serialize>(envelope: &Envelope<T>, json: bool) -> Result<String, AppError> {
    if json {
        render_json(envelope)
    } else {
        render_toon(envelope)
    }
}

/// The MCP `tools/call` text: the same choice without the trailing newline, because a protocol result
/// is a string field rather than a line stream and the JSON it replaces never ended in one.
pub fn mcp_text<T: Serialize>(envelope: &Envelope<T>, json: bool) -> Result<String, AppError> {
    if json {
        serde_json::to_string(envelope).map_err(|error| {
            AppError::internal(format!(
                "the envelope could not be rendered as JSON: {error}"
            ))
        })
    } else {
        serde_toon::to_string(envelope).map_err(|error| {
            AppError::internal(format!(
                "the envelope could not be rendered as TOON: {error}"
            ))
        })
    }
}

/// The process code an envelope earns: a positive or negative answer is 0, the two failures are not.
#[must_use]
pub fn exit_code_of<T>(envelope: &Envelope<T>) -> i32 {
    match envelope.status {
        Status::Error | Status::Bootstrapping => EXIT_FAILURE,
        Status::Ok | Status::Ambiguous | Status::NotFound => 0,
    }
}
