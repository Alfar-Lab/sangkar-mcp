//! The three small files that hold installation settings, one tree's overrides, and the
//! single-daemon guarantee.
//!
//! Every expected answer here was taken from the TypeScript oracle (`tests/global-config.test.ts`,
//! `tests/repo-config.test.ts` and `tests/pidlock.test.ts` under Bun 1.4.4) and ported rather than
//! re-derived. Each case is a real file in a `mkdtemp` tree of its own, and a warning is asserted by
//! counting what the injected sink received: `log()` writes to stderr, so a collector is the only way
//! to say "exactly one warning" without scraping a child process, and the injected sink is the shape
//! `registry.rs` already established.
//!
//! The two on-disk shapes are pinned byte for byte — key order, indentation and trailing newline —
//! because both trees write the same `config.json` and a reader on either side parses it.
//!
//! Nothing here reads the developer's own home: every path comes from a temp dir this file creates,
//! and every environment read goes through the map injected into `load_config`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sangkar_core::config::{Budgets, DATA_DIR_NAME, RepoConfigSource, RepoDefaults, load_config};
use sangkar_core::global_config::{
    DEFAULT_REPORT_HOST, DEFAULT_REPORT_PORT, GlobalBudgets, GlobalConfig, GlobalDaemon,
    default_global_settings, ensure_global_config, ensure_global_config_with, global_config_path,
    read_global_config, read_global_config_with, repo_defaults_of,
};
use sangkar_core::pidlock::{
    PID_STALE_AFTER_MS, acquire_pid_lock, acquire_pid_lock_with, heartbeat_pid_lock,
    heartbeat_pid_lock_with, read_pid_lock, release_pid_lock, release_pid_lock_with,
};
use sangkar_core::repo_config::{
    DEFAULT_REPO_SETTINGS, RepoConfig, WATCH_INTERVAL_DEFAULT_MS, WATCH_INTERVAL_MAX_MS,
    WATCH_INTERVAL_MIN_MS, read_repo_config, read_repo_config_with, recording_enabled,
    repo_config_path, write_repo_config,
};
use serde_json::Value;

/// The exact bytes `ensure_global_config` writes: `JSON.stringify(shape, null, 2)` plus one newline,
/// as `global-config.ts` produced them. Key order, two-space indent and the trailing newline are the
/// contract between the two binaries, so this is a literal rather than a re-serialisation.
const GLOBAL_CONFIG_BYTES: &str = "{\n  \"watch\": true,\n  \"watch_interval\": 2000,\n  \"telemetry\": true,\n  \"daemon\": {\n    \"host\": \"127.0.0.1\",\n    \"port\": 7317\n  },\n  \"budgets\": {\n    \"default_tokens\": 1200,\n    \"max_tokens\": 32000\n  }\n}\n";

/// `writeRepoConfig`'s bytes: `JSON.stringify(shape)` with no indentation and no trailing newline.
const REPO_CONFIG_BYTES: &str = r#"{"watch":true,"watch_interval":2000,"telemetry":true}"#;

/// A pid that is certainly free: `0x7ffffffe` is far above any Linux `pid_max`, which caps at
/// `4194304` with the largest setting this kernel accepts.
const DEAD_PID: u32 = 0x7fff_fffe;

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
            "sangkar-settings-{label}-{}-{nanos}-{}",
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

/// One warning the injected sink received.
#[derive(Clone, Debug)]
struct Recorded {
    msg: String,
    fields: Vec<(String, Value)>,
}

impl Recorded {
    /// One recorded field, as the string the assertion compares.
    fn field(&self, name: &str) -> Option<String> {
        self.fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
    }
}

/// A warning sink that records instead of writing: the warning is part of the contract.
#[derive(Default)]
struct Recorder {
    lines: RefCell<Vec<Recorded>>,
}

impl Recorder {
    fn new() -> Self {
        Self::default()
    }

    fn sink(&self) -> impl Fn(&str, &[(&str, Value)]) + '_ {
        move |msg: &str, fields: &[(&str, Value)]| {
            self.lines.borrow_mut().push(Recorded {
                msg: msg.to_owned(),
                fields: fields
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), value.clone()))
                    .collect(),
            });
        }
    }

    fn take(&self) -> Vec<Recorded> {
        self.lines.borrow().clone()
    }
}

