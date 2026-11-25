//! The parity bar as a command: every golden in `rust/tests/golden/` replayed against the built
//! binary, one JSON pointer per difference.
//!
//! `rust/tests/golden/<fixture>/<tool>.json` is the oracle (`{args, stdout, exit_code}`, captured by
//! `scripts/parity/capture-goldens.ts`). This suite rebuilds each fixture's database, replays every
//! golden through `CARGO_BIN_EXE_sangkar`, and compares the envelope structurally plus the cursor
//! token byte for byte — so "the port still answers the same" stops being a review opinion.
//!
//! Five decisions shape the replay, and each is a fact about the data rather than a relaxation:
//!
//! - **The archive sits at the path the capture used.** `/payload/project/root`, both `db_path`s and
//!   the `repo` field *inside the signed cursor* all embed the served tree's absolute path, and the
//!   cursor is compared byte for byte. A replay under a fresh `mkdtemp` name therefore cannot
//!   reproduce a single `next` token. `SANGKAR_PARITY_REPLAY_ROOT` can move the root, but only to a
//!   path the goldens were captured at — otherwise every `next` comparison answers with the path it
//!   wanted, and that is a recapture, not a test change.
//! - **The publish is pinned to the generation the goldens recorded.** `newGenerationId()` is `g-`
//!   plus three random bytes (`src/indexer/worker.ts:701`), minted once per publish, and it is
//!   inside the cursor payload *and* in every `read_status`. A fresh publish could never match, so
//!   after indexing this suite rewrites `generation.id`/`published_at` to the pair the fixture's
//!   goldens carry — the technique `scripts/parity/capture-reader-dump.ts` documents. Both values
//!   are read out of the oracle, and a fixture whose goldens disagree on them is refused before a
//!   byte is compared.
//! - **Four measured fields are compared by shape, not by value** ([`IMPRECISE`]): the freshness
//!   clock at call time, the publish's own duration, the database's size on disk and the
//!   process-wide statement-cache count. `rust/tests/parity/README.md` lists them; they measure
//!   *this* process, and copying the captured number into the assertion would hide a wrong one.
//!   Nothing else is normalised: key sets, key order, array order, `status`, `summary`, totals and
//!   `next` are compared exactly.
//! - **The TypeScript indexer is preferred while it exists.** phase 2's promise is that the Rust
//!   *server* serves a *TypeScript-built* graph, so the database is built with
//!   `~/.bun/bin/bun run bin/sangkar.ts index --mode fast`. Once `bin/sangkar.ts` is gone the write
//!   path is this binary's own `index` verb and the *identical* comparison runs. That stays sound
//!   only because `rust/crates/sangkar-index/tests/census.rs` (TASK-048) pins the six ordered row
//!   sequences: ids and order must match or these goldens break. Until the Rust write path lands
//!   (TASK-046) the fallback refuses loudly with that task's name — it never skips.
//! - **Nothing is skipped silently.** A fixture the machine cannot stage is named, counted and
//!   printed; a floor assertion fails a suite that shrank; every run prints how many goldens it
//!   compared.
//!
//! The `broken` fixture's `locked/` directory exists only while an index run is executing
//! (`rust/tests/parity/README.md`), so this suite creates it, `chmod 000`s it and — as a runtime
//! check rather than an assumption — verifies the process really cannot read it. Under uid 0 it is
//! readable, every `broken` golden would then be short one `not_indexed` row, and that fixture is
//! skipped *out loud* instead.
//!
//! Two known, documented divergences may surface and neither is normalised away: `bootstrapping()`
//! orders `hint` before `retry_after_ms` where the TypeScript appends `hint` last, and `expand`
//! publishes no continuation where the TypeScript signs a dead token. No golden reaches either at
//! this commit — none is a bootstrapping envelope, and no expand golden carries a `next` — so a
//! difference at either pointer is a real finding, not this header's excuse.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Map, Value};

/// The binary on trial: the one cargo built for the package that declares it, so the bytes compared
/// are the bytes an installed `sangkar` would run.
const SANGGAR: &str = env!("CARGO_BIN_EXE_sangkar");

/// The key every capture used — 48 bytes, above the 32 the cursor guard demands, and not a secret.
/// Without it each process mints 32 random bytes and no `next` token would survive its own process.
const FIXED_CURSOR_KEY: &str = "sangkar-parity-fixed-cursor-key-0123456789abcdef";

/// The three fixtures, in report order. `rust/tests/golden/reader/` belongs to `sangkar-store`'s
/// reader harness (TASK-023) and is deliberately not replayed here.
const FIXTURES: [&str; 3] = ["polyglot", "broken", "empty"];

/// The number of probes `capture-goldens.ts` writes per fixture. A floor, not an equality: a new
/// probe is welcome, a vanished one is a suite that shrank and must fail.
const GOLDENS_PER_FIXTURE_FLOOR: usize = 19;

/// The archive root `capture-goldens.ts` used (`$TMPDIR/sangkar-parity-capture/<fixture>/<fixture>`),
/// reproduced because that absolute path is inside the signed cursor.
const CAPTURE_ROOT_NAME: &str = "sangkar-parity-capture";

/// How long a replay waits for a concurrent one to release the root before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(120);

/// A lock older than this is a run that died without cleaning up, not a run in progress.
const LOCK_STALE: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------------------------
// The fields this replay cannot reproduce, and the shape each one must keep
// ---------------------------------------------------------------------------------------------

/// How an imprecise field is compared: the *kind* of value it must be, never the captured value.
#[derive(Clone, Copy)]
enum Imprecise {
    /// An ISO-8601 UTC instant with milliseconds: `2026-11-24T04:44:41.665Z`.
    Instant,
    /// A non-negative whole number.
    Count,
}

