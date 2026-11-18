import { afterEach, describe, expect, spyOn, test } from "bun:test";
import { mkdtemp, readFile, rm, utimes, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import {
  type Conn,
  db,
  openReader,
  openWriter,
  prepared,
  STATEMENT_CACHE_CAPACITY,
  statementCacheStats,
} from "#shared/store/db";
import {
  acquireLock,
  heartbeat,
  inspect,
  lockPath,
  release,
  STALE_AFTER_MS,
} from "#shared/store/lock";
import { meta } from "#shared/store/schema";

/**
 * Connection lifecycle, statement cache and write lock. Lock tests own this file; the db tests
 * live here too because the cache and the PRAGMAs are only observable through a real connection.
 * Every database and lock file is created under `mkdtemp`, never inside the repository.
 */

const dirs: string[] = [];
const handles: Conn[] = [];

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-lock-"));
  dirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const handle of handles.splice(0)) {
    handle.close();
  }
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

/** A pid that is certainly not running: probe downward from the Linux pid ceiling. */
function deadPid(): number {
  for (let pid = 4_194_304; pid > 4_000_000; pid -= 1) {
    try {
      process.kill(pid, 0);
    } catch (error) {
      if ((error as { code?: string }).code === "ESRCH") {
        return pid;
      }
    }
  }
  throw new Error("could not find an unused pid to simulate a dead holder");
}

function track(conn: Conn): Conn {
  handles.push(conn);
  return conn;
}

async function writeLockFile(dataDir: string, content: unknown): Promise<string> {
  const path = lockPath(dataDir);
  await writeFile(path, typeof content === "string" ? content : JSON.stringify(content), "utf8");
  return path;
}

describe("write lock", () => {
  test("acquire creates write.lock carrying this pid and a startedAt", async () => {
    const dir = await tempDir();

    const handle = await acquireLock(dir);
    expect(handle.path).toBe(lockPath(dir));
    expect(handle.pid).toBe(process.pid);

    const stored = JSON.parse(await readFile(lockPath(dir), "utf8")) as {
      pid: number;
      startedAt: string;
    };
    expect(stored.pid).toBe(process.pid);
    expect(Number.isNaN(Date.parse(stored.startedAt))).toBe(false);

    const state = await inspect(dir);
    expect(state).toEqual({ held: true, pid: process.pid, stale: false });

    await release(handle);
    expect(await inspect(dir)).toEqual({ held: false, pid: null, stale: false });
  });

  test("a second acquire fails with index_locked and leaves the first holder's file", async () => {
    const dir = await tempDir();
    const first = await acquireLock(dir);

    const error = await acquireLock(dir).then(
      () => null,
      (thrown: unknown) => thrown,
    );
    expect(error).toBeInstanceOf(AppError);
    expect((error as AppError).code).toBe("index_locked");
    expect((error as AppError).retryable).toBe(true);

    expect((await inspect(dir)).pid).toBe(first.pid);
    await release(first);
  });

  test("a lock whose pid is gone is reclaimed and the reclaim is logged as a warning", async () => {
    const dir = await tempDir();
    const holder = deadPid();
    await writeLockFile(dir, { pid: holder, startedAt: new Date().toISOString() });

    expect(await inspect(dir)).toEqual({ held: true, pid: holder, stale: true });

    const warnings: string[] = [];
    const spy = spyOn(process.stderr, "write").mockImplementation(
      (chunk: string | Uint8Array): boolean => {
        warnings.push(String(chunk));
        return true;
      },
    );
    let reclaimed: Awaited<ReturnType<typeof acquireLock>>;
    try {
      reclaimed = await acquireLock(dir);
    } finally {
      spy.mockRestore();
    }

    expect(reclaimed.pid).toBe(process.pid);
    expect(await inspect(dir)).toEqual({ held: true, pid: process.pid, stale: false });
    expect(warnings.join("")).toContain("reclaiming stale write lock");

    await release(reclaimed);
  });

  test("an alive holder with a heartbeat older than 60 s is stale and reclaimable", async () => {
    const dir = await tempDir();
    const path = await writeLockFile(dir, { pid: process.pid, startedAt: "2020-01-01T00:00:00Z" });
    const old = new Date(Date.now() - STALE_AFTER_MS - 1_000);
    await utimes(path, old, old);

    expect((await inspect(dir)).stale).toBe(true);

    const reclaimed = await acquireLock(dir);
    expect(reclaimed.pid).toBe(process.pid);
    await release(reclaimed);
  });

  test("heartbeat refreshes the mtime so a live holder is not judged stale", async () => {
    const dir = await tempDir();
    const handle = await acquireLock(dir);
    const old = new Date(Date.now() - STALE_AFTER_MS - 1_000);
    await utimes(handle.path, old, old);
    expect((await inspect(dir)).stale).toBe(true);

    await heartbeat(handle);
    expect((await inspect(dir)).stale).toBe(false);

    await release(handle);
  });

  test("release leaves a lock another process has taken over", async () => {
    const dir = await tempDir();
    const handle = await acquireLock(dir);
    const other = deadPid();
    await writeLockFile(dir, { pid: other, startedAt: new Date().toISOString() });

    await release(handle);
    expect((await inspect(dir)).pid).toBe(other);
  });
});

