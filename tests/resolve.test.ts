import { describe, expect, test } from "bun:test";
import { qn } from "#shared/paths";
import {
  EXTRACTED_CANDIDATE,
  type ExtractedNode,
  type RawEdge,
  type RawEdgeKind,
  type UnresolvedSite,
} from "../src/indexer/extract";
import {
  bind,
  EDGE_TIERS,
  type FileUnit,
  type ResolvedEdge,
  SymbolIndex,
} from "../src/indexer/resolve";

/**
 * The bind pass on a hand-built unit set, because binding is a function of qns and names: the
 * extractor's grammar work is proved in `extract.test.ts`, and what is under test here is the
 * binding order, the tiers it assigns, and the edges it refuses to emit.
 *
 * The graph is deliberately six small files. `render` occurs twice in the index (`Base.render`,
 * `Panel.render`) so the unique-name tier cannot answer for it and the superclass tier has to;
 * `Widget` both inherits `Base` and implements `Reader`; `Config` is unique, so a reference to it
 * binds, while a reference to `render` is ambiguous and must disappear without a trace.
 */

const BASE_FILE = "src/base.py::base.py";
const BASE_MODULE = "src/base.py::base";
const BASE = "src/base.py::Base";
const BASE_RENDER = "src/base.py::Base.render";

const PANEL_MODULE = "src/panel.py::panel";
const PANEL = "src/panel.py::Panel";
const PANEL_RENDER = "src/panel.py::Panel.render";

const HELPERS_FILE = "src/helpers.py::helpers.py";
const HELPERS_MODULE = "src/helpers.py::helpers";
const NORMALISE = "src/helpers.py::normalise";

const UTIL_MODULE = "src/util.py::util";
const UTIL_UNIQUE = "src/util.py::uniqueHelper";
const UTIL_CONFIG = "src/util.py::Config";

const PORTS_MODULE = "src/ports.py::ports";
const READER = "src/ports.py::Reader";

const WIDGET_FILE = "src/widget.py::widget.py";
const WIDGET_MODULE = "src/widget.py::widget";
const WIDGET = "src/widget.py::Widget";
const WIDGET_PAINT = "src/widget.py::Widget.paint";
const WIDGET_DRAW = "src/widget.py::Widget.draw";

/** One declaration. The line numbers are the call-site lines the assertions below refer to. */
function decl(
  kind: ExtractedNode["kind"],
  path: string,
  scope: string,
  name: string,
  startLine = 1,
): ExtractedNode {
  return {
    kind,
    name,
    qn: qn(path, scope, name),
    startLine,
    endLine: startLine + 2,
    signature: null,
    isTest: false,
    props: {},
  };
}

interface RawSpec {
  readonly receiver?: string;
  readonly localName?: string;
  readonly evidence?: string;
}

function raw(
  kind: RawEdgeKind,
  src: string,
  target: string,
  line: number,
  spec: RawSpec = {},
): RawEdge {
  return {
    kind,
    src,
    target,
    receiver: spec.receiver ?? null,
    line,
    tier: EXTRACTED_CANDIDATE,
    evidence: spec.evidence ?? kind,
    localName: spec.localName ?? "",
  };
}

function contains(parent: string, child: string, line: number): RawEdge {
  return raw("contains", parent, child, line, { evidence: "contains" });
}

function unit(
  relPath: string,
  nodes: readonly ExtractedNode[],
  rawEdges: readonly RawEdge[],
  unresolved: readonly UnresolvedSite[] = [],
): FileUnit {
  return { relPath, nodes, rawEdges, unresolved };
}