/// The complete normalisation list, and the only one: `rust/tests/parity/README.md` §"Fields that are
/// per-run, not contract" is the prose form. A path absent from this table is compared exactly.
const IMPRECISE: [(&str, Imprecise); 4] = [
    // `freshness::read_status`'s own clock, read at the moment of the call.
    ("/read_status/stale_checked_at", Imprecise::Instant),
    // `generation.duration_ms`: how long *this* machine's publish took.
    ("/payload/health/last_publish_ms", Imprecise::Count),
    // The database file this process opened: a page count times a page size.
    ("/payload/graph/size_bytes", Imprecise::Count),
    // The process-wide prepared-statement cache, which counts every handle the run opened.
    ("/payload/health/statement_cache_entries", Imprecise::Count),
];

// ---------------------------------------------------------------------------------------------
// Where everything lives
// ---------------------------------------------------------------------------------------------

/// The repository, the oracle and the replay root, resolved once.
struct Replay {
    repo: PathBuf,
    goldens: PathBuf,
    fixtures: PathBuf,
    root: PathBuf,
}

impl Replay {
    /// Resolves the four paths, honouring the two overrides. `SANGKAR_PARITY_GOLDEN_DIR` exists so a
    /// *copied* golden can be mutated without touching the oracle: that is how "one changed byte
    /// flips the verdict" is demonstrated.
    fn locate() -> Self {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("the crate sits three levels below the repository root")
            .to_path_buf();
        let goldens =
            env_path("SANGKAR_PARITY_GOLDEN_DIR").unwrap_or_else(|| repo.join("rust/tests/golden"));
        let root = env_path("SANGKAR_PARITY_REPLAY_ROOT")
            .unwrap_or_else(|| std::env::temp_dir().join(CAPTURE_ROOT_NAME));
        Self {
            fixtures: repo.join("rust/tests/parity/fixtures"),
            repo,
            goldens,
            root,
        }
    }

    /// The served tree: a copy of the fixture, at the path the capture published from.
    fn archive(&self, fixture: &str) -> PathBuf {
        self.root.join(fixture).join(fixture)
    }

    /// The data dir beside it: `<root>/<fixture>/data`, the `db_path` every golden records.
    fn data(&self, fixture: &str) -> PathBuf {
        self.root.join(fixture).join("data")
    }

    /// A machine home per fixture, so no run reads the developer's registry or config.
    fn home(&self, fixture: &str) -> PathBuf {
        self.root.join(fixture).join("home")
    }

    /// The second, cold archive the `index` golden was captured on. Its path is unobservable — the
    /// bootstrap verb prints nothing — so it does not need the captured name.
    fn cold(&self, fixture: &str) -> PathBuf {
        self.root.join(fixture).join("cold")
    }

    /// The golden files of one fixture, sorted so the report is stable across filesystems.
    fn golden_names(&self, fixture: &str) -> Vec<String> {
        let dir = self.goldens.join(fixture);
        let mut names: Vec<String> = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("goldens: cannot read {}: {error}", dir.display()))
            .map(|entry| {
                entry
                    .expect("a readable golden directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.ends_with(".json"))
            .collect();
        names.sort();
        names
    }

    /// The generation and publish instant the fixture's goldens recorded, refusing anything the
    /// replay could not pin to a single pair.
    fn captured_publish(&self, fixture: &str, names: &[String]) -> (String, String) {
        let mut pairs: BTreeSet<(String, String)> = BTreeSet::new();
        for name in names {
            let golden = load_golden(&self.goldens.join(fixture).join(name));
            let Ok(document) = serde_json::from_str::<Value>(&golden.stdout) else {
                continue;
            };
            let status = &document["read_status"];
            let (Some(generation), Some(indexed_at)) =
                (status["generation"].as_str(), status["indexed_at"].as_str())
            else {
                continue;
            };
            pairs.insert((generation.to_owned(), indexed_at.to_owned()));
        }
        assert_eq!(
            pairs.len(),
            1,
            "goldens: {fixture}'s goldens name {} different (generation, indexed_at) pairs; the \
             replay pins exactly one, so the fixture must be recaptured in a single run",
            pairs.len()
        );
        pairs.into_iter().next().expect("the one pair")
    }
}

/// An environment override, if it is set to something.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

// ---------------------------------------------------------------------------------------------
// The exclusive root: two concurrent replays would otherwise index and read the same directories
// ---------------------------------------------------------------------------------------------

/// A directory-created lock, released on drop. `create_dir` is atomic, which is all this needs — the
/// store's own `write.lock` is about two *indexers* racing a publish, not about two test runs sharing
/// a replay root.
struct ReplayLock {
    path: PathBuf,
}

impl ReplayLock {
    fn acquire(root: &Path) -> Self {
        let path = root.join(".lock");
        fs::create_dir_all(root)
            .unwrap_or_else(|error| panic!("goldens: cannot create {}: {error}", root.display()));
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            match fs::create_dir(&path) {
                Ok(()) => {
                    let _ = fs::write(path.join("owner"), format!("{}\n", std::process::id()));
                    return Self { path };
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Self::abandoned(&path) {
                        let _ = fs::remove_dir_all(&path);
                        continue;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "goldens: {} is held by another replay after {}s; remove it if no run is \
                         alive (the replay root is shared because the goldens encode its path)",
                        path.display(),
                        LOCK_WAIT.as_secs()
                    );
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("goldens: cannot create {}: {error}", path.display()),
            }
        }
    }

    fn abandoned(path: &Path) -> bool {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|created| SystemTime::now().duration_since(created).ok())
            .is_some_and(|age| age > LOCK_STALE)
    }
}

impl Drop for ReplayLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------------------------
// Running a child, and reading what it wrote
// ---------------------------------------------------------------------------------------------

/// One finished child: the code it exited with, and the two streams exactly as written.
struct Ran {
    code: i64,
    stdout: String,
    stderr: String,
}

/// Runs `program` with `args` in `cwd`, on a machine that carries only the variables this suite sets.
///
/// Every inherited `SANGKAR_*` is dropped first: `SANGKAR_WORKER_CMD` alone would redirect the
/// `index` verb at a different worker, and a stray `SANGKAR_HOME` would point the run at this
/// developer's registry. `PATH` and `HOME` survive because `bun` and `git` need them.
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
        panic!("goldens: cannot run {}: {error}", program.to_string_lossy())
    });
    Ran {
        code: i64::from(status.code().unwrap_or(-1)),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    }
}

