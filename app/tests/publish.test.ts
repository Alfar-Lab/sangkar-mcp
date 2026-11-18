import { Database } from "bun:sqlite";
import { afterEach, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdtemp, readdir, readFile, rm, stat, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AppError } from "#shared/errors";
import { migrate } from "#shared/store/migrate";
import {
  beginWrite,
  discard,
  publish,
  STAGE_INFIX,
  seal,
  stage,
  stagePath,
  withStage,
} from "#shared/store/publish";

/**
 * The publish protocol of `DATABASE.md` §5. The assertions are about files on disk — the stage
 * name, the sealed PRAGMA values read back, the renamed-over live file, the absent sidecars and
 * the sha256 of a live file that survived an aborted run — because "a partial graph is never
 * published" is a statement about the data directory, not about a return value.
 *
 * Every database is built by the real `migrate()` under `mkdtemp`, so a stage is a real graph and
 * nothing is ever written inside the repository.
 */

const dirs: string[] = [];

async function tempDir(): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), "sangkar-publish-"));
  dirs.push(dir);
  return dir;
}

afterEach(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

function errorOf(run: () => unknown): unknown {
  try {
    run();
    return null;
  } catch (e) {
    return e;
  }
}

/** A real graph: the shipped migrations plus the `meta.generation` row that names it. */
function buildLive(livePath: string, generation: string): void {
  const handle = new Database(livePath, { create: true });
  try {
    migrate(handle);
    handle
      .query<unknown, [string]>("INSERT INTO meta (key, value) VALUES ('generation', ?)")
      .run(generation);
  } finally {
    handle.close();
  }
}

function readMeta(path: string, key: string): string | null {
  const handle = new Database(path, { readonly: true });
  try {
    const row = handle
      .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
      .get(key);
    return row?.value ?? null;
  } finally {
    handle.close();
  }
}

async function sha256(path: string): Promise<string> {
  const hasher = new Bun.CryptoHasher("sha256");
  hasher.update(await readFile(path));
  return hasher.digest("hex");
}

async function dirNames(dir: string): Promise<string[]> {
  return (await readdir(dir)).sort();
}

/** Anything the protocol is supposed to have removed: a stage file or a database sidecar. */
async function leftovers(dir: string): Promise<string[]> {
  return (await dirNames(dir)).filter(
    (name) =>
      name.includes(STAGE_INFIX) ||
      name.endsWith("-wal") ||
      name.endsWith("-shm") ||
      name.endsWith("-journal"),
  );
}

describe("stage", () => {
  test("names the seed graph.db.stage.<rand> and fills it with VACUUM INTO from the live graph", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const liveBytes = (await stat(livePath)).size;
    const before = await sha256(livePath);

    const staged = stage(livePath);
    try {
      expect(staged.path).toMatch(/graph\.db\.stage\.[0-9a-f]{12}$/);
      expect(staged.path.startsWith(`${livePath}${STAGE_INFIX}`)).toBe(true);
      expect(stagePath(livePath)).not.toBe(staged.path);
      expect(staged.seededFrom).toBe("live");

      // A VACUUM INTO seed is a readable graph, not a byte copy: the schema and the rows came
      // across, and the seed was rebuilt by SQLite (so no free pages and no inherited -wal).
      expect(readMeta(staged.path, "generation")).toBe("gen-1");
      expect(readMeta(staged.path, "schema_version")).not.toBeNull();
      expect((await stat(staged.path)).size).toBeGreaterThan(0);
      expect((await stat(staged.path)).size).toBeLessThanOrEqual(liveBytes);
      expect(existsSync(`${staged.path}-wal`)).toBe(false);

      // Staging never touches the live graph.
      expect(await sha256(livePath)).toBe(before);
    } finally {
      discard(staged.path);
    }
    expect(await dirNames(dir)).toEqual(["graph.db"]);
  });

  test("starts an empty file on force full and leaves the live graph alone", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const before = await sha256(livePath);

    const staged = stage(livePath, { forceFull: true });
    try {
      expect(staged.seededFrom).toBe("empty");
      expect((await stat(staged.path)).size).toBe(0);

      const handle = new Database(staged.path, { readonly: true });
      try {
        const row = handle
          .query<{ n: number }, []>("SELECT count(*) AS n FROM sqlite_master WHERE type = 'table'")
          .get();
        expect(row).toEqual({ n: 0 });
      } finally {
        handle.close();
      }
    } finally {
      discard(staged.path);
    }

    expect(await sha256(livePath)).toBe(before);
    expect(await dirNames(dir)).toEqual(["graph.db"]);
  });
});

