//! The per-repo settings file (`<dataDir>/config.json`): whether this tree is watched, how often,
//! and whether its usage may be recorded.
//!
//! Ported from `src/shared/repo-config.ts`. The file is hand-editable and outlives any one process,
//! so every read is defensive — a broken file degrades to the defaults, never to a crash in the
//! daemon or a tool call. A repository that says nothing inherits the machine-level file
//! (`<home>/config.json`, see [`crate::global_config`]): this file narrows those defaults, it does
//! not restate them.
//!
//! The two settings types it speaks in — [`RepoDefaults`] and [`RepoConfigSource`] — live in
//! [`crate::config`], which composes them onto the process configuration; this module is their only
//! reader and the only place the file's snake_case spelling is mapped.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::config::{DATA_DIR_NAME, RepoConfigSource, RepoDefaults};
use crate::errors::AppError;
use crate::log;

/// The built-in poll period, the value a file that names none inherits.
pub const WATCH_INTERVAL_DEFAULT_MS: u64 = 2_000;

/// Below this a poll costs more than the edit it notices: one save lands in every window.
pub const WATCH_INTERVAL_MIN_MS: u64 = 1_000;

/// Above this the graph is stale between polls for as long as a working day.
pub const WATCH_INTERVAL_MAX_MS: u64 = 300_000;

/// The built-in fallback, used only when no machine-level file supplies one. Both switches are
/// `true` because this file only narrows what the machine allows; the machine-level switch is the
/// one an operator turns things off with, and [`recording_enabled`] still requires consent and
/// registration.
pub const DEFAULT_REPO_SETTINGS: RepoDefaults = crate::config::DEFAULT_REPO_DEFAULTS;

/// Where a settings problem is reported: [`crate::log::warn`] in production, a collector in a test,
/// so "exactly one warning" is an assertion rather than a scrape of stderr. The shape is the one
/// [`crate::registry`] established.
pub type Warn<'a> = &'a dyn Fn(&str, &[(&str, Value)]);

/// The resolved settings for one tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoConfig {
    pub repo_root: PathBuf,
    pub data_dir: PathBuf,
    pub watch: bool,
    pub watch_interval_ms: u64,
    pub telemetry: bool,
    /// Which file the three values came from, reported as `status.payload.config.source` so "the
    /// tree says nothing" is visible without a log line.
    pub source: RepoConfigSource,
}

/// The file's shape, in the key order this module writes: `{"watch":true,"watch_interval":2000,
/// "telemetry":true}` and no trailing newline, byte for byte what `JSON.stringify` produced.
#[derive(Serialize)]
struct RepoConfigFile {
    watch: bool,
    watch_interval: u64,
    telemetry: bool,
}

/// `<dataDir>/config.json`.
#[must_use]
pub fn repo_config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.json")
}

/// The settings for `repo_root`, read from `<dataDir>/config.json`. A missing file is silent — it is
/// the normal state of a repo that was indexed but never configured, the values fall back to the
/// machine file, and `source: Global` says so in `status`. Any other rejection keeps its warning: an
/// unreadable or malformed file is something an operator meant to have, and a file-level failure
/// falls back to every default while a bad field falls back alone, so one typo does not silently
/// discard the others.
///
/// `data_dir` is taken as the caller resolved it; `None` means `<repo_root>/.sangkar`, which is what
/// a caller that resolved nothing means. `defaults` is the machine-level fallback
/// (`<home>/config.json`).
#[must_use]
pub fn read_repo_config(
    repo_root: &Path,
    data_dir: Option<&Path>,
    defaults: RepoDefaults,
) -> RepoConfig {
    read_repo_config_with(repo_root, data_dir, defaults, &log::warn)
}

