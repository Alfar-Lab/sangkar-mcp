//! Where the repository, its data directory and the machine-level home are decided.
//!
//! Ported from `src/shared/config.ts`. This is the only module in the workspace that reads the
//! process environment, and it reads it through an injected [`Env`] lookup so a test can state a
//! machine's environment without mutating the one it shares with every other test in the process.
//!
//! Precedence is flag, then environment, then `<home>/config.json`, then the built-in default. The
//! constants below are what a missing machine file yields; `global_config` reads the file and
//! answers them, so a machine with no `config.json` is the same machine these constants describe.

use std::collections::hash_map::RandomState;
use std::ffi::OsString;
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::cursor::CURSOR_KEY_MIN_BYTES;
use crate::errors::{AppError, ErrorCode};
use crate::global_config;
use crate::repo_config;

/// The directory name under the config root, so `${XDG_CONFIG_HOME:-$HOME/.config}/sangkar`.
pub const HOME_DIR_NAME: &str = "sangkar";

/// The data dir's name inside a repository, per DATABASE.md §3.0.
pub const DATA_DIR_NAME: &str = ".sangkar";

/// The marker a linked worktree has instead of a `.git` directory.
const GIT_DIR_NAME: &str = ".git";

/// How many levels above the working directory an autodetected tree may sit before a caller with no
/// `--yes` has to confirm it. `0` (the working directory itself) is always allowed: that is a first
/// index in a fresh checkout, not a guess. The number is a guard against a marker belonging to
/// something else — a `.sangkar` in `$HOME` swallows every directory below it — and it is a budget,
/// not a rule: the walk keeps reporting a deeper marker, and refusing it is the caller's decision.
pub const MAX_AUTODETECT_LEVELS: usize = 3;

/// The fallback token budget for a call that sends none (`TOOLS-SPEC.md` §9). Per-tool defaults live
/// in each tool's manifest; this is only the fallback for a handler without one.
const FALLBACK_DEFAULT_TOKENS: u64 = 1_200;

/// The ceiling above which `emit` refuses with `budget_too_large`. `budget::MAX_BUDGET_TOKENS` is
/// the same number and owns the splitting rules; this pair is what an absent machine file yields.
const FALLBACK_MAX_TOKENS: u64 = 32_000;

/// The poll period a machine that never edited its config inherits (`repo_config`'s default).
const FALLBACK_WATCH_INTERVAL_MS: u64 = 2_000;

/// The budgets a machine with no `config.json` gets.
pub const DEFAULT_BUDGETS: Budgets = Budgets {
    default_tokens: FALLBACK_DEFAULT_TOKENS,
    max_tokens: FALLBACK_MAX_TOKENS,
};

/// The three repository settings a machine with no `config.json` gets. Both switches are `true`
/// because the machine file is the widest scope there is: a default that silently narrowed what a
/// repository may do would be a policy nobody asked for.
pub const DEFAULT_REPO_DEFAULTS: RepoDefaults = RepoDefaults {
    watch: true,
    watch_interval_ms: FALLBACK_WATCH_INTERVAL_MS,
    telemetry: true,
};

/// A machine's environment as a lookup. Injected because the alternative — reading `std::env` in
/// the function under test — makes every precedence case a race with the rest of the test binary.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The fallback token budget for a call that sends none, and the ceiling above which `emit` refuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budgets {
    pub default_tokens: u64,
    pub max_tokens: u64,
}

/// The consent switch as a value, so the recorder never reads the environment itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelemetryConfig {
    /// Whether this installation may record usage events at all.
    pub enabled: bool,
    /// Where events would be written; always computed so a caller can name the path without one.
    pub dir: PathBuf,
}

/// The two files and the one address a background daemon needs in the machine-level home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonPaths {
    pub pid_path: PathBuf,
    pub log_path: PathBuf,
    pub host: String,
    pub port: u16,
}

/// The three values a repository inherits before its own `<dataDir>/config.json` speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RepoDefaults {
    pub watch: bool,
    pub watch_interval_ms: u64,
    pub telemetry: bool,
}

/// Which file the three repository settings were read from.
///
/// [`load_config`] answers [`RepoConfigSource::Global`], because it never consults a tree: the tree
/// a call answers from is chosen later, so the composition root settles this before a handler sees
/// the config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoConfigSource {
    Project,
    Global,
}

impl RepoConfigSource {
    /// The wire spelling, as `status.payload.config.source` publishes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Global => "global",
        }
    }
}

