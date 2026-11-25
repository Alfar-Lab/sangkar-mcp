//! The `repo` verb on real files and a stated machine: the CLI-facing half of
//! `tests/repo-config.test.ts`, plus the registry lines the command line never had in TypeScript.
//!
//! Every case drives [`run_repo_cli`] with two buffers instead of the process's streams, a temporary
//! home stated through the environment seam, and trees under `std::env::temp_dir()`. The suite
//! therefore never reads or writes the machine's own `~/.config/sangkar/`, nor this repository's
//! `.sangkar/`, which belongs to the daemon. What is asserted is what a caller sees: the bytes on
//! each stream, the code the process would exit with, and the files the run left behind.
//!
//! The path comparison is a string comparison, so the tree root is canonicalised once when it is
//! created: on a system whose `$TMPDIR` is a symlink, the verb's own `canonical_root` would answer
//! the resolved spelling and every expectation here would be a differently spelled path.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_cli::args::{Verb, verb_spec};
use sangkar_cli::repo::run_repo_cli;
use sangkar_core::config::load_config;
use sangkar_core::registry::{self, REGISTRY_VERSION};
use sangkar_core::repo_config::{
    self, DEFAULT_REPO_SETTINGS, RepoConfig, WATCH_INTERVAL_MAX_MS, WATCH_INTERVAL_MIN_MS,
};

/// A temporary directory that removes itself, so the suite leaves no home and no tree behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-repo-cmd-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        let path = fs::canonicalize(&path).expect("canonicalise the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// One machine: a temporary home, which is both `SANGKAR_HOME` and `$HOME` for the run, and the
/// trees a case registers.
struct Machine {
    root: TempDir,
}

/// What one invocation answered.
struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Machine {
    fn new(label: &str) -> Self {
        Self {
            root: TempDir::new(label),
        }
    }

    fn home(&self) -> &Path {
        self.root.path()
    }

    /// A tree that looks indexed — a directory with a `.git` marker — so autodetection stops there
    /// instead of walking out of the temporary root.
    fn tree(&self, name: &str) -> PathBuf {
        let tree = self.home().join(name);
        fs::create_dir_all(tree.join(".git")).expect("create the tree");
        tree
    }

    /// One invocation in `cwd`, against this machine's home. `argv` is the tokens *after* the verb
    /// name, which is the tail the dispatcher hands over — `sangkar repo` is already consumed.
    fn run_in(&self, cwd: &Path, argv: &[&str]) -> Run {
        self.run_with(self.home(), cwd, argv)
    }

    /// One invocation in the machine's home.
    fn run(&self, argv: &[&str]) -> Run {
        self.run_in(self.home(), argv)
    }

    /// One invocation against a home this machine does not own, for the case about a home the verb
    /// cannot write into.
    fn run_with(&self, home: &Path, cwd: &Path, argv: &[&str]) -> Run {
        let vars = vars_of(home);
        let env = |name: &str| vars.get(name).cloned();
        let argv: Vec<String> = argv.iter().map(|arg| (*arg).to_owned()).collect();
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = run_repo_cli(&argv, &env, cwd, &mut stdout, &mut stderr);
        Run {
            code,
            stdout: String::from_utf8(stdout).expect("stdout is UTF-8"),
            stderr: String::from_utf8(stderr).expect("stderr is UTF-8"),
        }
    }
}

/// The environment the verb sees: one home as both the config home and `$HOME`, and nothing else,
/// so a tilde expands into the temporary tree rather than into the account running the suite.
fn vars_of(home: &Path) -> HashMap<&'static str, String> {
    let home = home.display().to_string();
    HashMap::from([("SANGKAR_HOME", home.clone()), ("HOME", home)])
}

/// The registry file the run wrote, composed by `config::load_config` rather than by the verb's own
/// constant: the two must name one file, so this is where a drift between them fails.
fn registry_path(machine: &Machine) -> PathBuf {
    let vars = vars_of(machine.home());
    let env = |name: &str| vars.get(name).cloned();
    load_config(&[], &env, machine.home())
        .expect("the config of a temporary machine")
        .registry_path
}

/// `<tree>/.sangkar/config.json`, the file `repo config` writes.
fn settings_path(tree: &Path) -> PathBuf {
    repo_config::repo_config_path(&tree.join(".sangkar"))
}

/// The tree's settings as any reader sees them.
fn settings_of(tree: &Path) -> RepoConfig {
    repo_config::read_repo_config(tree, None, DEFAULT_REPO_SETTINGS)
}

