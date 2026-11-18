import { afterAll, describe, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AppConfig } from "#shared/config";
import { createCursors } from "#shared/cursor";
import type { Envelope } from "#shared/envelope";
import type { LogFields, Logger } from "#shared/log";
import { lockPath } from "#shared/store/lock";
import type { CursorFacade, ToolContext } from "#shared/store/read/context";
import type { GraphReader } from "#shared/store/read/reader";
import {
  handle,
  type IndexPayload,
  streamWorker,
  type WorkerEvent,
  type WorkerRunner,
  workerInvocation,
} from "../src/tools/index/handler";
import { input, manifest } from "../src/tools/index/manifest";

/**
 * `index` is the only tool that starts a process, so these tests assert the two things a repair
 * path can get wrong: mapping the worker's four exit codes onto envelopes, and starting the child
 * against the tree the server is bound to while leaving the database to the child alone.
 *
 * The mapping is asserted through the injection seam (`handle`'s third parameter), so the four
 * cases run without indexing anything. One test drives the real {@link streamWorker} against a
 * short-lived Bun script, because the seam must not hide the plumbing that parses and forwards a
 * child's stderr — a stub that returns events would prove nothing about the code that reads them.
 */

const GENERATION_LIVE = "g-prev";
const NOW = new Date("2026-01-04T12:00:00Z");

const dirs: string[] = [];

function tempDir(): string {
  const dir = mkdtempSync(join(tmpdir(), "sangkar-index-tool-"));
  dirs.push(dir);
  return dir;
}

function cleanup(): void {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
}

/** A logger that records instead of writing: the handler's log calls are part of its contract. */
function recordingLogger(lines: Array<{ msg: string; level: string; fields: LogFields }>): Logger {
  const record =
    (level: string) =>
    (msg: string, fields: LogFields = {}) => {
      lines.push({ msg, level, fields });
    };
  return {
    debug: record("debug"),
    info: record("info"),
    warn: record("warn"),
    error: record("error"),
  };
}

/** A hand-written reader: every method present, types only from `#shared/*`. */
function fakeReader(generation: string | null): GraphReader {
  return {
    findByQn: () => null,
    findByName: () => [],
    members: () => [],
    nodesByFile: () => [],
    inbound: () => [],
    outbound: () => [],
    metricsFor: () => null,
    rowsForPaths: () => ({ indexed: [], notIndexed: [] }),
    unresolvedForFiles: () => [],
    fileByPath: () => null,
    allFiles: () => [],
    censusByLanguage: () => [],
    metaFor: () => ({
      projectName: "fake",
      projectRoot: "/tmp/fake",
      generation,
      indexedAt: generation === null ? null : "2026-01-03T00:00:00Z",
      indexerState: "idle",
      consecutiveFailures: 0,
    }),
    latestGeneration: () => null,
    removedSymbolsFor: () => [],
    entryPoints: () => [],
    hotspots: () => [],
    testGaps: () => [],
    readNodeFts: () => [],
  };
}

interface Harness {
  readonly ctx: ToolContext;
  readonly dir: string;
  readonly logLines: Array<{ msg: string; level: string; fields: LogFields }>;
}

function harness(generation: string | null = GENERATION_LIVE): Harness {
  const dir = tempDir();
  const repoRoot = join(dir, "repo");
  const dataDir = join(dir, "data");
  const logLines: Array<{ msg: string; level: string; fields: LogFields }> = [];
  const config: AppConfig = {
    repoRoot,
    dataDir,
    cursorKey: "k".repeat(32),
    budgets: { defaultTokens: 1_200, maxTokens: 32_000 },
  };
  const cursors: CursorFacade = createCursors({ key: "fake-cursor-key-material-32-bytes" });
  const ctx: ToolContext = {
    reader: fakeReader(generation),
    cursor: cursors,
    log: recordingLogger(logLines),
    now: () => NOW,
    config,
  };
  return { ctx, dir, logLines };
}

/** One stderr line in the child's own log format (`#shared/log`'s `{ts, level, msg, …}`). */
function line(msg: string, level: string, fields: Record<string, unknown> = {}): string {
  return JSON.stringify({ ts: NOW.toISOString(), level, msg, ...fields });
}

