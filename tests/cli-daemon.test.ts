import { afterAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { canonicalRoot } from "#shared/registry";

/**
 * `sangkar repo` and `sangkar daemon` end to end: the shipped command is spawned as a child, so
 * what is proven is the path an operator runs — the reserved token in `bin/sangkar.ts`, the lazy
 * dispatch that happens before `../src/index` is loaded, the subcommand grammar, the files the two
 * verbs write and the exit code each answer carries.
 *
 * Four properties shape the file:
 *
 * - **`SANGKAR_HOME` is always this suite's own.** Every spawn drops the inherited `SANGKAR_*` keys
 *   and points the home at a `mkdtemp` directory, so nothing reads or writes the developer's real
 *   `~/.config/sangkar`, and a home read that leaked would land in the fixture.
 * - **No daemon is started.** `start` would spawn a detached child and `run` would bind the report
 *   port, so the cases below exercise the paths that need neither: `status`, the refusals, and a
 *   `run` that is refused before it binds (a bad port, a non-loopback host). The foreground loop
 *   itself is `tests/daemon-main.test.ts`'s subject, through an injected signal.
 * - **stdout is the answer, stderr is the diagnosis.** A refusal writes the usage block and nothing
 *   to stdout, which is how "never reached the tool dispatch" is asserted: a tool call would have
 *   printed an envelope.
 * - **`--help` and a refusal must not drift.** Both blocks live twice — once in `bin/sangkar.ts` for
 *   `--help`, once in the module that prints it on a refusal — so one case compares them byte for
 *   byte.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const CLI_ENTRY = join(APP_ROOT, "bin", "sangkar.ts");

/** A spawn loads two small modules and exits; a wedge should fail, not hang the suite. */
const RUN_TIMEOUT_MS = 60_000;

/** Every temp directory this suite made, removed in one pass. */
const scratch: string[] = [];

async function tempDir(prefix: string): Promise<string> {
  const dir = await mkdtemp(join(tmpdir(), prefix));
  scratch.push(dir);
  return dir;
}

afterAll(async () => {
  for (const dir of scratch) {
    await rm(dir, { recursive: true, force: true });
  }
});

interface Fixture {
  /** The machine-level home the spawn sees as `SANGKAR_HOME`. */
  readonly home: string;
  /** The working directory relative paths are typed against. */
  readonly root: string;
  /** `root/work`, the tree a case registers and configures. */
  readonly repo: string;
}

async function fixture(): Promise<Fixture> {
  const root = await tempDir("sangkar-cli-daemon-");
  const home = join(root, "home");
  const repo = join(root, "work");
  await mkdir(home, { recursive: true });
  await mkdir(repo, { recursive: true });
  return { home, root, repo };
}

interface Run {
  readonly code: number;
  readonly stdout: string;
  readonly stderr: string;
}

/** The repository's own settings are removed: the fixture decides the home, never the machine. */
function cleanEnv(overrides: Readonly<Record<string, string>>): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (key.startsWith("SANGKAR_") || value === undefined) {
      continue;
    }
    env[key] = value;
  }
  return { ...env, ...overrides };
}

