import { describe, expect, spyOn, test } from "bun:test";
import { join } from "node:path";
import {
  INSTRUCTIONS_TEXT,
  nextToolsFor,
  printInstructions,
  TOOL_NAMES,
} from "#shared/instructions";

/**
 * The guide and its constant are two copies of one deliverable: one is read by a human, one is
 * imported by the composition root. A silent edit to either is the whole failure mode this file
 * exists to catch, so the doc is compared as text rather than checked for keywords alone.
 */
const DOC_PATH = join(import.meta.dir, "..", "docs", "INSTRUCTIONS.md");

/** A tool is named only where it is written as code; prose mentions are not a reference. */
function namesTool(text: string, tool: string): boolean {
  return text.includes(`\`${tool}\``);
}

describe("instructions: the guide names the surface", () => {
  test("the closing paragraph names all seven tools", () => {
    const start = INSTRUCTIONS_TEXT.indexOf("The surface is seven tools");
    expect(start).toBeGreaterThan(-1);
    const closing = INSTRUCTIONS_TEXT.slice(start);

    const missing = TOOL_NAMES.filter((tool) => !namesTool(closing, tool));
    expect(missing).toEqual([]);
  });

  test("index is described as a repair, never as a query", () => {
    expect(namesTool(INSTRUCTIONS_TEXT, "index")).toBe(true);
    // The paragraph is wrapped, so the wording is checked with newlines collapsed.
    const prose = INSTRUCTIONS_TEXT.replace(/\s+/gu, " ");
    expect(prose).toContain("only when `status` says the graph is broken");
    expect(prose).toContain("never as a query");
    expect(prose).toContain("a rebuild is a repair");
  });

  test("the guide takes `repo` and no longer claims a bound repository", () => {
    expect(namesTool(INSTRUCTIONS_TEXT, "repo")).toBe(true);
    // Claims this task retires: the surface now serves the whole registry.
    expect(INSTRUCTIONS_TEXT).not.toContain("bound at startup");
    expect(INSTRUCTIONS_TEXT).not.toContain("Do not pass a project argument");
    // Who watches the graph is a real fact an agent must not infer from this process.
    expect(INSTRUCTIONS_TEXT).toContain("daemon");
    expect(INSTRUCTIONS_TEXT).not.toContain("server already knows");
  });

  test("the three anti-patterns are each stated", () => {
    const antiPatterns: readonly (readonly [string, RegExp])[] = [
      ["do not read total_relation gte as none", /do not read `total_relation: gte` as none/i],
      ["do not use trace to read source", /do not use `trace` to read source/i],
      [
        "do not expect repo to work for an unregistered path",
        /do not expect `repo` to work for an unregistered path/i,
      ],
    ];
    const unstated = antiPatterns
      .filter(([, pattern]) => !pattern.test(INSTRUCTIONS_TEXT))
      .map(([name]) => name);
    expect(unstated).toEqual([]);
    expect(INSTRUCTIONS_TEXT.match(/^- \*\*Do not/gmu) ?? []).toHaveLength(3);
  });

  test("the rules appear in the order an agent needs them", () => {
    const headings = [
      "## 1. Start a task with",
      "## 2. Confine work to an area with",
      "## 3. Look a symbol up with",
      "## 4. Call",
      "## 5. Reach for",
      "## Do not",
    ];
    const positions = headings.map((heading) => INSTRUCTIONS_TEXT.indexOf(heading));
    for (const position of positions) {
      expect(position).toBeGreaterThan(-1);
    }
    expect(positions).toEqual([...positions].sort((a, b) => a - b));
  });
});

describe("instructions: nextToolsFor routes within the surface", () => {
  test("all seven tools suggest only names in TOOL_NAMES, and never themselves", () => {
    const surfaceNames = new Set<string>(TOOL_NAMES);
    for (const tool of TOOL_NAMES) {
      const next = nextToolsFor(tool);
      expect(next.length).toBeGreaterThan(0);
      // A name outside the surface would be a promise the tool cannot keep.
      expect(next.filter((suggestion) => !surfaceNames.has(suggestion))).toEqual([]);
      expect(next).not.toContain(tool);
    }
  });

  test("index routes to status, the follow-up after a repair", () => {
    expect(nextToolsFor("index")).toEqual(["status"]);
  });

  test("a tool outside the surface gets no suggestion rather than a guess", () => {
    for (const unknown of ["", "index_repository", "Status", "status "]) {
      expect(nextToolsFor(unknown)).toEqual([]);
    }
  });

  test("every call returns a fresh array, so one envelope cannot mutate the table", () => {
    const first = nextToolsFor("summary");
    first.push("index");
    expect(nextToolsFor("summary")).toEqual(["impact", "trace", "expand"]);
  });
});

describe("instructions: one deliverable, two copies", () => {
  test("docs/INSTRUCTIONS.md is the same text as INSTRUCTIONS_TEXT", async () => {
    const doc = (await Bun.file(DOC_PATH).text()).trimEnd();
    expect(doc).toBe(INSTRUCTIONS_TEXT);
  });

  test("the guide stays one screen", () => {
    expect(INSTRUCTIONS_TEXT.split("\n").length).toBeLessThanOrEqual(70);
  });

  test("printInstructions writes the whole guide to stdout once, and nothing else", () => {
    const chunks: string[] = [];
    const spy = spyOn(process.stdout, "write").mockImplementation((chunk) => {
      chunks.push(String(chunk));
      return true;
    });
    try {
      printInstructions();
    } finally {
      spy.mockRestore();
    }
    expect(chunks).toEqual([`${INSTRUCTIONS_TEXT}\n`]);
  });
});
