//! The write path's gate: a database the Rust worker wrote, read back the way a tool would, and
//! held to the census the TypeScript indexer produced for the same corpus.
//!
//! `rust/tests/golden/census/<fixture>.ts.json` is that census — one JSON document per fixture,
//! captured by `scripts/parity/census.ts --capture <fixture> --rust-sqlite-version <v>`. This suite
//! archives the checked-in fixture to a temp directory exactly as the capture did (a copy, because
//! a run that indexed the fixture in place would leave a database inside the corpus), publishes it
//! with [`run_worker`], and recomputes the same document from the file the worker wrote. Every key
//! is compared and printed as one row of a delta table — `key | ts | rust | delta | verdict` — so a
//! difference is a number on a line rather than a wall of two JSON documents.
//!
//! Three decisions shape what is compared:
//!
//! - **Order is part of the census, not a detail of it.** A cursor is a position in a generation's
//!   stable ordering and rowids are minted by the writer, so the six list-producing queries carry
//!   their ids element by element: node ids by rowid, edge ids by rowid, `node_fts_state` rowids,
//!   `entryPoints(5)`, `hotspots(5)`, and `findByName` for a duplicated name. A writer that inserts
//!   the same rows in another order passes a count comparison and invalidates every cursor; here it
//!   fails on the position that moved, and no allowance can cover an array.
//! - **The reader answers wherever it has an API.** `files`, `unresolved`, `not_indexed`,
//!   `generation` and three of the six sequences come from [`GraphReader`] on the worker's own file,
//!   so this is also a test that the read path agrees with the write path. The rest — rowid
//!   sequences, `GROUP BY` aggregates and `sqlite_version()` — has no reader method and is read with
//!   raw SQL; [`census`] carries that split as a table, and the header of `scripts/parity/census.ts`
//!   lists it the same way.
//! - **One key is recorded rather than compared.** `sqlite_version()` is the *reader's* SQLite: bun
//!   links its own build, this binary links `libsqlite3-sys`'s bundled one, and FTS5's `bm25`
//!   scoring and its tie-breaks are version quantities — the `bm25 ASC, in_degree DESC, id ASC`
//!   ordering `search` ranks by depends on them. So the census reports the version it read, the
//!   golden records the Rust side's too (`rust_sqlite_version`, measured by this suite), and a moved
//!   Rust version fails here: a dependency bump has to be looked at, not absorbed.
//!
//! A count may differ only if the golden's `allowed_delta` names the census path, the exact number,
//! the language and the grammar version that caused it — today the C# grammar substitution behind
//! one `unresolved.evidence_kind` bucket, which the golden explains. An allowance that no longer
//! applies fails as well: a stale allowance is a false statement about the port.
//!
//! Finally `sangkar status` answers from the Rust-built database as a separate process, and
//! `read_status.generation`, `coverage.indexed` and `coverage.unresolved_edges` are checked against
//! the same file's census — the reader-versus-census half of the readability proof, run rather than
//! assumed. Its cross-implementation half retired with the TypeScript tree in phase 5: while
//! `bin/sangkar.ts` existed a *TypeScript* server read the Rust-built file, and one implementation
//! cannot make that claim about itself. The surviving half still measures something a compile
//! cannot: the reader a tool holds agrees with the census the writer was held to, on one file.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_index::worker::{WorkerExit, WorkerMode, WorkerOptions, run_worker};
use sangkar_store::OptionalExtension;
use sangkar_store::db::{Conn, open_reader};
use sangkar_store::pool::DATABASE_FILE;
use sangkar_store::read::reader::{GraphReader, SqliteReader};
use serde_json::{Map, Value, json};

/// The fixtures with a captured census, in report order. `empty` is deliberately absent: a tree
/// with no files produces counts only, and the ordering this gate exists for needs rows to order.
const FIXTURES: [&str; 2] = ["polyglot", "broken"];

/// The bound the two ranked sequences are read at, matching `census.ts`'s `PAGE`.
const PAGE: usize = 5;

/// How many elements of an ordered sequence a table cell shows; a difference prints its position.
const ARRAY_HEAD: usize = 6;

/// The key every capture ran with (`rust/tests/parity/README.md`), so a `status` call here signs
/// with the key the goldens use rather than 32 bytes this process invents.
const FIXED_CURSOR_KEY: &str = "sangkar-parity-fixed-cursor-key-0123456789abcdef";

