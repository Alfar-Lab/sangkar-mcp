import { describe, expect, test } from "bun:test";
import {
  CliUsageError,
  GLOBAL_VALUE_FLAGS,
  type InputLiteral,
  parseArgs,
  parseCliArgs,
  resolveCommand,
  splitGlobals,
  usageOf,
} from "#shared/cli/args";
import { AppError } from "#shared/errors";
import { input as expandInput } from "../src/tools/expand/manifest";
import { input as impactInput } from "../src/tools/impact/manifest";
import { input as indexInput } from "../src/tools/index/manifest";
import { input as searchInput } from "../src/tools/search/manifest";
import { input as statusInput } from "../src/tools/status/manifest";
import { input as summaryInput } from "../src/tools/summary/manifest";
import { input as traceInput } from "../src/tools/trace/manifest";

/**
 * The CLI parser is one contract with two callers — a future `sangkar <tool>` command and the MCP
 * surface — so these tests read the real manifests rather than a fixture. A field added to a tool
 * without teaching the parser about it fails here.
 */
const MANIFESTS: readonly (readonly [string, InputLiteral])[] = [
  ["status", statusInput],
  ["summary", summaryInput],
  ["trace", traceInput],
  ["search", searchInput],
  ["impact", impactInput],
  ["expand", expandInput],
  ["index", indexInput],
];

/** The three tools whose one required string argument a bare token may bind. */
const POSITIONAL_TOOLS: readonly (readonly [string, InputLiteral, string])[] = [
  ["trace", traceInput, "query"],
  ["search", searchInput, "query"],
  ["expand", expandInput, "target"],
];

/** The four tools with no required string argument, so a bare token has nowhere to go. */
const NO_POSITIONAL_TOOLS: readonly (readonly [string, InputLiteral])[] = [
  ["status", statusInput],
  ["summary", summaryInput],
  ["impact", impactInput],
  ["index", indexInput],
];

function usageErrorOf(run: () => unknown): CliUsageError {
  try {
    run();
  } catch (e) {
    if (e instanceof CliUsageError) {
      return e;
    }
    throw e;
  }
  throw new Error("expected a CliUsageError");
}

function appErrorOf(run: () => unknown): AppError {
  try {
    run();
  } catch (e) {
    if (e instanceof AppError) {
      return e;
    }
    throw e;
  }
  throw new Error("expected an AppError");
}

describe("cli args: globals", () => {
  test("the value flags are the two the CLI knows", () => {
    expect(GLOBAL_VALUE_FLAGS).toEqual(["repo-root", "data-dir"]);
  });

  test("a verb's own flag is not a global: --out is refused", () => {
    // `sangkar telemetry --out` names a report path and nothing else, so promoting it to the
    // global set would make every tool accept it. The short list is the assertion.
    expect(GLOBAL_VALUE_FLAGS).toEqual(["repo-root", "data-dir"]);
    const refusal = usageErrorOf(() => parseCliArgs("status", statusInput, ["--out", "x"]));
    expect(refusal.message).toBe('unknown option "--out"');
    expect(refusal.message).toContain("--out");
  });

  test("switches and value flags are removed, everything else keeps its order", () => {
    const split = splitGlobals([
      "--json",
      "--repo-root",
      "/tmp/repo",
      "trace",
      "find callers",
      "--data-dir=/tmp/graph",
      "--depth",
      "2",
      "--help",
    ]);
    expect(split.json).toBe(true);
    expect(split.help).toBe(true);
    expect(split.rest).toEqual(["trace", "find callers", "--depth", "2"]);
  });

  test("plain argv passes through untouched", () => {
    expect(splitGlobals([])).toEqual({ json: false, help: false, rest: [] });
    expect(splitGlobals(["trace", "--depth", "2"])).toEqual({
      json: false,
      help: false,
      rest: ["trace", "--depth", "2"],
    });
  });

  test("-h is help, and a tool flag is never a global", () => {
    expect(splitGlobals(["-h"])).toEqual({ json: false, help: true, rest: [] });
    expect(splitGlobals(["--json-budget"])).toEqual({
      json: false,
      help: false,
      rest: ["--json-budget"],
    });
  });

  test("a value flag with no value is refused and names the flag", () => {
    const cases: readonly (readonly [readonly string[], string])[] = [
      [["--repo-root"], "--repo-root"],
      [["trace", "--data-dir"], "--data-dir"],
      [["--data-dir", "--json"], "--data-dir"],
      [["--repo-root="], "--repo-root"],
    ];
    for (const [argv, flag] of cases) {
      const refusal = usageErrorOf(() => splitGlobals(argv));
      expect(refusal.message).toBe(`${flag} needs a value`);
      expect(refusal.usage).toContain("--repo-root");
    }
  });
});

