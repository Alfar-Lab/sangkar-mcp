import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { createCursors } from "#shared/cursor";
import { canonicalRoot, readRegistry } from "#shared/registry";
import { openReader } from "#shared/store/db";
import { makeReader } from "#shared/store/read/reader";

/**
 * One `sangkar mcp` process, several registered repositories.
 *
 * The composition root used to answer for exactly one tree: the one it was started on. It now reads
 * the registry once, resolves every call's `repo` (a path or a basename), opens that repository's
 * own graph from a pool, and records the call into that repository's telemetry directory — so the
 * properties below are the ones only a real server over real graphs can show:
 *
 * - an answer comes from the graph the call named, and two repos answer differently;
 * - a name nothing registered is `not_found` and the hint lists the registry;
 * - a cwd outside every registered tree is `ambiguous` rather than a guess, while a cwd inside one
 *   answers from it and a lone registered repo is the default;
 * - a page token minted for one repository does not resume in another (`cursor_mismatch`), because
 *   the cursor facade is bound to the root that opened it;
 * - `--repo-root` is a *default*: it answers when nothing is named and loses to an explicit `repo`,
 *   and an unregistered pin still answers (that is the ad-hoc invocation the CLI always allowed);
 * - the `index` verb registers the tree it indexed, unless `--no-register` says otherwise.
 *
 * Two properties justify the shape of the file. Every spawn gets its own `mkdtemp` `SANGKAR_HOME`
 * and every `SANGKAR_*` variable is deleted first, because `index` now writes a registry: nothing
 * here may touch `$HOME/.config/sangkar`. And the graphs are published by spawning `sangkar index`
 * rather than by writing rows, because the thing under test is the process an operator runs.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** `#shared/cursor` refuses anything under 32 bytes, so the fixture has to be realistic. */
const CURSOR_KEY = "multi-repo-test-cursor-key-0123456789";

const PROTOCOL_VERSION = "2024-11-05";

/** Three cold indexes and a handful of sessions; short enough that a wedge fails the suite. */
const RUN_TIMEOUT_MS = 120_000;

/** One symbol per tree, so an answer proves *which* graph it came from, not just that it answered. */
const SOURCES_A: Readonly<Record<string, string>> = {
  "src/a.ts": `export function alphaThing(): number {
  return 1;
}
`,
};

const SOURCES_B: Readonly<Record<string, string>> = {
  "src/b.ts": `export function betaThing(): number {
  return 2;
}
`,
};

const SOURCES_C: Readonly<Record<string, string>> = {
  "src/c.ts": `export function gammaThing(): number {
  return 3;
}
`,
};

interface Fixture {
  /** The suite's one `mkdtemp`, removed in `afterAll`. */
  readonly root: string;
  /** The machine home `repo-a` and `repo-b` are registered in. */
  readonly home: string;
  readonly repoA: string;
  readonly repoB: string;
  /** Indexed only by the verb test, in a home of its own: never registered in `home`. */
  readonly repoC: string;
  /** Never registered and never indexed: the ad-hoc `--repo-root` case. */
  readonly loose: string;
  /** A cwd inside no tree at all. */
  readonly elsewhere: string;
}

let f: Fixture;
const roots: string[] = [];

afterAll(async () => {
  for (const root of roots.splice(0)) {
    await rm(root, { recursive: true, force: true });
  }
});

/** A directory under the suite's temp root, created and removed with it. */
async function dir(base: string, name: string): Promise<string> {
  const path = join(base, name);
  await mkdir(path, { recursive: true });
  return path;
}

/** A tree's sources, written where discovery will find them. */
async function writeTree(
  repoRoot: string,
  sources: Readonly<Record<string, string>>,
): Promise<void> {
  for (const [path, source] of Object.entries(sources)) {
    const target = join(repoRoot, path);
    await mkdir(resolve(target, ".."), { recursive: true });
    await writeFile(target, source, "utf8");
  }
}

/**
 * The repository's own variables, removed, then the fixture's own home and key: an inherited
 * `SANGKAR_HOME` would decide which registry the child reads, which is the one thing this suite is
 * about, and an inherited key would make every cursor in the run a per-process accident.
 */
