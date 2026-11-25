//! The two identifiers every telemetry record carries.
//!
//! Ported from `src/shared/telemetry/identity.ts`. Both are random bytes: nothing about the
//! repository, the machine or the user may leak into a record, so neither is derived from them.
//! There is deliberately no fallback to a machine id, a MAC address or a hash of the repository
//! path — a directory hash would make the id stable across days and re-link the very records the
//! daily rotation exists to separate.
//!
//! No clock: the caller passes the UTC day it already derived from the session's own timestamp with
//! [`utc_date_of`], so this module holds no time source and a test can place "today" exactly.

use std::collections::hash_map::RandomState;
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const FILE_NAME: &str = "install_id.json";

/// 16 bytes, the length `randomBytes(16).toString("hex")` produced: 32 hex characters.
const ID_BYTES: usize = 16;

/// The ten characters of `YYYY-MM-DD` at the head of an ISO-8601 UTC timestamp.
const DAY_CHARS: usize = 10;

/// What [`resolve_install_id`] resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallIdentity {
    /// Stable for one UTC day, unlinkable from one day to the next.
    pub install_id: String,
    /// True when this process wrote today's `install_id.json`; the session record reports it.
    pub is_first_run: bool,
}

/// What `<dir>/install_id.json` holds, after the shape check in [`read_id`].
struct InstallFile {
    install_id: String,
    date: String,
}

/// What the exclusive create did.
enum WriteOutcome {
    Created,
    Exists,
    Failed,
}

/// The file-name form of a timestamp in UTC, so every process on a host agrees on "today".
///
/// `new Date(ts).toISOString().slice(0, 10)`: the record's `ts` is always the canonical form this
/// workspace writes, so its day is its first ten characters — the same ten `report.ts`'s own
/// `dayOf` groups by.
///
/// `None` when `ts` does not begin with `YYYY-MM-DD`. That is the one place this port refuses where
/// the TypeScript would throw (`new Date(garbage).toISOString()` raises `RangeError`): a name that is
/// not a day must never reach a file path, so the caller has to decide what to do with a record it
/// cannot date.
#[must_use]
pub fn utc_date_of(ts: &str) -> Option<&str> {
    let prefix = ts.get(..DAY_CHARS)?;
    let shaped = prefix.as_bytes().iter().enumerate().all(|(index, byte)| {
        if index == 4 || index == 7 {
            *byte == b'-'
        } else {
            byte.is_ascii_digit()
        }
    });
    shaped.then_some(prefix)
}

/// Resolves today's install id, or mints one for `today`.
///
/// `today` is the UTC day (`YYYY-MM-DD`) the caller derived from the session's own timestamp. A stale
/// or unparseable file is rotated: it is removed first, because the exclusive create cannot replace
/// it — without that removal, the first start of a new day would look like a lost race and keep
/// yesterday's id.
#[must_use]
pub fn resolve_install_id(dir: &Path, today: &str) -> InstallIdentity {
    let file = dir.join(FILE_NAME);

    if let Some(current) = read_id(&file)
        && current.date == today
    {
        return InstallIdentity {
            install_id: current.install_id,
            is_first_run: false,
        };
    }

    // Removing is a no-op when there is no file, and an unremovable file fails the create below for
    // the same reason, so the result is deliberately dropped.
    let _ = fs::remove_file(&file);

    let minted = mint_id();
    if matches!(write_id(&file, dir, &minted, today), WriteOutcome::Created) {
        return InstallIdentity {
            install_id: minted,
            is_first_run: true,
        };
    }

    // Another process won the create between our read and our write: its id is the day's id. A
    // write that failed (read-only data dir, full disk) must not stop a process from running — the
    // in-memory id is used and the cost is one day's grouping, never an answer.
    match read_id(&file) {
        Some(winner) => InstallIdentity {
            install_id: winner.install_id,
            is_first_run: false,
        },
        None => InstallIdentity {
            install_id: minted,
            is_first_run: false,
        },
    }
}

/// One id per process, never persisted, so it cannot be joined across runs.
#[must_use]
pub fn new_session_id() -> String {
    mint_id()
}

/// `null` in the TypeScript: every failure — absent, unreadable, torn or foreign JSON — means
/// "rotate".
fn read_id(file: &Path) -> Option<InstallFile> {
    let raw = fs::read(file).ok()?;
    let parsed: Value = serde_json::from_slice(&raw).ok()?;
    let record = parsed.as_object()?;
    let install_id = record.get("install_id")?.as_str()?;
    if install_id.is_empty() {
        return None;
    }
    let date = record.get("date")?.as_str()?;
    Some(InstallFile {
        install_id: install_id.to_owned(),
        date: date.to_owned(),
    })
}

/// `create_new` is `O_CREAT | O_EXCL` (`wx`), the race breaker: of two processes starting on the same
/// day exactly one creates the file, and the loser learns it lost from `AlreadyExists` instead of
/// overwriting the winner's id.
fn write_id(file: &Path, dir: &Path, install_id: &str, date: &str) -> WriteOutcome {
    if fs::create_dir_all(dir).is_err() {
        return WriteOutcome::Failed;
    }
    // `JSON.stringify({ install_id, date })` plus the newline `writeFileSync` was handed.
    let body = format!("{{\"install_id\":\"{install_id}\",\"date\":\"{date}\"}}\n");
    match OpenOptions::new().write(true).create_new(true).open(file) {
        Ok(mut handle) => match handle.write_all(body.as_bytes()) {
            Ok(()) => WriteOutcome::Created,
            // A short write leaves a file whose content does not parse; the next start rotates it.
            Err(_) => WriteOutcome::Failed,
        },
        Err(error) if error.kind() == ErrorKind::AlreadyExists => WriteOutcome::Exists,
        Err(_) => WriteOutcome::Failed,
    }
}

/// [`ID_BYTES`] random bytes as lowercase hex — the spelling `randomBytes(16).toString("hex")`
/// produces.
///
/// `rand` and `getrandom` are not dependencies, so the entropy comes from the standard library's one
/// per-instance OS-seeded source. `RandomState::new()` seeds each hasher from the operating system
/// and is documented to make two instances produce different keys, which is what keeps two installs
/// from sharing an id. The clock, the pid and a stack address are mixed in because the fallback must
/// not rest on a single property of one hasher: a process that forked would repeat the pid space, two
/// threads a same-instant clock, and ASLR a same address. The counter is mixed in last so two mints
/// in one process differ even when those three coincide — an install id and a session id that
/// collided would join the records this module exists to separate. Same technique as
/// `sangkar_core::config`'s cursor-key fallback.
fn mint_id() -> String {
    static MINTS: AtomicU64 = AtomicU64::new(0);

    let state = RandomState::new();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let probe = 0u8;
    let address = std::ptr::addr_of!(probe) as usize;
    let mint = MINTS.fetch_add(1, Ordering::Relaxed);

    let mut bytes = [0u8; ID_BYTES];
    for (block, chunk) in bytes.chunks_mut(8).enumerate() {
        let mut hasher = state.build_hasher();
        u64::try_from(block).unwrap_or(u64::MAX).hash(&mut hasher);
        nanos.hash(&mut hasher);
        pid.hash(&mut hasher);
        address.hash(&mut hasher);
        mint.hash(&mut hasher);
        chunk.copy_from_slice(&hasher.finish().to_le_bytes());
    }
    hex(&bytes)
}

/// Lowercase hex, the spelling `randomBytes(16).toString("hex")` produces.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}
