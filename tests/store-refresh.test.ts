import { Database } from "bun:sqlite";
import { afterAll, describe, expect, test } from "bun:test";
import { existsSync, statSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { decode } from "@toon-format/toon";
import { type AppConfig, loadConfig } from "#shared/config";
import { openReader } from "#shared/store/db";
import { migrate, schemaVersion } from "#shared/store/migrate";
import { beginWrite, withStage } from "#shared/store/publish";
import type { ToolContext } from "#shared/store/read/context";
import { makeReader } from "#shared/store/read/reader";
import { fingerprintOf, openStore, refreshStore, type Store } from "../src/index";
import { handle } from "../src/tools/status/handler";

/**
 * The store is re-opened on a newer generation instead of answering from the one it started with.
 *
 * The unit half drives `openStore`/`refreshStore` against real published graphs under `mkdtemp`,
 * because the decision is a fact about a file — `VACUUM INTO`, `seal`, `rename` — and a stubbed
 * `stat` would prove the stub. Each case is one branch of that decision: an unchanged file (no
 * re-open), a published second generation (a swap, and the old handle closed), a newer schema (a
 * refusal that keeps the previous reader), a cursor that outlived its generation (refused), and the
 * absent → ready flow a server started before the first index goes through.
 *
 * Two more cases drive `status` over those same published files, because `graph.size_bytes` is the
 * one payload field that can be read from the wrong file: it has to be the size of the file the
 * reader opened, not of whatever is at `graph.db` when the answer is built. One swaps in a padded
 * generation and then publishes a third that nobody refreshed into the handle; the other is refused
 * a newer-schema copy and keeps serving the size it opened while the file on disk has grown. A live
 * `stat` would print the larger number in both.
 *
 * The last case is the one only an end-to-end run can show: a spawned `sangkar mcp` answering
 * `status`, a *different* process publishing over its data dir, and the same session reporting the
 * new generation on its next call — with every answer in the run carrying
 * `read_status.decisive === !read_status.stale`. No watcher runs in that server (there is none in
 * `mcp` at all: the daemon owns the pollers), so the refresh at the top of the next call is the only
 * way it can learn about the new generation.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "store-refresh-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";
const PUBLISHED_AT = "2026-11-19T09:00:00.000Z";

/** Generous for two real index runs and four protocol calls; short enough that a wedge fails. */
const RUN_TIMEOUT_MS = 120_000;

const A_SOURCE = `export function handleThing(): number {
  return 1;
}
`;

const B_SOURCE = `import { handleThing } from "./a";

export function callHandleThing(): number {
  return handleThing();
}
`;

/** Two files, one call edge: enough for the indexer to publish a generation with a stable shape. */
const SOURCES: Readonly<Record<string, string>> = {
  "src/a.ts": A_SOURCE,
  "src/b.ts": B_SOURCE,
};

interface Fixture {
  readonly home: string;
  readonly repoRoot: string;
  readonly dataDir: string;
  readonly dbPath: string;
}

const roots: string[] = [];

afterAll(async () => {
  for (const root of roots.splice(0)) {
    await rm(root, { recursive: true, force: true });
  }
});

/**
 * A tree, a data dir and a machine-level home per test, all outside the repository and all removed
 * afterwards. The home is not optional: the `index` verb registers the tree it indexed, so a spawn
 * without its own `SANGKAR_HOME` would write this machine's real registry.
 */
async function fixture(label: string): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), `sangkar-refresh-${label}-`));
  roots.push(root);
  const home = join(root, "home");
  const repoRoot = join(root, "repo");
  const dataDir = join(root, "data");
  await mkdir(home, { recursive: true });
  await mkdir(repoRoot, { recursive: true });
  await mkdir(dataDir, { recursive: true });
  return { home, repoRoot, dataDir, dbPath: join(dataDir, "graph.db") };
}

function configFor(f: Fixture): AppConfig {
  return loadConfig({
    argv: ["--repo-root", f.repoRoot, "--data-dir", f.dataDir],
    env: {
      SANGKAR_HOME: f.home,
      SANGKAR_CURSOR_KEY: CURSOR_KEY,
      SANGKAR_TELEMETRY: "off",
    },
    cwd: f.repoRoot,
  });
}

interface PublishOptions {
  /** Bytes of one padding `meta` value, so the published file is provably a different size. */
  readonly paddingBytes?: number;
  /** `1` models a graph written by a build that ships one more migration than this one. */
  readonly schemaVersionOffset?: number;
}

