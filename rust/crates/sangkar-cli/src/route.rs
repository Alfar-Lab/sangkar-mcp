//! The three post-conditions the composition root applies to every envelope, and the spec every
//! answer that never reached a handler is built from.
//!
//! Ported from `route()`, `boundarySpec()`, `uncheckedReadStatus()` and `UNKNOWN_COVERAGE` in
//! `src/index.ts`. The split from `mcp.rs` is deliberate: these are decisions about a *response*,
//! not about the protocol, so the tests that pin them need no server, no socket and no store.
//!
//! # The three post-conditions
//!
//! A handler answers its own envelope, and three of that envelope's fields are not the handler's to
//! decide:
//!
//! - **`next_tools`** is always replaced by [`next_tools_for`]. A handler that hand-wrote a routing
//!   promise would be one edit away from naming a tool this surface does not publish, and the
//!   routing table is the one place that list is reconciled with the registry.
//! - **a stale store forces a staleness disclosure.** A publish this process could not open, or a
//!   graph whose recorded `project_root` is a tree that has since moved, is a generation the reader
//!   is still answering from; `read_status.stale` and its `stale_reason` are how the answer says so
//!   instead of looking current. A handler's own reason survives a `None` here: the disclosure is
//!   applied, never cleared.
//! - **`decisive` is derived from the staleness that survived both**, and read from the final value
//!   so `stale && decisive` cannot describe one answer. It is written unconditionally, which is why
//!   no handler sets it: a handler cannot know what the guard decided after it returned.
//!
//! # The boundary spec
//!
//! An answer that never reached a handler — an unknown tool, a refused argument, a refused
//! repository, a handler that threw — still owes the client the whole envelope shape, including a
//! `read_status` and a `coverage`. [`boundary_spec`] builds that from the two facts a graph may
//! still honestly contribute (its generation and the served publish's timestamp) and nothing else:
//! the coverage is [`UNKNOWN_COVERAGE`] with `complete: false`, and the read status is
//! [`unchecked_read_status`] with `stale: true`, because a call that failed before the graph
//! answered supports no freshness claim.
//!
//! The TypeScript passes the whole `Store` to `boundarySpec` and reads the meta inside
//! `uncheckedReadStatus`. This port passes [`GraphFacts`], so this module names no store type and a
//! test can state the facts without a database on disk.

use sangkar_core::envelope::{Coverage, Envelope, EnvelopeSpec, ReadStatus, Status, fail};
use sangkar_core::errors::AppError;
use sangkar_core::instructions::next_tools_for;
use serde_json::Value;

/// The coverage a response claims when no handler assembled one.
///
/// Nothing is indexed *by this answer*, so `complete: false` and every absence claim it might
/// otherwise support is withdrawn — `total_relation` follows from the same pair through
/// [`sangkar_core::envelope::relation_for`], which is why a boundary answer is always `gte`.
pub const UNKNOWN_COVERAGE: Coverage = Coverage {
    complete: false,
    indexed: 0,
    skipped: 0,
    failed: 0,
    unresolved_edges: 0,
    not_indexed: Vec::new(),
    tests: None,
};

/// The two facts a graph can still contribute to an answer it did not produce.
///
/// Both are empty when no graph could be read at all — the ordinary state before the first publish,
/// and the state of the recorded session this surface is pinned against. The empty strings are the
/// truthful value, not a placeholder: a fabricated generation would give a cursor something to bind
/// to that no publish will ever match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphFacts {
    /// `meta.generation` of the file the reader opened.
    pub generation: String,
    /// The served publish's `published_at`; `sangkar_store`'s `GraphMeta` carries no `indexed_at`,
    /// so this is the same value every handler already publishes.
    pub indexed_at: String,
}

impl GraphFacts {
    /// No graph to read: the answer claims neither a generation nor a timestamp.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            generation: String::new(),
            indexed_at: String::new(),
        }
    }
}

/// The `read_status` a response carries when no handler built one: whatever generation the reader
/// knows and an explicit `stale: true`, because a failure or an empty index supports no freshness
/// claim — [`sangkar_core::envelope::relation_for`] then downgrades every total to `gte`.
#[must_use]
pub fn unchecked_read_status(facts: &GraphFacts, reason: &str) -> ReadStatus {
    ReadStatus {
        generation: facts.generation.clone(),
        indexed_at: facts.indexed_at.clone(),
        stale: true,
        stale_reason: Some(reason.to_owned()),
        stale_checked_at: None,
        // Nothing that skipped the handler can be the answer that ends an agent's search.
        decisive: Some(false),
    }
}

/// The spec every non-handler answer is built from, including the `next_tools` routing promise.
///
/// The reason is what the client reads as `stale_reason`, so it says *why* the graph did not
/// answer; the summary is the answer's own headline, which belongs to the call site because "no
/// tool named …" and "… could not answer" are different facts.
#[must_use]
pub fn boundary_spec(
    facts: &GraphFacts,
    tool: &str,
    summary: impl Into<String>,
    reason: impl Into<String>,
) -> EnvelopeSpec {
    EnvelopeSpec::new(
        summary,
        unchecked_read_status(facts, &reason.into()),
        UNKNOWN_COVERAGE,
    )
    .with_next_tools(next_tools_for(tool))
}

/// A failure that never reached a handler, as the envelope the client reads.
///
/// One function so the four boundary refusals (unknown tool, refused arguments, refused repository,
/// a handler that threw) cannot drift in anything but their wording.
#[must_use]
pub fn boundary_fail(
    facts: &GraphFacts,
    tool: &str,
    error: AppError,
    summary: impl Into<String>,
    reason: impl Into<String>,
) -> Envelope<Value> {
    fail(error, boundary_spec(facts, tool, summary, reason))
}