// ---------------------------------------------------------------------------------------------
// Which indexer builds the fixture databases
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Indexer {
    /// `bin/sangkar.ts`, the oracle while phase 5 has not deleted it.
    Typescript,
    /// This binary's own write path: the configuration phase 5 leaves behind.
    Rust,
}

impl Indexer {
    /// `SANGKAR_PARITY_INDEXER=typescript|rust` states the mode; absent, the mode is whether
    /// `bin/sangkar.ts` is still there. The override exists so the fallback can be exercised before
    /// phase 5 deletes the TypeScript — and so a run can prove which write path it used.
    fn detect(repo: &Path) -> Self {
        let stated = std::env::var("SANGKAR_PARITY_INDEXER")
            .ok()
            .filter(|value| !value.is_empty());
        match stated.as_deref() {
            Some("rust") => Self::Rust,
            Some("typescript") => Self::Typescript,
            Some(other) => {
                panic!("goldens: SANGKAR_PARITY_INDEXER={other} is neither `typescript` nor `rust`")
            }
            None if repo.join("bin/sangkar.ts").exists() => Self::Typescript,
            None => Self::Rust,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Typescript => "typescript (bin/sangkar.ts)",
            Self::Rust => "rust (sangkar index)",
        }
    }
}

/// The program and the tokens that precede the verb name, for the indexer this run uses.
///
/// One function so the fixture publish and the `index` golden cannot disagree about which write
/// path the goldens are being replayed against.
fn indexer_program(indexer: Indexer) -> (OsString, Vec<String>) {
    match indexer {
        Indexer::Typescript => {
            let bun = bun();
            assert!(
                bun.exists(),
                "goldens: bin/sangkar.ts exists but {} does not; set SANGKAR_PARITY_BUN",
                bun.display()
            );
            (
                bun.into_os_string(),
                vec!["run".to_owned(), "bin/sangkar.ts".to_owned()],
            )
        }
        Indexer::Rust => (OsString::from(SANGGAR), Vec::new()),
    }
}

/// The `bun` the capture used, overridable for a machine that keeps it elsewhere.
fn bun() -> PathBuf {
    env_path("SANGKAR_PARITY_BUN").unwrap_or_else(|| {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/root"), PathBuf::from);
        home.join(".bun/bin/bun")
    })
}

// ---------------------------------------------------------------------------------------------
// Building one fixture's database
// ---------------------------------------------------------------------------------------------

/// A fixture staged and published: the tree the binary serves, its data dir, and its home.
struct Built {
    archive: PathBuf,
    data: PathBuf,
    home: PathBuf,
}

/// Copies a tree file by file, permissions included. `fs::copy` carries a file's mode; a directory's
/// is irrelevant here because `broken`'s only unusual one is created after the copy.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to)
        .unwrap_or_else(|error| panic!("goldens: cannot create {}: {error}", to.display()));
    for entry in fs::read_dir(from)
        .unwrap_or_else(|error| panic!("goldens: cannot read {}: {error}", from.display()))
    {
        let entry = entry.expect("a readable fixture entry");
        let source = entry.path();
        let destination = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &destination);
        } else {
            fs::copy(&source, &destination).unwrap_or_else(|error| {
                panic!(
                    "goldens: cannot copy {} to {}: {error}",
                    source.display(),
                    destination.display()
                )
            });
        }
    }
}

/// Makes a tree removable again, including a `locked/` a killed run left at mode `000`.
fn make_removable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    }
}

impl Replay {
    /// Stages a fixture and publishes it, or says why this machine cannot stage it.
    ///
    /// `pinned` is the `(generation, indexed_at)` pair the goldens recorded; it is written into the
    /// published database after the indexer runs.
    fn build(
        &self,
        fixture: &str,
        indexer: Indexer,
        pinned: &(String, String),
    ) -> Result<Built, String> {
        let archive = self.archive(fixture);
        let data = self.data(fixture);
        let home = self.home(fixture);
        // The fixture's own directory holds the archive, the data dir, the home and the cold copy;
        // all four are copies, so none of them outlives the next run.
        let scratch = self.root.join(fixture);
        make_removable(&scratch);
        let _ = fs::remove_dir_all(&scratch);
        copy_tree(&self.fixtures.join(fixture), &archive);
        fs::create_dir_all(&data).expect("a writable data dir");
        fs::create_dir_all(&home).expect("a writable home");

        if let Some(reason) = stage_unreadable(&archive) {
            return Err(reason);
        }

        // The one failure this suite could cause: indexing the repository itself.
        assert!(
            archive != self.repo && archive.starts_with(&self.root),
            "goldens: refusing to index {} — the served tree must be a copy under {}",
            archive.display(),
            self.root.display()
        );

        self.publish(&archive, &data, &home, indexer, fixture);
        pin_publish(&data.join("graph.db"), &pinned.0, &pinned.1);
        Ok(Built {
            archive,
            data,
            home,
        })
    }