/// How the working tree was chosen. The walk that produces this is the only place a tree is
/// guessed, so the guess travels on the config instead of being recomputed by whoever reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoDetection {
    /// The tree the walk settled on.
    pub root: PathBuf,
    /// The marker that stopped the walk: `Some(".sangkar")`, `Some(".git")`, or `None` when nothing
    /// matched and the working directory became the tree.
    pub via: Option<&'static str>,
    /// Directories between the working directory and the root; `0` means the working directory is
    /// the tree.
    pub levels: usize,
    /// Absolute paths walked, from the working directory up to the root inclusive.
    pub chain: Vec<PathBuf>,
    /// True when `--repo-root` named the tree, so nothing was guessed and nothing needs confirming.
    pub explicit: bool,
}

impl RepoDetection {
    /// Whether a caller that was not given `--yes` may act on this detection: no more than
    /// [`MAX_AUTODETECT_LEVELS`] directories above the working directory. A deeper marker is still
    /// reported, because the walk is a lookup rather than a policy; the refusal belongs to the
    /// index gate, which asks for `--yes`.
    #[must_use]
    pub const fn within_autodetect_budget(&self) -> bool {
        self.levels <= MAX_AUTODETECT_LEVELS
    }
}

/// The resolved process configuration. Resolved once at start; every module that needs a setting
/// receives it from here, because this module is the only reader of the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppConfig {
    /// Directory holding graph.db, write.lock and the data dir's own .gitignore.
    pub data_dir: PathBuf,
    /// Absolute path of the indexed working tree.
    pub repo_root: PathBuf,
    /// How `repo_root` was chosen: the marker found, how far above the working directory, the walk.
    pub repo_detection: RepoDetection,
    /// Cursor HMAC key, passed to the cursor module so it never reads the environment.
    pub cursor_key: String,
    pub budgets: Budgets,
    /// The consent switch as a value, so the recorder never reads the environment itself.
    pub telemetry: TelemetryConfig,
    /// Machine-level home: `$SANGKAR_HOME`, else `<config root>/sangkar`. It belongs to the
    /// machine, not to the tree, so [`with_repo`] leaves it alone while swapping the repository
    /// fields.
    pub home: PathBuf,
    /// The daemon's repo registry, `<home>/sangkar.json`.
    pub registry_path: PathBuf,
    /// The machine-level settings file, `<home>/config.json`, created on the first run.
    pub config_path: PathBuf,
    /// What a repository inherits before its own `<repo>/.sangkar/config.json` speaks.
    pub repo_defaults: RepoDefaults,
    /// Which file the three repository settings were read from.
    pub repo_config_source: RepoConfigSource,
    /// Where the daemon writes its pid and its log, both under `home`, and what it binds.
    pub daemon: DaemonPaths,
    /// The process to spawn for indexing. Added by this migration and not present in the
    /// TypeScript config: it defaults to this executable, and `SANGKAR_WORKER_CMD` overrides it so
    /// the Rust `index` tool can spawn the TypeScript worker until Phase 3's worker exists
    /// (RUST-MIGRATION.md §6.4). TASK-063 deletes the override with the TypeScript tree, and this
    /// field with it.
    pub worker_cmd: Vec<OsString>,
}

/// Value of a `--<name> value` or `--<name>=value` flag, or `None` when the flag is absent.
///
/// The whole array is scanned in any position because the CLI hands over its arguments with the
/// mode token still in them, so a global flag may sit on either side of that token. A bare flag
/// whose value would be past the end stops the scan, as the TypeScript's `argv[index + 1]` does.
#[must_use]
pub fn flag_value(argv: &[String], name: &str) -> Option<String> {
    let bare = format!("--{name}");
    let inline = format!("{bare}=");
    for (index, arg) in argv.iter().enumerate() {
        if arg == &bare {
            return argv.get(index + 1).cloned();
        }
        if let Some(value) = arg.strip_prefix(&inline) {
            return Some(value.to_owned());
        }
    }
    None
}

/// The working tree that owns `cwd`: the nearest directory holding a `.sangkar` data dir, else the
/// nearest directory holding a `.git` (a directory, or a file in a linked worktree), else `cwd`
/// itself. Pure filesystem lookup — no `git` subprocess, so this works in a bare temp directory and
/// when git is not installed.
#[must_use]
pub fn detect_repo_root(cwd: &Path) -> PathBuf {
    detect_tree(cwd).root
}

