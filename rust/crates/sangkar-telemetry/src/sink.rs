//! The telemetry seam: where a serialised line goes, and nothing else.
//!
//! Ported from `src/shared/telemetry/sink.ts`. The sink owns no policy. It is handed a `YYYY-MM-DD`
//! the recorder derived from the record's own `ts` ([`crate::identity::utc_date_of`]), so it holds no
//! clock and does no date arithmetic; it never parses, filters or re-serialises a line, so the one
//! place a record's shape is decided stays [`crate::record`]. A future uploader is one more
//! implementation of [`TelemetrySink`] — no call site moves.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;

/// Where one flush's lines go.
pub trait TelemetrySink {
    /// Appends `lines` to the file that holds `date`, one line each.
    ///
    /// Errors are deliberately handed back rather than swallowed here: the recorder wraps every call,
    /// logs once and degrades, so a read-only data dir or a full disk costs records rather than a
    /// tool call.
    ///
    /// # Errors
    ///
    /// The filesystem's own error when the directory cannot be created or the file cannot be
    /// appended to.
    fn write(&mut self, date: &str, lines: &[String]) -> io::Result<()>;
}

/// One daily file, appended to, never rewritten.
///
/// The server and the index worker it spawns hold separate recorders writing the same
/// `<dir>/<date>.jsonl`, so the single append below is the whole concurrency contract: `O_APPEND`
/// lands the joined buffer whole, which is why interleaving happens between lines and never inside
/// one. The caller caps how many lines it hands over for exactly that reason.
#[derive(Clone, Debug)]
pub struct JsonlSink {
    dir: PathBuf,
    ready: bool,
}

impl JsonlSink {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            ready: false,
        }
    }
}

impl TelemetrySink for JsonlSink {
    fn write(&mut self, date: &str, lines: &[String]) -> io::Result<()> {
        if lines.is_empty() {
            // A zero-line append would write a bare newline, which the report reader has to count as
            // an unparsed (torn) line. There is nothing to persist, so nothing is written — and the
            // directory is not created for it either.
            return Ok(());
        }
        if !self.ready {
            fs::create_dir_all(&self.dir)?;
            self.ready = true;
        }
        let mut body = lines.join("\n");
        body.push('\n');
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(self.dir.join(format!("{date}.jsonl")))?;
        file.write_all(body.as_bytes())
    }
}

/// For a recorder that must exist but must not touch the disk (tests, a disabled switch).
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl TelemetrySink for NullSink {
    fn write(&mut self, _date: &str, _lines: &[String]) -> io::Result<()> {
        // Discard: the recorder is real, the destination is not.
        Ok(())
    }
}

/// The only implementation that touches a filesystem: one daily file under `dir`.
#[must_use]
pub fn jsonl_sink(dir: impl Into<PathBuf>) -> JsonlSink {
    JsonlSink::new(dir)
}

/// A sink that writes nothing.
#[must_use]
pub fn null_sink() -> NullSink {
    NullSink
}
