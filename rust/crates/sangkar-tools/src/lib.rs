//! The seven tools, one module each, plus the registry that publishes them.
//!
//! A tool module is `manifest.rs` (the name, the description and the declared arguments) beside
//! `handler.rs` (what the tool does). [`registry`] is the crate root's one file that names every
//! tool: `tools/list`, the CLI and the protocol layer all read the published surface from there, and
//! `sangkar-boundary`'s isolation rule is written against `src/<tool>/`, not against this root —
//! reconciling seven manifests is the one job that has to see all seven.

pub mod expand;
pub mod impact;
pub mod index;
pub mod registry;
pub mod search;
pub mod status;
pub mod summary;
pub mod trace;