function spawnCli(args: readonly string[], cwd: string, home: string): Promise<Run> {
  const proc = Bun.spawn({
    cmd: [process.execPath, "run", CLI_ENTRY, ...args],
    cwd,
    env: cleanEnv({ SANGKAR_HOME: home }),
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

/** The registry as it sits on disk, read without going through the code under test. */
async function registryOnDisk(home: string): Promise<unknown> {
  return JSON.parse(await readFile(join(home, "sangkar.json"), "utf8"));
}

async function configOnDisk(repo: string): Promise<unknown> {
  return JSON.parse(await readFile(join(repo, ".sangkar", "config.json"), "utf8"));
}

/**
 * The usage block a refusal prints, minus its reason line: the first line is the reason, so what is
 * left must be exactly the text `--help` answers with.
 */
function blockAfterReason(stderr: string): string {
  const block = stderr.trimEnd();
  const newline = block.indexOf("\n");
  return newline === -1 ? "" : block.slice(newline + 1);
}

describe("sangkar repo: the registry", () => {
  test(
    "add registers the canonical path and says so once",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);

      // A relative path on purpose: the verb resolves it against the directory it ran in.
      const run = await spawnCli(["repo", "add", "work"], f.root, f.home);

      expect(run.code).toBe(0);
      expect(run.stdout).toBe(`registered ${canonical}\n`);
      expect(await registryOnDisk(f.home)).toEqual({ version: 1, repos: [canonical] });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a second add reports already registered and leaves one entry",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);
      await spawnCli(["repo", "add", "work"], f.root, f.home);

      const run = await spawnCli(["repo", "add", "work"], f.root, f.home);

      expect(run.code).toBe(0);
      expect(run.stdout).toBe(`already registered ${canonical}\n`);
      expect(await registryOnDisk(f.home)).toEqual({ version: 1, repos: [canonical] });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "list prints one path per line, and --json the document",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);
      await spawnCli(["repo", "add", "work"], f.root, f.home);

      const human = await spawnCli(["repo", "list"], f.root, f.home);
      expect(human.code).toBe(0);
      expect(human.stdout).toBe(`${canonical}\n`);

      const machine = await spawnCli(["repo", "list", "--json"], f.root, f.home);
      expect(machine.code).toBe(0);
      expect(JSON.parse(machine.stdout)).toEqual({ version: 1, repos: [canonical] });
      // The document is the whole of stdout: one compact object and one newline, no banner and no
      // "version" spelled any other way.
      expect(machine.stdout).toBe(`${JSON.stringify({ version: 1, repos: [canonical] })}\n`);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a global flag before the verb reaches it too",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);
      await spawnCli(["repo", "add", "work"], f.root, f.home);

      // The position of a global is not the command's business: `--json` before the name and after
      // it must be the same document, or the flag means one thing for a tool and another for a verb.
      const before = await spawnCli(["--json", "repo", "list"], f.root, f.home);
      expect(before.code).toBe(0);
      expect(before.stdout).toBe(`${JSON.stringify({ version: 1, repos: [canonical] })}\n`);

      const dangling = await spawnCli(["repo", "add", "work", "--data-dir"], f.root, f.home);
      expect(dangling.code).toBe(2);
      expect(dangling.stderr).toContain("--data-dir needs a value");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an empty registry lists nothing, and --json an empty list",
    async () => {
      const f = await fixture();

      const human = await spawnCli(["repo", "list"], f.root, f.home);
      expect(human.code).toBe(0);
      expect(human.stdout).toBe("");

      const machine = await spawnCli(["repo", "list", "--json"], f.root, f.home);
      expect(machine.code).toBe(0);
      expect(JSON.parse(machine.stdout)).toEqual({ version: 1, repos: [] });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "remove unregisters once, then reports not registered with exit 1",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);
      await spawnCli(["repo", "add", "work"], f.root, f.home);

      const first = await spawnCli(["repo", "remove", "work"], f.root, f.home);
      expect(first.code).toBe(0);
      expect(first.stdout).toBe(`removed ${canonical}\n`);
      expect(await registryOnDisk(f.home)).toEqual({ version: 1, repos: [] });

      const second = await spawnCli(["repo", "remove", "work"], f.root, f.home);
      expect(second.code).toBe(1);
      expect(second.stdout).toBe(`not registered ${canonical}\n`);
    },
    RUN_TIMEOUT_MS,
  );
});