/**
 * Publishes one generation the way the worker does: `migrate()` on the stage (the only schema
 * author a fresh data dir has), one transaction writing the `generation` row plus the `meta` keys
 * the reader reads back, then `withStage`'s seal and rename.
 *
 * `options` is for the cases that need the file to differ in a way the graph does not: padding a
 * value makes the publish demonstrably larger, and a schema offset makes it unservable. Both land
 * inside the transaction that already writes `meta`, so a stage is never half of either.
 */
async function publish(
  f: Fixture,
  generation: string,
  seq: number,
  options: PublishOptions = {},
): Promise<void> {
  await withStage(f.dbPath, (staged) => {
    const scope = beginWrite(staged.path);
    try {
      migrate(scope.conn.sqlite);
      const insertGeneration = scope.conn.sqlite.query<unknown, [string, number, string]>(
        `INSERT OR REPLACE INTO generation
           (id, seq, published_at, base_commit, worktree_dirty, files_indexed, nodes, edges,
            duration_ms, mode, status)
         VALUES (?, ?, ?, NULL, 0, 1, 2, 0, 12, 'full', 'ok')`,
      );
      const upsertMeta = scope.conn.sqlite.query<unknown, [string, string]>(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
      );
      scope.transaction(() => {
        insertGeneration.run(generation, seq, PUBLISHED_AT);
        upsertMeta.run("generation", generation);
        // The root the guard compares: the fixture tree itself, so a refresh is not refused.
        upsertMeta.run("project_root", f.repoRoot);
        upsertMeta.run("project_name", "refresh-fixture");
        if (options.paddingBytes !== undefined) {
          upsertMeta.run("test.padding", "x".repeat(options.paddingBytes));
        }
        if (options.schemaVersionOffset !== undefined) {
          upsertMeta.run("schema_version", String(schemaVersion() + options.schemaVersionOffset));
        }
      });
    } finally {
      scope.close();
    }
  });
}

type Ready = Extract<Store, { kind: "ready" }>;

function readyOf(store: Store): Ready {
  if (store.kind !== "ready") {
    throw new Error(`expected a ready store, got "${store.kind}"`);
  }
  return store;
}

/** The generation a reader of the live file reports, i.e. what a refreshed handle must have read. */
function publishedGeneration(dbPath: string): string {
  const conn = openReader(dbPath);
  try {
    return makeReader(conn).metaFor().generation ?? "";
  } finally {
    conn.close();
  }
}

