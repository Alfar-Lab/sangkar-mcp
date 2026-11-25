//! `trace`: one free-text query in, the resolved symbol and its neighbours out (`TOOLS-SPEC.md` §3).
//!
//! Two files, in the order a call moves through them: [`manifest`] is the description a model reads
//! and the arguments the handler parses, [`handler`] is the tool's whole policy — resolution, the
//! bounded walk, the unresolved census and the page boundary. Nothing here reads a file, opens a
//! connection or writes to stdout.

pub mod handler;
pub mod manifest;