/// The `field` of every recorded warning, in the order they arrived.
fn fields_of(lines: &[Recorded]) -> Vec<String> {
    lines
        .iter()
        .map(|line| line.field("field").unwrap_or_default())
        .collect()
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

/// A home holding a `config.json` with exactly `body`.
fn home_with_config(label: &str, body: &str) -> TempDir {
    let home = TempDir::new(label);
    fs::write(global_config_path(home.path()), body).expect("write the fixture config");
    home
}

/// The warnings `read_global_config` reports for `home`.
fn global_with_warnings(home: &Path) -> (GlobalConfig, Vec<Recorded>) {
    let recorder = Recorder::new();
    let config = read_global_config_with(home, &recorder.sink());
    (config, recorder.take())
}

// ---------------------------------------------------------------------------------------------
// global config: defaults, path and ensure
// ---------------------------------------------------------------------------------------------

#[test]
fn the_built_in_defaults_are_the_documented_values() {
    assert_eq!(
        default_global_settings(),
        GlobalConfig {
            watch: true,
            watch_interval_ms: 2_000,
            telemetry: true,
            daemon: GlobalDaemon {
                host: "127.0.0.1".to_owned(),
                port: 7_317,
            },
            budgets: GlobalBudgets {
                default_tokens: 1_200,
                max_tokens: 32_000,
            },
        }
    );
}

#[test]
fn the_daemon_defaults_cannot_drift_from_the_report_servers() {
    assert_eq!(DEFAULT_REPORT_HOST, "127.0.0.1");
    assert_eq!(DEFAULT_REPORT_PORT, 7_317);
    assert_eq!(default_global_settings().daemon.host, DEFAULT_REPORT_HOST);
    assert_eq!(default_global_settings().daemon.port, DEFAULT_REPORT_PORT);
}

#[test]
fn the_file_is_config_json_directly_in_the_machine_home() {
    assert_eq!(
        global_config_path(Path::new("/machine/home")),
        Path::new("/machine/home/config.json")
    );
}

#[test]
fn repo_defaults_of_takes_exactly_the_three_keys_a_repository_inherits() {
    assert_eq!(
        repo_defaults_of(&default_global_settings()),
        RepoDefaults {
            watch: true,
            watch_interval_ms: 2_000,
            telemetry: true,
        }
    );
    assert_eq!(
        repo_defaults_of(&default_global_settings()),
        DEFAULT_REPO_SETTINGS
    );
}

#[test]
fn ensure_writes_the_defaults_newline_terminated_in_the_spelling_it_reads() {
    let home = TempDir::new("global-create");
    assert!(ensure_global_config(home.path()));

    let path = global_config_path(home.path());
    let text = fs::read_to_string(&path).expect("read the created config");
    // The exact bytes the TypeScript wrote, key order and indentation included.
    assert_eq!(text, GLOBAL_CONFIG_BYTES);
    assert!(text.ends_with('\n'));

    // The temp name carried the pid and was renamed away: the home holds the file and nothing else.
    let leftovers: Vec<String> = fs::read_dir(home.path())
        .expect("list the home")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(leftovers, vec!["config.json".to_owned()]);

    // What was written is what a read answers, so the file cannot describe itself differently.
    assert_eq!(read_global_config(home.path()), default_global_settings());

    // A hand edit of one written key is honoured, which is the same claim from the other side.
    fs::write(&path, text.replace("2000", "5000")).expect("hand edit the config");
    assert_eq!(read_global_config(home.path()).watch_interval_ms, 5_000);
}

#[test]
fn a_second_ensure_does_not_rewrite_the_file() {
    let home = TempDir::new("global-second");
    assert!(ensure_global_config(home.path()));
    let path = global_config_path(home.path());
    let before = fs::metadata(&path).expect("stat the created config");

    // `false` is "it exists", and the file is untouched: a rewrite through the temp-and-rename
    // publish would replace the inode even when the bytes came out identical.
    assert!(!ensure_global_config(home.path()));
    let after = fs::metadata(&path).expect("stat the kept config");
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.modified().ok(), before.modified().ok());
    assert_eq!(
        fs::read_to_string(&path).expect("read the kept config"),
        GLOBAL_CONFIG_BYTES
    );
}

#[test]
fn an_existing_hand_edited_file_is_left_alone() {
    let body = "{\"telemetry\": false}\n";
    let home = home_with_config("global-keep", body);
    assert!(!ensure_global_config(home.path()));
    assert_eq!(
        fs::read_to_string(global_config_path(home.path())).expect("read the kept config"),
        body
    );
}

#[test]
fn an_unwritable_home_is_one_warning_and_no_throw() {
    let parent = TempDir::new("global-unwritable");
    // A regular file where the home directory should be: `create_dir_all` fails deterministically.
    let home = parent.file("not-a-directory", "");
    let recorder = Recorder::new();

    assert!(!ensure_global_config_with(&home, &recorder.sink()));

    let lines = recorder.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].msg.contains("global config not created"));
    assert_eq!(
        lines[0].field("path").as_deref(),
        Some(global_config_path(&home).to_string_lossy().as_ref())
    );
}

// ---------------------------------------------------------------------------------------------
// global config: an unusable file
// ---------------------------------------------------------------------------------------------

#[test]
fn no_file_means_the_defaults_silently() {
    let home = TempDir::new("global-missing");
    let (config, lines) = global_with_warnings(home.path());
    assert_eq!(config, default_global_settings());
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn unparseable_json_means_the_defaults_and_one_warning_naming_the_path() {
    let home = home_with_config("global-broken", "{\"watch\": tru");
    let (config, lines) = global_with_warnings(home.path());
    assert_eq!(config, default_global_settings());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0].field("path").as_deref(),
        Some(global_config_path(home.path()).to_string_lossy().as_ref())
    );
    assert!(
        lines[0]
            .field("reason")
            .is_some_and(|reason| reason.contains("invalid JSON")),
        "{lines:?}"
    );
}

#[test]
fn a_json_value_that_is_not_an_object_means_the_defaults_and_one_warning() {
    for body in ["[]", "null", "\"config\"", "42"] {
        let home = home_with_config("global-nonobject", body);
        let (config, lines) = global_with_warnings(home.path());
        assert_eq!(config, default_global_settings(), "body {body}");
        assert_eq!(lines.len(), 1, "body {body}: {lines:?}");
        assert_eq!(
            lines[0].field("reason").as_deref(),
            Some("not a JSON object")
        );
    }
}

