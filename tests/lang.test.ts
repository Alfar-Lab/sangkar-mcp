import { beforeAll, describe, expect, test } from "bun:test";
import { join } from "node:path";
import { Language, type Node, Parser } from "web-tree-sitter";
import { detectLanguage, EXTENSION_MAP, LANGS, specFor } from "../src/indexer/lang/registry";
import type { LangId, LangRow } from "../src/indexer/lang/types";

/**
 * Provenance for the language table: what `registry.ts` claims is checked against the grammar that
 * `tree-sitter-wasms` actually ships, not against a node-types listing read while writing the table.
 *
 * Two probes per language. First, every declared node type must exist as a named type in the loaded
 * grammar. Second, one snippet per language is parsed and must contain at least one node from every
 * non-empty list — that is what makes the four lists a claim about code rather than a vocabulary.
 * Grammars load once in `beforeAll` from `node_modules`; nothing here reaches the network.
 */

const APP_ROOT = join(import.meta.dir, "..");
const GRAMMAR_DIR = join(APP_ROOT, "node_modules", "tree-sitter-wasms", "out");
const RUNTIME_WASM = join(APP_ROOT, "node_modules", "web-tree-sitter", "web-tree-sitter.wasm");

const MVP_IDS: readonly LangId[] = [
  "python",
  "typescript",
  "tsx",
  "javascript",
  "go",
  "java",
  "rust",
  "c",
  "cpp",
  "csharp",
  "php",
  "ruby",
];

/** Extensions the MVP promises to index, independent of how the table is written. */
const REQUIRED_EXTENSIONS: readonly (readonly [string, LangId])[] = [
  [".py", "python"],
  [".ts", "typescript"],
  [".tsx", "tsx"],
  [".js", "javascript"],
  [".jsx", "javascript"],
  [".mjs", "javascript"],
  [".cjs", "javascript"],
  [".go", "go"],
  [".java", "java"],
  [".rs", "rust"],
  [".c", "c"],
  [".h", "c"],
  [".cpp", "cpp"],
  [".cc", "cpp"],
  [".hpp", "cpp"],
  [".cs", "csharp"],
  [".php", "php"],
  [".rb", "ruby"],
];

interface Probe {
  /** One file per language, exercising a type from each of the five lists. */
  readonly snippet: string;
  /** A repo-relative path the row's `testFileRegex` must match, and a source path it must not. */
  readonly testFile: string;
  readonly sourceFile: string;
}

