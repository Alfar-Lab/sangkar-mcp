//! The machine-level repository registry (`<home>/sangkar.json`) and the rule for which tree a call
//! means.
//!
//! Ported from `src/shared/registry.ts`. The file is hand-editable and shared by every process on
//! the machine, so every read degrades to "nothing registered" with exactly one warning rather than
//! failing the server, the daemon or a tool call; every write goes through a pid-named temp file and
//! a rename, so a daemon polling the file sees the previous list or the new one and never a
//! half-written one.
//!
//! The environment and the working directory are `config.rs`'s business, not this module's: a
//! registry entry that starts with `~/` expands against the home the caller injects, else the
//! process's.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::config;
use crate::errors::{AppError, ErrorCode};
use crate::log;

/// The only registry shape this build reads and writes.
pub const REGISTRY_VERSION: u32 = 1;

/// The file's contents: a version and the canonical absolute roots this installation indexes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    pub version: u32,
    pub repos: Vec<PathBuf>,
}

/// What a missing, unreadable or untrustworthy file means. `const` because a `Vec::new` is one.
pub const EMPTY_REGISTRY: Registry = Registry {
    version: REGISTRY_VERSION,
    repos: Vec::new(),
};

/// Where a registry problem is reported. [`crate::log::warn`] is the production sink; a test passes a
/// collector, so "exactly one warning" is an assertion instead of a scrape of stderr.
pub type Warn<'a> = &'a dyn Fn(&str, &[(&str, Value)]);

/// One spelling for a repo root, so a path compared against the registry matches regardless of how
/// the caller wrote it: `~` expanded, relative forms resolved, and symlinks followed for a path that
/// exists.
///
/// A path that does not exist keeps its resolved form — a deleted tree is still a registry entry
/// that other code reports, not grounds to rewrite the entry out of existence. `home` is the base a
/// leading `~/` expands against; `None` means the process's `$HOME`, which is what the TypeScript
/// version's `homedir()` default did.
#[must_use]
pub fn canonical_root(path: &Path, home: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(path, home);
    let resolved = config::resolve_path(&config::current_dir(), &expanded);
    // `canonicalize` is its own existence test: it rejects a missing path — the case that keeps the
    // resolved form — instead of a check-then-use race with a tree being deleted.
    fs::canonicalize(&resolved).unwrap_or(resolved)
}

/// `~/x` against `home`, else the path unchanged.
fn expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    let base = home.map_or_else(config::process_home_dir, Path::to_path_buf);
    config::expand_home(raw, &base).unwrap_or_else(|| path.to_path_buf())
}

/// The registry on disk, or [`EMPTY_REGISTRY`] when the file is absent, unreadable, not the shape
/// this version writes, or otherwise untrustworthy. Each rejection logs exactly one warning naming
/// the reason, so a broken file is diagnosable from stderr without ever failing a call.
#[must_use]
pub fn read_registry(path: &Path) -> Registry {
    read_registry_with(path, &log::warn)
}

/// [`read_registry`] with the warning sink injected.
#[must_use]
pub fn read_registry_with(path: &Path, warn: Warn<'_>) -> Registry {
    let ignored = |reason: String| -> Registry {
        warn(
            "registry ignored, using an empty registry",
            &[
                ("path", Value::String(path.display().to_string())),
                ("reason", Value::String(reason)),
            ],
        );
        EMPTY_REGISTRY
    };

    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => return ignored(format!("unreadable: {error}")),
    };

    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(parsed) => parsed,
        Err(error) => return ignored(format!("invalid JSON: {error}")),
    };
    let Some(record) = parsed.as_object() else {
        return ignored("not a JSON object".to_owned());
    };
    if record.get("version").and_then(Value::as_u64) != Some(u64::from(REGISTRY_VERSION)) {
        return ignored(format!(
            "unsupported version {}",
            record
                .get("version")
                .map_or_else(|| "undefined".to_owned(), Value::to_string)
        ));
    }
    let Some(entries) = record.get("repos").and_then(Value::as_array) else {
        return ignored("repos is not an array".to_owned());
    };

    let mut repos: Vec<PathBuf> = Vec::with_capacity(entries.len());
    let mut dropped = 0usize;
    for entry in entries {
        let Some(raw) = entry.as_str() else {
            dropped += 1;
            continue;
        };
        // First occurrence keeps its position: the file's order is the operator's preference.
        let root = canonical_root(Path::new(raw), None);
        if !repos.contains(&root) {
            repos.push(root);
        }
    }
    if dropped > 0 {
        warn(
            "registry entries dropped: not a path",
            &[
                ("path", Value::String(path.display().to_string())),
                (
                    "dropped",
                    Value::from(u64::try_from(dropped).unwrap_or(u64::MAX)),
                ),
            ],
        );
    }

    Registry {
        version: REGISTRY_VERSION,
        repos,
    }
}

