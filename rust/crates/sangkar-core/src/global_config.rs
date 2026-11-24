//! The machine-level settings file (`<home>/config.json`), the sibling of `sangkar.json`: the two
//! budget ceilings, the report server's default address, and the three switches every repository
//! inherits until its own `<dataDir>/config.json` says otherwise.
//!
//! Ported from `src/shared/global-config.ts`. A file next to the registry rather than more
//! environment variables, because a preference written to a file outlives the shell that exported
//! it and an operator can see and edit every default in one place; the variables that remain
//! (`SANGKAR_HOME`, `SANGKAR_TELEMETRY`, `SANGKAR_CURSOR_KEY`) are the ones a launcher sets, so they
//! outrank this file instead of being replaced by it.
//!
//! The file is created with the defaults on the first real run and hand-edited afterwards, so every
//! read is defensive: a broken file degrades to the defaults with one warning, never to a crash in
//! the server, the daemon or the CLI. Both trees write this one file, so its bytes are a contract —
//! the key order, the two-space indent and the trailing newline are the TypeScript's
//! `JSON.stringify(shape, null, 2)` plus one `"\n"` exactly.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::budget::MAX_BUDGET_TOKENS;
use crate::config::RepoDefaults;
use crate::log;
use crate::repo_config::{WATCH_INTERVAL_DEFAULT_MS, clamp_watch_interval, json_integer};

/// The file's name inside the machine home; `sangkar.json` is the registry beside it.
pub const GLOBAL_CONFIG_NAME: &str = "config.json";

/// The report server's defaults. `serve.rs` owns the loopback policy and refuses anything else, but
/// these two values are what a machine that has not edited the file gets.
pub const DEFAULT_REPORT_HOST: &str = "127.0.0.1";
pub const DEFAULT_REPORT_PORT: u16 = 7317;

/// Where a settings problem is reported: [`crate::log::warn`] in production, a collector in a test,
/// so "exactly one warning" is an assertion rather than a scrape of stderr. The shape is the one
/// [`crate::registry`] established.
pub type Warn<'a> = &'a dyn Fn(&str, &[(&str, Value)]);

/// The two ceilings the resolved configuration hands every process, in the file's own vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GlobalBudgets {
    pub default_tokens: u64,
    pub max_tokens: u64,
}

/// The report server's default address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalDaemon {
    pub host: String,
    pub port: u16,
}

/// The machine settings, or what a missing file means.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalConfig {
    pub watch: bool,
    pub watch_interval_ms: u64,
    pub telemetry: bool,
    pub daemon: GlobalDaemon,
    pub budgets: GlobalBudgets,
}

/// What a missing file means, and what [`ensure_global_config`] writes.
///
/// A function rather than a `const` because the daemon's host is a `String`; the numbers are the
/// TypeScript's `DEFAULT_GLOBAL_SETTINGS` and the two switches are [`crate::repo_config`]'s, so the
/// machine file and the per-repo file cannot disagree about what "unset" is.
#[must_use]
pub fn default_global_settings() -> GlobalConfig {
    GlobalConfig {
        watch: crate::repo_config::DEFAULT_REPO_SETTINGS.watch,
        watch_interval_ms: WATCH_INTERVAL_DEFAULT_MS,
        telemetry: crate::repo_config::DEFAULT_REPO_SETTINGS.telemetry,
        daemon: GlobalDaemon {
            host: DEFAULT_REPORT_HOST.to_owned(),
            port: DEFAULT_REPORT_PORT,
        },
        budgets: GlobalBudgets {
            default_tokens: crate::config::DEFAULT_BUDGETS.default_tokens,
            max_tokens: MAX_BUDGET_TOKENS,
        },
    }
}

/// `<home>/config.json`.
#[must_use]
pub fn global_config_path(home: &Path) -> PathBuf {
    home.join(GLOBAL_CONFIG_NAME)
}

/// The three settings a repository's own file narrows, taken from the machine-level values.
#[must_use]
pub fn repo_defaults_of(config: &GlobalConfig) -> RepoDefaults {
    RepoDefaults {
        watch: config.watch,
        watch_interval_ms: config.watch_interval_ms,
        telemetry: config.telemetry,
    }
}

/// The file's shape, in the key order both implementations write: snake_case on disk, camel case in
/// the process, mapped only here.
#[derive(Serialize)]
struct GlobalConfigFile {
    watch: bool,
    watch_interval: u64,
    telemetry: bool,
    daemon: DaemonFile,
    budgets: BudgetsFile,
}

