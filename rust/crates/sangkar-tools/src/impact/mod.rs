//! `impact`: what a change set can reach, how far, through what, and whether it is tested
//! (`TOOLS-SPEC.md` §6).
//!
//! Two files, in the order a call moves through them: [`manifest`] is the description a model reads
//! and the arguments the handler parses, [`handler`] is the tool's whole policy — the change set,
//! one inbound walk, the test-coverage join, the unresolved census and the page boundary. Nothing
//! here reads a file, opens a connection or writes to stdout.

pub mod handler;
pub mod manifest;