function cleanEnv(home: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  env.SANGKAR_HOME = home;
  env.SANGKAR_CURSOR_KEY = CURSOR_KEY;
  // Nothing writes telemetry beside a graph this suite publishes.
  env.SANGKAR_TELEMETRY = "off";
  return env;
}

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

/** One CLI invocation, in its own cwd and its own machine home. */
async function spawnCli(args: readonly string[], cwd: string, home: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: cleanEnv(home),
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, code] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { code, stdout, stderr };
}

/** `sangkar index` for one tree, in `home`: the only thing that publishes a graph here. */
async function runIndex(
  repoRoot: string,
  home: string,
  extra: readonly string[] = [],
): Promise<Run> {
  return spawnCli(["index", "--mode", "full", "--repo-root", repoRoot, ...extra], APP_ROOT, home);
}

/** The roots the registry names, read through the module the process itself uses. */
function registered(home: string): readonly string[] {
  return readRegistry(join(home, "sangkar.json")).repos;
}

/** The generation the tree's published graph carries: what an answer about that tree must report. */
function generationOf(repoRoot: string): string {
  const conn = openReader(join(repoRoot, ".sangkar", "graph.db"));
  try {
    return makeReader(conn).metaFor().generation ?? "";
  } finally {
    conn.close();
  }
}

/** One `tools/call` answer, narrowed to the members these tests assert. */
interface Answer {
  readonly status: string;
  readonly read_status: { readonly generation: string; readonly stale: boolean };
  readonly payload?: { readonly results?: readonly Record<string, unknown>[] } | null;
  readonly error?: { readonly code: string; readonly message: string; readonly hint?: string };
}

interface Session {
  call(name: string, args: Record<string, unknown>): Promise<Answer>;
  finish(): Promise<{ exitCode: number; nonJsonLines: readonly string[] }>;
}

interface SessionOptions {
  readonly cwd: string;
  readonly home: string;
  /** The `--repo-root` pin, or nothing: the flag is what the pin cases are about. */
  readonly pin?: string;
}

/**
 * A server over stdio, driven the way a client drives it: one request written, one response read,
 * dispatched by the id it carried. `cwd` is the process's working directory, which the selector
 * reads, so a test that is about the implicit default gets a session started where it means.
 */
async function session(options: SessionOptions): Promise<Session> {
  const cmd = [process.execPath, "run", CLI_ENTRY, "mcp"];
  if (options.pin !== undefined) {
    cmd.push("--repo-root", options.pin);
  }
  const proc = Bun.spawn({
    cmd,
    cwd: options.cwd,
    env: cleanEnv(options.home),
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
      clientInfo: { name: "multi-repo-test", version: "0.0.0" },
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
      return JSON.parse(text) as Answer;
    },
    async finish() {
      if (finished) {
        throw new Error("this session already finished");
      }
      finished = true;
      clearTimeout(timer);
      await reader.cancel();
      proc.stdin.end();
      const [exitCode] = await Promise.all([proc.exited, new Response(proc.stderr).text()]);
      return { exitCode, nonJsonLines };
    },
  };
}

/** Runs one session to completion: every answer asserted, then the exit path checked once. */
async function withSession(
  options: SessionOptions,
  body: (s: Session) => Promise<void>,
): Promise<void> {
  const s = await session(options);
  try {
    await body(s);
  } finally {
    const report = await s.finish();
    expect(report.nonJsonLines).toEqual([]);
    expect(report.exitCode).toBe(0);
  }
}

/**
 * `repo-a` and `repo-b` are published by the `index` verb under one home, so they are registered by
 * the same code path an operator's first run goes through. `repo-c` and `loose` stay unregistered.
 */