/// Every line of a multi-line answer, with the trailing newline a verb's stdout ends in.
fn lines(lines: &[&str]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

/// A writer that refuses every byte, for the one case about a stream the caller has already closed.
struct Closed;

impl Write for Closed {
    fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn add_registers_the_tree_and_reports_its_canonical_path() {
    let machine = Machine::new("add");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let run = machine.run(&["add", &arg]);

    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, format!("registered {}\n", tree.display()));
    assert_eq!(run.stderr, "");
    assert_eq!(
        registry::read_registry(&registry_path(&machine)).repos,
        vec![tree]
    );
}

#[test]
fn add_twice_answers_already_registered_and_keeps_one_entry() {
    let machine = Machine::new("add-twice");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let first = machine.run(&["add", &arg]);
    let second = machine.run(&["add", &arg]);

    assert_eq!(first.code, 0);
    assert_eq!(first.stdout, format!("registered {}\n", tree.display()));
    assert_eq!(second.code, 0);
    assert_eq!(
        second.stdout,
        format!("already registered {}\n", tree.display())
    );
    assert_eq!(second.stderr, "");
    assert_eq!(
        registry::read_registry(&registry_path(&machine)).repos,
        vec![tree]
    );
}

#[test]
fn a_registry_that_cannot_be_written_fails_the_run_instead_of_claiming_success() {
    let machine = Machine::new("unwritable");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    // A home that is a regular file: the registry's parent cannot be created, `register_repo`
    // swallows the write error and answers `added: false`, and the verb must not read that as
    // "already registered" — the entry is not in the registry it hands back.
    let blocked = machine.home().join("blocked");
    fs::write(&blocked, b"not a directory").expect("write the blocking file");

    let run = machine.run_with(&blocked, machine.home(), &["add", &arg]);

    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, "");
    assert!(
        run.stderr.starts_with("sangkar repo: could not write "),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("blocked"), "{}", run.stderr);
}

#[test]
fn remove_unregisters_a_registered_tree_and_leaves_the_tree_alone() {
    let machine = Machine::new("remove");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();
    machine.run(&["add", &arg]);

    let run = machine.run(&["remove", &arg]);

    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, format!("removed {}\n", tree.display()));
    assert_eq!(run.stderr, "");
    assert!(
        registry::read_registry(&registry_path(&machine))
            .repos
            .is_empty()
    );
    // Unregistering is not deleting: the tree, its marker and its settings are the operator's.
    assert!(tree.join(".git").is_dir());
}

#[test]
fn remove_of_an_unregistered_tree_answers_not_registered_and_exits_one() {
    let machine = Machine::new("remove-unknown");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let run = machine.run(&["remove", &arg]);

    assert_eq!(run.code, 1);
    assert_eq!(run.stdout, format!("not registered {}\n", tree.display()));
    assert_eq!(run.stderr, "");
    assert!(!registry_path(&machine).exists(), "nothing was written");
}

#[test]
fn list_prints_one_registered_path_per_line_and_nothing_for_an_empty_registry() {
    let machine = Machine::new("list");
    let first = machine.tree("first");

    // A machine that never registered anything has an empty answer, not a refusal: the warning the
    // registry module logs for the missing file is stderr's business and is not asserted here.
    let empty = machine.run(&["list"]);
    assert_eq!(empty.code, 0);
    assert_eq!(empty.stdout, "");

    let second = machine.tree("second");
    for tree in [&first, &second] {
        let arg = tree.display().to_string();
        machine.run(&["add", &arg]);
    }

    let run = machine.run(&["list"]);

    assert_eq!(run.code, 0);
    assert_eq!(
        run.stdout,
        lines(&[&first.display().to_string(), &second.display().to_string()])
    );
    assert_eq!(run.stderr, "");
}

#[test]
fn list_json_prints_the_registry_document_the_module_writes() {
    let machine = Machine::new("list-json");

    let empty = machine.run(&["list", "--json"]);
    let expected = serde_json::to_string(&serde_json::json!({
        "version": REGISTRY_VERSION,
        "repos": [],
    }))
    .expect("the empty document serialises");
    assert_eq!(empty.code, 0);
    assert_eq!(empty.stdout, format!("{expected}\n"));

    let first = machine.tree("first");
    let second = machine.tree("second");
    for tree in [&first, &second] {
        let arg = tree.display().to_string();
        machine.run(&["add", &arg]);
    }

    let run = machine.run(&["list", "--json"]);

    assert_eq!(run.code, 0);
    // Byte for byte the body `registry::write_registry` put on disk, plus the line's newline: the
    // key order and the framing are the registry module's, so the printed document and the stored
    // one cannot drift apart.
    let stored = fs::read_to_string(registry_path(&machine)).expect("the registry file");
    assert_eq!(run.stdout, format!("{stored}\n"));

    let document: serde_json::Value =
        serde_json::from_str(run.stdout.trim_end()).expect("the document parses as JSON");
    assert_eq!(document["version"], serde_json::json!(REGISTRY_VERSION));
    assert_eq!(
        document["repos"],
        serde_json::json!([first.display().to_string(), second.display().to_string()])
    );
}

