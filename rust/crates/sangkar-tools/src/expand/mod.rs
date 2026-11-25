//! `expand`: one target in, one detail view out (`TOOLS-SPEC.md` §7).
//!
//! Two files, in the order a call moves through them: [`manifest`] is the description a model reads
//! and the six arguments the handler parses, [`handler`] is the tool's whole policy — resolution,
//! the kind-to-`what` inference, the five detail modes, the source window, the drift check and the
//! disclosure of both. Nothing here writes to stdout, opens a connection or reads the environment;
//! `what: "source"` is the one mode that touches the working tree, and it is permitted to by this
//! tool's contract.
//!
//! `expand` is the leaf of the detail chain, which is what lets the other six tools answer with
//! summaries: detail is addressable here instead of pre-shipped everywhere. That also fixes its
//! continuation story — [`manifest::PARAMETERS`] declares no `cursor`, because a call that was
//! truncated on purpose is finished rather than resumable.

pub mod handler;
pub mod manifest;