beforeAll(async () => {
  const root = await mkdtemp(join(tmpdir(), "sangkar-multi-repo-"));
  roots.push(root);
  const home = await dir(root, "home");
  const repoA = await dir(root, "repo-a");
  const repoB = await dir(root, "repo-b");
  const repoC = await dir(root, "repo-c");
  const loose = await dir(root, "loose");
  const elsewhere = await dir(root, "elsewhere");
  await writeTree(repoA, SOURCES_A);
  await writeTree(repoB, SOURCES_B);
  await writeTree(repoC, SOURCES_C);
  f = { root, home, repoA, repoB, repoC, loose, elsewhere };

  for (const repoRoot of [repoA, repoB]) {
    const run = await runIndex(repoRoot, home);
    if (run.code !== 0) {
      throw new Error(`the index of ${repoRoot} exited ${run.code}:\n${run.stderr}`);
    }
  }
  expect(registered(home)).toEqual([canonicalRoot(repoA), canonicalRoot(repoB)]);
}, RUN_TIMEOUT_MS);

describe("a call answers from the repository it names", () => {
  test(
    "a basename and a path select the same graph, and two repos answer differently",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home }, async (s) => {
        const byName = await s.call("status", { repo: "repo-a" });
        // The absolute path of the *other* repo: the selector matches a path before a basename, so
        // this is the half a basename-only lookup would silently answer from the wrong tree.
        const byPath = await s.call("status", { repo: f.repoB });

        expect(byName.status).toBe("ok");
        expect(byPath.status).toBe("ok");
        expect(byName.read_status.generation).toBe(generationOf(f.repoA));
        expect(byPath.read_status.generation).toBe(generationOf(f.repoB));
        expect(byName.read_status.generation).not.toBe(byPath.read_status.generation);

        // The content, not just the generation: each graph knows only its own symbol.
        const alpha = await s.call("search", { query: "alphaThing", repo: "repo-a" });
        expect(alpha.status).toBe("ok");
        expect(alpha.payload?.results?.length ?? 0).toBeGreaterThan(0);

        const absent = await s.call("search", { query: "alphaThing", repo: "repo-b" });
        expect(absent.status).toBe("not_found");
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an unregistered name is not_found, and the hint lists every registered repo",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home }, async (s) => {
        const answer = await s.call("status", { repo: "repo-z" });
        expect(answer.status).toBe("error");
        expect(answer.error?.code).toBe("not_found");
        expect(answer.error?.hint).toContain(canonicalRoot(f.repoA));
        expect(answer.error?.hint).toContain(canonicalRoot(f.repoB));
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "two registered repos and a cwd outside both is ambiguous rather than a guess",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home }, async (s) => {
        const answer = await s.call("status", {});
        expect(answer.status).toBe("error");
        expect(answer.error?.code).toBe("ambiguous");
        expect(answer.error?.hint).toContain(canonicalRoot(f.repoA));
        expect(answer.error?.hint).toContain(canonicalRoot(f.repoB));
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a cwd inside a registered repo answers from it, with no repo argument",
    async () => {
      await withSession({ cwd: f.repoA, home: f.home }, async (s) => {
        const answer = await s.call("status", {});
        expect(answer.status).toBe("ok");
        expect(answer.read_status.generation).toBe(generationOf(f.repoA));
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "one registered repo answers with no repo argument, wherever the process starts",
    async () => {
      // Its own home, and A registered by a run that had nothing to publish (exit 2): a tree worth
      // serving is registered by the verb even when the graph is already current.
      const home = await mkdtemp(join(tmpdir(), "sangkar-multi-repo-only-"));
      roots.push(home);
      const again = await runIndex(f.repoA, home);
      expect(again.code).toBe(2);
      expect(registered(home)).toEqual([canonicalRoot(f.repoA)]);

      await withSession({ cwd: f.elsewhere, home }, async (s) => {
        const answer = await s.call("status", {});
        expect(answer.status).toBe("ok");
        expect(answer.read_status.generation).toBe(generationOf(f.repoA));
      });
    },
    RUN_TIMEOUT_MS,
  );
});

describe("a page token belongs to the repository it was minted in", () => {
  test(
    "a cursor minted in A is refused with cursor_mismatch when the resumed call names B",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home }, async (s) => {
        const statusB = await s.call("status", { repo: "repo-b" });
        const generationB = statusB.read_status.generation;
        // The call identity `search` pins: the three arguments that decide the list. The token
        // carries B's generation, so the generation check passes and only the repository stamp can
        // refuse it — that is the boundary this case is about.
        const argsHash = createHash("sha256")
          .update(JSON.stringify(["alphaThing", null, ""]), "utf8")
          .digest("hex");

        const mintedInA = createCursors({ key: CURSOR_KEY, repoId: canonicalRoot(f.repoA) }).sign({
          generation: generationB,
          tool: "search",
          argsHash,
          position: 0,
        });
        const foreign = await s.call("search", {
          query: "alphaThing",
          repo: "repo-b",
          cursor: mintedInA,
        });
        expect(foreign.status).toBe("error");
        expect(foreign.error?.code).toBe("cursor_mismatch");

        // The same page stamped by the repository it is asked of verifies: the stamp, not the shape
        // of the token, is what refused the first call.
        const mintedInB = createCursors({ key: CURSOR_KEY, repoId: canonicalRoot(f.repoB) }).sign({
          generation: generationB,
          tool: "search",
          argsHash,
          position: 0,
        });
        const resumed = await s.call("search", {
          query: "alphaThing",
          repo: "repo-b",
          cursor: mintedInB,
        });
        expect(resumed.error).toBeUndefined();
      });
    },
    RUN_TIMEOUT_MS,
  );
});