describe("db connections", () => {
  test("writer applies every PRAGMA value from DATABASE.md §3.7", async () => {
    const dir = await tempDir();
    const conn = track(openWriter(join(dir, "graph.db")));

    const read = (name: string): unknown =>
      Object.values(
        conn.sqlite.query<Record<string, unknown>, []>(`PRAGMA ${name}`).get() ?? {},
      )[0];

    expect(read("journal_mode")).toBe("wal");
    expect(read("synchronous")).toBe(1);
    expect(read("foreign_keys")).toBe(1);
    expect(read("busy_timeout")).toBe(10_000);
    expect(read("cache_size")).toBe(-65_536);
    expect(read("mmap_size")).toBe(268_435_456);
    expect(read("journal_size_limit")).toBe(67_108_864);
  });

  test("reader is readonly, applies its own PRAGMAs and reads through Drizzle", async () => {
    const dir = await tempDir();
    const path = join(dir, "graph.db");
    const writer = track(openWriter(path));
    writer.sqlite.exec("create table meta (key text primary key, value text not null)");
    db(writer).insert(meta).values({ key: "schema_version", value: "1" }).run();
    writer.close();
    handles.splice(handles.indexOf(writer), 1);

    const reader = track(openReader(path));

    expect(reader.mode).toBe("reader");
    expect(reader.sqlite.query("PRAGMA foreign_keys").get()).toEqual({ foreign_keys: 1 });
    expect(reader.sqlite.query("PRAGMA busy_timeout").get()).toEqual({ timeout: 10_000 });
    expect(reader.sqlite.query("PRAGMA cache_size").get()).toEqual({ cache_size: -16_384 });
    expect(reader.sqlite.query("PRAGMA mmap_size").get()).toEqual({ mmap_size: 268_435_456 });

    expect(db(reader).select().from(meta).all()).toEqual([{ key: "schema_version", value: "1" }]);

    expect(() => reader.sqlite.exec("create table forbidden (x integer)")).toThrow();
    expect(() => reader.sqlite.exec("insert into meta (key, value) values ('k', 'v')")).toThrow();
  });

  test("a reader cannot create the database and a URI filename is refused", async () => {
    const dir = await tempDir();
    expect(() => openReader(join(dir, "absent.db"))).toThrow();
    // bun:sqlite cannot open `file:…?immutable=1`; the guard says so instead of SQLite.
    expect(() => openReader(`file:${join(dir, "absent.db")}?immutable=1`)).toThrow(/URI filename/);
  });

  test("the prepared cache is capped at 200 entries and stays a cache", async () => {
    const dir = await tempDir();
    const conn = track(openWriter(join(dir, "graph.db")));

    const build = (sqlite: Conn["sqlite"]): object => sqlite.prepare("select 1");

    const first = prepared(conn, "warm", build);
    expect(prepared(conn, "warm", build)).toBe(first);
    expect(statementCacheStats(conn).hits).toBe(1);
    expect(statementCacheStats(conn).misses).toBe(1);

    for (let i = 0; i < 500; i += 1) {
      conn.prepared(`shape-${i}`, build);
    }

    // 501 distinct shapes were inserted ("warm" plus 500), so the cache must have dropped 301.
    const stats = statementCacheStats(conn);
    expect(stats.size).toBe(STATEMENT_CACHE_CAPACITY);
    expect(stats.capacity).toBe(200);
    expect(stats.misses).toBe(501);
    expect(stats.evictions).toBe(501 - STATEMENT_CACHE_CAPACITY);

    // The no-argument form is what `status.health` reports from, so it must work without a handle.
    const aggregate = statementCacheStats();
    expect(aggregate.capacity).toBe(STATEMENT_CACHE_CAPACITY);
    expect(aggregate.size).toBeGreaterThanOrEqual(stats.size);
  });
});
