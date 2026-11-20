import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { LogFields, Logger } from "#shared/log";
import {
  DEFAULT_REPO_SETTINGS,
  type RepoConfig,
  readRepoConfig,
  recordingEnabled,
  repoConfigPath,
  WATCH_INTERVAL_DEFAULT_MS,
  WATCH_INTERVAL_MAX_MS,
  WATCH_INTERVAL_MIN_MS,
  writeRepoConfig,
} from "#shared/repo-config";

/**
 * The settings file is hand-editable and shared, so every case here is a real file in a
 * `mkdtemp` tree and the assertion is what a process would conclude from it: the resolved config,
 * plus the one warning that makes a broken file diagnosable. Nothing reads this repository's own
 * `.sangkar`.
 */

const dirs: string[] = [];

function tempRepo(prefix = "sangkar-repo-config-"): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

/** A repo with a data dir and a config.json holding exactly `body`. */
function repoWithConfig(body: string, prefix = "sangkar-repo-config-file-"): string {
  const repoRoot = tempRepo(prefix);
  const dataDir = join(repoRoot, ".sangkar");
  mkdirSync(dataDir, { recursive: true });
  writeFileSync(join(dataDir, "config.json"), body);
  return repoRoot;
}

interface RecordedWarning {
  msg: string;
  fields: LogFields;
}

/** A logger that records instead of writing: the warning is part of the contract. */
function recordingLogger(): { logger: Logger; warnings: RecordedWarning[] } {
  const warnings: RecordedWarning[] = [];
  const record =
    (level: string) =>
    (msg: string, fields: LogFields = {}) => {
      if (level === "warn") {
        warnings.push({ msg, fields });
      }
    };
  return {
    logger: {
      debug: record("debug"),
      info: record("info"),
      warn: record("warn"),
      error: record("error"),
    },
    warnings,
  };
}

function readWithWarnings(
  repoRoot: string,
  dataDir?: string,
): {
  config: RepoConfig;
  warnings: RecordedWarning[];
} {
  const { logger, warnings } = recordingLogger();
  const config =
    dataDir === undefined
      ? readRepoConfig(repoRoot, { log: logger })
      : readRepoConfig(repoRoot, { dataDir, log: logger });
  return { config, warnings };
}

afterEach(() => {
  for (const dir of dirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("defaults and path", () => {
  test("the exported defaults are the values a missing file yields", () => {
    expect(DEFAULT_REPO_SETTINGS).toEqual({
      watch: true,
      watchIntervalMs: 2_000,
      telemetry: true,
    });
    expect(WATCH_INTERVAL_DEFAULT_MS).toBe(2_000);
    expect(WATCH_INTERVAL_MIN_MS).toBe(1_000);
    expect(WATCH_INTERVAL_MAX_MS).toBe(300_000);
  });

  test("repoConfigPath is config.json inside the data dir", () => {
    expect(repoConfigPath("/tmp/repo/.sangkar")).toBe(join("/tmp/repo/.sangkar", "config.json"));
  });
});

describe("readRepoConfig with an unusable file", () => {
  test("no file yields the defaults and exactly one warning naming path and reason", () => {
    const repoRoot = tempRepo();
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config).toEqual({
      repoRoot,
      dataDir: join(repoRoot, ".sangkar"),
      watch: true,
      watchIntervalMs: 2_000,
      telemetry: true,
    });
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.fields.path).toBe(join(repoRoot, ".sangkar", "config.json"));
    expect(String(warnings[0]?.fields.reason)).toContain("ENOENT");
  });

  test("unparseable JSON yields the defaults and one warning, without throwing", () => {
    const repoRoot = repoWithConfig('{"watch": tru');
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config.watch).toBe(true);
    expect(config.watchIntervalMs).toBe(WATCH_INTERVAL_DEFAULT_MS);
    expect(config.telemetry).toBe(true);
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.fields.path).toBe(join(repoRoot, ".sangkar", "config.json"));
    expect(String(warnings[0]?.fields.reason)).toContain("invalid JSON");
  });

  test("a JSON value that is not an object yields the defaults and one warning", () => {
    for (const body of ["[]", "null", '"config"', "42"]) {
      const repoRoot = repoWithConfig(body, "sangkar-repo-config-nonobject-");
      const { config, warnings } = readWithWarnings(repoRoot);

      expect(config).toEqual({
        repoRoot,
        dataDir: join(repoRoot, ".sangkar"),
        watch: true,
        watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
        telemetry: true,
      });
      expect(warnings).toHaveLength(1);
      expect(warnings[0]?.fields.reason).toBe("not a JSON object");
    }
  });

  test("an absent field takes its default with one warning, the present ones are kept", () => {
    const repoRoot = repoWithConfig('{"watch_interval":5000}');
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config.watch).toBe(true);
    expect(config.telemetry).toBe(true);
    expect(config.watchIntervalMs).toBe(5_000);
    expect(warnings.map((w) => w.fields.field)).toEqual(["watch", "telemetry"]);
  });
});

