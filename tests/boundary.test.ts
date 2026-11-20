import { describe, expect, test } from "bun:test";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { isUnderPrefix } from "#shared/paths";

/**
 * Module isolation as a test failure instead of a review comment (TECH-SPEC.md §3–§4,
 * plan.constraint_invariant.md). The rules below are pure functions of
 * `(repoRelativeFile, importSpecifier)` so a synthetic input can prove a rule fires without
 * adding fixture files under `src/`.
 */

const APP_ROOT = join(import.meta.dir, "..");
const ROOT_FILE = "src/index.ts";

export type RuleId =
  | "tool-isolation"
  | "indexer-isolation"
  | "shared-isolation"
  | "drizzle-location"
  | "root-privileges"
  | "daemon-privileges";

const RULE_TEXT: Record<RuleId, string> = {
  "tool-isolation":
    "src/tools/<tool>/** may import only #shared/* and ./ siblings in the same tool",
  "indexer-isolation":
    "src/indexer/** may import only #shared/* and ./ siblings inside src/indexer",
  "shared-isolation": "src/shared/** may never import src/tools/** or src/indexer/**",
  "drizzle-location": "drizzle-orm may be imported only under src/shared/store/**",
  "root-privileges":
    "src/index.ts is the only module allowed to import the indexer or a tool handler, and it may import only #shared/*, ./indexer/* and ./tools/*/manifest or ./tools/*/handler",
  "daemon-privileges":
    "src/daemon/** may import only #shared/*, src/indexer/** and ./ siblings inside src/daemon, never src/tools/**",
};

export interface Violation {
  rule: RuleId;
  file: string;
  specifier: string;
  /** Names the offending file, the import path and the rule, in one line. */
  message: string;
}

/** `null` means the import does not break this rule. */
type RuleCheck = (file: string, specifier: string) => string | null;

function dirOf(file: string): string {
  const slash = file.lastIndexOf("/");
  return slash === -1 ? "" : file.slice(0, slash);
}

function collapse(path: string): string {
  const kept: string[] = [];
  for (const segment of path.split("/")) {
    if (segment === "" || segment === ".") {
      continue;
    }
    if (segment === ".." && kept.length > 0 && kept[kept.length - 1] !== "..") {
      kept.pop();
    } else {
      kept.push(segment);
    }
  }
  return kept.join("/");
}

function stripModuleSuffix(path: string): string {
  const extension = /\.[cm]?[jt]sx?$/.exec(path);
  const withoutExtension = extension === null ? path : path.slice(0, extension.index);
  return withoutExtension.endsWith("/index")
    ? withoutExtension.slice(0, -"/index".length)
    : withoutExtension;
}

/**
 * App-relative target of a specifier, or `null` when the specifier leaves the app's own module
 * graph (a package or a `node:`/`bun` builtin). Private `#tools`/`#indexer` aliases are resolved
 * too even though `package.json` maps only `#shared/*`, so importing them is reported as the
 * boundary defect it is rather than as an unresolvable alias.
 */
export function resolveSpecifier(fromFile: string, specifier: string): string | null {
  if (specifier.startsWith("#")) {
    const [namespace, ...rest] = specifier.slice(1).split("/");
    if (namespace !== "shared" && namespace !== "tools" && namespace !== "indexer") {
      return null;
    }
    return stripModuleSuffix(collapse(["src", namespace, ...rest].join("/")));
  }
  if (!specifier.startsWith(".")) {
    return null;
  }
  return stripModuleSuffix(collapse(`${dirOf(fromFile)}/${specifier}`));
}

function isSharedAlias(specifier: string): boolean {
  return specifier === "#shared" || specifier.startsWith("#shared/");
}

function toolOf(file: string): string | null {
  return /^src\/tools\/([^/]+)\//.exec(file)?.[1] ?? null;
}

function targetsPrivateArea(target: string): boolean {
  return isUnderPrefix(target, "src/tools") || isUnderPrefix(target, "src/indexer");
}

function isToolHandler(target: string): boolean {
  return isUnderPrefix(target, "src/tools") && /^src\/tools\/[^/]+\/handler$/.test(target);
}

