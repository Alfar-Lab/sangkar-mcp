//! The daemon's vocabulary: what one process serving several repositories reports, and nothing that
//! reads or writes. Types only, so a test can build a [`DaemonStatus`] by hand.
//!
//! Ported from `src/daemon/types.ts`, with the two answers the Rust contract adds: `uptime_ms` (the
//! pid file's `startedAt` against the reader's clock, so an operator needs no date arithmetic) and
//! `report` (the report server's address, which the TypeScript's status never carried because the
//! server bound its port inside the same process that read the status).
//!
//! The daemon may import only `sangkar-core`'s vocabulary and its own modules, never
//! `sangkar-tools` — the `daemon-privileges` rule of `tests/boundary.test.ts` — and this module
//! imports nothing at all: a status answer is plain data, and a type that reached into the store or
//! the indexer would tie the shape to the writer.
//!
//! # Two shapes for one repository
//!
//! [`RepoRuntime`] is the answer `status` prints for one registered root: its root, whether it is
//! watched, and what the daemon last did there. [`RepoRecord`] is the part of that answer the
//! running daemon records, because `status` is a *different process*: it can read the registry, the
//! repo's config and the daemon's state file, but it cannot read the reconciler's memory. The loop
//! therefore persists a [`DaemonState`] — one [`RepoRecord`] per root plus the report address —
//! and `status` merges it with the files it can read itself.

use serde::{Deserialize, Serialize};

/// One registered repository as `status` reports it: where it is, and what the daemon last did there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RepoRuntime {
    /// The canonical absolute root, as the registry spells it.
    pub root: String,
    /// Whether this repository has a live watcher, which is a per-repo decision (its own config,
    /// narrowed by the machine file), not a global one.
    pub watch: bool,
    /// When the daemon last finished a reconcile pass for this root, from the daemon's own clock;
    /// `None` when no daemon has recorded one on this home since the last start.
    pub last_run: Option<String>,
    /// The exit code of the last index run the daemon watched for this root. `None` is "no run
    /// recorded", never "success": `Some(0)` is the published case.
    pub last_exit: Option<i32>,
    /// The graph generation that run published, as `meta.generation` spells it; `None` when the
    /// root has no published graph yet.
    pub generation: Option<String>,
}

/// The part of one repository's runtime the running daemon records for a later `status`.
///
/// Kept apart from [`RepoRuntime`] on purpose: `watch` is derived from the repo's config file, and a
/// record that carried it would let a daemon's stale opinion outrank a file an operator just edited.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RepoRecord {
    pub root: String,
    pub last_run: Option<String>,
    pub last_exit: Option<i32>,
    pub generation: Option<String>,
}

/// Where the daemon's report answers. Filled by the composition root from the server it bound
/// (`TASK-054`'s `serve`), never guessed from the config: a daemon started with `--port 0` serves
/// on the port the kernel chose, and a status that printed the configured one would be a lie.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReportAddress {
    /// The full URL, `http://<host>:<port>`, as the ready line carries it.
    pub url: String,
    pub host: String,
    pub port: u16,
}

/// What the running daemon has recorded in `<home>/daemon.state.json`.
///
/// A record, not a live claim: it survives the daemon, so a stopped home can still say what the last
/// run did. `status` reads [`Self::report`] only while the pid lock is live, because an address
/// nobody is listening on is the one field an operator would act on wrongly.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DaemonState {
    pub report: Option<ReportAddress>,
    pub repositories: Vec<RepoRecord>,
}

/// The whole answer to "is the daemon up, and what is it serving".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatus {
    /// `None` before the process has bound one, and for a lock whose holder is gone or whose
    /// heartbeat aged: `status` answers what an operator means by "running", not what a file says.
    pub pid: Option<u32>,
    /// ISO-8601, from the daemon's own clock; `None` until it has started serving.
    pub started_at: Option<String>,
    /// Milliseconds since [`Self::started_at`], or `None` when the daemon is not running (or its
    /// recorded start time is unreadable, or the clock moved backwards).
    pub uptime_ms: Option<u64>,
    /// One row per registered root, in the registry's order.
    pub repositories: Vec<RepoRuntime>,
    /// The report server's address while a daemon is up and serving one.
    pub report: Option<ReportAddress>,
}

#[cfg(test)]
mod tests {
    use super::{DaemonState, DaemonStatus, RepoRecord, ReportAddress};

    /// `status --json` prints this object, so the key spelling is a contract with the CLI: camelCase
    /// like the TypeScript's `DaemonStatus`, and a field that is absent is `null` rather than a
    /// missing key, which is what `JSON.stringify` does with the TypeScript's `null` answer.
    #[test]
    fn the_status_json_is_the_camel_case_object_with_nulls() {
        let status = DaemonStatus {
            pid: None,
            started_at: None,
            uptime_ms: None,
            repositories: Vec::new(),
            report: None,
        };
        assert_eq!(
            serde_json::to_string(&status).expect("a serialisable status"),
            r#"{"pid":null,"startedAt":null,"uptimeMs":null,"repositories":[],"report":null}"#
        );
    }

    /// The daemon's own file is read back by its own `status`, so the shape has to round-trip; a
    /// hand-edited file that drops a key is the partial record, not a refusal.
    #[test]
    fn the_daemon_state_round_trips_and_tolerates_a_partial_record() {
        let state = DaemonState {
            report: Some(ReportAddress {
                url: "http://127.0.0.1:7317".to_owned(),
                host: "127.0.0.1".to_owned(),
                port: 7317,
            }),
            repositories: vec![RepoRecord {
                root: "/repo".to_owned(),
                last_run: Some("2026-01-01T00:00:00.000Z".to_owned()),
                last_exit: Some(0),
                generation: Some("g-1".to_owned()),
            }],
        };
        let json = serde_json::to_string(&state).expect("a serialisable state");
        assert_eq!(
            json,
            r#"{"report":{"url":"http://127.0.0.1:7317","host":"127.0.0.1","port":7317},"repositories":[{"root":"/repo","lastRun":"2026-01-01T00:00:00.000Z","lastExit":0,"generation":"g-1"}]}"#
        );
        assert_eq!(
            serde_json::from_str::<DaemonState>(&json).expect("its own bytes"),
            state
        );
        assert_eq!(
            serde_json::from_str::<DaemonState>(r#"{"repositories":[{"root":"/repo"}]}"#)
                .expect("a partial record"),
            DaemonState {
                report: None,
                repositories: vec![RepoRecord {
                    root: "/repo".to_owned(),
                    last_run: None,
                    last_exit: None,
                    generation: None,
                }],
            }
        );
    }
}