describe("readRepoConfig booleans", () => {
  test('a quoted "yes" is not a boolean, so the field falls back to the default', () => {
    const repoRoot = repoWithConfig('{"watch":"yes","telemetry":"yes"}');
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config.watch).toBe(true);
    expect(config.telemetry).toBe(true);
    expect(warnings).toHaveLength(2);
    expect(warnings[0]?.msg).toContain("field ignored");
  });

  test("a JSON number is not a boolean either", () => {
    const repoRoot = repoWithConfig('{"watch":1,"telemetry":0}');
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config.watch).toBe(true);
    expect(config.telemetry).toBe(true);
    expect(warnings).toHaveLength(2);
  });

  test("explicit false in the file is honoured, not overridden by the default", () => {
    const repoRoot = repoWithConfig('{"watch":false,"watch_interval":2000,"telemetry":false}');
    const { config, warnings } = readWithWarnings(repoRoot);

    expect(config.watch).toBe(false);
    expect(config.telemetry).toBe(false);
    expect(warnings).toHaveLength(0);
  });
});

describe("readRepoConfig watch_interval", () => {
  const cases: ReadonlyArray<readonly [string, number]> = [
    ["5", WATCH_INTERVAL_MIN_MS],
    ["1", WATCH_INTERVAL_MIN_MS],
    ["999", WATCH_INTERVAL_MIN_MS],
    ["1000", 1_000],
    ["2000", 2_000],
    ["300000", WATCH_INTERVAL_MAX_MS],
    ["500000", WATCH_INTERVAL_MAX_MS],
    ["-1", WATCH_INTERVAL_DEFAULT_MS],
    ["0", WATCH_INTERVAL_DEFAULT_MS],
    ["2.5", WATCH_INTERVAL_DEFAULT_MS],
    ['"2000"', WATCH_INTERVAL_DEFAULT_MS],
    ["null", WATCH_INTERVAL_DEFAULT_MS],
  ];

  test("clamps into the band and falls back to the default for nonsense", () => {
    for (const [raw, expected] of cases) {
      const repoRoot = repoWithConfig(
        `{"watch":true,"watch_interval":${raw},"telemetry":true}`,
        "sangkar-repo-config-interval-",
      );
      const { config } = readWithWarnings(repoRoot);
      expect({ raw, interval: config.watchIntervalMs }).toEqual({ raw, interval: expected });
    }
  });

  test("an absent interval is the default", () => {
    const repoRoot = repoWithConfig('{"watch":true,"telemetry":true}');
    expect(readWithWarnings(repoRoot).config.watchIntervalMs).toBe(WATCH_INTERVAL_DEFAULT_MS);
  });
});

describe("writeRepoConfig", () => {
  test("writes the snake_case shape and creates a missing data dir", () => {
    const repoRoot = tempRepo();
    const dataDir = join(repoRoot, "nested", ".sangkar");
    const config: RepoConfig = {
      repoRoot,
      dataDir,
      watch: true,
      watchIntervalMs: 2_000,
      telemetry: true,
    };

    writeRepoConfig(config);

    expect(readFileSync(repoConfigPath(dataDir), "utf8")).toBe(
      '{"watch":true,"watch_interval":2000,"telemetry":true}',
    );
  });

  test("a full valid file round-trips through writeRepoConfig and readRepoConfig", () => {
    const repoRoot = repoWithConfig(
      '{"watch":false,"watch_interval":9000,"telemetry":false}',
      "sangkar-repo-config-roundtrip-",
    );
    const config: RepoConfig = {
      repoRoot,
      dataDir: join(repoRoot, ".sangkar"),
      watch: false,
      watchIntervalMs: 9_000,
      telemetry: false,
    };

    writeRepoConfig(config);

    const { config: read, warnings } = readWithWarnings(repoRoot);
    expect(warnings).toEqual([]);
    expect(read).toEqual(config);
  });

  test("an explicit data dir is what readRepoConfig reads", () => {
    const repoRoot = tempRepo();
    const dataDir = join(repoRoot, "var", "graph");
    writeRepoConfig({ repoRoot, dataDir, watch: false, watchIntervalMs: 1_500, telemetry: true });

    const { config } = readWithWarnings(repoRoot, dataDir);
    expect(config.dataDir).toBe(dataDir);
    expect(config.watch).toBe(false);
    expect(config.watchIntervalMs).toBe(1_500);
    expect(config.telemetry).toBe(true);
  });
});

describe("recordingEnabled", () => {
  const recorded: RepoConfig = {
    repoRoot: "/tmp/repo",
    dataDir: "/tmp/repo/.sangkar",
    watch: true,
    watchIntervalMs: WATCH_INTERVAL_DEFAULT_MS,
    telemetry: true,
  };

  test("true only when consent, registration and the repo file all allow it", () => {
    expect(recordingEnabled({ consent: true, repo: recorded, registered: true })).toBe(true);
  });

  test("each of the three is a veto on its own", () => {
    expect(recordingEnabled({ consent: false, repo: recorded, registered: true })).toBe(false);
    expect(recordingEnabled({ consent: true, repo: recorded, registered: false })).toBe(false);
    expect(
      recordingEnabled({
        consent: true,
        repo: { ...recorded, telemetry: false },
        registered: true,
      }),
    ).toBe(false);
  });

  test("all three refusing is still false, and never throws", () => {
    expect(
      recordingEnabled({
        consent: false,
        repo: { ...recorded, telemetry: false },
        registered: false,
      }),
    ).toBe(false);
  });
});