function isToolManifest(target: string): boolean {
  return /^src\/tools\/[^/]+\/manifest$/.test(target);
}

/** Rule 1: a tool sees `#shared/*` and its own `./` siblings, never another tool. */
const toolIsolation: RuleCheck = (file, specifier) => {
  const tool = toolOf(file);
  if (tool === null || isSharedAlias(specifier)) {
    return null;
  }
  const target = resolveSpecifier(file, specifier);
  if (target === null) {
    return specifier.startsWith("#")
      ? `a tool may use only #shared/*, never the private alias ${specifier}`
      : null;
  }
  if (isUnderPrefix(target, `src/tools/${tool}`)) {
    return null;
  }
  return `tool "${tool}" may import only #shared/* and its own ./ siblings, but ${specifier} resolves to ${target}`;
};

/** Rule 2: the indexer sees `#shared/*` and its own `./` siblings. */
const indexerIsolation: RuleCheck = (file, specifier) => {
  if (!isUnderPrefix(file, "src/indexer") || isSharedAlias(specifier)) {
    return null;
  }
  const target = resolveSpecifier(file, specifier);
  if (target === null) {
    return specifier.startsWith("#")
      ? `the indexer may use only #shared/*, never the private alias ${specifier}`
      : null;
  }
  return isUnderPrefix(target, "src/indexer")
    ? null
    : `the indexer may import only #shared/* and its own ./ siblings, but ${specifier} resolves to ${target}`;
};

/** Rule 3: `shared` is the kernel; it may never look up at the tools or the indexer. */
const sharedIsolation: RuleCheck = (file, specifier) => {
  if (!isUnderPrefix(file, "src/shared")) {
    return null;
  }
  const target = resolveSpecifier(file, specifier);
  if (target === null || !targetsPrivateArea(target)) {
    return null;
  }
  return `src/shared/** must never import src/tools/** or src/indexer/**, but ${specifier} resolves to ${target}`;
};

/** Rule 4: Drizzle is a storage detail, so it stops at the store. */
const DRIZZLE_SPECIFIER = /^drizzle-orm(?:\/|$)/;

const drizzleLocation: RuleCheck = (file, specifier) => {
  if (!DRIZZLE_SPECIFIER.test(specifier) || isUnderPrefix(file, "src/shared/store")) {
    return null;
  }
  return `drizzle-orm may be imported only under src/shared/store/**, but this file is outside it`;
};

function rootAllowList(specifier: string): string | null {
  if (isSharedAlias(specifier)) {
    return null;
  }
  const target = resolveSpecifier(ROOT_FILE, specifier);
  if (target === null) {
    // Packages and node:/bun builtins are not part of the app's module graph; drizzle-orm is
    // already confined by rule 4.
    return specifier.startsWith("#")
      ? `the composition root may use only #shared/*, never the private alias ${specifier}`
      : null;
  }
  // A private alias resolves to a real target, so it has to be refused before the allow-list: a
  // reachable specifier (`#tools/find/handler`) is still not one the root may write.
  if (specifier.startsWith("#")) {
    return `the composition root may use only #shared/*, never the private alias ${specifier}`;
  }
  if (isUnderPrefix(target, "src/indexer") || isToolManifest(target) || isToolHandler(target)) {
    return null;
  }
  if (isUnderPrefix(target, "src/tools")) {
    return `the composition root may import a tool's manifest or handler only, but ${specifier} resolves to ${target}`;
  }
  return `the composition root may import only #shared/*, ./indexer/* and ./tools/*/{manifest,handler}, but ${specifier} resolves to ${target}`;
}