describe("cli args: where the command name sits", () => {
  test("a global flag before the name does not hide it", () => {
    // The entry point needs the name before any config exists, so `sangkar --json status` and
    // `sangkar status --json` have to name the same command.
    const cases: readonly (readonly [readonly string[], string])[] = [
      [["--json", "status"], "status"],
      [["--repo-root", "/x", "status"], "status"],
      [["--repo-root=/x", "status"], "status"],
      [["--data-dir", "/d", "--json", "trace", "q"], "trace"],
    ];
    for (const [argv, name] of cases) {
      expect({ argv, name: resolveCommand(argv).name }).toEqual({ argv, name });
    }
    // The tail is every token after the name: the globals before it are the entry point's, the
    // ones after it are the command's own to strip.
    expect(resolveCommand(["--data-dir", "/d", "--json", "trace", "q"]).rest).toEqual(["q"]);
  });

  test("globals after the name are kept for the command to consume", () => {
    expect(resolveCommand(["status", "--json"])).toEqual({
      name: "status",
      rest: ["--json"],
      help: false,
    });
    expect(resolveCommand(["status", "--repo-root", "/x"])).toEqual({
      name: "status",
      rest: ["--repo-root", "/x"],
      help: false,
    });
    // Including a value flag whose value is the last token: nothing is consumed before the name, so
    // the command sees exactly what the user typed and refuses the missing value itself.
    expect(resolveCommand(["trace", "--data-dir"]).rest).toEqual(["--data-dir"]);
  });

  test("no name at all resolves to null, and nothing is left over", () => {
    for (const argv of [[], ["--json"], ["--repo-root"], ["--repo-root="], ["--help"], ["-h"]]) {
      const resolution = resolveCommand(argv);
      expect({ argv, name: resolution.name }).toEqual({ argv, name: null });
      expect(resolution.rest).toEqual([]);
    }
  });

  test("--help anywhere sets help, and the name is still found", () => {
    for (const argv of [
      ["trace", "--help"],
      ["--help"],
      ["-h", "trace"],
      ["--data-dir", "/d", "--help", "status"],
    ]) {
      expect({ argv, help: resolveCommand(argv).help }).toEqual({ argv, help: true });
    }
    expect(resolveCommand(["trace", "--help"]).name).toBe("trace");
    expect(resolveCommand(["--help"]).name).toBeNull();
  });

  test("a value flag with nothing to consume is skipped rather than refused", () => {
    // This scan runs before a usage block exists, so it may not throw: the spaced form reads the
    // next token as its value when that token is not a flag, and otherwise consumes nothing and
    // lets the following token name the command.
    expect(resolveCommand(["--repo-root", "--json", "status"])).toEqual({
      name: "status",
      rest: [],
      help: false,
    });
    expect(resolveCommand(["--repo-root"])).toEqual({ name: null, rest: [], help: false });
    // A value written inline is its own token, so the name follows immediately.
    expect(resolveCommand(["--repo-root=", "status"]).name).toBe("status");
  });
});