/// What a cell shows where a key exists on one side only: an absent key is a difference, not a `0`.
const ABSENT: &str = "<absent>";

// ---------------------------------------------------------------------------------------------
// Where everything lives
// ---------------------------------------------------------------------------------------------

/// The goldens and the fixtures, resolved once from the repository this crate sits in.
struct Layout {
    goldens: PathBuf,
    fixtures: PathBuf,
}

impl Layout {
    fn locate() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("the crate sits three levels below the repository root")
            .to_path_buf();
        // `SANGKAR_CENSUS_GOLDEN_DIR` exists so a *copied* golden can be mutated: that is how the
        // gate's own bite is demonstrated (a moved element fails on its position) without touching
        // the committed oracle.
        let goldens = std::env::var_os("SANGKAR_CENSUS_GOLDEN_DIR")
            .map_or_else(|| repo.join("rust/tests/golden/census"), PathBuf::from);
        Self {
            fixtures: repo.join("rust/tests/parity/fixtures"),
            goldens,
        }
    }

    fn golden(&self, fixture: &str) -> PathBuf {
        self.goldens.join(format!("{fixture}.ts.json"))
    }
}

/// A temporary directory that removes itself, so the suite leaves no staged corpus behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-census-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// The fixture archive: a copy, because the capture indexed a copy and a checked-in fixture must
/// never grow a database of its own.
fn archive(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap_or_else(|error| panic!("create {}: {error}", to.display()));
    let entries =
        fs::read_dir(from).unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("read {}: {error}", from.display()));
        let path = entry.path();
        let target = to.join(entry.file_name());
        let metadata =
            fs::metadata(&path).unwrap_or_else(|error| panic!("stat {}: {error}", path.display()));
        if metadata.is_dir() {
            archive(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap_or_else(|error| {
                panic!("copy {} to {}: {error}", path.display(), target.display())
            });
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The census, from the worker's own published file
// ---------------------------------------------------------------------------------------------

/// One integer from one statement.
fn scalar_i64(conn: &Conn, label: &str, sql: &str) -> i64 {
    conn.connection()
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .unwrap_or_else(|error| panic!("census: {label}: {error}"))
}

/// One string from one statement.
fn scalar_text(conn: &Conn, label: &str, sql: &str) -> String {
    conn.connection()
        .query_row(sql, [], |row| row.get::<_, String>(0))
        .unwrap_or_else(|error| panic!("census: {label}: {error}"))
}

/// One `GROUP BY` result per row: the key it grouped on, and the rows that landed in it.
fn counters(conn: &Conn, label: &str, sql: &str) -> BTreeMap<String, i64> {
    let connection = conn.connection();
    let mut statement = connection
        .prepare(sql)
        .unwrap_or_else(|error| panic!("census: {label}: {error}"));
    let mapped = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .unwrap_or_else(|error| panic!("census: {label}: {error}"));
    let mut counts = BTreeMap::new();
    for row in mapped {
        let (key, rows) = row.unwrap_or_else(|error| panic!("census: {label}: {error}"));
        counts.insert(key, rows);
    }
    counts
}

/// Ordered ids, in the order the statement asked for: a sequence, not a set.
fn id_list(conn: &Conn, label: &str, sql: &str) -> Vec<Value> {
    let connection = conn.connection();
    let mut statement = connection
        .prepare(sql)
        .unwrap_or_else(|error| panic!("census: {label}: {error}"));
    let mapped = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap_or_else(|error| panic!("census: {label}: {error}"));
    let mut ids = Vec::new();
    for id in mapped {
        ids.push(Value::from(
            id.unwrap_or_else(|error| panic!("census: {label}: {error}")),
        ));
    }
    ids
}

/// The name `findByName` is probed with: the lexicographically first name more than one node
/// carries, chosen from the database itself so both sides probe the same symbol.
fn duplicated_name(conn: &Conn) -> Option<String> {
    conn.connection()
        .query_row(
            "SELECT name FROM node GROUP BY name HAVING count(*) > 1 ORDER BY name LIMIT 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap_or_else(|error| panic!("census: the duplicated-name probe: {error}"))
}

/// A `JSON` object from a sorted count map, so both documents group a table the same way.
fn object_of(counts: &BTreeMap<String, i64>) -> Value {
    let mut object = Map::new();
    for (key, value) in counts {
        object.insert(key.clone(), Value::from(*value));
    }
    Value::Object(object)
}

/// One count per language, from the table the count lives in.
fn by_language(conn: &Conn, label: &str, sql: &str) -> BTreeMap<String, i64> {
    counters(conn, label, sql)
}

/// The census document. Mirrors `scripts/parity/census.ts` key for key and in the same order; a key
/// added there without a line here shows up as an `absent` row rather than passing quietly.
///
/// Which side answers each key, since the split is the point of the test:
///
/// | key | source |
/// |---|---|
/// | `files`, `unresolved*`, `not_indexed_by_reason`, `generation`, `duplicate_name`'s list | [`GraphReader`] |
/// | `order.entry_point_ids`, `order.hotspot_ids`, `order.duplicate_name_ids` | [`GraphReader`] |
/// | `nodes`, `edges`, `per_language`, `nodes_by_kind`, `nodes_by_language`, `edges_by_kind_tier` | raw `GROUP BY` |
/// | `order.node_ids`, `order.edge_ids`, `order.node_fts_state_rowids` | raw `ORDER BY rowid` |
/// | `duplicate_name`, `sqlite_version` | raw SQL: no reader method exists |
fn census(conn: &Conn, reader: &SqliteReader) -> Value {
    let files = reader.all_files().expect("census: all_files");
    let skipped = reader.not_indexed_all().expect("census: not_indexed_all");
    let file_ids: Vec<i64> = files.iter().map(|file| file.id).collect();
    let unresolved = reader
        .unresolved_for_files(&file_ids)
        .expect("census: unresolved_for_files");
    let generation = reader
        .latest_generation()
        .expect("census: latest_generation")
        .expect("census: the worker published a generation");

    let mut language_of: BTreeMap<i64, String> = BTreeMap::new();
    let mut files_by_language: BTreeMap<String, i64> = BTreeMap::new();
    for file in &files {
        language_of.insert(file.id, file.language.clone());
        *files_by_language.entry(file.language.clone()).or_insert(0) += 1;
    }
    let nodes_by_language = by_language(
        conn,
        "nodes per language",
        "SELECT f.language, count(*) FROM node n JOIN file f ON f.id = n.file_id GROUP BY f.language",
    );
    let edges_by_language = by_language(
        conn,
        "edges per language",
        "SELECT f.language, count(*) FROM edge e JOIN file f ON f.id = e.file_id GROUP BY f.language",
    );
    let mut unresolved_by_language: BTreeMap<String, i64> = BTreeMap::new();
    let mut unresolved_by_kind: BTreeMap<String, i64> = BTreeMap::new();
    for row in &unresolved {
        if let Some(language) = language_of.get(&row.file_id) {
            *unresolved_by_language.entry(language.clone()).or_insert(0) += 1;
        }
        let kind = row
            .evidence_kind
            .clone()
            .unwrap_or_else(|| "none".to_owned());
        *unresolved_by_kind.entry(kind).or_insert(0) += 1;
    }

    // The union of every table that groups by `file.language`: a language with a row anywhere is a
    // language of the census, and one that appears in a single table would otherwise go unlisted.
    let languages: Vec<String> = files_by_language
        .keys()
        .chain(nodes_by_language.keys())
        .chain(edges_by_language.keys())
        .chain(unresolved_by_language.keys())
        .cloned()
        .collect::<BTreeSet<String>>()
        .into_iter()
        .collect();

    let mut per_language = Map::new();
    let mut nodes_per_language = BTreeMap::new();
    for language in &languages {
        let count = |map: &BTreeMap<String, i64>| map.get(language).copied().unwrap_or(0);
        let nodes = count(&nodes_by_language);
        per_language.insert(
            language.clone(),
            json!({
                "files": count(&files_by_language),
                "nodes": nodes,
                "edges": count(&edges_by_language),
                "unresolved": count(&unresolved_by_language),
            }),
        );
        nodes_per_language.insert(language.clone(), nodes);
    }

    let mut not_indexed_by_reason: BTreeMap<String, i64> = BTreeMap::new();
    for row in &skipped {
        *not_indexed_by_reason.entry(row.reason.clone()).or_insert(0) += 1;
    }

    let duplicate = duplicated_name(conn);
    let duplicate_ids = duplicate.as_ref().map_or_else(Vec::new, |name| {
        reader
            .find_by_name(name, None)
            .expect("census: find_by_name")
            .into_iter()
            .map(|node| Value::from(node.id))
            .collect()
    });
    let entry_points = id_list_of(
        reader
            .entry_points(PAGE)
            .expect("census: entry_points")
            .into_iter()
            .map(|row| row.id),
    );
    let hotspots = id_list_of(
        reader
            .hotspots(PAGE)
            .expect("census: hotspots")
            .into_iter()
            .map(|row| row.id),
    );

    json!({
        "sqlite_version": scalar_text(conn, "sqlite_version", "SELECT sqlite_version()"),
        "generation": {
            "seq": generation.seq,
            "mode": generation.mode,
            "status": generation.status,
        },
        "files": files.len(),
        "nodes": scalar_i64(conn, "count node", "SELECT count(*) FROM node"),
        "edges": scalar_i64(conn, "count edge", "SELECT count(*) FROM edge"),
        "unresolved": unresolved.len(),
        "per_language": Value::Object(per_language),
        "nodes_by_kind": object_of(&counters(
            conn,
            "nodes per kind",
            "SELECT kind, count(*) FROM node GROUP BY kind",
        )),
        "nodes_by_language": object_of(&nodes_per_language),
        "edges_by_kind_tier": object_of(&counters(
            conn,
            "edges per kind:tier",
            "SELECT kind || ':' || tier, count(*) FROM edge GROUP BY kind, tier",
        )),
        "unresolved_by_kind": object_of(&unresolved_by_kind),
        "not_indexed_by_reason": object_of(&not_indexed_by_reason),
        "duplicate_name": duplicate,
        "order": {
            "node_ids": id_list(conn, "node ids", "SELECT id FROM node ORDER BY id"),
            "edge_ids": id_list(conn, "edge ids", "SELECT id FROM edge ORDER BY id"),
            "node_fts_state_rowids": id_list(
                conn,
                "node_fts_state rowids",
                "SELECT rowid_ FROM node_fts_state ORDER BY rowid_",
            ),
            "entry_point_ids": entry_points,
            "hotspot_ids": hotspots,
            "duplicate_name_ids": duplicate_ids,
        },
    })
}

/// Ids from a reader result, as the JSON array a sequence is compared as.
fn id_list_of(ids: impl Iterator<Item = i64>) -> Vec<Value> {
    ids.map(Value::from).collect()
}

// ---------------------------------------------------------------------------------------------
// The allowances a golden may carry
// ---------------------------------------------------------------------------------------------

/// One reviewed allowance, filed under the census path it covers. The language and the grammar
/// version are part of it because the contract is "a named grammar caused this exact delta", not
/// "this number may be anything".
struct Allowance {
    delta: i64,
    label: String,
    #[expect(
        dead_code,
        reason = "read by a reviewer in the golden, not by this suite"
    )]
    reason: String,
}

/// The golden's `allowed_delta`, keyed by census path.
struct Allowances {
    by_path: BTreeMap<String, Allowance>,
}

impl Allowances {
    /// Parse and validate: a malformed allowance is a golden that cannot be judged, so it panics
    /// naming the missing field rather than allowing nothing silently.
    fn parse(golden: &Value, golden_path: &Path) -> Self {
        let entries = golden
            .get("allowed_delta")
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!(
                    "census: {} has no allowed_delta list; the golden cannot be read",
                    golden_path.display()
                )
            });
        let mut by_path = BTreeMap::new();
        for entry in entries {
            let text = |key: &str| -> String {
                entry
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        panic!(
                            "census: {}: an allowed_delta entry has no {key}",
                            golden_path.display()
                        )
                    })
                    .to_owned()
            };
            let language = text("language");
            let grammar = text("grammar");
            let reason = text("reason");
            let deltas = entry
                .get("deltas")
                .and_then(Value::as_object)
                .unwrap_or_else(|| {
                    panic!(
                        "census: {}: the {language} allowance has no deltas map",
                        golden_path.display()
                    )
                });
            for (path, delta) in deltas {
                let delta = delta.as_i64().unwrap_or_else(|| {
                    panic!(
                        "census: {}: allowed_delta.{path} is not an integer",
                        golden_path.display()
                    )
                });
                by_path.insert(
                    path.clone(),
                    Allowance {
                        delta,
                        label: format!("allowed ({language}, {grammar})"),
                        reason: reason.clone(),
                    },
                );
            }
        }
        Self { by_path }
    }
}