describe("sangkar repo: the global flags beside a subcommand", () => {
  test(
    "--json and --repo-root are accepted by add and change nothing",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);

      // A global flag next to a subcommand is an acceptance, not a stray one: this verb's answer is
      // one line per result, so `--json` is inert here and only `list` changes with it.
      const withJson = await spawnCli(["repo", "add", "work", "--json"], f.root, f.home);
      expect(withJson.code).toBe(0);
      expect(withJson.stdout).toBe(`registered ${canonical}\n`);
      expect(withJson.stderr).not.toContain("takes no");

      const withRoot = await spawnCli(
        ["repo", "add", "work", "--repo-root", "/tmp"],
        f.root,
        f.home,
      );
      expect(withRoot.code).toBe(0);
      expect(withRoot.stdout).toBe(`already registered ${canonical}\n`);
      expect(withRoot.stderr).not.toContain("takes no");
      expect(await registryOnDisk(f.home)).toEqual({ version: 1, repos: [canonical] });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a value flag with no value is refused with the repo block and registers nothing",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["repo", "add", "work", "--repo-root"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--repo-root needs a value");
      expect(run.stderr).toContain("usage: sangkar repo");
      expect(existsSync(join(f.home, "sangkar.json"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );
});

describe("sangkar repo: the per-repo config", () => {
  test(
    "config writes the snake_case file and prints the effective lines",
    async () => {
      const f = await fixture();

      const run = await spawnCli(
        [
          "repo",
          "config",
          "work",
          "--watch",
          "false",
          "--watch-interval",
          "5000",
          "--telemetry",
          "false",
        ],
        f.root,
        f.home,
      );

      expect(run.code).toBe(0);
      expect(run.stdout).toBe("watch=off\ninterval=5000ms\ntelemetry=off\n");
      expect(await configOnDisk(f.repo)).toEqual({
        watch: false,
        watch_interval: 5000,
        telemetry: false,
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "config with no flags writes the defaults it prints",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["repo", "config", "work"], f.root, f.home);

      expect(run.code).toBe(0);
      expect(run.stdout).toBe("watch=on\ninterval=2000ms\ntelemetry=on\n");
      expect(await configOnDisk(f.repo)).toEqual({
        watch: true,
        watch_interval: 2000,
        telemetry: true,
      });
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a value that is neither true nor false is a usage refusal, and nothing is written",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["repo", "config", "work", "--watch", "yes"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--watch must be true or false");
      expect(run.stderr).toContain("usage: sangkar repo");
      expect(existsSync(join(f.repo, ".sangkar", "config.json"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an interval outside the supported band is refused rather than written and clamped",
    async () => {
      const f = await fixture();

      const run = await spawnCli(
        ["repo", "config", "work", "--watch-interval", "10"],
        f.root,
        f.home,
      );

      expect(run.code).toBe(2);
      expect(run.stderr).toContain("--watch-interval must be 1000..300000 ms");
      expect(existsSync(join(f.repo, ".sangkar", "config.json"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );
});

describe("sangkar repo: the verb itself", () => {
  test(
    "a bare repo is a usage refusal that never reaches the tool dispatch",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["repo"], f.root, f.home);

      expect(run.code).toBe(2);
      // A tool call would have written an envelope, or `sangkar: unknown tool` on stderr.
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("repo needs a subcommand");
      expect(run.stderr).toContain("usage: sangkar repo");
      expect(run.stderr).not.toContain("unknown tool");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "an unknown subcommand is refused, and the block is the one --help prints",
    async () => {
      const f = await fixture();

      const bad = await spawnCli(["repo", "bogus"], f.root, f.home);
      expect(bad.code).toBe(2);
      expect(bad.stdout).toBe("");
      expect(bad.stderr).not.toContain("unknown tool");

      const help = await spawnCli(["repo", "--help"], f.root, f.home);
      expect(help.code).toBe(0);
      expect(blockAfterReason(bad.stderr)).toBe(help.stdout.trimEnd());
    },
    RUN_TIMEOUT_MS,
  );
});

describe("sangkar daemon", () => {
  test(
    "status on an untouched home says not running and exits 1",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["daemon", "status"], f.root, f.home);

      expect(run.code).toBe(1);
      expect(run.stdout).toBe("not running\n");
      expect(run.stderr).not.toContain("unknown tool");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status names each registered repo's settings on its own line",
    async () => {
      const f = await fixture();
      const canonical = canonicalRoot(f.repo);
      await spawnCli(["repo", "add", "work"], f.root, f.home);
      await spawnCli(
        [
          "repo",
          "config",
          "work",
          "--watch",
          "false",
          "--watch-interval",
          "5000",
          "--telemetry",
          "false",
        ],
        f.root,
        f.home,
      );

      const run = await spawnCli(["daemon", "status"], f.root, f.home);

      expect(run.code).toBe(1);
      expect(run.stdout).toBe(
        `not running\n  ${canonical}  watch=off interval=5000ms telemetry=off\n`,
      );
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "status accepts --json, and a live pid file is what makes it exit 0",
    async () => {
      const f = await fixture();
      // No daemon is started: the pid file *is* the state `status` reads, and a fresh one naming a
      // live process is exactly the record a running daemon leaves behind (`pidlock.ts`).
      await writeFile(
        join(f.home, "daemon.pid"),
        JSON.stringify({ pid: process.pid, startedAt: new Date().toISOString() }),
      );

      const run = await spawnCli(["daemon", "status", "--json"], f.root, f.home);

      expect(run.code).toBe(0);
      expect(run.stdout).toBe(`running, pid ${process.pid}\n`);
      expect(run.stderr).not.toContain("takes no");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a value flag with no value is refused with the daemon block and takes no lock",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["daemon", "status", "--data-dir"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--data-dir needs a value");
      expect(run.stderr).toContain("usage: sangkar daemon");
      expect(existsSync(join(f.home, "daemon.pid"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "stop on a home with no daemon says not running and exits 1",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["daemon", "stop"], f.root, f.home);

      expect(run.code).toBe(1);
      expect(run.stdout).toBe("not running\n");
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "a bare daemon and an unknown subcommand are usage refusals",
    async () => {
      const f = await fixture();

      const bare = await spawnCli(["daemon"], f.root, f.home);
      expect(bare.code).toBe(2);
      expect(bare.stdout).toBe("");
      expect(bare.stderr).toContain("daemon needs a subcommand");
      expect(bare.stderr).toContain("usage: sangkar daemon");

      const bad = await spawnCli(["daemon", "bogus"], f.root, f.home);
      expect(bad.code).toBe(2);
      expect(bad.stdout).toBe("");
      expect(bad.stderr).toContain('unknown subcommand "bogus"');
      expect(bad.stderr).toContain("usage: sangkar daemon");
      expect(bad.stderr).not.toContain("unknown tool");

      const help = await spawnCli(["daemon", "--help"], f.root, f.home);
      expect(help.code).toBe(0);
      expect(blockAfterReason(bad.stderr)).toBe(help.stdout.trimEnd());
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "start does not silently accept run's flags, and holds no lock when it refuses",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["daemon", "start", "--port", "8080"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--port");
      expect(existsSync(join(f.home, "daemon.pid"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "run refuses a port outside the range before it takes the lock",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["daemon", "run", "--port", "99999"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("--port must be an integer in 0..65535");
      expect(existsSync(join(f.home, "daemon.pid"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );

  test(
    "run refuses a non-loopback host and leaves no pid file behind",
    async () => {
      const f = await fixture();
      // The host is validated by the report server before it binds, so this exercises `run`'s
      // refusal path without a listener and without a watcher: the home stays as it was.
      const run = await spawnCli(["daemon", "run", "--host", "0.0.0.0"], f.root, f.home);

      expect(run.code).toBe(2);
      expect(run.stdout).toBe("");
      expect(run.stderr).toContain("loopback only");
      expect(run.stderr).toContain("usage: sangkar daemon");
      expect(existsSync(join(f.home, "daemon.pid"))).toBe(false);
    },
    RUN_TIMEOUT_MS,
  );
});

describe("sangkar telemetry --help", () => {
  test(
    "lists the three flags --serve added",
    async () => {
      const f = await fixture();

      const run = await spawnCli(["telemetry", "--help"], f.root, f.home);

      expect(run.code).toBe(0);
      expect(run.stdout).toContain("usage: sangkar telemetry");
      expect(run.stdout).toContain("--serve");
      expect(run.stdout).toContain("--port <n>");
      expect(run.stdout).toContain("--host <addr>");
    },
    RUN_TIMEOUT_MS,
  );
});
