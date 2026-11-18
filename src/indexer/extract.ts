/**
 * The extractor: one walk over a parsed tree produces everything a single file contributes to the
 * graph — its nodes, the containment edges that place them, and the raw edges the bind pass
 * resolves later.
 *
 * The walk is driven by the language row in `lang/types.ts`: which grammar nodes are classes,
 * functions, imports, calls or inheritance clauses is data, so no code here names a language. Two
 * grammar shapes cannot be expressed as a list of node types; they are the entries of
 * `EXTRACTION_EXCEPTIONS`, which is the only place a language is named and is held to three
 * entries.
 *
 * Nothing here is resolved. Every raw edge carries `tier: "extracted_candidate"` and a textual
 * `target`; `resolve.ts` either binds it to a node id or records an `unresolved` row. A site that
 * carries no name at all (a computed callee, an import with nothing nameable in it) is recorded
 * straight in `unresolved` with `evidence_kind: "dynamic"`, because no later pass can recover a
 * name the source never wrote.
 */

import type { Node, Tree } from "web-tree-sitter";
import { normalizeRel, qn as qualify } from "#shared/paths";
import type { LangId, LangRow } from "./lang/types";

/**
 * Every node kind the graph declares. `endpoint` is declared and produced by no rule here: a route
 * node needs the route props (method, path) of a specific framework, which is the endpoint pass's
 * business. Emitting one from a decorator's mere presence would be exactly the language-specific
 * guess this table-driven walk exists to avoid.
 */
export const NODE_KINDS = [
  "file",
  "module",
  "class",
  "function",
  "method",
  "test",
  "endpoint",
] as const;

export type NodeKind = (typeof NODE_KINDS)[number];

/** The edge kinds a file can produce before anything is bound. */
export const RAW_EDGE_KINDS = [
  "contains",
  "calls",
  "imports",
  "inherits",
  "implements",
  "references",
] as const;

export type RawEdgeKind = (typeof RAW_EDGE_KINDS)[number];

/** The only tier a raw edge has: nothing has been resolved yet, so nothing is "extracted". */
export const EXTRACTED_CANDIDATE = "extracted_candidate";

export type RawEdgeTier = typeof EXTRACTED_CANDIDATE;

/**
 * The six value positions that name something and therefore become `references` candidates. Order
 * is documentation, not significance: `object_element` and `array_element` are exactly two sites
 * because the grammar shapes differ and a fixture asserts both.
 */
export const REFERENCE_SITES = [
  "call_argument",
  "assignment_rhs",
  "object_element",
  "array_element",
  "decorator_argument",
  "return_expression",
] as const;

export type ReferenceSite = (typeof REFERENCE_SITES)[number];

/** JSON-serialisable node props (`node.props`); decorators are the only producer today. */
export type NodeProps = Readonly<Record<string, string | number | boolean | readonly string[]>>;

/** One declared node, ready for the persist pass. `qn` is the key everything else refers to. */
export interface ExtractedNode {
  readonly kind: NodeKind;
  readonly name: string;
  /** `relPath::Scope.name`, from the shared formatter so the key form has one definition. */
  readonly qn: string;
  readonly startLine: number;
  readonly endLine: number;
  readonly signature: string | null;
  readonly isTest: boolean;
  readonly props: NodeProps;
}

/**
 * One site, unresolved by construction. `src` is the qn of the declaration the site sits in (the
 * file node for top-level sites), `target` is the last name segment the bind pass looks up and
 * `receiver` is the qualifier as written (`self`, `os.path`), so a receiver-sensitive binding does
 * not have to re-parse `evidence`.
 */
export interface RawEdge {
  readonly kind: RawEdgeKind;
  readonly src: string;
  readonly target: string;
  readonly receiver: string | null;
  readonly line: number;
  readonly tier: RawEdgeTier;
  /** Why the site exists: `contains`, `call:<as written>`, `import:<as written>`, the edge kind, or the reference site. */
  readonly evidence: string;
  /** The name an import binds locally; `""` for every other kind. */
  readonly localName: string;
}

