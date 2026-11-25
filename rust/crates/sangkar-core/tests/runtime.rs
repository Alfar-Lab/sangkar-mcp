//! The process facts, checked against the target and the machine the suite actually runs on.
//!
//! Two of these are probes rather than product claims: `os()`/`arch()` must agree with
//! `std::env::consts` because that is where they read from, and `git_available()` answers for the
//! host it is called on.

use std::env::consts;

use sangkar_core::runtime::{
    arch, current_exe, git_available, os, runtime_version, server_version,
};

#[test]
fn server_version_matches_the_manifest_version() {
    // The test target belongs to this package, so `env!` here and in the module resolve to the same
    // workspace version: a version bump keeps this green instead of turning it into a chore.
    assert_eq!(server_version(), env!("CARGO_PKG_VERSION"));
    assert!(
        server_version()
            .split(['.', '-'])
            .take(2)
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())),
        "{} does not start with a numeric major.minor",
        server_version()
    );
}

#[test]
fn os_and_arch_are_what_std_reports_for_the_target() {
    assert_eq!(os(), consts::OS);
    assert_eq!(arch(), consts::ARCH);
}

#[test]
fn current_exe_is_a_real_file_while_the_suite_runs() {
    let exe = current_exe().expect("the test binary has a path");
    assert!(exe.is_absolute(), "{} is not absolute", exe.display());
    assert!(exe.is_file(), "{} does not exist", exe.display());
}

#[test]
fn runtime_version_names_the_compiler_that_built_the_crate() {
    let version = runtime_version();
    // `rustc --version`, captured by `build.rs`; `unknown` is its documented fallback for a host
    // where rustc could not be invoked at build time.
    assert!(
        version == "unknown" || version.starts_with("rustc "),
        "unexpected runtime version {version:?}"
    );
}

#[test]
fn git_available_sees_the_git_this_machine_has() {
    // Environment-dependent on purpose: `git_available()` is a probe, and a host without git gets
    // `false` rather than a panic. git is installed here, so the probe has to find it.
    assert!(git_available(), "git is on PATH on this machine");
}