/// The file's shape, in the key order this module writes: `{"version":1,"repos":[...]}` and no
/// trailing newline, byte for byte what `JSON.stringify` produced.
#[derive(Serialize)]
struct RegistryFile {
    version: u32,
    repos: Vec<String>,
}

/// Replaces the registry atomically: the temp file carries this process's pid and the rename is the
/// publish, so a reader sees either the previous list or the new one.
///
/// The version is this module's, not a caller's: a caller cannot write a registry this build would
/// refuse to read back. A failed write removes its temp file before returning.
///
/// # Errors
///
/// `AppError` with code `internal` when the parent directory or the file cannot be written; the
/// message names the path. [`register_repo`] and [`remove_repo`] turn that into a warning, because
/// registering is a convenience for the next call rather than a reason to fail this one.
pub fn write_registry(path: &Path, registry: &Registry) -> Result<(), AppError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| write_failure(path, &error))?;
    }
    let wire = RegistryFile {
        version: REGISTRY_VERSION,
        repos: registry
            .repos
            .iter()
            .map(|repo| repo.to_string_lossy().into_owned())
            .collect(),
    };
    let body = serde_json::to_string(&wire).map_err(|error| {
        AppError::internal(format!(
            "registry at {} is not serialisable: {error}",
            path.display()
        ))
    })?;

    let temp = temp_path(path);
    fs::write(&temp, body).map_err(|error| {
        let _ = fs::remove_file(&temp);
        write_failure(path, &error)
    })?;
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        write_failure(path, &error)
    })
}

/// `<name>.tmp.<pid>` beside the registry: two processes racing cannot share a temp file, and a
/// leftover is attributable to the process that made it.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map_or_else(|| OsString::from("sangkar.json"), OsString::from);
    name.push(format!(".tmp.{}", std::process::id()));
    path.with_file_name(name)
}

fn write_failure(path: &Path, error: &std::io::Error) -> AppError {
    AppError::internal(format!(
        "registry at {} not written: {error}",
        path.display()
    ))
}

/// What a registration did: the registry before or after the write, and whether it added the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisterResult {
    pub registry: Registry,
    pub added: bool,
}

/// Appends one canonicalised root. A failed write is logged and swallowed: registering is a
/// convenience for the next call, so it must not take down the call that is already working.
#[must_use]
pub fn register_repo(path: &Path, repo_path: &Path) -> RegisterResult {
    register_repo_with(path, repo_path, &log::warn)
}

/// [`register_repo`] with the warning sink injected.
#[must_use]
pub fn register_repo_with(path: &Path, repo_path: &Path, warn: Warn<'_>) -> RegisterResult {
    let registry = read_registry_with(path, warn);
    let root = canonical_root(repo_path, None);
    if registry.repos.contains(&root) {
        return RegisterResult {
            registry,
            added: false,
        };
    }
    let mut repos = registry.repos.clone();
    repos.push(root);
    let next = Registry {
        version: REGISTRY_VERSION,
        repos,
    };
    match write_registry(path, &next) {
        Ok(()) => RegisterResult {
            registry: next,
            added: true,
        },
        Err(error) => {
            warn(
                "registry not updated",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(error.message)),
                ],
            );
            RegisterResult {
                registry,
                added: false,
            }
        }
    }
}

/// What a removal did: the registry before or after the write, and whether it removed the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoveResult {
    pub registry: Registry,
    pub removed: bool,
}

/// The inverse of [`register_repo`], with the same read-modify-write and swallowed-IO-error shape.
#[must_use]
pub fn remove_repo(path: &Path, repo_path: &Path) -> RemoveResult {
    remove_repo_with(path, repo_path, &log::warn)
}

/// [`remove_repo`] with the warning sink injected.
#[must_use]
pub fn remove_repo_with(path: &Path, repo_path: &Path, warn: Warn<'_>) -> RemoveResult {
    let registry = read_registry_with(path, warn);
    let root = canonical_root(repo_path, None);
    let repos: Vec<PathBuf> = registry
        .repos
        .iter()
        .filter(|entry| **entry != root)
        .cloned()
        .collect();
    if repos.len() == registry.repos.len() {
        return RemoveResult {
            registry,
            removed: false,
        };
    }
    let next = Registry {
        version: REGISTRY_VERSION,
        repos,
    };
    match write_registry(path, &next) {
        Ok(()) => RemoveResult {
            registry: next,
            removed: true,
        },
        Err(error) => {
            warn(
                "registry not updated",
                &[
                    ("path", Value::String(path.display().to_string())),
                    ("reason", Value::String(error.message)),
                ],
            );
            RemoveResult {
                registry,
                removed: false,
            }
        }
    }
}

