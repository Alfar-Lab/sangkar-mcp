/**
 * The bind pass: the only place a raw edge becomes a real one.
 *
 * `extract.ts` leaves every edge but containment unbound — an `extracted_candidate` carrying a
 * textual `target`. This module turns those candidates into edges whose two ends are qns that exist
 * in the `SymbolIndex`, or records why it could not. A site that binds nothing is a fact about the
 * graph ("we could not prove it") and belongs in `unresolved`; a site that binds two things is a
 * fact we refuse to guess at, because a spurious edge is worse than a missing one.
 *
 * Two rules shape everything here:
 * - the binding order is fixed (same file, then import evidence, then a globally unique name, then
 *   an inherited method), and the tier is the evidence that won: `extracted` when the declaration
 *   was found structurally, `inferred` when it was found by name;
 * - no edge is ever emitted whose `src` or `dst` is not a node in the index. `persist.ts` therefore
 *   inserts without a second existence check, and the foreign keys cannot be violated by binding.
 *
 * Nothing here reads a file, opens a connection or knows a language: a `FileUnit` is the output of
 * one `extractFile` call plus the path it came from, so the pass is a pure function of its input.
 */

import { normalizeRel, qn as qualify, resolveFrom } from "#shared/paths";
import type {
  ExtractedNode,
  ExtractResult,
  NodeKind,
  RawEdge,
  RawEdgeKind,
  UnresolvedSite,
} from "./extract";

/** The two tiers a bound edge can have. `extracted_candidate` never survives this pass. */
export const EDGE_TIERS = ["extracted", "inferred"] as const;

export type EdgeTier = (typeof EDGE_TIERS)[number];

/**
 * Why a call, an import or an inheritance base could not be bound. `dynamic` is the extractor's own
 * kind (the source offered no name), kept here so an extractor site passes through unchanged.
 *
 * `missing_module` is the read side's only way to tell a broken relative import (a file that is
 * not in the tree) from a module this resolver was never meant to resolve (a package, `node:fs`,
 * or a `#` tsconfig alias): the specifier already said which one it was, and recording both as
 * `dynamic` made a real defect indistinguishable from an out-of-tree module.
 */
export const BIND_EVIDENCE_KINDS = [
  "receiver_type_unknown",
  "ambiguous",
  "dynamic",
  "missing_module",
] as const;

export type BindEvidenceKind = (typeof BIND_EVIDENCE_KINDS)[number];

/**
 * Everything one file contributes to the graph before anything is bound. `relPath` is repo-relative
 * and already normalized, and the extractor's own `unresolved` sites ride along so the pass is the
 * single producer of the final unresolved list.
 */
export interface FileUnit {
  readonly relPath: string;
  readonly nodes: readonly ExtractedNode[];
  readonly rawEdges: readonly RawEdge[];
  readonly unresolved: readonly UnresolvedSite[];
}

/** Pairs one extractor result with its path, so a caller never assembles a unit by hand. */
export function fileUnit(relPath: string, result: ExtractResult): FileUnit {
  return {
    relPath: normalizeRel(relPath),
    nodes: result.nodes,
    rawEdges: result.rawEdges,
    unresolved: result.unresolved,
  };
}

/**
 * One resolved edge, keyed by qn on both ends. `file` is the unit the site came from, so persist
 * can find the `file_id` without re-deriving it from `src` — a call edge's source can be a symbol
 * the site does not sit in once binding jumps to a superclass.
 */
export interface ResolvedEdge {
  readonly file: string;
  readonly kind: RawEdgeKind;
  readonly src: string;
  readonly dst: string;
  readonly tier: EdgeTier;
  readonly line: number;
  readonly localName: string;
  readonly evidence: string;
}

/** One site this pass could not bind, with the reason. `rawTarget` is the name as written. */
export interface UnresolvedBinding {
  readonly file: string;
  readonly line: number;
  readonly rawTarget: string;
  readonly evidenceKind: BindEvidenceKind;
  readonly evidence: string;
}

export interface BindResult {
  readonly edges: readonly ResolvedEdge[];
  readonly unresolved: readonly UnresolvedBinding[];
}

