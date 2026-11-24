//! The boundary gate as the workspace's own test.
//!
//! Each rule is proved twice: against the real tree, and against a synthetic input that makes it
//! fire. The second half matters most — a scanner that quietly stopped matching would pass every
//! assertion that only looks at a clean tree.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_boundary::{
    CrateManifest, DAG, Dependency, PHASE_MARKER, check_dag, check_rusqlite, parse_manifest,
    phase_marker_violation, scan_sources, scan_workspace, stdout_violation,
    tool_isolation_violation, tool_modules,
};

/// `rust/`, derived from this crate's own location — never the process CWD, so the test passes
/// from any directory.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

#[test]
fn manifest_scanner_reads_sections_and_inline_tables() {
    let text = r#"
# serde = "0.0" in a comment is not a dependency
[package]
name = "sangkar-sample"
version.workspace = true

[dependencies]
sangkar-core = { path = "../sangkar-core" }
serde = "1.0" # the version pin is not this scanner's business
rusqlite = { version = "0.43", features = ["bundled"] }
tokio = {
  version = "1",
  features = ["macros"],
}

[dev-dependencies]
sangkar-cli = { path = "../sangkar-cli" }

[[bin]]
name = "sangkar"
"#;
    let manifest = parse_manifest(text);
    assert_eq!(
        manifest.package.as_deref(),
        Some("sangkar-sample"),
        "the [package] name, not the [[bin]] name"
    );
    let dependencies: Vec<(&str, &str, Option<&str>)> = manifest
        .dependencies
        .iter()
        .map(|dep| (dep.name.as_str(), dep.table.as_str(), dep.path.as_deref()))
        .collect();
    assert_eq!(
        dependencies,
        vec![
            ("sangkar-core", "dependencies", Some("../sangkar-core")),
            ("serde", "dependencies", None),
            ("rusqlite", "dependencies", None),
            // The multi-line inline table is one dependency, not three.
            ("tokio", "dependencies", None),
            ("sangkar-cli", "dev-dependencies", Some("../sangkar-cli")),
        ]
    );
}

#[test]
fn the_real_crate_dag_matches_the_contract() {
    let report = check_dag(&workspace_root()).expect("read rust/crates/*/Cargo.toml");
    println!(
        "sangkar-boundary: scanned {} crate manifests, checked {} sangkar-* edges in both directions",
        report.manifests, report.edges
    );
    assert!(
        report.violations.is_empty(),
        "the crate DAG is not the contract:\n{}",
        report.violations.join("\n")
    );
    assert_eq!(
        report.manifests,
        DAG.len(),
        "one manifest per crate in the table"
    );
}

#[test]
fn rusqlite_appears_only_in_the_store() {
    let manifests: Vec<CrateManifest> = scan_workspace(&workspace_root()).expect("read manifests");
    let violations = check_rusqlite(&manifests);
    assert!(violations.is_empty(), "{}", violations.join("\n"));

    // A second crate opening SQLite must be refused: the rule has to fire, not just hold.
    let mut moved = manifests;
    let tools = moved
        .iter_mut()
        .find(|manifest| manifest.name == "sangkar-tools")
        .expect("sangkar-tools has a manifest");
    tools.dependencies.push(Dependency {
        name: "rusqlite".to_string(),
        table: "dependencies".to_string(),
        path: None,
    });
    let violations = check_rusqlite(&moved);
    assert_eq!(
        violations.len(),
        1,
        "only the moved rusqlite may fail: {violations:?}"
    );
    assert!(
        violations[0].contains("sangkar-tools -> rusqlite"),
        "the message names the crate and the edge: {}",
        violations[0]
    );
}

#[test]
fn a_forbidden_core_to_store_edge_is_rejected() {
    let fixture = TempTree::mirror(&workspace_root()).expect("mirror rust/crates into a temp tree");

    // The copy of the real tree passes first: the rejection below is the edit, not the fixture.
    let clean = check_dag(fixture.path()).expect("scan the copy");
    assert!(
        clean.violations.is_empty(),
        "the untouched copy must pass: {:?}",
        clean.violations
    );
    assert_eq!(
        clean.manifests,
        DAG.len(),
        "the fixture mirrors every crate"
    );

    // The error a careless edit makes: one dependency line in the wrong table.
    let core = fixture
        .path()
        .join("crates")
        .join("sangkar-core")
        .join("Cargo.toml");
    let mut manifest = fs::read_to_string(&core).expect("read core's manifest copy");
    let anchor = "[dependencies]\n";
    let at = manifest
        .find(anchor)
        .expect("sangkar-core has a [dependencies] table")
        + anchor.len();
    manifest.insert_str(at, "sangkar-store = { path = \"../sangkar-store\" }\n");
    fs::write(&core, manifest).expect("write core's manifest copy");

    let broken = check_dag(fixture.path()).expect("scan the edited copy");
    let named: Vec<&String> = broken
        .violations
        .iter()
        .filter(|violation| violation.contains("sangkar-core -> sangkar-store"))
        .collect();
    println!(
        "sangkar-boundary: injected edge rejected with {} violation(s): {named:?}",
        broken.violations.len()
    );
    assert_eq!(
        named.len(),
        1,
        "the injected edge is named exactly once: {:?}",
        broken.violations
    );
    let unrelated: Vec<&String> = broken
        .violations
        .iter()
        .filter(|violation| !violation.contains("sangkar-core -> sangkar-store"))
        .collect();
    assert!(
        unrelated.is_empty(),
        "the fixture's other crates stay clean: {unrelated:?}"
    );
}