/// What [`select_repo`] needs: the registry, what the caller named, the working directory, and the
/// `--repo-root` pin.
#[derive(Clone, Copy, Debug)]
pub struct SelectRepoInput<'a> {
    pub registry: &'a Registry,
    /// What the caller asked for by name or path: outranks the pin and every implicit default.
    pub requested: Option<&'a str>,
    pub cwd: &'a Path,
    /// `--repo-root`: a process-level default, not a barrier that hides a bad `requested`.
    pub pinned_root: Option<&'a Path>,
}

impl<'a> SelectRepoInput<'a> {
    #[must_use]
    pub const fn new(registry: &'a Registry, cwd: &'a Path) -> Self {
        Self {
            registry,
            requested: None,
            cwd,
            pinned_root: None,
        }
    }

    #[must_use]
    pub const fn requested(mut self, requested: &'a str) -> Self {
        self.requested = Some(requested);
        self
    }

    #[must_use]
    pub const fn pinned_root(mut self, pinned_root: &'a Path) -> Self {
        self.pinned_root = Some(pinned_root);
        self
    }
}

/// The one-line-per-repo hint every refusal carries, so the caller can act without guessing.
fn registry_hint(registry: &Registry) -> String {
    if registry.repos.is_empty() {
        return "sangkar repo add <path>".to_owned();
    }
    let mut hint = String::from("Registered repos:");
    for repo in &registry.repos {
        hint.push_str("\n  ");
        hint.push_str(&repo.to_string_lossy());
    }
    hint
}

/// A request is a path unless it starts with `~`, which [`canonical_root`] must expand itself.
fn against_cwd(root: &str, cwd: &Path) -> PathBuf {
    if root.starts_with("~/") {
        PathBuf::from(root)
    } else {
        config::resolve_path(cwd, Path::new(root))
    }
}

/// Resolves the repository a call is about. The order is the whole contract:
///
/// 1. a supplied `requested` — matched as a path, then as a basename — always wins, because an
///    explicit request that silently fell back to a pin is a wrong answer, not a default;
/// 2. otherwise a non-empty `pinned_root` wins without reading the registry, which keeps ad-hoc
///    invocations (`--repo-root <unregistered tree>`) working;
/// 3. otherwise the working directory, then the only registered repo, then a refusal.
///
/// Both refusals use the existing closed union, no new code: `not_found` when a name resolves to
/// nothing, `ambiguous` when several entries could be meant. The hint is the registry's contents,
/// one repo per line, or the command that would register one.
///
/// # Errors
///
/// `AppError` with code `not_found` or `ambiguous`, always carrying that hint.
pub fn select_repo(input: &SelectRepoInput<'_>) -> Result<PathBuf, AppError> {
    let hint = registry_hint(input.registry);

    if let Some(requested) = input
        .requested
        .filter(|requested| !requested.trim().is_empty())
    {
        let requested_root = canonical_root(&against_cwd(requested, input.cwd), None);
        if input.registry.repos.contains(&requested_root) {
            return Ok(requested_root);
        }
        let name = requested_root
            .file_name()
            .map(std::ffi::OsStr::to_os_string);
        let by_name: Vec<&PathBuf> = input
            .registry
            .repos
            .iter()
            .filter(|repo| repo.file_name().map(std::ffi::OsStr::to_os_string) == name)
            .collect();
        match by_name.as_slice() {
            [single] => return Ok((*single).clone()),
            [] => {
                return Err(AppError::new(
                    ErrorCode::NOT_FOUND,
                    format!("repo \"{requested}\" is not registered"),
                )
                .with_hint(hint));
            }
            several => {
                let name = name.map_or_else(
                    || requested_root.to_string_lossy().into_owned(),
                    |name| name.to_string_lossy().into_owned(),
                );
                return Err(AppError::new(
                    ErrorCode::AMBIGUOUS,
                    format!("{} registered repos are named \"{name}\"", several.len()),
                )
                .with_hint(hint));
            }
        }
    }

    if let Some(pinned) = input
        .pinned_root
        .filter(|pinned| !pinned.to_string_lossy().trim().is_empty())
    {
        return Ok(canonical_root(pinned, None));
    }

    let cwd_root = canonical_root(input.cwd, None);
    if input.registry.repos.contains(&cwd_root) {
        return Ok(cwd_root);
    }
    match input.registry.repos.as_slice() {
        [] => {
            Err(AppError::new(ErrorCode::NOT_FOUND, "no repository is registered").with_hint(hint))
        }
        [only] => Ok(only.clone()),
        _ => Err(AppError::new(
            ErrorCode::AMBIGUOUS,
            format!(
                "several repositories are registered and none matches {}",
                cwd_root.display()
            ),
        )
        .with_hint(hint)),
    }
}