describe("refreshStore: the file on disk decides, never the store variant", () => {
  test("an unchanged file returns the identical store object", async () => {
    const f = await fixture("unchanged");
    const config = configFor(f);
    await publish(f, "g-aaaaaa", 1);

    const store = readyOf(openStore(config));
    expect(store.context.reader.metaFor().generation).toBe("g-aaaaaa");

    // Identity, not equality: the unchanged path must not open a second handle.
    expect(refreshStore(store, config)).toBe(store);
    store.conn.close();
  });

  test("a published second generation is what the refreshed reader reports, and the old handle is closed", async () => {
    const f = await fixture("newer");
    const config = configFor(f);
    await publish(f, "g-b00001", 1);

    const before = readyOf(openStore(config));
    const oldConn = before.conn;
    expect(before.context.reader.metaFor().generation).toBe("g-b00001");

    // The publish renames a new inode over the live path, so this handle is reading a file that is
    // no longer the one at `graph.db`.
    await publish(f, "g-b00002", 2);

    const after = readyOf(refreshStore(before, config));
    expect(after).not.toBe(before);
    expect(after.conn).not.toBe(oldConn);
    expect(after.context.reader.metaFor().generation).toBe("g-b00002");
    expect(after.fingerprint).toEqual(fingerprintOf(f.dbPath));
    expect(after.staleReason).toBeNull();

    // Closed, not leaked: the superseded handle refuses work instead of serving a swapped file.
    expect(() => oldConn.prepared("probe", (sqlite) => sqlite.query("SELECT 1"))).toThrow(
      "connection is closed",
    );
    after.conn.close();
  });

  test("a newer schema is a refusal, not a swap: the previous reader stays and admits it is behind", async () => {
    const f = await fixture("schema");
    const config = configFor(f);
    await publish(f, "g-c00001", 1);

    const before = readyOf(openStore(config));

    // A graph written by a build that ships more migrations than this one. The update is in place,
    // so the inode is unchanged and the size may be too: `mtimeMs` is what tells the two apart.
    const handle = new Database(f.dbPath, { create: false });
    try {
      handle
        .query<unknown, [string]>("UPDATE meta SET value = ? WHERE key = 'schema_version'")
        .run(String(schemaVersion() + 1));
    } finally {
      handle.close();
    }

    const after = readyOf(refreshStore(before, config));
    expect(after.conn).toBe(before.conn);
    expect(after.context.reader.metaFor().generation).toBe("g-c00001");
    expect(after.staleReason).toBe("a newer generation could not be opened");
    after.conn.close();
  });

  test("a cursor minted under the pre-swap generation is refused after the swap", async () => {
    const f = await fixture("cursor");
    const config = configFor(f);
    await publish(f, "g-d00001", 1);

    const before = readyOf(openStore(config));
    const argsHash = "0".repeat(32);
    const token = before.context.cursor.sign({
      generation: "g-d00001",
      tool: "search",
      argsHash,
      position: 4,
    });
    expect(
      before.context.cursor.verify(token, { generation: "g-d00001", tool: "search", argsHash }),
    ).toEqual({ ok: true, position: 4 });

    await publish(f, "g-d00002", 2);
    const after = readyOf(refreshStore(before, config));
    const generation = after.context.reader.metaFor().generation ?? "";
    expect(generation).toBe("g-d00002");

    // The boundary a swap must not move: a position minted in the previous generation addresses
    // rows this one may not have, so it expires rather than being served from the wrong graph.
    expect(after.context.cursor.verify(token, { generation, tool: "search", argsHash })).toEqual({
      ok: false,
      code: "cursor_expired",
      hint: "re-run the original call",
    });
    after.conn.close();
  });

  test("absent becomes ready when a publish creates the file", async () => {
    const f = await fixture("absent");
    const config = configFor(f);

    const absent = openStore(config);
    expect(absent.kind).toBe("absent");
    expect(existsSync(f.dbPath)).toBe(false);
    // No file is not a newer generation: there is nothing to compare, so nothing is re-opened.
    expect(refreshStore(absent, config)).toBe(absent);

    await publish(f, "g-e00001", 1);

    const ready = readyOf(refreshStore(absent, config));
    expect(ready.context.reader.metaFor().generation).toBe("g-e00001");
    expect(ready.fingerprint).toEqual(fingerprintOf(f.dbPath));
    expect(ready.staleReason).toBeNull();
    ready.conn.close();
  });

  test("a vanished file is not a newer generation: the ready store keeps serving what it read", async () => {
    const f = await fixture("vanished");
    const config = configFor(f);
    await publish(f, "g-f00001", 1);

    const ready = readyOf(openStore(config));
    await rm(f.dbPath, { force: true });

    expect(refreshStore(ready, config)).toBe(ready);
    expect(ready.context.reader.metaFor().generation).toBe("g-f00001");
    ready.conn.close();
  });
});

/** Enough padding that a published generation cannot share the previous file's size by accident. */
const PAD = 64 * 1024;

/**
 * The size `status` publishes for the graph file. Narrowed through the payload so a bootstrapping
 * or error answer fails here instead of comparing `undefined` against a byte count.
 */
function graphSize(ctx: ToolContext): number | undefined {
  const env = handle(ctx);
  return env.payload === null ? undefined : env.payload.graph.size_bytes;
}

describe("status graph.size_bytes: the file the reader opened", () => {
  test("an accepted swap reports the size it opened, not the third file at the same path", async () => {
    const f = await fixture("size-swap");
    const config = configFor(f);
    await publish(f, "g-e00001", 1);

    const first = readyOf(openStore(config));
    const size1 = statSync(f.dbPath).size;
    expect(first.context.dbSizeBytes).toBe(size1);
    expect(graphSize(first.context)).toBe(size1);

    await publish(f, "g-e00002", 2, { paddingBytes: PAD });
    const size2 = statSync(f.dbPath).size;
    // The guard against a vacuous case: a publish that did not grow the file would make every
    // equality below true for the wrong reason.
    expect(size2).not.toBe(size1);

    const second = readyOf(refreshStore(first, config));
    expect(second.context.dbSizeBytes).toBe(size2);
    expect(graphSize(second.context)).toBe(size2);

    // A third generation, published and deliberately not refreshed into this handle: a live `stat`
    // while building the answer would print `size3`, which is a file this reader never opened.
    await publish(f, "g-e00003", 3, { paddingBytes: 2 * PAD });
    const size3 = statSync(f.dbPath).size;
    expect(size3).not.toBe(size2);
    expect(second.context.reader.metaFor().generation).toBe("g-e00002");
    expect(graphSize(second.context)).toBe(size2);
    expect(graphSize(second.context)).not.toBe(size3);
    second.conn.close();
  });

  test("a refused newer-schema publish keeps reporting the size the previous reader opened", async () => {
    const f = await fixture("size-refused");
    const config = configFor(f);
    await publish(f, "g-f00001", 1);

    const before = readyOf(openStore(config));
    const served = statSync(f.dbPath).size;
    expect(before.context.dbSizeBytes).toBe(served);

    // A copy that is both larger and unservable: the refusal is what leaves the old reader serving
    // a generation whose size no longer matches the file at that path.
    await publish(f, "g-f00002", 2, { paddingBytes: 8 * PAD, schemaVersionOffset: 1 });
    const unserved = statSync(f.dbPath).size;
    expect(unserved).toBeGreaterThan(served);

    const refused = readyOf(refreshStore(before, config));
    expect(refused.conn).toBe(before.conn);
    expect(refused.staleReason).toBe("a newer generation could not be opened");
    expect(refused.context.reader.metaFor().generation).toBe("g-f00001");
    // The persistent divergence of issue #13, stated as a number: the answer is the served size,
    // while the file on disk — and any `stat` taken while answering — is `unserved`.
    expect(graphSize(refused.context)).toBe(served);
    expect(graphSize(refused.context)).not.toBe(unserved);
    refused.conn.close();
  });
});

