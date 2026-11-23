import { afterAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { PassThrough } from "node:stream";
import { askContinue, CONTINUE_QUESTION, indexGate, refusalMessage } from "#shared/cli/index-gate";

/**
 * The `index` verb asks before it guesses. This file proves the three layers in the order they are
 * cheap to prove: the decision is a pure function of five facts (the whole matrix, no filesystem),
 * the question reads exactly one line and treats anything but `y` as no, and the shipped command
 * refuses a guessed tree without writing a byte into it.
 *
 * The child is spawned with `stdin: "ignore"`, so every end-to-end case below is the non-interactive
 * one — which is the case that must never block, and the case a script hits.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");
const RUN_TIMEOUT_MS = 120_000;

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

const dirs: string[] = [];

async function tempDir(prefix: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

afterAll(async () => {
  for (const dir of dirs.splice(0)) {
    await rm(dir, { recursive: true, force: true });
  }
});

describe("indexGate", () => {
  // Every row of the matrix: name, the input, and the action it must produce.
  const base = { explicit: false, yes: false, levels: 2, tty: false, registered: false };

  test("a named tree is never questioned and never refused", () => {
    expect(indexGate({ ...base, explicit: true, levels: 9 })).toEqual({ action: "run" });
  });

  test("--yes is the operator's answer, so it wins over every refusal", () => {
    expect(indexGate({ ...base, yes: true, levels: 9 })).toEqual({ action: "run" });
    expect(indexGate({ ...base, yes: true, registered: true, levels: 9 })).toEqual({
      action: "run",
    });
  });

  test("levels 0 is the working directory itself: a bootstrap, not a guess", () => {
    expect(indexGate({ ...base, levels: 0 })).toEqual({ action: "run" });
    expect(indexGate({ ...base, levels: 0, tty: true })).toEqual({ action: "run" });
  });

  test("a TTY is asked, registered or not, at any depth above the working directory", () => {
    expect(indexGate({ ...base, tty: true })).toEqual({ action: "ask" });
    expect(indexGate({ ...base, tty: true, registered: true, levels: 4 })).toEqual({
      action: "ask",
    });
  });

  test("no TTY: a registered tree within the walk budget proceeds", () => {
    expect(indexGate({ ...base, registered: true, levels: 1 })).toEqual({ action: "run" });
    // The budget is inclusive: 3 levels is inside it, 4 is not.
    expect(indexGate({ ...base, registered: true, levels: 3 })).toEqual({ action: "run" });
  });

  test("no TTY: a registered tree beyond the budget is refused as too deep", () => {
    expect(indexGate({ ...base, registered: true, levels: 4 })).toEqual({
      action: "refuse",
      reason: "too_deep",
    });
  });

  test("no TTY: an unregistered tree is refused whatever its depth", () => {
    expect(indexGate({ ...base, levels: 1 })).toEqual({
      action: "refuse",
      reason: "unregistered",
    });
  });
});

describe("refusalMessage", () => {
  test("names what was refused, the working directory, and the three ways forward", () => {
    const message = refusalMessage({
      root: "/home/someone",
      cwd: "/home/someone/work/app",
      levels: 4,
      reason: "too_deep",
    });
    expect(message).toContain("/home/someone is 4 levels above /home/someone/work/app (max 3)");
    expect(message).toContain("Nothing was indexed.");
    expect(message).toContain("--repo-root .");
    expect(message).toContain("sangkar repo add <path>");
    expect(message).toContain("--yes");
  });

  test("an unregistered tree says so, and a declined one does not argue", () => {
    expect(
      refusalMessage({ root: "/r", cwd: "/r/x", levels: 1, reason: "unregistered" }),
    ).toContain("/r is not registered as a repository");
    expect(refusalMessage({ root: "/r", cwd: "/r/x", levels: 1, reason: "declined" })).toContain(
      "/r was not confirmed",
    );
  });
});

/** One line on an injected stream, with the question captured on the other side. */
async function answer(line: string | null): Promise<{ taken: boolean; written: string }> {
  const input = new PassThrough();
  const output = new PassThrough();
  let written = "";
  output.on("data", (chunk: Buffer) => {
    written += chunk.toString();
  });

  const pending = askContinue({ input, output });
  if (line === null) {
    input.end();
  } else {
    input.write(line);
  }
  return { taken: await pending, written };
}

describe("askContinue", () => {
  test("the question goes to the injected stream, never to stdout", async () => {
    const { written } = await answer("\n");
    expect(written).toBe(CONTINUE_QUESTION);
    expect(CONTINUE_QUESTION.endsWith("(y/N) ")).toBe(true);
  });

  test("only y and yes say yes", async () => {
    expect((await answer("y\n")).taken).toBe(true);
    expect((await answer("yes\n")).taken).toBe(true);
    expect((await answer("Y\n")).taken).toBe(true);
  });

  test("Enter, anything else, and a stdin that closes without an answer are all no", async () => {
    expect((await answer("\n")).taken).toBe(false);
    expect((await answer("n\n")).taken).toBe(false);
    expect((await answer("yep\n")).taken).toBe(false);
    expect((await answer(null)).taken).toBe(false);
  });
});

/**
 * The repository's own settings, removed: nothing here may inherit a `SANGKAR_*` this machine
 * happens to export, and the machine-level home is this suite's own temp directory.
 */
function cleanEnv(home: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  return { ...env, SANGKAR_HOME: home };
}

function spawnCli(args: readonly string[], cwd: string, home: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: cleanEnv(home),
    // No TTY and no pipe: the non-interactive case, which must answer itself instead of waiting.
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  return (async () => {
    const [stdout, stderr, code] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    return { code, stdout, stderr };
  })();
}

/**
 * A tree whose root is marked with `.git` — so autodetect has something to find — plus the nested
 * directories a run can start from at each depth the gate cares about.
 */
async function tree(name: string): Promise<{ root: string; at: (depth: number) => string }> {
  const root = await tempDir(`sangkar-gate-${name}-`);
  await mkdir(join(root, ".git"), { recursive: true });
  await writeFile(join(root, "app.ts"), "export function app(): number {\n  return 1;\n}\n");
  const names = ["a", "b", "c", "d", "e"];
  for (let depth = 1; depth <= names.length; depth += 1) {
    await mkdir(join(root, ...names.slice(0, depth)), { recursive: true });
  }
  return { root, at: (depth) => join(root, ...names.slice(0, depth)) };
}

describe("sangkar index, end to end", () => {
  test(
    "an unregistered tree above the working directory is refused without writing anything",
    async () => {
      const home = await tempDir("sangkar-gate-home-");
      const { root, at } = await tree("refuse");
      const run = await spawnCli(["index"], at(2), home);

      expect(run.code).toBe(2);
      // A refusal is a diagnostic, not a payload: nothing may reach stdout, ever.
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain(`${at(2)} is not registered as repo root`);
      expect(run.stderr).toContain("index.walk");
      expect(run.stderr).toContain("index.resolution");
      expect(run.stderr).toContain("refused:");
      expect(run.stderr).toContain("Nothing was indexed.");

      // The whole point: the refused tree holds no data dir, so no database was created either.
      expect(existsSync(join(root, ".sangkar"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "--yes indexes the same tree, and the run registers it",
    async () => {
      const home = await tempDir("sangkar-gate-home-");
      const { root, at } = await tree("yes");
      const run = await spawnCli(["index", "--yes"], at(2), home);

      // `--yes` answers the question, it does not hide the guess: the resolution is still reported.
      expect(run.stderr).toContain("index.resolution");
      expect(run.stderr).not.toContain("refused:");
      expect(run.code).toBe(0);
      expect(existsSync(join(root, ".sangkar", "graph.db"))).toBe(true);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a registered tree within the budget proceeds without a question",
    async () => {
      const home = await tempDir("sangkar-gate-home-");
      const { root, at } = await tree("registered");
      const added = await spawnCli(["repo", "add", root], root, home);
      expect(added.code).toBe(0);

      const run = await spawnCli(["index"], at(3), home);
      expect(run.code).toBe(0);
      expect(existsSync(join(root, ".sangkar", "graph.db"))).toBe(true);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a registered tree beyond the budget is refused as too deep",
    async () => {
      const home = await tempDir("sangkar-gate-home-");
      const { root, at } = await tree("deep");
      const added = await spawnCli(["repo", "add", root], root, home);
      expect(added.code).toBe(0);

      const run = await spawnCli(["index"], at(4), home);
      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("is 4 levels above");
      expect(run.stderr).toContain("(max 3)");
      expect(existsSync(join(root, ".sangkar"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "the working directory itself is a bootstrap: no question, no refusal",
    async () => {
      const home = await tempDir("sangkar-gate-home-");
      const { root } = await tree("bootstrap");
      const run = await spawnCli(["index"], root, home);

      expect(run.code).toBe(0);
      expect(run.stderr).not.toContain("refused:");
      expect(run.stderr).not.toContain("index.resolution");
      expect(existsSync(join(root, ".sangkar", "graph.db"))).toBe(true);
    },
    RUN_TIMEOUT_MS,
  );
});
