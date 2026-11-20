import { afterAll, describe, expect, test } from "bun:test";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir, tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import type { AppError } from "#shared/errors";
import type { LogFields, Logger } from "#shared/log";
import {
  canonicalRoot,
  EMPTY_REGISTRY,
  REGISTRY_VERSION,
  readRegistry,
  registerRepo,
  removeRepo,
  type Selection,
  type SelectionFailure,
  selectRepo,
  writeRegistry,
} from "#shared/registry";

/**
 * The registry is a hand-editable file shared by every process on the machine, so these cases are
 * about refusal rather than success: a corrupt file must degrade to `EMPTY_REGISTRY` with one
 * warning and never throw, and `selectRepo`'s ladder must keep an explicit request ahead of the
 * `--repo-root` pin while the pin still answers every caller that sends no request.
 */

const HOME = homedir();
const dirs: string[] = [];

function tempDir(prefix = "sangkar-registry-"): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  dirs.push(dir);
  return dir;
}

/** The canonical spelling of a temp dir, which is what `canonicalRoot` returns for an existing one. */
function canonical(dir: string): string {
  return realpathSync(dir);
}

function registryFile(dir: string): string {
  return join(dir, "sangkar.json");
}

function writeRaw(path: string, body: string): void {
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, body);
}

function writeJson(path: string, value: unknown): void {
  writeRaw(path, JSON.stringify(value));
}

interface Captured {
  readonly logger: Logger;
  readonly warns: LogFields[];
}

function capture(): Captured {
  const warns: LogFields[] = [];
  const logger: Logger = {
    debug: () => {},
    info: () => {},
    warn: (_msg, fields) => {
      warns.push(fields ?? {});
    },
    error: () => {},
  };
  return { logger, warns };
}

function refused(result: Selection | SelectionFailure): AppError {
  if (result.ok) {
    throw new Error(`expected a refusal, got ${result.repoRoot}`);
  }
  return result.error;
}

function selected(result: Selection | SelectionFailure): string {
  if (!result.ok) {
    throw new Error(`expected a selection, got ${result.error.code}: ${result.error.message}`);
  }
  return result.repoRoot;
}

/** The hint as the caller reads it: one repo per line, so a test can assert the whole list. */
function hintLines(error: AppError): string[] {
  return (error.hint ?? "")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line !== "");
}

afterAll(() => {
  for (const dir of dirs) {
    rmSync(dir, { recursive: true, force: true });
  }
});

describe("canonicalRoot", () => {
  test("expands a leading ~/ against the injected home", () => {
    const home = tempDir();
    expect(canonicalRoot("~/Documents/a", { home })).toBe(join(home, "Documents/a"));
  });

  test("realpaths a path that exists and keeps the resolved form for one that does not", () => {
    const dir = tempDir();
    const missing = join(dir, "deleted");
    expect(canonicalRoot(dir)).toBe(canonical(dir));
    expect(canonicalRoot(missing)).toBe(resolve(missing));
  });

  test("resolves a relative path instead of returning it unresolved", () => {
    expect(canonicalRoot("./a/../b")).toBe(resolve("b"));
  });
});

describe("readRegistry", () => {
  test("a missing file is EMPTY_REGISTRY with exactly one warning", () => {
    const { logger, warns } = capture();
    expect(readRegistry(join(tempDir(), "absent.json"), { log: logger })).toBe(EMPTY_REGISTRY);
    expect(warns).toHaveLength(1);
    expect(String(warns[0]?.reason)).toContain("ENOENT");
  });

  test("a version other than 1 is EMPTY_REGISTRY with exactly one warning", () => {
    const path = registryFile(tempDir());
    writeJson(path, { version: 2, repos: ["/srv/a"] });
    const { logger, warns } = capture();
    expect(readRegistry(path, { log: logger })).toBe(EMPTY_REGISTRY);
    expect(warns).toHaveLength(1);
    expect(String(warns[0]?.reason)).toContain("version");
  });

  test("invalid JSON, a non-object, and a non-array repos each warn once and never throw", () => {
    const bodies = [
      "{not json",
      "[]",
      '"a string"',
      "null",
      '{"repos":["/srv/a"]}',
      '{"version":1,"repos":"/srv/a"}',
    ];
    for (const body of bodies) {
      const path = registryFile(tempDir());
      writeRaw(path, body);
      const { logger, warns } = capture();
      expect(readRegistry(path, { log: logger })).toBe(EMPTY_REGISTRY);
      expect(warns).toHaveLength(1);
    }
  });

  test("an unreadable path (a directory) is EMPTY_REGISTRY with exactly one warning", () => {
    const path = registryFile(tempDir());
    mkdirSync(path);
    const { logger, warns } = capture();
    expect(readRegistry(path, { log: logger })).toBe(EMPTY_REGISTRY);
    expect(warns).toHaveLength(1);
  });

  test("a non-string entry is dropped with exactly one warning", () => {
    const path = registryFile(tempDir());
    const kept = join(tempDir(), "kept");
    writeJson(path, { version: 1, repos: [kept, 7, null, true] });
    const { logger, warns } = capture();
    expect(readRegistry(path, { log: logger })).toEqual({
      version: REGISTRY_VERSION,
      repos: [resolve(kept)],
    });
    expect(warns).toHaveLength(1);
    expect(warns[0]?.dropped).toBe(3);
  });

  test("~ expands and duplicates collapse with the first occurrence keeping its position", () => {
    const path = registryFile(tempDir());
    const first = join(tempDir(), "first");
    writeJson(path, { version: 1, repos: [first, "~//b", `~/${"b"}`, first] });
    expect(readRegistry(path)).toEqual({
      version: REGISTRY_VERSION,
      repos: [resolve(first), join(HOME, "b")],
    });
  });
});