describe("beginWrite", () => {
  test("runs the callback immediately inside BEGIN IMMEDIATE and returns its value", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const staged = stage(livePath);

    try {
      const scope = beginWrite(staged.path);
      try {
        const value = scope.transaction((tx) => {
          // Inside the callback the transaction is already open and the write is already visible.
          expect(scope.conn.sqlite.inTransaction).toBe(true);
          tx.run("INSERT INTO meta (key, value) VALUES ('written', 'immediately')");
          return scope.conn.sqlite
            .query<{ n: number }, []>("SELECT count(*) AS n FROM meta WHERE key = 'written'")
            .get();
        });
        expect(value).toEqual({ n: 1 });
        expect(scope.conn.sqlite.inTransaction).toBe(false);

        // The stage is the writer's file, so it is in WAL while the scope is open.
        expect(existsSync(`${staged.path}-wal`)).toBe(true);
      } finally {
        scope.close();
      }

      // close() releases the WAL, so the stage is already the self-contained file seal expects.
      expect(existsSync(`${staged.path}-wal`)).toBe(false);
      expect(existsSync(`${staged.path}-shm`)).toBe(false);
      const handle = new Database(staged.path, { readonly: true });
      try {
        expect(handle.query("PRAGMA journal_mode").get()).toEqual({ journal_mode: "delete" });
        expect(readMeta(staged.path, "written")).toBe("immediately");
      } finally {
        handle.close();
      }
    } finally {
      discard(staged.path);
    }

    expect(await dirNames(dir)).toEqual(["graph.db"]);
  });
});

describe("seal", () => {
  test("checkpoint, journal_mode=DELETE, synchronous=FULL and quick_check all read back", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const staged = stage(livePath);

    const scope = beginWrite(staged.path);
    try {
      scope.transaction((tx) => {
        tx.run("INSERT INTO meta (key, value) VALUES ('sealed', 'yes')");
      });
    } finally {
      scope.close();
    }

    try {
      const report = seal(staged.path);

      expect(report.checkpoint.busy).toBe(0);
      expect(report.journalMode).toBe("delete");
      expect(report.synchronous).toBe(2);
      expect(report.quickCheck).toEqual(["ok"]);

      // journal_mode=DELETE dropped the sidecars, so the stage is one self-contained file.
      expect(existsSync(`${staged.path}-wal`)).toBe(false);
      expect(existsSync(`${staged.path}-shm`)).toBe(false);
      expect(readMeta(staged.path, "sealed")).toBe("yes");
    } finally {
      discard(staged.path);
    }
  });

  test("refuses with AppError internal while the write scope is still open", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const staged = stage(livePath);

    const scope = beginWrite(staged.path);
    try {
      scope.transaction((tx) => {
        tx.run("INSERT INTO meta (key, value) VALUES ('unsealed', 'yes')");
      });
      expect(existsSync(`${staged.path}-wal`)).toBe(true);

      // SQLite will not change the journal mode under a second open connection: it either
      // reports the database as locked or leaves journal_mode at "wal". Both are refused.
      const error = errorOf(() => seal(staged.path));
      expect(error).toBeInstanceOf(AppError);
      expect((error as AppError).code).toBe("internal");
    } finally {
      scope.close();
      discard(staged.path);
    }
  });
});