#[derive(Serialize)]
struct DaemonFile {
    host: String,
    port: u16,
}

#[derive(Serialize)]
struct BudgetsFile {
    default_tokens: u64,
    max_tokens: u64,
}

/// The resolved settings in the file's own spelling, so the file this installation creates is one it
/// also reads — camelCase keys would be a file that describes itself differently from every
/// hand-edited copy.
fn file_shape(config: &GlobalConfig) -> GlobalConfigFile {
    GlobalConfigFile {
        watch: config.watch,
        watch_interval: config.watch_interval_ms,
        telemetry: config.telemetry,
        daemon: DaemonFile {
            host: config.daemon.host.clone(),
            port: config.daemon.port,
        },
        budgets: BudgetsFile {
            default_tokens: config.budgets.default_tokens,
            max_tokens: config.budgets.max_tokens,
        },
    }
}

/// Writes the defaults when the file is absent, and never touches one that exists — a hand-edited
/// file is the whole point of the file.
///
/// Returns whether this call created it. A home the process may not write (a read-only container
/// image, a CI checkout) is one warning: the defaults still apply, so nothing that follows needs it.
/// Creating it is otherwise silent, because stderr is the diagnostic channel of a run and a settings
/// file that appears with its documented defaults is not something the caller has to act on.
#[must_use]
pub fn ensure_global_config(home: &Path) -> bool {
    ensure_global_config_with(home, &log::warn)
}

/// [`ensure_global_config`] with the warning sink injected.
#[must_use]
pub fn ensure_global_config_with(home: &Path, warn: Warn<'_>) -> bool {
    let path = global_config_path(home);
    if path.exists() {
        return false;
    }
    match create_defaults(&path) {
        Ok(()) => true,
        Err(reason) => {
            if path.exists() {
                // Another process won the race; the file it wrote is byte-identical to ours.
                return false;
            }
            warn(
                "global config not created, using the defaults",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(reason)),
                ],
            );
            false
        }
    }
}

/// The machine settings, or [`default_global_settings`] when the file is absent, unreadable, not an
/// object, or carries a field of the wrong shape.
///
/// An absent file is silent rather than a warning: it is the normal state of a home that has not run
/// the CLI yet, and the defaults are exactly what a fresh installation means.
#[must_use]
pub fn read_global_config(home: &Path) -> GlobalConfig {
    read_global_config_with(home, &log::warn)
}