function event(msg: string, level: string, fields: Record<string, unknown> = {}): WorkerEvent {
  return { msg, level: level as WorkerEvent["level"], fields, raw: line(msg, level, fields) };
}

function runner(exitCode: number, events: readonly WorkerEvent[] = []): WorkerRunner {
  return async () => ({ exitCode, events });
}

/** The payload a successful envelope carries; a helper so assertions read as payload fields. */
function payloadOf(env: Envelope<IndexPayload | null>): IndexPayload {
  if (env.payload === null) {
    throw new Error("expected an index payload");
  }
  return env.payload;
}

describe("index manifest", () => {
  test("is not profile-gated and carries TOOLS-SPEC.md §8's description verbatim", () => {
    expect(manifest.name).toBe("index");
    expect("profile" in manifest).toBe(false);
    expect(manifest.description).toBe(
      "Admin: force a full or fast index, or repair a broken one. Not for normal use — the server " +
        "keeps the graph current automatically.",
    );
  });

  test("declares exactly the two optional arguments with their documented defaults", () => {
    expect(Object.keys(input)).toEqual(["force", "mode"]);
    expect(input.force.default).toBe(false);
    expect(input.mode.values).toEqual(["full", "fast"]);
    expect(input.mode.default).toBe("full");
  });
});

describe("workerInvocation", () => {
  test("runs the CLI entry in worker mode with the server's own tree and data dir", () => {
    const { ctx } = harness();
    const invocation = workerInvocation(ctx, { force: false, mode: "full" });

    expect(invocation.cmd.at(-2)).toBe("--mode");
    expect(invocation.cmd.at(-1)).toBe("full");
    expect(invocation.cmd).toContain("worker");
    expect(invocation.cmd[0]).toBe(process.execPath);
    expect(invocation.cmd.some((part) => part.endsWith(join("bin", "sangkar.ts")))).toBe(true);
    // The two paths the child acts on come from the config, never from this process's cwd, and
    // they travel as ordered argv flags rather than as environment variables.
    const argv = invocation.cmd;
    expect(argv.slice(argv.indexOf("--repo-root"), argv.indexOf("--repo-root") + 2)).toEqual([
      "--repo-root",
      ctx.config.repoRoot,
    ]);
    expect(argv.slice(argv.indexOf("--data-dir"), argv.indexOf("--data-dir") + 2)).toEqual([
      "--data-dir",
      ctx.config.dataDir,
    ]);
    expect(argv.indexOf("--repo-root")).toBe(argv.indexOf("worker") + 1);
    expect(argv.indexOf("--data-dir")).toBe(argv.indexOf("--repo-root") + 2);
    expect(invocation.env.SANGKAR_REPO_ROOT).toBeUndefined();
    expect(invocation.env.SANGKAR_DATA_DIR).toBeUndefined();
  });

  test("force is passed through, and absent when not requested", () => {
    const { ctx } = harness();
    expect(workerInvocation(ctx, { force: true, mode: "fast" }).cmd).toContain("--force");
    expect(workerInvocation(ctx, { force: false, mode: "fast" }).cmd).not.toContain("--force");
  });
});