// ---------------------------------------------------------------------------------------------
// The delta table
// ---------------------------------------------------------------------------------------------

/// One line of the table. `failed` is what the suite asserts on; the text is for the reader.
struct Row {
    key: String,
    ts: String,
    rust: String,
    delta: String,
    verdict: String,
    failed: bool,
}

/// The rows of one fixture's comparison, plus the allowances that were actually needed.
struct Report {
    rows: Vec<Row>,
    used: BTreeSet<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            used: BTreeSet::new(),
        }
    }

    /// A row that agrees or is recorded on purpose.
    fn note(&mut self, key: &str, ts: &str, rust: &str, verdict: &str) {
        self.rows.push(Row {
            key: key.to_owned(),
            ts: ts.to_owned(),
            rust: rust.to_owned(),
            delta: "-".to_owned(),
            verdict: verdict.to_owned(),
            failed: false,
        });
    }

    fn failed_rows(&self) -> usize {
        self.rows.iter().filter(|row| row.failed).count()
    }

    /// The table, one line per key, with the columns sized to what is in them.
    fn print(&self, fixture: &str) {
        let key_width = widest(self.rows.iter().map(|row| row.key.len()), "key".len());
        let ts_width = widest(self.rows.iter().map(|row| row.ts.len()), "ts".len());
        let rust_width = widest(self.rows.iter().map(|row| row.rust.len()), "rust".len());
        let delta_width = widest(self.rows.iter().map(|row| row.delta.len()), "delta".len());

        println!(
            "--- {fixture}: {} keys compared, {} differ",
            self.rows.len(),
            self.failed_rows()
        );
        println!(
            "{:<key_width$} | {:<ts_width$} | {:<rust_width$} | {:<delta_width$} | verdict",
            "key", "ts", "rust", "delta"
        );
        for row in &self.rows {
            println!(
                "{:<key_width$} | {:<ts_width$} | {:<rust_width$} | {:<delta_width$} | {}",
                row.key, row.ts, row.rust, row.delta, row.verdict
            );
        }
    }
}