/** The only evidence kind this pass can vouch for: the source offered no name to resolve. */
export type UnresolvedEvidenceKind = "dynamic";

/** A site with no name to bind, recorded so absence is never mistaken for "no such call". */
export interface UnresolvedSite {
  readonly line: number;
  readonly rawTarget: string;
  readonly evidenceKind: UnresolvedEvidenceKind;
  readonly evidence: string;
}

export interface ExtractResult {
  readonly nodes: readonly ExtractedNode[];
  readonly rawEdges: readonly RawEdge[];
  readonly unresolved: readonly UnresolvedSite[];
}

export type ExtractionRule = "fileModule" | "callImport";

/** A grammar shape no list of node types can express, implemented under `rule` for `lang`. */
export interface ExtractionException {
  readonly lang: LangId;
  readonly rule: ExtractionRule;
  readonly reason: string;
}

/**
 * The complete set of language-specific branches. Capped at three by
 * `tests/extract.test.ts`; a fourth entry means the walk grew a language condition instead of a
 * table.
 */
export const EXTRACTION_EXCEPTIONS: readonly ExtractionException[] = [
  {
    lang: "python",
    rule: "fileModule",
    reason:
      "the grammar has no module declaration, so one file is one module: the root emits a `module` " +
      "node named after the file stem, which is the file's own namespace and adds no qn segment",
  },
  {
    lang: "ruby",
    rule: "callImport",
    reason:
      "the row lists `call` as both an import and a call, because `require` is an ordinary call: " +
      "only a `require`, `require_relative` or `load` callee is an import",
  },
];

/** `require`-style callees for the `callImport` rule. */
const REQUIRE_CALLEES: ReadonlySet<string> = new Set(["require", "require_relative", "load"]);

/** Node types that carry a name as their whole text. */
const NAME_TYPES: ReadonlySet<string> = new Set([
  "identifier",
  "type_identifier",
  "field_identifier",
  "property_identifier",
  "shorthand_property_identifier",
  "constant",
  "name",
  "simple_identifier",
  "this",
  "super",
  "variable_name",
]);

/** Node types that are a name chain (`os.path.join`, `A::B`), whose text is the chain. */
const CHAIN_TYPES: ReadonlySet<string> = new Set([
  "dotted_name",
  "attribute",
  "member_expression",
  "field_expression",
  "selector_expression",
  "scoped_identifier",
  "scoped_type_identifier",
  "qualified_name",
  "namespace_name",
  "member_access_expression",
]);

/** Grammar types that declare a namespace rather than a class; Ruby and TS/PHP have one. */
const MODULE_TYPES: ReadonlySet<string> = new Set([
  "module",
  "module_declaration",
  "namespace_definition",
  "internal_module",
]);

/** Argument/parenthesis containers, shared by call arguments and decorator arguments. */
const ARGUMENT_CONTAINERS: ReadonlySet<string> = new Set([
  "argument_list",
  "arguments",
  "value_arguments",
  "value_argument_list",
]);

/** The array-literal shapes of the twelve grammars. */
const ARRAY_LITERALS: ReadonlySet<string> = new Set([
  "list",
  "tuple",
  "set",
  "array",
  "array_expression",
  "list_expression",
  "set_expression",
]);

/** The object/map-literal shapes. A `pair` inside one contributes its `value`, never its key. */
const OBJECT_LITERALS: ReadonlySet<string> = new Set([
  "dictionary",
  "object",
  "object_expression",
  "map_expression",
]);

const RETURN_TYPES: ReadonlySet<string> = new Set(["return_statement", "return_expression"]);

const ASSIGNMENT_TYPES: ReadonlySet<string> = new Set([
  "assignment",
  "augmented_assignment",
  "assignment_expression",
]);