describe("handle: exit code mapping", () => {
  test("0 publishes: the payload is the child's report, previous generation included", async () => {
    const { ctx } = harness();
    const report = (exitCode: number, events: readonly WorkerEvent[]): WorkerRunner =>
      runner(exitCode, events);

    const env = await handle(
      ctx,
      {},
      report(0, [
        event("worker.file_not_parsed", "warn", {
          path: "docs/broken.ts",
          reason: "grammar_unavailable",
        }),
        event("worker.published", "info", {
          generation: "g-new",
          repoRoot: "/repo",
          files: 12,
          changed: 12,
          removed: 0,
          edges: 340,
          durationMs: 1_450,
          mode: "full",
        }),
      ]),
    );

    expect(env.status).toBe("ok");
    const payload = payloadOf(env);
    expect(payload.mode).toBe("full");
    expect(payload.duration_ms).toBe(1_450);
    expect(payload.previous_generation).toBe(GENERATION_LIVE);
    // 12 offered, 1 of them logged a parse failure, so 11 produced a unit.
    expect(payload.files).toEqual({ scanned: 12, parsed: 11, failed: 1, skipped: 0 });
    expect(env.read_status.generation).toBe("g-new");
    expect(env.read_status.stale).toBe(false);
    expect(env.coverage.indexed).toBe(11);
    expect(env.coverage.failed).toBe(1);
    // The child never reports the gap list, so coverage may not claim completeness.
    expect(env.coverage.complete).toBe(false);
    expect(env.summary).toContain("Full index published");
    expect(env.summary).toContain("340 edge(s)");
    expect(env.truncated).toBe(false);
    expect(env.next).toBeNull();
  });

  test("2 is ok with a nothing-to-do summary and no published counts", async () => {
    const { ctx } = harness();
    const env = await handle(
      ctx,
      { mode: "fast" },
      runner(2, [event("worker.nothing_to_do", "info", { files: 7 })]),
    );

    expect(env.status).toBe("ok");
    const payload = payloadOf(env);
    expect(payload.mode).toBe("fast");
    expect(payload.files).toEqual({ scanned: 7, parsed: 0, failed: 0, skipped: 0 });
    expect(payload.previous_generation).toBe(GENERATION_LIVE);
    expect(env.summary).toContain("Nothing to do");
    // Nothing published, so the live generation answered and is unchanged.
    expect(env.read_status.generation).toBe(GENERATION_LIVE);
  });

  test("75 is a failure with index_locked and a hint naming the lock file", async () => {
    const { ctx } = harness();
    const env = await handle(
      ctx,
      {},
      runner(75, [event("worker.locked", "warn", { pid: 4242, retryAfterMs: 1_000 })]),
    );

    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("index_locked");
    expect(env.error?.hint).toContain(lockPath(ctx.config.dataDir));
    expect(env.error?.hint).toContain("write.lock");
    expect(env.error?.retryable).toBe(true);
    expect(env.error?.retry_after_ms).toBe(1_000);
    // The holder is named, and the live generation did not move.
    expect(env.error?.message).toContain("4242");
    expect(env.read_status.generation).toBe(GENERATION_LIVE);
    expect(env.read_status.stale).toBe(true);
    expect(env.payload).toBeNull();
  });

  test("1 is internal and carries the child's own diagnosis", async () => {
    const { ctx } = harness();
    const env = await handle(
      ctx,
      {},
      runner(1, [event("worker.failed", "error", { reason: "SQLiteError: no such table: file" })]),
    );

    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("internal");
    expect(env.error?.message).toContain("SQLiteError: no such table: file");
    expect(env.payload).toBeNull();
  });

  test("an unexpected exit code, an unparsable last line, and a failed spawn are all internal", async () => {
    const { ctx } = harness();
    const unexpected = await handle(
      ctx,
      {},
      runner(3, [event("worker.stderr", "warn", { line: "boom" })]),
    );
    expect(unexpected.error?.code).toBe("internal");
    expect(unexpected.error?.message).toContain("exit 3");
    expect(unexpected.error?.message).toContain("boom");

    const spawnFailed = await handle(ctx, {}, async () => {
      throw new Error("ENOENT: bun");
    });
    expect(spawnFailed.status).toBe("error");
    expect(spawnFailed.error?.message).toContain("could not start the index worker");
  });

  test("a 0 without a published line is a broken child contract, not an empty success", async () => {
    const { ctx } = harness();
    const env = await handle(ctx, {}, runner(0, []));
    expect(env.status).toBe("error");
    expect(env.error?.message).toContain("without publishing a report");
  });
});