/// The same walk with its provenance: which marker stopped it, how many levels above the working
/// directory that marker sat, and every directory crossed on the way.
///
/// A tree found with no marker at all is the working directory itself, and reporting that as
/// `via: None, levels: 0` is what lets a caller say "nothing told me this was a repository" instead
/// of guessing silently.
#[must_use]
pub fn detect_tree(cwd: &Path) -> RepoDetection {
    let start = resolve_path(&current_dir(), cwd);
    let mut chain = vec![start.clone()];
    let mut dir = start.clone();
    loop {
        if let Some(via) = marker_in(&dir) {
            return RepoDetection {
                root: dir,
                via: Some(via),
                levels: chain.len() - 1,
                chain,
                explicit: false,
            };
        }
        // The filesystem root has no parent: nothing on the way up matched, so the caller's
        // directory is the tree.
        let Some(parent) = dir.parent() else {
            return unnamed(start);
        };
        let parent = parent.to_path_buf();
        chain.push(parent.clone());
        dir = parent;
    }
}

/// The detection for a tree nobody named and no marker introduced: the directory itself at level 0.
fn unnamed(root: PathBuf) -> RepoDetection {
    RepoDetection {
        chain: vec![root.clone()],
        root,
        via: None,
        levels: 0,
        explicit: false,
    }
}

/// The detection for a tree the caller named: nothing was walked and nothing is a guess.
fn named(root: &Path) -> RepoDetection {
    let root = root.to_path_buf();
    RepoDetection {
        root: root.clone(),
        via: None,
        levels: 0,
        chain: vec![root],
        explicit: true,
    }
}

/// The marker a directory holds, if any. `.sangkar` wins because it is the one this product writes.
fn marker_in(dir: &Path) -> Option<&'static str> {
    if dir.join(DATA_DIR_NAME).exists() {
        return Some(DATA_DIR_NAME);
    }
    dir.join(GIT_DIR_NAME).exists().then_some(GIT_DIR_NAME)
}

/// The process environment, the only place the workspace reads it.
#[must_use]
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The process working directory. It is process state rather than a setting, but it is read here so
/// `registry` never has to spell `std::env` itself. A deleted working directory yields the
/// filesystem root, which is wrong and visible rather than a panic in a helper that cannot report.
#[must_use]
pub fn current_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}

/// `$HOME` as the process sees it: the default for a caller that injects no environment.
///
/// An account without `HOME` gets `/` as the base, which is a container state rather than a
/// repository one; `SANGKAR_HOME` is how a machine states its home in that case.
#[must_use]
pub fn process_home_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// An empty variable is an absent one: `SANGKAR_HOME=` or `XDG_CONFIG_HOME=` in a shell profile must
/// not resolve to a working-directory-relative path, which would scatter the registry and the
/// daemon files through whatever tree the process happens to start in.
fn env_value(env: Env<'_>, name: &str) -> Option<String> {
    env(name).filter(|value| !value.is_empty())
}

/// Expands a leading `~/` against `home`, because the value comes from an environment block or a
/// config file rather than a shell, so nothing else would have expanded it.
///
/// Segment by segment, so `~//b` is `home/b` rather than the root: a naive join with the `"/b"` tail
/// would replace the home outright. `None` when the path does not start with `~/`.
#[must_use]
pub fn expand_home(path: &str, home: &Path) -> Option<PathBuf> {
    let rest = path.strip_prefix("~/")?;
    let mut expanded = home.to_path_buf();
    for segment in rest.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        expanded.push(segment);
    }
    Some(collapse(&expanded))
}

/// The machine-level home: `SANGKAR_HOME` when set and non-empty, else the XDG config root with
/// [`HOME_DIR_NAME`] appended and `$HOME/.config` as the root's own fallback.
#[must_use]
pub fn resolve_home(env: Env<'_>) -> PathBuf {
    if let Some(explicit) = env_value(env, "SANGKAR_HOME") {
        let home = home_dir(env);
        return expand_home(&explicit, &home).unwrap_or_else(|| PathBuf::from(&explicit));
    }
    let config_root = env_value(env, "XDG_CONFIG_HOME")
        .map_or_else(|| home_dir(env).join(".config"), PathBuf::from);
    config_root.join(HOME_DIR_NAME)
}

