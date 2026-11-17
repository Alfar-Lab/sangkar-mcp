/**
 * The single migration runner: two SQL streams, one ledger (`TECH-SPEC.md` §5.4.4).
 *
 * Every `migrations/*.sql` (produced by `drizzle-kit generate` from `schema.ts`) is applied
 * first, then every `migrations/raw/*.sql` (hand-written FTS5 and other objects Drizzle cannot
 * model). One file = one transaction, the file's name lands in `schema_migration`, and the
 * count of files lands in `meta.schema_version` (`DATABASE.md` §10). Forward-only: there are no
 * down migrations, and a database written by a newer build is refused, never downgraded.
 *
 * Deliberately not Drizzle's bundled migrator (the `bun-sqlite` one): it keeps a second ledger
 * (`__drizzle_migrations` with its own hash bookkeeping) and has nowhere to record a migration
 * that cannot be expressed as DDL — `meta.rebuild_required`. One graph store, one ledger.
 *
 * The runner works on the caller's `Database` and on the raw SQL text, so it imports neither
 * Drizzle nor the store's connection module (`store/db.ts` opens the file, this shapes it).
 */
import type { Database } from "bun:sqlite";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";

/** The two streams live here: this directory and its `raw/` child. */
export const MIGRATIONS_DIR = join(import.meta.dir, "migrations");

/** Ledger table. Not modeled in `schema.ts` on purpose: it belongs to the runner, not the graph. */
export const LEDGER_TABLE = "schema_migration";

/** `meta` keys the runner reads and writes (`DATABASE.md` §3.1, §10). */
export const META_SCHEMA_VERSION = "schema_version";
export const META_REBUILD_REQUIRED = "rebuild_required";

/** `drizzle-kit generate` separates statements with this comment (journal `breakpoints: true`). */
const STATEMENT_BREAKPOINT = "--> statement-breakpoint";

/**
 * A migration that half-applied must be re-runnable: SQLite rejects the statements that
 * already landed, and those two messages are the only ones we swallow.
 */
const IDEMPOTENT_ERROR = /already exists|duplicate column name/i;

/** One migration file, in application order. */
export interface MigrationFile {
  /** Ledger key: `0000_init` for the modeled stream, `raw/0001_fts` for the raw stream. */
  readonly name: string;
  /** Absolute path of the `.sql` file. */
  readonly path: string;
}

/** What one `migrate()` call did, including the two facts the caller must act on. */
export interface MigrateResult {
  /** Ledger names applied by this call, in order. Empty when the database was already current. */
  readonly applied: readonly string[];
  /** `meta.schema_version` before this call; `0` when the key is unset. */
  readonly fromVersion: number;
  /** `meta.schema_version` after this call: the number of committed migration files. */
  readonly schemaVersion: number;
  /** `meta.rebuild_required` was `1`: the indexer must throw away the graph and index fully. */
  readonly rebuildRequired: boolean;
}

export interface MigrateOptions {
  /** Directory holding both streams; defaults to the migrations shipped beside this file. */
  readonly migrationsDir?: string;
  /** Injected clock for `schema_migration.applied_at`; defaults to `new Date()`. */
  readonly now?: () => Date;
}

/** The `.sql` files of one directory, in name order; a missing directory is an empty stream. */
function sqlFilesIn(dir: string): string[] {
  if (!existsSync(dir)) {
    return [];
  }
  try {
    return readdirSync(dir, { withFileTypes: true })
      .filter((entry) => entry.isFile() && entry.name.endsWith(".sql"))
      .map((entry) => entry.name)
      .sort();
  } catch (e) {
    throw new AppError("internal", `cannot read migrations in ${dir}`, {
      hint: "the migrations directory is part of the package; reinstall or restore it",
      cause: e,
    });
  }
}

/**
 * Every migration in application order: the `drizzle-kit` stream first, the raw stream after
 * it, because raw SQL may depend on a modeled table (`node_fts` needs `node_fts_state`).
 */
export function listMigrations(migrationsDir: string = MIGRATIONS_DIR): MigrationFile[] {
  const modeled = sqlFilesIn(migrationsDir).map((file) => ({
    name: file.replace(/\.sql$/, ""),
    path: join(migrationsDir, file),
  }));
  const rawDir = join(migrationsDir, "raw");
  const raw = sqlFilesIn(rawDir).map((file) => ({
    name: `raw/${file.replace(/\.sql$/, "")}`,
    path: join(rawDir, file),
  }));
  return [...modeled, ...raw];
}

/** The schema version this build produces: one ledger row per migration file. */
export function schemaVersion(migrationsDir: string = MIGRATIONS_DIR): number {
  return listMigrations(migrationsDir).length;
}

function hasTable(db: Database, name: string): boolean {
  const row = db
    .query<{ present: number }, [string]>(
      "SELECT count(*) AS present FROM sqlite_master WHERE type = 'table' AND name = ?",
    )
    .get(name);
  return (row?.present ?? 0) > 0;
}

function readMeta(db: Database, key: string): string | null {
  if (!hasTable(db, "meta")) {
    return null;
  }
  const row = db
    .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
    .get(key);
  return row?.value ?? null;
}

/**
 * `meta.schema_version` as an integer; `0` when the database has not been migrated. A value
 * that is not a number is treated as 0 so the ledger, not a stray string, decides what ran.
 */
