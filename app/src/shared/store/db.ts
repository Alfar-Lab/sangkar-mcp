/**
 * Connection lifecycle for the graph database: the two PRAGMA sets of `DATABASE.md` §3.7, one
 * Drizzle instance per handle and the bounded prepared-statement cache that makes the measured
 * 42.7 µs (re-built builder) → 1.7 µs (prepared) difference reachable from every read path.
 *
 * Readers get their own handle rather than borrowing the writer's: the publish path renames a
 * stage file over the live one, and an open handle must keep serving the previous generation
 * until it is re-opened (`DATABASE.md` §4).
 *
 * Two traps this module closes once, so no caller has to remember them:
 * - `bun:sqlite` cannot open a URI filename (`file:…?immutable=1`), so a `file:` path is rejected
 *   with a message instead of failing later inside SQLite;
 * - a prepared statement belongs to the connection that built it, so the cache is per handle.
 *   A process-wide one would hand a statement bound to a closed database to the next reader.
 */
import { Database } from "bun:sqlite";
import { type BunSQLiteDatabase, drizzle } from "drizzle-orm/bun-sqlite";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";
import * as schema from "#shared/store/schema";

/** `DATABASE.md` §3.7, reader column. */
const READER_CACHE_SIZE = -16_384;
/** `DATABASE.md` §3.7, build column. */
const WRITER_CACHE_SIZE = -65_536;
const BUSY_TIMEOUT_MS = 10_000;
/** 64 MiB of journal kept before SQLite truncates it; the bulk load is what makes it necessary. */
const JOURNAL_SIZE_LIMIT = 67_108_864;
/** The mmap window both handles ask for; identical on purpose (`DATABASE.md` §3.7). */
const MMAP_SIZE = 268_435_456;

/** `SQLite` value of `synchronous = NORMAL`; `FULL` is 2. */
const SYNCHRONOUS_NORMAL = 1;

const READER_PRAGMAS = [
  "foreign_keys = ON",
  `busy_timeout = ${BUSY_TIMEOUT_MS}`,
  `cache_size = ${READER_CACHE_SIZE}`,
  `mmap_size = ${MMAP_SIZE}`,
] as const;

const WRITER_PRAGMAS = [
  "journal_mode = WAL",
  "synchronous = NORMAL",
  `cache_size = ${WRITER_CACHE_SIZE}`,
  `mmap_size = ${MMAP_SIZE}`,
  `busy_timeout = ${BUSY_TIMEOUT_MS}`,
  `journal_size_limit = ${JOURNAL_SIZE_LIMIT}`,
  "foreign_keys = ON",
] as const;

/** The only two handles the store opens; the mode decides the PRAGMA set, nothing else. */
export type ConnMode = "reader" | "writer";

export interface StatementCacheStats {
  /** Entries currently retained by this handle (or the sum over all live handles). */
  readonly size: number;
  /** Hard ceiling per handle: {@link STATEMENT_CACHE_CAPACITY}. */
  readonly capacity: number;
  readonly hits: number;
  readonly misses: number;
  /** Entries dropped because `size` would have exceeded `capacity`. */
  readonly evictions: number;
}

/**
 * The ceiling on retained prepared statements. `TECH-SPEC.md` §11 makes boundedness observable:
 * `statementCacheStats()` reports it, so growth is a number and not a guess.
 */
export const STATEMENT_CACHE_CAPACITY = 200;

export type GraphDb = BunSQLiteDatabase<typeof schema>;

export interface Conn {
  /** The filesystem path this handle was opened from, never a URI. */
  readonly path: string;
  readonly mode: ConnMode;
  /** The raw connection, for the raw-SQL paths (FTS5, PRAGMAs) Drizzle cannot model. */
  readonly sqlite: Database;
  /**
   * Returns the statement cached under `sqlKey`, building it once on a miss. `sqlKey` must name
   * the query shape, including any parameter arity that changes the generated SQL.
   */
  prepared<T extends object>(sqlKey: string, build: (sqlite: Database) => T): T;
  statementCacheStats(): StatementCacheStats;
  /** Idempotent; the handle is unusable afterwards. */
  close(): void;
}

const instances = new WeakMap<Conn, GraphDb>();
const liveCaches = new Set<StatementCache>();

/** A bounded LRU: `Map` keeps insertion order, so the first key is the least recently used. */
class StatementCache {
  readonly capacity = STATEMENT_CACHE_CAPACITY;
  readonly #entries = new Map<string, object>();
  #hits = 0;
  #misses = 0;
  #evictions = 0;

  get<T extends object>(key: string, build: () => T): T {
    const cached = this.#entries.get(key);
    if (cached !== undefined) {
      this.#hits += 1;
      this.#entries.delete(key);
      this.#entries.set(key, cached);
      return cached as T;
    }
    this.#misses += 1;
    const built = build();
    this.#entries.set(key, built);
    this.#evict();
    return built;
  }

  #evict(): void {
    while (this.#entries.size > this.capacity) {
      const oldest = this.#entries.keys().next();
      if (oldest.done === true) {
        return;
      }
      // Dropping the reference is the whole eviction: the statement belongs to `bun:sqlite`, and
      // finalizing it here would pull it out from under a caller that still holds it.
      this.#entries.delete(oldest.value);
      this.#evictions += 1;
    }
  }

  clear(): void {
    this.#entries.clear();
  }

  stats(): StatementCacheStats {
    return {
      size: this.#entries.size,
      capacity: this.capacity,
      hits: this.#hits,
      misses: this.#misses,
      evictions: this.#evictions,
    };
  }
}