/** A wrapper around a declaration whose name has to be found one level down (`type_spec`, `function_declarator`). */
const DECLARATOR_TYPES = /(?:^|_)(?:spec|declarator)$/u;

/** A name that reads as a test: `test_x`, `x_test`, `TestCase`, `handle_spec`. */
const TEST_NAME = /(?:^|[._-])(?:test|spec)s?(?:[^a-z]|$)|^Test(?:s|Cases)?(?:[^a-z]|$)/u;

/** Node fields that hold a callee, in the order the twelve grammars spell it. */
const CALLEE_FIELDS: readonly string[] = [
  "function",
  "method",
  "name",
  "constructor",
  "declarator",
];

const SIGNATURE_CAP = 200;
const TEXT_CAP = 120;

/**
 * Extracts one file. `tree` comes from `parse.ts`, `spec` is the resolved language row and
 * `relPath` is the repo-relative path every qn is built from. The file node is always `nodes[0]`.
 */
export function extractFile(tree: Tree, spec: LangRow, relPath: string): ExtractResult {
  return new FileExtractor(spec, normalizeRel(relPath)).run(tree);
}

/** Where the walk currently is, so a declaration knows its qn chain and its containment parent. */
interface WalkScope {
  /** qn segments of the enclosing declarations, outermost first. */
  readonly names: readonly string[];
  /** qn of the enclosing declaration: the file node at the top, then the innermost declaration. */
  readonly parentQn: string;
  /** Structural role of the enclosing declaration, which decides `method` versus `function`. */
  readonly parentRole: DeclarationRole;
  /** The enclosing declaration node itself, for clauses that must be a *direct* child of it. */
  readonly parent: Node;
  /** Decorators of the enclosing `decorated_definition`, handed to the declaration it wraps. */
  readonly decorators: readonly string[];
}

/** The structural roles a scope can have. Reportable kinds such as `test` never reach the scope. */
type DeclarationRole = "file" | "module" | "class" | "function";

class FileExtractor {
  readonly #spec: LangRow;
  readonly #relPath: string;
  readonly #nodes: ExtractedNode[] = [];
  readonly #rawEdges: RawEdge[] = [];
  readonly #unresolved: UnresolvedSite[] = [];
  /**
   * qns already emitted for this file. The graph keys symbols by qn (`UNIQUE (qn)`), so a second
   * declaration of the same name — an overload, a redefinition — is the same symbol: the first
   * declaration keeps the qn and the later one adds no node, because a line suffix would invent a
   * symbol no caller can name.
   */
  readonly #taken = new Set<string>();
  readonly #isTestFile: boolean;

  constructor(spec: LangRow, relPath: string) {
    this.#spec = spec;
    this.#relPath = relPath;
    this.#isTestFile = matchesTestFile(spec, relPath);
  }

  run(tree: Tree): ExtractResult {
    const root = tree.rootNode;
    const fileName = baseName(this.#relPath);
    const fileNode: ExtractedNode = {
      kind: "file",
      name: fileName,
      qn: qualify(this.#relPath, "", fileName),
      startLine: 1,
      endLine: lastLine(root),
      signature: null,
      isTest: this.#isTestFile,
      props: {},
    };
    this.#taken.add(fileNode.qn);
    this.#nodes.push(fileNode);

    let scope: WalkScope = {
      names: [],
      parentQn: fileNode.qn,
      parentRole: "file",
      parent: root,
      decorators: [],
    };

    if (hasRule(this.#spec.id, "fileModule")) {
      const moduleName = stemOf(fileName);
      const moduleQn = this.#emit({
        kind: "module",
        name: moduleName,
        scope: "",
        startLine: 1,
        endLine: lastLine(root),
        signature: null,
        isTest: this.#isTestFile,
        props: {},
      });
      if (moduleQn !== null) {
        this.#pushEdge({
          kind: "contains",
          src: fileNode.qn,
          target: moduleQn,
          receiver: null,
          line: 1,
          evidence: "contains",
          localName: "",
        });
        scope = {
          names: [],
          parentQn: moduleQn,
          parentRole: "module",
          parent: root,
          decorators: [],
        };
      }
    }