/** One `tools/call` answer, narrowed to the three freshness members these tests assert. */
interface Answer {
  readonly status: string;
  readonly read_status: {
    readonly generation: string;
    readonly stale: boolean;
    readonly decisive?: boolean;
    readonly stale_reason?: string;
  };
}

interface IndexRun {
  readonly code: number;
  readonly stderr: string;
}

interface ServerSession {
  call(name: string, args: Record<string, unknown>): Promise<Answer>;
  finish(): Promise<{ exitCode: number; stderr: string; nonJsonLines: readonly string[] }>;
}

/**
 * The repository's own settings, removed: nothing about the child is inherited by accident, and the
 * machine-level home is the fixture's own — `sangkar index` registers the tree it indexed, so an
 * inherited `$HOME/.config/sangkar` would be written by this test.
 */
function cleanEnv(f: Fixture): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  env.SANGKAR_HOME = f.home;
  // The publish is the fixture's own: no telemetry files beside the graph it writes.
  env.SANGKAR_TELEMETRY = "off";
  return env;
}

/** The publishing half of the end-to-end case: a separate process, as in production. */
async function runIndex(f: Fixture, mode: "full" | "fast"): Promise<IndexRun> {
  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "index",
      "--mode",
      mode,
      "--repo-root",
      f.repoRoot,
      "--data-dir",
      f.dataDir,
    ],
    cwd: APP_ROOT,
    env: cleanEnv(f),
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    // Drained, not asserted on: an undrained pipe would wedge the child.
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  expect(stdout).toBe("");
  return { code, stderr };
}

/**
 * A server over stdio, driven the way a client drives it: one request written, one response read,
 * dispatched by the id it carried. The process owns no poller — `mcp` never starts one, the daemon
 * owns every watcher — so a generation published while it runs can only be noticed by the refresh
 * at the top of the next call. Its home is the fixture's own, so the registry it reads is the one
 * this test wrote.
 */
async function openServer(f: Fixture): Promise<ServerSession> {
  const proc = Bun.spawn({
    cmd: [
      process.execPath,
      "run",
      CLI_ENTRY,
      "mcp",
      "--repo-root",
      f.repoRoot,
      "--data-dir",
      f.dataDir,
    ],
    cwd: APP_ROOT,
    env: { ...cleanEnv(f), SANGKAR_CURSOR_KEY: CURSOR_KEY },
    stdin: "pipe",
    stdout: "pipe",
    stderr: "pipe",
  });

  const timer = setTimeout(() => proc.kill("SIGKILL"), RUN_TIMEOUT_MS);
  const reader = proc.stdout.getReader();
  const decoder = new TextDecoder();
  const nonJsonLines: string[] = [];
  let buffer = "";
  let nextId = 0;

  async function nextMessage(): Promise<Record<string, unknown>> {
    for (;;) {
      const newline = buffer.indexOf("\n");
      if (newline >= 0) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        if (line === "") {
          continue;
        }
        try {
          return JSON.parse(line) as Record<string, unknown>;
        } catch {
          // Recorded rather than thrown: the corruption is asserted once, at `finish`.
          nonJsonLines.push(line);
          continue;
        }
      }
      const chunk = await reader.read();
      if (chunk.done === true) {
        throw new Error("the server closed stdout before answering");
      }
      buffer += decoder.decode(chunk.value, { stream: true });
    }
  }

  async function request(
    method: string,
    params: Record<string, unknown>,
  ): Promise<Record<string, unknown>> {
    nextId += 1;
    const id = nextId;
    proc.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", id, method, params })}\n`);
    await proc.stdin.flush();
    const response = await nextMessage();
    expect(response.id).toBe(id);
    return response;
  }

  try {
    const initialized = await request("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      capabilities: {},
      clientInfo: { name: "store-refresh-test", version: "0.0.0" },
    });
    const serverInfo = (initialized.result as { serverInfo?: { name?: string } } | undefined)
      ?.serverInfo;
    expect(serverInfo?.name).toBe("sangkar");
    // A notification has no id and earns no answer, so it is written and never awaited.
    proc.stdin.write(
      `${JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" })}\n`,
    );
    await proc.stdin.flush();
  } catch (error) {
    clearTimeout(timer);
    proc.kill();
    throw error;
  }

  let finished = false;
  return {
    async call(name, args) {
      const response = await request("tools/call", { name, arguments: args });
      const result = response.result as { content?: readonly { text?: string }[] } | undefined;
      const text = result?.content?.[0]?.text;
      if (typeof text !== "string") {
        throw new Error(`${name}: the tool result carried no text`);
      }
      // TOON by default: this server is not started with `--json`, so `decode` is how a client reads
      // the answer.
      return decode(text) as unknown as Answer;
    },
    async finish() {
      if (finished) {
        throw new Error("this session already finished");
      }
      finished = true;
      clearTimeout(timer);
      await reader.cancel();
      proc.stdin.end();
      const [stderr, exitCode] = await Promise.all([new Response(proc.stderr).text(), proc.exited]);
      return { exitCode, stderr, nonJsonLines };
    },
  };
}

