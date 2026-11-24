//! Configuration precedence, repository autodetection, the machine registry and the selection
//! rule.
//!
//! Every expected answer here was taken from the TypeScript oracle (`tests/config.test.ts` and
//! `tests/registry.test.ts` under Bun 1.4.4) and ported rather than re-derived. The environment is
//! always injected, so no case mutates the process it shares with the rest of the suite and none
//! depends on the developer's own `$HOME`: paths that would touch a real machine home are asserted
//! through [`sangkar_core::config::resolve_home`] or `canonical_root`'s injected home, never by
//! reading the variable.
//!
//! `tests/repo-config.test.ts` is out of scope: its cases are about the watch file, which TASK-006's
//! `repo_config` owns.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::config::{
    AppConfig, DATA_DIR_NAME, DEFAULT_BUDGETS, DEFAULT_REPO_DEFAULTS, HOME_DIR_NAME,
    MAX_AUTODETECT_LEVELS, RepoConfigSource, RepoDefaults, detect_repo_root, detect_tree,
    expand_home, flag_value, load_config, resolve_home, with_repo,
};
use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::registry::{
    EMPTY_REGISTRY, REGISTRY_VERSION, Registry, SelectRepoInput, canonical_root, read_registry,
    read_registry_with, register_repo_with, remove_repo_with, select_repo, write_registry,
};
use serde_json::Value;

/// A temporary directory that removes itself, so the suite leaves no fixture behind.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let unique = format!(
            "sangkar-config-{label}-{}-{nanos}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = std::env::temp_dir().join(unique);
        fs::create_dir_all(&path).expect("create the temp directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    /// `<self>/<name>`, created as a directory.
    fn dir(&self, name: &str) -> PathBuf {
        let path = self.join(name);
        fs::create_dir_all(&path).expect("create the fixture directory");
        path
    }

    /// `<self>/<name>` holding `body`.
    fn file(&self, name: &str, body: &str) -> PathBuf {
        let path = self.join(name);
        fs::write(&path, body).expect("write the fixture file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A tree the autodetect walk stops at, marked the way the product marks it.
fn repo_with_data_dir(label: &str) -> TempDir {
    let repo = TempDir::new(label);
    repo.dir(DATA_DIR_NAME);
    repo
}

/// A tree the walk stops at on `.git` alone.
fn repo_with_git_dir(label: &str) -> TempDir {
    let repo = TempDir::new(label);
    repo.dir(".git");
    repo
}

/// `<root>/<segments...>`, created.
fn nested(root: &Path, segments: &[&str]) -> PathBuf {
    let mut path = root.to_path_buf();
    for segment in segments {
        path.push(segment);
        fs::create_dir_all(&path).expect("create the nested directory");
    }
    path
}

fn argv(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn env_lookup(env: &HashMap<String, String>) -> impl Fn(&str) -> Option<String> + '_ {
    move |name| env.get(name).cloned()
}

/// The config for one call, with the environment stated rather than inherited.
fn load(args: &[&str], env: &HashMap<String, String>, cwd: &Path) -> AppConfig {
    try_load(args, env, cwd).expect("the config resolves")
}

fn try_load(
    args: &[&str],
    env: &HashMap<String, String>,
    cwd: &Path,
) -> Result<AppConfig, AppError> {
    let argv = argv(args);
    let lookup = env_lookup(env);
    load_config(&argv, &lookup, cwd)
}

/// One warning as the sink received it: the message and its fields, both as text.
#[derive(Clone, Debug)]
struct Record {
    message: String,
    fields: HashMap<String, String>,
}

impl Record {
    fn field(&self, name: &str) -> &str {
        self.fields.get(name).map_or("", String::as_str)
    }
}

/// A warning collector, so "exactly one warning" is an assertion instead of a stderr scrape.
struct Warnings {
    records: RefCell<Vec<Record>>,
}

impl Warnings {
    fn new() -> Self {
        Self {
            records: RefCell::new(Vec::new()),
        }
    }

    fn sink(&self) -> impl Fn(&str, &[(&str, Value)]) + '_ {
        move |message, fields| {
            self.records.borrow_mut().push(Record {
                message: message.to_owned(),
                fields: fields
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), field_text(value)))
                    .collect(),
            });
        }
    }

    fn count(&self) -> usize {
        self.records.borrow().len()
    }

    fn first(&self) -> Record {
        self.records
            .borrow()
            .first()
            .cloned()
            .expect("at least one warning")
    }
}

