import { describe, expect, spyOn, test } from "bun:test";
import { join, resolve } from "node:path";
import {
  guardRuntime,
  MIN_BUN,
  RUNTIME_EXIT,
  type RuntimeProbe,
  readRuntime,
  runtimeDiagnostic,
} from "#shared/runtime";

/**
 * The preflight is the only thing between an unsupported interpreter and a crash deep inside
 * `bun:sqlite`, so it is asserted in three places: the decision itself (a table, not a sample),
 * the wiring inside this process (one stderr line, stdout untouched, exit 70) and one real child
 * process, because "wrote nothing to stdout" is a property of a stream, not of a function.
 */

const APP_ROOT = resolve(import.meta.dir, "..");
const ENTRY_POINT = join(APP_ROOT, "bin", "sangkar.ts");
const RUNTIME_MODULE = join(APP_ROOT, "src", "shared", "runtime.ts");

/** `node` is the version Bun reports through `process.version` in the foreign-interpreter row. */
const NODE_VERSION = "v26.7.0";

interface Case {
  readonly probe: RuntimeProbe;
  readonly supported: boolean;
  readonly why: string;
}

const CASES: readonly Case[] = [
  { probe: { bun: "1.4.4", node: NODE_VERSION }, supported: true, why: "exactly the floor" },
  { probe: { bun: "1.5.0", node: NODE_VERSION }, supported: true, why: "above the floor" },
  { probe: { bun: "1.10.0", node: NODE_VERSION }, supported: true, why: "1.10 is past 1.5" },
  { probe: { bun: "1.4.3", node: NODE_VERSION }, supported: false, why: "one patch short" },
  { probe: { bun: "1.3.9", node: NODE_VERSION }, supported: false, why: "one minor short" },
  { probe: { bun: undefined, node: NODE_VERSION }, supported: false, why: "not bun at all" },
];

function refusalOf(probe: RuntimeProbe): Record<string, unknown> {
  const diagnostic = runtimeDiagnostic(probe);
  if (diagnostic === null) {
    throw new Error(`expected a refusal for ${JSON.stringify(probe)}`);
  }
  return JSON.parse(diagnostic) as Record<string, unknown>;
}

describe("runtime: the version floor is one number", () => {
  test("MIN_BUN matches the floor package.json declares", async () => {
    const manifest = (await Bun.file(join(APP_ROOT, "package.json")).json()) as {
      engines?: { bun?: string };
    };
    expect(manifest.engines?.bun).toBe(`>=${MIN_BUN}`);
    expect(MIN_BUN).toBe("1.4.4");
    expect(RUNTIME_EXIT).toBe(70);
  });
});

describe("runtime: runtimeDiagnostic", () => {
  for (const { probe, supported, why } of CASES) {
    const label = `bun ${probe.bun ?? "(absent)"} / node ${probe.node ?? "(absent)"} — ${why}`;

    test(`${supported ? "supported" : "refused"}: ${label}`, () => {
      if (supported) {
        expect(runtimeDiagnostic(probe)).toBeNull();
        return;
      }
      const payload = refusalOf(probe);
      expect(payload.level).toBe("error");
      expect(payload.msg).toBe("runtime.unsupported");
      expect(payload.required).toBe(`Bun >= ${MIN_BUN}`);
      expect(payload.hint).toContain(`install Bun >= ${MIN_BUN}`);
      expect(String(payload.found)).toBe(probe.bun ?? probe.node ?? "unknown");
    });
  }

  test("a refused line is one line of JSON, not a paragraph", () => {
    const diagnostic = runtimeDiagnostic({ bun: "1.3.9", node: NODE_VERSION }) ?? "";
    expect(diagnostic).not.toContain("\n");
    const keys = Object.keys(refusalOf({ bun: "1.3.9", node: NODE_VERSION }));
    expect(keys).toEqual(["level", "msg", "required", "found", "resolved", "hint"]);
  });

  test("a foreign interpreter is named as such, not given a bun path", () => {
    const payload = refusalOf({ bun: undefined, node: NODE_VERSION });
    expect(payload.resolved).toBe("not bun");
    expect(payload.found).toBe(NODE_VERSION);
  });

  test("a too-old bun is named by the binary that is running it", () => {
    const payload = refusalOf({ bun: "1.3.9", node: NODE_VERSION });
    expect(payload.resolved).not.toBe("not bun");
    expect(String(payload.resolved).length).toBeGreaterThan(0);
  });
});

describe("runtime: the guard", () => {
  test("a supported runtime writes nothing and reports no exit code", () => {
    const written: string[] = [];
    expect(guardRuntime(readRuntime(), (text) => written.push(text))).toBeNull();
    expect(
      guardRuntime({ bun: MIN_BUN, node: undefined }, (text) => written.push(text)),
    ).toBeNull();
    expect(written).toEqual([]);
  });

  test("a refusal writes exactly one stderr line and never touches stdout", () => {
    const stdout = spyOn(process.stdout, "write");
    const written: string[] = [];
    try {
      const code = guardRuntime({ bun: "1.4.3", node: NODE_VERSION }, (text) => written.push(text));
      expect(code).toBe(70);
      expect(written).toHaveLength(1);
      expect(written[0]).toMatch(/^\{.*\}\n$/);
      expect(JSON.parse(String(written[0]))).toMatchObject({
        msg: "runtime.unsupported",
        required: `Bun >= ${MIN_BUN}`,
      });
      expect(stdout).not.toHaveBeenCalled();
    } finally {
      stdout.mockRestore();
    }
  });

  test("a real process exits 70 with one stderr line and a byte-empty stdout", () => {
    const script = `const { guardRuntime } = await import(${JSON.stringify(RUNTIME_MODULE)});
const code = guardRuntime({ bun: "1.4.3", node: undefined });
process.exit(code ?? 0);`;
    const child = Bun.spawnSync({
      cmd: [process.execPath, "--eval", script],
      cwd: APP_ROOT,
      stdout: "pipe",
      stderr: "pipe",
    });

    expect(child.exitCode).toBe(70);
    expect(child.stdout.byteLength).toBe(0);

    const lines = new TextDecoder()
      .decode(child.stderr)
      .split("\n")
      .filter((line) => line.length > 0);
    expect(lines).toHaveLength(1);
    expect(JSON.parse(String(lines[0]))).toMatchObject({
      level: "error",
      msg: "runtime.unsupported",
      required: `Bun >= ${MIN_BUN}`,
    });
  });

  test("the entry point guards before it imports the kernel", async () => {
    const entry = await Bun.file(ENTRY_POINT).text();
    const guard = entry.indexOf("guardRuntime(");
    const kernel = entry.indexOf('await import("../src/index")');
    // Two positive assertions first: an absent match would otherwise pass the ordering check.
    expect(guard).toBeGreaterThan(-1);
    expect(kernel).toBeGreaterThan(-1);
    expect(guard).toBeLessThan(kernel);
  });
});