describe("publish", () => {
  test("renames the sealed stage over the live file and deletes the stale sidecars", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const before = await sha256(livePath);

    const staged = stage(livePath);
    const scope = beginWrite(staged.path);
    try {
      scope.transaction((tx) => {
        tx.run("INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', 'gen-2')");
      });
    } finally {
      scope.close();
    }
    seal(staged.path);

    // The previous generation's sidecars, as a crashed writer would have left them.
    await writeFile(`${livePath}-wal`, "");
    await writeFile(`${livePath}-shm`, "");

    publish(staged.path, livePath);

    expect(existsSync(staged.path)).toBe(false);
    expect(existsSync(`${livePath}-wal`)).toBe(false);
    expect(existsSync(`${livePath}-shm`)).toBe(false);
    expect(readMeta(livePath, "generation")).toBe("gen-2");
    expect(await sha256(livePath)).not.toBe(before);
    expect(await dirNames(dir)).toEqual(["graph.db"]);
  });

  test("refuses a path that is not a stage file", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const before = await sha256(livePath);

    const error = errorOf(() => publish(livePath, livePath));
    expect(error).toBeInstanceOf(AppError);
    expect((error as AppError).code).toBe("internal");
    expect(await sha256(livePath)).toBe(before);
  });
});

describe("withStage", () => {
  test("stages, writes, seals and publishes, returning the callback's value", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const before = await sha256(livePath);

    const echoed = await withStage(livePath, (staged) => {
      expect(staged.seededFrom).toBe("live");
      expect(readMeta(staged.path, "generation")).toBe("gen-1");

      const scope = beginWrite(staged.path);
      try {
        scope.transaction((tx) => {
          tx.run("INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', 'gen-2')");
        });
      } finally {
        scope.close();
      }
      return staged.path;
    });

    expect(echoed).toContain(STAGE_INFIX);
    expect(await sha256(livePath)).not.toBe(before);
    expect(readMeta(livePath, "generation")).toBe("gen-2");

    // The published file is sealed, not WAL: it is one self-contained file that passes quick_check.
    const handle = new Database(livePath, { readonly: true });
    try {
      expect(handle.query("PRAGMA journal_mode").get()).toEqual({ journal_mode: "delete" });
      expect(handle.query("PRAGMA quick_check(1)").get()).toEqual({ quick_check: "ok" });
    } finally {
      handle.close();
    }

    expect(await dirNames(dir)).toEqual(["graph.db"]);
    expect(await leftovers(dir)).toEqual([]);
  });

  test("a throw discards the stage and leaves the live sha256 unchanged", async () => {
    const dir = await tempDir();
    const livePath = join(dir, "graph.db");
    buildLive(livePath, "gen-1");
    const before = await sha256(livePath);
    const beforeNames = await dirNames(dir);

    const failure = new Error("indexer failed mid-load");
    let stageSeen = "";

    const thrown = await withStage(livePath, (staged) => {
      stageSeen = staged.path;

      const scope = beginWrite(staged.path);
      try {
        scope.transaction((tx) => {
          // A graph that is half-written: meta already claims gen-2, no file/node rows exist.
          tx.run("INSERT OR REPLACE INTO meta (key, value) VALUES ('generation', 'gen-2')");
        });
      } finally {
        scope.close();
      }
      throw failure;
    }).then(
      () => null,
      (error: unknown) => error,
    );

    expect(thrown).toBe(failure);
    expect(stageSeen).not.toBe("");
    expect(existsSync(stageSeen)).toBe(false);

    // Nothing reached publish: same bytes, same sha256, same rows in the live graph.
    expect(await sha256(livePath)).toBe(before);
    expect(readMeta(livePath, "generation")).toBe("gen-1");
    expect(await dirNames(dir)).toEqual(beforeNames);
    expect(await leftovers(dir)).toEqual([]);
  });
});