const UNITS: readonly FileUnit[] = [
  unit(
    "src/base.py",
    [
      decl("file", "src/base.py", "", "base.py"),
      decl("module", "src/base.py", "", "base"),
      decl("class", "src/base.py", "", "Base", 3),
      decl("method", "src/base.py", "Base", "render", 4),
    ],
    [
      contains(BASE_FILE, BASE_MODULE, 1),
      contains(BASE_MODULE, BASE, 3),
      contains(BASE, BASE_RENDER, 4),
    ],
  ),
  unit(
    "src/panel.py",
    [
      decl("file", "src/panel.py", "", "panel.py"),
      decl("module", "src/panel.py", "", "panel"),
      decl("class", "src/panel.py", "", "Panel", 3),
      decl("method", "src/panel.py", "Panel", "render", 4),
    ],
    [
      contains("src/panel.py::panel.py", PANEL_MODULE, 1),
      contains(PANEL_MODULE, PANEL, 3),
      contains(PANEL, PANEL_RENDER, 4),
    ],
  ),
  unit(
    "src/helpers.py",
    [
      decl("file", "src/helpers.py", "", "helpers.py"),
      decl("module", "src/helpers.py", "", "helpers"),
      decl("function", "src/helpers.py", "", "normalise", 2),
    ],
    [contains(HELPERS_FILE, HELPERS_MODULE, 1), contains(HELPERS_MODULE, NORMALISE, 2)],
  ),
  unit(
    "src/util.py",
    [
      decl("file", "src/util.py", "", "util.py"),
      decl("module", "src/util.py", "", "util"),
      decl("function", "src/util.py", "", "uniqueHelper", 2),
      decl("class", "src/util.py", "", "Config", 5),
    ],
    [
      contains("src/util.py::util.py", UTIL_MODULE, 1),
      contains(UTIL_MODULE, UTIL_UNIQUE, 2),
      contains(UTIL_MODULE, UTIL_CONFIG, 5),
    ],
  ),
  unit(
    "src/ports.py",
    [
      decl("file", "src/ports.py", "", "ports.py"),
      decl("module", "src/ports.py", "", "ports"),
      decl("class", "src/ports.py", "", "Reader", 3),
    ],
    [contains("src/ports.py::ports.py", PORTS_MODULE, 1), contains(PORTS_MODULE, READER, 3)],
  ),
  unit(
    "src/widget.py",
    [
      decl("file", "src/widget.py", "", "widget.py"),
      decl("module", "src/widget.py", "", "widget"),
      decl("class", "src/widget.py", "", "Widget", 5),
      decl("method", "src/widget.py", "Widget", "paint", 9),
      decl("method", "src/widget.py", "Widget", "draw", 12),
    ],
    [
      contains(WIDGET_FILE, WIDGET_MODULE, 1),
      contains(WIDGET_MODULE, WIDGET, 5),
      contains(WIDGET, WIDGET_PAINT, 9),
      contains(WIDGET, WIDGET_DRAW, 12),
      // Non-call edges: one resolvable base, one interface, two imports.
      raw("inherits", WIDGET, "Base", 5, { evidence: "inherits" }),
      raw("implements", WIDGET, "Reader", 5, { evidence: "implements" }),
      raw("imports", WIDGET_MODULE, "helpers", 2, {
        localName: "normalise",
        evidence: "import:helpers",
      }),
      raw("imports", WIDGET_MODULE, "pathlib", 3, {
        localName: "Path",
        evidence: "import:pathlib",
      }),
      // The five call-binding cases, in order of the line they sit on.
      raw("calls", WIDGET_DRAW, "paint", 10, { receiver: "self", evidence: "call:self.paint" }),
      raw("calls", WIDGET_DRAW, "normalise", 11, { evidence: "call:normalise" }),
      raw("calls", WIDGET_DRAW, "uniqueHelper", 12, { evidence: "call:uniqueHelper" }),
      raw("calls", WIDGET_DRAW, "render", 13, { receiver: "self", evidence: "call:self.render" }),
      raw("calls", WIDGET_DRAW, "render", 14, {
        receiver: "widget",
        evidence: "call:widget.render",
      }),
      raw("calls", WIDGET_DRAW, "render", 15, { evidence: "call:render" }),
      raw("calls", WIDGET_DRAW, "mystery", 16, { evidence: "call:mystery" }),
      // One reference that binds, one ambiguous, one unmatched.
      raw("references", WIDGET_DRAW, "Config", 17, { evidence: "assignment_rhs" }),
      raw("references", WIDGET_DRAW, "render", 18, { evidence: "array_element" }),
      raw("references", WIDGET_DRAW, "absent", 19, { evidence: "return_expression" }),
    ],
    [{ line: 20, rawTarget: "obj[0]", evidenceKind: "dynamic", evidence: "call" }],
  ),
];

