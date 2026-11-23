import { afterAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { loadConfig, withRepo } from "#shared/config";
import {
  DEFAULT_GLOBAL_SETTINGS,
  DEFAULT_REPORT_HOST,
  DEFAULT_REPORT_PORT,
  ensureGlobalConfig,
  globalConfigPath,
  readGlobalConfig,
  repoDefaultsOf,
} from "#shared/global-config";
import type { LogFields, Logger } from "#shared/log";
import { readRepoConfig } from "#shared/repo-config";
import { LOOPBACK_HOSTS, DEFAULT_REPORT_PORT as SERVE_DEFAULT_PORT } from "#shared/telemetry/serve";

/**
 * The machine-level file is created on a first run and hand-edited afterwards, so every case here
 * is a real file in a `mkdtemp` home and the assertion is what a process would conclude from it:
 * the resolved settings, plus the warnings that make a broken file diagnosable. Nothing reads the
 * developer's own home.
 */

const dirs: string[] = [];

function tempHome(prefix = "sangkar-global-config-"): string {
  const home = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(home);
  return home;
}

/** A home holding a config.json with exactly `body`. */
function homeWithConfig(body: string, prefix = "sangkar-global-config-file-"): string {
  const home = tempHome(prefix);
  writeFileSync(globalConfigPath(home), body);
  return home;
}

interface Recorded {
  msg: string;
  fields: LogFields;
}

/** A logger that records instead of writing: the warning is part of the contract. */
function recordingLogger(): { logger: Logger; lines: Recorded[] } {
  const lines: Recorded[] = [];
  const record =
    (level: string) =>
    (msg: string, fields: LogFields = {}) => {
      lines.push({ msg: `${level}:${msg}`, fields });
    };
  return {
    logger: {
      debug: record("debug"),
      info: record("info"),
      warn: record("warn"),
      error: record("error"),
    },
    lines,
  };
}

function warningsOf(home: string): {
  config: ReturnType<typeof readGlobalConfig>;
  warnings: Recorded[];
} {
  const { logger, lines } = recordingLogger();
  const config = readGlobalConfig(home, { log: logger });
  return { config, warnings: lines.filter((line) => line.msg.startsWith("warn:")) };
}

afterAll(() => {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("defaults and path", () => {
  test("the built-in defaults are the documented values", () => {
    expect(DEFAULT_GLOBAL_SETTINGS).toEqual({
      watch: true,
      watchIntervalMs: 2_000,
      telemetry: true,
      daemon: { host: "127.0.0.1", port: 7_317 },
      budgets: { defaultTokens: 1_200, maxTokens: 32_000 },
    });
  });

  test("the daemon defaults cannot drift from the report server's", () => {
    expect(DEFAULT_REPORT_PORT).toBe(7_317);
    expect(DEFAULT_GLOBAL_SETTINGS.daemon.port).toBe(SERVE_DEFAULT_PORT);
    expect(LOOPBACK_HOSTS).toContain(DEFAULT_REPORT_HOST);
    expect(DEFAULT_GLOBAL_SETTINGS.daemon.host).toBe(DEFAULT_REPORT_HOST);
  });

  test("the file is config.json directly in the machine home, beside sangkar.json", () => {
    expect(globalConfigPath("/machine/home")).toBe(join("/machine/home", "config.json"));
  });

  test("repoDefaultsOf takes exactly the three keys a repository inherits", () => {
    expect(repoDefaultsOf(DEFAULT_GLOBAL_SETTINGS)).toEqual({
      watch: true,
      watchIntervalMs: 2_000,
      telemetry: true,
    });
  });
});

describe("ensureGlobalConfig", () => {
  test("writes the defaults, newline-terminated, in the spelling it reads", () => {
    const home = tempHome("sangkar-global-create-");
    expect(ensureGlobalConfig(home)).toBe(true);
    const text = readFileSync(globalConfigPath(home), "utf8");
    expect(text.endsWith("\n")).toBe(true);
    // Snake case on disk: camelCase keys here would be a file the reader ignores.
    expect(JSON.parse(text)).toEqual({
      watch: true,
      watch_interval: 2_000,
      telemetry: true,
      daemon: { host: "127.0.0.1", port: 7_317 },
      budgets: { default_tokens: 1_200, max_tokens: 32_000 },
    });
    // What was written is what a read answers, so the file cannot describe itself differently.
    expect(readGlobalConfig(home)).toEqual(DEFAULT_GLOBAL_SETTINGS);

    // A hand edit of one written key is honoured, which is the same claim from the other side.
    writeFileSync(globalConfigPath(home), text.replace("2000", "5000"));
    expect(readGlobalConfig(home).watchIntervalMs).toBe(5_000);
  });

  test("leaves a hand-edited file alone", () => {
    const home = homeWithConfig('{"telemetry": false}\n', "sangkar-global-keep-");
    expect(ensureGlobalConfig(home)).toBe(false);
    expect(readFileSync(globalConfigPath(home), "utf8")).toBe('{"telemetry": false}\n');
  });

  test("an unwritable home is one warning and no throw", () => {
    const parent = tempHome("sangkar-global-unwritable-");
    // A regular file where the home directory should be: mkdir fails with ENOTDIR, deterministically.
    const home = join(parent, "not-a-directory");
    writeFileSync(home, "");
    const { logger, lines } = recordingLogger();
    expect(ensureGlobalConfig(home, { log: logger })).toBe(false);
    expect(lines).toHaveLength(1);
    expect(lines[0]?.msg).toContain("global config not created");
    expect(lines[0]?.fields.path).toBe(join(home, "config.json"));
  });
});

describe("readGlobalConfig with an unusable file", () => {
  test("no file means the defaults, silently: a fresh home is not a fault", () => {
    const home = tempHome("sangkar-global-missing-");
    const { config, warnings } = warningsOf(home);
    expect(config).toEqual(DEFAULT_GLOBAL_SETTINGS);
    expect(warnings).toHaveLength(0);
  });

  test("unparseable JSON means the defaults and one warning naming the path", () => {
    const home = homeWithConfig('{"watch": tru');
    const { config, warnings } = warningsOf(home);
    expect(config).toEqual(DEFAULT_GLOBAL_SETTINGS);
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.fields.path).toBe(join(home, "config.json"));
    expect(String(warnings[0]?.fields.reason)).toContain("invalid JSON");
  });

  test("a JSON value that is not an object means the defaults and one warning", () => {
    for (const body of ["[]", "null", '"config"', "42"]) {
      const home = homeWithConfig(body, "sangkar-global-nonobject-");
      const { config, warnings } = warningsOf(home);
      expect(config).toEqual(DEFAULT_GLOBAL_SETTINGS);
      expect(warnings).toHaveLength(1);
      expect(String(warnings[0]?.fields.reason)).toContain("not a JSON object");
    }
  });
});

describe("readGlobalConfig field by field", () => {
  test("a complete file is honoured as written", () => {
    const home = homeWithConfig(
      JSON.stringify({
        watch: false,
        watch_interval: 30_000,
        telemetry: false,
        daemon: { host: "127.0.0.1", port: 0 },
        budgets: { default_tokens: 900, max_tokens: 4_000 },
      }),
      "sangkar-global-full-",
    );
    const { config, warnings } = warningsOf(home);
    expect(config).toEqual({
      watch: false,
      watchIntervalMs: 30_000,
      telemetry: false,
      daemon: { host: "127.0.0.1", port: 0 },
      budgets: { defaultTokens: 900, maxTokens: 4_000 },
    });
    expect(warnings).toHaveLength(0);
  });

  test("keys the operator left out are silent defaults", () => {
    const home = homeWithConfig('{"watch": false}\n', "sangkar-global-partial-");
    const { config, warnings } = warningsOf(home);
    expect(config).toEqual({ ...DEFAULT_GLOBAL_SETTINGS, watch: false });
    expect(warnings).toHaveLength(0);
  });

  test("unknown keys are ignored without a warning", () => {
    const home = homeWithConfig('{"colour": "blue", "watch": false}\n', "sangkar-global-unknown-");
    const { config, warnings } = warningsOf(home);
    expect(config.watch).toBe(false);
    expect(warnings).toHaveLength(0);
  });

  test("a switch that is not a JSON boolean takes the default, with a warning", () => {
    const home = homeWithConfig('{"watch": "yes", "telemetry": 1}\n', "sangkar-global-switches-");
    const { config, warnings } = warningsOf(home);
    expect(config.watch).toBe(true);
    expect(config.telemetry).toBe(true);
    expect(warnings.map((warning) => warning.fields.field)).toEqual(["watch", "telemetry"]);
    expect(warnings.every((warning) => warning.fields.path === join(home, "config.json"))).toBe(
      true,
    );
  });

  test("the interval is clamped into the supported band", () => {
    const low = homeWithConfig('{"watch_interval": 500}\n', "sangkar-global-low-");
    expect(warningsOf(low).config.watchIntervalMs).toBe(1_000);
    const high = homeWithConfig('{"watch_interval": 900000}\n', "sangkar-global-high-");
    expect(warningsOf(high).config.watchIntervalMs).toBe(300_000);
    // A wrong type is not a slow poll but nonsense, and takes the default silently.
    const nonsense = homeWithConfig('{"watch_interval": "fast"}\n', "sangkar-global-nonsense-");
    const read = warningsOf(nonsense);
    expect(read.config.watchIntervalMs).toBe(2_000);
    expect(read.warnings).toHaveLength(0);
  });

  test("daemon and budgets must be JSON objects; a wrong shape takes the whole subtree's default", () => {
    const home = homeWithConfig(
      '{"daemon": "localhost:9000", "budgets": 5}\n',
      "sangkar-global-shapes-",
    );
    const { config, warnings } = warningsOf(home);
    expect(config.daemon).toEqual(DEFAULT_GLOBAL_SETTINGS.daemon);
    expect(config.budgets).toEqual(DEFAULT_GLOBAL_SETTINGS.budgets);
    expect(warnings.map((warning) => warning.fields.field)).toEqual(["daemon", "budgets"]);
  });

  test("a port is an integer in 0..65535, and 0 is a real answer", () => {
    const bad = homeWithConfig('{"daemon": {"port": 70000}}\n', "sangkar-global-port-bad-");
    const read = warningsOf(bad);
    expect(read.config.daemon.port).toBe(7_317);
    expect(read.warnings[0]?.fields.field).toBe("daemon.port");

    const fraction = homeWithConfig('{"daemon": {"port": 80.5}}\n', "sangkar-global-port-float-");
    expect(warningsOf(fraction).config.daemon.port).toBe(7_317);
  });

  test("the host is any non-empty string; the loopback policy belongs to the server", () => {
    const empty = homeWithConfig('{"daemon": {"host": "  "}}\n', "sangkar-global-host-empty-");
    const read = warningsOf(empty);
    expect(read.config.daemon.host).toBe("127.0.0.1");
    expect(read.warnings[0]?.fields.field).toBe("daemon.host");

    const typed = homeWithConfig('{"daemon": {"host": 0}}\n', "sangkar-global-host-type-");
    expect(warningsOf(typed).config.daemon.host).toBe("127.0.0.1");

    const localhost = homeWithConfig(
      '{"daemon": {"host": "localhost"}}\n',
      "sangkar-global-host-ok-",
    );
    expect(warningsOf(localhost).config.daemon.host).toBe("localhost");
  });

  test("budgets are positive integers, and the fallback is clamped to the ceiling", () => {
    const bad = homeWithConfig(
      '{"budgets": {"default_tokens": 0, "max_tokens": "lots"}}\n',
      "sangkar-global-budgets-bad-",
    );
    const read = warningsOf(bad);
    expect(read.config.budgets).toEqual({ defaultTokens: 1_200, maxTokens: 32_000 });
    expect(read.warnings.map((warning) => warning.fields.field)).toEqual([
      "budgets.max_tokens",
      "budgets.default_tokens",
    ]);

    const above = homeWithConfig(
      '{"budgets": {"default_tokens": 9000, "max_tokens": 5000}}\n',
      "sangkar-global-budgets-clamp-",
    );
    const clamped = warningsOf(above);
    expect(clamped.config.budgets).toEqual({ defaultTokens: 5_000, maxTokens: 5_000 });
    expect(clamped.warnings).toHaveLength(0);

    // The hard ceiling in `budget.ts` cannot be raised from the file; the file only lowers it.
    const raised = homeWithConfig(
      '{"budgets": {"max_tokens": 100000}}\n',
      "sangkar-global-budgets-hard-",
    );
    expect(warningsOf(raised).config.budgets.maxTokens).toBe(32_000);
  });
});

describe("loadConfig reads the machine file", () => {
  function homeWith(body: string): string {
    const home = tempHome("sangkar-global-load-");
    writeFileSync(globalConfigPath(home), body);
    return home;
  }

  test("budgets, daemon address and repository defaults come from the file", () => {
    const home = homeWith(
      JSON.stringify({
        watch: false,
        watch_interval: 5_000,
        telemetry: false,
        daemon: { host: "localhost", port: 9_000 },
        budgets: { default_tokens: 800, max_tokens: 8_000 },
      }),
    );
    const config = loadConfig({ argv: [], env: { SANGKAR_HOME: home }, cwd: tmpdir() });
    expect(config.home).toBe(home);
    expect(config.configPath).toBe(join(home, "config.json"));
    expect(config.budgets).toEqual({ defaultTokens: 800, maxTokens: 8_000 });
    expect(config.daemon.port).toBe(9_000);
    expect(config.daemon.host).toBe("localhost");
    expect(config.repoDefaults).toEqual({
      watch: false,
      watchIntervalMs: 5_000,
      telemetry: false,
    });
    expect(config.telemetry.enabled).toBe(false);
  });

  test("SANGKAR_TELEMETRY outranks the file in both directions, and empty still means enabled", () => {
    const off = homeWith('{"telemetry": true}\n');
    expect(
      loadConfig({ argv: [], env: { SANGKAR_HOME: off, SANGKAR_TELEMETRY: "off" }, cwd: tmpdir() })
        .telemetry.enabled,
    ).toBe(false);

    const on = homeWith('{"telemetry": false}\n');
    expect(
      loadConfig({ argv: [], env: { SANGKAR_HOME: on, SANGKAR_TELEMETRY: "1" }, cwd: tmpdir() })
        .telemetry.enabled,
    ).toBe(true);
    // The documented rule for the variable is unchanged: an empty value is an opt-in.
    expect(
      loadConfig({ argv: [], env: { SANGKAR_HOME: on, SANGKAR_TELEMETRY: "" }, cwd: tmpdir() })
        .telemetry.enabled,
    ).toBe(true);
  });

  test("a missing file leaves every built-in default in place", () => {
    const home = tempHome("sangkar-global-load-missing-");
    const config = loadConfig({ argv: [], env: { SANGKAR_HOME: home }, cwd: tmpdir() });
    expect(config.budgets).toEqual(DEFAULT_GLOBAL_SETTINGS.budgets);
    expect(config.daemon).toMatchObject({
      host: DEFAULT_GLOBAL_SETTINGS.daemon.host,
      port: DEFAULT_GLOBAL_SETTINGS.daemon.port,
    });
    expect(config.repoDefaults).toEqual(repoDefaultsOf(DEFAULT_GLOBAL_SETTINGS));
  });

  test("a broken file is one warning and the defaults, never a throw", () => {
    const home = homeWith("{");
    const { logger, lines } = recordingLogger();
    const config = loadConfig({
      argv: [],
      env: { SANGKAR_HOME: home },
      cwd: tmpdir(),
      log: logger,
    });
    expect(config.budgets).toEqual(DEFAULT_GLOBAL_SETTINGS.budgets);
    expect(lines.filter((line) => line.msg.startsWith("warn:"))).toHaveLength(1);
  });

  test("withRepo rebinds the tree and carries the machine settings unchanged", () => {
    const home = homeWith('{"budgets": {"default_tokens": 700}}\n');
    const base = loadConfig({ argv: [], env: { SANGKAR_HOME: home }, cwd: tmpdir() });
    const other = tempHome("sangkar-global-load-other-");
    const rebound = withRepo(base, other);
    expect(rebound.dataDir).toBe(join(other, ".sangkar"));
    expect(rebound.budgets).toEqual(base.budgets);
    expect(rebound.repoDefaults).toEqual(base.repoDefaults);
    expect(rebound.configPath).toBe(base.configPath);
    expect(rebound.daemon).toEqual(base.daemon);
  });
});

describe("a repository narrows the machine defaults", () => {
  function repoWith(body: string | null): string {
    const root = tempHome("sangkar-global-repo-");
    mkdirSync(join(root, ".sangkar"), { recursive: true });
    if (body !== null) {
      writeFileSync(join(root, ".sangkar", "config.json"), body);
    }
    return join(root, ".sangkar");
  }

  const machine = {
    watch: false,
    watchIntervalMs: 30_000,
    telemetry: false,
  };

  test("no repo file inherits the machine values, including the interval", () => {
    const dataDir = repoWith(null);
    const config = readRepoConfig(join(dataDir, ".."), { dataDir, defaults: machine });
    expect(config.watch).toBe(false);
    expect(config.watchIntervalMs).toBe(30_000);
    expect(config.telemetry).toBe(false);
  });

  test("a repo file overrides one switch and inherits the other two", () => {
    const dataDir = repoWith('{"watch": true}\n');
    const config = readRepoConfig(join(dataDir, ".."), { dataDir, defaults: machine });
    expect(config.watch).toBe(true);
    expect(config.watchIntervalMs).toBe(30_000);
    expect(config.telemetry).toBe(false);
  });

  test("a malformed interval takes the machine interval, not the built-in period", () => {
    const dataDir = repoWith('{"watch_interval": "slow"}\n');
    const config = readRepoConfig(join(dataDir, ".."), { dataDir, defaults: machine });
    expect(config.watchIntervalMs).toBe(30_000);
  });

  test("without a machine default the built-in one still applies", () => {
    const dataDir = repoWith(null);
    const config = readRepoConfig(join(dataDir, ".."), { dataDir });
    expect(config.watchIntervalMs).toBe(2_000);
  });
});