/** A base named by an inheritance clause, and the file whose import table could type it. */
export interface BaseReference {
  readonly name: string;
  readonly file: string;
}

/** One name an import statement makes visible in a file: `local_name = import(module)`. */
export interface ImportBinding {
  readonly module: string;
  readonly localName: string;
}

/**
 * Reportable symbols, the only nodes a name can bind to. `file`, `module` and `endpoint` are
 * deliberately excluded: a call to `base` must not resolve to the module node `base`, and an
 * endpoint is a route, not a callee.
 */
const DECLARATION_KINDS: ReadonlySet<NodeKind> = new Set(["class", "function", "method", "test"]);

/** Kinds an inheritance base may name: a class, or a namespace standing in for an interface. */
const TYPE_KINDS: ReadonlySet<NodeKind> = new Set(["class", "module"]);

/** Receivers that name the enclosing class rather than a value with a type of its own. */
const SELF_RECEIVERS: ReadonlySet<string> = new Set(["self", "this"]);

/** Stems of the files a directory import lands in, per the module systems of the 12 grammars. */
const INDEX_MODULE_STEMS: ReadonlySet<string> = new Set(["index", "__init__", "mod"]);

/**
 * The whole graph's symbol table, built once from the units. Construction owns every derived table
 * the binding order needs — per-file declarations, per-file import tables, inheritance bases and
 * the extensionless path of each unit — so no caller has to assemble index internals.
 */
export class SymbolIndex {
  readonly #nodes = new Map<string, ExtractedNode>();
  /** Rel path → qns of that file's nodes, in declaration order. */
  readonly #byFile = new Map<string, string[]>();
  /** Name → qns of the reportable declarations carrying it, across the whole index. */
  readonly #byName = new Map<string, string[]>();
  /** Rel path → local name → the import bindings that make it visible. */
  readonly #imports = new Map<string, Map<string, ImportBinding[]>>();
  /** Class qn → the bases its inheritance clauses named, in source order. */
  readonly #bases = new Map<string, BaseReference[]>();
  /** Extensionless rel path → the units it could name, for module-string resolution. */
  readonly #stems = new Map<string, string[]>();
  /** `specifier\0importer` → the units it could name, so two importers never share an entry. */
  readonly #candidates = new Map<string, readonly string[]>();