fn field_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// The hint as the caller reads it: one repo per line, so a test can assert the whole list.
fn hint_lines(error: &AppError) -> Vec<String> {
    error
        .hint
        .as_deref()
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A registry whose entries are canonicalised, the way every reader sees them.
fn registry_of(repos: &[&Path]) -> Registry {
    Registry {
        version: REGISTRY_VERSION,
        repos: repos
            .iter()
            .map(|repo| canonical_root(repo, None))
            .collect(),
    }
}

// ---------------------------------------------------------------------------------------------
// flag_value
// ---------------------------------------------------------------------------------------------

#[test]
fn flag_value_reads_both_forms() {
    assert_eq!(
        flag_value(&argv(&["--repo-root", "/a"]), "repo-root"),
        Some("/a".to_owned())
    );
    assert_eq!(
        flag_value(&argv(&["--repo-root=/a"]), "repo-root"),
        Some("/a".to_owned())
    );
}

#[test]
fn flag_value_returns_none_when_absent_including_a_prefix_lookalike() {
    assert_eq!(flag_value(&argv(&[]), "repo-root"), None);
    assert_eq!(flag_value(&argv(&["index", "--force"]), "repo-root"), None);
    assert_eq!(
        flag_value(&argv(&["--repo-roots", "/a"]), "repo-root"),
        None
    );
    // A bare flag at the end has no value, which is an absence rather than a `--x` value.
    assert_eq!(flag_value(&argv(&["--repo-root"]), "repo-root"), None);
}

#[test]
fn flag_value_scans_the_whole_argv() {
    let after_token = &argv(&["index", "extra-token", "--repo-root", "/a"]);
    assert_eq!(flag_value(after_token, "repo-root"), Some("/a".to_owned()));
    let inline = &argv(&["index", "extra-token", "--repo-root=/a"]);
    assert_eq!(flag_value(inline, "repo-root"), Some("/a".to_owned()));
}

// ---------------------------------------------------------------------------------------------
// detect_tree / detect_repo_root
// ---------------------------------------------------------------------------------------------

#[test]
fn detect_repo_root_finds_the_nearest_data_dir() {
    let root = repo_with_data_dir("detect-data");
    let deep = nested(root.path(), &["src", "shared", "store"]);
    assert_eq!(detect_repo_root(&deep), root.path());
}

#[test]
fn detect_repo_root_falls_back_to_git_and_prefers_sangkar() {
    let git_repo = repo_with_git_dir("detect-git");
    assert_eq!(
        detect_repo_root(&nested(git_repo.path(), &["src"])),
        git_repo.path()
    );

    let both = TempDir::new("detect-both");
    both.dir(".git");
    both.dir(DATA_DIR_NAME);
    assert_eq!(
        detect_repo_root(&nested(both.path(), &["src"])),
        both.path()
    );
}

#[test]
fn detect_repo_root_accepts_a_git_file_as_a_linked_worktree_has() {
    let worktree = TempDir::new("detect-worktree");
    worktree.file(".git", "gitdir: /elsewhere/.git/worktrees/wt\n");
    assert_eq!(
        detect_repo_root(&nested(worktree.path(), &["src"])),
        worktree.path()
    );
}

#[test]
fn detect_repo_root_returns_the_directory_itself_when_nothing_matches() {
    let bare = TempDir::new("detect-bare");
    assert_eq!(detect_repo_root(bare.path()), bare.path());
}

#[test]
fn detect_tree_reports_the_marker_the_depth_and_the_chain_crossed() {
    let root = repo_with_data_dir("tree-data");
    let deep = nested(root.path(), &["src", "shared"]);

    let detected = detect_tree(&deep);

    assert_eq!(detected.root, root.path());
    assert_eq!(detected.via, Some(DATA_DIR_NAME));
    // Two directories above: `src/shared` -> `src` -> the root.
    assert_eq!(detected.levels, 2);
    // The chain runs from the working directory up to the root: it is what a warning prints.
    assert_eq!(
        detected.chain,
        vec![deep.clone(), root.join("src"), root.path().to_path_buf()]
    );
    assert!(!detected.explicit);
}

#[test]
fn detect_tree_reports_no_marker_at_level_zero_for_a_bare_directory() {
    let bare = TempDir::new("tree-bare");

    let detected = detect_tree(bare.path());

    assert_eq!(detected.root, bare.path());
    assert_eq!(detected.via, None);
    assert_eq!(detected.levels, 0);
    assert_eq!(detected.chain, vec![bare.path().to_path_buf()]);
}

#[test]
fn detect_tree_prefers_sangkar_over_git_and_names_the_marker_it_used() {
    let both = TempDir::new("tree-both");
    both.dir(".git");
    both.dir(DATA_DIR_NAME);

    let detected = detect_tree(&nested(both.path(), &["src"]));

    assert_eq!(detected.root, both.path());
    assert_eq!(detected.via, Some(DATA_DIR_NAME));
    assert_eq!(detected.levels, 1);
}

#[test]
fn detect_tree_reads_a_worktree_git_file_as_a_marker() {
    let worktree = TempDir::new("tree-worktree");
    worktree.file(".git", "gitdir: /elsewhere/.git/worktrees/wt\n");

    let detected = detect_tree(worktree.path());

    assert_eq!(detected.via, Some(".git"));
    assert_eq!(detected.levels, 0);
}

#[test]
fn the_autodetect_cap_is_three_and_a_deeper_marker_is_still_reported() {
    assert_eq!(MAX_AUTODETECT_LEVELS, 3);

    let root = repo_with_data_dir("tree-cap");
    let within = detect_tree(&nested(root.path(), &["a", "b", "c"]));
    assert_eq!(within.levels, MAX_AUTODETECT_LEVELS);
    assert!(within.within_autodetect_budget());

    // One level past the budget: the walk still answers, and the refusal belongs to the index gate.
    let past = detect_tree(&nested(root.path(), &["a", "b", "c", "d"]));
    assert_eq!(past.root, root.path());
    assert_eq!(past.levels, MAX_AUTODETECT_LEVELS + 1);
    assert!(!past.within_autodetect_budget());
}

#[test]
fn the_repo_root_flag_names_the_tree_so_nothing_was_walked() {
    let autodetected = repo_with_data_dir("tree-flag-auto");
    let chosen = TempDir::new("tree-flag-chosen");

    let config = load(
        &["index", "--repo-root", &chosen.path().to_string_lossy()],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        &nested(autodetected.path(), &["src"]),
    );

    assert_eq!(
        config.repo_detection,
        sangkar_core::config::RepoDetection {
            root: chosen.path().to_path_buf(),
            via: None,
            levels: 0,
            chain: vec![chosen.path().to_path_buf()],
            explicit: true,
        }
    );
}

#[test]
fn with_repo_states_the_tree_it_was_handed_so_a_served_tree_is_never_a_guess() {
    let autodetected = repo_with_data_dir("tree-with-auto");
    let other = TempDir::new("tree-with-other");
    let config = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        &nested(autodetected.path(), &["src"]),
    );
    assert_eq!(config.repo_detection.levels, 1);

    let rebound = with_repo(&config, other.path(), None);

    assert!(rebound.repo_detection.explicit);
    assert_eq!(rebound.repo_detection.root, other.path());
    assert_eq!(rebound.repo_detection.levels, 0);
}

// ---------------------------------------------------------------------------------------------
// load_config: repo root, data dir, budgets
// ---------------------------------------------------------------------------------------------

#[test]
fn the_repo_root_flag_wins_over_the_autodetected_tree() {
    let autodetected = repo_with_data_dir("config-auto");
    let chosen = TempDir::new("config-chosen");
    let nested_dir = nested(autodetected.path(), &["src"]);
    // Sanity: without the flag the walk lands on `autodetected`.
    assert_eq!(detect_repo_root(&nested_dir), autodetected.path());

    let config = load(
        &["status", "--repo-root", &chosen.path().to_string_lossy()],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        &nested_dir,
    );

    assert_eq!(config.repo_root, chosen.path());
    assert_eq!(config.data_dir, chosen.join(DATA_DIR_NAME));
}