describe("writeRegistry", () => {
  test("creates the parent directories and writes the file shape verbatim", () => {
    const dir = tempDir();
    const path = join(dir, "nested/deeper/sangkar.json");
    writeRegistry(path, { version: REGISTRY_VERSION, repos: ["/srv/b", "~/Documents/a"] });
    expect(readFileSync(path, "utf8")).toBe('{"version":1,"repos":["/srv/b","~/Documents/a"]}');
    expect(existsSync(`${path}.tmp`)).toBe(false);
  });

  test("always stamps the current version, whatever the argument says", () => {
    const path = registryFile(tempDir());
    writeRegistry(path, { version: 99, repos: ["/srv/a"] });
    expect(readFileSync(path, "utf8")).toBe('{"version":1,"repos":["/srv/a"]}');
  });
});

describe("registerRepo", () => {
  test("registers once, reports added:false the second time, and leaves one entry", () => {
    const path = registryFile(tempDir());
    const repo = tempDir();
    const { logger } = capture();

    const first = registerRepo(path, repo, { log: logger });
    expect(first.added).toBe(true);
    expect(first.registry.repos).toEqual([canonical(repo)]);

    const second = registerRepo(path, repo, { log: logger });
    expect(second.added).toBe(false);
    expect(second.registry.repos).toHaveLength(1);
    expect(readRegistry(path, { log: logger }).repos).toEqual([canonical(repo)]);
  });

  test("an existing path is realpathed, a missing one keeps its resolved form", () => {
    const path = registryFile(tempDir());
    const existing = tempDir();
    const missing = join(tempDir(), "deleted");
    const { logger } = capture();

    expect(registerRepo(path, existing, { log: logger }).added).toBe(true);
    expect(registerRepo(path, missing, { log: logger }).added).toBe(true);
    expect(readRegistry(path, { log: logger }).repos).toEqual([
      canonical(existing),
      resolve(missing),
    ]);
  });

  test("a write failure is swallowed into one warning and no throw", () => {
    const blocker = join(tempDir(), "not-a-dir");
    writeRaw(blocker, "x");
    const { logger, warns } = capture();
    const result = registerRepo(join(blocker, "sangkar.json"), tempDir(), { log: logger });
    expect(result.added).toBe(false);
    expect(result.registry.repos).toEqual([]);
    // The unreadable registry is one warning, the failed write the second.
    expect(warns).toHaveLength(2);
  });
});

describe("removeRepo", () => {
  test("removes a registered root once, then reports removed:false", () => {
    const path = registryFile(tempDir());
    const kept = tempDir();
    const dropped = tempDir();
    const { logger } = capture();
    registerRepo(path, kept, { log: logger });
    registerRepo(path, dropped, { log: logger });

    const first = removeRepo(path, dropped, { log: logger });
    expect(first.removed).toBe(true);
    expect(first.registry.repos).toEqual([canonical(kept)]);

    const second = removeRepo(path, dropped, { log: logger });
    expect(second.removed).toBe(false);
    expect(readRegistry(path, { log: logger }).repos).toEqual([canonical(kept)]);
  });
});