/// The widest of a column's cells, never narrower than its header.
fn widest(values: impl Iterator<Item = usize>, at_least: usize) -> usize {
    values
        .chain(std::iter::once(at_least))
        .max()
        .unwrap_or(at_least)
}

/// How a value reads in a table cell: a string as itself, a sequence as its head and its length.
fn render(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_owned(),
        Value::Array(items) => render_sequence(items),
        other => other.to_string(),
    }
}

/// A sequence as one cell: `len=80 [1,2,3,4,5,6,…]`. The length is the first fact and the head is
/// the second, because the alternative is 80 numbers nobody diffs by eye.
fn render_sequence(items: &[Value]) -> String {
    let head: Vec<String> = items.iter().take(ARRAY_HEAD).map(render).collect();
    let more = if items.len() > ARRAY_HEAD { ",…" } else { "" };
    format!("len={} [{}{more}]", items.len(), head.join(","))
}

/// One ordered sequence, element by element: the first position that moved is the verdict, and a
/// length difference is named before any element is compared. No allowance reaches this function.
fn compare_sequence(path: &str, ts: &[Value], rust: &[Value]) -> Row {
    let mismatch = if ts.len() != rust.len() {
        Some(format!("length {} vs {}", ts.len(), rust.len()))
    } else {
        ts.iter()
            .zip(rust.iter())
            .position(|(expected, actual)| expected != actual)
            .map(|index| {
                format!(
                    "first differs at {index}: {} vs {}",
                    render(&ts[index]),
                    render(&rust[index])
                )
            })
    };
    let delta = if ts.len() == rust.len() {
        "0".to_owned()
    } else {
        format!("{}", rust.len() as i64 - ts.len() as i64)
    };
    let (verdict, failed) =
        mismatch.map_or_else(|| ("same".to_owned(), false), |detail| (detail, true));
    Row {
        key: path.to_owned(),
        ts: render_sequence(ts),
        rust: render_sequence(rust),
        delta,
        verdict,
        failed,
    }
}

