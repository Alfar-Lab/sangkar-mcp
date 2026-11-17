import { beforeAll, describe, expect, test } from "bun:test";
import {
  EXTRACTED_CANDIDATE,
  EXTRACTION_EXCEPTIONS,
  type ExtractedNode,
  type ExtractResult,
  extractFile,
  NODE_KINDS,
  type RawEdge,
  REFERENCE_SITES,
} from "../src/indexer/extract";
import { specFor } from "../src/indexer/lang/registry";
import { initParsers, parseFile } from "../src/indexer/parse";

/**
 * The extractor's contract, exercised on a Python fixture because Python has every shape in one
 * grammar: a class with a base, a decorated method, a top-level function, an import and a from
 * import, and the six value positions that become `references` candidates. The grammars load from
 * `node_modules`; nothing here reaches the network.
 */

const FIXTURE_PATH = "tests/test_widget.py";

const FIXTURE = `import os
from pathlib import Path


class Base:
    def label(self):
        return "base"


class Widget(Base):
    def render(self, title):
        prefix = os.sep
        parts = [prefix, title]
        options = {"title": title, "prefix": prefix}
        self.paint(parts, options)
        return prefix

    @route("/items", handler=render)
    def paint(self, parts, options):
        return Path("index.html")


def build(count):
    prefix = str(count)
    return Widget(prefix)


def test_render():
    widget = Widget("x")
    return widget.render("t")
`;

/** A non-test path, so the `test` kind cannot come from the file name alone. */
const SOURCE_PATH = "src/widget.py";
const SOURCE = `class Widget(Base):
    def render(self, title):
        return title


def test_smoke():
    return Widget(None)
`;

let result: ExtractResult;
let sourceResult: ExtractResult;

beforeAll(async () => {
  await initParsers();
  result = await extract(FIXTURE_PATH, FIXTURE);
  sourceResult = await extract(SOURCE_PATH, SOURCE);
});

async function extract(path: string, source: string): Promise<ExtractResult> {
  const parsed = await parseFile(path, source, "python");
  if (!parsed.ok) {
    throw new Error(`${path} did not parse: ${parsed.detail}`);
  }
  return extractFile(parsed.tree, specFor("python"), path);
}

function nodeWithQn(nodes: readonly ExtractedNode[], qn: string): ExtractedNode {
  const found = nodes.find((node) => node.qn === qn);
  if (found === undefined) {
    throw new Error(`no node with qn ${qn}; got ${nodes.map((node) => node.qn).join(", ")}`);
  }
  return found;
}

function edgesOfKind(edges: readonly RawEdge[], kind: RawEdge["kind"]): RawEdge[] {
  return edges.filter((edge) => edge.kind === kind);
}