/// `$HOME` from the injected environment, else the process's own.
fn home_dir(env: Env<'_>) -> PathBuf {
    env_value(env, "HOME").map_or_else(process_home_dir, PathBuf::from)
}

/// Resolves the config from the command line, the environment and the working directory:
/// `--repo-root` (default autodetected from `cwd`), `--data-dir` relative to the repo root (default
/// `<repo_root>/.sangkar`), the machine-level home, and the two switches that come from the
/// environment — `SANGKAR_CURSOR_KEY` and the telemetry consent switch `SANGKAR_TELEMETRY`, whose
/// one spelling `off` disables it while every other value (including empty) enables it.
///
/// The environment is injected so a test states a machine instead of mutating the process: every
/// `${VAR}` above and below is read through `env`, and neither this function nor [`with_repo`]
/// consults the real environment.
///
/// # Errors
///
/// `AppError` with code `internal` when `SANGKAR_CURSOR_KEY` is shorter than the cursor module's
/// minimum, and when the current executable cannot be resolved for the default `worker_cmd`. Both
/// are startup misconfigurations, never caller input. The TypeScript config reports a short key
/// verbatim and lets `cursor.ts` refuse it at construction; the guard is the same `internal`
/// refusal, moved to the one place that builds the config.
pub fn load_config(argv: &[String], env: Env<'_>, cwd: &Path) -> Result<AppConfig, AppError> {
    let detection = match flag_value(argv, "repo-root").filter(|value| !value.is_empty()) {
        Some(raw) => named(&resolve_path(cwd, Path::new(&raw))),
        None => detect_tree(cwd),
    };
    let repo_root = detection.root.clone();

    let data_dir = match flag_value(argv, "data-dir").filter(|value| !value.is_empty()) {
        Some(raw) => resolve_path(&repo_root, Path::new(&raw)),
        None => repo_root.join(DATA_DIR_NAME),
    };

    // Read once, here: the rest of the process receives a value on the config rather than reopening
    // the environment. An absent or empty variable yields a per-process key, so a cursor minted by
    // one invocation cannot be verified by another; set SANGKAR_CURSOR_KEY to share cursors.
    let cursor_key = match env_value(env, "SANGKAR_CURSOR_KEY") {
        Some(key) => {
            if key.len() < CURSOR_KEY_MIN_BYTES {
                return Err(AppError::new(
                    ErrorCode::INTERNAL,
                    format!(
                        "SANGKAR_CURSOR_KEY is {} bytes; the cursor guard needs at least {CURSOR_KEY_MIN_BYTES}",
                        key.len()
                    ),
                )
                .with_hint("set SANGKAR_CURSOR_KEY to a random value of 32 bytes or more"));
            }
            key
        }
        None => random_cursor_key(),
    };

    let home = resolve_home(env);
    // The machine-level settings, read once here: budgets, the daemon's default address, the
    // telemetry consent and the values every repository inherits. A missing file is a fresh
    // installation, so it is silent; a broken one is one warning and the values above.
    let global = global_config::read_global_config(&home);

    // One spelling only: `off` disables, everything else (empty, "0", "false", "OFF") enables.
    // Aliases would make an operator's typo silently opt a machine in. An unset variable defers to
    // the machine-level file (`config.ts:302`), so the file is the installation's default and the
    // variable is the launcher's override; a missing file and this default are the same `true`.
    // Deferring is why the file is read first: an operator who wrote `"telemetry": false` must
    // not be recorded over just because the launcher left the variable unset.
    let telemetry_enabled = match env("SANGKAR_TELEMETRY") {
        Some(value) => value != "off",
        None => global.telemetry,
    };
    let daemon = DaemonPaths {
        pid_path: home.join("daemon.pid"),
        log_path: home.join("daemon.log"),
        host: global.daemon.host.clone(),
        port: global.daemon.port,
    };
    let registry_path = home.join("sangkar.json");
    let config_path = home.join("config.json");
    // The selected tree's own `<dataDir>/config.json`, settled here because this is the one place
    // both the tree and its machine defaults are known. No tree file is the normal state of a tree
    // that was never configured, and it answers the machine values with `source: Global`.
    let repo = repo_config::read_repo_config(
        &repo_root,
        Some(&data_dir),
        global_config::repo_defaults_of(&global),
    );

    Ok(AppConfig {
        data_dir: data_dir.clone(),
        repo_root,
        repo_detection: detection,
        cursor_key,
        budgets: Budgets {
            default_tokens: global.budgets.default_tokens,
            max_tokens: global.budgets.max_tokens,
        },
        telemetry: TelemetryConfig {
            enabled: telemetry_enabled,
            dir: data_dir.join("telemetry"),
        },
        home,
        registry_path,
        config_path,
        repo_defaults: RepoDefaults {
            watch: repo.watch,
            watch_interval_ms: repo.watch_interval_ms,
            telemetry: repo.telemetry,
        },
        repo_config_source: repo.source,
        daemon,
        worker_cmd: worker_cmd(env)?,
    })
}