/// Walk both documents together and emit one row per leaf, per map key and per ordered sequence.
fn compare(path: &str, ts: &Value, rust: &Value, allowances: &Allowances, report: &mut Report) {
    match (ts, rust) {
        (Value::Object(left), Value::Object(right)) => {
            let keys: BTreeSet<&String> = left.keys().chain(right.keys()).collect();
            for key in keys {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match (left.get(key), right.get(key)) {
                    (Some(expected), Some(actual)) => {
                        compare(&child, expected, actual, allowances, report);
                    }
                    (Some(expected), None) => report.rows.push(Row {
                        key: child,
                        ts: render(expected),
                        rust: ABSENT.to_owned(),
                        delta: "-".to_owned(),
                        verdict: "absent in the rust census".to_owned(),
                        failed: true,
                    }),
                    (None, Some(actual)) => report.rows.push(Row {
                        key: child,
                        ts: ABSENT.to_owned(),
                        rust: render(actual),
                        delta: "-".to_owned(),
                        verdict: "absent in the typescript census".to_owned(),
                        failed: true,
                    }),
                    (None, None) => {}
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            let row = compare_sequence(path, left, right);
            report.rows.push(row);
        }
        _ => report.leaf(path, ts, rust, allowances),
    }
}

impl Report {
    /// One scalar: equal, or a difference an allowance has to name to the exact number.
    fn leaf(&mut self, path: &str, ts: &Value, rust: &Value, allowances: &Allowances) {
        let ts_text = render(ts);
        let rust_text = render(rust);
        // `sqlite_version()` measures the process that ran the query, not the file: bun and this
        // binary link different SQLite builds by design, so it is recorded, never compared.
        if path == "sqlite_version" {
            self.note(
                path,
                &ts_text,
                &rust_text,
                "recorded (the reader's SQLite, not the file's)",
            );
            return;
        }
        let delta = match (ts.as_i64(), rust.as_i64()) {
            (Some(expected), Some(actual)) => Some(actual - expected),
            _ => None,
        };
        let rendered = delta.map_or_else(|| "-".to_owned(), |value| value.to_string());
        if ts == rust {
            self.rows.push(Row {
                key: path.to_owned(),
                ts: ts_text,
                rust: rust_text,
                delta: rendered,
                verdict: "same".to_owned(),
                failed: false,
            });
            return;
        }
        match allowances
            .by_path
            .get(path)
            .filter(|allowance| delta == Some(allowance.delta))
        {
            Some(allowance) => {
                self.used.insert(path.to_owned());
                self.rows.push(Row {
                    key: path.to_owned(),
                    ts: ts_text,
                    rust: rust_text,
                    delta: rendered,
                    verdict: allowance.label.clone(),
                    failed: false,
                });
            }
            None => self.rows.push(Row {
                key: path.to_owned(),
                ts: ts_text,
                rust: rust_text,
                delta: rendered,
                verdict: "differ".to_owned(),
                failed: true,
            }),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The readability proof
// ---------------------------------------------------------------------------------------------

/// One finished child: the code it exited with, and the two streams exactly as written.
struct Ran {
    code: i64,
    stdout: String,
    stderr: String,
}

/// Run `program` in `cwd`, on a machine that carries only the variables this suite sets. Every
/// inherited `SANGKAR_*` is dropped first: a stray `SANGKAR_HOME` would point the run at this
/// developer's registry, and `PATH`/`HOME` survive because a child process expects them.
fn run(program: &OsStr, args: &[String], cwd: &Path, env: &[(&str, &str)]) -> Ran {
    let mut command = Command::new(program);
    command.args(args).current_dir(cwd);
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("SANGKAR_") {
            command.env_remove(name.as_ref());
        }
    }
    for (name, value) in env {
        command.env(name, value);
    }
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().unwrap_or_else(|error| {
        panic!("census: cannot run {}: {error}", program.to_string_lossy())
    });
    Ran {
        code: i64::from(status.code().unwrap_or(-1)),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

/// What `status` answered from the Rust-built file, and the two counts it agrees with the census on.
struct Answer {
    generation: String,
    indexed: i64,
    unresolved: i64,
}

/// The `sangkar` binary this workspace built, found beside the artifacts of the run that is testing
/// it.
///
/// `CARGO_BIN_EXE_sangkar` exists only for tests of the crate that declares the binary, and this
/// suite belongs to `sangkar-index`. The profile directory holding this test binary holds the CLI
/// too, so the nearest ancestor with a `sangkar` file beside it is the one `cargo test` just built.
fn sangkar_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("census: the test binary has a path");
    exe.ancestors()
        .filter_map(Path::parent)
        .map(|dir| dir.join("sangkar"))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "census: no `sangkar` binary beside {}; the status proof runs the CLI this \
                 workspace builds, so run `cargo test --workspace` (or `cargo build -p \
                 sangkar-cli` first)",
                exe.display()
            )
        })
}

/// Ask `sangkar status` — a separate process, from the bytes this workspace built — what it reads
/// from the database the Rust worker published.
///
/// The envelope is the evidence: a non-null `read_status.generation` means the reader resolved
/// `meta` and the generation row, and `coverage.indexed`/`coverage.unresolved_edges` mean it
/// counted the same rows the census did. The cross-implementation half of this proof (a TypeScript
/// server reading a Rust-built graph) retired with the TypeScript tree in phase 5; what is left
/// still runs the reader a tool runs, against the file the gate above just judged.
fn status(archive_root: &Path, data_dir: &Path, home: &Path) -> Answer {
    let program = sangkar_binary();
    let args: Vec<String> = vec![
        "status".to_owned(),
        "--json".to_owned(),
        "--repo-root".to_owned(),
        archive_root.display().to_string(),
        "--data-dir".to_owned(),
        data_dir.display().to_string(),
    ];
    let home_text = home.display().to_string();
    let ran = run(
        program.as_os_str(),
        &args,
        archive_root,
        &[
            ("SANGKAR_HOME", home_text.as_str()),
            ("SANGKAR_CURSOR_KEY", FIXED_CURSOR_KEY),
            ("SANGKAR_TELEMETRY", "off"),
        ],
    );
    assert_eq!(
        ran.code, 0,
        "census: status over the rust-built database exited {}:\n{}",
        ran.code, ran.stderr
    );
    let envelope: Value = serde_json::from_str(&ran.stdout).unwrap_or_else(|error| {
        panic!(
            "census: status did not answer with a JSON envelope ({error}):\n{}",
            ran.stdout
        )
    });

    let field = |pointer: &str| -> String {
        match envelope.pointer(pointer) {
            Some(Value::String(value)) => value.clone(),
            Some(other) => render(other),
            None => panic!("census: status answered with no {pointer}:\n{}", ran.stdout),
        }
    };
    let count = |pointer: &str| -> i64 {
        let value = field(pointer);
        value.parse::<i64>().unwrap_or_else(|_| {
            panic!("census: status answered {pointer} = {value:?}, which is not a count")
        })
    };
    Answer {
        generation: field("/read_status/generation"),
        indexed: count("/coverage/indexed"),
        unresolved: count("/coverage/unresolved_edges"),
    }
}

// ---------------------------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------------------------

/// Publish one fixture with the Rust worker, census the result, and judge it against the golden.
fn gate(fixture: &str) {
    let layout = Layout::locate();
    let golden_path = layout.golden(fixture);
    assert!(
        golden_path.exists(),
        "census: {} is missing; the committed golden is this suite's only oracle, so restore it \
         (`git checkout -- rust/tests/golden/census/{fixture}.ts.json`) rather than recapturing — \
         the TypeScript capture script retired with `src/` and cannot be re-run",
        golden_path.display()
    );
    let golden: Value = serde_json::from_str(
        &fs::read_to_string(&golden_path)
            .unwrap_or_else(|error| panic!("census: read {}: {error}", golden_path.display())),
    )
    .unwrap_or_else(|error| panic!("census: {} is not JSON: {error}", golden_path.display()));
    let expected = golden
        .get("census")
        .unwrap_or_else(|| panic!("census: {} has no census key", golden_path.display()));
    let allowances = Allowances::parse(&golden, &golden_path);

    // The archive sits outside the repository, at a fresh path: the same mechanic the capture used,
    // so both implementations indexed the same relative layout and neither wrote inside the fixture.
    let tree = TempDir::new(fixture);
    let archive_root = tree.join(fixture);
    archive(&layout.fixtures.join(fixture), &archive_root);
    let data_dir = tree.join("data");
    let home = tree.join("home");
    fs::create_dir_all(&data_dir).expect("census: create the data dir");
    fs::create_dir_all(&home).expect("census: create the temp home");

    let exit = run_worker(&WorkerOptions {
        repo_root: archive_root.clone(),
        data_dir: data_dir.clone(),
        force: true,
        mode: Some(WorkerMode::Full),
    });
    assert_eq!(
        exit,
        WorkerExit::Published,
        "census: the worker did not publish {fixture} (exit {})",
        exit.code()
    );

    // Two read-only handles on the published file: one for raw SQL, one owned by the reader the
    // tools hold. Both read the same bytes, and a second reader is the pool's own pattern.
    let live = data_dir.join(DATABASE_FILE);
    let conn = open_reader(&live)
        .unwrap_or_else(|error| panic!("census: open {}: {error}", live.display()));
    let reader = SqliteReader::new(
        open_reader(&live).unwrap_or_else(|error| panic!("census: open a reader: {error}")),
    );
    let observed = census(&conn, &reader);

    let mut report = Report::new();
    compare("", expected, &observed, &allowances, &mut report);

    // The version this binary links, which the census above cannot report on its own behalf: the
    // golden pins it, so a dependency bump that could move the FTS5 tie-breaks is a visible re-pin.
    let rust_version = observed
        .get("sqlite_version")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let recorded = golden
        .get("rust_sqlite_version")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            panic!(
                "census: {} records no rust_sqlite_version; a golden that did not pin the Rust \
                 build's SQLite cannot tell a dependency bump from a writer change",
                golden_path.display()
            )
        });
    if recorded == rust_version {
        report.note(
            "sqlite_version (rust build, recorded)",
            recorded,
            rust_version,
            "same",
        );
    } else {
        report.rows.push(Row {
            key: "sqlite_version (rust build, recorded)".to_owned(),
            ts: recorded.to_owned(),
            rust: rust_version.to_owned(),
            delta: "-".to_owned(),
            verdict: "recapture: the Rust SQLite moved, and FTS5 tie-breaks are version quantities"
                .to_owned(),
            failed: true,
        });
    }

    // A stale allowance is a claim about the port that is no longer true, so it fails too.
    for (path, allowance) in &allowances.by_path {
        if !report.used.contains(path) {
            report.rows.push(Row {
                key: format!("allowance:{path}"),
                ts: allowance.delta.to_string(),
                rust: "0".to_owned(),
                delta: "-".to_owned(),
                verdict: "stale allowance: this row no longer differs by the listed delta"
                    .to_owned(),
                failed: true,
            });
        }
    }

    let files = observed
        .get("files")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    report.print(fixture);

    let answer = status(&archive_root, &data_dir, &home);
    let unresolved = observed
        .get("unresolved")
        .and_then(Value::as_i64)
        .unwrap_or(-1);
    println!("--- {fixture}: sangkar status over the rust-built database");
    println!("  exit                       0");
    println!("  read_status.generation     {}", answer.generation);
    println!(
        "  coverage.indexed           {} (census files {files})",
        answer.indexed
    );
    println!(
        "  coverage.unresolved_edges  {} (census unresolved {unresolved})",
        answer.unresolved
    );
    assert!(
        !answer.generation.is_empty(),
        "census: status answered with an empty read_status.generation"
    );
    assert_eq!(
        answer.indexed, files as i64,
        "census: status reports {} indexed files, the census counted {files}",
        answer.indexed
    );
    assert_eq!(
        answer.unresolved, unresolved,
        "census: status reports {} unresolved edges, the census counted {unresolved}",
        answer.unresolved
    );

    assert_eq!(
        report.failed_rows(),
        0,
        "census: {fixture} differs from {} at {} keys (see the table above)",
        golden_path.display(),
        report.failed_rows()
    );
    println!("--- {fixture}: every key agrees");
}

#[test]
fn polyglot_census_matches_the_typescript_indexer() {
    gate(FIXTURES[0]);
}

#[test]
fn broken_census_matches_the_typescript_indexer() {
    gate(FIXTURES[1]);
}
