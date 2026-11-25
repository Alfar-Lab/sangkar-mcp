//! `search`: one free-text query in, the ranked declarations it names out (`TOOLS-SPEC.md` §5).
//!
//! Two files, in the order a call moves through them: [`manifest`] is the description a model reads
//! and the arguments the handler parses, [`handler`] is the tool's whole policy — the FTS5 match
//! expression, the prefix and kind filters, the degree join and the page boundary. Nothing here
//! reads a file, opens a connection or writes to stdout.

pub mod handler;
pub mod manifest;