const INDEX = SymbolIndex.from(UNITS);
const RESULT = bind(UNITS, INDEX);

/** Every qn the units declare, the set the no-dangling-edge invariant is checked against. */
const NODE_QNS: ReadonlySet<string> = new Set(
  UNITS.flatMap((entry) => entry.nodes.map((node) => node.qn)),
);

const UNIT_PATHS: ReadonlySet<string> = new Set(UNITS.map((entry) => entry.relPath));

function edgesOfKind(kind: RawEdgeKind): ResolvedEdge[] {
  return RESULT.edges.filter((edge) => edge.kind === kind);
}

function edgeAt(line: number): ResolvedEdge | undefined {
  return RESULT.edges.find((edge) => edge.line === line && edge.kind === "calls");
}

function unresolvedAt(line: number) {
  return RESULT.unresolved.find((entry) => entry.line === line);
}

describe("SymbolIndex", () => {
  test("is built from units and answers per-file declarations and import tables", () => {
    expect(INDEX.nodesIn("src/widget.py").map((node) => node.qn)).toEqual([
      WIDGET_FILE,
      WIDGET_MODULE,
      WIDGET,
      WIDGET_PAINT,
      WIDGET_DRAW,
    ]);
    expect(INDEX.nodesIn("src/widget.py")).toHaveLength(5);

    const imports = INDEX.importsOf("src/widget.py");
    expect(imports.get("normalise")).toEqual([{ module: "helpers", localName: "normalise" }]);
    expect(imports.get("Path")).toEqual([{ module: "pathlib", localName: "Path" }]);

    // A module string names a unit; `index`/`__init__` answering for their directory is not used
    // here, so only the plain stem lookup is exercised.
    expect(INDEX.moduleFile("helpers")).toBe("src/helpers.py");
    expect(INDEX.moduleFile("pathlib")).toBeNull();
    expect(INDEX.moduleCandidates("helpers")).toEqual(["src/helpers.py"]);
    expect(INDEX.moduleCandidates("pathlib")).toEqual([]);

    // Reportable declarations only: the module node named `widget` is not a call target.
    expect(INDEX.nodesNamed("widget")).toEqual([]);
    expect(
      INDEX.nodesNamed("render")
        .map((node) => node.qn)
        .sort(),
    ).toEqual([BASE_RENDER, PANEL_RENDER].sort());
  });
});

describe("bind: the invariant", () => {
  test("no edge targets a node that does not exist, on either end", () => {
    expect(RESULT.edges.length).toBeGreaterThan(0);
    for (const edge of RESULT.edges) {
      expect({ dst: edge.dst, known: NODE_QNS.has(edge.dst) }).toEqual({
        dst: edge.dst,
        known: true,
      });
      expect({ src: edge.src, known: NODE_QNS.has(edge.src) }).toEqual({
        src: edge.src,
        known: true,
      });
    }
  });

  test("every edge names a unit and a declared tier", () => {
    for (const edge of RESULT.edges) {
      expect(UNIT_PATHS.has(edge.file)).toBe(true);
      expect(EDGE_TIERS).toContain(edge.tier);
    }
  });

  test("the pass is pure: a second run over the same input is identical", () => {
    expect(bind(UNITS, SymbolIndex.from(UNITS))).toEqual(RESULT);
  });
});

