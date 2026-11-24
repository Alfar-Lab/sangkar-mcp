//! Freshness: is the generation a reader serves still true of the tree on disk?
//!
//! Ported from `src/shared/domain/freshness.ts`. Every answer a tool gives about the graph is a
//! claim about a working tree, and the claim is only as good as the distance between the two. This
//! module is the one place that distance is measured, so `status` does not have to guess and no two
//! tools can disagree about what "stale" means.
//!
//! The ways a served generation can stop matching its tree, in the order they are checked because
//! a later check cannot be trusted once an earlier one fails:
//!
//! - `root_mismatch`: `meta.project_root` is not the root the caller is answering about. Every
//!   other comparison is meaningless against the wrong directory — hashing `src/a.rs` under a
//!   different tree answers a different question — so the digest comparison is not attempted and
//!   the reason says why. Only a caller that supplies [`FreshnessOptions::repo_root`] can be
//!   compared: this module is not handed a root by the envelope path, and "no root to compare"
//!   must never be reported as a mismatch.
//! - `base_commit_moved`: the recorded `generation.base_commit` is no longer `HEAD`. A commit
//!   change means the tree the index was built from is behind, even when every indexed file still
//!   hashes the same.
//! - `watcher_stalled`: the indexer records itself as busy and nothing has been published for
//!   longer than [`WATCHER_STALL_MS`]. `meta` carries no "busy since" column, so the last publish
//!   time is the only publish-side clock there is; a run recorded as busy across a stretch that
//!   long is the observable form of a watcher that died mid-run.
//! - `files_changed`: an indexed file's current bytes hash differently from the `file.sha256` the
//!   indexer stored.
//!
//! Three degradations are deliberate, because a freshness verdict that threw would take the
//! envelope down with it:
//!
//! - `git` is never invoked. [`head_commit_of`] reads `.git/HEAD`, the ref file it names and
//!   `packed-refs` — three reads, no child process, no lock, nothing a target repo's config could
//!   make this module execute. A directory that is not a checkout yields `None`, and
//!   `base_commit_moved` is then simply not claimed: "unknown" must never be reported as "stale".
//! - a file that cannot be read is not counted as changed. It has no bytes to compare, so it is
//!   neither equal nor different; [`crate::diff`] is the module that names removals for the
//!   change-set tools, and `expand` reports an unreadable path at the point of use. `status` says so
//!   in its own summary rather than inventing a count here.
//! - a reader that cannot answer (`meta_for` or `all_files` failing) produces `stale: true` with no
//!   `stale_reason`. The type distinguishes that from a fresh answer — a reason is what a check
//!   found, and no check ran — and it is the one verdict that never overstates certainty.
//!
//! One source decision is this port's, not the TypeScript's: **`indexed_at` is
//! `latest_generation().published_at`**, because `sangkar_store`'s `GraphMeta` deliberately carries
//! no timestamp. The newest publish by `seq` is the generation the read path serves after a hot
//! reload, so its `published_at` is the time of the bytes being described; taking the served
//! generation id from `meta.generation` (the one a cursor binds to) and the time from the newest
//! publish is the only pairing the store offers, and a mismatch between them is a publish that has
//! not yet been observed, which is exactly what `stale` is for.
//!
//! The stall comparison parses `published_at` to epoch milliseconds ([`parse_iso8601_millis`]) and
//! subtracts. The other two candidates were rejected for a reason: lexicographic order on the
//! ISO-8601 string answers "which is later" but cannot add the ten-minute budget, and a `SystemTime`
//! delta still needs exactly the same parser before there is a `SystemTime` to subtract from.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sangkar_core::log;
use sangkar_store::db;
use sangkar_store::read::reader::{GenerationRow, GraphMeta, GraphReader};

use crate::diff::sha256_hex;

/// Ten minutes of "busy" with no publish: the bound `plan` sets for a stalled watcher.
pub const WATCHER_STALL_MS: u64 = 600_000;