const PROBES: Readonly<Record<LangId, Probe>> = {
  python: {
    snippet: [
      "import os",
      "import sys as system",
      "",
      "class Base:",
      "    pass",
      "",
      "class Widget(Base):",
      "    def render(self):",
      '        return os.path.join("a", "b")',
      "",
    ].join("\n"),
    testFile: "tests/test_widget.py",
    sourceFile: "src/widget.py",
  },
  typescript: {
    snippet: [
      'import { helper } from "./helper";',
      "",
      "interface Shape {",
      "  area(): number;",
      "}",
      "",
      "class Base {}",
      "",
      "abstract class Widget extends Base implements Shape {",
      "  abstract paint(): void;",
      "  area(): number {",
      "    return helper();",
      "  }",
      "}",
      "",
      "const make = (): Widget => new Widget();",
      "",
    ].join("\n"),
    testFile: "src/widget.test.ts",
    sourceFile: "src/widget.ts",
  },
  tsx: {
    snippet: [
      'import { helper } from "./helper";',
      "",
      "interface Props {",
      "  name: string;",
      "}",
      "",
      "class Base {}",
      "",
      "class Widget extends Base implements Props {",
      "  render() {",
      "    return <div onClick={() => helper()}>{this.name}</div>;",
      "  }",
      "}",
      "",
    ].join("\n"),
    testFile: "src/widget.test.tsx",
    sourceFile: "src/widget.tsx",
  },
  javascript: {
    snippet: [
      'import { helper } from "./helper.js";',
      "",
      "class Base {}",
      "",
      "class Widget extends Base {",
      "  run() {",
      "    helper();",
      "    return new Base();",
      "  }",
      "}",
      "",
    ].join("\n"),
    testFile: "src/widget.spec.js",
    sourceFile: "src/widget.js",
  },
  go: {
    snippet: [
      "package main",
      "",
      'import "fmt"',
      "",
      "type Base struct{}",
      "",
      "type Widget struct {",
      "\tBase",
      "}",
      "",
      "type Shape interface {",
      "\tArea() float64",
      "}",
      "",
      "func (w Widget) Area() float64 {",
      '\tfmt.Println("area")',
      "\treturn 1",
      "}",
      "",
      "func main() {",
      "\tWidget{}.Area()",
      "}",
      "",
    ].join("\n"),
    testFile: "internal/widget_test.go",
    sourceFile: "internal/widget.go",
  },
  java: {
    snippet: [
      "import java.util.List;",
      "",
      "interface Shape {",
      "  double area();",
      "}",
      "",
      "class Base {}",
      "",
      "class Widget extends Base implements Shape {",
      "  public double area() {",
      "    return size();",
      "  }",
      "",
      "  int size() {",
      "    return 1;",
      "  }",
      "}",
      "",
    ].join("\n"),
    testFile: "src/test/java/AppWidgetTest.java",
    sourceFile: "src/main/java/AppWidget.java",
  },
  rust: {
    snippet: [
      "use std::fmt;",
      "",
      "struct Base;",
      "",
      "trait Shape: fmt::Debug {",
      "    fn area(&self) -> f64;",
      "}",
      "",
      "impl Shape for Base {",
      "    fn area(&self) -> f64 {",
      "        helper()",
      "    }",
      "}",
      "",
      "fn helper() -> f64 {",
      "    1.0",
      "}",
      "",
    ].join("\n"),
    testFile: "tests/widget.rs",
    sourceFile: "src/widget.rs",
  },
  c: {
    snippet: [
      "#include <stdio.h>",
      "",
      "struct point {",
      "  int x;",
      "};",
      "",
      "int area(struct point p) {",
      "  return compute(p.x);",
      "}",
      "",
    ].join("\n"),
    testFile: "tests/widget.c",
    sourceFile: "src/widget.c",
  },
  cpp: {
    snippet: [
      "#include <string>",
      "",
      "struct Base {};",
      "",
      "class Widget : public Base {",
      " public:",
      "  int area() {",
      "    return compute();",
      "  }",
      "};",
      "",
    ].join("\n"),
    testFile: "src/widget_test.cpp",
    sourceFile: "src/widget.cpp",
  },
  csharp: {
    snippet: [
      "using System;",
      "",
      "interface IShape {",
      "  double Area();",
      "}",
      "",
      "class Base {}",
      "",
      "class Widget : Base, IShape {",
      "  public double Area() {",
      "    return Helper();",
      "  }",
      "",
      "  double Helper() {",
      "    return 1;",
      "  }",
      "}",
      "",
    ].join("\n"),
    testFile: "src/Widget.Tests/WidgetTest.cs",
    sourceFile: "src/Widget.cs",
  },
  php: {
    snippet: [
      "<?php",
      "",
      "namespace App;",
      "",
      "use Other\\Thing;",
      "",
      "interface Shape {}",
      "",
      "class Base {}",
      "",
      "class Widget extends Base implements Shape {",
      "  public function area() {",
      "    return $this->size();",
      "  }",
      "",
      "  public function size() {",
      "    return 1;",
      "  }",
      "}",
      "",
    ].join("\n"),
    testFile: "tests/WidgetTest.php",
    sourceFile: "src/Widget.php",
  },
  ruby: {
    snippet: [
      'require "json"',
      "",
      "class Base",
      "end",
      "",
      "class Widget < Base",
      "  def area",
      "    size",
      "  end",
      "",
      "  def size",
      "    1",
      "  end",
      "end",
      "",
    ].join("\n"),
    testFile: "spec/widget_spec.rb",
    sourceFile: "lib/widget.rb",
  },
};

interface Grammar {
  readonly language: Language;
  readonly namedTypes: ReadonlySet<string>;
}

const grammars = new Map<LangId, Grammar>();

function grammarFor(id: LangId): Grammar {
  const grammar = grammars.get(id);
  if (grammar === undefined) {
    throw new Error(`grammar for ${id} was not loaded`);
  }
  return grammar;
}

/** `noUncheckedIndexedAccess` makes every byte a possible undefined; a wasm reader wants a failure. */
function readByte(bytes: Uint8Array, offset: number): number {
  const byte = bytes[offset];
  if (byte === undefined) {
    throw new Error(`truncated grammar wasm at byte ${offset}`);
  }
  return byte;
}

interface Cursor {
  offset: number;
}

function readLeb(bytes: Uint8Array, cursor: Cursor): number {
  let value = 0;
  let shift = 0;
  for (;;) {
    const byte = readByte(bytes, cursor.offset);
    cursor.offset += 1;
    value |= (byte & 0x7f) << shift;
    shift += 7;
    if ((byte & 0x80) === 0) {
      return value;
    }
  }
}

function encodeLeb(value: number): number[] {
  const encoded: number[] = [];
  let rest = value;
  do {
    const byte = rest & 0x7f;
    rest >>>= 7;
    encoded.push(rest === 0 ? byte : byte | 0x80);
  } while (rest !== 0);
  return encoded;
}

