//! `index`: the repair tool that starts a worker and reports what it published (`TOOLS-SPEC.md` §8).
//!
//! Two files, in the order a call moves through them: [`manifest`] is the description an admin
//! client reads and the arguments the handler applies, [`handler`] is the tool's whole policy — the
//! child command, the stderr reader, the exit-status mapping, the budget pass and the registration
//! that follows a successful run.
//!
//! This is the one tool module that starts a process, and it starts only the command the resolved
//! config names ([`sangkar_core::config::AppConfig::worker_cmd`]): the writer lives in exactly one
//! process, so a tool that indexed in-process would put a WASM parser's death inside the session
//! that answered the call. Nothing here opens a database or writes to stdout.

pub mod handler;
pub mod manifest;