/** Rule 5, both halves: the root's privileges, and everyone else's exclusion. */
const rootPrivileges: RuleCheck = (file, specifier) => {
  if (file === ROOT_FILE) {
    return rootAllowList(specifier);
  }
  // Two callers are not "another module" in the sense of this rule. `src/indexer/**` is its own
  // tree with its own isolation rule (rule 2), so one indexer file importing a sibling never
  // crosses the root; and `tests/**` is the harness that has to exercise the indexer, never a
  // module the composition root or a tool can reach at runtime.
  if (isUnderPrefix(file, "src/indexer") || isUnderPrefix(file, "tests")) {
    return null;
  }
  const target = resolveSpecifier(file, specifier);
  if (target === null) {
    return null;
  }
  if (isUnderPrefix(target, "src/indexer")) {
    // `src/daemon/**` is the second process that legitimately drives the indexer, so it is exempt
    // from this half the way `src/indexer/**` and `tests/**` are; what it may reach is rule 6's
    // business. The tool-handler half below still applies to it.
    if (isUnderPrefix(file, "src/daemon")) {
      return null;
    }
    return `only ${ROOT_FILE} (the composition root) may import src/indexer/**, but ${specifier} resolves to ${target}`;
  }
  if (isToolHandler(target)) {
    return `only ${ROOT_FILE} (the composition root) may import a tool handler, but ${specifier} resolves to ${target}`;
  }
  return null;
};

/**
 * Rule 6: the daemon serves every registered repo in one process, so it may hold the store's
 * kernel and drive the indexer — but it is a reader, never a tool: a handler is the composition
 * root's to call. The daemon also gets no private alias beyond `#shared`, so `package.json` keeps
 * exactly one alias to audit.
 */
const daemonPrivileges: RuleCheck = (file, specifier) => {
  if (!isUnderPrefix(file, "src/daemon") || isSharedAlias(specifier)) {
    return null;
  }
  const target = resolveSpecifier(file, specifier);
  if (target === null) {
    return specifier.startsWith("#")
      ? `the daemon may use only #shared/*, never the private alias ${specifier}`
      : null;
  }
  if (specifier.startsWith("#")) {
    return `the daemon may use only #shared/*, never the private alias ${specifier}`;
  }
  if (isUnderPrefix(target, "src/daemon") || isUnderPrefix(target, "src/indexer")) {
    return null;
  }
  return `the daemon may import only #shared/*, src/indexer/** and its own ./ siblings, but ${specifier} resolves to ${target}`;
};

const RULES: ReadonlyArray<{ id: RuleId; check: RuleCheck }> = [
  { id: "tool-isolation", check: toolIsolation },
  { id: "indexer-isolation", check: indexerIsolation },
  { id: "shared-isolation", check: sharedIsolation },
  { id: "drizzle-location", check: drizzleLocation },
  { id: "root-privileges", check: rootPrivileges },
  { id: "daemon-privileges", check: daemonPrivileges },
];

export function evaluateImport(file: string, specifier: string): Violation[] {
  const violations: Violation[] = [];
  for (const rule of RULES) {
    const reason = rule.check(file, specifier);
    if (reason !== null) {
      violations.push({
        rule: rule.id,
        file,
        specifier,
        message: `${file} imports "${specifier}" — ${reason} [${rule.id}: ${RULE_TEXT[rule.id]}]`,
      });
    }
  }
  return violations;
}

const transpiler = new Bun.Transpiler({ loader: "ts" });

/**
 * `import type { X } from "y"` is erased by the transpiler's TS transform, so `scan()` alone
 * cannot see it — and the drizzle rule is exactly about the type-only imports of row types.
 * This pattern only ever yields specifiers that appear literally after `from "…"` in the file.
 */
const TYPE_ONLY_SPECIFIER =
  /^[ \t]*(?:import|export)[ \t]+type[ \t]+([^;]*?)[ \t]*from[ \t]*["']([^"'\n]+)["']/gm;

export function importedSpecifiers(source: string): string[] {
  const found = new Set<string>(transpiler.scan(source).imports.map((entry) => entry.path));
  for (const match of source.matchAll(TYPE_ONLY_SPECIFIER)) {
    const specifier = match[2];
    if (specifier !== undefined) {
      found.add(specifier);
    }
  }
  return [...found];
}

export function evaluateSource(file: string, source: string): Violation[] {
  return importedSpecifiers(source).flatMap((specifier) => evaluateImport(file, specifier));
}