    this.#walk(root, scope);

    return { nodes: this.#nodes, rawEdges: this.#rawEdges, unresolved: this.#unresolved };
  }

  /**
   * The walk itself. An explicit stack rather than recursion because a grammar nests by
   * construction: a generated file with ten thousand chained additions is a ten-thousand-deep
   * tree, and a walk that runs out of call stack would lose the whole file to one expression.
   * Children are pushed in reverse so nodes and edges still come out in source order.
   */
  #walk(root: Node, scope: WalkScope): void {
    const stack: { node: Node; scope: WalkScope }[] = [{ node: root, scope }];
    while (stack.length > 0) {
      const frame = stack.pop();
      if (frame === undefined) {
        break;
      }
      const inner = this.#visit(frame.node, frame.scope);
      const children = frame.node.namedChildren;
      for (let index = children.length - 1; index >= 0; index -= 1) {
        const child = children[index];
        if (child !== undefined) {
          stack.push({ node: child, scope: inner });
        }
      }
    }
  }

  /** Handles one node and returns the scope its children are walked in. */
  #visit(node: Node, scope: WalkScope): WalkScope {
    const spec = this.#spec;
    const decorators = decoratorTexts(node);
    const scoped: WalkScope = decorators.length === 0 ? scope : { ...scope, decorators };

    let inner = scoped;
    if (spec.classTypes.includes(node.type)) {
      inner = this.#emitDeclaration(node, scoped, isModuleType(node.type) ? "module" : "class");
    } else if (spec.functionTypes.includes(node.type)) {
      inner = this.#emitDeclaration(node, scoped, "function");
    } else if (this.#isImport(node)) {
      this.#emitImports(node, scope);
      // `require "x"` is an import and a call at once; a plain import statement is not a call.
      if (spec.callTypes.includes(node.type)) {
        this.#emitCall(node, scope);
      }
    } else if (spec.callTypes.includes(node.type)) {
      this.#emitCall(node, scope);
    } else if (spec.inheritanceTypes.includes(node.type)) {
      this.#emitInheritance(node, scope);
    }

    this.#emitReferenceSites(node, scope);
    return inner;
  }

  /**
   * Emits a declaration and its `contains` edge, and returns the scope its children are walked in.
   * A declaration node with no recoverable name (an anonymous class expression) declares nothing
   * nameable, so it is not a node and its children stay in the enclosing scope.
   */
  #emitDeclaration(node: Node, scope: WalkScope, role: DeclarationRole): WalkScope {
    const name = declarationName(node);
    if (name === null) {
      return scope;
    }
    const readsAsTest = TEST_NAME.test(name);
    const isTest = this.#isTestFile || readsAsTest;
    const kind = this.#kindOf(role, scope.parentRole, readsAsTest);
    const qn = this.#emit({
      kind,
      name,
      scope: scope.names.join("."),
      startLine: node.startPosition.row + 1,
      endLine: lastLine(node),
      signature: signatureOf(node),
      isTest,
      props: scope.decorators.length === 0 ? {} : { decorators: [...scope.decorators] },
    });
    if (qn === null) {
      return scope;
    }
    this.#pushEdge({
      kind: "contains",
      src: scope.parentQn,
      target: qn,
      receiver: null,
      line: node.startPosition.row + 1,
      evidence: "contains",
      localName: "",
    });
    return {
      // A file's own module is its namespace, not a prefix: `tests/test_x.py::test_x`, not
      // `::test_x.test_x`. Every other declaration is a segment of the qn chain.
      names:
        role === "module" && hasRule(this.#spec.id, "fileModule")
          ? scope.names
          : [...scope.names, name],
      parentQn: qn,
      parentRole: role,
      parent: node,
      decorators: [],
    };
  }

  #kindOf(role: DeclarationRole, parentRole: DeclarationRole, readsAsTest: boolean): NodeKind {
    if (role === "module") {
      return "module";
    }
    if (readsAsTest) {
      return "test";
    }
    if (role === "class") {
      return "class";
    }
    return parentRole === "class" ? "method" : "function";
  }

  #isImport(node: Node): boolean {
    if (!this.#spec.importTypes.includes(node.type)) {
      return false;
    }
    if (!this.#spec.callTypes.includes(node.type) || !hasRule(this.#spec.id, "callImport")) {
      return true;
    }
    const callee = calleeOf(node);
    const name = callee === null ? null : nameChain(callee);
    if (name === null) {
      return false;
    }
    return REQUIRE_CALLEES.has(splitQualified(name).name);
  }

  #emitImports(node: Node, scope: WalkScope): void {
    const sites = importSites(node, this.#spec);
    const line = node.startPosition.row + 1;
    if (sites.length === 0) {
      this.#unresolved.push({
        line,
        rawTarget: clean(node.text),
        evidenceKind: "dynamic",
        evidence: "import",
      });
      return;
    }
    for (const site of sites) {
      this.#pushEdge({
        kind: "imports",
        src: scope.parentQn,
        target: site.target,
        receiver: null,
        line,
        evidence: `import:${site.target}`,
        localName: site.localName,
      });
    }
  }

  #emitCall(node: Node, scope: WalkScope): void {
    const line = node.startPosition.row + 1;
    const callee = calleeOf(node);
    const written = callee === null ? null : calleeSite(callee);
    if (written === null) {
      this.#unresolved.push({
        line,
        rawTarget: clean(callee?.text ?? node.text),
        evidenceKind: "dynamic",
        evidence: "call",
      });
      return;
    }
    this.#pushEdge({
      kind: "calls",
      src: scope.parentQn,
      target: written.name,
      receiver: written.receiver,
      line,
      evidence: `call:${clean(callee?.text ?? written.name)}`,
      localName: "",
    });
  }

  /**
   * An inheritance clause contributes one edge per base. What makes a base an interface is the
   * clause's own grammar shape (`implements_clause`, `super_interfaces`, …), never the language
   * name: a clause node whose type says "implements" or "interface" produces `implements`, and
   * everything else produces `inherits`. A language with a single mixed clause (C#) therefore
   * reads as `inherits`, which the bind pass can still correct from the target's subkind.
   */
  #emitInheritance(clause: Node, scope: WalkScope): void {
    // A clause only belongs to the class it is a *direct* child of. Python's `argument_list` is a
    // base list and also a decorator's and a call's argument list, so proximity alone would make
    // every decorated method inherit from its arguments.
    if (
      scope.parentRole !== "class" ||
      clause.parent === null ||
      clause.parent.id !== scope.parent.id
    ) {
      return;
    }
    for (const base of clauseBases(clause)) {
      const written = nameChain(base);
      if (written === null) {
        continue;
      }
      const { receiver, name } = splitQualified(written);
      const kind = clauseKind(base, clause);
      this.#pushEdge({
        kind,
        src: scope.parentQn,
        target: name,
        receiver,
        line: base.startPosition.row + 1,
        evidence: kind,
        localName: "",
      });
    }
  }

  /**
   * The value positions of one node. A site container contributes its immediate values only, so a
   * literal nested in another literal is counted by its own site and never twice: `f([a, b])`
   * yields no reference for the list and one per element.
   */
  #emitReferenceSites(node: Node, scope: WalkScope): void {
    const site = referenceSite(node, this.#spec);
    if (site === null) {
      return;
    }
    for (const value of siteValues(node, site)) {
      const written = nameChain(value);
      if (written === null) {
        continue;
      }
      const { receiver, name } = splitQualified(written);
      this.#pushEdge({
        kind: "references",
        src: scope.parentQn,
        target: name,
        receiver,
        line: value.startPosition.row + 1,
        evidence: site,
        localName: "",
      });
    }
  }

  #pushEdge(edge: Omit<RawEdge, "tier">): void {
    this.#rawEdges.push({ ...edge, tier: EXTRACTED_CANDIDATE });
  }

  /** Adds a node, or returns `null` when its qn is already claimed by an earlier declaration. */
  #emit(node: Omit<ExtractedNode, "qn"> & { readonly scope: string }): string | null {
    const qn = qualify(this.#relPath, node.scope, node.name);
    if (this.#taken.has(qn)) {
      return null;
    }
    this.#taken.add(qn);
    this.#nodes.push({
      kind: node.kind,
      name: node.name,
      qn,
      startLine: node.startLine,
      endLine: node.endLine,
      signature: node.signature,
      isTest: node.isTest,
      props: node.props,
    });
    return qn;
  }
}