describe("cli args: the positional argument", () => {
  test("a bare token binds the tool's one required string argument", () => {
    for (const [tool, input, field] of POSITIONAL_TOOLS) {
      const { rest } = splitGlobals(["--json", tool, "some name", "--budget", "100"]);
      const args = parseCliArgs(tool, input, rest.slice(1));
      expect(args[field]).toBe("some name");
      expect(args.budget).toBe(100);
    }
  });

  test("a second positional is refused", () => {
    const refusal = usageErrorOf(() => parseCliArgs("trace", traceInput, ["one", "two"]));
    expect(refusal.message).toBe('unexpected positional argument "two"');
    expect(refusal.usage).toBe(usageOf("trace", traceInput));
  });

  test("a positional already given as a flag cannot be given twice", () => {
    const refusal = usageErrorOf(() =>
      parseCliArgs("search", searchInput, ["--query", "one", "two"]),
    );
    expect(refusal.message).toBe('unexpected positional argument "two"');
  });

  test("a positional is refused for a tool with no required string argument", () => {
    for (const [tool, input] of NO_POSITIONAL_TOOLS) {
      const refusal = usageErrorOf(() => parseCliArgs(tool, input, ["somewhere"]));
      expect(refusal.message).toBe('unexpected positional argument "somewhere"');
    }
  });
});

describe("cli args: flags", () => {
  test("--name value and --name=value are both accepted", () => {
    expect(parseCliArgs("trace", traceInput, ["q", "--depth", "3"]).depth).toBe(3);
    expect(parseCliArgs("trace", traceInput, ["q", "--depth=3"]).depth).toBe(3);
    expect(parseCliArgs("trace", traceInput, ["--query", "find calls"]).query).toBe("find calls");
    expect(parseCliArgs("trace", traceInput, ["--query=find calls"]).query).toBe("find calls");
  });

  test("integer flags are parsed with Number, negatives and out-of-range included", () => {
    expect(parseCliArgs("status", statusInput, ["--budget", "1200"]).budget).toBe(1200);
    expect(parseCliArgs("status", statusInput, ["--budget=1200"]).budget).toBe(1200);
    // A single dash is a value, so a negative number survives argv reading.
    expect(parseCliArgs("impact", impactInput, ["--depth", "-1"]).depth).toBe(-1);
    // min/max are the handler's business: the parser neither clamps nor refuses.
    expect(parseCliArgs("trace", traceInput, ["q", "--depth", "99"]).depth).toBe(99);
  });

  test("a non-integer integer flag is refused", () => {
    for (const argv of [["--budget", "2.5"], ["--budget", "many"], ["--budget="]]) {
      const refusal = usageErrorOf(() => parseCliArgs("status", statusInput, argv));
      expect(refusal.message).toMatch(/^--budget (must be an integer|needs a value)$/);
    }
  });

  test("a flag with no value is refused", () => {
    for (const argv of [["q", "--depth"], ["q", "--depth", "--budget", "5"], ["--query"]]) {
      const refusal = usageErrorOf(() => parseCliArgs("trace", traceInput, argv));
      expect(refusal.message).toMatch(/^--(depth|query) needs a value$/);
    }
  });

  test("boolean fields take the switch, =true, =false and --no-", () => {
    const cases: readonly (readonly [string[], boolean])[] = [
      [["--force"], true],
      [["--force=true"], true],
      [["--force=false"], false],
      [["--no-force"], false],
    ];
    for (const [argv, expected] of cases) {
      expect(parseCliArgs("index", indexInput, argv).force).toBe(expected);
    }
  });

  test("a bad boolean value is refused", () => {
    const refusal = usageErrorOf(() => parseCliArgs("index", indexInput, ["--force=maybe"]));
    expect(refusal.message).toBe("--force must be true or false");
  });

  test("a boolean switch does not swallow the next token", () => {
    const refusal = usageErrorOf(() => parseCliArgs("index", indexInput, ["--force", "full"]));
    expect(refusal.message).toBe('unexpected positional argument "full"');
  });

  test("array fields collect repeats and never split a comma", () => {
    expect(
      parseCliArgs("impact", impactInput, ["--paths", "a.ts", "--paths", "b.ts"]).paths,
    ).toEqual(["a.ts", "b.ts"]);
    expect(parseCliArgs("status", statusInput, ["--paths=a.ts", "--paths=b.ts"]).paths).toEqual([
      "a.ts",
      "b.ts",
    ]);
    expect(parseCliArgs("status", statusInput, ["--paths", "one,two"]).paths).toEqual(["one,two"]);
  });
});

