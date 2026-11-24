//! The boundary gate: the module rules of `.agents/prompts/rules.md` as a test failure instead of
//! a review comment.
//!
//! Every check here is a pure function of text, so a synthetic input can prove a rule fires
//! without adding fixture files under `rust/crates`. The crate has no dependencies by design: it
//! reads `rust/crates/*/Cargo.toml` and `rust/crates/*/{src,tests}/**/*.rs` as text. A gate that
//! linked the crates it polices would be describing them through the compiler, not as written.
//!
//! What it checks:
//!
//! - the crate DAG: each `sangkar-*` dependency of a crate is exactly one of the edges in [`DAG`],
//!   in both directions (no undeclared edge, no missing one);
//! - `rusqlite` in `sangkar-store` and nowhere else, so SQLite stays behind one crate;
//! - stdout discipline: no `println!`, `print!`, `eprintln!` or `std::io::stdout` in a `src/`
//!   file — stdout is the MCP protocol, and log lines go to stderr through
//!   `sangkar-core/src/log.rs`;
//! - tool isolation: a tool module never names another tool module;
//! - no hand-off markers: deferring work to a later phase is a plan violation, not a comment.
//!
//! `build.rs` files are outside the source scan: a build script writes to cargo's own pipes, never
//! to the protocol stdout of a running `sangkar` process.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The crate DAG, exactly: for each crate, the `sangkar-*` crates it may depend on.
///
/// The daemon never depends on the cli, and the cli is the only crate that names `tools`, `index`
/// and `daemon` together, so no cycle is possible by construction.
pub const DAG: [(&str, &[&str]); 9] = [
    ("sangkar-core", &[]),
    ("sangkar-store", &["sangkar-core"]),
    ("sangkar-domain", &["sangkar-core", "sangkar-store"]),
    (
        "sangkar-tools",
        &["sangkar-core", "sangkar-store", "sangkar-domain"],
    ),
    (
        "sangkar-index",
        &["sangkar-core", "sangkar-store", "sangkar-domain"],
    ),
    ("sangkar-telemetry", &["sangkar-core"]),
    (
        "sangkar-daemon",
        &[
            "sangkar-core",
            "sangkar-store",
            "sangkar-index",
            "sangkar-telemetry",
        ],
    ),
    (
        "sangkar-cli",
        &[
            "sangkar-core",
            "sangkar-store",
            "sangkar-domain",
            "sangkar-tools",
            "sangkar-index",
            "sangkar-telemetry",
            "sangkar-daemon",
        ],
    ),
    ("sangkar-boundary", &[]),
];

/// The prefix that makes a dependency this workspace's business: third-party edges are free.
const DEPENDENCY_PREFIX: &str = "sangkar-";

/// Every manifest table that can declare a dependency.
const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

/// The SQLite crate: `sangkar-store` owns it so no other crate can open a connection.
const SQLITE_CRATE: &str = "rusqlite";

/// The crates that own SQLite.
const STORE_CRATE: &str = "sangkar-store";

/// The macros banned from `src/`, because the only writer to stdout is the protocol frame.
const STDOUT_MACROS: [&str; 3] = ["print", "println", "eprintln"];

/// `std::io::stdout`, assembled so this file does not match its own scan.
const STDOUT_HANDLE: &str = concat!("std", "::io::stdout");

/// The phase hand-off marker, assembled for the same reason: rule 3 scans raw source, comments
/// included, and this crate's `src/lib.rs` is one of the files it scans.
pub const PHASE_MARKER: &str = concat!("TODO", "(phase");

/// `crates/sangkar-tools/src/`, relative to the workspace root `rust/`.
const TOOLS_SRC_PREFIX: &str = "crates/sangkar-tools/src/";

// -------------------------------------------------------------------------------------------
// Manifest scanner
// -------------------------------------------------------------------------------------------