/// The `meta.indexer_state` values that claim a run is in flight. `failed` is not one of them: a
/// failed run is reported by `health.consecutive_failures`, not as a stall.
pub const BUSY_INDEXER_STATES: [&str; 2] = ["running", "busy"];

/// Every value [`Freshness::stale_reason`] can take, copied verbatim from the TypeScript.
///
/// `none` is a value rather than an absent field so a caller maps one closed set. Rust represents
/// it as [`None`] (a fresh answer carries no reason at all, which is what the envelope serialises),
/// but the constant keeps the wire vocabulary in one place for a reader who has the TypeScript
/// open.
pub const STALE_REASONS: [&str; 5] = [
    "none",
    "files_changed",
    "root_mismatch",
    "base_commit_moved",
    "watcher_stalled",
];

/// The verdict, with the evidence that produced it.
///
/// Mirrors the envelope's `read_status` (minus `decisive`, which the composition root fills once it
/// knows whether this is the answer that ends the search). `stale_reason` is `None` exactly when
/// `stale` is `false`, or when a check could not run at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Freshness {
    pub generation: String,
    pub indexed_at: String,
    pub stale: bool,
    pub stale_reason: Option<String>,
    pub stale_checked_at: Option<String>,
    /// Indexed files whose current bytes no longer match their recorded `sha256`; `0` when the
    /// comparison did not run.
    ///
    /// The TypeScript carries the same field on `Freshness` and `status` renders it in its summary
    /// ("N changed on disk since the index"), so it is part of the verdict rather than something a
    /// second caller should recompute with a second pass over the tree.
    pub files_changed_since: usize,
}

/// What the caller knows that the reader cannot: the clock, and the tree the answer is about.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreshnessOptions<'a> {
    /// Injected clock in epoch milliseconds; staleness must be testable rather than wall-clock.
    pub now_ms: Option<i64>,
    /// The root the caller is answering about. When supplied and different from the recorded
    /// `meta.project_root`, the verdict is `root_mismatch` before anything is hashed; when absent,
    /// the recorded root (if any) is used for the digest and `HEAD` probes.
    pub repo_root: Option<&'a Path>,
}

/// The freshness verdict for a reader, computed from that reader and the injected clock alone.
///
/// The comparison is ordered as the module comment describes and the first failing check wins: a
/// mismatched root makes the hash comparison meaningless, and a commit or a stalled watcher explains
/// the drift before the per-file count does. When a root is known the cost is one `all_files()` and
/// one read per indexed file; the caller owns the decision to ask.
#[must_use]
pub fn check_freshness(reader: &dyn GraphReader, opts: &FreshnessOptions<'_>) -> Freshness {
    let now_ms = opts.now_ms.unwrap_or_else(epoch_millis);
    let checked_at = iso8601_millis(now_ms);

    let meta = match reader.meta_for() {
        Ok(meta) => meta,
        Err(error) => {
            log::warn(
                &format!(
                    "the graph's identity could not be read, so freshness is unverified: {error}"
                ),
                &[],
            );
            return Freshness {
                generation: String::new(),
                indexed_at: String::new(),
                stale: true,
                stale_reason: None,
                stale_checked_at: Some(checked_at),
                files_changed_since: 0,
            };
        }
    };
    let latest = match reader.latest_generation() {
        Ok(latest) => latest,
        Err(error) => {
            log::warn(
                &format!("the newest generation could not be read: {error}"),
                &[],
            );
            None
        }
    };

    let generation = meta.generation.clone().unwrap_or_default();
    let indexed_at = latest
        .as_ref()
        .map(|row| row.published_at.clone())
        .unwrap_or_default();
    let verdict = |reason: Option<&str>, files_changed_since: usize| Freshness {
        generation: generation.clone(),
        indexed_at: indexed_at.clone(),
        stale: reason.is_some(),
        stale_reason: reason.map(str::to_owned),
        stale_checked_at: Some(checked_at.clone()),
        files_changed_since,
    };

    // An unrecorded `project_root` is not a mismatch: the graph never claimed a tree, so there is
    // nothing to contradict. Only two recorded roots that differ are a `root_mismatch`.
    if let (Some(caller), Some(recorded)) = (opts.repo_root, meta.project_root.as_deref())
        && caller != Path::new(recorded)
    {
        return verdict(Some("root_mismatch"), 0);
    }

    let root: Option<&Path> = opts
        .repo_root
        .or_else(|| meta.project_root.as_deref().map(Path::new));

    // One `all_files()` and one read per indexed file for the whole verdict, whatever the reason it
    // reports: the count is part of the answer in every branch below.
    let files_changed_since = match root.map(|root| count_changed(root, reader)) {
        Some(Ok(count)) => count,
        Some(Err(error)) => {
            log::warn(
                &format!(
                    "the indexed files could not be listed, so freshness is unverified: {error}"
                ),
                &[],
            );
            return Freshness {
                generation: generation.clone(),
                indexed_at: indexed_at.clone(),
                stale: true,
                stale_reason: None,
                stale_checked_at: Some(checked_at.clone()),
                files_changed_since: 0,
            };
        }
        None => 0,
    };

    let recorded = latest
        .as_ref()
        .and_then(|row| row.base_commit.as_deref())
        .map(str::trim)
        .unwrap_or("")
        .to_lowercase();
    let head = root.and_then(head_commit_of).unwrap_or_default();
    if !recorded.is_empty() && !head.is_empty() && moved_since(&recorded, &head) {
        return verdict(Some("base_commit_moved"), files_changed_since);
    }

    if watcher_stalled(&meta, &indexed_at, now_ms) {
        return verdict(Some("watcher_stalled"), files_changed_since);
    }

    verdict(
        if files_changed_since == 0 {
            None
        } else {
            Some("files_changed")
        },
        files_changed_since,
    )
}