describe("bind: call binding order", () => {
  test("1. an exact qn in the same file binds at extracted", () => {
    const edge = edgeAt(10);
    expect(edge).toMatchObject({
      src: WIDGET_DRAW,
      dst: WIDGET_PAINT,
      tier: "extracted",
      file: "src/widget.py",
      evidence: "call:self.paint",
    });
  });

  test("2. a name visible through the file's imports binds at extracted", () => {
    const edge = edgeAt(11);
    expect(edge).toMatchObject({ src: WIDGET_DRAW, dst: NORMALISE, tier: "extracted" });
  });

  test("3. a name unique in the whole index binds at inferred", () => {
    const edge = edgeAt(12);
    expect(edge).toMatchObject({ src: WIDGET_DRAW, dst: UTIL_UNIQUE, tier: "inferred" });

    // Only reachable because `normalise` is imported: the same call without the import statement
    // falls through to the unique-name tier, which is what makes this tier observable.
    const stripped = UNITS.map((entry) =>
      entry.relPath === "src/widget.py"
        ? unit(
            entry.relPath,
            entry.nodes,
            entry.rawEdges.filter((rawEdge) => rawEdge.kind !== "imports"),
            entry.unresolved,
          )
        : entry,
    );
    const noImports = bind(stripped, SymbolIndex.from(stripped));
    const fallback = noImports.edges.find((edge) => edge.line === 11 && edge.kind === "calls");
    expect(fallback).toMatchObject({ dst: NORMALISE, tier: "inferred" });
  });

  test("4. a superclass method binds at inferred when the receiver's class inherits it", () => {
    const edge = edgeAt(13);
    expect(edge).toMatchObject({ src: WIDGET_DRAW, dst: BASE_RENDER, tier: "inferred" });
    // The name alone is ambiguous (`Base.render` and `Panel.render`), so this edge can only come
    // from the inheritance evidence.
    expect(INDEX.nodesNamed("render")).toHaveLength(2);
    expect(edgesOfKind("calls").map((edge) => edge.dst)).not.toContain(PANEL_RENDER);
  });

  test("5. every failing call is recorded with its evidence kind and emits no edge", () => {
    // An untypeable receiver: `widget` names no class, so the two candidates cannot be separated.
    expect(edgeAt(14)).toBeUndefined();
    expect(unresolvedAt(14)).toMatchObject({
      file: "src/widget.py",
      rawTarget: "render",
      evidenceKind: "receiver_type_unknown",
      evidence: "call:widget.render",
    });

    // No receiver and two candidates: ambiguous.
    expect(edgeAt(15)).toBeUndefined();
    expect(unresolvedAt(15)).toMatchObject({ rawTarget: "render", evidenceKind: "ambiguous" });

    // No receiver and no candidate anywhere: dynamic.
    expect(edgeAt(16)).toBeUndefined();
    expect(unresolvedAt(16)).toMatchObject({ rawTarget: "mystery", evidenceKind: "dynamic" });
  });

  test("the extractor's own nameless sites pass through unchanged", () => {
    expect(unresolvedAt(20)).toMatchObject({
      file: "src/widget.py",
      rawTarget: "obj[0]",
      evidenceKind: "dynamic",
      evidence: "call",
    });
  });
});