/// The same machine-level config rebound to another working tree: one process can serve N repos
/// without re-reading the environment, because the home, the registry, the daemon files, the key,
/// the budgets and the repository defaults are machine state rather than repo state.
///
/// `data_dir` is taken as the caller resolved it; without one it is `<root>/.sangkar`.
#[must_use]
pub fn with_repo(config: &AppConfig, repo_root: &Path, data_dir: Option<&Path>) -> AppConfig {
    let root = resolve_path(&current_dir(), repo_root);
    let data_dir = data_dir.map_or_else(|| root.join(DATA_DIR_NAME), Path::to_path_buf);
    AppConfig {
        repo_root: root.clone(),
        // A caller that named the tree is speaking about that tree; the walk that produced the
        // previous root describes nothing here.
        repo_detection: named(&root),
        telemetry: TelemetryConfig {
            enabled: config.telemetry.enabled,
            dir: data_dir.join("telemetry"),
        },
        data_dir,
        ..config.clone()
    }
}

/// Node's `path.resolve(base, path)`: an absolute path stands as given, a relative one is joined to
/// `base`, and `.`/`..` are collapsed lexically. Nothing is canonicalised — the filesystem is not
/// consulted — so a path that does not exist still resolves, and symlinks survive.
#[must_use]
pub fn resolve_path(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    collapse(&joined)
}

/// Drops `.` components and applies `..` to the previous component, leaving the filesystem alone.
fn collapse(path: &Path) -> PathBuf {
    let mut collapsed = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => collapsed.push(prefix.as_os_str()),
            Component::RootDir => collapsed.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            // `..` past the root stays at the root, which is what `path.resolve('/..')` answers.
            Component::ParentDir => {
                collapsed.pop();
            }
            Component::Normal(segment) => collapsed.push(segment),
        }
    }
    collapsed
}

/// The command an index run spawns: `SANGKAR_WORKER_CMD`, else this executable.
///
/// The override is a developer knob for the hand-over (RUST-MIGRATION.md §6.4), so it is
/// whitespace-split argv — `bun bin/sangkar.ts` — and a command whose path contains a space is not
/// expressible. That is a reason the knob must not survive to the release, not a feature to keep.
fn worker_cmd(env: Env<'_>) -> Result<Vec<OsString>, AppError> {
    if let Some(raw) = env_value(env, "SANGKAR_WORKER_CMD") {
        let argv: Vec<OsString> = raw.split_whitespace().map(OsString::from).collect();
        if !argv.is_empty() {
            return Ok(argv);
        }
    }
    let exe = std::env::current_exe().map_err(|error| {
        AppError::internal(format!("cannot resolve the current executable: {error}"))
    })?;
    Ok(vec![exe.into_os_string()])
}

/// 32 random bytes as lowercase hex — the key a run with no `SANGKAR_CURSOR_KEY` uses.
///
/// `rand` and `getrandom` are not dependencies, so the entropy comes from the standard library's one
/// per-instance OS-seeded source: `RandomState::new()` seeds each hasher from the operating system
/// and is documented to make two instances produce different keys, which is what keeps two runs from
/// sharing a cursor key. The clock, the pid and a stack address are mixed in because the fallback
/// must not rest on a single property of one hasher: a process that forked would repeat the pid
/// space, two threads a same-instant clock, and ASLR a same address.
fn random_cursor_key() -> String {
    let state = RandomState::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let probe = 0u8;
    let address = std::ptr::addr_of!(probe) as usize;

    let mut bytes = [0u8; 32];
    for (block, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        u64::try_from(block).unwrap_or(u64::MAX).hash(&mut hasher);
        nanos.hash(&mut hasher);
        pid.hash(&mut hasher);
        address.hash(&mut hasher);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    hex(&bytes)
}

/// Lowercase hex, the spelling `randomBytes(32).toString("hex")` produces.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}