/** The `testFileRegex` of the row, without the statefulness a user row could introduce with `g`. */
function matchesTestFile(spec: LangRow, relPath: string): boolean {
  const pattern = spec.testFileRegex;
  if (pattern === null) {
    return false;
  }
  pattern.lastIndex = 0;
  return pattern.test(relPath);
}

function hasRule(lang: LangId, rule: ExtractionRule): boolean {
  return EXTRACTION_EXCEPTIONS.some((entry) => entry.lang === lang && entry.rule === rule);
}

function isModuleType(type: string): boolean {
  return MODULE_TYPES.has(type);
}

interface ImportSite {
  readonly target: string;
  readonly localName: string;
}

/**
 * The import shapes of the twelve grammars in one reader: a from-import keeps its module in the
 * `module_name` field (or, for TS and JS, in the `source` string) and binds each remaining name,
 * while a plain import binds each name to the module it names. A call-shaped import (Ruby's
 * `require`) contributes only its argument, never its callee.
 */
function importSites(node: Node, spec: LangRow): ImportSite[] {
  const moduleField = node.childForFieldName("module_name") ?? node.childForFieldName("source");
  const moduleText = moduleField === null ? null : clean(unquote(moduleField.text));
  const callee = spec.callTypes.includes(node.type) ? calleeOf(node) : null;
  const sites: ImportSite[] = [];
  for (const child of node.namedChildren) {
    if (moduleField !== null && child.id === moduleField.id) {
      continue;
    }
    if (callee !== null && child.id === callee.id) {
      continue;
    }
    collectImportSites(child, moduleText, sites);
  }
  if (sites.length === 0 && moduleText !== null) {
    sites.push({ target: moduleText, localName: "" });
  }
  return sites;
}