describe("bind: non-call edges", () => {
  test("resolved inherits and implements edges are extracted", () => {
    expect(edgesOfKind("inherits")).toEqual([
      expect.objectContaining({ src: WIDGET, dst: BASE, tier: "extracted" }),
    ]);
    expect(edgesOfKind("implements")).toEqual([
      expect.objectContaining({ src: WIDGET, dst: READER, tier: "extracted" }),
    ]);
  });

  test("an import binds to the target unit's file node and an out-of-tree module does not bind", () => {
    expect(edgesOfKind("imports")).toEqual([
      expect.objectContaining({
        src: WIDGET_MODULE,
        dst: HELPERS_FILE,
        tier: "extracted",
        localName: "normalise",
      }),
    ]);
    expect(unresolvedAt(3)).toMatchObject({
      rawTarget: "pathlib",
      evidenceKind: "dynamic",
      evidence: "import:pathlib",
    });
  });

  test("a reference binds at inferred only when exactly one node matches", () => {
    expect(edgesOfKind("references")).toEqual([
      expect.objectContaining({
        src: WIDGET_DRAW,
        dst: UTIL_CONFIG,
        tier: "inferred",
        evidence: "assignment_rhs",
      }),
    ]);
  });

  test("an ambiguous or unmatched reference is dropped silently, with no unresolved row", () => {
    const referenced = edgesOfKind("references").map((edge) => edge.dst);
    expect(referenced).not.toContain(BASE_RENDER);
    expect(referenced).not.toContain(PANEL_RENDER);
    expect(RESULT.edges.some((edge) => edge.kind === "references" && edge.line === 18)).toBe(false);
    expect(RESULT.edges.some((edge) => edge.kind === "references" && edge.line === 19)).toBe(false);

    // Silence is the contract: a non-unique name is not an unresolvable site.
    expect(unresolvedAt(18)).toBeUndefined();
    expect(unresolvedAt(19)).toBeUndefined();
    expect(RESULT.unresolved.map((entry) => entry.evidence)).not.toContain("array_element");
    expect(RESULT.unresolved.map((entry) => entry.evidence)).not.toContain("return_expression");
  });

  test("containment survives the pass at extracted, once per raw contains edge", () => {
    const rawContains = UNITS.flatMap((entry) =>
      entry.rawEdges.filter((edge) => edge.kind === "contains"),
    );
    const bound = edgesOfKind("contains");
    expect(bound).toHaveLength(rawContains.length);
    expect(bound.every((edge) => edge.tier === "extracted")).toBe(true);
    expect(bound).toContainEqual(
      expect.objectContaining({ src: WIDGET_FILE, dst: WIDGET_MODULE, tier: "extracted" }),
    );
    expect(bound).toContainEqual(
      expect.objectContaining({ src: WIDGET, dst: WIDGET_DRAW, tier: "extracted" }),
    );
    // No containment edge was collapsed by the per-kind filter: the full list is the count. Every
    // other edge in the fixture is one of the eight resolved non-contains edges.
    expect(RESULT.edges).toHaveLength(bound.length + 8);
  });
});

/*
 * The import taxonomy (#12), on its own six-line fixture so the main graph above keeps its shape.
 *
 * `src/one/thing.py` and `src/two/thing.py` share the stem `thing`, so the specifier alone cannot
 * pick one; `src/app.py` imports them and nothing else. The four failures are the four causes a
 * failed import can have, and they must not read the same way: a stem tie is a tree defect, a
 * `./` miss is a broken path, a package and a `#` alias are modules this resolver never sees.
 */
const THING_ONE_FILE = "src/one/thing.py::thing.py";
const THING_ONE_MODULE = "src/one/thing.py::thing";
const THING_TWO_FILE = "src/two/thing.py::thing.py";
const THING_TWO_MODULE = "src/two/thing.py::thing";
const APP_FILE = "src/app.py::app.py";
const APP_MODULE = "src/app.py::app";

function thingUnit(relPath: string, fileQn: string, moduleQn: string): FileUnit {
  return unit(
    relPath,
    [
      decl("file", relPath, "", relPath.slice(relPath.lastIndexOf("/") + 1)),
      decl("module", relPath, "", "thing"),
    ],
    [contains(fileQn, moduleQn, 1)],
  );
}

