/**
 * The telemetry seam: where a serialised line goes, and nothing else.
 *
 * The sink owns no policy. It is handed a `YYYY-MM-DD` the recorder derived from the record's own
 * `ts`, so it holds no clock and does no date arithmetic; it never parses, filters or re-serialises
 * a line, so the one place a record's shape is decided stays `record.ts`. A future uploader is one
 * more implementation of {@link TelemetrySink} — no call site moves.
 */
import { appendFileSync, mkdirSync } from "node:fs";
import { join } from "node:path";

export interface TelemetrySink {
  write(date: string, lines: readonly string[]): void;
}

/**
 * One daily file, appended to, never rewritten.
 *
 * The server and the index worker it spawns hold separate recorders writing the same
 * `<dir>/<date>.jsonl`, so the single `appendFileSync` below is the whole concurrency contract:
 * `O_APPEND` lands the joined buffer whole, which is why interleaving happens between lines and
 * never inside one. The caller caps how many lines it hands over for exactly that reason.
 *
 * Errors are deliberately not caught: the recorder wraps every call, logs once and degrades, so a
 * read-only data dir or a full disk costs records rather than a tool call.
 */
export function jsonlSink(dir: string): TelemetrySink {
  let ready = false;
  return {
    write(date, lines) {
      if (lines.length === 0) {
        // A zero-line append would write a bare newline, which the report reader has to count as
        // an unparsed (torn) line. There is nothing to persist, so nothing is written.
        return;
      }
      if (!ready) {
        mkdirSync(dir, { recursive: true });
        ready = true;
      }
      appendFileSync(join(dir, `${date}.jsonl`), `${lines.join("\n")}\n`);
    },
  };
}

/** For a recorder that must exist but must not touch the disk (tests, a disabled switch). */
export function nullSink(): TelemetrySink {
  return {
    write() {
      // Discard: the recorder is real, the destination is not.
    },
  };
}
