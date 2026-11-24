// src/main.rs is the daemon's composition module, not a binary target (`autobins = false` in
// Cargo.toml): rustc's special-file-name lint assumes library code never lives in main.rs.
#![allow(special_module_name)]

pub mod cli;
pub mod lifecycle;
pub mod main;
pub mod reconcile;
pub mod types;
