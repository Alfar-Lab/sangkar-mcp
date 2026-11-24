//! The daemon's pid lock: the file that makes "exactly one daemon per home" provable.
//!
//! Ported from `src/shared/pidlock.ts`, which mirrors the writer lock (`sangkar-store`'s `lock.rs`)
//! for the same reason: a create-exclusive file is the whole lock, so nothing is shelled out and no
//! further dependency is needed. The two files stay separate on purpose — `write.lock` guards one
//! repository's writer, `daemon.pid` guards the machine's singleton — because one shared
//! implementation would make the writer and the daemon contend for the same file.
//!
//! The reclaim rule is the writer's rule unchanged: a lock is stale only when its recorded pid is
//! gone or its mtime is older than [`PID_STALE_AFTER_MS`]. A file whose content cannot be parsed (a
//! holder that died between the create and the write) is reclaimed by age alone, so the
//! microseconds-long window of a healthy acquire is never mistaken for an abandoned daemon.

use std::fs::{self, FileTimes, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::errors::{AppError, ErrorCode};
use crate::log;

/// How old the heartbeat may get before the pid is treated as abandoned. The daemon refreshes the
/// mtime through [`heartbeat_pid_lock`] on every tick, so a long-running daemon renews rather than
/// expires — and a machine that slept past this window is still rescued by the dead-pid rule.
pub const PID_STALE_AFTER_MS: u64 = 60_000;

/// A contended start is worth retrying once the previous daemon has finished exiting.
const RETRY_AFTER_MS: u64 = 1_000;

/// The create, then one retry after a reclaim: the contract's "unlink then one retry".
const ACQUIRE_ATTEMPTS: u32 = 2;

const CONTENDED_HINT: &str =
    "stop it with `sangkar daemon stop`, or delete the pid file if you know it is dead";
const ATTEMPTS_HINT: &str = "another process keeps winning the create; retry once it exits";

/// Where a lock problem is reported: [`crate::log::warn`] in production, a collector in a test, so
/// "exactly one warning" is an assertion rather than a scrape of stderr. The shape is the one
/// [`crate::registry`] established.
pub type Warn<'a> = &'a dyn Fn(&str, &[(&str, Value)]);

/// A held lock.
///
/// The fields are private so only [`acquire_pid_lock`] can mint one: [`release_pid_lock`] trusts the
/// recorded pid, and a handle a caller could edit is a handle that can unlink a live daemon's file.
#[derive(Debug)]
pub struct PidLockHandle {
    path: PathBuf,
    pid: u32,
    started_at: String,
}

impl PidLockHandle {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// ISO-8601, from the holder's clock; kept for `status`, never compared across hosts.
    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }
}

/// What [`read_pid_lock`] saw, without creating or removing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PidLockState {
    pub held: bool,
    /// The recorded holder, or `None` when the file is absent or its content is unreadable.
    pub pid: Option<u32>,
    pub stale: bool,
    /// The recorded start time, or `None` when the content could not be parsed.
    pub started_at: Option<String>,
}

impl PidLockState {
    fn unheld() -> Self {
        Self {
            held: false,
            pid: None,
            stale: false,
            started_at: None,
        }
    }
}

/// The `pid` and optional `startedAt` a file records.
struct PidRecord {
    pid: u32,
    started_at: Option<String>,
}

fn internal(message: impl Into<String>) -> AppError {
    AppError::internal(message)
}

fn index_locked(message: impl Into<String>, hint: &str) -> AppError {
    AppError::new(ErrorCode::INDEX_LOCKED, message)
        .with_hint(hint)
        .with_retryable(true)
        .with_retry_after_ms(RETRY_AFTER_MS)
}