/// [`read_global_config`] with the warning sink injected.
///
/// A broken *file* falls back wholly; a broken *field* falls back alone, so one typo does not
/// silently discard the rest of a hand-edited file. Field warnings arrive in the order the
/// TypeScript evaluated them — the `daemon` and `budgets` subtrees before the two switches — because
/// both trees read the same file and the same edit should read the same way.
#[must_use]
pub fn read_global_config_with(home: &Path, warn: Warn<'_>) -> GlobalConfig {
    let path = global_config_path(home);
    let defaults = default_global_settings();
    let ignored = |reason: String| -> GlobalConfig {
        warn(
            "global config ignored, using the defaults",
            &[
                ("path", Value::String(path.display().to_string())),
                ("reason", Value::String(reason)),
            ],
        );
        defaults.clone()
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == ErrorKind::NotFound => return defaults,
        Err(error) => return ignored(format!("unreadable: {error}")),
    };

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => return ignored(format!("invalid JSON: {error}")),
    };
    let Some(file) = parsed.as_object() else {
        return ignored("not a JSON object".to_owned());
    };

    let subtree = |name: &str| -> Option<&Map<String, Value>> {
        let value = file.get(name)?;
        if value.is_object() {
            return value.as_object();
        }
        warn(
            "global config field ignored, using the default",
            &[
                ("path", Value::String(path.display().to_string())),
                ("field", Value::String(name.to_owned())),
                ("reason", Value::String("not a JSON object".to_owned())),
            ],
        );
        None
    };
    let field_warning = |name: &str, reason: &str| {
        warn(
            "global config field ignored, using the default",
            &[
                ("path", Value::String(path.display().to_string())),
                ("field", Value::String(name.to_owned())),
                ("reason", Value::String(reason.to_owned())),
            ],
        );
    };

    let daemon = subtree("daemon");
    let budgets = subtree("budgets");

    // A port is an integer in 0..=65535 and `0` is a real answer: it asks the kernel for one.
    let raw_port = daemon.and_then(|daemon| daemon.get("port"));
    let valid_port = raw_port
        .and_then(json_integer)
        .and_then(|port| u16::try_from(port).ok());
    let daemon_port = valid_port.unwrap_or(defaults.daemon.port);
    if raw_port.is_some() && valid_port != Some(daemon_port) {
        field_warning("daemon.port", "not an integer in 0..65535");
    }

    // The loopback policy belongs to the report server, which refuses a non-loopback address before
    // it binds; this file only insists on a string, so a typo costs one refused start that names the
    // allowed spellings rather than a config layer that quietly rewrote it.
    let raw_host = daemon.and_then(|daemon| daemon.get("host"));
    let valid_host = raw_host
        .and_then(Value::as_str)
        .filter(|host| !host.trim().is_empty());
    let daemon_host = valid_host.map_or_else(|| defaults.daemon.host.clone(), str::to_owned);
    if raw_host.is_some() && valid_host != Some(daemon_host.as_str()) {
        field_warning("daemon.host", "not a non-empty string");
    }

    // `budget.rs` refuses a call above `MAX_BUDGET_TOKENS` whatever this file says, so a ceiling
    // above it is unsatisfiable and is clamped rather than promised: the file can lower the hard
    // limit, never raise it. A per-call fallback above the ceiling is clamped the same way.
    let max_tokens = positive_integer(
        budgets.and_then(|budgets| budgets.get("max_tokens")),
        defaults.budgets.max_tokens,
        "budgets.max_tokens",
        &field_warning,
    )
    .min(MAX_BUDGET_TOKENS);
    let default_tokens = positive_integer(
        budgets.and_then(|budgets| budgets.get("default_tokens")),
        defaults.budgets.default_tokens,
        "budgets.default_tokens",
        &field_warning,
    )
    .min(max_tokens);

    GlobalConfig {
        watch: switch(file.get("watch"), defaults.watch, "watch", &field_warning),
        // A wrong type is not a slow poll but nonsense, and takes the default silently: the clamp is
        // the one rule this field shares with the per-repo file.
        watch_interval_ms: clamp_watch_interval(
            file.get("watch_interval"),
            defaults.watch_interval_ms,
        ),
        telemetry: switch(
            file.get("telemetry"),
            defaults.telemetry,
            "telemetry",
            &field_warning,
        ),
        daemon: GlobalDaemon {
            host: daemon_host,
            port: daemon_port,
        },
        budgets: GlobalBudgets {
            default_tokens,
            max_tokens,
        },
    }
}

/// A switch is a JSON boolean or the default. `"yes"` and `1` are typos here, and guessing would let
/// a quoted `"false"` read as consent — the same rule the per-repo file follows, except that a key
/// the operator left out is silent, because this file is the one written by hand.
fn switch(
    value: Option<&Value>,
    absent: bool,
    name: &str,
    field_warning: &dyn Fn(&str, &str),
) -> bool {
    match value {
        None => absent,
        Some(value) => match value.as_bool() {
            Some(flag) => flag,
            None => {
                field_warning(name, "not a JSON boolean");
                absent
            }
        },
    }
}

/// A positive integer, or the default; only a key that is present and wrong earns a warning.
fn positive_integer(
    value: Option<&Value>,
    absent: u64,
    name: &str,
    field_warning: &dyn Fn(&str, &str),
) -> u64 {
    let Some(value) = value else {
        return absent;
    };
    match json_integer(value).and_then(|number| u64::try_from(number).ok()) {
        Some(number) if number > 0 => number,
        _ => {
            field_warning(name, "not a positive integer");
            absent
        }
    }
}

/// Creates the file through a pid-named temp and a rename: a reader sees the previous file or the
/// new one, never half of it, and two first runs racing cannot truncate each other's copy because
/// the pid is in the temp name.
///
/// The failure is returned as the message the warning carries rather than as an [`crate::errors::AppError`]:
/// a settings file that could not be created is not a failure of the call that wanted the settings,
/// because the defaults still apply.
fn create_defaults(path: &Path) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut body = serde_json::to_string_pretty(&file_shape(&default_global_settings()))
        .map_err(|error| error.to_string())?;
    body.push('\n');

    let temp = temp_path(path);
    if let Err(error) = fs::write(&temp, body) {
        let _ = fs::remove_file(&temp);
        return Err(error.to_string());
    }
    if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error.to_string());
    }
    Ok(())
}

/// `<name>.<pid>.tmp` beside the config: two processes racing cannot share a temp file, and a
/// leftover names the process that made it.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| OsString::from(GLOBAL_CONFIG_NAME), OsString::from);
    name.push(format!(".{}.tmp", std::process::id()));
    path.with_file_name(name)
}