#[test]
fn config_writes_the_flags_it_was_given_and_prints_the_effective_settings() {
    let machine = Machine::new("config");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let run = machine.run(&[
        "config",
        &arg,
        "--watch",
        "false",
        "--watch-interval",
        "5000",
        "--telemetry",
        "false",
    ]);

    assert_eq!(run.code, 0);
    assert_eq!(
        run.stdout,
        lines(&["watch=off", "interval=5000ms", "telemetry=off"])
    );
    assert_eq!(run.stderr, "");

    let settings = settings_of(&tree);
    assert!(!settings.watch);
    assert_eq!(settings.watch_interval_ms, 5_000);
    assert!(!settings.telemetry);
    assert!(settings_path(&tree).is_file());

    // A second run with no flags reports the file, not the defaults: the write persisted and the
    // verb reads before it writes.
    let reread = machine.run(&["config", &arg]);
    assert_eq!(reread.code, 0);
    assert_eq!(
        reread.stdout,
        lines(&["watch=off", "interval=5000ms", "telemetry=off"])
    );

    // The flags override exactly what they name: on a fresh tree, the two that were not given keep
    // the machine's values.
    let untouched = machine.tree("untouched");
    let untouched_arg = untouched.display().to_string();
    let first_write = machine.run(&["config", &untouched_arg, "--watch", "false"]);
    assert_eq!(first_write.code, 0);
    assert_eq!(
        first_write.stdout,
        lines(&[
            "watch=off",
            &format!("interval={}ms", DEFAULT_REPO_SETTINGS.watch_interval_ms),
            "telemetry=on",
        ])
    );
}

#[test]
fn an_interval_outside_the_band_is_a_usage_refusal_and_writes_nothing() {
    for out_of_band in [WATCH_INTERVAL_MIN_MS - 1, WATCH_INTERVAL_MAX_MS + 1] {
        let machine = Machine::new("interval-refused");
        let tree = machine.tree("tree");
        let arg = tree.display().to_string();
        let ms = out_of_band.to_string();

        let run = machine.run(&["config", &arg, "--watch-interval", &ms]);

        assert_eq!(run.code, 2, "{} ms must be refused", out_of_band);
        assert_eq!(run.stdout, "");
        // The reason first, then the verb's own block: the band is `repo_config`'s constant, never
        // a number typed into the message.
        assert_eq!(
            run.stderr,
            format!(
                "--watch-interval must be {WATCH_INTERVAL_MIN_MS}..{WATCH_INTERVAL_MAX_MS} ms, not {out_of_band}\n{}\n",
                verb_spec(Verb::Repo).usage
            )
        );
        assert!(
            !settings_path(&tree).exists(),
            "a refused interval must not write a settings file"
        );
    }
}

#[test]
fn a_dash_prefixed_token_is_never_read_as_a_path() {
    let machine = Machine::new("dash");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    // `--watch` belongs to `config`, so `add` must refuse it rather than register a directory
    // called `--watch` — the silent wrong target the TypeScript comment is about.
    let flag = machine.run(&["add", "--watch", "false"]);
    assert_eq!(flag.code, 2);
    assert_eq!(flag.stdout, "");
    assert!(
        flag.stderr.starts_with("\"repo add\" takes no --watch\n"),
        "{}",
        flag.stderr
    );
    assert!(
        !registry_path(&machine).exists(),
        "a refused argument must not write the registry"
    );

    // An option nothing declares is refused as well, and the path it follows is never reached.
    let unknown = machine.run(&["add", "--nope", &arg]);
    assert_eq!(unknown.code, 2);
    assert_eq!(unknown.stdout, "");
    assert!(
        unknown.stderr.starts_with("unknown option \"--nope\"\n"),
        "{}",
        unknown.stderr
    );
    assert!(!registry_path(&machine).exists());
}

