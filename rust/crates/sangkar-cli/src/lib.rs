//! The `sangkar` command line as a library, so the module tree has exactly one home.
//!
//! `src/main.rs` is a thin binary over this root: it declares no modules of its own. Declaring the
//! tree in both places would compile every module twice, and a module an integration test links
//! would then not be the module the binary runs — the failure mode this crate most needs to avoid,
//! because the store lifecycle is decided by the code under test, not by a copy of it.
//!
//! The verbs arrive with the dispatch tasks. Until then this root only names the modules, which is
//! what lets `tests/store_refresh.rs` link `store_lifecycle` at all: an integration test cannot
//! link a binary-only crate.

pub mod args;
pub mod dispatch;
pub mod index_gate;
pub mod mcp;
pub mod render;
pub mod repo;
pub mod route;
pub mod store_lifecycle;