/// One `key = value` line inside a dependency table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    /// The table it was declared in, without brackets: `dependencies`, `dev-dependencies`, …
    pub table: String,
    /// The `path = "…"` of an inline table, when the dependency is written as one.
    pub path: Option<String>,
}

/// What the DAG needs from a `Cargo.toml`: the package name and its dependencies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    pub package: Option<String>,
    pub dependencies: Vec<Dependency>,
}

/// Reads the manifest tables the DAG is written in.
///
/// Handles what a crate manifest may write: section headers including `[[bin]]`, single-line and
/// multi-line inline tables, plain version strings, dotted keys (`dep.path = "…"`), and `#`
/// comments. An array value — a `features = ["a", "b"]` list, say — is not a dependency
/// line, and only the last segment of a target-qualified table names the table.
pub fn parse_manifest(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    let mut table = String::new();
    let mut lines = text.lines();
    while let Some(raw) = lines.next() {
        let mut statement = strip_comment(raw).trim().to_string();
        if statement.is_empty() {
            continue;
        }
        if let Some(header) = section_header(&statement) {
            table = header;
            continue;
        }
        // An inline table may be spread over several lines.
        while brace_depth(&statement) > 0 {
            let Some(next) = lines.next() else { break };
            statement.push(' ');
            statement.push_str(strip_comment(next).trim());
        }
        let Some((key, value)) = statement.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key.is_empty() {
            continue;
        }
        if table == "package" && key == "name" {
            manifest.package = string_value(value);
            continue;
        }
        if !is_dependency_table(&table) {
            continue;
        }
        match key.split_once('.') {
            None => manifest.dependencies.push(Dependency {
                name: key.to_string(),
                table: table.clone(),
                path: inline_field(value, "path"),
            }),
            Some((name, field)) => {
                // `dep.version = "1"` extends a dependency declared on another line.
                let Some(existing) = manifest
                    .dependencies
                    .iter_mut()
                    .find(|dep| dep.name == name && dep.table == table)
                else {
                    continue;
                };
                if field == "path" {
                    existing.path = string_value(value).or_else(|| inline_field(value, "path"));
                }
            }
        }
    }
    manifest
}

/// Cuts a `#` comment, leaving `#` inside a quoted value alone.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if in_string => index += 1,
            b'"' => in_string = !in_string,
            b'#' if !in_string => return &line[..index],
            _ => {}
        }
        index += 1;
    }
    line
}

/// `[dependencies]` → `dependencies`, `[[bin]]` → `bin`, a value line → `None`.
fn section_header(statement: &str) -> Option<String> {
    if !statement.starts_with('[') || !statement.ends_with(']') {
        return None;
    }
    Some(
        statement
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim()
            .to_string(),
    )
}

/// How many `{` are still open outside string literals.
fn brace_depth(statement: &str) -> i32 {
    let mut depth = 0;
    let mut in_string = false;
    let mut escaped = false;
    for ch in statement.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => depth -= 1,
            _ => {}
        }
    }
    depth
}

/// `[target.'cfg(unix)'.dependencies]` counts: only the last segment names the table.
fn is_dependency_table(table: &str) -> bool {
    table
        .rsplit('.')
        .next()
        .is_some_and(|last| DEPENDENCY_TABLES.contains(&last))
}

/// The contents of a quoted value.
fn string_value(value: &str) -> Option<String> {
    let inner = value.trim().strip_prefix('"')?.strip_suffix('"')?;
    Some(inner.to_string())
}

/// A field of a `{ … }` inline table.
fn inline_field(value: &str, field: &str) -> Option<String> {
    let inner = value.trim().strip_prefix('{')?.strip_suffix('}')?;
    for part in split_top_level(inner, ',') {
        let Some((key, val)) = part.split_once('=') else {
            continue;
        };
        if key.trim().trim_matches('"') == field {
            return string_value(val);
        }
    }
    None
}