/// The commit `HEAD` names, read from the repository's own files: the `HEAD` file, the ref it names
/// (loose, then `packed-refs`), or the raw object name of a detached `HEAD`.
///
/// `None` is the honest answer for "not a checkout", "no ref yet" and "unreadable": the caller
/// degrades to "no commit comparison", which is why this module never shells out to git. A child
/// process would cost milliseconds, take an optional lock and execute whatever a target repo's
/// `core.pager`/`core.fsmonitor` config asked for, none of which a read-only tool may do.
#[must_use]
pub fn head_commit_of(repo_root: &Path) -> Option<String> {
    let git_dir = git_dir_of(repo_root)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    if let Some(reference) = head.strip_prefix("ref:") {
        let reference = reference.trim();
        if reference.is_empty() {
            return None;
        }
        let loose = git_dir.join(reference.split('/').collect::<PathBuf>());
        if let Some(sha) = fs::read_to_string(loose)
            .ok()
            .and_then(|text| commit_sha(&text))
        {
            return Some(sha);
        }
        // A ref `git gc` has packed is not a loose file; fall through rather than call it unknown.
        return packed_ref(&git_dir, reference);
    }
    commit_sha(head)
}

/// The git directory for `repo_root`: `.git` itself, or the `gitdir:` target a worktree or
/// submodule names in a `.git` file.
fn git_dir_of(repo_root: &Path) -> Option<PathBuf> {
    let dot_git = repo_root.join(".git");
    match fs::metadata(&dot_git) {
        Ok(metadata) if metadata.is_dir() => return Some(dot_git),
        Ok(_) => {}
        Err(_) => return None,
    }
    let text = fs::read_to_string(&dot_git).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    if target.is_empty() {
        return None;
    }
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        repo_root.join(target)
    })
}

/// A ref that has been packed away by `git gc`: one line per ref, `^` lines are peel lines.
fn packed_ref(git_dir: &Path, reference: &str) -> Option<String> {
    let text = fs::read_to_string(git_dir.join("packed-refs")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else {
            continue;
        };
        if parts.next() == Some(reference) {
            return commit_sha(sha);
        }
    }
    None
}