function collectImportSites(node: Node, moduleText: string | null, out: ImportSite[]): void {
  const nameField = node.childForFieldName("name");
  const aliasField = node.childForFieldName("alias");
  if (nameField !== null) {
    const name = nameChain(nameField) ?? clean(nameField.text);
    if (name !== "") {
      out.push({
        target: moduleText ?? name,
        localName: aliasField === null ? splitQualified(name).name : clean(aliasField.text),
      });
    }
    return;
  }
  if (node.type.includes("string")) {
    const text = unquote(node.text);
    if (text !== "") {
      out.push({ target: moduleText ?? text, localName: "" });
    }
    return;
  }
  const chain = nameChain(node);
  if (chain !== null) {
    out.push({ target: moduleText ?? chain, localName: splitQualified(chain).name });
    return;
  }
  for (const child of node.namedChildren) {
    collectImportSites(child, moduleText, out);
  }
}

/** The command being called, in the field order the grammars use, else the first named child. */
function calleeOf(node: Node): Node | null {
  for (const field of CALLEE_FIELDS) {
    const child = node.childForFieldName(field);
    if (child !== null) {
      return child;
    }
  }
  return node.namedChildren[0] ?? null;
}

/**
 * The name and receiver of a callee. A member access whose object is not itself a name
 * (`PurePosixPath(head).is_absolute`, `sqliteReader!.getNode`) still names the member being
 * called, so its trailing name and its object are recorded instead of the whole site going to
 * `unresolved`: only a callee with no nameable tail (a subscript, a computed call) is nameless.
 */
