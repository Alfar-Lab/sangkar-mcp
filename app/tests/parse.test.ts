import { describe, expect, test } from "bun:test";
import { LANGS } from "../src/indexer/lang/registry";
import type { LangId } from "../src/indexer/lang/types";
import {
  disposeLang,
  MAX_CACHED_LANGUAGES,
  MAX_ERROR_RANGES,
  type ParseResult,
  type ParseSuccess,
  parseFile,
  parserPoolStats,
} from "../src/indexer/parse";

/**
 * The parser pool's contract, exercised against the grammars `tree-sitter-wasms` installed into
 * `node_modules`: a clean file is a tree and no error ranges, a missing grammar is a result rather
 * than a throw, and the six-language LRU plus the one-parse-at-a-time rule hold. Nothing here reaches
 * the network.
 */

const PYTHON = `import os
from pathlib import Path

class Widget(Base):
    def render(self, name: str) -> str:
        return str(Path(name) / "index.html")

def build(count: int) -> list[str]:
    return [str(i) for i in range(count)]
`;

const TYPESCRIPT = `import { join } from "node:path";

export interface Shape {
  area(): number;
}

export class Box implements Shape {
  readonly side: number;

  constructor(side: number) {
    this.side = side;
  }

  area(): number {
    return this.side * this.side;
  }
}

export function label(shape: Shape): string {
  return join("box", String(shape.area()));
}
`;

const GO = `package widget

type Box struct {
	Side int
}

func (b Box) Area() int {
	return b.Side * b.Side
}
`;

/** The success branch, with a failure that names the real reason instead of a bare assertion. */
function parsed(result: ParseResult): ParseSuccess {
  if (!result.ok) {
    throw new Error(`expected a tree, got ${result.reason}: ${result.detail}`);
  }
  return result;
}

describe("parseFile", () => {
  test("a Python snippet parses with no error ranges", async () => {
    const { tree, errorRanges } = parsed(await parseFile("pkg/widget.py", PYTHON, "python"));
    expect(tree.rootNode.type).toBe("module");
    expect(errorRanges).toEqual([]);
  });

  test("a TypeScript snippet parses with no error ranges", async () => {
    const { tree, errorRanges } = parsed(await parseFile("src/box.ts", TYPESCRIPT, "typescript"));
    expect(tree.rootNode.type).toBe("program");
    expect(errorRanges).toEqual([]);
  });

  test("a reused parser starts at the beginning of the next file", async () => {
    const first = parsed(await parseFile("pkg/first.py", "first_name = 1\n", "python"));
    const second = parsed(await parseFile("pkg/second.py", "second_name = 2\n", "python"));
    expect(first.tree.rootNode.text).toContain("first_name");
    expect(second.tree.rootNode.text).toContain("second_name");
    expect(second.tree.rootNode.text).not.toContain("first_name");
  });

  test("an unregistered language is grammar_unavailable, not a throw", async () => {
    const result = await parseFile(
      "src/widget.cobol",
      "DISPLAY 'hi'.",
      "cobol" as unknown as LangId,
    );
    expect(result).toMatchObject({ ok: false, reason: "grammar_unavailable" });
    if (!result.ok) {
      expect(result.detail).toContain("cobol");
    }
  });

  test("error ranges are 1-based lines, comma-joined and capped with a count suffix", async () => {
    const single = parsed(await parseFile("pkg/single.py", "result = compute(\n", "python"));
    expect(single.errorRanges).toEqual(["1"]);

    const spanning = parsed(await parseFile("pkg/spanning.py", "items = [1, 2,\n3\n", "python"));
    expect(spanning.errorRanges).toEqual(["1-2"]);

    const many = Array.from({ length: 40 }, (_, index) => `def f${index}(:`).join("\n");
    const capped = parsed(await parseFile("pkg/many.py", many, "python"));
    expect(capped.errorRanges).toHaveLength(MAX_ERROR_RANGES + 1);
    for (const range of capped.errorRanges.slice(0, MAX_ERROR_RANGES)) {
      expect(range).toMatch(/^\d+(?:-\d+)?$/);
    }
    expect(capped.errorRanges[0]).toBe("1");
    expect(capped.errorRanges[MAX_ERROR_RANGES]).toMatch(/^\+\d+ more ranges$/);
  });

  test("concurrent requests are serialised and never share a parser", async () => {
    const results = await Promise.all([
      parseFile("pkg/a.py", PYTHON, "python"),
      parseFile("pkg/a.py", PYTHON, "python"),
      parseFile("src/box.ts", TYPESCRIPT, "typescript"),
      parseFile("src/box.ts", TYPESCRIPT, "typescript"),
      parseFile("pkg/box.go", GO, "go"),
    ]);
    const trees = results.map((result) => parsed(result).tree);
    expect(new Set(trees).size).toBe(trees.length);
    // The peak is the whole point: without the queue this would be the number of requests.
    expect(parserPoolStats().peakInFlight).toBe(1);
    expect(parserPoolStats().inFlight).toBe(0);
  });

  test("the pool keeps at most six languages and evicts the least recently used", async () => {
    for (const id of Object.keys(LANGS) as LangId[]) {
      await disposeLang(id);
    }
    const visited: LangId[] = ["python", "typescript", "javascript", "go", "java", "ruby", "php"];
    for (const id of visited) {
      parsed(await parseFile(`probe-${id}`, "", id));
    }
    const { cached } = parserPoolStats();
    expect(cached).toHaveLength(MAX_CACHED_LANGUAGES);
    expect(cached).toEqual(visited.slice(visited.length - MAX_CACHED_LANGUAGES));
  });

  test("disposeLang drops a cached parser and the language still parses afterwards", async () => {
    parsed(await parseFile("pkg/dispose.py", PYTHON, "python"));
    expect(parserPoolStats().cached).toContain("python");
    await disposeLang("python");
    expect(parserPoolStats().cached).not.toContain("python");
    const again = parsed(await parseFile("pkg/dispose.py", PYTHON, "python"));
    expect(again.errorRanges).toEqual([]);
  });
});