#[test]
fn the_repo_root_flag_works_inline_and_resolves_a_relative_value_against_cwd() {
    let parent = TempDir::new("config-parent");
    let child = nested(parent.path(), &["child"]);
    let env = env_of(&[("SANGKAR_HOME", "/machine/home")]);

    let inline = load(
        &[&format!("--repo-root={}", child.to_string_lossy())],
        &env,
        parent.path(),
    );
    assert_eq!(inline.repo_root, child);

    let relative = load(&["--repo-root=child"], &env, parent.path());
    assert_eq!(relative.repo_root, child);
}

#[test]
fn a_flag_after_another_token_is_still_applied() {
    let chosen = TempDir::new("config-trailing");
    let base = TempDir::new("config-trailing-cwd");
    let env = env_of(&[("SANGKAR_HOME", "/machine/home")]);
    let inline = format!("--repo-root={}", chosen.path().to_string_lossy());

    let bare = load(
        &[
            "index",
            "some-token",
            "--repo-root",
            &chosen.path().to_string_lossy(),
        ],
        &env,
        base.path(),
    );
    assert_eq!(bare.repo_root, chosen.path());

    let attached = load(&["index", "some-token", &inline], &env, base.path());
    assert_eq!(attached.repo_root, chosen.path());
}

#[test]
fn autodetects_the_tree_from_a_nested_cwd_and_puts_the_data_dir_inside_it() {
    let root = repo_with_data_dir("config-nested");
    let config = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        &nested(root.path(), &["app", "src"]),
    );
    assert_eq!(config.repo_root, root.path());
    assert_eq!(config.data_dir, root.join(DATA_DIR_NAME));
}

#[test]
fn falls_back_to_cwd_in_a_bare_directory() {
    let bare = TempDir::new("config-bare");
    let config = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        bare.path(),
    );
    assert_eq!(config.repo_root, bare.path());
    assert_eq!(config.data_dir, bare.join(DATA_DIR_NAME));
    assert_eq!(config.repo_detection.via, None);
}

#[test]
fn the_data_dir_flag_resolves_against_the_repo_root() {
    let root = TempDir::new("config-data");
    let elsewhere = TempDir::new("config-absdata").join("elsewhere");
    let env = env_of(&[("SANGKAR_HOME", "/machine/home")]);
    let scratch = TempDir::new("config-data-cwd");

    let relative = load(
        &[
            "--repo-root",
            &root.path().to_string_lossy(),
            "--data-dir",
            "var/graph",
        ],
        &env,
        scratch.path(),
    );
    assert_eq!(relative.repo_root, root.path());
    assert_eq!(relative.data_dir, root.join("var").join("graph"));

    let inline = load(
        &[
            &format!("--repo-root={}", root.path().to_string_lossy()),
            &format!("--data-dir={}", elsewhere.to_string_lossy()),
        ],
        &env,
        scratch.path(),
    );
    assert_eq!(inline.data_dir, elsewhere);
}

#[test]
fn stale_repo_root_environment_variables_are_inert() {
    let bare = TempDir::new("config-envignored");
    let flagged = TempDir::new("config-envignored-flag");
    let env = env_of(&[
        ("SANGKAR_HOME", "/machine/home"),
        ("SANGKAR_REPO_ROOT", "/should/not/be/used"),
        ("SANGKAR_DATA_DIR", "/should/not/be/used/either"),
        ("SANGKAR_PROFILE", "admin"),
        ("SANGKAR_CURSOR_KEY", &"k".repeat(32)),
    ]);

    let config = load(&[], &env, bare.path());
    assert_eq!(config.repo_root, bare.path());
    assert_eq!(config.data_dir, bare.join(DATA_DIR_NAME));
    assert_eq!(config.cursor_key, "k".repeat(32));

    // The tree comes from the flag even while the stale variables are set.
    let overridden = load(
        &["--repo-root", &flagged.path().to_string_lossy()],
        &env,
        bare.path(),
    );
    assert_eq!(overridden.repo_root, flagged.path());
    assert_eq!(overridden.data_dir, flagged.join(DATA_DIR_NAME));
}

#[test]
fn the_fallback_budgets_and_repo_defaults_are_the_typescript_values() {
    let bare = TempDir::new("config-budget");
    let config = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        bare.path(),
    );
    assert_eq!(config.budgets.default_tokens, 1_200);
    assert_eq!(config.budgets.max_tokens, 32_000);
    assert_eq!(config.budgets, DEFAULT_BUDGETS);
    assert_eq!(
        config.repo_defaults,
        RepoDefaults {
            watch: true,
            watch_interval_ms: 2_000,
            telemetry: true,
        }
    );
    assert_eq!(config.repo_defaults, DEFAULT_REPO_DEFAULTS);
    // No tree has been consulted yet, so the composition root settles the source later.
    assert_eq!(config.repo_config_source, RepoConfigSource::Global);
    assert_eq!(config.repo_config_source.as_str(), "global");
}

// ---------------------------------------------------------------------------------------------
// load_config: the machine home
// ---------------------------------------------------------------------------------------------

#[test]
fn sangkar_home_wins_over_the_xdg_root_and_names_the_derived_paths() {
    let bare = TempDir::new("home-explicit");
    let config = load(
        &[],
        &env_of(&[
            ("SANGKAR_HOME", "/machine/home"),
            ("XDG_CONFIG_HOME", "/xdg/config"),
            ("HOME", "/home/tester"),
        ]),
        bare.path(),
    );
    assert_eq!(config.home, Path::new("/machine/home"));
    assert_eq!(
        config.registry_path,
        Path::new("/machine/home/sangkar.json")
    );
    assert_eq!(config.config_path, Path::new("/machine/home/config.json"));
    assert_eq!(
        config.daemon.pid_path,
        Path::new("/machine/home/daemon.pid")
    );
    assert_eq!(
        config.daemon.log_path,
        Path::new("/machine/home/daemon.log")
    );
    assert_eq!(config.daemon.host, "127.0.0.1");
    assert_eq!(config.daemon.port, 7317);
}