/// Splits on `separator` outside braces, brackets and strings.
fn split_top_level(input: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, ch) in input.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ if ch == separator && depth == 0 => {
                parts.push(&input[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

// -------------------------------------------------------------------------------------------
// The crate DAG
// -------------------------------------------------------------------------------------------

/// A crate manifest as the DAG sees it.
#[derive(Debug, Clone)]
pub struct CrateManifest {
    pub name: String,
    pub path: PathBuf,
    pub dependencies: Vec<Dependency>,
}

impl CrateManifest {
    /// The crate's `sangkar-*` runtime dependencies: a dev-dependency is not an edge of the DAG.
    fn runtime_sangkar(&self) -> BTreeSet<&str> {
        self.dependencies
            .iter()
            .filter(|dep| dep.table == "dependencies" && dep.name.starts_with(DEPENDENCY_PREFIX))
            .map(|dep| dep.name.as_str())
            .collect()
    }
}

/// Reads every `crates/*/Cargo.toml` under `root` (the workspace's `rust/`).
pub fn scan_workspace(root: &Path) -> io::Result<Vec<CrateManifest>> {
    let crates_dir = root.join("crates");
    let mut manifests = Vec::new();
    for entry in sorted_entries(&crates_dir)? {
        let path = entry.path().join("Cargo.toml");
        if !path.is_file() {
            continue;
        }
        let parsed = parse_manifest(&fs::read_to_string(&path)?);
        let name = parsed
            .package
            .unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
        manifests.push(CrateManifest {
            name,
            path,
            dependencies: parsed.dependencies,
        });
    }
    if manifests.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no crate manifest under {}", crates_dir.display()),
        ));
    }
    Ok(manifests)
}

/// What the DAG check found, with the counts it actually made.
#[derive(Debug, Clone, Default)]
pub struct DagReport {
    /// Crate manifests read from `root`.
    pub manifests: usize,
    /// `sangkar-*` edges the contract requires, each checked in both directions.
    pub edges: usize,
    pub violations: Vec<String>,
}

/// Reads `root/crates/*/Cargo.toml` and checks the DAG and the `rusqlite` rule.
pub fn check_dag(root: &Path) -> io::Result<DagReport> {
    Ok(dag_report(&scan_workspace(root)?))
}

/// The DAG check over manifests already read, so a fixture can prove it rejects an edge.
pub fn dag_report(manifests: &[CrateManifest]) -> DagReport {
    let mut report = DagReport {
        manifests: manifests.len(),
        ..DagReport::default()
    };
    for (name, allowed) in DAG {
        report.edges += allowed.len();
        let Some(manifest) = manifests.iter().find(|manifest| manifest.name == name) else {
            report
                .violations
                .push(format!("{name}: no manifest at crates/{name}/Cargo.toml"));
            continue;
        };
        for dep in &manifest.dependencies {
            if dep.name.starts_with(DEPENDENCY_PREFIX) && !allowed.contains(&dep.name.as_str()) {
                report.violations.push(format!(
                    "{name} -> {}: sangkar-* dependency is not in the DAG (declared in [{}]; allowed: {})",
                    dep.name,
                    dep.table,
                    allowed_list(allowed)
                ));
            }
        }
        let declared = manifest.runtime_sangkar();
        for wanted in allowed {
            if !declared.contains(wanted) {
                report.violations.push(format!(
                    "{name} -> {wanted}: required sangkar-* edge is missing from [dependencies]"
                ));
            }
        }
    }
    for manifest in manifests {
        let known = DAG.iter().any(|(name, _)| *name == manifest.name.as_str());
        if manifest.name.starts_with(DEPENDENCY_PREFIX) && !known {
            report
                .violations
                .push(format!("{}: crate is not in the DAG table", manifest.name));
        }
    }
    report.violations.extend(check_rusqlite(manifests));
    report
}

