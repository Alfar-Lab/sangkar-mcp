//! The single-writer lock. `flock` does not exist on Bun, so the lock is a create-exclusive file:
//! `create_new(true)` fails when someone else holds it, and the content says who. Nothing is
//! shelled out — liveness is the recorded pid and the heartbeat is the file's own mtime, so the
//! lock survives a machine without `kill` and needs no further dependency.
//!
//! Ported from `src/shared/store/lock.ts`. Two processes must never both believe they hold it,
//! hence the reclaim rule is narrow: a lock is stale only when its recorded pid is gone or its
//! mtime is older than [`STALE_AFTER_MS`]. A lock whose content cannot be parsed (its holder died
//! between the create and the write) is reclaimed by the age rule alone, so the microseconds-long
//! window of a healthy acquire is never mistaken for a stale lock.

use std::fs::{self, FileTimes, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::errors::{AppError, ErrorCode};
use sangkar_core::log;

/// The lock file lives in the data dir, outside the database (`DATABASE.md` §4).
const LOCK_FILE: &str = "write.lock";

/// How old the heartbeat may get before the lock is treated as abandoned. The indexer refreshes
/// the mtime with [`heartbeat`] while it works, so a longer run renews rather than expires.
pub const STALE_AFTER_MS: u64 = 60_000;

/// A contended call is worth retrying once the journal-mode lock inside SQLite has cleared.
const RETRY_AFTER_MS: u64 = 1_000;

/// Reclaims are races by nature: re-check the file a few times before reporting contention.
const ACQUIRE_ATTEMPTS: u32 = 3;

const CONTENDED_HINT: &str =
    "wait for the running indexer to finish, or delete the lock if you know it is dead";
const ATTEMPTS_HINT: &str = "another indexer keeps winning the create; retry once it exits";

/// A held lock.
///
/// The fields are private so only [`acquire_lock`] can mint one: [`release`] trusts the recorded
/// pid, and a handle a caller could edit is a handle that can unlink somebody else's lock.
#[derive(Debug)]
pub struct LockHandle {
    path: PathBuf,
    pid: u32,
    started_at: String,
}

impl LockHandle {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// ISO-8601, from the holder's clock; kept for diagnostics, never compared across hosts.
    #[must_use]
    pub fn started_at(&self) -> &str {
        &self.started_at
    }
}

/// What [`inspect`] saw, without creating or removing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockState {
    pub held: bool,
    /// The recorded holder, or `None` when the file is absent or its content is unreadable.
    pub pid: Option<u32>,
    pub stale: bool,
}

impl LockState {
    fn unheld() -> Self {
        Self {
            held: false,
            pid: None,
            stale: false,
        }
    }
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

/// `<dataDir>/write.lock`.
#[must_use]
pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join(LOCK_FILE)
}

/// Reports the current holder without creating or removing anything. Safe to call from any
/// process; the caller decides what a held lock means.
pub fn inspect(data_dir: &Path) -> Result<LockState, AppError> {
    let path = lock_path(data_dir);

    let modified = match fs::metadata(&path) {
        Ok(metadata) => metadata.modified().map_err(|error| {
            internal(format!(
                "cannot read the mtime of {}: {error}",
                path.display()
            ))
        })?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(LockState::unheld()),
        Err(error) => {
            return Err(internal(format!("cannot stat {}: {error}", path.display())));
        }
    };

    let raw = match fs::read(&path) {
        Ok(bytes) => bytes,
        // The holder released between the `stat` and the `read`; that is an unheld lock, not a
        // fault.
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(LockState::unheld()),
        Err(error) => {
            return Err(internal(format!("cannot read {}: {error}", path.display())));
        }
    };

    // Node's `readFile(path, "utf8")` decodes invalid bytes with replacement characters instead of
    // failing, and a torn body is a case this module must classify rather than crash on.
    let pid = parse_pid(&String::from_utf8_lossy(&raw));
    let age = age_ms(modified);
    let stale = match pid {
        None => age > STALE_AFTER_MS,
        Some(pid) => !is_alive(pid) || age > STALE_AFTER_MS,
    };

    Ok(LockState {
        held: true,
        pid,
        stale,
    })
}