// ---------------------------------------------------------------------------------------------
// global config: field by field
// ---------------------------------------------------------------------------------------------

#[test]
fn a_complete_file_is_honoured_as_written() {
    let home = home_with_config(
        "global-full",
        r#"{"watch":false,"watch_interval":30000,"telemetry":false,"daemon":{"host":"127.0.0.1","port":0},"budgets":{"default_tokens":900,"max_tokens":4000}}"#,
    );
    let (config, lines) = global_with_warnings(home.path());
    assert_eq!(
        config,
        GlobalConfig {
            watch: false,
            watch_interval_ms: 30_000,
            telemetry: false,
            daemon: GlobalDaemon {
                host: "127.0.0.1".to_owned(),
                port: 0,
            },
            budgets: GlobalBudgets {
                default_tokens: 900,
                max_tokens: 4_000,
            },
        }
    );
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn keys_the_operator_left_out_are_silent_defaults() {
    let home = home_with_config("global-partial", "{\"watch\": false}\n");
    let (config, lines) = global_with_warnings(home.path());
    assert_eq!(
        config,
        GlobalConfig {
            watch: false,
            ..default_global_settings()
        }
    );
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn unknown_keys_are_ignored_without_a_warning() {
    let home = home_with_config(
        "global-unknown",
        "{\"colour\": \"blue\", \"watch\": false}\n",
    );
    let (config, lines) = global_with_warnings(home.path());
    assert!(!config.watch);
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn a_switch_that_is_not_a_json_boolean_takes_the_default_with_one_warning_each() {
    let home = home_with_config(
        "global-switches",
        "{\"watch\": \"yes\", \"telemetry\": 1}\n",
    );
    let (config, lines) = global_with_warnings(home.path());
    assert!(config.watch);
    assert!(config.telemetry);
    assert_eq!(fields_of(&lines), ["watch", "telemetry"]);
    let path = global_config_path(home.path())
        .to_string_lossy()
        .into_owned();
    assert!(
        lines
            .iter()
            .all(|line| line.field("path").as_deref() == Some(path.as_str())),
        "{lines:?}"
    );
}

#[test]
fn the_interval_is_clamped_into_the_supported_band() {
    let low = home_with_config("global-low", "{\"watch_interval\": 500}\n");
    assert_eq!(global_with_warnings(low.path()).0.watch_interval_ms, 1_000);

    let high = home_with_config("global-high", "{\"watch_interval\": 900000}\n");
    assert_eq!(
        global_with_warnings(high.path()).0.watch_interval_ms,
        300_000
    );

    // A wrong type is not a slow poll but nonsense, and takes the default silently.
    let nonsense = home_with_config("global-nonsense", "{\"watch_interval\": \"fast\"}\n");
    let (config, lines) = global_with_warnings(nonsense.path());
    assert_eq!(config.watch_interval_ms, 2_000);
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn daemon_and_budgets_must_be_json_objects_and_a_wrong_shape_takes_the_whole_subtree() {
    let home = home_with_config(
        "global-shapes",
        "{\"daemon\": \"localhost:9000\", \"budgets\": 5}\n",
    );
    let (config, lines) = global_with_warnings(home.path());
    assert_eq!(config.daemon, default_global_settings().daemon);
    assert_eq!(config.budgets, default_global_settings().budgets);
    assert_eq!(fields_of(&lines), ["daemon", "budgets"]);
}

#[test]
fn a_port_is_an_integer_in_0_65535_and_0_is_a_real_answer() {
    let bad = home_with_config("global-port-bad", "{\"daemon\": {\"port\": 70000}}\n");
    let (config, lines) = global_with_warnings(bad.path());
    assert_eq!(config.daemon.port, 7_317);
    assert_eq!(fields_of(&lines), ["daemon.port"]);

    let fraction = home_with_config("global-port-float", "{\"daemon\": {\"port\": 80.5}}\n");
    let (config, lines) = global_with_warnings(fraction.path());
    assert_eq!(config.daemon.port, 7_317);
    assert_eq!(fields_of(&lines), ["daemon.port"]);
}

#[test]
fn the_host_is_any_non_empty_string_and_the_loopback_policy_belongs_to_the_server() {
    let empty = home_with_config("global-host-empty", "{\"daemon\": {\"host\": \"  \"}}\n");
    let (config, lines) = global_with_warnings(empty.path());
    assert_eq!(config.daemon.host, "127.0.0.1");
    assert_eq!(fields_of(&lines), ["daemon.host"]);

    let typed = home_with_config("global-host-type", "{\"daemon\": {\"host\": 0}}\n");
    assert_eq!(
        global_with_warnings(typed.path()).0.daemon.host,
        "127.0.0.1"
    );

    let localhost = home_with_config(
        "global-host-ok",
        "{\"daemon\": {\"host\": \"localhost\"}}\n",
    );
    let (config, lines) = global_with_warnings(localhost.path());
    assert_eq!(config.daemon.host, "localhost");
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn budgets_are_positive_integers_and_the_fallback_is_clamped_to_the_ceiling() {
    let bad = home_with_config(
        "global-budgets-bad",
        "{\"budgets\": {\"default_tokens\": 0, \"max_tokens\": \"lots\"}}\n",
    );
    let (config, lines) = global_with_warnings(bad.path());
    assert_eq!(
        config.budgets,
        GlobalBudgets {
            default_tokens: 1_200,
            max_tokens: 32_000,
        }
    );
    assert_eq!(
        fields_of(&lines),
        ["budgets.max_tokens", "budgets.default_tokens"]
    );

    // A per-call fallback above the ceiling is unsatisfiable, so it is clamped rather than honoured.
    let above = home_with_config(
        "global-budgets-clamp",
        "{\"budgets\": {\"default_tokens\": 9000, \"max_tokens\": 5000}}\n",
    );
    let (config, lines) = global_with_warnings(above.path());
    assert_eq!(
        config.budgets,
        GlobalBudgets {
            default_tokens: 5_000,
            max_tokens: 5_000,
        }
    );
    assert!(lines.is_empty(), "{lines:?}");

    // The hard ceiling in `budget.rs` cannot be raised from the file; the file only lowers it.
    let raised = home_with_config(
        "global-budgets-hard",
        "{\"budgets\": {\"max_tokens\": 100000}}\n",
    );
    assert_eq!(
        global_with_warnings(raised.path()).0.budgets.max_tokens,
        32_000
    );
}

// ---------------------------------------------------------------------------------------------
// repo config: defaults, path and an unusable file
// ---------------------------------------------------------------------------------------------

#[test]
fn the_exported_repo_defaults_are_the_values_a_missing_file_yields() {
    assert_eq!(
        DEFAULT_REPO_SETTINGS,
        RepoDefaults {
            watch: true,
            watch_interval_ms: 2_000,
            telemetry: true,
        }
    );
    assert_eq!(WATCH_INTERVAL_DEFAULT_MS, 2_000);
    assert_eq!(WATCH_INTERVAL_MIN_MS, 1_000);
    assert_eq!(WATCH_INTERVAL_MAX_MS, 300_000);
}

#[test]
fn repo_config_path_is_config_json_inside_the_data_dir() {
    assert_eq!(
        repo_config_path(Path::new("/tmp/repo/.sangkar")),
        Path::new("/tmp/repo/.sangkar/config.json")
    );
}

/// A repo with a data dir and a `config.json` holding exactly `body`.
fn repo_with_config(label: &str, body: &str) -> TempDir {
    let repo = TempDir::new(label);
    let data_dir = repo.dir(DATA_DIR_NAME);
    fs::write(repo_config_path(&data_dir), body).expect("write the fixture repo config");
    repo
}

/// The machine-level fallback the repo cases narrow.
fn machine() -> RepoDefaults {
    RepoDefaults {
        watch: false,
        watch_interval_ms: 30_000,
        telemetry: false,
    }
}

fn repo_with_warnings(repo_root: &Path, defaults: RepoDefaults) -> (RepoConfig, Vec<Recorded>) {
    let recorder = Recorder::new();
    let config = read_repo_config_with(
        repo_root,
        Some(&repo_root.join(DATA_DIR_NAME)),
        defaults,
        &recorder.sink(),
    );
    (config, recorder.take())
}

#[test]
fn no_repo_file_yields_the_machine_values_silently_and_says_so_with_source() {
    let repo = TempDir::new("repo-missing");
    let (config, lines) = repo_with_warnings(repo.path(), DEFAULT_REPO_SETTINGS);
    assert_eq!(
        config,
        RepoConfig {
            repo_root: repo.path().to_path_buf(),
            data_dir: repo.join(DATA_DIR_NAME),
            watch: true,
            watch_interval_ms: 2_000,
            telemetry: true,
            source: RepoConfigSource::Global,
        }
    );
    // The fallback is the normal state of a tree that was never configured: no log line at all,
    // because `status.payload.config.source` is where it becomes visible.
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn unparseable_repo_json_yields_the_defaults_and_one_warning() {
    let repo = repo_with_config("repo-broken", "{\"watch\": tru");
    let (config, lines) = repo_with_warnings(repo.path(), machine());
    assert!(!config.watch);
    assert_eq!(config.watch_interval_ms, 30_000);
    assert!(!config.telemetry);
    assert_eq!(config.source, RepoConfigSource::Global);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].msg, "repo config invalid, using the global config");
    assert_eq!(
        lines[0].field("path").as_deref(),
        Some(
            repo.join(DATA_DIR_NAME)
                .join("config.json")
                .to_string_lossy()
                .as_ref()
        )
    );
    assert!(
        lines[0]
            .field("reason")
            .is_some_and(|reason| reason.contains("invalid JSON")),
        "{lines:?}"
    );
}

#[test]
fn a_repo_json_value_that_is_not_an_object_yields_the_defaults_and_one_warning() {
    for body in ["[]", "null", "\"config\"", "42"] {
        let repo = repo_with_config("repo-nonobject", body);
        let (config, lines) = repo_with_warnings(repo.path(), machine());
        assert_eq!(config.source, RepoConfigSource::Global, "body {body}");
        assert_eq!(lines.len(), 1, "body {body}: {lines:?}");
        assert_eq!(
            lines[0].field("reason").as_deref(),
            Some("not a JSON object")
        );
    }
}

#[test]
fn an_absent_repo_field_takes_its_default_with_one_warning_and_the_present_ones_are_kept() {
    let repo = repo_with_config("repo-partial", "{\"watch_interval\":5000}");
    let (config, lines) = repo_with_warnings(repo.path(), machine());
    assert!(!config.watch);
    assert!(!config.telemetry);
    assert_eq!(config.watch_interval_ms, 5_000);
    assert_eq!(fields_of(&lines), ["watch", "telemetry"]);
    assert_eq!(config.source, RepoConfigSource::Project);
}

// ---------------------------------------------------------------------------------------------
// repo config: booleans, the interval band and the writer
// ---------------------------------------------------------------------------------------------

#[test]
fn a_quoted_yes_is_not_a_boolean_so_the_field_falls_back_to_the_default() {
    let repo = repo_with_config("repo-yes", "{\"watch\":\"yes\",\"telemetry\":\"yes\"}");
    let (config, lines) = repo_with_warnings(repo.path(), machine());
    assert!(!config.watch);
    assert!(!config.telemetry);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines.iter().all(|line| line.msg.contains("field ignored")));
}

#[test]
fn a_json_number_is_not_a_boolean_either() {
    let repo = repo_with_config("repo-number", "{\"watch\":1,\"telemetry\":0}");
    let (config, lines) = repo_with_warnings(repo.path(), machine());
    assert!(!config.watch);
    assert!(!config.telemetry);
    assert_eq!(lines.len(), 2, "{lines:?}");
}

#[test]
fn an_explicit_false_is_honoured_not_overridden_by_the_default() {
    let repo = repo_with_config(
        "repo-false",
        "{\"watch\":false,\"watch_interval\":2000,\"telemetry\":false}",
    );
    let (config, lines) = repo_with_warnings(repo.path(), machine());
    assert!(!config.watch);
    assert!(!config.telemetry);
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn the_repo_interval_clamps_into_the_band_and_falls_back_to_the_machine_default_for_nonsense() {
    let cases: [(&str, u64); 12] = [
        ("5", WATCH_INTERVAL_MIN_MS),
        ("1", WATCH_INTERVAL_MIN_MS),
        ("999", WATCH_INTERVAL_MIN_MS),
        ("1000", 1_000),
        ("2000", 2_000),
        ("300000", WATCH_INTERVAL_MAX_MS),
        ("500000", WATCH_INTERVAL_MAX_MS),
        ("-1", 30_000),
        ("0", 30_000),
        ("2.5", 30_000),
        ("\"2000\"", 30_000),
        ("null", 30_000),
    ];
    for (raw, expected) in cases {
        let repo = repo_with_config(
            "repo-interval",
            &format!("{{\"watch\":true,\"watch_interval\":{raw},\"telemetry\":true}}"),
        );
        let (config, _) = repo_with_warnings(repo.path(), machine());
        assert_eq!(config.watch_interval_ms, expected, "raw {raw}");
    }
}

#[test]
fn an_absent_repo_interval_is_the_machine_default() {
    let repo = repo_with_config(
        "repo-interval-absent",
        "{\"watch\":true,\"telemetry\":true}",
    );
    let (config, _) = repo_with_warnings(repo.path(), machine());
    assert_eq!(config.watch_interval_ms, 30_000);
}

#[test]
fn without_a_machine_default_the_built_in_period_still_applies() {
    let repo = TempDir::new("repo-interval-fallback");
    let config = read_repo_config(
        repo.path(),
        Some(&repo.join(DATA_DIR_NAME)),
        DEFAULT_REPO_SETTINGS,
    );
    assert_eq!(config.watch_interval_ms, 2_000);
}

#[test]
fn write_repo_config_writes_the_snake_case_shape_and_creates_a_missing_data_dir() {
    let repo = TempDir::new("repo-write");
    let data_dir = repo.join("nested").join(DATA_DIR_NAME);
    let config = RepoConfig {
        repo_root: repo.path().to_path_buf(),
        data_dir: data_dir.clone(),
        watch: true,
        watch_interval_ms: 2_000,
        telemetry: true,
        source: RepoConfigSource::Project,
    };

    write_repo_config(&config).expect("write the repo config");

    assert_eq!(
        fs::read_to_string(repo_config_path(&data_dir)).expect("read the written config"),
        REPO_CONFIG_BYTES
    );
}

#[test]
fn a_full_valid_repo_file_round_trips_through_the_writer_and_the_reader() {
    let repo = repo_with_config(
        "repo-roundtrip",
        "{\"watch\":false,\"watch_interval\":9000,\"telemetry\":false}",
    );
    let config = RepoConfig {
        repo_root: repo.path().to_path_buf(),
        data_dir: repo.join(DATA_DIR_NAME),
        watch: false,
        watch_interval_ms: 9_000,
        telemetry: false,
        source: RepoConfigSource::Project,
    };

    write_repo_config(&config).expect("write the repo config");

    let (read, lines) = repo_with_warnings(repo.path(), DEFAULT_REPO_SETTINGS);
    assert!(lines.is_empty(), "{lines:?}");
    assert_eq!(read, config);
}

#[test]
fn an_explicit_data_dir_is_what_the_reader_reads() {
    let repo = TempDir::new("repo-explicit-datadir");
    let data_dir = repo.join("var").join("graph");
    let config = RepoConfig {
        repo_root: repo.path().to_path_buf(),
        data_dir: data_dir.clone(),
        watch: false,
        watch_interval_ms: 1_500,
        telemetry: true,
        source: RepoConfigSource::Project,
    };
    write_repo_config(&config).expect("write the repo config");

    let read = read_repo_config(repo.path(), Some(&data_dir), DEFAULT_REPO_SETTINGS);
    assert_eq!(read.data_dir, data_dir);
    assert!(!read.watch);
    assert_eq!(read.watch_interval_ms, 1_500);
    assert!(read.telemetry);
}

// ---------------------------------------------------------------------------------------------
// recording precedence
// ---------------------------------------------------------------------------------------------

#[test]
fn recording_is_enabled_only_when_consent_registration_and_the_repo_file_all_allow_it() {
    let recorded = RepoConfig {
        repo_root: PathBuf::from("/tmp/repo"),
        data_dir: PathBuf::from("/tmp/repo/.sangkar"),
        watch: true,
        watch_interval_ms: WATCH_INTERVAL_DEFAULT_MS,
        telemetry: true,
        source: RepoConfigSource::Global,
    };
    assert!(recording_enabled(true, &recorded, true));

    // Each of the three is a veto on its own, and all three refusing is still false.
    assert!(!recording_enabled(false, &recorded, true));
    assert!(!recording_enabled(true, &recorded, false));
    let refused = RepoConfig {
        telemetry: false,
        ..recorded.clone()
    };
    assert!(!recording_enabled(true, &refused, true));
    assert!(!recording_enabled(false, &refused, false));
}

// ---------------------------------------------------------------------------------------------
// the `config.rs` seam: `load_config` reads both files
// ---------------------------------------------------------------------------------------------

#[test]
fn load_config_takes_budgets_daemon_and_repo_defaults_from_the_machine_file() {
    let home = home_with_config(
        "global-load",
        r#"{"watch":false,"watch_interval":5000,"telemetry":false,"daemon":{"host":"localhost","port":9000},"budgets":{"default_tokens":800,"max_tokens":8000}}"#,
    );
    let tree = TempDir::new("global-load-tree");
    let home_value = home.path().to_string_lossy().into_owned();
    let env = env_of(&[("SANGKAR_HOME", home_value.as_str())]);

    let config = load_config(&[], &env_lookup(&env), tree.path()).expect("resolve the config");

    assert_eq!(config.home, home.path());
    assert_eq!(config.config_path, global_config_path(home.path()));
    assert_eq!(
        config.budgets,
        Budgets {
            default_tokens: 800,
            max_tokens: 8_000,
        }
    );
    assert_eq!(config.daemon.host, "localhost");
    assert_eq!(config.daemon.port, 9_000);
    assert_eq!(
        config.repo_defaults,
        RepoDefaults {
            watch: false,
            watch_interval_ms: 5_000,
            telemetry: false,
        }
    );
    // No tree file exists, so the three values are the machine's.
    assert_eq!(config.repo_config_source, RepoConfigSource::Global);
}

#[test]
fn a_missing_machine_file_leaves_every_built_in_default_in_place() {
    let home = TempDir::new("global-load-missing");
    let tree = TempDir::new("global-load-missing-tree");
    let home_value = home.path().to_string_lossy().into_owned();
    let env = env_of(&[("SANGKAR_HOME", home_value.as_str())]);

    let config = load_config(&[], &env_lookup(&env), tree.path()).expect("resolve the config");

    assert_eq!(
        config.budgets,
        Budgets {
            default_tokens: 1_200,
            max_tokens: 32_000,
        }
    );
    assert_eq!(config.daemon.host, DEFAULT_REPORT_HOST);
    assert_eq!(config.daemon.port, DEFAULT_REPORT_PORT);
    assert_eq!(
        config.repo_defaults,
        repo_defaults_of(&default_global_settings())
    );
    assert_eq!(config.repo_config_source, RepoConfigSource::Global);
}

#[test]
fn load_config_settles_the_source_from_the_selected_trees_own_file() {
    let home = TempDir::new("global-load-tree-home");
    let repo = repo_with_config(
        "global-load-project",
        "{\"watch\":false,\"watch_interval\":9000,\"telemetry\":false}",
    );
    let home_value = home.path().to_string_lossy().into_owned();
    let env = env_of(&[("SANGKAR_HOME", home_value.as_str())]);

    let config = load_config(&[], &env_lookup(&env), repo.path()).expect("resolve the config");

    // The walk stops at the `.sangkar` the fixture created, so that data dir is the one read.
    assert_eq!(config.repo_root, repo.path());
    assert_eq!(config.data_dir, repo.join(DATA_DIR_NAME));
    assert_eq!(config.repo_config_source, RepoConfigSource::Project);
    assert_eq!(
        config.repo_defaults,
        RepoDefaults {
            watch: false,
            watch_interval_ms: 9_000,
            telemetry: false,
        }
    );
}

// ---------------------------------------------------------------------------------------------
// pid lock: reading
// ---------------------------------------------------------------------------------------------

/// A path for a pid file inside a fresh temp dir.
fn pid_path(label: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new(label);
    let path = dir.join("daemon.pid");
    (dir, path)
}

/// Ages the file's mtime, which is the heartbeat the stale rule reads.
fn age_the_file(path: &Path, age_ms: u64) {
    let old = SystemTime::now() - Duration::from_millis(age_ms);
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the pid file to age it")
        .set_modified(old)
        .expect("set the mtime");
}

#[test]
fn the_dead_pid_the_cases_use_is_certainly_free() {
    // The crate's liveness rule is the `/proc` entry, so that is what this proves is absent: the
    // number is above every `pid_max` a Linux kernel accepts.
    assert!(!Path::new(&format!("/proc/{DEAD_PID}")).exists());
}

#[test]
fn an_absent_pid_file_is_an_unheld_lock_not_an_error() {
    let (_dir, path) = pid_path("pidlock-absent");
    let state = read_pid_lock(&path).expect("read an absent pid lock");
    assert!(!state.held);
    assert_eq!(state.pid, None);
    assert!(!state.stale);
    assert_eq!(state.started_at, None);
}

#[test]
fn a_fresh_file_written_by_a_live_pid_is_held_and_not_stale() {
    let (_dir, path) = pid_path("pidlock-live");
    let started_at = "2026-01-01T00:00:00.000Z".to_owned();
    fs::write(
        &path,
        format!(
            r#"{{"pid":{},"startedAt":"{started_at}"}}"#,
            std::process::id()
        ),
    )
    .expect("write the pid file");

    let state = read_pid_lock(&path).expect("read the pid lock");
    assert!(state.held);
    assert_eq!(state.pid, Some(std::process::id()));
    assert!(!state.stale);
    assert_eq!(state.started_at.as_deref(), Some(started_at.as_str()));
}

// ---------------------------------------------------------------------------------------------
// pid lock: acquiring
// ---------------------------------------------------------------------------------------------

#[test]
fn the_content_is_the_documented_shape_written_create_exclusive() {
    let (_dir, path) = pid_path("pidlock-content");
    let handle = acquire_pid_lock(&path).expect("take the pid lock");

    assert_eq!(handle.path(), path);
    assert_eq!(handle.pid(), std::process::id());
    // `JSON.stringify({pid, startedAt})`: compact, `pid` first, no trailing newline.
    assert_eq!(
        fs::read_to_string(&path).expect("read the pid file"),
        format!(
            r#"{{"pid":{},"startedAt":"{}"}}"#,
            std::process::id(),
            handle.started_at()
        )
    );
    // The recorded start is the ISO form `new Date().toISOString()` produces, to the byte.
    let started_at = handle.started_at();
    assert_eq!(started_at.len(), 24, "{started_at}");
    assert!(started_at.ends_with('Z'));
    assert_eq!(&started_at[4..5], "-");
    assert_eq!(&started_at[10..11], "T");

    release_pid_lock(&handle).expect("release the pid lock");
}

#[test]
fn a_second_acquire_while_the_holder_is_alive_refuses_with_index_locked() {
    let (_dir, path) = pid_path("pidlock-refuse");
    let first = acquire_pid_lock(&path).expect("take the pid lock");

    let refusal = acquire_pid_lock(&path).expect_err("the second acquire must refuse");

    assert_eq!(refusal.code, "index_locked");
    assert!(refusal.retryable);
    assert_eq!(refusal.retry_after_ms, Some(1_000));
    assert!(refusal.message.contains(path.to_string_lossy().as_ref()));
    assert!(refusal.message.contains(&std::process::id().to_string()));
    // The refusal changes nothing: the first holder still owns the file.
    let state = read_pid_lock(&path).expect("read the pid lock");
    assert_eq!(state.pid, Some(first.pid()));

    release_pid_lock(&first).expect("release the pid lock");
}

#[test]
fn a_dead_pid_is_reclaimed_with_one_warning() {
    let (_dir, path) = pid_path("pidlock-dead");
    fs::write(
        &path,
        format!(r#"{{"pid":{DEAD_PID},"startedAt":"2026-01-01T00:00:00.000Z"}}"#),
    )
    .expect("write the pid file");
    assert!(read_pid_lock(&path).expect("read the pid lock").stale);

    let recorder = Recorder::new();
    let handle = acquire_pid_lock_with(&path, &recorder.sink()).expect("reclaim the pid lock");

    assert_eq!(handle.pid(), std::process::id());
    assert_eq!(
        read_pid_lock(&path).expect("read the pid lock").pid,
        Some(std::process::id())
    );
    let lines = recorder.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].msg, "reclaiming stale pid lock");
    assert_eq!(
        lines[0].field("holderPid").as_deref(),
        Some(DEAD_PID.to_string().as_str())
    );
    assert_eq!(
        lines[0].field("staleAfterMs").as_deref(),
        Some("60000"),
        "{lines:?}"
    );

    release_pid_lock(&handle).expect("release the pid lock");
}

#[test]
fn an_aged_heartbeat_is_reclaimed_even_though_the_pid_is_alive() {
    let (_dir, path) = pid_path("pidlock-aged");
    fs::write(
        &path,
        format!(
            r#"{{"pid":{},"startedAt":"2026-01-01T00:00:00.000Z"}}"#,
            std::process::id()
        ),
    )
    .expect("write the pid file");
    age_the_file(&path, PID_STALE_AFTER_MS + 1_000);

    let aged = read_pid_lock(&path).expect("read the aged pid lock");
    assert!(aged.held);
    assert!(aged.stale);

    let recorder = Recorder::new();
    let handle = acquire_pid_lock_with(&path, &recorder.sink()).expect("reclaim the aged lock");

    assert_eq!(handle.pid(), std::process::id());
    // A taken lock starts now, not when the stale file was written.
    assert_ne!(handle.started_at(), "2026-01-01T00:00:00.000Z");
    assert_eq!(handle.started_at().len(), 24);
    assert!(handle.started_at().ends_with('Z'));
    assert_eq!(recorder.take().len(), 1);

    release_pid_lock(&handle).expect("release the pid lock");
}

#[test]
fn an_unparseable_file_is_reclaimed_by_age_alone() {
    let (_dir, path) = pid_path("pidlock-torn");
    fs::write(&path, "not json at all").expect("write the torn pid file");

    // Young and unreadable: the microseconds of a healthy create, so it is honoured.
    let young = read_pid_lock(&path).expect("read the torn pid lock");
    assert!(young.held);
    assert_eq!(young.pid, None);
    assert!(!young.stale);
    assert_eq!(young.started_at, None);
    let refusal = acquire_pid_lock(&path).expect_err("a young torn file must refuse");
    assert_eq!(refusal.code, "index_locked");

    age_the_file(&path, PID_STALE_AFTER_MS + 1_000);
    assert!(read_pid_lock(&path).expect("read the aged pid lock").stale);
    let recorder = Recorder::new();
    let handle = acquire_pid_lock_with(&path, &recorder.sink()).expect("reclaim the torn lock");
    assert_eq!(handle.pid(), std::process::id());
    // An unparsable body has no holder to name, so the warning carries `null`.
    let lines = recorder.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].field("holderPid").as_deref(), Some("null"));
    release_pid_lock(&handle).expect("release the pid lock");
}

// ---------------------------------------------------------------------------------------------
// pid lock: releasing and heartbeating
// ---------------------------------------------------------------------------------------------

#[test]
fn a_release_removes_the_file_and_a_second_release_of_the_same_handle_succeeds() {
    let (_dir, path) = pid_path("pidlock-release");
    let handle = acquire_pid_lock(&path).expect("take the pid lock");

    release_pid_lock(&handle).expect("release the pid lock");
    assert!(!path.exists());
    // Idempotent: an already-released handle is not an error.
    release_pid_lock(&handle).expect("release the pid lock again");
    assert!(!path.exists());
}

#[test]
fn a_release_leaves_a_file_another_process_re_took_with_one_warning() {
    let (_dir, path) = pid_path("pidlock-restolen");
    let handle = acquire_pid_lock(&path).expect("take the pid lock");
    fs::write(
        &path,
        format!(r#"{{"pid":{DEAD_PID},"startedAt":"2026-01-01T00:00:00.000Z"}}"#),
    )
    .expect("overwrite the pid file");

    let recorder = Recorder::new();
    release_pid_lock_with(&handle, &recorder.sink()).expect("release somebody else's file");

    assert!(path.exists());
    assert_eq!(
        read_pid_lock(&path).expect("read the pid lock").pid,
        Some(DEAD_PID)
    );
    let lines = recorder.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(
        lines[0].msg,
        "pid lock is held by another process; not removing it"
    );
}

#[test]
fn a_heartbeat_moves_the_mtime_forward_and_the_lock_stays_fresh() {
    let (_dir, path) = pid_path("pidlock-heartbeat");
    let handle = acquire_pid_lock(&path).expect("take the pid lock");
    age_the_file(&path, 30_000);
    let before = fs::metadata(&path)
        .expect("stat the pid file")
        .modified()
        .expect("read the mtime");

    heartbeat_pid_lock(&handle).expect("heartbeat the pid lock");

    let after = fs::metadata(&path)
        .expect("stat the pid file")
        .modified()
        .expect("read the mtime");
    assert!(after > before);
    assert!(!read_pid_lock(&path).expect("read the pid lock").stale);
    release_pid_lock(&handle).expect("release the pid lock");
}

#[test]
fn a_file_that_vanished_while_heartbeating_is_one_warning_not_a_throw() {
    let (_dir, path) = pid_path("pidlock-vanished");
    let handle = acquire_pid_lock(&path).expect("take the pid lock");
    release_pid_lock(&handle).expect("release the pid lock");

    let recorder = Recorder::new();
    heartbeat_pid_lock_with(&handle, &recorder.sink()).expect("heartbeat a vanished lock");

    assert!(!path.exists());
    let lines = recorder.take();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0].msg, "pid lock disappeared while heartbeating");
}