/// Reports the current holder without creating or removing anything, so `sangkar daemon status` is
/// safe on a home no daemon has ever touched.
///
/// # Errors
///
/// `AppError` with code `internal` when the file exists but cannot be stat-ed or read; an absent file
/// is an unheld lock, not a fault.
pub fn read_pid_lock(path: &Path) -> Result<PidLockState, AppError> {
    let modified = match fs::metadata(path) {
        Ok(metadata) => metadata.modified().map_err(|error| {
            internal(format!(
                "cannot read the mtime of {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(PidLockState::unheld()),
        Err(error) => return Err(internal(format!("cannot stat {}: {error}", path.display()))),
    };

    let raw = match fs::read(path) {
        Ok(bytes) => bytes,
        // The holder released between the `stat` and the `read`; that is an unheld lock, not a
        // fault.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(PidLockState::unheld()),
        Err(error) => return Err(internal(format!("cannot read {}: {error}", path.display()))),
    };

    // A torn body is a case this module must classify rather than crash on, so invalid bytes are
    // decoded with replacement characters, exactly as the TypeScript's `readFile(…, "utf8")` does.
    let record = parse_record(&String::from_utf8_lossy(&raw));
    let age = age_ms(modified);
    let stale = match &record {
        None => age > PID_STALE_AFTER_MS,
        Some(record) => !is_alive(record.pid) || age > PID_STALE_AFTER_MS,
    };

    Ok(PidLockState {
        held: true,
        pid: record.as_ref().map(|record| record.pid),
        stale,
        started_at: record.and_then(|record| record.started_at),
    })
}

/// Takes the pid lock, or fails with `index_locked` when a live daemon owns it.
pub fn acquire_pid_lock(path: &Path) -> Result<PidLockHandle, AppError> {
    acquire_pid_lock_with(path, &log::warn)
}

/// [`acquire_pid_lock`] with the warning sink injected.
///
/// A stale lock is removed, logged as a warning and taken in the same call: a daemon killed by a
/// reboot must not brick the next start. The parent directory is created because a fresh machine has
/// no home yet.
///
/// # Errors
///
/// `AppError` with code `index_locked`, `retryable: true` and `retry_after_ms: 1000` when a live
/// daemon still owns the file after both attempts (or when another process keeps winning the
/// create); code `internal` for any other filesystem failure.
pub fn acquire_pid_lock_with(path: &Path, warn: Warn<'_>) -> Result<PidLockHandle, AppError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| internal(format!("cannot create {}: {error}", parent.display())))?;
    }

    for attempt in 1..=ACQUIRE_ATTEMPTS {
        // The TypeScript carried this number only as advice in the error payload; a retry the
        // contract promises is worth actually waiting, and one second is the exit time of the daemon
        // that just released the file.
        if attempt > 1 {
            std::thread::sleep(Duration::from_millis(RETRY_AFTER_MS));
        }

        // The start time is taken per attempt, so a lock reclaimed from a dead daemon records when
        // it was actually taken rather than when the first attempt failed.
        let handle = PidLockHandle {
            path: path.to_path_buf(),
            pid: std::process::id(),
            started_at: iso8601_millis(epoch_millis()),
        };
        match create_exclusive(path, &pid_body(&handle)) {
            // The exclusive create is the whole lock: it is atomic, so exactly one daemon wins the
            // race and the loser learns it from `EEXIST`.
            Ok(()) => return Ok(handle),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(internal(format!(
                    "cannot create {}: {error}",
                    path.display()
                )));
            }
        }

        let state = read_pid_lock(path)?;
        if !state.held {
            // Released between our failed create and the read; try the create again.
            continue;
        }
        if !state.stale {
            let holder = state
                .pid
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default();
            return Err(index_locked(
                format!("another daemon holds {}{holder}", path.display()),
                CONTENDED_HINT,
            ));
        }

        warn(
            "reclaiming stale pid lock",
            &[
                ("path", Value::String(path.display().to_string())),
                ("holderPid", state.pid.map_or(Value::Null, Value::from)),
                ("staleAfterMs", Value::from(PID_STALE_AFTER_MS)),
            ],
        );
        match fs::remove_file(path) {
            Ok(()) => {}
            // Another process reclaimed it first; the next attempt sees the real state.
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => {
                return Err(internal(format!(
                    "cannot remove stale {}: {error}",
                    path.display()
                )));
            }
        }
    }

    Err(index_locked(
        format!(
            "could not take {} after {ACQUIRE_ATTEMPTS} attempts",
            path.display()
        ),
        ATTEMPTS_HINT,
    ))
}

/// Releases the lock, but only while this handle's pid is still the recorded holder.
pub fn release_pid_lock(handle: &PidLockHandle) -> Result<(), AppError> {
    release_pid_lock_with(handle, &log::warn)
}

/// [`release_pid_lock`] with the warning sink injected.
///
/// If the file was reclaimed by another daemon (this one was wrongly judged dead), removing it would
/// give two daemons the same home, so it is left alone. A missing file is success, which makes a
/// release safe to repeat.
///
/// # Errors
///
/// `AppError` with code `internal` when the file exists but cannot be read or removed.
pub fn release_pid_lock_with(handle: &PidLockHandle, warn: Warn<'_>) -> Result<(), AppError> {
    let raw = match fs::read(&handle.path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(internal(format!(
                "cannot read {}: {error}",
                handle.path.display()
            )));
        }
    };

    if parse_record(&String::from_utf8_lossy(&raw)).map(|record| record.pid) != Some(handle.pid) {
        warn(
            "pid lock is held by another process; not removing it",
            &[
                ("path", Value::String(handle.path.display().to_string())),
                ("pid", Value::from(handle.pid)),
            ],
        );
        return Ok(());
    }

    match fs::remove_file(&handle.path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(internal(format!(
            "cannot remove {}: {error}",
            handle.path.display()
        ))),
    }
}

/// Refreshes the mtime, which is the heartbeat [`read_pid_lock`] reads.
pub fn heartbeat_pid_lock(handle: &PidLockHandle) -> Result<(), AppError> {
    heartbeat_pid_lock_with(handle, &log::warn)
}

/// [`heartbeat_pid_lock`] with the warning sink injected.
///
/// A daemon that runs longer than [`PID_STALE_AFTER_MS`] must call this on every tick, or its own lock
/// looks abandoned. A file that has vanished is one warning rather than an error: the daemon has
/// already lost the lock and the loss is what the next `status` reports.
///
/// # Errors
///
/// `AppError` with code `internal` when the file exists but its timestamps cannot be set.
pub fn heartbeat_pid_lock_with(handle: &PidLockHandle, warn: Warn<'_>) -> Result<(), AppError> {
    let now = SystemTime::now();
    let file = match OpenOptions::new().write(true).open(&handle.path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            warn(
                "pid lock disappeared while heartbeating",
                &[("path", Value::String(handle.path.display().to_string()))],
            );
            return Ok(());
        }
        Err(error) => {
            return Err(internal(format!(
                "cannot open {} to heartbeat: {error}",
                handle.path.display()
            )));
        }
    };

    file.set_times(FileTimes::new().set_accessed(now).set_modified(now))
        .map_err(|error| {
            internal(format!(
                "cannot heartbeat {}: {error}",
                handle.path.display()
            ))
        })
}