function calleeSite(callee: Node): { name: string; receiver: string | null } | null {
  const whole = nameChain(callee);
  if (whole !== null) {
    return splitQualified(whole);
  }
  const property =
    callee.childForFieldName("attribute") ??
    callee.childForFieldName("property") ??
    callee.childForFieldName("field");
  if (property === null) {
    return null;
  }
  const name = nameChain(property);
  if (name === null) {
    return null;
  }
  const object = callee.childForFieldName("object") ?? callee.childForFieldName("value");
  return {
    name: splitQualified(name).name,
    receiver: object === null ? null : textOrNull(clean(object.text)),
  };
}

/** The name of a declaration: its `name` field, a wrapped spec/declarator, or a single name child. */
function declarationName(node: Node): string | null {
  const named = node.childForFieldName("name");
  if (named !== null) {
    return textOrNull(nameChain(named) ?? clean(named.text));
  }
  for (const child of node.namedChildren) {
    if (DECLARATOR_TYPES.test(child.type)) {
      const nested = declarationName(child);
      if (nested !== null) {
        return nested;
      }
    }
  }
  const names = node.namedChildren.filter((child) => NAME_TYPES.has(child.type));
  return names.length === 1 ? textOrNull(clean(names[0]?.text ?? "")) : null;
}

/**
 * The bases of an inheritance clause: the name nodes at most two levels down, so a clause that
 * wraps a field list (Go's struct body) contributes nothing rather than every field's type.
 */
function clauseBases(clause: Node): readonly Node[] {
  const bases: Node[] = [];
  collectClauseBases(clause, 0, bases);
  return bases;
}

function collectClauseBases(node: Node, depth: number, out: Node[]): void {
  if (depth > 2) {
    return;
  }
  for (const child of node.namedChildren) {
    if (NAME_TYPES.has(child.type) || CHAIN_TYPES.has(child.type)) {
      if (!out.some((seen) => seen.id === child.id)) {
        out.push(child);
      }
      continue;
    }
    collectClauseBases(child, depth + 1, out);
  }
}

function clauseKind(base: Node, clause: Node): "inherits" | "implements" {
  for (let node: Node | null = base; node !== null; node = node.parent) {
    if (/implements|interface/u.test(node.type)) {
      return "implements";
    }
    if (node.id === clause.id) {
      break;
    }
  }
  return "inherits";
}

/** The reference site a node is a container for, or `null` when it holds no value position. */
function referenceSite(node: Node, spec: LangRow): ReferenceSite | null {
  if (ARGUMENT_CONTAINERS.has(node.type)) {
    const parent = node.parent;
    if (parent === null) {
      return null;
    }
    if (isDecoratorType(parent.type)) {
      return "decorator_argument";
    }
    // A class's base list is an `argument_list` too (Python); it is an inheritance clause, not a
    // value position, so only a call's argument list counts here.
    if (!spec.callTypes.includes(parent.type)) {
      return null;
    }
    return parent.parent !== null && isDecoratorType(parent.parent.type)
      ? "decorator_argument"
      : "call_argument";
  }
  if (ARRAY_LITERALS.has(node.type)) {
    return "array_element";
  }
  if (OBJECT_LITERALS.has(node.type)) {
    return "object_element";
  }
  if (RETURN_TYPES.has(node.type)) {
    return "return_expression";
  }
  if (ASSIGNMENT_TYPES.has(node.type)) {
    return node.childForFieldName("right") === null ? null : "assignment_rhs";
  }
  return null;
}

