export type LogLevel = "debug" | "info" | "warn" | "error";

export type LogFields = Record<string, unknown>;

export interface Logger {
  debug(msg: string, fields?: LogFields): void;
  info(msg: string, fields?: LogFields): void;
  warn(msg: string, fields?: LogFields): void;
  error(msg: string, fields?: LogFields): void;
}

function stringify(record: LogFields): string {
  try {
    return JSON.stringify(record);
  } catch {
    // A log call that throws would take the server down mid-session; report the failure instead.
    return JSON.stringify({
      ts: new Date().toISOString(),
      level: "error",
      msg: "log serialisation failed",
    });
  }
}

function write(level: LogLevel, msg: string, fields: LogFields = {}): void {
  // stderr only: stdout carries the MCP protocol (TECH-SPEC.md §11).
  process.stderr.write(`${stringify({ ts: new Date().toISOString(), level, msg, ...fields })}\n`);
}

/** The only logger. Writes JSON lines `{ts, level, msg, ...fields}` to stderr. */
export const log: Logger = {
  debug: (msg, fields) => write("debug", msg, fields),
  info: (msg, fields) => write("info", msg, fields),
  warn: (msg, fields) => write("warn", msg, fields),
  error: (msg, fields) => write("error", msg, fields),
};