function pragmaValue(sqlite: Database, name: string): string {
  const row = sqlite.query<Record<string, unknown>, []>(`PRAGMA ${name}`).get();
  if (row === null) {
    throw new AppError("internal", `PRAGMA ${name} returned no row`);
  }
  const values = Object.values(row);
  if (values.length !== 1) {
    throw new AppError("internal", `PRAGMA ${name} returned ${values.length} columns`);
  }
  const value = values[0];
  return typeof value === "string" ? value : String(value);
}

function expectPragma(sqlite: Database, name: string, expected: string): void {
  const actual = pragmaValue(sqlite, name);
  if (actual !== expected) {
    throw new AppError("internal", `PRAGMA ${name} is ${actual}, expected ${expected}`);
  }
}

/**
 * Applies a PRAGMA set and reads every value back. The read-back is the point: a silently
 * ignored PRAGMA is how a store ends up with foreign keys off or a 2 MB page cache, and neither
 * shows up until the data is already wrong or the query is already slow.
 */
function applyPragmas(sqlite: Database, mode: ConnMode): void {
  for (const pragma of mode === "reader" ? READER_PRAGMAS : WRITER_PRAGMAS) {
    sqlite.exec(`PRAGMA ${pragma}`);
  }

  expectPragma(sqlite, "foreign_keys", "1");
  expectPragma(sqlite, "busy_timeout", String(BUSY_TIMEOUT_MS));
  expectPragma(
    sqlite,
    "cache_size",
    String(mode === "reader" ? READER_CACHE_SIZE : WRITER_CACHE_SIZE),
  );

  // mmap_size is a ceiling the OS may refuse to grant in full, and a smaller window is slower
  // rather than wrong, so this one is reported instead of thrown.
  const mmap = Number(pragmaValue(sqlite, "mmap_size"));
  if (mmap < MMAP_SIZE) {
    log.warn("mmap_size below the requested window", { requested: MMAP_SIZE, granted: mmap });
  }

  if (mode === "writer") {
    const journalMode = pragmaValue(sqlite, "journal_mode");
    if (journalMode !== "wal") {
      throw new AppError("unsupported_filesystem", `journal_mode is ${journalMode}, not wal`, {
        hint: "WAL needs a writable local directory; point --data-dir at one",
      });
    }
    expectPragma(sqlite, "synchronous", String(SYNCHRONOUS_NORMAL));
    expectPragma(sqlite, "journal_size_limit", String(JOURNAL_SIZE_LIMIT));
  }
}

/** `bun:sqlite` cannot open a URI filename, so reject one before SQLite reports it confusingly. */
function assertFilesystemPath(path: string): void {
  if (path.startsWith("file:")) {
    throw new AppError("internal", `bun:sqlite cannot open the URI filename ${path}`, {
      hint: "pass a plain filesystem path",
    });
  }
}

function open(path: string, mode: ConnMode): Conn {
  assertFilesystemPath(path);

  const sqlite =
    mode === "reader"
      ? new Database(path, { readonly: true })
      : new Database(path, { create: true });

  try {
    applyPragmas(sqlite, mode);
  } catch (error) {
    // A half-configured handle must not survive: the caller sees the PRAGMA that failed and no
    // file descriptor stays open on the data dir.
    sqlite.close();
    throw error;
  }

  const cache = new StatementCache();
  liveCaches.add(cache);
  let closed = false;

  const conn: Conn = {
    path,
    mode,
    sqlite,
    prepared<T extends object>(sqlKey: string, build: (database: Database) => T): T {
      if (closed) {
        throw new AppError("internal", "connection is closed");
      }
      return cache.get(sqlKey, () => build(sqlite));
    },
    statementCacheStats: () => cache.stats(),
    close(): void {
      if (closed) {
        return;
      }
      closed = true;
      liveCaches.delete(cache);
      cache.clear();
      sqlite.close();
    },
  };

  instances.set(conn, drizzle(sqlite, { schema }));
  return conn;
}

/** A read-only handle: `readonly: true`, so a write fails in SQLite rather than in a review. */
export function openReader(path: string): Conn {
  return open(path, "reader");
}

/** The single writer's handle, in WAL with `synchronous = NORMAL` (`DATABASE.md` §3.7). */
export function openWriter(path: string): Conn {
  return open(path, "writer");
}

/** The Drizzle instance bound to this handle; build queries from it, `.prepare()` them, cache. */
export function db(conn: Conn): GraphDb {
  const instance = instances.get(conn);
  if (instance === undefined) {
    throw new AppError("internal", "connection was not opened by store/db");
  }
  return instance;
}

/** Free-function form of {@link Conn.prepared}; the handle comes first, as everywhere in store. */
export function prepared<T extends object>(
  conn: Conn,
  sqlKey: string,
  build: (sqlite: Database) => T,
): T {
  return conn.prepared(sqlKey, build);
}

/**
 * Cache statistics for one handle, or summed over every live handle when called with none — so
 * `status.health` can report growth without holding a connection.
 */
export function statementCacheStats(conn?: Conn): StatementCacheStats {
  if (conn !== undefined) {
    return conn.statementCacheStats();
  }
  let size = 0;
  let hits = 0;
  let misses = 0;
  let evictions = 0;
  for (const cache of liveCaches) {
    const stats = cache.stats();
    size += stats.size;
    hits += stats.hits;
    misses += stats.misses;
    evictions += stats.evictions;
  }
  return { size, capacity: STATEMENT_CACHE_CAPACITY, hits, misses, evictions };
}
