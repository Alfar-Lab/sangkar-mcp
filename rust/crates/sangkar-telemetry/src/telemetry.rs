//! The recorder: one per process, and the only owner of the identity, the `seq` counter, the buffer
//! and the sink.
//!
//! Ported from `src/shared/telemetry/telemetry.ts`. The shape is a consequence of two constraints.
//! First, a tool call must never fail because telemetry did: [`Recorder::record_call`] appends to a
//! vector and returns, and every sink call is wrapped, so a full disk or a read-only data dir costs
//! records, never an answer. Second, a flush must be one append per daily file — the server and the
//! index worker it spawns hold separate recorders over the same `<dir>/<date>.jsonl`, so the buffer
//! is grouped by the date each record's own `ts` falls on and each group is handed to the sink in a
//! single call, which is what makes interleaving line-atomic.
//!
//! There is no interval: the age trigger is evaluated on the next record and reads the injected
//! clock, so nothing here keeps the process alive and a test can prove the trigger without sleeping.
//!
//! Two seams the TypeScript gets for free are explicit here. The clock is a [`Now`] the caller hands
//! over ([`SystemTime`] when it passes none), because this module must hold no second time source
//! and the age trigger has to be provable. The sink is a boxed [`TelemetrySink`] rather than a
//! module-level `jsonlSink` call, so a test can hold the lines. Both make the options an owned value
//! — a boxed sink cannot be moved out of a `&TelemetryOptions` — which is the one signature change
//! from `createTelemetry(options)`.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

use sangkar_core::log;
use sangkar_core::runtime::{arch, os};

use crate::identity::{new_session_id, resolve_install_id, utc_date_of};
use crate::record::{
    CallSpec, IndexRunSpec, RecordContext, RepoSizeBucket, SessionFacts, SessionMode,
    call_record_of, index_run_record_of, repo_size_bucket_of, root_kind_of, session_record_of,
};
use crate::sink::{NullSink, TelemetrySink, jsonl_sink};

/// Records buffered before a flush. Bounds a lost tail and caps one `O_APPEND` write.
pub const FLUSH_AT_RECORDS: usize = 32;

/// Age of the oldest buffered record that forces a flush, in milliseconds.
pub const FLUSH_AT_MS: u64 = 5_000;

/// The clock seam (`now?: () => Date` in the TypeScript), in milliseconds since the UNIX epoch.
///
/// Boxed rather than the `fn` pointer `sangkar_store`'s cursor clock uses: proving the age trigger
/// takes two readings from one recorder, and a `fn` cannot carry a test's counter.
pub type Now = Box<dyn Fn() -> i64 + Send>;

/// Everything [`create`] needs, and nothing it may re-derive.
pub struct TelemetryOptions {
    /// The telemetry directory, always `AppConfig.telemetry.dir`; the recorder never re-derives it.
    pub dir: PathBuf,
    pub repo_root: PathBuf,
    pub data_dir: PathBuf,
    pub session_mode: SessionMode,
    pub server_version: String,
    pub graph_schema_version: u64,
    /// Where the lines go; [`jsonl_sink`] over [`TelemetryOptions::dir`] when absent.
    pub sink: Option<Box<dyn TelemetrySink + Send>>,
    /// The clock; [`SystemTime`] when absent.
    pub now: Option<Now>,
}

/// One serialised record, with the date its own `ts` falls on, so a flush can group without a
/// clock, and the clock reading of the record itself — the age trigger never takes a second one.
struct BufferedLine {
    date: String,
    line: String,
    at: i64,
}

/// The buffered recorder: the identity, the per-session counter, the buffer and the sink.
pub struct Recorder {
    install_id: String,
    session_id: String,
    seq: u64,
    buffer: Vec<BufferedLine>,
    sink: Box<dyn TelemetrySink + Send>,
    now: Now,
    /// Set once a flush has failed; a degraded recorder stays silent.
    degraded: bool,
    closed: bool,
    close_reported: bool,
    /// A recorder with no destination: every method returns before it touches anything.
    nulled: bool,
}