describe("extractFile: nodes", () => {
  test("the file node is first and its qn is the path plus the file name", () => {
    const file = result.nodes[0];
    expect(file).toBeDefined();
    expect(file?.kind).toBe("file");
    expect(file?.qn).toBe("tests/test_widget.py::test_widget.py");
    expect(file?.name).toBe("test_widget.py");
    expect(file?.startLine).toBe(1);
    expect(file?.isTest).toBe(true);
    expect(file?.props).toEqual({});
  });

  test("every node kind the fixture can produce appears exactly once per declaration", () => {
    const kinds = new Set(result.nodes.map((node) => node.kind));
    expect([...kinds].sort()).toEqual(["class", "file", "function", "method", "module", "test"]);
    // `endpoint` is declared by the union and produced by no rule yet: a route needs framework
    // props that no node-type table carries, and inventing one would be a language branch.
    expect(NODE_KINDS).toContain("endpoint");
    expect(kinds.has("endpoint")).toBe(false);
  });

  test("a method's qn is path::Class.method and a top-level function has no scope", () => {
    const render = nodeWithQn(result.nodes, "tests/test_widget.py::Widget.render");
    expect(render.kind).toBe("method");
    expect(render.name).toBe("render");
    expect(render.signature).toBe("def render(self, title):");
    expect(render.startLine).toBe(11);
    expect(render.endLine).toBe(16);

    const label = nodeWithQn(result.nodes, "tests/test_widget.py::Base.label");
    expect(label.kind).toBe("method");

    const build = nodeWithQn(result.nodes, "tests/test_widget.py::build");
    expect(build.kind).toBe("function");
    expect(build.signature).toBe("def build(count):");
    expect(build.startLine).toBe(23);
    expect(build.endLine).toBe(25);
  });

  test("the class node carries its header as the signature and the module sits under the file", () => {
    const widget = nodeWithQn(result.nodes, "tests/test_widget.py::Widget");
    expect(widget.kind).toBe("class");
    expect(widget.signature).toBe("class Widget(Base):");
    expect(widget.startLine).toBe(10);
    expect(widget.endLine).toBe(20);

    const module = nodeWithQn(result.nodes, "tests/test_widget.py::test_widget");
    expect(module.kind).toBe("module");
  });

  test("a decorator becomes the declaration's props, not part of its signature", () => {
    const paint = nodeWithQn(result.nodes, "tests/test_widget.py::Widget.paint");
    expect(paint.signature).toBe("def paint(self, parts, options):");
    expect(paint.props).toEqual({ decorators: ['route("/items", handler=render)'] });
  });

  test("the test kind follows the declaration name, not only the file name", () => {
    expect(nodeWithQn(result.nodes, "tests/test_widget.py::test_render").kind).toBe("test");
    const smoke = nodeWithQn(sourceResult.nodes, "src/widget.py::test_smoke");
    expect(smoke.kind).toBe("test");
    expect(smoke.isTest).toBe(true);
    // A non-test path keeps `isTest` false for everything its file name did not mark.
    expect(nodeWithQn(sourceResult.nodes, "src/widget.py::Widget").isTest).toBe(false);
    expect(nodeWithQn(sourceResult.nodes, "src/widget.py::Widget.render").kind).toBe("method");
  });
});

describe("extractFile: contains edges", () => {
  test("every declaration has exactly one containing edge", () => {
    const contains = edgesOfKind(result.rawEdges, "contains");
    const declared = result.nodes.filter((node) => node.kind !== "file").map((node) => node.qn);
    expect(contains.map((edge) => edge.target).sort()).toEqual([...declared].sort());
    expect(new Set(contains.map((edge) => edge.target)).size).toBe(declared.length);
  });

  test("containment runs from the file or the class, never from nothing", () => {
    const contains = edgesOfKind(result.rawEdges, "contains");
    const parentOf = (target: string): string | undefined =>
      contains.find((edge) => edge.target === target)?.src;
    expect(parentOf("tests/test_widget.py::test_widget")).toBe(
      "tests/test_widget.py::test_widget.py",
    );
    expect(parentOf("tests/test_widget.py::Widget")).toBe("tests/test_widget.py::test_widget");
    expect(parentOf("tests/test_widget.py::Widget.render")).toBe("tests/test_widget.py::Widget");
    for (const edge of contains) {
      expect(edge.tier).toBe(EXTRACTED_CANDIDATE);
      expect(edge.receiver).toBeNull();
    }
  });
});

describe("extractFile: inheritance", () => {
  test("a class base becomes one inherits edge from the class node", () => {
    const inherits = edgesOfKind(result.rawEdges, "inherits");
    expect(inherits).toHaveLength(1);
    expect(inherits[0]).toEqual({
      kind: "inherits",
      src: "tests/test_widget.py::Widget",
      target: "Base",
      receiver: null,
      line: 10,
      tier: EXTRACTED_CANDIDATE,
      evidence: "inherits",
      localName: "",
    });
  });

  test("a decorator's argument list is not mistaken for a base list", () => {
    // Python's `argument_list` is both a base list and a call's argument list; only the direct
    // child of a class may produce inheritance edges.
    expect(edgesOfKind(result.rawEdges, "implements")).toHaveLength(0);
    expect(edgesOfKind(result.rawEdges, "inherits").map((edge) => edge.target)).not.toContain(
      "handler",
    );
  });
});

