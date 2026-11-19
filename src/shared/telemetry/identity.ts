import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

/**
 * The two identifiers every telemetry record carries. Both are random bytes: nothing about the
 * repository, the machine or the user may leak into a record, so neither is derived from them.
 */
export interface InstallIdentity {
  /** Stable for one UTC day, unlinkable from one day to the next. */
  readonly installId: string;
  /** True when this process wrote today's `install_id.json`; the session record reports it. */
  readonly isFirstRun: boolean;
}

const FILE_NAME = "install_id.json";
const ID_BYTES = 16;

/** What `<dir>/install_id.json` holds, after the shape check in `readId`. */
interface InstallFile {
  readonly installId: string;
  readonly date: string;
}

function mintId(): string {
  return randomBytes(ID_BYTES).toString("hex");
}

/** The file-name form of a date in UTC, so every process on a host agrees on "today". */
export function utcDateOf(date: Date): string {
  return date.toISOString().slice(0, 10);
}

/** `null` for every failure — absent, unreadable, torn or foreign JSON all mean "rotate". */
function readId(file: string): InstallFile | null {
  try {
    const parsed: unknown = JSON.parse(readFileSync(file, "utf8"));
    if (typeof parsed !== "object" || parsed === null) {
      return null;
    }
    const { install_id: installId, date } = parsed as Record<string, unknown>;
    if (typeof installId !== "string" || installId === "" || typeof date !== "string") {
      return null;
    }
    return { installId, date };
  } catch {
    return null;
  }
}

type WriteOutcome = "created" | "exists" | "failed";

/**
 * `wx` is the race breaker: of two processes starting on the same day exactly one creates the file,
 * and the loser learns it lost from `EEXIST` instead of overwriting the winner's id.
 */
function writeId(file: string, dir: string, installId: string, date: string): WriteOutcome {
  try {
    mkdirSync(dir, { recursive: true });
    writeFileSync(file, `${JSON.stringify({ install_id: installId, date })}\n`, { flag: "wx" });
    return "created";
  } catch (error) {
    return (error as { code?: string }).code === "EEXIST" ? "exists" : "failed";
  }
}

export function resolveInstallId(dir: string, now: Date): InstallIdentity {
  const file = join(dir, FILE_NAME);
  const today = utcDateOf(now);

  const current = readId(file);
  if (current !== null && current.date === today) {
    return { installId: current.installId, isFirstRun: false };
  }

  // `wx` cannot replace an existing file, so a stale or unparseable copy is cleared first; without
  // this, the first start of a new day would land in the `EEXIST` branch and keep yesterday's id.
  // `force` makes this a no-op when there is no file at all.
  try {
    rmSync(file, { force: true });
  } catch {
    // Unremovable for the same reason the create below fails; that path already swallows it.
  }

  const minted = mintId();
  const outcome = writeId(file, dir, minted, today);
  if (outcome === "created") {
    return { installId: minted, isFirstRun: true };
  }

  // Another process won the create between our read and our write: its id is the day's id.
  const winner = readId(file);
  if (winner !== null) {
    return { installId: winner.installId, isFirstRun: false };
  }

  // A write that failed (read-only data dir, full disk) must not stop a process from running: the
  // caller's sink is guarded too, and an id that never reaches disk costs one day's grouping only.
  return { installId: minted, isFirstRun: false };
}

/** One id per process, never persisted, so it cannot be joined across runs. */
export function newSessionId(): string {
  return mintId();
}