/// A recorded commit name in comparison form, or `None` when the file does not hold one.
///
/// The accepted shape is the TypeScript's `/^[0-9a-f]{7,64}$/i`: a full `sha1` (40), a full
/// `sha256` object name (64), or a short form no shorter than seven. Lowercased because git writes
/// lowercase and a hand-edited file may not.
fn commit_sha(text: &str) -> Option<String> {
    let sha = text.trim().to_lowercase();
    if (7..=64).contains(&sha.len()) && sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Some(sha)
    } else {
        None
    }
}

/// A short recorded commit and a full `HEAD` are the same commit when one is a prefix of the other,
/// which is what makes `base_commit` written by `git rev-parse --short` comparable at all.
fn moved_since(recorded: &str, head: &str) -> bool {
    !(recorded == head || head.starts_with(recorded) || recorded.starts_with(head))
}

/// `meta.indexer_state` busy and nothing published for longer than the threshold.
fn watcher_stalled(meta: &GraphMeta, published_at: &str, now_ms: i64) -> bool {
    let state = meta.indexer_state.as_deref().unwrap_or("").to_lowercase();
    if !BUSY_INDEXER_STATES.contains(&state.as_str()) {
        return false;
    }
    let Some(published_at) = parse_iso8601_millis(published_at) else {
        return false;
    };
    now_ms.saturating_sub(published_at) > WATCHER_STALL_MS as i64
}

/// Indexed files whose working-tree bytes differ from the digest the indexer stored.
fn count_changed(
    repo_root: &Path,
    reader: &dyn GraphReader,
) -> Result<usize, sangkar_core::errors::AppError> {
    let files = reader.all_files()?;
    let mut changed = 0;
    for file in files {
        let Some(absolute) = absolute_within(repo_root, &file.path) else {
            continue;
        };
        // An unreadable file is not a changed one: it has no bytes to compare.
        let Ok(bytes) = fs::read(&absolute) else {
            continue;
        };
        if sha256_hex(&bytes) != file.sha256.to_lowercase() {
            changed += 1;
        }
    }
    Ok(changed)
}

/// A path node of the working tree, or `None` when the row escapes `root`.
///
/// The containment test is component-based and rejects an absolute path, `..` and `.` before any
/// read: a `file.path` is written by this project, but a row read from a database is data, not a
/// promise.
fn absolute_within(root: &Path, path: &str) -> Option<PathBuf> {
    if path.is_empty() {
        return None;
    }
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(relative))
}

/// `status.health`: the indexer's own state and the two costs a caller can act on.
///
/// The type has no derives on purpose: this crate depends on neither `serde` nor `serde_json`, so
/// the crate that serialises a payload owns the wire attributes. Every field is a plain value the
/// reader already carries — `last_publish_ms` is the duration of the publish being served (`0`
/// before the first one), and `consecutive_failures` is `meta.consecutive_failures` unmapped.
pub struct ToolHealth {
    pub indexer: Option<String>,
    pub last_publish_ms: u64,
    pub consecutive_failures: i64,
    pub statement_cache_entries: usize,
}

/// The health report for `status`.
///
/// `statement_cache_entries` is [`sangkar_store::db::statement_cache_stats`]'s process-wide `size`,
/// not [`sangkar_store::read::reader::SqliteReader::cache_stats`]'s single-handle count: a
/// `&dyn GraphReader` carries no cache member (the reader trait is deliberately statements only),
/// and the store documents the process-wide sum as exactly the number this payload field reports.
/// A fake reader in a test therefore reports the same number a real handle would, and nothing here
/// invents a count.
#[must_use]
pub fn health(reader: &dyn GraphReader) -> ToolHealth {
    let meta = match reader.meta_for() {
        Ok(meta) => meta,
        Err(error) => {
            log::warn(
                &format!("the graph's identity could not be read, so health is empty: {error}"),
                &[],
            );
            return ToolHealth {
                indexer: None,
                last_publish_ms: 0,
                consecutive_failures: 0,
                statement_cache_entries: db::statement_cache_stats().size,
            };
        }
    };
    let last_publish_ms = reader
        .latest_generation()
        .ok()
        .flatten()
        .map_or(0, |row: GenerationRow| {
            u64::try_from(row.duration_ms).unwrap_or(0)
        });
    ToolHealth {
        indexer: meta.indexer_state,
        last_publish_ms,
        consecutive_failures: meta.consecutive_failures,
        statement_cache_entries: db::statement_cache_stats().size,
    }
}

