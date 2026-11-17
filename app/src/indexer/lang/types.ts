/**
 * Language support is data (TECH-SPEC.md §9): one row per grammar plus the node-type lists that
 * decide which syntax nodes become graph nodes and which become edges. No parser code branches on a
 * language name; adding a language is a row in `registry.ts`.
 */

/** The 12 languages of the v1 MVP. */
export type LangId =
  | "python"
  | "typescript"
  | "tsx"
  | "javascript"
  | "go"
  | "java"
  | "rust"
  | "c"
  | "cpp"
  | "csharp"
  | "php"
  | "ruby";

/**
 * One language row as declared. Arrays are readonly views because a row is reference data, but the
 * shape stays a plain object so a user's `.codegraph/languages.toml` (GRAPH-MODEL.md) can produce
 * the same values; `registry.ts` serves the resolved `LangRow` the parser consumes.
 */
export interface LangSpec {
  readonly id: LangId;
  /** Source extensions, leading dot and lowercase: `.ts`, never `ts` or `.TS`. */
  readonly extensions: readonly string[];
  /** File name inside `tree-sitter-wasms/out`. The directory belongs to the parser, not this table. */
  readonly grammarWasm: string;
  /** Nodes that produce a class node: a type declaration, not only a runtime class. */
  readonly classTypes: readonly string[];
  /** Nodes that produce a function node: functions, methods, constructors, arrow functions. */
  readonly functionTypes: readonly string[];
  /** Nodes that produce an import edge. */
  readonly importTypes: readonly string[];
  /** Nodes that produce a call edge; the callee name still has to be resolved later. */
  readonly callTypes: readonly string[];
  /**
   * Grammar nodes that carry a superclass or interface clause. This names the node holding the
   * clause, not the base name's node: TypeScript wraps `extends_clause`/`implements_clause` in
   * `class_heritage`, and a row with no such construct in the language records an empty list — C
   * has no inheritance at all.
   */
  readonly inheritanceTypes: readonly string[];
  /** Matches a repo-relative path (forward slashes) that is a test file for this language. */
  readonly testFileRegex?: RegExp;
}

/**
 * A `LangSpec` after `registry.ts` resolved it: the optional test pattern is materialised as a value
 * so a caller never distinguishes "absent" from "not set", and the object is what the parser holds.
 */
export interface LangRow extends Omit<LangSpec, "testFileRegex"> {
  readonly testFileRegex: RegExp | null;
}