describe("--repo-root is a default, and an explicit repo outranks it", () => {
  test(
    "a pinned session answers from the pin, and repo B outranks that pin",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home, pin: f.repoA }, async (s) => {
        const byDefault = await s.call("status", {});
        expect(byDefault.status).toBe("ok");
        expect(byDefault.read_status.generation).toBe(generationOf(f.repoA));

        const named = await s.call("status", { repo: "repo-b" });
        expect(named.status).toBe("ok");
        expect(named.read_status.generation).toBe(generationOf(f.repoB));
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a pinned session naming an unregistered repo is not_found",
    async () => {
      await withSession({ cwd: f.elsewhere, home: f.home, pin: f.repoA }, async (s) => {
        const answer = await s.call("status", { repo: "loose" });
        expect(answer.status).toBe("error");
        expect(answer.error?.code).toBe("not_found");
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an unregistered --repo-root still answers: the pin never needs the registry",
    async () => {
      // No graph here and no registry entry: the selector answers with the pin, and the store says
      // `bootstrapping` — an answer, not a refusal. This is the ad-hoc invocation `sangkar` has
      // always allowed, and the one a client with a `--repo-root` flag must keep getting.
      expect(registered(f.home)).not.toContain(canonicalRoot(f.loose));

      await withSession({ cwd: f.elsewhere, home: f.home, pin: f.loose }, async (s) => {
        const answer = await s.call("status", {});
        expect(answer.status).toBe("bootstrapping");
        expect(answer.error?.code).toBe("bootstrapping");
      });
    },
    RUN_TIMEOUT_MS,
  );
});

describe("the index verb registers what it indexed", () => {
  test(
    "a registered run adds the tree, and --no-register leaves the registry alone",
    async () => {
      // A home of its own: `repo-c` starts unregistered everywhere, and this run is the only thing
      // that puts it in a registry.
      const home = await mkdtemp(join(tmpdir(), "sangkar-multi-repo-verb-"));
      roots.push(home);
      expect(existsSync(join(home, "sangkar.json"))).toBe(false);

      const first = await runIndex(f.repoC, home);
      expect(first.code).toBe(0);
      expect(registered(home)).toEqual([canonicalRoot(f.repoC)]);

      // The same tree from a second fresh home, opted out: the run still succeeds, and no registry
      // file is created at all.
      const optedOut = await mkdtemp(join(tmpdir(), "sangkar-multi-repo-optout-"));
      roots.push(optedOut);
      const skipped = await runIndex(f.repoC, optedOut, ["--no-register"]);
      expect(skipped.code).toBe(2);
      expect(existsSync(join(optedOut, "sangkar.json"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );
});