/// Takes the lock, or fails with `index_locked` when a live holder owns it. A stale lock is
/// removed, logged as a warning, and taken in the same call: a crashed indexer must not brick the
/// repository.
pub fn acquire_lock(data_dir: &Path) -> Result<LockHandle, AppError> {
    let path = lock_path(data_dir);
    let handle = LockHandle {
        path: path.clone(),
        pid: std::process::id(),
        started_at: iso8601_millis(epoch_millis()),
    };
    let body = lock_body(&handle);

    for _attempt in 1..=ACQUIRE_ATTEMPTS {
        match create_exclusive(&path, &body) {
            // The exclusive create is the whole lock: it is atomic, so exactly one process wins
            // the race and the loser learns it from `EEXIST`.
            Ok(()) => return Ok(handle),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(internal(format!(
                    "cannot create {}: {error}",
                    path.display()
                )));
            }
        }

        let state = inspect(data_dir)?;
        if !state.held {
            // Released between our failed create and the inspect; try the create again.
            continue;
        }
        if !state.stale {
            let holder = state
                .pid
                .map(|pid| format!(" (pid {pid})"))
                .unwrap_or_default();
            return Err(index_locked(
                format!("another indexer holds {}{holder}", path.display()),
                CONTENDED_HINT,
            ));
        }

        log::warn(
            "reclaiming stale write lock",
            &[
                ("path", path.display().to_string().into()),
                ("holderPid", state.pid.into()),
                ("staleAfterMs", STALE_AFTER_MS.into()),
            ],
        );
        match fs::remove_file(&path) {
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

/// Releases the lock, but only while this handle's pid is still the recorded holder: if the lock
/// was reclaimed by someone else (this process was wrongly judged dead), removing it would give
/// two writers the same repository.
pub fn release(handle: &LockHandle) -> Result<(), AppError> {
    let raw = match fs::read(&handle.path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(internal(format!(
                "cannot read {}: {error}",
                handle.path.display()
            )));
        }
    };

    if parse_pid(&raw) != Some(handle.pid) {
        log::warn(
            "write lock is held by another process; not removing it",
            &[
                ("path", handle.path.display().to_string().into()),
                ("pid", handle.pid.into()),
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

/// Refreshes the lock's mtime, which is the heartbeat [`inspect`] reads. A holder that runs longer
/// than [`STALE_AFTER_MS`] must call this, or its own lock can be reclaimed underneath it.
pub fn heartbeat(handle: &LockHandle) -> Result<(), AppError> {
    let now = SystemTime::now();
    let file = match OpenOptions::new().write(true).open(&handle.path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            log::warn(
                "write lock disappeared while heartbeating",
                &[("path", handle.path.display().to_string().into())],
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

/// The lock body: `JSON.stringify(handle)`, key order and all, written by hand because this crate
/// carries no JSON library and the file is read by both implementations.
fn lock_body(handle: &LockHandle) -> String {
    format!(
        r#"{{"path":{},"pid":{},"startedAt":"{}"}}"#,
        json_string(&handle.path.to_string_lossy()),
        handle.pid,
        handle.started_at
    )
}

/// `JSON.stringify`'s string escaping: `"` and `\` are escaped, the five control characters with a
/// short form get it, every other control character becomes `\u00xx`, and everything else — `/`,
/// DEL, non-ASCII — stays raw.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{0c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// The `pid` of a lock body, by the rule the TypeScript `parsePid` applies: the content must be an
/// object whose `pid` is a positive integer. Anything else — a torn write, a hand-edited file, a
/// body written by another version — reads as `None`, and [`inspect`] falls back to the age rule.
///
/// A hand scanner, because this crate carries no JSON library: it accepts exactly the shape
/// [`lock_body`] writes, an object of string and number values, and answers `None` for anything
/// else — the same answer a `JSON.parse` failure gives.
fn parse_pid(raw: &str) -> Option<u32> {
    let mut scanner = Scanner::new(raw);
    scanner.expect(b'{')?;
    let mut pid = None;
    loop {
        scanner.skip_whitespace();
        match scanner.peek()? {
            b'}' => return pid,
            b',' => {
                scanner.bump()?;
            }
            b'"' => {
                let key = scanner.string()?;
                scanner.expect(b':')?;
                scanner.skip_whitespace();
                let value = scanner.value()?;
                if key == "pid"
                    && let Value::Integer(recorded) = value
                {
                    pid = Some(recorded);
                }
            }
            _ => return None,
        }
    }
}

/// A lock body's value, reduced to the only distinction the pid rule makes.
enum Value {
    /// A number that is a positive integer, the only shape a pid may take.
    Integer(u32),
    Other,
}

struct Scanner<'a> {
    raw: &'a str,
    at: usize,
}

impl<'a> Scanner<'a> {
    fn new(raw: &'a str) -> Self {
        Self { raw, at: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.raw.as_bytes().get(self.at).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn bump_char(&mut self) -> Option<char> {
        // `at` only ever lands on a char boundary: it advances one byte for ASCII punctuation and
        // one whole char when the input is not ASCII.
        let ch = self.raw.get(self.at..)?.chars().next()?;
        self.at += ch.len_utf8();
        Some(ch)
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Option<()> {
        self.skip_whitespace();
        (self.bump()? == byte).then_some(())
    }

    fn string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.bump_char()? {
                '"' => return Some(out),
                '\\' => match self.bump_char()? {
                    '"' => out.push('"'),
                    '\\' => out.push('\\'),
                    '/' => out.push('/'),
                    'b' => out.push('\u{08}'),
                    'f' => out.push('\u{0c}'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    't' => out.push('\t'),
                    'u' => {
                        // A `\u` escape cannot spell a key this module compares, and the string is
                        // discarded after the comparison, so the digits are consumed, not decoded.
                        for _ in 0..4 {
                            self.bump_char()?;
                        }
                        out.push('\u{fffd}');
                    }
                    _ => return None,
                },
                control if (control as u32) < 0x20 => return None,
                ch => out.push(ch),
            }
        }
    }

    fn value(&mut self) -> Option<Value> {
        match self.peek()? {
            b'"' => {
                self.string()?;
                Some(Value::Other)
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => None,
        }
    }

    fn number(&mut self) -> Option<Value> {
        let negative = self.peek() == Some(b'-');
        if negative {
            self.bump()?;
        }

        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        if self.at == start {
            return None;
        }

        let mut integral = true;
        if self.peek() == Some(b'.') {
            integral = false;
            self.bump()?;
            let fraction = self.at;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == fraction {
                return None;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            integral = false;
            self.bump()?;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump()?;
            }
            let exponent = self.at;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
            if self.at == exponent {
                return None;
            }
        }

        if negative || !integral {
            return Some(Value::Other);
        }
        // A number that does not fit a `u32` is not a pid this host can have: it reads as
        // unparsable and the age rule decides, which is where a dead holder also lands.
        match self.raw[start..self.at].parse::<u32>() {
            Ok(pid) if pid > 0 => Some(Value::Integer(pid)),
            _ => Some(Value::Other),
        }
    }
}

/// `kill(pid, 0)` by the only route this crate has: whether `/proc/<pid>` exists.
///
/// `sangkar-store` depends on `sangkar-core` and `rusqlite` only, so it may not call libc's
/// `kill`, and the migration ships the Linux x64 target (the plan's maintainer decision), where a
/// pid has a `/proc` entry exactly while it is alive. The errno rule is preserved rather than
/// reduced to existence: only a *missing* entry counts as dead, so any other error — the `EPERM`
/// of another user's pid, for instance — stays alive, exactly as the TypeScript treats every errno
/// except `ESRCH`.
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

/// The exclusive create *is* the lock; `create_new` is `O_CREAT | O_EXCL`, so the failure that
/// means contention is `AlreadyExists` and every other one is a real I/O error.
fn create_exclusive(path: &Path, body: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    // A write that fails leaves the short body behind; `inspect` reads it as unparsable and the age
    // rule reclaims it after [`STALE_AFTER_MS`], which is the same window a crashed holder gets.
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
/// `sangkar_core::log`'s formatter is private, and this crate may not add a date library, so the
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
    use super::{iso8601_millis, json_string, parse_pid};

    /// Every expectation is `new Date(millis).toISOString()` under Bun 1.4.4, the oracle the
    /// parity bar names — the same fixtures `sangkar_core::log` pins its private formatter with.
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

    #[test]
    fn a_pid_is_a_positive_integer_inside_an_object() {
        let cases: [(&str, Option<u32>); 11] = [
            (
                r#"{"path":"/tmp/a","pid":4242,"startedAt":"2024-01-01T00:00:00.000Z"}"#,
                Some(4242),
            ),
            (r#"{"pid":7}"#, Some(7)),
            (r#"{"pid": 7 }"#, Some(7)),
            // A quoted `"pid":` inside the path value is data, not a key.
            (r#"{"path":"/tmp/\"pid\":9/x","pid":11}"#, Some(11)),
            (r#"{"path":"/a/b"}"#, None),
            (r#"{"pid":0}"#, None),
            (r#"{"pid":-5}"#, None),
            (r#"{"pid":"42"}"#, None),
            (r#"{"pid":42.5}"#, None),
            // A torn body from a holder that died mid-create.
            (r#"{"path":"/x","pid":"#, None),
            ("", None),
        ];
        for (raw, expected) in cases {
            assert_eq!(parse_pid(raw), expected, "{raw:?}");
        }
    }

    #[test]
    fn a_lock_body_is_escaped_the_way_json_stringify_escapes_it() {
        assert_eq!(json_string("/tmp/a b/graph.db"), r#""/tmp/a b/graph.db""#);
        assert_eq!(
            json_string("/tmp/quote\"and\\slash"),
            r#""/tmp/quote\"and\\slash""#
        );
        assert_eq!(
            json_string("tab\tnl\ncr\rbs\u{8}ff\u{c}"),
            r#""tab\tnl\ncr\rbs\bff\f""#
        );
        assert_eq!(json_string("ctl\u{1}"), r#""ctl\u0001""#);
        // `/`, DEL and non-ASCII are emitted raw, as `JSON.stringify` does.
        assert_eq!(json_string("a/b\u{7f}é"), "\"a/b\u{7f}é\"");
    }
}