const IMPORT_UNITS: readonly FileUnit[] = [
  thingUnit("src/one/thing.py", THING_ONE_FILE, THING_ONE_MODULE),
  thingUnit("src/two/thing.py", THING_TWO_FILE, THING_TWO_MODULE),
  unit(
    "src/app.py",
    [decl("file", "src/app.py", "", "app.py"), decl("module", "src/app.py", "", "app")],
    [
      contains(APP_FILE, APP_MODULE, 1),
      // A stem tie, a broken relative path, a package, a tsconfig alias, and one import that binds.
      raw("imports", APP_MODULE, "thing", 2, { evidence: "import:thing" }),
      raw("imports", APP_MODULE, "./missing", 3, { evidence: "import:./missing" }),
      raw("imports", APP_MODULE, "sangkar-elsewhere", 4, {
        evidence: "import:sangkar-elsewhere",
      }),
      raw("imports", APP_MODULE, "#shared/thing", 5, { evidence: "import:#shared/thing" }),
      raw("imports", APP_MODULE, "./two/thing", 6, { evidence: "import:./two/thing" }),
    ],
  ),
];

const IMPORT_INDEX = SymbolIndex.from(IMPORT_UNITS);
const IMPORT_RESULT = bind(IMPORT_UNITS, IMPORT_INDEX);

function importFailureAt(line: number) {
  return IMPORT_RESULT.unresolved.find((entry) => entry.line === line);
}

describe("SymbolIndex.moduleCandidates", () => {
  test("lists every unit a stem could name, sorted, and moduleFile agrees with the count", () => {
    expect(IMPORT_INDEX.moduleCandidates("thing")).toEqual([
      "src/one/thing.py",
      "src/two/thing.py",
    ]);
    // Two candidates cannot be resolved, so the coarse answer stays null rather than guessing one.
    expect(IMPORT_INDEX.moduleFile("thing")).toBeNull();

    // A specifier with a path segment narrows the candidates to one, and then both agree again.
    expect(IMPORT_INDEX.moduleCandidates("./two/thing")).toEqual(["src/two/thing.py"]);
    expect(IMPORT_INDEX.moduleFile("./two/thing")).toBe("src/two/thing.py");

    // A module nothing matches is an empty list, not a null string.
    expect(IMPORT_INDEX.moduleCandidates("nowhere")).toEqual([]);
    expect(IMPORT_INDEX.moduleFile("nowhere")).toBeNull();
  });
});

describe("bind: import failures carry their cause", () => {
  test("a stem tie is ambiguous and names both competing paths in its evidence", () => {
    // Neither file is chosen for line 2, so no import edge is emitted for the tie (line 6 binds
    // `./two/thing`, which is the same unit reached by a specifier that names it).
    expect(IMPORT_RESULT.edges.some((edge) => edge.kind === "imports" && edge.line === 2)).toBe(
      false,
    );
    expect(importFailureAt(2)).toEqual({
      file: "src/app.py",
      line: 2,
      rawTarget: "thing",
      evidenceKind: "ambiguous",
      evidence: "import:thing candidates: src/one/thing.py | src/two/thing.py",
    });
  });

  test("a relative specifier that matched nothing is missing_module", () => {
    expect(importFailureAt(3)).toEqual({
      file: "src/app.py",
      line: 3,
      rawTarget: "./missing",
      evidenceKind: "missing_module",
      evidence: "import:./missing (no file matched)",
    });
  });

  test("a bare package specifier stays dynamic, with its evidence unchanged", () => {
    expect(importFailureAt(4)).toMatchObject({
      rawTarget: "sangkar-elsewhere",
      evidenceKind: "dynamic",
      evidence: "import:sangkar-elsewhere",
    });
  });

  test("a # alias stays dynamic: a tsconfig path map is not a defect in this tree", () => {
    expect(importFailureAt(5)).toMatchObject({
      rawTarget: "#shared/thing",
      evidenceKind: "dynamic",
      evidence: "import:#shared/thing",
    });
  });

  test("the one import with a single candidate still binds to its file node", () => {
    expect(IMPORT_RESULT.edges.filter((edge) => edge.kind === "imports")).toEqual([
      expect.objectContaining({ src: APP_MODULE, dst: THING_TWO_FILE, tier: "extracted" }),
    ]);
    expect(importFailureAt(6)).toBeUndefined();
  });
});
