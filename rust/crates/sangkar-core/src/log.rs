//! The workspace's only stderr writer.
//!
//! Ported from `src/shared/log.ts`. stdout carries the MCP protocol or the rendered answer, so
//! every diagnostic is one compact JSON line on stderr and no other module writes there.

use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

/// The four levels, spelled as the TypeScript `level` field spells them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Writes one line `{ts, level, msg, ...fields}` — compact JSON, keys in that order — to stderr.
///
/// One `write_all` and one `flush` per call, so two threads cannot interleave their bytes and a
/// reader never sees half a record.
pub fn log(level: Level, msg: &str, fields: &[(&str, Value)]) {
    let mut record = Map::new();
    record.insert(
        "ts".to_owned(),
        Value::String(iso8601_millis(epoch_millis())),
    );
    record.insert("level".to_owned(), Value::String(level.as_str().to_owned()));
    record.insert("msg".to_owned(), Value::String(msg.to_owned()));
    for (key, value) in fields {
        // `Map::insert` keeps the position of a key it replaces, which is what the TypeScript's
        // object spread over `{ts, level, msg}` does with a colliding field name.
        record.insert((*key).to_owned(), value.clone());
    }

    let mut line = serialise(&record);
    line.push('\n');

    let mut stderr = std::io::stderr().lock();
    // A stderr write has nowhere left to report its own failure, and a logger that panicked on a
    // full disk would take the session with it, so both results are dropped deliberately.
    let _ = stderr.write_all(line.as_bytes());
    let _ = stderr.flush();
}

/// `log.debug(msg, fields)`.
pub fn debug(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Debug, msg, fields);
}

/// `log.info(msg, fields)`.
pub fn info(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Info, msg, fields);
}

/// `log.warn(msg, fields)`.
pub fn warn(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Warn, msg, fields);
}

/// `log.error(msg, fields)`.
pub fn error(msg: &str, fields: &[(&str, Value)]) {
    log(Level::Error, msg, fields);
}

fn serialise(record: &Map<String, Value>) -> String {
    match serde_json::to_string(record) {
        Ok(json) => json,
        // A field that cannot be serialised (a NaN float, say) must not take the process down
        // mid-session; report the failure instead, exactly as the TypeScript guard does.
        Err(_) => format!(
            r#"{{"ts":"{}","level":"error","msg":"log serialisation failed"}}"#,
            iso8601_millis(epoch_millis())
        ),
    }
}

/// Milliseconds since the UNIX epoch — the unit a JavaScript date is an integer count of.
fn epoch_millis() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // A host clock set before 1970 is not a state this tool supports; the epoch keeps the line
        // well formed instead of emitting a negative timestamp the TypeScript never produced.
        Err(_) => 0,
    }
}

/// `new Date(millis).toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// Written by hand because the crate set carries no date library and this is the only timestamp the
/// kernel formats. Pure ordinal arithmetic, valid for any `i64` millisecond count.
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
    use super::{Level, iso8601_millis};

    /// Every expectation is `new Date(millis).toISOString()` under Bun 1.4.4, the oracle the
    /// parity bar names — including the two dates either side of the epoch and a leap day.
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
    fn level_spellings_are_the_typescript_strings() {
        let spellings: [&str; 4] = [
            Level::Debug.as_str(),
            Level::Info.as_str(),
            Level::Warn.as_str(),
            Level::Error.as_str(),
        ];
        assert_eq!(spellings, ["debug", "info", "warn", "error"]);
    }
}
