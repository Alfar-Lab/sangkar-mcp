//! The process facts telemetry records and the worker's default command need, with no interpreter
//! in the picture.
//!
//! Ported from `src/shared/runtime.ts`, of which only the facts survive: the Bun version gate retired
//! with the interpreter, so nothing in this binary produces exit code 70. `EX_SOFTWARE` stays with
//! the npm launcher shim, which owns the one setup fault that can still happen before this binary
//! starts — a platform package missing from the install. No Rust path emits 70.
//!
//! [`runtime_version`] is what fills the telemetry field `bun_version`. That field keeps its name
//! for record-shape compatibility and carries this runtime's version string; renaming it would
//! change the shape of every call record. Nothing reads it (`src/shared/telemetry/report.ts`), so
//! the mismatch never reaches a report.

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// The version this binary reports: this crate's manifest version at compile time, which is the
/// workspace version — not a copy of it, so a bump needs no edit here.
#[must_use]
pub fn server_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// `rustc <version>`, captured when this crate was built (see `build.rs`).
///
/// Read from the build, not from PATH now: the record should name the compiler behind the binary,
/// and a host may have no `rustc` at all.
#[must_use]
pub fn runtime_version() -> String {
    env!("RUSTC_VERSION").to_owned()
}

/// [`std::env::consts::OS`]: `linux`, `macos`, `windows`.
#[must_use]
pub fn os() -> &'static str {
    std::env::consts::OS
}

/// [`std::env::consts::ARCH`]: `x86_64`, `aarch64`.
#[must_use]
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The running binary's own path, as the platform reports it.
///
/// # Errors
///
/// Propagates [`std::env::current_exe`], which fails when the platform cannot report the path —
/// not merely when the file has been moved.
pub fn current_exe() -> std::io::Result<PathBuf> {
    std::env::current_exe()
}

/// Whether `git` can be executed.
///
/// `git` is spawned as an argv, never through a shell, with stdin closed and `GIT_PAGER=cat`, so a
/// missing git, a hosted pager or a terminal that never answers all end as `false` instead of a
/// hang or a panic.
#[must_use]
pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .env("GIT_PAGER", "cat")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}