/// A live recorder for [`TelemetryOptions`].
///
/// The session record is buffered here, before the caller can make a call: it is the denominator
/// every later record is read against. It carries no `seq` (the schema has none for it), which the
/// shape of [`crate::record::SessionRecord`] enforces rather than this module.
#[must_use]
pub fn create(opts: TelemetryOptions) -> Recorder {
    let TelemetryOptions {
        dir,
        repo_root,
        data_dir,
        session_mode,
        server_version,
        graph_schema_version,
        sink,
        now,
    } = opts;
    let now: Now = now.unwrap_or_else(|| Box::new(system_now_ms));
    let sink: Box<dyn TelemetrySink + Send> = sink.unwrap_or_else(|| Box::new(jsonl_sink(&dir)));

    // One reading for both the install id's day and the session record's `ts`. Two readings cannot
    // differ except across a midnight, where they would disagree about which day the process
    // belongs to — a disagreement the TypeScript's two `clock()` calls can produce.
    let opened = now();
    let opened_ts = iso8601_millis(opened);
    // `utc_date_of` is the crate's one rule for "which day does this timestamp fall on". The string
    // just built always begins with its own day, so the default is unreachable and exists only
    // because that rule refuses a prefix it cannot date.
    let identity = resolve_install_id(&dir, utc_date_of(&opened_ts).unwrap_or_default());
    let session_id = new_session_id();

    let opened_context = RecordContext {
        ts: opened_ts.clone(),
        install_id: identity.install_id.clone(),
        session_id: session_id.clone(),
        seq: 0,
        server_version,
        os: os().to_owned(),
        arch: arch().to_owned(),
        graph_schema_version,
    };
    let facts = SessionFacts {
        session_mode,
        is_first_run: identity.is_first_run,
        repo_size_bucket: repo_size_bucket_for(&data_dir),
        // A process that never walked the tree cannot name its languages; `index_run.languages` is
        // the run-level truth.
        languages: Vec::new(),
        root_kind: root_kind_of(repo_root.join(".git").exists()),
    };

    let mut recorder = Recorder {
        install_id: identity.install_id,
        session_id,
        seq: 0,
        buffer: Vec::new(),
        sink,
        now,
        degraded: false,
        closed: false,
        close_reported: false,
        nulled: false,
    };
    recorder.append(
        &session_record_of(&opened_context, &facts),
        &opened_ts,
        opened,
    );
    recorder
}

/// What a caller holds when the switch is off or no recorder was supplied: the same interface with
/// no destination, so the call sites carry no conditional. Never touches the filesystem — it has no
/// directory to touch and mints no install id.
#[must_use]
pub fn null() -> Recorder {
    Recorder {
        install_id: String::new(),
        session_id: String::new(),
        seq: 0,
        buffer: Vec::new(),
        sink: Box::new(NullSink),
        now: Box::new(|| 0i64),
        degraded: false,
        closed: false,
        close_reported: false,
        nulled: true,
    }
}

impl Recorder {
    /// Stable for one UTC day, unlinkable from one day to the next; empty on [`null`].
    #[must_use]
    pub fn install_id(&self) -> &str {
        &self.install_id
    }

    /// One id per process; empty on [`null`].
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Buffers one answer's record. Never fails, never propagates a sink error.
    ///
    /// The recorder stamps the four fields a caller cannot know — the clock reading, both
    /// identifiers and the session's own counter — and keeps the caller's facts about its
    /// composition (`server_version`, `os`, `arch`, `graph_schema_version`); it reads no
    /// environment of its own for either.
    pub fn record_call(&mut self, ctx: &RecordContext, spec: &CallSpec) {
        if !self.accepts() {
            return;
        }
        let at = (self.now)();
        self.seq = self.seq.saturating_add(1);
        let stamp = self.stamp(ctx, at);
        self.append(&call_record_of(&stamp, spec), &stamp.ts, at);
    }

    /// Buffers one index run's record, stamped exactly as [`Recorder::record_call`] stamps a call.
    ///
    /// The counter is incremented here too, as the TypeScript does — but `IndexRunRecord` has no
    /// `seq` field, so the step is only visible on the next `call` line. That asymmetry is the v1
    /// record shape's, not this module's; the report never reads a run's position in the session.
    pub fn record_index_run(&mut self, ctx: &RecordContext, spec: &IndexRunSpec) {
        if !self.accepts() {
            return;
        }
        let at = (self.now)();
        self.seq = self.seq.saturating_add(1);
        let stamp = self.stamp(ctx, at);
        self.append(&index_run_record_of(&stamp, spec), &stamp.ts, at);
    }

    /// Writes every buffered record, one append per UTC date.
    pub fn flush(&mut self) {
        if self.nulled || self.degraded || self.closed || self.buffer.is_empty() {
            return;
        }
        // Grouped by the date each record's own `ts` falls on, in first-seen order (the TypeScript's
        // `Map`), so a buffer that straddles UTC midnight becomes one append per file.
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        for entry in std::mem::take(&mut self.buffer) {
            match groups.iter_mut().find(|(date, _)| *date == entry.date) {
                Some((_, lines)) => lines.push(entry.line),
                None => groups.push((entry.date, vec![entry.line])),
            }
        }
        // Consumed before the writes, and never re-queued on failure: a flush that straddles UTC
        // midnight can have its first group already on disk, and a retry would duplicate those lines
        // in a file that is append-only.
        for (date, lines) in groups {
            if let Err(error) = self.sink.write(&date, &lines) {
                self.degraded = true;
                log::warn(
                    "telemetry.sink_failed",
                    &[("reason", Value::String(error.to_string()))],
                );
                // The TypeScript's `try` wraps the whole loop: the first failure ends the flush, and
                // the one `warn` above is the whole report — a line per dropped record would turn a
                // full disk into a log flood.
                break;
            }
        }
    }