describe("cli args: refusals keep the schema's own message", () => {
  test("an unknown flag is refused and named", () => {
    expect(
      usageErrorOf(() => parseCliArgs("search", searchInput, ["q", "--colour", "red"])).message,
    ).toBe('unknown option "--colour"');
    // `--no-…` is only a switch for a boolean field.
    expect(usageErrorOf(() => parseCliArgs("index", indexInput, ["--no-mode"])).message).toBe(
      'unknown option "--no-mode"',
    );
  });

  test("an enum value outside the manifest reads exactly as parseArgs reads it", () => {
    const fromCli = usageErrorOf(() =>
      parseCliArgs("search", searchInput, ["q", "--kind", "nope"]),
    );
    const fromCall = appErrorOf(() =>
      parseArgs("search", searchInput, { query: "q", kind: "nope" }),
    );
    expect(fromCli.message).toBe(fromCall.message);
    expect(fromCli.message).toBe('search received an invalid "kind" argument');
    expect(fromCli.usage).toBe(usageOf("search", searchInput));
    expect(parseCliArgs("search", searchInput, ["q", "--kind=file"]).kind).toBe("file");
  });

  test("a missing required argument reads exactly as parseArgs reads it", () => {
    const fromCli = usageErrorOf(() => parseCliArgs("trace", traceInput, ["--depth", "2"]));
    const fromCall = appErrorOf(() => parseArgs("trace", traceInput, { depth: 2 }));
    expect(fromCli.message).toBe(fromCall.message);
    expect(fromCli.message).toBe('trace is missing the required argument "query"');
    expect(fromCli.usage).toBe(usageOf("trace", traceInput));
  });
});

describe("cli args: parseArgs is reached, not reimplemented", () => {
  test("defaults and dropped keys behave exactly as a protocol call", () => {
    expect(parseCliArgs("index", indexInput, [])).toEqual(parseArgs("index", indexInput, {}));
    expect(parseCliArgs("index", indexInput, [])).toEqual({
      force: false,
      mode: "full",
      register: true,
    });
    expect(parseCliArgs("index", indexInput, ["--no-register"])).toEqual({
      force: false,
      mode: "full",
      register: false,
    });
    expect(parseCliArgs("status", statusInput, [])).toEqual({ paths: [], budget: 800 });
    // A key no manifest declares never reaches the handler, whichever layer asks.
    expect(parseArgs("search", searchInput, { query: "q", colour: "red" })).toEqual({
      query: "q",
      budget: 2000,
    });
  });
});

describe("cli args: usage", () => {
  test("every manifest field is named, and the global flags close the block", () => {
    for (const [tool, input] of MANIFESTS) {
      const usage = usageOf(tool, input);
      const lines = usage.split("\n");
      expect(lines[0]?.startsWith(`sangkar ${tool}`)).toBe(true);
      expect(lines.length).toBe(Object.keys(input).length + 2);
      expect(lines.at(-1)).toBe(
        "global options: --repo-root <path>, --data-dir <path>, --json (JSON instead of TOON)",
      );
      for (const name of Object.keys(input)) {
        expect(usage).toContain(`--${name} <`);
      }
    }
  });

  test("the header names the positional the command accepts", () => {
    expect(usageOf("trace", traceInput).split("\n")[0]).toBe("sangkar trace [<query>] [options]");
    expect(usageOf("expand", expandInput).split("\n")[0]).toBe(
      "sangkar expand [<target>] [options]",
    );
    expect(usageOf("status", statusInput).split("\n")[0]).toBe("sangkar status [options]");
  });

  test("a required field is marked and a description is reproduced", () => {
    expect(usageOf("trace", traceInput)).toContain("--query <string> (required)");
    expect(usageOf("index", indexInput)).toContain(
      "Rebuild from an empty stage and publish even when the file hashes say nothing changed.",
    );
  });
});