    /// Runs the indexer once on a fresh archive. Its stdout must be empty for the same reason the
    /// capture asserts it: a worker that printed there would corrupt the session that spawned it.
    fn publish(&self, archive: &Path, data: &Path, home: &Path, indexer: Indexer, fixture: &str) {
        let home_text = home.to_string_lossy().into_owned();
        let (program, mut args) = indexer_program(indexer);
        args.push("index".to_owned());
        args.extend([
            "--mode".to_owned(),
            "fast".to_owned(),
            // The verb asks before it spawns a worker; a script answers instead of hanging on a
            // stdin that will never speak.
            "--yes".to_owned(),
            "--repo-root".to_owned(),
            archive.to_string_lossy().into_owned(),
            "--data-dir".to_owned(),
            data.to_string_lossy().into_owned(),
        ]);
        let env = [
            ("SANGKAR_HOME", home_text.as_str()),
            ("SANGKAR_CURSOR_KEY", FIXED_CURSOR_KEY),
            ("SANGKAR_TELEMETRY", "off"),
        ];
        let ran = run(&program, &args, &self.repo, &env);
        let advice = match indexer {
            Indexer::Rust => {
                "the Rust write path, which is TASK-046 ('the index worker is not implemented in \
                 this binary yet'). Once it lands this fallback indexes the fixture itself and the \
                 same byte comparison runs — sound because TASK-048 pins the ordered row sequences."
            }
            Indexer::Typescript => {
                "recapture, or fix the fixture; a golden is never relaxed to fit a broken run."
            }
        };
        assert!(
            ran.code == 0,
            "goldens: the {} indexer could not publish {fixture} (exit {}), so this run stops here \
             and compares nothing. Its stderr was:\n{}\n{advice}",
            indexer.name(),
            ran.code,
            ran.stderr.trim_end(),
        );
        assert!(
            ran.stdout.is_empty(),
            "goldens: indexing {fixture} wrote to stdout ({:?}); a worker's report belongs on stderr",
            ran.stdout
        );
    }

    /// Builds the second, cold archive the `index` golden was captured on, and returns it unpublished.
    fn build_cold(&self, fixture: &str) -> Built {
        let root = self.cold(fixture);
        let archive = root.join(fixture);
        let data = root.join("data");
        make_removable(&root);
        let _ = fs::remove_dir_all(&root);
        copy_tree(&self.fixtures.join(fixture), &archive);
        fs::create_dir_all(&data).expect("a writable cold data dir");
        let _ = stage_unreadable(&archive);
        Built {
            archive,
            data,
            home: self.home(fixture),
        }
    }
}

/// Creates `broken`'s unreadable directory and proves it is unreadable.
///
/// A mode bit is not a portable checkout artefact (`rust/tests/parity/README.md`), so the case exists
/// only while a run is executing. `Ok(None)` means this machine cannot stage it — uid 0 reads a 000
/// directory — and the caller skips that fixture out loud rather than comparing a graph the capture
/// never built.
fn stage_unreadable(archive: &Path) -> Option<String> {
    let locked = archive.join("locked");
    if !archive.ends_with("broken") {
        return None;
    }
    fs::create_dir(&locked).expect("the unreadable directory");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))
        .expect("the unreadable directory's mode");
    if fs::read_dir(&locked).is_ok() {
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));
        return Some(format!(
            "{} is still readable by this process (uid 0 reads a mode-000 directory), so every \
             `broken` golden would be short one `not_indexed` row",
            locked.display()
        ));
    }
    None
}

/// Rewrites the publish's identity to the pair the goldens recorded.
///
/// Three columns carry it on a first publish — `generation.id`/`published_at`, `meta.generation` and
/// every `file.indexed_generation` — plus the two tombstone columns, empty here but the same fact and
/// a trap the day a fixture publishes twice. The writer handle sets `journal_mode = WAL` (its own
/// precondition, `sangkar-store/src/db.rs:380-391`); the pragma is restored to the `delete` mode the
/// publisher sealed with, so the file this suite hands the server is the kind of file a publish
/// really leaves behind.
fn pin_publish(db: &Path, generation: &str, indexed_at: &str) {
    let conn = sangkar_store::db::open_writer(db)
        .unwrap_or_else(|error| panic!("goldens: cannot open {}: {}", db.display(), error.message));
    let connection = conn.connection();
    let current: String = connection
        .query_row(
            "SELECT id FROM generation ORDER BY seq DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|error| panic!("goldens: cannot read the generation: {error}"));
    if current != generation {
        let sql = format!(
            "UPDATE generation SET id = {new}, published_at = {at} WHERE id = {old};\n\
             UPDATE meta SET value = {new} WHERE key = 'generation';\n\
             UPDATE file SET indexed_generation = {new} WHERE indexed_generation = {old};\n\
             UPDATE tombstone SET last_seen_generation = {new} WHERE last_seen_generation = {old};\n\
             UPDATE tombstone SET removed_in_generation = {new} WHERE removed_in_generation = {old};",
            new = sql_literal(generation),
            old = sql_literal(&current),
            at = sql_literal(indexed_at),
        );
        connection.execute_batch(&sql).unwrap_or_else(|error| {
            panic!(
                "goldens: cannot pin {} to {generation}: {error}",
                db.display()
            )
        });
    }
    connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode = delete;")
        .unwrap_or_else(|error| panic!("goldens: cannot reseal {}: {error}", db.display()));
    let mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap_or_else(|error| panic!("goldens: cannot read journal_mode: {error}"));
    assert_eq!(
        mode,
        "delete",
        "goldens: {} is left in {mode} journal mode; a publish seals the file the server reads",
        db.display()
    );
}

/// A SQL string literal. The values written here are a `g-` id and an ISO instant, so the only
/// escaping that can apply is a doubled quote — written anyway so a future value cannot break it.
fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------------------------
// One golden, as the capture wrote it
// ---------------------------------------------------------------------------------------------

/// `{args, stdout, exit_code}`: the whole contract a golden carries.
struct Golden {
    args: Vec<String>,
    stdout: String,
    exit_code: i64,
}

fn load_golden(path: &Path) -> Golden {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("goldens: cannot read {}: {error}", path.display()));
    let document: Value = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("goldens: {} is not JSON: {error}", path.display()));
    let args = document["args"]
        .as_array()
        .unwrap_or_else(|| panic!("goldens: {} has no 'args' list", path.display()))
        .iter()
        .map(|value| {
            value
                .as_str()
                .unwrap_or_else(|| panic!("goldens: {} has a non-string in 'args'", path.display()))
                .to_owned()
        })
        .collect();
    let stdout = document["stdout"]
        .as_str()
        .unwrap_or_else(|| panic!("goldens: {} has no 'stdout' string", path.display()))
        .to_owned();
    let exit_code = document["exit_code"]
        .as_i64()
        .unwrap_or_else(|| panic!("goldens: {} has no 'exit_code' integer", path.display()));
    Golden {
        args,
        stdout,
        exit_code,
    }
}