#[test]
fn every_source_rule_holds_for_the_real_tree() {
    let report = scan_sources(&workspace_root()).expect("scan crates/*/src and crates/*/tests");
    println!(
        "sangkar-boundary: scanned {} source files under crates/*/src and crates/*/tests, {} violations",
        report.files,
        report.violations.len()
    );
    assert!(
        report.violations.is_empty(),
        "source rule violations:\n{}",
        report.violations.join("\n")
    );

    // The stdout exemption is the walker's, not a file list: this test file calls println! to
    // print the counts above, and the same rule would reject it under `src/`.
    let own_source = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("dag.rs"),
    )
    .expect("read this test's own source");
    assert!(
        stdout_violation("crates/sangkar-boundary/tests/dag.rs", &own_source).is_some(),
        "a test file is exempt by path, not because the rule stopped matching"
    );
}

#[test]
fn stdout_is_reserved_for_the_protocol() {
    let banned = [
        "fn main() { println!(\"hi\"); }",
        "fn main() { print!(\"{}\", 1); }",
        "fn main() { eprintln!(\"hi\"); }",
        "fn run(stdout: &mut impl std::io::Write) { let _ = std::io::stdout(); }",
    ];
    for source in banned {
        assert!(
            stdout_violation("crates/sangkar-cli/src/main.rs", source).is_some(),
            "this source must be rejected: {source}"
        );
    }

    // The exemption that keeps the scan honest: prose and messages are not calls. This is the
    // shape sangkar-core/src/log.rs will have — a doc comment naming the macro it replaces, and a
    // string that quotes it.
    let logger = "\
/// Never call println! here: the protocol owns stdout.
pub fn note() -> &'static str { \"stdout is not println!, and not std::io::stdout\" }
/* eprintln! is equally banned; a block comment is not code. */
pub fn writer() -> std::io::Stderr { std::io::stderr() }
pub const HELPFUL: &str = r#\"raw strings may say println! too\"#;
";
    assert!(
        stdout_violation("crates/sangkar-core/src/log.rs", logger).is_none(),
        "a comment or a string literal is not a call"
    );
}

#[test]
fn a_tool_module_may_not_name_another_tool() {
    let root = workspace_root();
    let tools = tool_modules(&root).expect("list the tool modules");
    assert!(
        tools.contains(&"trace".to_string()) && tools.contains(&"search".to_string()),
        "the real layout is src/<tool>/: {tools:?}"
    );

    let crossed = "use crate::trace::handler::TraceTool;\n";
    assert!(
        tool_isolation_violation("crates/sangkar-tools/src/search/mod.rs", crossed, &tools)
            .is_some(),
        "search may not name trace"
    );
    let grouped = "use crate::{search, trace};\n";
    assert!(
        tool_isolation_violation("crates/sangkar-tools/src/impact/mod.rs", grouped, &tools)
            .is_some(),
        "a braced import is still a reference"
    );
    let legacy_path = "use crate::tools::trace;\n";
    assert!(
        tool_isolation_violation("crates/sangkar-tools/src/lib.rs", legacy_path, &tools).is_some(),
        "there is no crate::tools to name"
    );

    // Siblings, the crate root and the shared crates are all fine.
    let siblings = "pub mod handler;\npub mod manifest;\nuse crate::search::manifest::Manifest;\n";
    assert!(
        tool_isolation_violation("crates/sangkar-tools/src/search/mod.rs", siblings, &tools)
            .is_none(),
        "a tool owns its own directory"
    );
    assert!(
        tool_isolation_violation(
            "crates/sangkar-tools/src/search/handler.rs",
            "use sangkar_core::envelope::Envelope;\n",
            &tools
        )
        .is_none(),
        "shared machinery comes from the crates the DAG allows"
    );
    assert!(
        tool_isolation_violation(
            "crates/sangkar-tools/src/lib.rs",
            "pub mod trace;\n",
            &tools
        )
        .is_none(),
        "the crate root declares the tool modules"
    );
    assert!(
        tool_isolation_violation("crates/sangkar-core/src/lib.rs", crossed, &tools).is_none(),
        "the rule is about the tools crate only"
    );
}

#[test]
fn no_phase_hand_off_markers() {
    let marker = format!("// {PHASE_MARKER} 3 fills this in");
    assert!(
        phase_marker_violation("crates/sangkar-cli/src/main.rs", &marker).is_some(),
        "the marker is the violation"
    );
    assert!(
        phase_marker_violation("crates/sangkar-cli/src/main.rs", "// TODO: a real note").is_none(),
        "a plain TODO is a note, not a phase hand-off"
    );
}

/// A temporary copy of `rust/crates`, so an injected violation never touches the real tree.
struct TempTree(PathBuf);

impl TempTree {
    fn mirror(root: &Path) -> std::io::Result<Self> {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let base = std::env::temp_dir().join(format!(
            "sangkar-boundary-{}-{nanos}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let crates = base.join("crates");
        fs::create_dir_all(&crates)?;
        for entry in fs::read_dir(root.join("crates"))? {
            let entry = entry?;
            let source = entry.path().join("Cargo.toml");
            if !source.is_file() {
                continue;
            }
            let destination = crates.join(entry.file_name());
            fs::create_dir_all(&destination)?;
            fs::copy(&source, destination.join("Cargo.toml"))?;
        }
        Ok(Self(base))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