describe("selectRepo", () => {
  test("an explicit request outranks the pin", () => {
    const a = tempDir();
    const b = tempDir();
    const result = selectRepo({
      registry: { version: REGISTRY_VERSION, repos: [canonical(a)] },
      requested: a,
      cwd: tempDir(),
      pinnedRoot: b,
    });
    expect(selected(result)).toBe(canonical(a));
  });

  test("the canonical path wins over a basename shared by two entries", () => {
    const one = join(tempDir(), "one/dup");
    const two = join(tempDir(), "two/dup");
    mkdirSync(one, { recursive: true });
    mkdirSync(two, { recursive: true });
    const result = selectRepo({
      registry: { version: REGISTRY_VERSION, repos: [canonical(one), canonical(two)] },
      requested: two,
      cwd: tempDir(),
    });
    expect(selected(result)).toBe(canonical(two));
  });

  test("an unregistered request is not_found even while a pin is set", () => {
    const registered = tempDir();
    const result = selectRepo({
      registry: { version: REGISTRY_VERSION, repos: [canonical(registered)] },
      requested: "no-such-repo",
      cwd: tempDir(),
      pinnedRoot: tempDir(),
    });
    const error = refused(result);
    expect(error.code).toBe("not_found");
    expect(hintLines(error)).toEqual(["Registered repos:", canonical(registered)]);
  });

  test("a basename selects its single registered repo", () => {
    const repo = join(tempDir(), "named");
    mkdirSync(repo, { recursive: true });
    const result = selectRepo({
      registry: { version: REGISTRY_VERSION, repos: [canonical(repo)] },
      requested: "named",
      cwd: tempDir(),
    });
    expect(selected(result)).toBe(canonical(repo));
  });

  test("a basename shared by two entries is ambiguous and names both", () => {
    const one = join(tempDir(), "one/dup");
    const two = join(tempDir(), "two/dup");
    mkdirSync(one, { recursive: true });
    mkdirSync(two, { recursive: true });
    const error = refused(
      selectRepo({
        registry: { version: REGISTRY_VERSION, repos: [canonical(one), canonical(two)] },
        requested: "dup",
        cwd: tempDir(),
      }),
    );
    expect(error.code).toBe("ambiguous");
    expect(hintLines(error)).toEqual(["Registered repos:", canonical(one), canonical(two)]);
  });

  test("the pin answers with an empty registry and with a multi-entry registry", () => {
    const pin = tempDir();
    expect(
      selected(selectRepo({ registry: EMPTY_REGISTRY, cwd: tempDir(), pinnedRoot: pin })),
    ).toBe(canonical(pin));
    const a = tempDir();
    const b = tempDir();
    const multi = { version: REGISTRY_VERSION, repos: [canonical(a), canonical(b)] };
    expect(selected(selectRepo({ registry: multi, cwd: tempDir(), pinnedRoot: pin }))).toBe(
      canonical(pin),
    );
  });

  test("an empty or null pin is not a pin", () => {
    const only = tempDir();
    const registry = { version: REGISTRY_VERSION, repos: [canonical(only)] };
    expect(selected(selectRepo({ registry, cwd: tempDir(), pinnedRoot: "" }))).toBe(
      canonical(only),
    );
    expect(selected(selectRepo({ registry, cwd: tempDir(), pinnedRoot: null }))).toBe(
      canonical(only),
    );
  });

  test("a request outranks the cwd match", () => {
    const a = tempDir();
    const b = tempDir();
    const result = selectRepo({
      registry: { version: REGISTRY_VERSION, repos: [canonical(a), canonical(b)] },
      requested: a,
      cwd: b,
    });
    expect(selected(result)).toBe(canonical(a));
  });

  test("a cwd inside the registry wins over the single-repo default", () => {
    const a = tempDir();
    const b = tempDir();
    const registry = { version: REGISTRY_VERSION, repos: [canonical(a), canonical(b)] };
    expect(selected(selectRepo({ registry, cwd: b }))).toBe(canonical(b));
  });

  test("exactly one registered repo answers a cwd that matches nothing", () => {
    const only = tempDir();
    const registry = { version: REGISTRY_VERSION, repos: [canonical(only)] };
    expect(selected(selectRepo({ registry, cwd: tempDir() }))).toBe(canonical(only));
  });

  test("an empty registry refuses with not_found and the add hint", () => {
    const error = refused(selectRepo({ registry: EMPTY_REGISTRY, cwd: tempDir() }));
    expect(error.code).toBe("not_found");
    expect(error.hint).toBe("sangkar repo add <path>");
  });

  test("several repos with no cwd match refuse with ambiguous and list them all", () => {
    const a = tempDir();
    const b = tempDir();
    const error = refused(
      selectRepo({
        registry: { version: REGISTRY_VERSION, repos: [canonical(a), canonical(b)] },
        cwd: tempDir(),
      }),
    );
    expect(error.code).toBe("ambiguous");
    expect(hintLines(error)).toEqual(["Registered repos:", canonical(a), canonical(b)]);
  });
});