export function readSchemaVersion(db: Database): number {
  const raw = readMeta(db, META_SCHEMA_VERSION);
  if (raw === null) {
    return 0;
  }
  const parsed = Number.parseInt(raw, 10);
  return Number.isFinite(parsed) ? parsed : 0;
}

/** True when a migration asked for a full index instead of an incremental one. */
export function readRebuildRequired(db: Database): boolean {
  return readMeta(db, META_REBUILD_REQUIRED) === "1";
}

function ensureLedger(db: Database): void {
  db.exec(
    `CREATE TABLE IF NOT EXISTS ${LEDGER_TABLE} (\n` +
      "\tname       TEXT PRIMARY KEY NOT NULL,\n" +
      "\tapplied_at TEXT NOT NULL\n" +
      ")",
  );
}

function ledgerNames(db: Database): Set<string> {
  const rows = db.query<{ name: string }, []>(`SELECT name FROM ${LEDGER_TABLE}`).all();
  return new Set(rows.map((row) => row.name));
}

/**
 * Splits a file into executable statements. Drizzle's `--> statement-breakpoint` wins when
 * present; a hand-written file without breakpoints is split on `;` at a line end. Splitting is
 * what makes a single already-exists error survivable: only that statement is skipped.
 */
function splitStatements(sql: string): string[] {
  const chunks = sql.includes(STATEMENT_BREAKPOINT)
    ? sql.split(STATEMENT_BREAKPOINT)
    : sql.split(/;\s*(?:\r?\n|$)/);
  return chunks.map((chunk) => chunk.trim()).filter((chunk) => carriesSql(chunk));
}

/** A chunk that is only comments executes nothing and must not reach the driver. */
function carriesSql(chunk: string): boolean {
  return chunk.split("\n").some((line) => {
    const trimmed = line.trim();
    return trimmed !== "" && !trimmed.startsWith("--");
  });
}

function isIdempotentFailure(e: unknown): boolean {
  return e instanceof Error && IDEMPOTENT_ERROR.test(e.message);
}

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * Applies one file in its own transaction and records it. Statement-level failures that only
 * say "it is already there" are skipped, which is what makes a partially applied migration
 * re-runnable; any other failure rolls the whole file back and is reported with its name.
 */
function applyFile(db: Database, file: MigrationFile, now: () => Date): void {
  let statements: string[];
  try {
    statements = splitStatements(readFileSync(file.path, "utf8"));
  } catch (e) {
    throw new AppError("internal", `cannot read migration ${file.name}`, {
      hint: `restore ${file.path}`,
      cause: e,
    });
  }

  db.exec("BEGIN IMMEDIATE");
  try {
    for (const statement of statements) {
      try {
        db.exec(statement);
      } catch (e) {
        if (!isIdempotentFailure(e)) {
          throw e;
        }
        log.debug("migration statement skipped", { file: file.name, reason: messageOf(e) });
      }
    }
    db.query<unknown, [string, string]>(
      `INSERT INTO ${LEDGER_TABLE} (name, applied_at) VALUES (?, ?)`,
    ).run(file.name, now().toISOString());
    db.exec("COMMIT");
  } catch (e) {
    db.exec("ROLLBACK");
    throw new AppError("internal", `migration ${file.name} failed: ${messageOf(e)}`, {
      hint: `${file.path} was rolled back and not recorded; fix the SQL and retry`,
      cause: e,
    });
  }
  log.debug("migration applied", { name: file.name, statements: statements.length });
}

function writeSchemaVersion(db: Database, version: number): void {
  if (!hasTable(db, "meta")) {
    return;
  }
  db.query<unknown, [string, string]>(
    `INSERT INTO meta (key, value) VALUES (?, ?)
     ON CONFLICT (key) DO UPDATE SET value = excluded.value`,
  ).run(META_SCHEMA_VERSION, String(version));
}

/**
 * Brings `db` to this build's schema and reports what changed.
 *
 * Refuses with `schema_too_new` when the database was written by a newer build: a silent
 * downgrade would serve a graph the code cannot read. Otherwise applies the pending files in
 * order and returns `rebuildRequired` so the indexer can honour `meta.rebuild_required`.
 */
export function migrate(db: Database, options: MigrateOptions = {}): MigrateResult {
  const files = listMigrations(options.migrationsDir ?? MIGRATIONS_DIR);
  const version = files.length;
  const fromVersion = readSchemaVersion(db);

  if (fromVersion > version) {
    throw new AppError(
      "schema_too_new",
      `database schema_version ${fromVersion} is newer than this build (${version})`,
      {
        hint: "upgrade codegraph, or point --data-dir at a fresh directory; this build will not downgrade the graph",
      },
    );
  }
  if (db.inTransaction) {
    throw new AppError("internal", "migrate() must be called outside a transaction", {
      hint: "the runner opens one transaction per migration file",
    });
  }

  ensureLedger(db);
  const recorded = ledgerNames(db);
  const now = options.now ?? (() => new Date());

  const applied: string[] = [];
  for (const file of files) {
    if (recorded.has(file.name)) {
      continue;
    }
    applyFile(db, file, now);
    applied.push(file.name);
  }

  // Only a version change touches `meta`: a re-run on a current database is a true no-op.
  if (version !== fromVersion) {
    writeSchemaVersion(db, version);
  }
  if (applied.length > 0) {
    log.info("schema migrated", { from: fromVersion, to: version, applied });
  }

  return {
    applied,
    fromVersion,
    schemaVersion: version,
    rebuildRequired: readRebuildRequired(db),
  };
}