#[test]
fn xdg_config_home_is_the_parent_of_the_sangkar_dir_when_sangkar_home_is_unset() {
    let bare = TempDir::new("home-xdg");
    let config = load(
        &[],
        &env_of(&[("XDG_CONFIG_HOME", "/xdg/config"), ("HOME", "/home/tester")]),
        bare.path(),
    );
    assert_eq!(config.home, Path::new("/xdg/config").join(HOME_DIR_NAME));
    assert_eq!(
        config.registry_path,
        Path::new("/xdg/config")
            .join(HOME_DIR_NAME)
            .join("sangkar.json")
    );
    assert_eq!(
        config.daemon.pid_path,
        Path::new("/xdg/config")
            .join(HOME_DIR_NAME)
            .join("daemon.pid")
    );
}

#[test]
fn both_unset_falls_back_to_dot_config_under_home() {
    let bare = TempDir::new("home-fallback");
    let config = load(&[], &env_of(&[("HOME", "/home/tester")]), bare.path());
    assert_eq!(
        config.home,
        Path::new("/home/tester")
            .join(".config")
            .join(HOME_DIR_NAME)
    );
    assert_eq!(
        config.daemon.log_path,
        Path::new("/home/tester")
            .join(".config")
            .join(HOME_DIR_NAME)
            .join("daemon.log")
    );
}

#[test]
fn an_empty_sangkar_home_counts_as_absent() {
    let bare = TempDir::new("home-empty");
    let with_xdg = load(
        &[],
        &env_of(&[("SANGKAR_HOME", ""), ("XDG_CONFIG_HOME", "/xdg/config")]),
        bare.path(),
    );
    assert_eq!(with_xdg.home, Path::new("/xdg/config").join(HOME_DIR_NAME));

    // A bare `SANGKAR_HOME=` in a shell profile must not put the registry in the working directory.
    let home_only = load(
        &[],
        &env_of(&[("SANGKAR_HOME", ""), ("HOME", "/home/tester")]),
        bare.path(),
    );
    assert_eq!(
        home_only.home,
        Path::new("/home/tester")
            .join(".config")
            .join(HOME_DIR_NAME)
    );
}

#[test]
fn an_empty_xdg_config_home_counts_as_absent_too() {
    let bare = TempDir::new("home-empty-xdg");
    let config = load(
        &[],
        &env_of(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/tester")]),
        bare.path(),
    );
    // `join("", "sangkar")` would be the relative "sangkar": a cwd-relative home, not a machine one.
    assert_eq!(
        config.home,
        Path::new("/home/tester")
            .join(".config")
            .join(HOME_DIR_NAME)
    );
}

#[test]
fn a_leading_tilde_expands_against_home_because_no_shell_expanded_it() {
    let bare = TempDir::new("home-tilde");
    let config = load(
        &[],
        &env_of(&[
            ("SANGKAR_HOME", "~/machine/sangkar"),
            ("HOME", "/home/tester"),
        ]),
        bare.path(),
    );
    assert_eq!(
        config.home,
        Path::new("/home/tester").join("machine").join("sangkar")
    );
}

#[test]
fn expand_home_joins_segments_so_a_doubled_slash_stays_under_the_home() {
    let home = Path::new("/home/tester");
    assert_eq!(
        expand_home("~/machine/sangkar", home),
        Some(home.join("machine").join("sangkar"))
    );
    // `~//b`: the tail starts with `/`, which must not replace the home the way `Path::join` would.
    assert_eq!(expand_home("~//b", home), Some(home.join("b")));
    assert_eq!(expand_home("~/", home), Some(home.to_path_buf()));
    assert_eq!(expand_home("/absolute", home), None);
}

#[test]
fn the_machine_home_is_machine_state_so_repo_flags_do_not_move_it() {
    let root = TempDir::new("home-machine");
    let cwd = TempDir::new("home-machine-cwd");
    let config = load(
        &[
            "--repo-root",
            &root.path().to_string_lossy(),
            "--data-dir",
            "var/graph",
        ],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        cwd.path(),
    );
    assert_eq!(config.data_dir, root.join("var").join("graph"));
    assert_eq!(config.home, Path::new("/machine/home"));
    assert_eq!(
        config.registry_path,
        Path::new("/machine/home/sangkar.json")
    );
}

#[test]
fn resolve_home_answers_the_same_paths_without_a_full_config() {
    assert_eq!(
        resolve_home(&env_lookup(&env_of(&[("SANGKAR_HOME", "/machine/home")]))),
        Path::new("/machine/home")
    );
    assert_eq!(
        resolve_home(&env_lookup(&env_of(&[("HOME", "/home/tester")]))),
        Path::new("/home/tester")
            .join(".config")
            .join(HOME_DIR_NAME)
    );
}

// ---------------------------------------------------------------------------------------------
// load_config: telemetry, cursor key, worker command
// ---------------------------------------------------------------------------------------------

#[test]
fn the_telemetry_switch_has_one_off_spelling() {
    let bare = TempDir::new("telemetry-spellings");
    let with = |name: &str, value: &str| -> HashMap<String, String> {
        env_of(&[("SANGKAR_HOME", "/machine/home"), (name, value)])
    };

    let unset = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        bare.path(),
    );
    assert!(
        unset.telemetry.enabled,
        "an unset switch defers to the default"
    );

    let empty = load(&[], &with("SANGKAR_TELEMETRY", ""), bare.path());
    assert!(empty.telemetry.enabled, "an empty switch means consent");

    let off = load(&[], &with("SANGKAR_TELEMETRY", "off"), bare.path());
    assert!(!off.telemetry.enabled);

    // One spelling only: a value that reads as a negation elsewhere is still consent.
    for value in ["OFF", "Off", "0", "false"] {
        let config = load(&[], &with("SANGKAR_TELEMETRY", value), bare.path());
        assert!(config.telemetry.enabled, "{value} is not the off spelling");
    }
}