/// `rusqlite` belongs to `sangkar-store` alone, as a runtime dependency.
pub fn check_rusqlite(manifests: &[CrateManifest]) -> Vec<String> {
    let mut violations = Vec::new();
    for manifest in manifests {
        for dep in manifest
            .dependencies
            .iter()
            .filter(|dep| dep.name == SQLITE_CRATE)
        {
            if manifest.name != STORE_CRATE {
                violations.push(format!(
                    "{} -> {SQLITE_CRATE}: SQLite belongs to {STORE_CRATE} alone (declared in [{}])",
                    manifest.name, dep.table
                ));
            } else if dep.table != "dependencies" {
                violations.push(format!(
                    "{} -> {SQLITE_CRATE}: declared in [{}]; the store's SQLite is a runtime dependency",
                    STORE_CRATE, dep.table
                ));
            }
        }
    }
    let store_declares = manifests.iter().any(|manifest| {
        manifest.name == STORE_CRATE
            && manifest
                .dependencies
                .iter()
                .any(|dep| dep.name == SQLITE_CRATE && dep.table == "dependencies")
    });
    if !store_declares {
        violations.push(format!(
            "{STORE_CRATE} -> {SQLITE_CRATE}: [dependencies] must declare it, because the store owns SQLite"
        ));
    }
    violations
}

fn allowed_list(allowed: &[&str]) -> String {
    if allowed.is_empty() {
        "none".to_string()
    } else {
        allowed.join(", ")
    }
}

// -------------------------------------------------------------------------------------------
// Source scans
// -------------------------------------------------------------------------------------------

/// What the source scans found, with the file count they actually read.
#[derive(Debug, Clone, Default)]
pub struct SourceReport {
    pub files: usize,
    pub violations: Vec<String>,
}

/// Scans `crates/*/src/**/*.rs` and `crates/*/tests/**/*.rs`.
///
/// Tests are scanned for the phase marker but exempt from stdout discipline: a test prints its
/// own evidence. `rust/target` and `rust/spikes` are outside this walk by construction — spikes
/// are standalone experiments, not workspace sources.
pub fn scan_sources(root: &Path) -> io::Result<SourceReport> {
    let tools = tool_modules(root)?;
    let mut files = Vec::new();
    for entry in sorted_entries(&root.join("crates"))? {
        for directory in ["src", "tests"] {
            let candidate = entry.path().join(directory);
            if candidate.is_dir() {
                collect_rust_files(&candidate, &mut files)?;
            }
        }
    }
    let mut report = SourceReport {
        files: files.len(),
        violations: Vec::new(),
    };
    for file in &files {
        let relative = relative_path(root, file);
        let source = fs::read_to_string(file).map_err(|error| {
            io::Error::new(error.kind(), format!("{}: {error}", file.display()))
        })?;
        // `crates/<crate>/src/…`, not a `src` directory a test happens to create.
        if relative.split('/').nth(2) == Some("src") {
            if let Some(violation) = stdout_violation(&relative, &source) {
                report.violations.push(violation);
            }
            if let Some(violation) = tool_isolation_violation(&relative, &source, &tools) {
                report.violations.push(violation);
            }
        }
        if let Some(violation) = phase_marker_violation(&relative, &source) {
            report.violations.push(violation);
        }
    }
    Ok(report)
}