describe("handle: the writer stays in the child", () => {
  test("the handler never touches the database file it reports on", async () => {
    const { ctx, dir } = harness();
    // A published database the tool must leave exactly as it is: same bytes, same mtime.
    const livePath = join(dir, "graph.db");
    writeFileSync(livePath, "not really a database");
    const before = statSync(livePath);

    await handle(
      ctx,
      {},
      runner(0, [event("worker.published", "info", { files: 1, mode: "fast" })]),
    );

    const after = statSync(livePath);
    expect(readFileSync(livePath, "utf8")).toBe("not really a database");
    expect(after.mtimeMs).toBe(before.mtimeMs);
    expect(after.size).toBe(before.size);
  });

  test("the handler imports only #shared and its own sibling, and writes no stdout", () => {
    const source = readFileSync(
      join(import.meta.dir, "..", "src", "tools", "index", "handler.ts"),
      "utf8",
    );
    const specifiers = [...source.matchAll(/from\s+"([^"]+)"/g)].flatMap((match) =>
      match[1] === undefined ? [] : [match[1]],
    );
    expect(specifiers.length).toBeGreaterThan(0);
    // The writer is the child's: the indexer is not importable from a tool, and a tool that tried
    // would be a process indexing inside the server, which is the one thing this tool must not do.
    expect(specifiers.filter((value) => value.includes("indexer"))).toEqual([]);
    expect(specifiers.filter((value) => value.includes("drizzle"))).toEqual([]);
    expect(
      specifiers.filter(
        (value) =>
          !value.startsWith("#shared/") && !value.startsWith("./") && !value.startsWith("node:"),
      ),
    ).toEqual([]);
    // stdout is the MCP protocol, so a tool may not write there even by accident.
    expect(source).not.toContain("console.");
    expect(source).not.toContain("process.stdout");
    expect(source).toContain("Bun.spawn");
  });
});

describe("streamWorker", () => {
  test("streams a real child's stderr into the logger and keeps every event", async () => {
    const { ctx, dir, logLines } = harness();
    const script = join(dir, "fake-worker.ts");
    writeFileSync(
      script,
      [
        `process.stderr.write(${JSON.stringify(line("worker.change_plan", "debug", { discovered: 2 }))} + "\\n");`,
        `process.stderr.write(${JSON.stringify(line("worker.published", "info", { generation: "g-new", files: 2, mode: "fast", durationMs: 12 }))} + "\\n");`,
        `process.stderr.write("a line that is not JSON\\n");`,
        `process.exit(0);`,
      ].join("\n"),
    );

    const report = await streamWorker(ctx, { cmd: [process.execPath, "run", script], env: {} });

    expect(report.exitCode).toBe(0);
    expect(report.events.map((entry) => entry.msg)).toEqual([
      "worker.change_plan",
      "worker.published",
      "worker.stderr",
    ]);
    // Every line reached the server's log under the child's own message name.
    expect(logLines.map((entry) => entry.msg)).toEqual([
      "worker.change_plan",
      "worker.published",
      "worker.stderr",
    ]);
    expect(logLines[0]?.level).toBe("debug");
    expect(logLines[2]?.fields.line).toBe("a line that is not JSON");
  });

  test("a child that exits 75 reports the lock and no events are lost", async () => {
    const { ctx, dir } = harness();
    const script = join(dir, "locked-worker.ts");
    writeFileSync(
      script,
      [
        `process.stderr.write(${JSON.stringify(line("worker.locked", "warn", { pid: 7 }))} + "\\n");`,
        `process.exit(75);`,
      ].join("\n"),
    );

    const env = await handle(ctx, {}, (context, invocation) =>
      streamWorker(context, { cmd: [process.execPath, "run", script], env: invocation.env }),
    );

    expect(env.status).toBe("error");
    expect(env.error?.code).toBe("index_locked");
    expect(env.error?.message).toContain("7");
  });

  test("the handler forwards the invocation the runner receives unchanged", async () => {
    const { ctx } = harness();
    let seen: readonly string[] = [];
    await handle(ctx, { force: true, mode: "fast" }, async (_context, invocation) => {
      seen = invocation.cmd;
      return { exitCode: 2, events: [event("worker.nothing_to_do", "info", { files: 0 })] };
    });
    expect(seen).toContain("--force");
    expect(seen.at(-1)).toBe("--force");
    expect(seen).toContain("fast");
  });
});

afterAll(cleanup);