// ---------------------------------------------------------------------------------------------
// The comparator: scripts/parity/compare.py, ported
// ---------------------------------------------------------------------------------------------

/// The comparison class of a value. Booleans are not numbers here, and every JSON number is one
/// class — `1` and `1.0` are the same number to `compare.py`, and to this port.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Number,
    String,
    Null,
    List,
    Object,
}

fn kind_of(value: &Value) -> Kind {
    match value {
        Value::Bool(_) => Kind::Bool,
        Value::Number(_) => Kind::Number,
        Value::String(_) => Kind::String,
        Value::Null => Kind::Null,
        Value::Array(_) => Kind::List,
        Value::Object(_) => Kind::Object,
    }
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Bool => "bool",
            Self::Number => "number",
            Self::String => "string",
            Self::Null => "null",
            Self::List => "list",
            Self::Object => "object",
        }
    }
}

impl Imprecise {
    /// What the golden promises about the field, in the words a failure prints.
    fn expected(self) -> &'static str {
        match self {
            Self::Instant => "an ISO-8601 instant",
            Self::Count => "a non-negative number",
        }
    }
}

/// A value with its type, so `1` and `"1"` cannot look equal in a report.
fn describe(value: &Value) -> String {
    match value {
        Value::Bool(flag) => format!("bool {flag}"),
        Value::Number(number) => format!("number {number}"),
        Value::String(text) => serde_json::to_string(text).unwrap_or_else(|_| format!("{text:?}")),
        Value::Null => "null".to_owned(),
        Value::Array(items) => format!("list[{}]", items.len()),
        Value::Object(map) => format!("object{{{} keys}}", map.len()),
    }
}

/// JSON-pointer escaping (RFC 6901 §3): `~` then `/`, in that order.
fn escape(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// One child step of a JSON pointer; the root pointer is the empty string.
fn at(path: &str, token: &str) -> String {
    format!("{path}/{}", escape(token))
}

/// The root pointer as a report prints it.
fn shown(path: &str) -> &str {
    if path.is_empty() { "/" } else { path }
}

/// The imprecision at a pointer, if the table names one.
fn imprecision(path: &str) -> Option<Imprecise> {
    IMPRECISE
        .iter()
        .find(|(named, _)| *named == path)
        .map(|(_, shape)| *shape)
}

/// Walks both values in parallel, appending one line per difference at its JSON pointer.
fn compare(expected: &Value, actual: &Value, path: &str, diffs: &mut Vec<String>) {
    if let Some(shape) = imprecision(path) {
        if !shape_agrees(shape, actual) {
            diffs.push(format!(
                "{}: {} (expected {}) vs {} (actual)",
                shown(path),
                shape.expected(),
                describe(expected),
                describe(actual)
            ));
        }
        return;
    }

    let (left, right) = (kind_of(expected), kind_of(actual));
    if left != right {
        diffs.push(format!(
            "{}: type {} (expected) vs {} (actual)",
            shown(path),
            left.name(),
            right.name()
        ));
        return;
    }

    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            for (key, value) in expected {
                if !actual.contains_key(key) {
                    diffs.push(format!(
                        "{}: missing in actual ({})",
                        at(path, key),
                        describe(value)
                    ));
                }
            }
            for (key, value) in actual {
                if !expected.contains_key(key) {
                    diffs.push(format!(
                        "{}: unexpected in actual ({})",
                        at(path, key),
                        describe(value)
                    ));
                }
            }
            for (key, value) in expected {
                if let Some(found) = actual.get(key) {
                    compare(value, found, &at(path, key), diffs);
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                diffs.push(format!(
                    "{}: length {} (expected) vs {} (actual)",
                    shown(path),
                    expected.len(),
                    actual.len()
                ));
            }
            // Index by index, so a reordered pair reports the position that moved: array order is
            // contract (a cursor is a position in a generation's stable ordering).
            for (index, value) in expected.iter().enumerate() {
                if let Some(found) = actual.get(index) {
                    compare(value, found, &at(path, &index.to_string()), diffs);
                }
            }
        }
        (Value::Number(expected), Value::Number(actual)) => {
            if expected.as_f64() != actual.as_f64() {
                diffs.push(format!(
                    "{}: number {expected} (expected) vs number {actual} (actual)",
                    shown(path)
                ));
            }
        }
        _ => {
            if expected != actual {
                diffs.push(format!(
                    "{}: {} (expected) vs {} (actual)",
                    shown(path),
                    describe(expected),
                    describe(actual)
                ));
            }
        }
    }
}