/** `tests/fixtures` holds sample sources as data, not modules of the app. */
const SKIPPED_DIRS = new Set(["node_modules", "fixtures"]);

function listSources(dir: string, prefix: string): string[] {
  const files: string[] = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (SKIPPED_DIRS.has(entry.name)) {
      continue;
    }
    const relPath = `${prefix}/${entry.name}`;
    if (entry.isDirectory()) {
      files.push(...listSources(join(dir, entry.name), relPath));
    } else if (entry.name.endsWith(".ts")) {
      files.push(relPath);
    }
  }
  return files;
}

function walkTree(): Map<string, string> {
  const sources = new Map<string, string>();
  for (const root of ["src", "tests"]) {
    for (const file of listSources(join(APP_ROOT, root), root).sort()) {
      sources.set(file, readFileSync(join(APP_ROOT, file), "utf8"));
    }
  }
  return sources;
}

const tree = walkTree();

describe("module boundaries on the real tree", () => {
  test("src and tests are actually scanned", () => {
    expect(tree.size).toBeGreaterThan(5);
    const specifiers = [...tree.values()].flatMap(importedSpecifiers);
    expect(specifiers.length).toBeGreaterThan(0);
  });

  test("no file violates the six boundary rules", () => {
    const violations = [...tree].flatMap(([file, source]) => evaluateSource(file, source));
    expect(violations.map((violation) => violation.message)).toEqual([]);
  });

  test("no src/daemon file breaks the daemon boundary rule", () => {
    const violations = [...tree].flatMap(([file, source]) =>
      evaluateSource(file, source).filter((violation) => violation.rule === "daemon-privileges"),
    );
    expect(violations.map((violation) => violation.message)).toEqual([]);
  });
});