  private constructor(units: readonly FileUnit[]) {
    for (const unit of units) {
      const relPath = normalizeRel(unit.relPath);
      const qns: string[] = [];
      for (const node of unit.nodes) {
        qns.push(node.qn);
        if (!this.#nodes.has(node.qn)) {
          this.#nodes.set(node.qn, node);
        }
        if (DECLARATION_KINDS.has(node.kind)) {
          push(this.#byName, node.name, node.qn);
        }
      }
      this.#byFile.set(relPath, qns);

      const stem = stemOf(relPath);
      push(this.#stems, stem, relPath);
      const slash = stem.lastIndexOf("/");
      const base = slash === -1 ? stem : stem.slice(slash + 1);
      if (slash !== -1 && INDEX_MODULE_STEMS.has(base)) {
        push(this.#stems, stem.slice(0, slash), relPath);
      }
    }

    for (const unit of units) {
      const relPath = normalizeRel(unit.relPath);
      const table = new Map<string, ImportBinding[]>();
      for (const edge of unit.rawEdges) {
        if (edge.kind === "imports") {
          // `import os` records no local name; it binds `os`, the module's own last segment.
          const localName = edge.localName === "" ? lastSegment(edge.target) : edge.localName;
          if (localName !== "") {
            push(table, localName, { module: edge.target, localName });
          }
        } else if (edge.kind === "inherits" || edge.kind === "implements") {
          push(this.#bases, edge.src, { name: edge.target, file: relPath });
        }
      }
      this.#imports.set(relPath, table);
    }
  }

  /** Builds the index from the units about to be bound. The only public way in. */
  static from(units: readonly FileUnit[]): SymbolIndex {
    return new SymbolIndex(units);
  }

  node(qn: string): ExtractedNode | undefined {
    return this.#nodes.get(qn);
  }

  /** The declarations one file holds, in source order. */
  nodesIn(relPath: string): readonly ExtractedNode[] {
    return this.#materialize(this.#byFile.get(normalizeRel(relPath)) ?? []);
  }

  /** Every reportable declaration in the graph carrying `name`. */
  nodesNamed(name: string): readonly ExtractedNode[] {
    return this.#materialize(this.#byName.get(name) ?? []);
  }

  /** The unit's own file node; every `imports` edge targets one of these. */
  fileNode(relPath: string): ExtractedNode | undefined {
    return this.nodesIn(relPath).find((node) => node.kind === "file");
  }

  /** The import table of one file: which module each visible name came from. */
  importsOf(relPath: string): ReadonlyMap<string, readonly ImportBinding[]> {
    return this.#imports.get(normalizeRel(relPath)) ?? new Map<string, readonly ImportBinding[]>();
  }

  /** The bases a class's inheritance clauses named, whether or not they resolved. */
  basesOf(qn: string): readonly BaseReference[] {
    return this.#bases.get(qn) ?? [];
  }

  /**
   * Every unit a module string could name, sorted. `[]` is "nothing matched" and two or more paths
   * are "the stem is not enough to decide", which are different facts: the first is a module the
   * resolver cannot see (a package, a stdlib import), the second is a tree defect the caller can
   * report along with the competitors. Python's `a.b` and TypeScript's `./a/b` both become `a/b`;
   * `index` and `__init__` answering for their directory is the whole of directory-import support.
   *
   * `fromFile` is the importing unit, and for a relative specifier it is what decides the meaning:
   * `./x.js` from `src/a/b.ts` is the stem `src/a/x`, so that exact stem is searched before the
   * global suffix search below — without it every `./y` would tie between all the units whose stem
   * happens to end in `y`. An absolute or escaping specifier resolves to nothing and falls through
   * to the module-string reading, which is what keeps `../../../x` from binding on a coincidence.
   */
  moduleCandidates(module: string, fromFile?: string): readonly string[] {
    const cacheKey = `${module}\u0000${fromFile ?? ""}`;
    const cached = this.#candidates.get(cacheKey);
    if (cached !== undefined) {
      return cached;
    }
    const relative = fromFile !== undefined && isRelativeSpecifier(module);
    const local = this.#matchingStems(relative ? resolveFrom(fromFile, module) : null);
    const found = local.length > 0 ? local : this.#matchingStems(modulePath(module));
    this.#candidates.set(cacheKey, found);
    return found;
  }

  /**
   * The unit a module string names, or `null` when the string matches no unit or more than one —
   * a module string that matches two files cannot be resolved and must not be guessed. Expressed
   * over {@link moduleCandidates} so the two can never disagree about which units are in play.
   */
  moduleFile(module: string, fromFile?: string): string | null {
    const candidates = this.moduleCandidates(module, fromFile);
    return candidates.length === 1 ? (candidates[0] ?? null) : null;
  }

  /**
   * The units an extensionless rel path could name, sorted, or `[]` for no stem at all. An exact
   * single entry wins outright; otherwise the suffix match collects every unit the stem could be
   * the tail of.
   */
  #matchingStems(key: string | null): readonly string[] {
    if (key === null || key === "") {
      return [];
    }
    const found: string[] = [];
    const exact = this.#stems.get(key);
    if (exact !== undefined && exact.length === 1) {
      found.push(...exact);
    } else {
      const suffix = `/${key}`;
      const matches = new Set<string>();
      for (const [stem, paths] of this.#stems) {
        if (stem === key || stem.endsWith(suffix)) {
          for (const path of paths) {
            matches.add(path);
          }
        }
      }
      for (const path of matches) {
        found.push(path);
      }
    }
    // Sorted because the candidates end up in evidence text, and two runs over the same tree must
    // produce byte-identical rows.
    found.sort();
    return found;
  }

  #materialize(qns: readonly string[]): readonly ExtractedNode[] {
    const out: ExtractedNode[] = [];
    for (const qn of qns) {
      const node = this.#nodes.get(qn);
      if (node !== undefined) {
        out.push(node);
      }
    }
    return out;
  }
}

/**
 * Binds every unit against one index. Edges come out in unit order and, inside a unit, in raw-edge
 * order, so two runs over the same input are byte-identical; duplicate edges (the same site seen
 * twice) are collapsed on the key the `edge` table is unique on.
 */
export function bind(units: readonly FileUnit[], index: SymbolIndex): BindResult {
  const edges: ResolvedEdge[] = [];
  const unresolved: UnresolvedBinding[] = [];
  const seen = new Set<string>();

  const emit = (edge: ResolvedEdge): void => {
    // The invariant this pass exists to keep: an edge never dangles.
    if (index.node(edge.src) === undefined || index.node(edge.dst) === undefined) {
      return;
    }
    const key = [edge.file, edge.kind, edge.src, edge.dst, String(edge.line), edge.localName].join(
      "\u0000",
    );
    if (seen.has(key)) {
      return;
    }
    seen.add(key);
    edges.push(edge);
  };

  for (const unit of units) {
    const context: BindContext = { file: normalizeRel(unit.relPath), index, emit, unresolved };
    for (const site of unit.unresolved) {
      unresolved.push({
        file: context.file,
        line: site.line,
        rawTarget: site.rawTarget,
        evidenceKind: site.evidenceKind,
        evidence: site.evidence,
      });
    }
    for (const raw of unit.rawEdges) {
      bindRaw(context, raw);
    }
  }

  return { edges, unresolved };
}

interface BindContext {
  readonly file: string;
  readonly index: SymbolIndex;
  readonly emit: (edge: ResolvedEdge) => void;
  readonly unresolved: UnresolvedBinding[];
}

/** One raw edge, by kind: containment and inheritance are structural, calls and imports are names. */
function bindRaw(context: BindContext, raw: RawEdge): void {
  switch (raw.kind) {
    case "contains": {
      // The extractor already wrote the child's qn, so containment is evidence, not inference.
      context.emit(edge(context, raw, raw.target, "extracted"));
      return;
    }
    case "inherits":
    case "implements": {
      const resolved = resolveTypeName(context.file, raw.target, context.index);
      if (resolved.kind === "hit") {
        context.emit(edge(context, raw, resolved.qn, "extracted"));
        return;
      }
      context.unresolved.push(unresolvedOf(context, raw, typeEvidence(resolved)));
      return;
    }
    case "references": {
      // Exactly one node, or nothing: a reference is a value position with no receiver evidence,
      // so two candidates are indistinguishable and the edge is dropped *silently* — recording it
      // would claim a name was unresolvable when the name is merely non-unique.
      const hits = context.index.nodesNamed(raw.target);
      const only = hits[0];
      if (hits.length === 1 && only !== undefined) {
        context.emit(edge(context, raw, only.qn, "inferred"));
      }
      return;
    }
    case "calls": {
      const outcome = bindCall(context.file, raw, context.index);
      if (outcome.kind === "edge") {
        context.emit(edge(context, raw, outcome.dst, outcome.tier));
        return;
      }
      context.unresolved.push(unresolvedOf(context, raw, outcome.evidenceKind));
      return;
    }
    case "imports": {
      // A module string names a unit, not a declaration: the same-file, unique-name and
      // inheritance steps have nothing to look at, so an import binds to the target file node at
      // `extracted`, and the reason a failed one failed is read off the candidate set.
      const candidates = context.index.moduleCandidates(raw.target, context.file);
      const only = candidates[0];
      if (candidates.length === 1 && only !== undefined) {
        const target = context.index.fileNode(only);
        if (target !== undefined) {
          context.emit(edge(context, raw, target.qn, "extracted"));
          return;
        }
      }
      context.unresolved.push(importFailure(context, raw, candidates));
      return;
    }
  }
}

/** The four-step binding order, returning either a target qn or the reason there is none. */
function bindCall(file: string, raw: RawEdge, index: SymbolIndex): CallOutcome {
  // 1. Exact qn in the same file: the strongest evidence there is, so it is checked first and an
  //    ambiguity here ends the search rather than weakening to a wider guess.
  const same = sameFileHits(file, raw, index);
  const sameOne = same[0];
  if (same.length === 1 && sameOne !== undefined) {
    return { kind: "edge", dst: sameOne, tier: "extracted" };
  }
  if (same.length > 1) {
    return { kind: "unresolved", evidenceKind: "ambiguous" };
  }

  // 2. A name this file imported: the import statement is the evidence, so it is still `extracted`.
  const imported = importHits(file, raw.target, index);
  const importedOne = imported[0];
  if (imported.length === 1 && importedOne !== undefined) {
    return { kind: "edge", dst: importedOne, tier: "extracted" };
  }
  if (imported.length > 1) {
    return { kind: "unresolved", evidenceKind: "ambiguous" };
  }

  // 3. A name that occurs exactly once in the whole graph: unique, therefore `inferred`.
  const global = index.nodesNamed(raw.target);
  const globalOne = global[0];
  if (global.length === 1 && globalOne !== undefined) {
    return { kind: "edge", dst: globalOne.qn, tier: "inferred" };
  }

  // 4. A superclass method for a receiver whose class we could establish: still `inferred`,
  //    because the receiver's type came from the class's own declaration, not from the call site.
  const inherited = inheritedHits(file, raw, index);
  const inheritedOne = inherited[0];
  if (inherited.length === 1 && inheritedOne !== undefined) {
    return { kind: "edge", dst: inheritedOne, tier: "inferred" };
  }
  if (inherited.length > 1) {
    return { kind: "unresolved", evidenceKind: "ambiguous" };
  }

  return { kind: "unresolved", evidenceKind: fallbackEvidence(raw.receiver, global.length) };
}

type CallOutcome =
  | { readonly kind: "edge"; readonly dst: string; readonly tier: EdgeTier }
  | { readonly kind: "unresolved"; readonly evidenceKind: BindEvidenceKind };

/**
 * Why a call stayed unbound. An untyped receiver is the root cause whenever one was written: the
 * candidates may well differ by type, which is exactly what we could not establish. Without a
 * receiver the reason is the candidate set itself — several matches, or none at all.
 */
function fallbackEvidence(receiver: string | null, globalHits: number): BindEvidenceKind {
  if (receiver !== null) {
    return "receiver_type_unknown";
  }
  return globalHits > 1 ? "ambiguous" : "dynamic";
}

/** Step 1: the target's exact qn, using the receiver as a scope when it names one. */
function sameFileHits(file: string, raw: RawEdge, index: SymbolIndex): string[] {
  const receiver = raw.receiver;
  if (receiver !== null) {
    const scope = receiverScope(raw.src, receiver, index);
    if (scope === null) {
      return [];
    }
    const exact = qualify(file, scope, raw.target);
    return index.node(exact) === undefined ? [] : [exact];
  }
  return index
    .nodesIn(file)
    .filter((node) => node.name === raw.target && DECLARATION_KINDS.has(node.kind))
    .map((node) => node.qn);
}

/**
 * The qn scope a receiver names inside `src`. `self`/`this` mean the enclosing declaration's scope,
 * which for a method qn (`path::Class.method`) is the class; any other receiver is taken as the
 * scope as written, so `Widget.build()` finds `path::Widget.build` and a variable simply misses.
 */
function receiverScope(src: string, receiver: string, index: SymbolIndex): string | null {
  if (!SELF_RECEIVERS.has(receiver)) {
    return receiver;
  }
  const node = index.node(src);
  if (node === undefined || node.kind !== "method") {
    return null;
  }
  const scope = scopeOf(node.qn);
  return scope === "" ? null : scope;
}

/** Step 2: a name visible through this file's import table, resolved into the importing unit. */
function importHits(file: string, name: string, index: SymbolIndex): string[] {
  const bindings = index.importsOf(file).get(name);
  if (bindings === undefined) {
    return [];
  }
  const out: string[] = [];
  for (const binding of bindings) {
    const moduleRel = index.moduleFile(binding.module, file);
    if (moduleRel === null) {
      continue;
    }
    for (const node of index.nodesIn(moduleRel)) {
      if (node.name === binding.localName && DECLARATION_KINDS.has(node.kind)) {
        out.push(node.qn);
      }
    }
  }
  return out;
}

/**
 * Step 4: the methods a receiver's class inherits under the target's name. The chain is walked by
 * name because that is all an inheritance clause records, and it is cycle-safe: a class already
 * seen is never expanded again.
 */
function inheritedHits(file: string, raw: RawEdge, index: SymbolIndex): string[] {
  const start = receiverClass(file, raw, index);
  if (start === null) {
    return [];
  }
  const out: string[] = [];
  const seen = new Set<string>([start.qn]);
  let frontier: BaseReference[] = [...index.basesOf(start.qn)];
  while (frontier.length > 0) {
    const next: BaseReference[] = [];
    for (const base of frontier) {
      const ancestor = resolveClass(base.name, base.file, index);
      if (ancestor === null || seen.has(ancestor.qn)) {
        continue;
      }
      seen.add(ancestor.qn);
      for (const method of methodsOf(ancestor, index)) {
        if (method.name === raw.target) {
          out.push(method.qn);
        }
      }
      next.push(...index.basesOf(ancestor.qn));
    }
    frontier = next;
  }
  return out;
}

/** The class a receiver names, or `null` when its type cannot be established from the graph. */
function receiverClass(file: string, raw: RawEdge, index: SymbolIndex): ExtractedNode | null {
  const receiver = raw.receiver;
  if (receiver === null) {
    return null;
  }
  if (SELF_RECEIVERS.has(receiver)) {
    const node = index.node(raw.src);
    if (node === undefined || node.kind !== "method") {
      return null;
    }
    const scope = scopeOf(node.qn);
    return scope === "" ? null : (index.node(`${file}::${scope}`) ?? null);
  }
  return resolveClass(receiver, file, index);
}

/** A class or namespace by name: the same file first, then a name unique in the whole index. */
function resolveClass(name: string, file: string, index: SymbolIndex): ExtractedNode | null {
  const local = index
    .nodesIn(file)
    .filter((node) => node.name === name && TYPE_KINDS.has(node.kind));
  const localOne = local[0];
  if (local.length === 1 && localOne !== undefined) {
    return localOne;
  }
  if (local.length > 1) {
    return null;
  }
  const global = index.nodesNamed(name).filter((node) => TYPE_KINDS.has(node.kind));
  const globalOne = global[0];
  return global.length === 1 && globalOne !== undefined ? globalOne : null;
}

/** The direct methods of one class: qns one segment below the class's own qn. */
function methodsOf(cls: ExtractedNode, index: SymbolIndex): readonly ExtractedNode[] {
  const prefix = `${cls.qn}.`;
  return index
    .nodesIn(fileOf(cls.qn))
    .filter(
      (node) =>
        node.kind === "method" &&
        node.qn.startsWith(prefix) &&
        !node.qn.slice(prefix.length).includes("."),
    );
}

type NameOutcome =
  | { readonly kind: "hit"; readonly qn: string }
  | { readonly kind: "ambiguous" }
  | { readonly kind: "missing" };

/** An inheritance base or an `implements` entry: a type name, resolved like any other declaration. */
function resolveTypeName(file: string, name: string, index: SymbolIndex): NameOutcome {
  const local = index
    .nodesIn(file)
    .filter((node) => node.name === name && TYPE_KINDS.has(node.kind));
  const localOne = local[0];
  if (local.length === 1 && localOne !== undefined) {
    return { kind: "hit", qn: localOne.qn };
  }
  if (local.length > 1) {
    return { kind: "ambiguous" };
  }
  const global = index.nodesNamed(name).filter((node) => TYPE_KINDS.has(node.kind));
  const globalOne = global[0];
  if (global.length === 1 && globalOne !== undefined) {
    return { kind: "hit", qn: globalOne.qn };
  }
  return global.length > 1 ? { kind: "ambiguous" } : { kind: "missing" };
}

/** A type that exists nowhere in the indexed tree is outside the graph, which is `dynamic`. */
function typeEvidence(outcome: NameOutcome): BindEvidenceKind {
  return outcome.kind === "ambiguous" ? "ambiguous" : "dynamic";
}

function edge(context: BindContext, raw: RawEdge, dst: string, tier: EdgeTier): ResolvedEdge {
  return {
    file: context.file,
    kind: raw.kind,
    src: raw.src,
    dst,
    tier,
    line: raw.line,
    localName: raw.localName,
    evidence: raw.evidence,
  };
}

/**
 * Why an import did not bind, from the candidate set and the specifier's shape.
 *
 * A tie is `ambiguous` and names the competitors in the evidence, because the fix lives in the tree
 * (two files sharing a stem) and a count alone would not say which. A relative specifier that
 * matched nothing is `missing_module`: the file it points at is not in the tree. Anything else is
 * `dynamic`, unchanged — a bare package, `node:`/`bun:`, or a `#` alias, which is a tsconfig path
 * mapping this resolver cannot see and therefore must not report as a defect in the tree.
 */
function importFailure(
  context: BindContext,
  raw: RawEdge,
  candidates: readonly string[],
): UnresolvedBinding {
  if (candidates.length > 1) {
    return {
      ...unresolvedOf(context, raw, "ambiguous"),
      evidence: `${raw.evidence} candidates: ${candidates.join(" | ")}`,
    };
  }
  if (candidates.length === 0 && isRelativeSpecifier(raw.target)) {
    return {
      ...unresolvedOf(context, raw, "missing_module"),
      evidence: `${raw.evidence} (no file matched)`,
    };
  }
  // One candidate with no file node cannot happen (every unit carries one); it stays `dynamic`
  // rather than claiming a defect, so the fallthrough is honest instead of assumed.
  return unresolvedOf(context, raw, "dynamic");
}

/** The two forms that mean "a path in this tree", so a miss is a defect rather than an import. */
function isRelativeSpecifier(module: string): boolean {
  return module.startsWith("./") || module.startsWith("../");
}

function unresolvedOf(
  context: BindContext,
  raw: RawEdge,
  evidenceKind: BindEvidenceKind,
): UnresolvedBinding {
  return {
    file: context.file,
    line: raw.line,
    rawTarget: raw.target,
    evidenceKind,
    evidence: raw.evidence,
  };
}

function push<K, V>(map: Map<K, V[]>, key: K, value: V): void {
  const bucket = map.get(key);
  if (bucket === undefined) {
    map.set(key, [value]);
    return;
  }
  bucket.push(value);
}

/** `path::A.b` → `A`; `path::x` → `""`. The scope is what a same-file exact lookup composes. */
function scopeOf(qn: string): string {
  const separator = qn.indexOf("::");
  if (separator === -1) {
    return "";
  }
  const local = qn.slice(separator + 2);
  const dot = local.lastIndexOf(".");
  return dot === -1 ? "" : local.slice(0, dot);
}

/** `path::A.b` → `path`. */
function fileOf(qn: string): string {
  const separator = qn.indexOf("::");
  return separator === -1 ? qn : qn.slice(0, separator);
}

function lastSegment(text: string): string {
  const parts = text.split(/[./\\:]/u).filter((part) => part !== "");
  return parts.at(-1) ?? "";
}

/** `src/a/b.py` → `src/a/b`; a name with no extension is left alone. */
function stemOf(relPath: string): string {
  const slash = relPath.lastIndexOf("/");
  const dot = relPath.lastIndexOf(".");
  return dot > slash ? relPath.slice(0, dot) : relPath;
}

/**
 * A module string as repository-relative path segments: `a.b` → `a/b`, `./a/b` → `a/b`,
 * `src/helpers.rb` → `src/helpers`. Only the first form needs the dot rewrite, and only the second
 * needs the relative prefix stripped, so the two cases are decided by the separator they already
 * carry.
 */
function modulePath(module: string): string {
  const slashed = module.replace(/\\/gu, "/");
  if (slashed.includes("/")) {
    return normalizeRel(slashed).replace(/\.[A-Za-z0-9]+$/u, "");
  }
  return module.replace(/\./gu, "/");
}