#[test]
fn an_unset_telemetry_variable_defers_to_the_machine_file() {
    let home = home_with_config(
        "telemetry-defer",
        r#"{"watch":true,"watch_interval":2000,"telemetry":false,"daemon":{"host":"127.0.0.1","port":7317},"budgets":{"default_tokens":1200,"max_tokens":32000}}"#,
    );
    let tree = TempDir::new("telemetry-defer-tree");
    let home_value = home.path().to_string_lossy().into_owned();
    let home_only = [("SANGKAR_HOME", home_value.as_str())];

    // Unset: the machine file is the installation's default, so its `false` wins and the binary
    // does not record over an operator's refusal (`config.ts:302`).
    let unset = env_of(&home_only);
    let config = load_config(&[], &env_lookup(&unset), tree.path()).expect("resolve the config");
    assert!(!config.telemetry.enabled);

    // The variable is the launcher's override, in both directions, and beats the file.
    let off = env_of(&[
        ("SANGKAR_HOME", home_value.as_str()),
        ("SANGKAR_TELEMETRY", "off"),
    ]);
    let config = load_config(&[], &env_lookup(&off), tree.path()).expect("resolve the config");
    assert!(!config.telemetry.enabled);

    let on = env_of(&[
        ("SANGKAR_HOME", home_value.as_str()),
        ("SANGKAR_TELEMETRY", "1"),
    ]);
    let config = load_config(&[], &env_lookup(&on), tree.path()).expect("resolve the config");
    assert!(config.telemetry.enabled);
}