/// [`read_repo_config`] with the warning sink injected.
#[must_use]
pub fn read_repo_config_with(
    repo_root: &Path,
    data_dir: Option<&Path>,
    defaults: RepoDefaults,
    warn: Warn<'_>,
) -> RepoConfig {
    let data_dir = data_dir.map_or_else(|| repo_root.join(DATA_DIR_NAME), Path::to_path_buf);
    let path = repo_config_path(&data_dir);

    // `warn` is what separates "the tree says nothing, which is normal" from "the tree says
    // something this module cannot use": the first is reported by `source` alone, the second on
    // stderr as well.
    let fallback = |reason: String, warn_it: bool| -> RepoConfig {
        if warn_it {
            warn(
                "repo config invalid, using the global config",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(reason)),
                ],
            );
        }
        RepoConfig {
            repo_root: repo_root.to_path_buf(),
            data_dir: data_dir.clone(),
            watch: defaults.watch,
            watch_interval_ms: defaults.watch_interval_ms,
            telemetry: defaults.telemetry,
            source: RepoConfigSource::Global,
        }
    };

    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        // A missing file is the normal state of a repo that was indexed but never configured, so it
        // is answered with the machine values and no log line; the error kind is the only signal
        // that tells it apart from a file the operator wrote and cannot now be read.
        Err(error) => {
            return fallback(
                format!("unreadable: {error}"),
                error.kind() != std::io::ErrorKind::NotFound,
            );
        }
    };

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => return fallback(format!("invalid JSON: {error}"), true),
    };
    let Some(file) = parsed.as_object() else {
        return fallback("not a JSON object".to_owned(), true);
    };

    let flag = |name: &str, value: Option<&Value>, absent: bool| -> bool {
        if let Some(flag) = value.and_then(Value::as_bool) {
            return flag;
        }
        // JSON has no truthiness contract: `"yes"` or `1` here is a typo, and guessing would let a
        // quoted `"false"` read as consent. A key this file does not carry is reported as well: the
        // file is a partial override, so one log line per key is how an operator sees which of the
        // three the tree actually chose.
        let reason = if value.is_some() {
            "not a JSON boolean"
        } else {
            "missing"
        };
        warn(
            "repo config field ignored, using the default",
            &[
                ("path", Value::String(path.display().to_string())),
                ("field", Value::String(name.to_owned())),
                ("reason", Value::String(reason.to_owned())),
            ],
        );
        absent
    };

    RepoConfig {
        repo_root: repo_root.to_path_buf(),
        data_dir,
        watch: flag("watch", file.get("watch"), defaults.watch),
        // An absent or nonsensical interval inherits the machine default, so a repo file that only
        // turns watching off never silently retunes it to the built-in period.
        watch_interval_ms: clamp_watch_interval(
            file.get("watch_interval"),
            defaults.watch_interval_ms,
        ),
        telemetry: flag("telemetry", file.get("telemetry"), defaults.telemetry),
        source: RepoConfigSource::Project,
    }
}

/// Writes the three settings in the on-disk shape, creating the data dir because a repo that was
/// never indexed has none yet. The caller owns the write lock elsewhere; this is one small file, so
/// it is a single write rather than the temp-plus-rename publish the registry needs.
///
/// # Errors
///
/// `AppError` with code `internal` when the data dir or the file cannot be written; the message
/// names the path.
pub fn write_repo_config(config: &RepoConfig) -> Result<(), AppError> {
    let path = repo_config_path(&config.data_dir);
    fs::create_dir_all(&config.data_dir).map_err(|error| {
        AppError::internal(format!(
            "cannot create {}: {error}",
            config.data_dir.display()
        ))
    })?;
    let body = serde_json::to_string(&RepoConfigFile {
        watch: config.watch,
        watch_interval: config.watch_interval_ms,
        telemetry: config.telemetry,
    })
    .map_err(|error| {
        AppError::internal(format!(
            "repo config for {} is not serialisable: {error}",
            path.display()
        ))
    })?;
    fs::write(&path, body).map_err(|error| {
        AppError::internal(format!(
            "repo config at {} not written: {error}",
            path.display()
        ))
    })
}

/// The interval in milliseconds, clamped into the supported band.
///
/// A number outside the band is a mistake in the file, not a reason to stop watching, so both ends
/// are clamped rather than honoured; a non-integer, a zero, a negative or a wrong type is not a slow
/// poll but nonsense, and takes `fallback` — the machine default when one was supplied, else the
/// built-in period. `None` is the file's absent key and takes the same fallback, which is what the
/// TypeScript's `typeof raw !== "number"` does with an `undefined`.
#[must_use]
pub fn clamp_watch_interval(raw: Option<&Value>, fallback: u64) -> u64 {
    let Some(raw) = raw
        .and_then(json_integer)
        .and_then(|raw| u64::try_from(raw).ok())
    else {
        return fallback;
    };
    if raw == 0 {
        return fallback;
    }
    if raw < WATCH_INTERVAL_MIN_MS {
        return WATCH_INTERVAL_MIN_MS;
    }
    raw.min(WATCH_INTERVAL_MAX_MS)
}

/// The recording precedence: machine consent, then registration, then the repo file. A repo cannot
/// opt itself in past an operator's `SANGKAR_TELEMETRY=off`, and an unregistered tree is not
/// recorded no matter what its own file says.
#[must_use]
pub fn recording_enabled(consent: bool, repo: &RepoConfig, registered: bool) -> bool {
    consent && registered && repo.telemetry
}

/// The integer a JSON number denotes, or `None` when it is not one.
///
/// `JSON.parse` folds `900.0` into the number 900, so the TypeScript's `Number.isInteger` accepts
/// it; `serde_json` keeps it as an `f64`, so an integral float is accepted here too and only a real
/// fraction is refused. Every integer beyond `i64` is refused as well: a token count or a port that
/// does not fit one is not a value this file can hold.
pub(crate) fn json_integer(value: &Value) -> Option<i64> {
    let number = value.as_number()?;
    if let Some(integer) = number.as_i64() {
        return Some(integer);
    }
    if let Some(integer) = number.as_u64() {
        return i64::try_from(integer).ok();
    }
    let float = number.as_f64()?;
    // The largest integer a JavaScript number represents exactly; beyond it `Number.isInteger` still
    // answers true for a rounded value, which this file has no business accepting.
    const MAX_EXACT: f64 = 9_007_199_254_740_992.0;
    (float.fract() == 0.0 && float.abs() <= MAX_EXACT).then_some(float as i64)
}