#[test]
fn help_answers_the_usage_block_without_validating_the_rest() {
    let machine = Machine::new("help");
    let block = format!("{}\n", verb_spec(Verb::Repo).usage);

    let bare = machine.run(&["--help"]);
    assert_eq!(bare.code, 0);
    assert_eq!(bare.stdout, block);
    assert_eq!(bare.stderr, "");

    // A subcommand's help request is not a broken subcommand.
    let nested = machine.run(&["add", "--help"]);
    assert_eq!(nested.code, 0);
    assert_eq!(nested.stdout, block);
}

#[test]
fn a_missing_subcommand_a_missing_path_and_an_unknown_subcommand_are_usage_refusals() {
    let machine = Machine::new("refusals");
    let block = format!("{}\n", verb_spec(Verb::Repo).usage);

    let bare = machine.run(&[]);
    assert_eq!(bare.code, 2);
    assert_eq!(bare.stdout, "");
    assert_eq!(bare.stderr, format!("repo needs a subcommand\n{block}"));

    let unknown = machine.run(&["frobnicate"]);
    assert_eq!(unknown.code, 2);
    assert_eq!(
        unknown.stderr,
        format!("unknown subcommand \"frobnicate\"\n{block}")
    );

    let no_path = machine.run(&["add"]);
    assert_eq!(no_path.code, 2);
    assert_eq!(no_path.stderr, format!("repo add needs a path\n{block}"));

    let no_remove_path = machine.run(&["remove"]);
    assert_eq!(no_remove_path.code, 2);
    assert_eq!(
        no_remove_path.stderr,
        format!("repo remove needs a path\n{block}")
    );
}

#[test]
fn a_tilde_path_is_expanded_against_the_injected_home() {
    let machine = Machine::new("tilde");
    let tree = machine.tree("tilded");

    let run = machine.run(&["add", "~/tilded"]);

    assert_eq!(run.code, 0);
    // The temporary home, not `<cwd>/~/tilded`: the base is the machine's `$HOME`, which the case
    // states rather than the account the suite runs under.
    assert_eq!(run.stdout, format!("registered {}\n", tree.display()));
    assert_eq!(
        registry::read_registry(&registry_path(&machine)).repos,
        vec![tree]
    );
}

#[test]
fn config_without_a_path_is_refused_rather_than_guessing_the_tree() {
    let machine = Machine::new("config-cwd");
    let tree = machine.tree("standing");

    let run = machine.run_in(&tree, &["config", "--watch", "false"]);

    // `src/shared/cli/repo.ts:178-181`: `repo config needs a path`. The refusal is what keeps a
    // settings write from landing on whichever tree the caller happened to be standing in — the
    // file would carry no trace of which tree it was meant for, so the mistake is invisible later.
    assert_eq!(run.code, 2);
    assert!(run.stdout.is_empty(), "stdout: {:?}", run.stdout);
    assert!(
        run.stderr.contains("repo config needs a path"),
        "stderr: {:?}",
        run.stderr
    );
    assert!(
        !settings_path(&tree).is_file(),
        "a refused call must not write a settings file"
    );
}

#[test]
fn the_globals_are_accepted_next_to_a_subcommand_and_change_nothing() {
    let machine = Machine::new("globals");
    let tree = machine.tree("tree");
    let arg = tree.display().to_string();

    let run = machine.run(&[
        "add",
        &arg,
        "--json",
        "--repo-root",
        "/nonexistent",
        "--data-dir",
        "/nope",
    ]);

    assert_eq!(run.code, 0);
    assert_eq!(run.stdout, format!("registered {}\n", tree.display()));
    assert_eq!(
        registry::read_registry(&registry_path(&machine)).repos,
        vec![tree.clone()]
    );

    // `--data-dir` is inert for `config` too: the settings a tool call reads for this tree are the
    // ones under the tree, not under the flag's value.
    let config = machine.run(&["config", &arg, "--data-dir", "/nope"]);
    assert_eq!(config.code, 0);
    assert!(settings_path(&tree).is_file());
}

#[test]
fn a_stream_that_cannot_be_written_fails_the_run() {
    let machine = Machine::new("closed");
    let tree = machine.tree("tree");
    let argv = vec!["add".to_owned(), tree.display().to_string()];
    let vars = vars_of(machine.home());
    let env = |name: &str| vars.get(name).cloned();
    let mut err = Vec::new();

    let code = run_repo_cli(&argv, &env, machine.home(), &mut Closed, &mut err);

    // The answer never reached the caller, so the run did not succeed — and the failure says which
    // stream it was, rather than exiting 0 over an answer nobody received.
    assert_eq!(code, 1);
    let reported = String::from_utf8(err).expect("stderr is UTF-8");
    assert!(
        reported.starts_with("sangkar repo: could not write the answer:"),
        "{reported}"
    );
}