/// Milliseconds since the UNIX epoch — the unit a JavaScript date is an integer count of.
fn epoch_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // A host clock set before 1970 is not a state this tool supports; the epoch keeps the
        // timestamp well formed instead of emitting a negative one the TypeScript never produced.
        Err(_) => 0,
    }
}

/// `new Date(millis).toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// Written by hand for the same reason `sangkar_core::log` writes its own private copy: the crate
/// set carries no date library, this module owns nothing in `log`, and the alternative is widening
/// another module's API in a task that does not own it. The arithmetic is the same ordinal
/// conversion, valid for any `i64` millisecond count.
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

/// Howard Hinnant's `days_from_civil`: the inverse of [`civil_from_days`].
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The epoch milliseconds an ISO-8601 UTC timestamp names, or `None` when it is not the form this
/// workspace writes (`YYYY-MM-DDTHH:MM:SS[.fff]Z`).
///
/// Only the `Z` form is accepted. The TypeScript uses `Date.parse`, which also accepts offset forms
/// and a zone-less string as *local* time — the second is a host-dependent answer no stored value
/// should be interpreted through, so it is rejected instead of guessed at.
fn parse_iso8601_millis(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[10] != b'T'
    {
        return None;
    }
    let year = parse_digits(bytes.get(0..4)?)?;
    let month = parse_digits(bytes.get(5..7)?)?;
    let day = parse_digits(bytes.get(8..10)?)?;
    let hour = parse_digits(bytes.get(11..13)?)?;
    let minute = parse_digits(bytes.get(14..16)?)?;
    let second = parse_digits(bytes.get(17..19)?)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut rest = bytes.get(19..)?;
    let mut millis = 0i64;
    if rest.first() == Some(&b'.') {
        rest = rest.get(1..)?;
        let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let kept = digits.min(3);
        let value = parse_digits(rest.get(..kept)?)?;
        millis = value * 10i64.pow(u32::try_from(3 - kept).unwrap_or(0));
        rest = rest.get(digits..)?;
    }
    if rest != b"Z" && rest != b"z" {
        return None;
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3_600 + minute * 60 + second) * 1_000 + millis)
}

/// A run of ASCII digits as an integer; any other byte, or an empty slice, is `None`.
fn parse_digits(bytes: &[u8]) -> Option<i64> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0i64;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(i64::from(byte.saturating_sub(b'0')))?;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::{iso8601_millis, parse_iso8601_millis};

    /// Every expectation is `new Date(millis).toISOString()` under Bun 1.4.4, the oracle the parity
    /// bar names — including the two dates either side of the epoch and a leap day.
    #[test]
    fn iso8601_round_trips_through_the_parser() {
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
            assert_eq!(
                parse_iso8601_millis(expected),
                Some(millis),
                "the formatter's own output parses back"
            );
        }
    }

    /// The parser takes the seconds form too, pads a short fraction, truncates a long one, and
    /// refuses a local-time or offset string instead of guessing a zone.
    #[test]
    fn the_parser_accepts_the_wire_form_and_refuses_the_rest() {
        assert_eq!(
            parse_iso8601_millis("2024-02-29T12:34:56Z"),
            Some(1_709_210_096_000)
        );
        assert_eq!(
            parse_iso8601_millis("2024-02-29T12:34:56.5Z"),
            Some(1_709_210_096_500)
        );
        assert_eq!(
            parse_iso8601_millis("2024-02-29T12:34:56.789123Z"),
            Some(1_709_210_096_789)
        );
        for refused in [
            "2024-02-29T12:34:56",
            "2024-02-29T12:34:56+00:00",
            "2024-13-29T12:34:56.000Z",
            "2024-02-29 12:34:56.000Z",
            "",
        ] {
            assert_eq!(parse_iso8601_millis(refused), None, "{refused:?}");
        }
    }
}