/// The tool modules of `sangkar-tools`, read from the layout: `src/<tool>/{mod,manifest,handler}.rs`.
pub fn tool_modules(root: &Path) -> io::Result<Vec<String>> {
    let src = root.join("crates").join("sangkar-tools").join("src");
    let mut modules = Vec::new();
    if !src.is_dir() {
        return Ok(modules);
    }
    for entry in sorted_entries(&src)? {
        if entry.path().is_dir() {
            modules.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(modules)
}

/// A `src/` file may not write to stdout, directly or through the print macros.
pub fn stdout_violation(relative: &str, source: &str) -> Option<String> {
    let code = strip_comments_and_strings(source);
    for name in STDOUT_MACROS {
        if uses_macro(&code, name) {
            return Some(format!(
                "stdout discipline: {relative} calls {name}!; stdout is the MCP protocol, \
                 log lines go to stderr through sangkar-core/src/log.rs"
            ));
        }
    }
    if compact(&code).contains(STDOUT_HANDLE) {
        return Some(format!(
            "stdout discipline: {relative} uses {STDOUT_HANDLE}; stdout is the MCP protocol, \
             log lines go to stderr through sangkar-core/src/log.rs"
        ));
    }
    None
}

/// A tool module may use its own siblings and the shared crates, never another tool module.
///
/// The tools live at `src/<tool>/`, not `src/tools/<tool>/`, so the rule is: no `crate::tools`
/// anywhere, and no `crate::<other tool>` under `src/<tool>/`. `src/lib.rs` is the crate root, not
/// a tool: declaring the modules is its job.
pub fn tool_isolation_violation(
    relative: &str,
    source: &str,
    tool_modules: &[String],
) -> Option<String> {
    let within = relative.strip_prefix(TOOLS_SRC_PREFIX)?;
    let code = strip_comments_and_strings(source);
    if mentions_crate_module(&code, "tools") {
        return Some(format!(
            "tool isolation: {relative} names crate::tools; a tool module may not name another tool"
        ));
    }
    let current = within.split('/').next().unwrap_or_default();
    if !tool_modules.iter().any(|module| module == current) {
        return None;
    }
    for module in tool_modules
        .iter()
        .filter(|module| module.as_str() != current)
    {
        if mentions_crate_module(&code, module) {
            return Some(format!(
                "tool isolation: {relative} names crate::{module}; tool {current} may not use \
                 another tool — shared machinery lives in sangkar-core, sangkar-store and sangkar-domain"
            ));
        }
    }
    None
}

/// A comment that defers work to a later phase is a plan violation, not a note.
pub fn phase_marker_violation(relative: &str, source: &str) -> Option<String> {
    source.contains(PHASE_MARKER).then(|| {
        format!(
            "phase hand-off marker: {relative} contains {PHASE_MARKER}; deferred work is a plan \
             task, never a comment"
        )
    })
}

/// Blank out comments and string literals so a rule matches code, not prose.
///
/// The scan reads the file as written: a doc comment naming the banned macro, or a message that
/// quotes it, is not a call to it. Length-neutral characters are replaced by spaces so tokens on
/// either side cannot be glued together, and newlines are kept so a reported position still means
/// something.
pub fn strip_comments_and_strings(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut stripped = String::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index = skip_block_comment(bytes, index, &mut stripped);
                stripped.push(' ');
            }
            b'"' => {
                index = skip_string(bytes, index + 1);
                stripped.push(' ');
            }
            b'\'' => {
                index = skip_char_or_lifetime(bytes, index);
                stripped.push(' ');
            }
            _ => match raw_string_body(bytes, index) {
                Some((quote, hashes)) => {
                    index = skip_raw_string(bytes, quote, hashes).unwrap_or(bytes.len());
                    stripped.push(' ');
                }
                None => {
                    let ch = source[index..].chars().next().unwrap_or('\u{fffd}');
                    stripped.push(ch);
                    index += ch.len_utf8();
                }
            },
        }
    }
    stripped
}

/// Skips a (nested) block comment, keeping its newlines.
fn skip_block_comment(bytes: &[u8], start: usize, out: &mut String) -> usize {
    let mut depth = 0usize;
    let mut index = start;
    while index < bytes.len() {
        if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
            depth += 1;
            index += 2;
        } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
            depth -= 1;
            index += 2;
            if depth == 0 {
                return index;
            }
        } else {
            if bytes[index] == b'\n' {
                out.push('\n');
            }
            index += 1;
        }
    }
    index
}

/// Skips a string body that starts just past the opening quote.
fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return index + 1,
            _ => index += 1,
        }
    }
    index
}