    /// The TypeScript's `close()`: flush, then refuse further records. There is no interval to
    /// stop, so this is the one place a tail is written before the process ends.
    pub fn close(&mut self) {
        if self.closed {
            return;
        }
        self.flush();
        self.closed = true;
    }

    /// The four fields the recorder owns, stamped onto the caller's own facts.
    fn stamp(&self, ctx: &RecordContext, at: i64) -> RecordContext {
        RecordContext {
            ts: iso8601_millis(at),
            install_id: self.install_id.clone(),
            session_id: self.session_id.clone(),
            seq: self.seq,
            server_version: ctx.server_version.clone(),
            os: ctx.os.clone(),
            arch: ctx.arch.clone(),
            graph_schema_version: ctx.graph_schema_version,
        }
    }

    /// Buffers one line, then applies both triggers. A record that cannot be serialised is dropped
    /// with one warning: a telemetry line is never worth a panic in the caller's path.
    fn append<T: Serialize>(&mut self, record: &T, ts: &str, at: i64) {
        match buffered_line(record, ts, at) {
            Ok(entry) => {
                self.buffer.push(entry);
                self.maybe_flush(at);
            }
            Err(reason) => log::warn(
                "telemetry.record_failed",
                &[("reason", Value::String(reason.to_owned()))],
            ),
        }
    }

    fn maybe_flush(&mut self, now_ms: i64) {
        let aged = self
            .buffer
            .first()
            .is_some_and(|oldest| now_ms.saturating_sub(oldest.at) >= flush_at_ms());
        if self.buffer.len() >= FLUSH_AT_RECORDS || aged {
            self.flush();
        }
    }

    /// `false` once the recorder can no longer produce a line. A degraded recorder stays silent; a
    /// closed one reports itself exactly once, at [`sangkar_core::log::debug`].
    fn accepts(&mut self) -> bool {
        if self.nulled || self.degraded {
            return false;
        }
        if !self.closed {
            return true;
        }
        if !self.close_reported {
            self.close_reported = true;
            log::debug("telemetry.record_after_close", &[]);
        }
        false
    }
}

impl fmt::Debug for Recorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The sink and the clock have no shape worth printing, and a buffer of serialised lines
        // would drown the one number a reader wants: how much is waiting.
        f.debug_struct("Recorder")
            .field("install_id", &self.install_id)
            .field("session_id", &self.session_id)
            .field("seq", &self.seq)
            .field("buffered", &self.buffer.len())
            .field("degraded", &self.degraded)
            .field("closed", &self.closed)
            .field("nulled", &self.nulled)
            .finish()
    }
}

/// [`FLUSH_AT_MS`] in the clock's own unit.
fn flush_at_ms() -> i64 {
    i64::try_from(FLUSH_AT_MS).unwrap_or(i64::MAX)
}

/// One serialised record with the date its own `ts` names. `Err` names the reason in the log line.
fn buffered_line<T: Serialize>(
    record: &T,
    ts: &str,
    at: i64,
) -> Result<BufferedLine, &'static str> {
    let Some(date) = utc_date_of(ts) else {
        return Err("ts_not_a_day");
    };
    let line = serde_json::to_string(record).map_err(|_| "not_serialisable")?;
    Ok(BufferedLine {
        date: date.to_owned(),
        line,
        at,
    })
}

/// A stat, not a walk: bucketing the real file count would need the indexer's discovery, which
/// `shared-isolation` forbids, and it would run on every start. An absent or unreadable graph is
/// `"none"` — the same bucket a zero-byte file gets — because "no graph yet" is what it means.
fn repo_size_bucket_for(data_dir: &Path) -> RepoSizeBucket {
    repo_size_bucket_of(fs::metadata(data_dir.join("graph.db")).map_or(0, |meta| meta.len()))
}

/// Milliseconds since the UNIX epoch — the unit a JavaScript date is an integer count of.
fn system_now_ms() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        // A host clock set before 1970 is not a state this tool supports; the epoch keeps the line
        // well formed instead of emitting a timestamp nothing can parse.
        Err(_) => 0,
    }
}

/// `new Date(millis).toISOString()`: UTC, `YYYY-MM-DDTHH:MM:SS.sssZ`, milliseconds always present.
///
/// Written out because the crate set carries no date library and this is the only timestamp the
/// recorder formats. `sangkar_core`'s own formatter is private to `log` and `pidlock`, so this is the
/// sixth copy of the arithmetic in the tree; a public core helper would retire all of them.
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