/**
 * `tree-sitter-wasms@0.1.13` grammars carry the Emscripten custom section as it was before
 * `dylink.0`: a bare `dylink` payload of memory/table numbers with no subsection headers, which the
 * Emscripten runtime inside `web-tree-sitter@0.27` refuses. Only that section header is rewritten;
 * the memory and table sizes are carried over verbatim, and any other shape throws instead of being
 * loaded on a guess. Nothing else in the module changes, so the probe still runs the shipped
 * grammar.
 */
function upgradeLegacyDylink(wasm: Uint8Array): Uint8Array {
  const cursor: Cursor = { offset: 8 }; // wasm magic (4 bytes) + version (4 bytes)
  if (readByte(wasm, cursor.offset) !== 0) {
    throw new Error("grammar wasm does not start with a custom section");
  }
  cursor.offset += 1;
  const sectionSize = readLeb(wasm, cursor);
  const sectionEnd = cursor.offset + sectionSize;
  const nameLength = readLeb(wasm, cursor);
  const sectionName = new TextDecoder().decode(
    wasm.subarray(cursor.offset, cursor.offset + nameLength),
  );
  cursor.offset += nameLength;
  if (sectionName !== "dylink") {
    throw new Error(`unexpected grammar wasm section ${sectionName}`);
  }
  const memorySize = readLeb(wasm, cursor);
  const memoryAlign = readLeb(wasm, cursor);
  const tableSize = readLeb(wasm, cursor);
  const tableAlign = readLeb(wasm, cursor);
  for (let offset = cursor.offset; offset < sectionEnd; offset += 1) {
    // The trailing field is a zero needed-dynlibs count. Anything else is metadata this rewrite
    // would drop, so it is refused rather than ignored.
    if (readByte(wasm, offset) !== 0) {
      throw new Error("grammar wasm carries dylink metadata this probe does not understand");
    }
  }
  const memoryInfo = [
    ...encodeLeb(memorySize),
    ...encodeLeb(memoryAlign),
    ...encodeLeb(tableSize),
    ...encodeLeb(tableAlign),
  ];
  const subsection = [1, ...encodeLeb(memoryInfo.length), ...memoryInfo]; // WASM_DYLINK_MEM_INFO
  const sectionNameBytes = new TextEncoder().encode("dylink.0");
  const header = [...encodeLeb(sectionNameBytes.length), ...sectionNameBytes, ...subsection];
  const section = [0, ...encodeLeb(header.length), ...header];
  const upgraded = new Uint8Array(8 + section.length + (wasm.length - sectionEnd));
  upgraded.set(wasm.subarray(0, 8), 0);
  upgraded.set(section, 8);
  upgraded.set(wasm.subarray(sectionEnd), 8 + section.length);
  return upgraded;
}

function collectNodeTypes(root: Node): ReadonlySet<string> {
  const seen = new Set<string>();
  const visit = (node: Node): void => {
    seen.add(node.type);
    for (const child of node.namedChildren) {
      visit(child);
    }
  };
  visit(root);
  return seen;
}

beforeAll(async () => {
  await Parser.init({ locateFile: () => RUNTIME_WASM });
  for (const id of Object.keys(LANGS) as LangId[]) {
    const row = LANGS[id];
    const wasm = new Uint8Array(await Bun.file(join(GRAMMAR_DIR, row.grammarWasm)).arrayBuffer());
    const language = await Language.load(upgradeLegacyDylink(wasm));
    const namedTypes = new Set<string>();
    language.types.forEach((type, index) => {
      if (language.nodeTypeIsNamed(index)) {
        namedTypes.add(type);
      }
    });
    grammars.set(id, { language, namedTypes });
  }
}, 120_000);

function listsOf(row: LangRow): readonly (readonly [string, readonly string[]])[] {
  return [
    ["classTypes", row.classTypes],
    ["functionTypes", row.functionTypes],
    ["importTypes", row.importTypes],
    ["callTypes", row.callTypes],
    ["inheritanceTypes", row.inheritanceTypes],
  ];
}