describe("rule self-check: synthetic imports must be rejected", () => {
  test("a tool importing another tool is rejected (rule 1)", () => {
    const violations = evaluateImport("src/tools/find/handler.ts", "../search/handler");
    expect(violations.map((violation) => violation.rule)).toContain("tool-isolation");
    const [first] = violations;
    expect(first?.message).toContain("src/tools/find/handler.ts");
    expect(first?.message).toContain("../search/handler");
    expect(first?.message).toContain("tool-isolation");
  });

  test("a tool importing a tool through a private alias is rejected (rule 1)", () => {
    expect(
      evaluateImport("src/tools/find/handler.ts", "#tools/search/handler").map(
        (violation) => violation.rule,
      ),
    ).toContain("tool-isolation");
  });

  test("a non-root file importing the indexer is rejected (rule 5)", () => {
    const cases: ReadonlyArray<readonly [string, string]> = [
      ["src/tools/find/handler.ts", "../../indexer/pipeline"],
      ["src/shared/store/db.ts", "../../indexer/pipeline"],
      ["src/shared/log.ts", "../indexer/pipeline"],
      ["src/tools/find/manifest.ts", "#indexer/pipeline"],
    ];
    for (const [file, specifier] of cases) {
      const violations = evaluateImport(file, specifier);
      expect(violations.map((violation) => violation.rule)).toContain("root-privileges");
    }
  });

  test("intra-indexer and test harness imports stay inside the boundary (rule 5)", () => {
    const cases: ReadonlyArray<readonly [string, string]> = [
      ["src/indexer/lang/registry.ts", "./types"],
      ["src/indexer/parse.ts", "./lang/registry"],
      ["tests/lang.test.ts", "../src/indexer/lang/registry"],
    ];
    for (const [file, specifier] of cases) {
      expect(evaluateImport(file, specifier).map((violation) => violation.rule)).not.toContain(
        "root-privileges",
      );
    }
    // The exemption is scoped: an indexer file still may not reach the tools or the store's
    // outsiders, which rule 2 keeps enforcing.
    expect(
      evaluateImport("src/indexer/lang/registry.ts", "../../tools/find/handler").map(
        (violation) => violation.rule,
      ),
    ).toContain("indexer-isolation");
  });

  test("shared never reaches into the tools or the indexer (rule 3)", () => {
    const cases: ReadonlyArray<readonly [string, string]> = [
      ["src/shared/log.ts", "../tools/find/handler"],
      ["src/shared/store/db.ts", "../../tools/find/handler"],
      ["src/shared/store/db.ts", "#tools/find/handler"],
      ["src/shared/store/db.ts", "../../indexer/pipeline"],
      ["src/shared/log.ts", "#indexer/pipeline"],
    ];
    for (const [file, specifier] of cases) {
      expect(evaluateImport(file, specifier).map((violation) => violation.rule)).toContain(
        "shared-isolation",
      );
    }
  });

  test("drizzle-orm outside src/shared/store is rejected (rule 4)", () => {
    for (const specifier of ["drizzle-orm", "drizzle-orm/sqlite-core"]) {
      expect(
        evaluateImport("src/shared/log.ts", specifier).map((violation) => violation.rule),
      ).toContain("drizzle-location");
    }
    expect(evaluateImport("src/shared/store/schema.ts", "drizzle-orm/sqlite-core")).toEqual([]);
  });

  test("a type-only drizzle import outside the store is still caught", () => {
    const source = 'import type { InferSelectModel } from "drizzle-orm";';
    // The transpiler erases it, which is why the extractor also reads type-only imports.
    expect(new Bun.Transpiler({ loader: "ts" }).scan(source).imports).toEqual([]);
    expect(
      evaluateSource("src/shared/log.ts", source).map((violation) => violation.rule),
    ).toContain("drizzle-location");
  });

  test("the composition root has an allow-list, not a free hand (rule 5)", () => {
    for (const specifier of ["#shared/log", "./indexer/pipeline", "./tools/find/manifest"]) {
      expect(evaluateImport("src/index.ts", specifier)).toEqual([]);
    }
    // A tool module's two exports *are* its surface to the root: the manifest it registers and the
    // handler it calls. Anything else under a tool stays unreachable from the root, and the rule
    // below still forbids the manifest itself from reaching across to the handler.
    expect(evaluateImport("src/index.ts", "./tools/find/handler")).toEqual([]);
    for (const specifier of ["./tools/find/helper", "./shared/log", "#tools/find/handler"]) {
      expect(
        evaluateImport("src/index.ts", specifier).map((violation) => violation.rule),
      ).toContain("root-privileges");
    }
  });

  test("the daemon may drive the indexer and the kernel, never a tool (rule 6)", () => {
    // The daemon owns the poller for every registered repo, so the indexer is reachable from it
    // without rule 5 complaining; `#shared/*` is the rest of its surface.
    expect(evaluateImport("src/daemon/runtime.ts", "#shared/log")).toEqual([]);
    expect(evaluateImport("src/daemon/runtime.ts", "../indexer/watch")).toEqual([]);

    const violations = evaluateImport("src/daemon/runtime.ts", "../tools/trace/handler");
    expect(violations.map((violation) => violation.rule)).toContain("daemon-privileges");
    expect(violations.map((violation) => violation.message).join("\n")).toContain(
      "src/tools/trace/handler",
    );

    // The rule is scoped to the daemon: the same import anywhere else is not its business.
    expect(
      evaluateImport("src/shared/log.ts", "../tools/trace/handler").map(
        (violation) => violation.rule,
      ),
    ).not.toContain("daemon-privileges");
  });

  test("scan plus the rules reject a synthetic violating source end to end", () => {
    const violating = [
      'import { reader } from "#shared/store/read/reader";',
      'import { search } from "../search/handler.ts";',
    ].join("\n");
    const violations = importedSpecifiers(violating).flatMap((specifier) =>
      evaluateImport("src/tools/find/handler.ts", specifier),
    );
    const rules = violations.map((violation) => violation.rule);
    expect(rules).toContain("tool-isolation");
    expect(rules).toContain("root-privileges");

    const legal = [
      'import { reader } from "#shared/store/read/reader";',
      'import { helper } from "./helper";',
    ].join("\n");
    expect(evaluateSource("src/tools/find/handler.ts", legal)).toEqual([]);
  });
});
