//! Records the toolchain that compiled this crate as `RUSTC_VERSION`, for
//! `sangkar_core::runtime::runtime_version()`.
//!
//! A build script rather than a constant written by the packaging script: this crate has to build
//! from a plain `cargo build` in a fresh checkout, where no release tooling is present. Reading
//! `rustc --version` here also keeps the fact fixed to the compiler behind the binary instead of
//! whichever `rustc` happens to be on PATH at run time. Nothing here touches the network, and a
//! host where `rustc` cannot be executed leaves the value at `unknown` rather than failing a build
//! cargo had already accepted.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let version = Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=RUSTC_VERSION={version}");
}