#[test]
fn the_telemetry_dir_is_always_inside_the_data_dir() {
    let bare = TempDir::new("telemetry-dir");
    let cwd = TempDir::new("telemetry-dir-cwd");
    let autodetected = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        bare.path(),
    );
    assert_eq!(
        autodetected.telemetry.dir,
        autodetected.data_dir.join("telemetry")
    );

    let relocated = load(
        &[
            "--repo-root",
            &bare.path().to_string_lossy(),
            "--data-dir",
            "var/graph",
        ],
        &env_of(&[("SANGKAR_TELEMETRY", "off")]),
        cwd.path(),
    );
    assert_eq!(
        relocated.telemetry.dir,
        bare.join("var").join("graph").join("telemetry")
    );
}

#[test]
fn an_explicit_cursor_key_is_returned_verbatim() {
    let bare = TempDir::new("key-explicit");
    let key = "k".repeat(32);
    let config = load(&[], &env_of(&[("SANGKAR_CURSOR_KEY", &key)]), bare.path());
    assert_eq!(config.cursor_key, key);
}

#[test]
fn a_short_cursor_key_is_rejected_and_a_32_byte_one_survives() {
    let bare = TempDir::new("key-length");
    let short = "k".repeat(31);
    let refused = try_load(&[], &env_of(&[("SANGKAR_CURSOR_KEY", &short)]), bare.path())
        .expect_err("a 31-byte key is not key material");
    assert_eq!(refused.code, ErrorCode::INTERNAL);
    assert!(
        refused
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("SANGKAR_CURSOR_KEY"),
        "the refusal names the variable: {refused}"
    );

    let accepted = "k".repeat(32);
    let config = load(
        &[],
        &env_of(&[("SANGKAR_CURSOR_KEY", &accepted)]),
        bare.path(),
    );
    assert_eq!(config.cursor_key, accepted);
}

#[test]
fn an_empty_cursor_key_is_regenerated() {
    let bare = TempDir::new("key-empty");
    let config = load(&[], &env_of(&[("SANGKAR_CURSOR_KEY", "")]), bare.path());
    assert_ne!(config.cursor_key, "");
    assert_eq!(config.cursor_key.len(), 64);
}

#[test]
fn a_generated_cursor_key_is_32_random_bytes_hex() {
    let bare = TempDir::new("key-generated");
    let env = env_of(&[("SANGKAR_HOME", "/machine/home")]);

    let first = load(&[], &env, bare.path()).cursor_key;
    let second = load(&[], &env, bare.path()).cursor_key;

    assert_ne!(first, second, "the fallback is random rather than constant");
    for key in [&first, &second] {
        assert_eq!(key.len(), 64, "32 bytes are 64 hex characters");
        assert!(key.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }
}

#[test]
fn the_worker_command_defaults_to_this_executable() {
    let bare = TempDir::new("worker-default");
    let config = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        bare.path(),
    );
    let current = std::env::current_exe().expect("the test binary has a path");
    assert_eq!(config.worker_cmd, vec![current.into_os_string()]);
}