describe("extensions", () => {
  test("the MVP extensions resolve", () => {
    for (const [extension, id] of REQUIRED_EXTENSIONS) {
      expect({ extension, language: EXTENSION_MAP[extension] }).toEqual({
        extension,
        language: id,
      });
    }
  });

  test(".h is C while .cpp is C++", () => {
    expect(detectLanguage("include/widget.h")).toBe("c");
    expect(detectLanguage("src/widget.cpp")).toBe("cpp");
    expect(detectLanguage("src/widget.hpp")).toBe("cpp");
  });

  test("detectLanguage is extension-only and case-insensitive", () => {
    expect(detectLanguage("pkg/WIDGET.PY")).toBe("python");
    expect(detectLanguage("src/nested/deep/widget.ts")).toBe("typescript");
    expect(detectLanguage("Makefile")).toBeNull();
    expect(detectLanguage("src/widget")).toBeNull();
    expect(detectLanguage(".py")).toBeNull();
    expect(detectLanguage("src/widget.kt")).toBeNull();
  });

  test("every declared extension maps back to its own language and nothing else is mapped", () => {
    const declared: string[] = [];
    for (const id of MVP_IDS) {
      for (const extension of LANGS[id].extensions) {
        declared.push(extension);
        expect({ extension, language: EXTENSION_MAP[extension] }).toEqual({
          extension,
          language: id,
        });
      }
    }
    expect([...Object.keys(EXTENSION_MAP)].sort()).toEqual([...declared].sort());
  });

  test("the extension map is built in LANGS declaration order", () => {
    const expected: string[] = [];
    for (const id of Object.keys(LANGS) as LangId[]) {
      for (const extension of LANGS[id].extensions) {
        if (!expected.includes(extension)) {
          expected.push(extension);
        }
      }
    }
    expect(Object.keys(EXTENSION_MAP)).toEqual(expected);
  });

  test("LANGS holds exactly the 12 MVP ids and specFor returns the row", () => {
    expect([...(Object.keys(LANGS) as LangId[])].sort()).toEqual([...MVP_IDS].sort());
    for (const id of MVP_IDS) {
      const row = specFor(id);
      expect(row).toBe(LANGS[id]);
      expect(row.id).toBe(id);
    }
  });

  test("testFileRegex separates test files from source files", () => {
    for (const id of MVP_IDS) {
      const regex = LANGS[id].testFileRegex;
      expect({ language: id, hasRegex: regex !== null }).toEqual({
        language: id,
        hasRegex: true,
      });
      if (regex === null) {
        continue;
      }
      const probe = PROBES[id];
      expect({ path: probe.testFile, test: regex.test(probe.testFile) }).toEqual({
        path: probe.testFile,
        test: true,
      });
      expect({ path: probe.sourceFile, test: regex.test(probe.sourceFile) }).toEqual({
        path: probe.sourceFile,
        test: false,
      });
    }
  });
});

describe("node-type tables", () => {
  test("every non-inheritance list is non-empty for all 12 languages", () => {
    const empty: string[] = [];
    for (const id of MVP_IDS) {
      const row = LANGS[id];
      for (const [name, types] of listsOf(row)) {
        if (name !== "inheritanceTypes" && types.length === 0) {
          empty.push(`${id}.${name}`);
        }
      }
    }
    expect(empty).toEqual([]);
  });

  test("inheritance is declared wherever the language has the clause, and C declares none", () => {
    // C is the one language in the MVP with no superclass or interface clause in its grammar, so
    // its empty list is a fact of the table, not a gap to fill with a look-alike node.
    for (const id of MVP_IDS) {
      const declared = LANGS[id].inheritanceTypes.length > 0;
      expect({ language: id, declared }).toEqual({ language: id, declared: id !== "c" });
    }
  });

  test("every declared node type exists as a named type in the loaded grammar", () => {
    const missing: string[] = [];
    for (const id of MVP_IDS) {
      const { namedTypes } = grammarFor(id);
      for (const [name, types] of listsOf(LANGS[id])) {
        for (const type of types) {
          if (!namedTypes.has(type)) {
            missing.push(`${id}.${name}: ${type}`);
          }
        }
      }
    }
    expect(missing).toEqual([]);
  });

  test("one snippet per language parses cleanly and exercises every non-empty list", () => {
    const misses: string[] = [];
    for (const id of MVP_IDS) {
      const row = LANGS[id];
      const parser = new Parser();
      parser.setLanguage(grammarFor(id).language);
      const tree = parser.parse(PROBES[id].snippet);
      if (tree === null) {
        misses.push(`${id}: parser returned no tree`);
        parser.delete();
        continue;
      }
      if (tree.rootNode.hasError) {
        misses.push(`${id}: snippet contains an ERROR node`);
      }
      const observed = collectNodeTypes(tree.rootNode);
      for (const [name, types] of listsOf(row)) {
        if (types.length === 0) {
          continue;
        }
        if (!types.some((type) => observed.has(type))) {
          misses.push(`${id}.${name}: none of [${types.join(", ")}] appears in the snippet`);
        }
      }
      tree.delete();
      parser.delete();
    }
    expect(misses).toEqual([]);
  });
});