/// The lock body: `JSON.stringify({pid, startedAt})`, key order and all. Hand-written because every
/// part is a number or the ISO form this module generated, so there is nothing to escape — and the
/// bytes are the contract a reader on either side parses.
fn pid_body(handle: &PidLockHandle) -> String {
    format!(
        r#"{{"pid":{},"startedAt":"{}"}}"#,
        handle.pid, handle.started_at
    )
}

/// The record a body holds, by the rule the TypeScript `parseRecord` applies: an object whose `pid` is
/// a positive integer, with `startedAt` a string or absent. Anything else — a torn write, a
/// hand-edited file, a body written by another version — reads as `None`, and [`read_pid_lock`] falls
/// back to the age rule.
fn parse_record(raw: &str) -> Option<PidRecord> {
    let parsed: Value = serde_json::from_str(raw).ok()?;
    let record = parsed.as_object()?;
    let pid = record.get("pid")?;
    let pid = pid.as_u64().and_then(|pid| u32::try_from(pid).ok())?;
    if pid == 0 {
        return None;
    }
    Some(PidRecord {
        pid,
        started_at: record
            .get("startedAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// `kill(pid, 0)` by the only route this crate has: whether `/proc/<pid>` exists.
///
/// No dependency may be added and `libc` is not one, and the migration ships the Linux x64 target (the
/// plan's maintainer decision), where a pid has a `/proc` entry exactly while it is alive. The errno
/// rule is preserved rather than reduced to existence: only a *missing* entry counts as dead, so any
/// other error — the `EPERM` of another user's pid, or a `hidepid` mount that keeps the directory
/// unreadable — stays alive, exactly as the TypeScript treats every errno except `ESRCH`.
fn is_alive(pid: u32) -> bool {
    match fs::metadata(format!("/proc/{pid}")) {
        Ok(_) => true,
        Err(error) => error.kind() != ErrorKind::NotFound,
    }
}

fn age_ms(modified: SystemTime) -> u64 {
    match SystemTime::now().duration_since(modified) {
        Ok(age) => u64::try_from(age.as_millis()).unwrap_or(u64::MAX),
        // A clock that moved backwards leaves a future mtime; the lock then looks as fresh as it
        // claims to be, so the age rule cannot be the one that reclaims it.
        Err(_) => 0,
    }
}

/// `create_new` is `O_CREAT | O_EXCL`, so the failure that means contention is `AlreadyExists` and
/// every other one is a real I/O error.
fn create_exclusive(path: &Path, body: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    // A write that fails leaves the short body behind; `read_pid_lock` reads it as unparsable and the
    // age rule reclaims it after [`PID_STALE_AFTER_MS`], which is the same window a crashed holder
    // gets.
    file.write_all(body.as_bytes())
}

fn epoch_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(_) => 0,
    }
}

/// `new Date(millis).toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// `sangkar_core::log`'s formatter is private and this crate may not add a date library, so the
/// algorithm is reproduced here — the same ordinal arithmetic `log.rs` uses, for a byte-identical
/// `startedAt`. Both are pinned by the same Bun fixtures.
fn iso8601_millis(millis: i64) -> String {
    let seconds = millis.div_euclid(1_000);
    let subsecond = millis.rem_euclid(1_000);
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let second_of_day = seconds.rem_euclid(86_400);
    let (hour, minute, second) = (
        second_of_day / 3_600,
        second_of_day % 3_600 / 60,
        second_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{subsecond:03}Z")
}

/// Howard Hinnant's `civil_from_days`: the proleptic Gregorian date for a day count since
/// 1970-01-01, with the leap-year rule baked into the era arithmetic rather than branchy.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted / 146_097
    } else {
        (shifted - 146_096) / 146_097
    };
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::{PidLockHandle, iso8601_millis, parse_record, pid_body};
    use std::path::PathBuf;

    /// Every expectation is `new Date(millis).toISOString()` under Bun 1.4.4, the oracle the parity
    /// bar names — the same fixtures `sangkar_core::log` and `sangkar_store::lock` pin their copies
    /// of this formatter with.
    #[test]
    fn iso8601_matches_the_javascript_to_iso_string() {
        let cases: [(i64, &str); 9] = [
            (0, "1970-01-01T00:00:00.000Z"),
            (1, "1970-01-01T00:00:00.001Z"),
            (-1, "1969-12-31T23:59:59.999Z"),
            (-1_000, "1969-12-31T23:59:59.000Z"),
            (951_782_400_000, "2000-02-29T00:00:00.000Z"),
            (1_000_000_000_000, "2001-09-09T01:46:40.000Z"),
            (1_699_999_999_999, "2023-11-14T22:13:19.999Z"),
            (1_709_210_096_789, "2024-02-29T12:34:56.789Z"),
            (1_767_225_600_000, "2026-01-01T00:00:00.000Z"),
        ];
        for (millis, expected) in cases {
            assert_eq!(iso8601_millis(millis), expected, "millis {millis}");
        }
    }

    /// `JSON.stringify({pid, startedAt})` under Bun 1.4.4: compact, `pid` first, no trailing newline.
    #[test]
    fn the_body_is_the_javascript_stringify_bytes() {
        let handle = PidLockHandle {
            path: PathBuf::from("/machine/home/daemon.pid"),
            pid: 4_242,
            started_at: "2026-01-01T00:00:00.000Z".to_owned(),
        };
        assert_eq!(
            pid_body(&handle),
            r#"{"pid":4242,"startedAt":"2026-01-01T00:00:00.000Z"}"#
        );
    }

    #[test]
    fn a_record_needs_a_positive_integer_pid() {
        let parse = |raw: &str| parse_record(raw).map(|record| (record.pid, record.started_at));
        assert_eq!(
            parse(r#"{"pid":42,"startedAt":"2026-01-01T00:00:00.000Z"}"#),
            Some((42, Some("2026-01-01T00:00:00.000Z".to_owned())))
        );
        // `startedAt` is optional, and a string or nothing are the only two answers.
        assert_eq!(parse(r#"{"pid":42}"#), Some((42, None)));
        assert_eq!(parse(r#"{"pid":42,"startedAt":7}"#), Some((42, None)));
        // A torn body, a zero or negative pid, a fraction and a non-object all read as unparsable.
        assert_eq!(parse(""), None);
        assert_eq!(parse("not json at all"), None);
        assert_eq!(parse(r#"{"pid":0}"#), None);
        assert_eq!(parse(r#"{"pid":-1}"#), None);
        assert_eq!(parse(r#"{"pid":2.5}"#), None);
        assert_eq!(parse(r#"{"pid":"42"}"#), None);
        assert_eq!(parse("[]"), None);
    }
}