/// Whether a value has the shape an imprecise field must keep.
fn shape_agrees(shape: Imprecise, value: &Value) -> bool {
    match shape {
        Imprecise::Instant => value.as_str().is_some_and(is_instant),
        Imprecise::Count => value.as_u64().is_some(),
    }
}

/// `2026-11-24T04:44:41.665Z` — the one timestamp shape the envelopes carry.
fn is_instant(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 24
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[23] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 23) || byte.is_ascii_digit()
        })
}

/// The value at a JSON pointer, if it is there.
fn lookup<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut cursor = root;
    for token in pointer.split('/').skip(1) {
        cursor = cursor.get(token)?;
    }
    Some(cursor)
}

/// Replaces the value at a JSON pointer, so the byte comparison can run over a document whose
/// imprecise fields carry this run's numbers. Returns whether the pointer was there to replace.
///
/// The pointers in [`IMPRECISE`] carry no RFC 6901 escapes, so their tokens are used as written.
fn replace_at(root: &mut Value, pointer: &str, replacement: &Value) -> bool {
    let tokens: Vec<&str> = pointer.split('/').skip(1).collect();
    let Some((last, parents)) = tokens.split_last() else {
        return false;
    };
    let mut cursor = root;
    for token in parents {
        match cursor.get_mut(*token) {
            Some(next) => cursor = next,
            None => return false,
        }
    }
    match cursor.get_mut(*last) {
        Some(slot) => {
            *slot = replacement.clone();
            true
        }
        None => false,
    }
}

/// The payload of a cursor token, for the one message that must say *which* field of the cursor
/// moved. The token is `base64url(payload) "." base64url(mac)`, and the MAC is over the payload, so
/// a token whose `repo` differs can only be explained, never re-signed.
fn cursor_payload(token: &str) -> Option<Value> {
    let segment = token.rsplit_once('.').map_or(token, |(payload, _)| payload);
    let bytes = decode_base64url(segment)?;
    serde_json::from_slice(&bytes).ok()
}

