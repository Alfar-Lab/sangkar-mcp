//! The one decision "record or not, and where", per repository.
//!
//! Ported from `src/shared/telemetry/for-repo.ts`. A daemon serves several trees from one process, so
//! the recorder cannot be built from the machine configuration alone: the consent switch is
//! machine-level, but the directory, the root and the repo's own `telemetry` flag belong to the tree.
//! Keeping the precedence here — one call to [`recording_enabled`] plus the `create`/`null` fork —
//! means the composition root never re-derives it, and the rule itself has exactly one owner in
//! [`sangkar_core::repo_config`].
//!
//! There is no environment read: `consent` is the switch `sangkar_core::config` already resolved,
//! and that module is the only reader of the environment.
//!
//! The input is owned rather than borrowed because the clock it may carry is a boxed [`Now`] and a
//! box cannot be moved out of a shared reference; the TypeScript's `input` is a handle and passes
//! the same facts by reference for free.

use sangkar_core::repo_config::{RepoConfig, recording_enabled};

use crate::record::SessionMode;
use crate::telemetry::{Now, Recorder, TelemetryOptions, create, null};

/// The directory name `sangkar_core::config` computes for `TelemetryConfig::dir`: `<dataDir>/telemetry`.
/// It is not exported there, so it is restated once, here, for the per-repository case.
const TELEMETRY_DIR_NAME: &str = "telemetry";

/// Everything [`telemetry_for_repo`] decides from, with every gate already resolved by a caller.
pub struct RepoTelemetryInput {
    /// Machine-level: consent lives in `AppConfig::telemetry.enabled`.
    pub consent: bool,
    pub repo: RepoConfig,
    /// Registration is part of the precedence, so an unregistered tree is never recorded.
    pub registered: bool,
    pub session_mode: SessionMode,
    pub server_version: String,
    pub graph_schema_version: u64,
    pub now: Option<Now>,
}

/// A recorder for `input.repo`, or the same interface with no destination when the precedence says
/// no. The directory is the repo's own data dir — not `AppConfig::telemetry.dir`, which a daemon that
/// swapped trees through `AppConfig::with_repo` would have pointed at the wrong one.
#[must_use]
pub fn telemetry_for_repo(input: RepoTelemetryInput) -> Recorder {
    let RepoTelemetryInput {
        consent,
        repo,
        registered,
        session_mode,
        server_version,
        graph_schema_version,
        now,
    } = input;

    if !recording_enabled(consent, &repo, registered) {
        return null();
    }

    create(TelemetryOptions {
        dir: repo.data_dir.join(TELEMETRY_DIR_NAME),
        repo_root: repo.repo_root,
        data_dir: repo.data_dir,
        session_mode,
        server_version,
        graph_schema_version,
        sink: None,
        now,
    })
}