/// The tool's answer with the root's three post-conditions applied: `next_tools` from the routing
/// table (replacing whatever the handler set, never dropping the field), the guard's staleness as a
/// disclosure rather than a silent difference, and `decisive` derived from the staleness that
/// survived both.
#[must_use]
pub fn route(
    mut envelope: Envelope<Value>,
    tool: &str,
    stale_reason: Option<&str>,
) -> Envelope<Value> {
    // The `root_mismatch` downgrade is the last word on staleness, so `decisive` is read from the
    // final value: `stale && decisive` must not be able to describe the same answer.
    let stale = match stale_reason {
        None => envelope.read_status.stale,
        Some(reason) => {
            envelope.read_status.stale = true;
            envelope.read_status.stale_reason = Some(reason.to_owned());
            true
        }
    };
    envelope.read_status.decisive = Some(!stale);
    envelope.next_tools = next_tools_for(tool);
    envelope
}

/// Whether a status earns `isError: true` on the protocol result: `error` and `bootstrapping`, the
/// only two that carry an `error` object.
///
/// The contract spells the pair out; stating it as a predicate over the status keeps a future status
/// from being forgotten in one call site.
#[must_use]
pub const fn is_error_status(status: Status) -> bool {
    matches!(status, Status::Error | Status::Bootstrapping)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sangkar_core::envelope::{Coverage, TotalRelation, ok};
    use sangkar_core::errors::ErrorCode;

    /// A handler's answer, as a handler would build it: a complete envelope that names a `next_tool`
    /// the routing table does not know, and leaves `decisive` unset.
    fn handler_envelope(stale: bool) -> Envelope<Value> {
        ok(
            Value::Null,
            EnvelopeSpec::new(
                "an answer",
                ReadStatus {
                    generation: "g-1".to_owned(),
                    indexed_at: "2026-11-19T09:00:00.000Z".to_owned(),
                    stale,
                    stale_reason: None,
                    stale_checked_at: None,
                    decisive: None,
                },
                Coverage::complete(3),
            )
            .with_next_tools(["a-tool-that-does-not-exist"]),
        )
    }

    #[test]
    fn next_tools_is_replaced_by_the_routing_table() {
        let routed = route(handler_envelope(false), "trace", None);
        assert_eq!(routed.next_tools, vec!["expand".to_owned()]);

        // Even for a name the table does not know, the field is rewritten rather than kept.
        let routed = route(handler_envelope(false), "no-such-tool", None);
        assert!(routed.next_tools.is_empty());
    }

    #[test]
    fn a_root_mismatch_forces_the_staleness_disclosure() {
        let routed = route(handler_envelope(false), "summary", Some("root_mismatch"));
        assert!(routed.read_status.stale);
        assert_eq!(
            routed.read_status.stale_reason.as_deref(),
            Some("root_mismatch")
        );
        assert_eq!(routed.read_status.decisive, Some(false));
    }

    #[test]
    fn decisive_is_the_negation_of_the_staleness_that_survived() {
        // The handler's own staleness is enough, with no guard reason involved.
        let routed = route(handler_envelope(true), "status", None);
        assert!(routed.read_status.stale);
        assert_eq!(routed.read_status.decisive, Some(false));

        let routed = route(handler_envelope(false), "status", None);
        assert!(!routed.read_status.stale);
        assert_eq!(routed.read_status.decisive, Some(true));
    }

    #[test]
    fn a_boundary_answer_withdraws_every_claim() {
        let envelope = boundary_fail(
            &GraphFacts::none(),
            "status",
            AppError::new(ErrorCode::INTERNAL, "boom"),
            "status could not answer",
            "the call failed before the graph answered",
        );
        assert_eq!(envelope.status, Status::Error);
        assert!(is_error_status(envelope.status));
        assert!(envelope.payload.is_none());
        assert!(envelope.read_status.stale);
        assert_eq!(
            envelope.read_status.stale_reason.as_deref(),
            Some("the call failed before the graph answered")
        );
        assert_eq!(envelope.read_status.generation, "");
        assert_eq!(envelope.read_status.indexed_at, "");
        assert_eq!(envelope.read_status.decisive, Some(false));
        assert_eq!(envelope.coverage, UNKNOWN_COVERAGE);
        assert!(!envelope.coverage.complete);
        assert_eq!(envelope.totals.relation(), TotalRelation::Gte);
        assert_eq!(
            envelope.next_tools,
            vec!["summary".to_owned(), "search".to_owned()]
        );
        assert_eq!(
            envelope.error.expect("an error payload").code,
            ErrorCode::INTERNAL
        );
    }

    #[test]
    fn a_boundary_answer_keeps_the_generation_it_could_still_read() {
        let envelope = boundary_fail(
            &GraphFacts {
                generation: "g-42".to_owned(),
                indexed_at: "2026-11-19T09:00:00.000Z".to_owned(),
            },
            "search",
            AppError::internal("refused"),
            "search could not answer",
            "the graph was refused at startup",
        );
        assert_eq!(envelope.read_status.generation, "g-42");
        assert_eq!(envelope.read_status.indexed_at, "2026-11-19T09:00:00.000Z");
        // The routing table, not the call site's guess.
        assert_eq!(
            envelope.next_tools,
            vec!["trace".to_owned(), "expand".to_owned()]
        );
    }
}