/// Skips `'a'` and `'\n'`, and drops only the quote of a lifetime.
fn skip_char_or_lifetime(bytes: &[u8], start: usize) -> usize {
    if bytes.get(start + 1) == Some(&b'\\') {
        let mut index = start + 2;
        while index < bytes.len() && bytes[index] != b'\'' {
            index += 1;
        }
        return (index + 1).min(bytes.len());
    }
    if bytes.get(start + 2) == Some(&b'\'') && bytes.get(start + 1) != Some(&b'\'') {
        return start + 3;
    }
    start + 1
}

/// The opening quote and hash count of a raw string (`r"…"`, `r#"…"#`, `br##"…"##`).
fn raw_string_body(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    if index > 0 && is_identifier_byte(bytes[index - 1]) {
        return None;
    }
    let mut cursor = index;
    if bytes.get(cursor) == Some(&b'b') || bytes.get(cursor) == Some(&b'c') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let hashes_start = cursor;
    while bytes.get(cursor) == Some(&b'#') {
        cursor += 1;
    }
    (bytes.get(cursor) == Some(&b'"')).then_some((cursor, cursor - hashes_start))
}

/// The index just past a raw string's closing `"###`.
fn skip_raw_string(bytes: &[u8], quote: usize, hashes: usize) -> Option<usize> {
    let mut index = quote + 1;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let mut cursor = index + 1;
            let mut found = 0;
            while bytes.get(cursor) == Some(&b'#') {
                found += 1;
                cursor += 1;
            }
            if found >= hashes {
                return Some(cursor);
            }
        }
        index += 1;
    }
    None
}

/// True when `name` is used as a macro: `name!(`, never `name` inside a longer identifier.
fn uses_macro(code: &str, name: &str) -> bool {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(offset) = code[from..].find(name) {
        let start = from + offset;
        let end = start + name.len();
        let identifier_before = start > 0 && is_identifier_byte(bytes[start - 1]);
        if !identifier_before && code[end..].trim_start().starts_with('!') {
            return true;
        }
        from = end;
    }
    false
}

/// True when the code names `crate::<module>`, including a `crate::{a, b}` group.
fn mentions_crate_module(code: &str, module: &str) -> bool {
    let bytes = code.as_bytes();
    let mut from = 0;
    while let Some(offset) = code[from..].find("crate") {
        let start = from + offset;
        let end = start + "crate".len();
        from = end;
        if start > 0 && is_identifier_byte(bytes[start - 1]) {
            continue;
        }
        let Some(rest) = code[end..].trim_start().strip_prefix("::") else {
            continue;
        };
        let rest = rest.trim_start();
        if let Some(group) = braced_group(rest) {
            let named = split_top_level(group, ',')
                .into_iter()
                .any(|segment| leading_identifier(segment.trim()) == Some(module));
            if named {
                return true;
            }
            continue;
        }
        if leading_identifier(rest) == Some(module) {
            return true;
        }
    }
    false
}

/// The text between a leading `{` and its matching `}`.
fn braced_group(input: &str) -> Option<&str> {
    let inner = input.strip_prefix('{')?;
    let mut depth = 1;
    for (index, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&inner[..index]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The identifier a path segment starts with: `trace::handler` → `trace`.
fn leading_identifier(input: &str) -> Option<&str> {
    let end = input
        .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .unwrap_or(input.len());
    (end > 0).then(|| &input[..end])
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The stripped code with every whitespace removed, so `std :: io :: stdout` is still a match.
fn compact(code: &str) -> String {
    code.chars().filter(|ch| !ch.is_whitespace()).collect()
}

// -------------------------------------------------------------------------------------------
// Walking the tree
// -------------------------------------------------------------------------------------------

fn sorted_entries(directory: &Path) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn collect_rust_files(directory: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in sorted_entries(directory)? {
        let path = entry.path();
        if path.is_dir() {
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