#[test]
fn the_worker_command_is_overridable_during_the_transition() {
    let bare = TempDir::new("worker-override");
    let config = load(
        &[],
        &env_of(&[
            ("SANGKAR_HOME", "/machine/home"),
            ("SANGKAR_WORKER_CMD", "bun bin/sangkar.ts"),
        ]),
        bare.path(),
    );
    assert_eq!(
        config.worker_cmd,
        vec![
            std::ffi::OsString::from("bun"),
            std::ffi::OsString::from("bin/sangkar.ts"),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// with_repo
// ---------------------------------------------------------------------------------------------

#[test]
fn with_repo_swaps_the_repo_fields_and_copies_every_machine_field_untouched() {
    let first = TempDir::new("withrepo-first");
    let second = TempDir::new("withrepo-second");
    let cwd = TempDir::new("withrepo-cwd");
    let key = "k".repeat(32);
    let base = load(
        &["--repo-root", &first.path().to_string_lossy()],
        &env_of(&[
            ("SANGKAR_HOME", "/machine/home"),
            ("SANGKAR_CURSOR_KEY", &key),
        ]),
        cwd.path(),
    );
    assert_eq!(base.repo_root, first.path());

    let rebound = with_repo(&base, second.path(), None);

    assert_eq!(rebound.repo_root, second.path());
    assert_eq!(rebound.data_dir, second.join(DATA_DIR_NAME));
    assert_eq!(rebound.telemetry.dir, rebound.data_dir.join("telemetry"));
    assert_eq!(rebound.home, base.home);
    assert_eq!(rebound.registry_path, base.registry_path);
    assert_eq!(rebound.config_path, base.config_path);
    assert_eq!(rebound.daemon, base.daemon);
    assert_eq!(rebound.cursor_key, base.cursor_key);
    assert_eq!(rebound.budgets, base.budgets);
    assert_eq!(rebound.telemetry.enabled, base.telemetry.enabled);
    assert_eq!(rebound.worker_cmd, base.worker_cmd);
    assert_eq!(rebound.repo_defaults, base.repo_defaults);
    // The argument config keeps the first tree, so one binding cannot silently move another.
    assert_eq!(base.repo_root, first.path());
    assert_eq!(base.data_dir, first.join(DATA_DIR_NAME));
    assert_eq!(
        base.telemetry.dir,
        first.join(DATA_DIR_NAME).join("telemetry")
    );
}

#[test]
fn with_repo_resolves_the_root_it_is_given_and_honours_an_explicit_data_dir() {
    let parent = TempDir::new("withrepo-parent");
    let base = load(
        &[],
        &env_of(&[("SANGKAR_HOME", "/machine/home")]),
        parent.path(),
    );

    // `resolve` is the process cwd's business, not the previous root's: the caller hands over a path.
    let relative = with_repo(&base, Path::new("child"), None);
    assert!(relative.repo_root.is_absolute());
    assert_eq!(relative.repo_root.file_name(), Some(OsStr::new("child")));

    let explicit = with_repo(
        &base,
        parent.path(),
        Some(&parent.join("var").join("graph")),
    );
    assert_eq!(explicit.repo_root, parent.path());
    assert_eq!(explicit.data_dir, parent.join("var").join("graph"));
    assert_eq!(
        explicit.telemetry.dir,
        parent.join("var").join("graph").join("telemetry")
    );
    // Consent is a machine-level switch, so the rebound config carries it, not re-derives it.
    assert!(explicit.telemetry.enabled);
}

#[test]
fn with_repo_carries_a_disabled_consent_switch_across_the_rebind() {
    let bare = TempDir::new("withrepo-off");
    let other = TempDir::new("withrepo-off-other");
    let base = load(
        &[],
        &env_of(&[
            ("SANGKAR_HOME", "/machine/home"),
            ("SANGKAR_TELEMETRY", "off"),
        ]),
        bare.path(),
    );

    let rebound = with_repo(&base, other.path(), None);

    assert!(!rebound.telemetry.enabled);
    assert_eq!(
        rebound.telemetry.dir,
        other.join(DATA_DIR_NAME).join("telemetry")
    );
}

// ---------------------------------------------------------------------------------------------
// canonical_root
// ---------------------------------------------------------------------------------------------

#[test]
fn canonical_root_expands_a_leading_tilde_against_the_injected_home() {
    let home = TempDir::new("canonical-home");
    assert_eq!(
        canonical_root(Path::new("~/Documents/a"), Some(home.path())),
        home.join("Documents").join("a")
    );
    assert_eq!(
        canonical_root(Path::new("~//Documents/a"), Some(home.path())),
        home.join("Documents").join("a")
    );
}

#[test]
fn canonical_root_realpaths_an_existing_path_and_keeps_a_missing_one_resolved() {
    let dir = TempDir::new("canonical-existing");
    let real = fs::canonicalize(dir.path()).expect("the fixture is real");
    assert_eq!(canonical_root(dir.path(), None), real);
    assert!(
        !canonical_root(dir.path(), None)
            .to_string_lossy()
            .ends_with('/')
    );

    let missing = dir.join("deleted");
    assert_eq!(canonical_root(&missing, None), real.join("deleted"));
}

#[test]
fn canonical_root_resolves_a_relative_form_instead_of_returning_it() {
    let resolved = canonical_root(Path::new("./a/../b"), None);
    assert!(resolved.is_absolute());
    assert_eq!(resolved.file_name(), Some(OsStr::new("b")));
    assert!(
        !resolved
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir)),
        "{} is not normalised",
        resolved.display()
    );
}

// ---------------------------------------------------------------------------------------------
// read_registry
// ---------------------------------------------------------------------------------------------

#[test]
fn a_missing_registry_is_empty_with_exactly_one_warning() {
    let dir = TempDir::new("registry-absent");
    let warnings = Warnings::new();
    let sink = warnings.sink();

    let registry = read_registry_with(&dir.join("absent.json"), &sink);

    assert_eq!(registry, EMPTY_REGISTRY);
    assert_eq!(warnings.count(), 1);
    let warning = warnings.first();
    assert_eq!(warning.message, "registry ignored, using an empty registry");
    assert!(
        warning.field("reason").contains("unreadable"),
        "{warning:?}"
    );
}

#[test]
fn a_version_other_than_one_is_empty_with_exactly_one_warning() {
    let dir = TempDir::new("registry-version");
    let path = dir.file("sangkar.json", r#"{"version":2,"repos":["/srv/a"]}"#);
    let warnings = Warnings::new();
    let sink = warnings.sink();

    let registry = read_registry_with(&path, &sink);

    assert_eq!(registry, EMPTY_REGISTRY);
    assert_eq!(warnings.count(), 1);
    let warning = warnings.first();
    assert!(warning.field("reason").contains("version"), "{warning:?}");
}

#[test]
fn invalid_json_a_non_object_and_a_non_array_repos_each_warn_once() {
    let bodies = [
        "{not json",
        "[]",
        "\"a string\"",
        "null",
        r#"{"repos":["/srv/a"]}"#,
        r#"{"version":1,"repos":"/srv/a"}"#,
    ];
    for body in bodies {
        let dir = TempDir::new("registry-invalid");
        let path = dir.file("sangkar.json", body);
        let warnings = Warnings::new();
        let sink = warnings.sink();
        assert_eq!(
            read_registry_with(&path, &sink),
            EMPTY_REGISTRY,
            "body {body:?} is refused"
        );
        assert_eq!(warnings.count(), 1, "one warning for {body:?}");
    }
}

#[test]
fn an_unreadable_registry_path_is_empty_with_exactly_one_warning() {
    let dir = TempDir::new("registry-unreadable");
    // A directory where the file should be: the read fails without the shape being wrong.
    let path = dir.dir("sangkar.json");
    let warnings = Warnings::new();
    let sink = warnings.sink();

    assert_eq!(read_registry_with(&path, &sink), EMPTY_REGISTRY);
    assert_eq!(warnings.count(), 1);
}

#[test]
fn a_non_string_entry_is_dropped_with_exactly_one_warning() {
    let dir = TempDir::new("registry-nonstring");
    let kept = TempDir::new("registry-nonstring-kept");
    let body = format!(
        r#"{{"version":1,"repos":["{}",7,null,true]}}"#,
        kept.path().to_string_lossy()
    );
    let path = dir.file("sangkar.json", &body);
    let warnings = Warnings::new();
    let sink = warnings.sink();

    let registry = read_registry_with(&path, &sink);

    assert_eq!(registry.version, REGISTRY_VERSION);
    assert_eq!(registry.repos, vec![canonical_root(kept.path(), None)]);
    assert_eq!(warnings.count(), 1);
    assert_eq!(warnings.first().field("dropped"), "3");
}

#[test]
fn duplicates_collapse_and_the_first_occurrence_keeps_its_position() {
    let dir = TempDir::new("registry-duplicates");
    let first = TempDir::new("registry-duplicates-first");
    let second = TempDir::new("registry-duplicates-second");
    let body = format!(
        r#"{{"version":1,"repos":["{}","{}","{}"]}}"#,
        first.path().to_string_lossy(),
        second.path().to_string_lossy(),
        first.path().to_string_lossy()
    );
    let path = dir.file("sangkar.json", &body);

    let registry = read_registry(&path);

    assert_eq!(
        registry.repos,
        vec![
            canonical_root(first.path(), None),
            canonical_root(second.path(), None),
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// write_registry
// ---------------------------------------------------------------------------------------------

#[test]
fn write_registry_creates_the_parents_and_writes_the_file_shape_verbatim() {
    let dir = TempDir::new("registry-write");
    let path = dir.join("nested/deeper/sangkar.json");
    let registry = Registry {
        version: REGISTRY_VERSION,
        repos: vec![PathBuf::from("/srv/b"), PathBuf::from("~/Documents/a")],
    };

    write_registry(&path, &registry).expect("the registry is written");

    assert_eq!(
        fs::read_to_string(&path).expect("the file exists"),
        r#"{"version":1,"repos":["/srv/b","~/Documents/a"]}"#
    );
    let leftovers: Vec<String> = fs::read_dir(path.parent().expect("a parent"))
        .expect("the directory is readable")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp file is left behind: {leftovers:?}"
    );
}

#[test]
fn write_registry_always_stamps_the_current_version() {
    let dir = TempDir::new("registry-stamp");
    let path = dir.join("sangkar.json");
    let registry = Registry {
        version: 99,
        repos: vec![PathBuf::from("/srv/a")],
    };

    write_registry(&path, &registry).expect("the registry is written");

    assert_eq!(
        fs::read_to_string(&path).expect("the file exists"),
        r#"{"version":1,"repos":["/srv/a"]}"#
    );
}

// ---------------------------------------------------------------------------------------------
// register_repo / remove_repo
// ---------------------------------------------------------------------------------------------

#[test]
fn register_repo_registers_once_and_reports_added_false_the_second_time() {
    let dir = TempDir::new("register");
    let path = dir.join("sangkar.json");
    let repo = TempDir::new("register-repo");
    let warnings = Warnings::new();
    let sink = warnings.sink();

    let first = register_repo_with(&path, repo.path(), &sink);
    assert!(first.added);
    assert_eq!(
        first.registry.repos,
        vec![canonical_root(repo.path(), None)]
    );

    let second = register_repo_with(&path, repo.path(), &sink);
    assert!(!second.added);
    assert_eq!(second.registry.repos.len(), 1);
    assert_eq!(
        read_registry(&path).repos,
        vec![canonical_root(repo.path(), None)]
    );
}

#[test]
fn register_repo_realpaths_a_path_that_exists_and_keeps_a_missing_one_resolved() {
    let dir = TempDir::new("register-missing");
    let path = dir.join("sangkar.json");
    let existing = TempDir::new("register-existing");
    let missing_root = TempDir::new("register-missing-repo");
    let missing = missing_root.join("deleted");
    let warnings = Warnings::new();
    let sink = warnings.sink();

    assert!(register_repo_with(&path, existing.path(), &sink).added);
    assert!(register_repo_with(&path, &missing, &sink).added);

    // The existing tree is realpathed; the missing one keeps the absolute form it was given.
    assert_eq!(
        read_registry(&path).repos,
        vec![canonical_root(existing.path(), None), missing]
    );
}

#[test]
fn a_failed_registry_write_is_swallowed_into_a_warning() {
    let dir = TempDir::new("register-blocked");
    // A file where the registry's parent directory should be: both the read and the write fail.
    let blocker = dir.file("not-a-dir", "x");
    let path = blocker.join("sangkar.json");
    let warnings = Warnings::new();
    let sink = warnings.sink();

    let result = register_repo_with(&path, TempDir::new("register-blocked-repo").path(), &sink);

    assert!(!result.added);
    assert!(result.registry.repos.is_empty());
    // The unreadable registry is one warning, the failed write the second.
    assert_eq!(warnings.count(), 2);
}

#[test]
fn remove_repo_removes_a_root_once_then_reports_removed_false() {
    let dir = TempDir::new("remove");
    let path = dir.join("sangkar.json");
    let kept = TempDir::new("remove-kept");
    let dropped = TempDir::new("remove-dropped");
    let warnings = Warnings::new();
    let sink = warnings.sink();
    let _ = register_repo_with(&path, kept.path(), &sink);
    let _ = register_repo_with(&path, dropped.path(), &sink);

    let first = remove_repo_with(&path, dropped.path(), &sink);
    assert!(first.removed);
    assert_eq!(
        first.registry.repos,
        vec![canonical_root(kept.path(), None)]
    );

    let second = remove_repo_with(&path, dropped.path(), &sink);
    assert!(!second.removed);
    assert_eq!(
        read_registry(&path).repos,
        vec![canonical_root(kept.path(), None)]
    );
}

// ---------------------------------------------------------------------------------------------
// select_repo: the four-step order and its two refusals
// ---------------------------------------------------------------------------------------------

#[test]
fn an_explicit_request_outranks_the_pin() {
    let a = TempDir::new("select-request-pin-a");
    let b = TempDir::new("select-request-pin-b");
    let registry = registry_of(&[a.path()]);
    let cwd = TempDir::new("select-request-pin-cwd");

    let requested = a.path().to_string_lossy().into_owned();
    let input = SelectRepoInput::new(&registry, cwd.path())
        .requested(&requested)
        .pinned_root(b.path());

    assert_eq!(
        select_repo(&input).expect("the request wins"),
        canonical_root(a.path(), None)
    );
}

#[test]
fn the_canonical_path_wins_over_a_basename_shared_by_two_entries() {
    let dir = TempDir::new("select-dup-parent");
    let one = nested(dir.path(), &["one", "dup"]);
    let two = nested(dir.path(), &["two", "dup"]);
    let registry = registry_of(&[&one, &two]);
    let cwd = TempDir::new("select-dup-cwd");

    let requested = two.to_string_lossy().into_owned();
    let input = SelectRepoInput::new(&registry, cwd.path()).requested(&requested);

    assert_eq!(
        select_repo(&input).expect("the canonical path matches"),
        canonical_root(&two, None)
    );
}

#[test]
fn an_unregistered_request_is_not_found_even_while_a_pin_is_set() {
    let registered = TempDir::new("select-unregistered");
    let registry = registry_of(&[registered.path()]);
    let cwd = TempDir::new("select-unregistered-cwd");
    let pin = TempDir::new("select-unregistered-pin");

    let input = SelectRepoInput::new(&registry, cwd.path())
        .requested("no-such-repo")
        .pinned_root(pin.path());

    let error = select_repo(&input).expect_err("an unregistered request refuses");
    assert_eq!(error.code, ErrorCode::NOT_FOUND);
    assert_eq!(
        hint_lines(&error),
        vec![
            "Registered repos:".to_owned(),
            canonical_root(registered.path(), None)
                .to_string_lossy()
                .into_owned(),
        ]
    );
}

#[test]
fn a_basename_selects_its_single_registered_repo() {
    let dir = TempDir::new("select-basename");
    let repo = dir.dir("named");
    let registry = registry_of(&[&repo]);
    let cwd = TempDir::new("select-basename-cwd");

    let input = SelectRepoInput::new(&registry, cwd.path()).requested("named");

    assert_eq!(
        select_repo(&input).expect("the basename matches"),
        canonical_root(&repo, None)
    );
}

#[test]
fn a_basename_shared_by_two_entries_is_ambiguous_and_names_both() {
    let dir = TempDir::new("select-ambiguous-parent");
    let one = nested(dir.path(), &["one", "dup"]);
    let two = nested(dir.path(), &["two", "dup"]);
    let registry = registry_of(&[&one, &two]);
    let cwd = TempDir::new("select-ambiguous-cwd");

    let input = SelectRepoInput::new(&registry, cwd.path()).requested("dup");

    let error = select_repo(&input).expect_err("two candidates are ambiguous");
    assert_eq!(error.code, ErrorCode::AMBIGUOUS);
    assert!(
        error.message.contains("2 registered repos"),
        "{}",
        error.message
    );
    assert_eq!(
        hint_lines(&error),
        vec![
            "Registered repos:".to_owned(),
            canonical_root(&one, None).to_string_lossy().into_owned(),
            canonical_root(&two, None).to_string_lossy().into_owned(),
        ]
    );
}

#[test]
fn the_pin_answers_with_an_empty_and_with_a_multi_entry_registry() {
    let pin = TempDir::new("select-pin");
    let cwd = TempDir::new("select-pin-cwd");
    let empty_registry = EMPTY_REGISTRY;
    let empty = SelectRepoInput::new(&empty_registry, cwd.path()).pinned_root(pin.path());
    assert_eq!(
        select_repo(&empty).expect("the pin needs no registry"),
        canonical_root(pin.path(), None)
    );

    let a = TempDir::new("select-pin-a");
    let b = TempDir::new("select-pin-b");
    let multi = registry_of(&[a.path(), b.path()]);
    let pinned = SelectRepoInput::new(&multi, cwd.path()).pinned_root(pin.path());
    assert_eq!(
        select_repo(&pinned).expect("the pin outranks the registry"),
        canonical_root(pin.path(), None)
    );
}

#[test]
fn an_empty_pin_is_not_a_pin() {
    let only = TempDir::new("select-empty-pin");
    let registry = registry_of(&[only.path()]);
    let cwd = TempDir::new("select-empty-pin-cwd");

    let input = SelectRepoInput::new(&registry, cwd.path()).pinned_root(Path::new(""));

    assert_eq!(
        select_repo(&input).expect("the only repo answers"),
        canonical_root(only.path(), None)
    );
}

#[test]
fn a_request_outranks_the_cwd_match() {
    let a = TempDir::new("select-request-cwd-a");
    let b = TempDir::new("select-request-cwd-b");
    let registry = registry_of(&[a.path(), b.path()]);

    let requested = a.path().to_string_lossy().into_owned();
    let input = SelectRepoInput::new(&registry, b.path()).requested(&requested);

    assert_eq!(
        select_repo(&input).expect("the request wins"),
        canonical_root(a.path(), None)
    );
}

#[test]
fn a_cwd_inside_the_registry_wins_over_the_single_repo_default() {
    let a = TempDir::new("select-cwd-a");
    let b = TempDir::new("select-cwd-b");
    let registry = registry_of(&[a.path(), b.path()]);

    let input = SelectRepoInput::new(&registry, b.path());

    assert_eq!(
        select_repo(&input).expect("the cwd is registered"),
        canonical_root(b.path(), None)
    );
}

#[test]
fn exactly_one_registered_repo_answers_a_cwd_that_matches_nothing() {
    let only = TempDir::new("select-only");
    let registry = registry_of(&[only.path()]);
    let cwd = TempDir::new("select-only-cwd");

    assert_eq!(
        select_repo(&SelectRepoInput::new(&registry, cwd.path())).expect("the only repo answers"),
        canonical_root(only.path(), None)
    );
}

#[test]
fn an_empty_registry_refuses_with_not_found_and_the_add_hint() {
    let cwd = TempDir::new("select-empty-cwd");

    let error = select_repo(&SelectRepoInput::new(&EMPTY_REGISTRY, cwd.path()))
        .expect_err("nothing is registered");

    assert_eq!(error.code, ErrorCode::NOT_FOUND);
    assert_eq!(error.message, "no repository is registered");
    assert_eq!(error.hint.as_deref(), Some("sangkar repo add <path>"));
}

#[test]
fn several_repos_with_no_cwd_match_refuse_with_ambiguous_and_list_them_all() {
    let a = TempDir::new("select-several-a");
    let b = TempDir::new("select-several-b");
    let registry = registry_of(&[a.path(), b.path()]);
    let cwd = TempDir::new("select-several-cwd");

    let error =
        select_repo(&SelectRepoInput::new(&registry, cwd.path())).expect_err("two candidates");

    assert_eq!(error.code, ErrorCode::AMBIGUOUS);
    assert_eq!(
        hint_lines(&error),
        vec![
            "Registered repos:".to_owned(),
            canonical_root(a.path(), None)
                .to_string_lossy()
                .into_owned(),
            canonical_root(b.path(), None)
                .to_string_lossy()
                .into_owned(),
        ]
    );
}