/** The values of one site: arguments minus keyword names, pair values minus keys, return operands. */
function siteValues(node: Node, site: ReferenceSite): readonly Node[] {
  switch (site) {
    case "call_argument":
    case "decorator_argument":
    case "object_element":
      return node.namedChildren.map((child) => child.childForFieldName("value") ?? child);
    case "assignment_rhs": {
      const right = node.childForFieldName("right");
      if (right === null) {
        return [];
      }
      return right.type.endsWith("expression_list") ? right.namedChildren : [right];
    }
    case "array_element":
    case "return_expression":
      return node.namedChildren;
  }
}

function isDecoratorType(type: string): boolean {
  return type.includes("decorator") || type === "annotation" || type === "marker_annotation";
}

/** The decorator texts of a node that wraps one or more declarations (`@route(...)` without `@`). */
function decoratorTexts(node: Node): readonly string[] {
  const texts: string[] = [];
  for (const child of node.namedChildren) {
    if (isDecoratorType(child.type)) {
      texts.push(clean(child.text).replace(/^@/u, ""));
    }
  }
  return texts;
}

/**
 * The text of a name or of a name chain, or `null` when the expression is not a name at all
 * (a literal, a call, a subscript). The chain keeps its own separators; every part must itself be
 * a name, so `f().x` is not a name chain.
 */
function nameChain(node: Node): string | null {
  if (NAME_TYPES.has(node.type)) {
    return textOrNull(clean(node.text));
  }
  if (!CHAIN_TYPES.has(node.type)) {
    return null;
  }
  const children = node.namedChildren;
  if (children.length === 0) {
    return null;
  }
  for (const child of children) {
    if (nameChain(child) === null) {
      return null;
    }
  }
  return textOrNull(clean(node.text));
}

/** `self.paint` → `{receiver: "self", name: "paint"}`; `f` → `{receiver: null, name: "f"}`. */
function splitQualified(text: string): { receiver: string | null; name: string } {
  const parts = text.split(/\.|::|->|\//u).filter((part) => part !== "");
  const name = parts.at(-1) ?? text;
  return { receiver: parts.length > 1 ? parts.slice(0, -1).join(".") : null, name };
}

/** A declaration's header: its text up to the body, so a multi-line signature keeps its own rows. */
function signatureOf(node: Node): string | null {
  const body = node.childForFieldName("body");
  if (body === null) {
    return textOrNull(clean(node.text.split("\n")[0] ?? ""));
  }
  const bodyRow = body.startPosition.row - node.startPosition.row;
  const rows = node.text.split("\n").slice(0, Math.max(1, bodyRow));
  if (bodyRow === 0 && rows[0] !== undefined) {
    // The body starts on the signature's own row (`area() { return 1; }`), so the signature ends
    // where the body begins. A byte column only equals a string index on ASCII, so a row with
    // anything outside printable ASCII keeps its whole text rather than a cut that could land
    // inside a character.
    const row = rows[0];
    rows[0] = /^[ -~]*$/u.test(row) ? row.slice(0, body.startPosition.column) : row;
  }
  return textOrNull(clean(rows.join(" "), SIGNATURE_CAP));
}

/** The last line a node covers; a node ending at column 0 does not own the next row. */
function lastLine(node: Node): number {
  const row = node.endPosition.row;
  return node.endPosition.column === 0 && row > 0 ? row : row + 1;
}

/** Whitespace collapsed, trimmed and capped, so a signature is one line and a value stays small. */
function clean(text: string, cap: number = TEXT_CAP): string {
  const collapsed = text.replace(/\s+/gu, " ").trim();
  return collapsed.length > cap ? collapsed.slice(0, cap) : collapsed;
}

function unquote(text: string): string {
  return clean(text)
    .replace(/^[a-z]{0,2}["'`]/iu, "")
    .replace(/["'`]$/u, "");
}

function textOrNull(text: string): string | null {
  return text === "" ? null : text;
}

function baseName(relPath: string): string {
  return relPath.split("/").at(-1) ?? relPath;
}

function stemOf(fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  return dot > 0 ? fileName.slice(0, dot) : fileName;
}