/// RFC 4648 base64url, no padding — the alphabet `src/shared/cursor.ts` writes.
fn decode_base64url(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut accumulator: u32 = 0;
    let mut bits = 0;
    for character in text.chars() {
        let value = match character {
            'A'..='Z' => character as u32 - 'A' as u32,
            'a'..='z' => character as u32 - 'a' as u32 + 26,
            '0'..='9' => character as u32 - '0' as u32 + 52,
            '-' => 62,
            '_' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------------------------

/// Replays every golden and reports, in full, what did not match.
///
/// One test rather than one per fixture: the two indexer modes must print a single count and a single
/// fixture list, and "55 of 57 compared" has to be one number rather than a tally a reader adds up.
#[test]
fn every_golden_answers_the_same() {
    let replay = Replay::locate();
    let indexer = Indexer::detect(&replay.repo);
    println!(
        "goldens: replaying {} against {SANGGAR} with the {} indexer",
        replay.goldens.display(),
        indexer.name()
    );
    println!(
        "goldens: fixtures={} replay_root={}",
        FIXTURES.join(","),
        replay.root.display()
    );
    let _lock = ReplayLock::acquire(&replay.root);

    // `compared` counts every golden that was replayed and judged; `matched` the ones that agreed.
    // The floor below is about the first number: a suite that stopped replaying files must fail even
    // when everything it did replay agreed.
    let mut compared = 0_usize;
    let mut matched = 0_usize;
    let mut skipped_goldens = 0_usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut diffs: Vec<String> = Vec::new();
    let mut structure_only: Vec<String> = Vec::new();

    for fixture in FIXTURES {
        let names = replay.golden_names(fixture);
        assert!(
            names.len() >= GOLDENS_PER_FIXTURE_FLOOR,
            "goldens: {fixture} has {} goldens, below the {GOLDENS_PER_FIXTURE_FLOOR} \
             capture-goldens.ts writes; a suite that shrank must not pass",
            names.len()
        );
        let pinned = replay.captured_publish(fixture, &names);
        println!(
            "goldens: {fixture}: {} goldens, pinned generation {} at {}",
            names.len(),
            pinned.0,
            pinned.1
        );

        let built = match replay.build(fixture, indexer, &pinned) {
            Ok(built) => built,
            Err(reason) => {
                println!(
                    "goldens: SKIP {fixture}: its {} goldens were NOT compared — {reason}",
                    names.len()
                );
                skipped_goldens += names.len();
                skipped.push(format!("{fixture} ({} goldens): {reason}", names.len()));
                continue;
            }
        };
        let cold = names
            .iter()
            .any(|name| name.starts_with("index."))
            .then(|| replay.build_cold(fixture));

        for name in &names {
            let tool = name.split('.').next().expect("a file name has a stem");
            let golden = load_golden(&replay.goldens.join(fixture).join(name));
            let (archive, data) = if tool == "index" {
                let cold = cold.as_ref().expect("the index golden's cold archive");
                (cold.archive.clone(), cold.data.clone())
            } else {
                (built.archive.clone(), built.data.clone())
            };

            // The bootstrap verb goes through the indexer that built this fixture, not through the
            // server: in phase 2 that is `bun run bin/sangkar.ts`, and after phase 5 it is this
            // binary's own write path.
            let (program, mut args) = if tool == "index" {
                let (program, mut prefix) = indexer_program(indexer);
                prefix.push("index".to_owned());
                (program, prefix)
            } else {
                (OsString::from(SANGGAR), vec![tool.to_owned()])
            };
            args.extend(golden.args.iter().cloned());
            if tool == "index" {
                args.push("--yes".to_owned());
            }
            args.extend([
                "--json".to_owned(),
                "--repo-root".to_owned(),
                archive.to_string_lossy().into_owned(),
                "--data-dir".to_owned(),
                data.to_string_lossy().into_owned(),
            ]);
            let home = built.home.to_string_lossy().into_owned();
            let env = [
                ("SANGKAR_HOME", home.as_str()),
                ("SANGKAR_CURSOR_KEY", FIXED_CURSOR_KEY),
                ("SANGKAR_TELEMETRY", "off"),
            ];
            let ran = run(&program, &args, &replay.repo, &env);
            let label = format!("{fixture}/{name}");

            let mut found: Vec<String> = Vec::new();
            if ran.code != golden.exit_code {
                found.push(format!(
                    "/exit_code: {} (expected) vs {} (actual); stderr: {}",
                    golden.exit_code,
                    ran.code,
                    if ran.stderr.trim().is_empty() {
                        "(empty)"
                    } else {
                        ran.stderr.trim_end()
                    }
                ));
            }

            if golden.stdout.is_empty() {
                // `index` is the bootstrap verb: its contract is the exit code, and an envelope on
                // stdout would be a byte the TypeScript never wrote.
                if !ran.stdout.is_empty() {
                    found.push(format!(
                        "/stdout: expected no bytes (the bootstrap verb has no envelope), actual {}",
                        describe(&Value::String(ran.stdout.clone()))
                    ));
                }
            } else {
                match (
                    serde_json::from_str::<Value>(&golden.stdout),
                    serde_json::from_str::<Value>(&ran.stdout),
                ) {
                    (Ok(expected), Ok(actual)) => {
                        compare(&expected, &actual, "", &mut found);
                        if !byte_compare(&golden, &expected, &actual, &ran, &mut found) {
                            structure_only.push(label.clone());
                        }
                        cursor_bytes(&expected, &actual, &mut found);
                    }
                    (Ok(_), Err(error)) => found.push(format!(
                        "/stdout: not JSON ({error}); the run printed {:?}",
                        excerpt(&ran.stdout, 0)
                    )),
                    (Err(error), _) => {
                        found.push(format!("/stdout: the golden is not JSON ({error})"));
                    }
                }
            }

            compared += 1;
            if found.is_empty() {
                matched += 1;
                println!("goldens:   ok   {label}");
            } else {
                diffs.extend(found.into_iter().map(|line| format!("{label}: {line}")));
            }
        }
    }

    let failing = diffs
        .iter()
        .map(|line| line.split(':').next().unwrap_or(line))
        .collect::<BTreeSet<_>>()
        .len();
    println!(
        "goldens: {compared} compared ({matched} identical), {skipped_goldens} skipped in {} \
         fixture(s), {failing} file(s) with differences{}",
        skipped.len(),
        if structure_only.is_empty() {
            String::new()
        } else {
            format!(
                "; {} compared structurally only (the oracle does not round-trip through this \
                 serialiser)",
                structure_only.len()
            )
        }
    );
    if !structure_only.is_empty() {
        println!("goldens: structural-only {}", structure_only.join(","));
    }
    for line in &skipped {
        println!("goldens: skipped {line}");
    }
    for line in &diffs {
        println!("goldens: DIFF {line}");
    }
    assert!(
        compared + skipped_goldens >= FIXTURES.len() * GOLDENS_PER_FIXTURE_FLOOR,
        "goldens: {compared} replayed and {skipped_goldens} skipped of the {} goldens \
         capture-goldens.ts writes; the suite must compare every golden or name the reason it did \
         not",
        FIXTURES.len() * GOLDENS_PER_FIXTURE_FLOOR
    );
    assert!(
        diffs.is_empty(),
        "goldens: {} difference(s) across {failing} golden file(s); every line names a JSON pointer",
        diffs.len()
    );
}

/// Compares the rendered document byte for byte once the imprecise fields carry this run's numbers.
///
/// This is the assertion the structural walk cannot make — object key order, number spelling and the
/// one trailing newline are all bytes — and it only runs when the *golden* round-trips through this
/// serialiser, so a difference is never an artefact of how `serde_json` writes a document the
/// TypeScript wrote. Returns whether the bytes were comparable at all.
fn byte_compare(
    golden: &Golden,
    expected: &Value,
    actual: &Value,
    ran: &Ran,
    diffs: &mut Vec<String>,
) -> bool {
    let round_trip = serde_json::to_string(expected)
        .ok()
        .map(|text| format!("{text}\n"));
    if round_trip.as_deref() != Some(golden.stdout.as_str()) {
        return false;
    }
    let mut masked = expected.clone();
    for (pointer, _) in IMPRECISE {
        if let Some(replacement) = lookup(actual, pointer) {
            replace_at(&mut masked, pointer, replacement);
        }
    }
    let Ok(rendered) = serde_json::to_string(&masked) else {
        return false;
    };
    let rendered = format!("{rendered}\n");
    if rendered != ran.stdout {
        let offset = rendered
            .bytes()
            .zip(ran.stdout.bytes())
            .take_while(|(left, right)| left == right)
            .count();
        // A byte difference is reported as bytes, because object member order is contract: the
        // TypeScript writes the key order its handler declared, and `--json` is a rendering of that
        // declaration. The diagnosis says when the only thing that moved is that order, so the
        // reader knows whether the fix is a field's position or its value.
        let reordered = serde_json::to_string(&adopt_key_order(&masked, actual))
            .is_ok_and(|text| format!("{text}\n") == ran.stdout);
        diffs.push(format!(
            "/stdout: bytes differ at offset {offset} ({} expected vs {} actual){}; expected {:?}, \
             actual {:?}",
            rendered.len(),
            ran.stdout.len(),
            if reordered {
                ", and the only difference is object member order: the same document with the \
                 actual's member order re-serialises identically"
            } else {
                ""
            },
            excerpt(&rendered, offset),
            excerpt(&ran.stdout, offset)
        ));
    }
    true
}

/// Rebuilds `expected` with every object's members in `actual`'s order, values unchanged, so a
/// difference that is *only* member order can be named as such instead of being guessed at.
fn adopt_key_order(expected: &Value, actual: &Value) -> Value {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let mut ordered = Map::new();
            for (key, value) in right {
                if let Some(mine) = left.get(key) {
                    ordered.insert(key.clone(), adopt_key_order(mine, value));
                }
            }
            // A member the actual does not carry keeps its place at the end; the structural walk has
            // already reported it as missing.
            for (key, value) in left {
                ordered.entry(key.clone()).or_insert_with(|| value.clone());
            }
            Value::Object(ordered)
        }
        (Value::Array(left), Value::Array(right)) => Value::Array(
            left.iter()
                .enumerate()
                .map(|(index, value)| match right.get(index) {
                    Some(found) => adopt_key_order(value, found),
                    None => value.clone(),
                })
                .collect(),
        ),
        _ => expected.clone(),
    }
}

/// The cursor contract, stated as bytes: a token that differs is a different generation, repo,
/// per-tool `argsHash` or page position, and the decoded payloads say which.
fn cursor_bytes(expected: &Value, actual: &Value, diffs: &mut Vec<String>) {
    let (Some(left), Some(right)) = (expected["next"].as_str(), actual["next"].as_str()) else {
        return;
    };
    if left == right {
        return;
    }
    diffs.push(format!(
        "/next: the cursor must be byte-identical; expected payload {} vs actual payload {}",
        cursor_payload(left).map_or_else(|| "unreadable".to_owned(), |payload| payload.to_string()),
        cursor_payload(right)
            .map_or_else(|| "unreadable".to_owned(), |payload| payload.to_string()),
    ));
}

/// A few bytes either side of an offset, so a byte difference is readable without a hex dump.
fn excerpt(text: &str, offset: usize) -> String {
    let start = offset.saturating_sub(24);
    let end = (offset + 24).min(text.len());
    text.get(start..end).map_or_else(
        || format!("<offset {offset} not a character boundary>"),
        str::to_owned,
    )
}

// ---------------------------------------------------------------------------------------------
// The comparator's own tests: the Done-when, without touching the oracle
// ---------------------------------------------------------------------------------------------

#[test]
fn comparator_names_the_field_that_moved() {
    let expected = serde_json::json!({"payload": {"coverage": {"failed": 0}}});
    let actual = serde_json::json!({"payload": {"coverage": {"failed": 1}}});
    let mut diffs = Vec::new();
    compare(&expected, &actual, "", &mut diffs);
    assert_eq!(
        diffs,
        ["/payload/coverage/failed: number 0 (expected) vs number 1 (actual)"]
    );
}

#[test]
fn comparator_reports_key_sets_order_and_length() {
    let expected = serde_json::json!({"a": [1, 2], "gone": true, "k/~": "x"});
    let actual = serde_json::json!({"a": [2, 1], "extra": null, "k/~": "y"});
    let mut diffs = Vec::new();
    compare(&expected, &actual, "", &mut diffs);
    assert_eq!(
        diffs,
        [
            "/gone: missing in actual (bool true)",
            "/extra: unexpected in actual (null)",
            "/a/0: number 1 (expected) vs number 2 (actual)",
            "/a/1: number 2 (expected) vs number 1 (actual)",
            "/k~1~0: \"x\" (expected) vs \"y\" (actual)",
        ]
    );
}

#[test]
fn comparator_compares_imprecise_fields_by_shape() {
    let expected = serde_json::json!({
        "read_status": {"stale_checked_at": "2026-11-24T04:44:41.665Z"},
        "payload": {"graph": {"size_bytes": 196608}},
    });
    let reshaped = serde_json::json!({
        "read_status": {"stale_checked_at": "2026-12-01T00:00:00.000Z"},
        "payload": {"graph": {"size_bytes": 172032}},
    });
    let mut diffs = Vec::new();
    compare(&expected, &reshaped, "", &mut diffs);
    assert!(diffs.is_empty(), "{diffs:?}");

    let wrong = serde_json::json!({
        "read_status": {"stale_checked_at": "whenever"},
        "payload": {"graph": {"size_bytes": -1}},
    });
    compare(&expected, &wrong, "", &mut diffs);
    assert_eq!(
        diffs,
        [
            "/read_status/stale_checked_at: an ISO-8601 instant (expected \"2026-11-24T04:44:41.665Z\") vs \"whenever\" (actual)",
            "/payload/graph/size_bytes: a non-negative number (expected number 196608) vs number -1 (actual)",
        ]
    );
}