describe("extractFile: imports and calls", () => {
  test("import statements become imports edges with the local name they bind", () => {
    const imports = edgesOfKind(result.rawEdges, "imports");
    expect(imports.map((edge) => `${edge.target}:${edge.localName}`)).toEqual([
      "os:os",
      "pathlib:Path",
    ]);
    expect(imports.map((edge) => edge.tier)).toEqual([EXTRACTED_CANDIDATE, EXTRACTED_CANDIDATE]);
  });

  test("a call records the callee name and the receiver it was written with", () => {
    const calls = edgesOfKind(result.rawEdges, "calls");
    const paint = calls.find((edge) => edge.target === "paint");
    expect(paint).toMatchObject({
      src: "tests/test_widget.py::Widget.render",
      receiver: "self",
      evidence: "call:self.paint",
      line: 15,
    });
    const chained = calls.find((edge) => edge.evidence === "call:widget.render");
    expect(chained).toMatchObject({ target: "render", receiver: "widget" });
    expect(calls.map((edge) => edge.target)).toContain("Widget");
  });
});

describe("extractFile: value-position references", () => {
  test("the six value positions are all produced, and nothing else", () => {
    const sites = edgesOfKind(result.rawEdges, "references").map((edge) => edge.evidence);
    expect([...new Set(sites)].sort()).toEqual([...REFERENCE_SITES].sort());
    expect(REFERENCE_SITES).toHaveLength(6);
  });

  test("each site names the value it holds and stays inside its declaration", () => {
    const references = edgesOfKind(result.rawEdges, "references");
    const targetsOf = (site: string): string[] =>
      references
        .filter((edge) => edge.evidence === site)
        .map((edge) => edge.target)
        .sort()
        .filter((target, index, all) => all.indexOf(target) === index);
    expect(targetsOf("call_argument")).toEqual(["count", "options", "parts", "prefix"]);
    expect(targetsOf("assignment_rhs")).toEqual(["sep"]);
    expect(targetsOf("object_element")).toEqual(["prefix", "title"]);
    expect(targetsOf("array_element")).toEqual(["prefix", "title"]);
    expect(targetsOf("decorator_argument")).toEqual(["render"]);
    expect(targetsOf("return_expression")).toEqual(["prefix"]);

    for (const edge of references) {
      expect(edge.tier).toBe(EXTRACTED_CANDIDATE);
      // A value position is a site inside a declaration, never the file node's own edge.
      expect(edge.src).not.toBe(result.nodes[0]?.qn);
    }
    // `os.sep` keeps its qualifier, so the bind pass can decide whether the head or the tail is
    // the symbol instead of re-parsing the evidence string.
    const qualified = references.find((edge) => edge.evidence === "assignment_rhs");
    expect(qualified).toMatchObject({ target: "sep", receiver: "os", line: 12 });
    // A decorator argument belongs to the class the method is declared in, not to the method.
    const decorated = references.find((edge) => edge.evidence === "decorator_argument");
    expect(decorated).toMatchObject({ src: "tests/test_widget.py::Widget", target: "render" });
  });

  test("a literal or a nested call is not a reference; its own arguments still are", () => {
    const references = edgesOfKind(result.rawEdges, "references");
    const buildLine24 = references.filter((edge) => edge.line === 24).map((edge) => edge.target);
    expect(buildLine24).toEqual(["count"]);
  });
});

describe("extractFile: coverage facts", () => {
  test("the fixture produces no unresolved site", () => {
    expect(result.unresolved).toEqual([]);
  });

  test("every raw edge is an extracted candidate of a declared kind", () => {
    for (const edge of result.rawEdges) {
      expect(edge.tier).toBe(EXTRACTED_CANDIDATE);
      expect(edge.line).toBeGreaterThan(0);
      expect(edge.src).not.toBe("");
      expect(edge.target).not.toBe("");
    }
    expect(new Set(result.rawEdges.map((edge) => edge.tier))).toEqual(
      new Set([EXTRACTED_CANDIDATE]),
    );
  });

  test("the language-specific branches are a table of at most three documented entries", () => {
    expect(EXTRACTION_EXCEPTIONS.length).toBeLessThanOrEqual(3);
    expect(EXTRACTION_EXCEPTIONS.length).toBeGreaterThan(0);
    for (const entry of EXTRACTION_EXCEPTIONS) {
      expect(["fileModule", "callImport"]).toContain(entry.rule);
      expect(entry.reason.length).toBeGreaterThan(20);
    }
    // Every exception must name a language the table can be asked about.
    expect(EXTRACTION_EXCEPTIONS.map((entry) => entry.lang)).toContain("python");
  });
});