describe("stdio: a publish from another process is what the next answer reports", () => {
  test(
    "a server started before the first index answers from the published generation, and every answer's decisive matches its staleness",
    async () => {
      const f = await fixture("stdio");
      for (const [path, source] of Object.entries(SOURCES)) {
        const target = join(f.repoRoot, path);
        await mkdir(dirname(target), { recursive: true });
        await writeFile(target, source, "utf8");
      }

      const server = await openServer(f);
      const answers: Answer[] = [];
      try {
        // The state a client meets before the first index: nothing to answer from.
        const absent = await server.call("status", {});
        answers.push(absent);
        expect(absent.status).toBe("bootstrapping");
        expect(absent.read_status.generation).toBe("");
        expect(absent.read_status.stale).toBe(true);
        expect(absent.read_status.decisive).toBe(false);
        expect(existsSync(f.dbPath)).toBe(false);

        // Another process publishes. No watcher runs here (there is none in `mcp`), so the refresh
        // at the top of the next call is the only way this server can learn about it.
        const firstRun = await runIndex(f, "full");
        if (firstRun.code !== 0) {
          throw new Error(`the first index exited ${firstRun.code}:\n${firstRun.stderr}`);
        }
        const first = publishedGeneration(f.dbPath);
        expect(first).toMatch(/^g-[0-9a-f]{6}$/);

        const afterFirst = await server.call("status", {});
        answers.push(afterFirst);
        expect(afterFirst.status).toBe("ok");
        expect(afterFirst.read_status.generation).toBe(first);
        expect(afterFirst.read_status.stale).toBe(false);
        expect(afterFirst.read_status.decisive).toBe(true);

        // A second generation: the tree changes, another process publishes, and the *same* running
        // server answers from the newer graph rather than the one it opened at startup.
        await writeFile(
          join(f.repoRoot, "src/a.ts"),
          `${A_SOURCE}export function added(): number {
  return 2;
}
`,
          "utf8",
        );
        const secondRun = await runIndex(f, "fast");
        if (secondRun.code !== 0) {
          throw new Error(`the second index exited ${secondRun.code}:\n${secondRun.stderr}`);
        }
        const second = publishedGeneration(f.dbPath);
        expect(second).not.toBe(first);

        const afterSecond = await server.call("status", {});
        answers.push(afterSecond);
        expect(afterSecond.read_status.generation).toBe(second);
        expect(afterSecond.read_status.stale).toBe(false);
        expect(afterSecond.read_status.decisive).toBe(true);

        // Every answer of the run, including the bootstrapping one: `stale && decisive` is never
        // true, which is the only way a client may treat a `stale: false` as final.
        for (const answer of answers) {
          expect({
            status: answer.status,
            decisive: answer.read_status.decisive,
            stale: answer.read_status.stale,
          }).toEqual({
            status: answer.status,
            decisive: !answer.read_status.stale,
            stale: answer.read_status.stale,
          });
        }
      } finally {
        const report = await server.finish();
        expect(report.nonJsonLines).toEqual([]);
        expect(report.exitCode).toBe(0);
      }
    },
    RUN_TIMEOUT_MS,
  );
});
