import { extname } from "node:path";
import type { LangId, LangRow } from "./types";

/**
 * The v1 language table. Key order is the extension map's build order; within a row the extension
 * order is the one a user sees first. `grammarWasm` names a file inside `tree-sitter-wasms/out` so
 * that a test loads exactly the grammar the parser will load.
 *
 * The node-type lists follow the reference parser's tables (code-review-graph's `_CLASS_TYPES`,
 * `_FUNCTION_TYPES`, `_IMPORT_TYPES`, `_CALL_TYPES`) where this grammar bundle still has the node
 * name. `tests/lang.test.ts` proves every name against the loaded grammar and against a parsed
 * snippet rather than against a node-types listing; a missing name is a test failure, not a comment.
 */
export const LANGS: Readonly<Record<LangId, LangRow>> = {
  python: {
    id: "python",
    extensions: [".py"],
    grammarWasm: "tree-sitter-python.wasm",
    classTypes: ["class_definition"],
    functionTypes: ["function_definition"],
    importTypes: ["import_statement", "import_from_statement"],
    callTypes: ["call"],
    // `class Widget(Base)` keeps the bases in the class_definition's argument_list.
    inheritanceTypes: ["argument_list"],
    testFileRegex: /(^|\/)(?:tests?\/[^/]*|test_[^/]*|[^/]*_test)\.py$/i,
  },
  typescript: {
    id: "typescript",
    extensions: [".ts", ".mts", ".cts"],
    grammarWasm: "tree-sitter-typescript.wasm",
    // Interfaces, type aliases and enums are what callers depend on, so they are class nodes too.
    classTypes: [
      "class_declaration",
      "abstract_class_declaration",
      "class",
      "interface_declaration",
      "type_alias_declaration",
      "enum_declaration",
    ],
    functionTypes: [
      "function_declaration",
      "generator_function_declaration",
      "method_definition",
      "abstract_method_signature",
      "arrow_function",
    ],
    importTypes: ["import_statement"],
    callTypes: ["call_expression", "new_expression"],
    inheritanceTypes: ["class_heritage", "extends_type_clause"],
    testFileRegex: /(^|\/)(?:__tests__\/[^/]*|[^/]*\.(?:test|spec))\.(?:ts|mts|cts)$/i,
  },
  tsx: {
    id: "tsx",
    extensions: [".tsx"],
    grammarWasm: "tree-sitter-tsx.wasm",
    classTypes: [
      "class_declaration",
      "abstract_class_declaration",
      "class",
      "interface_declaration",
      "type_alias_declaration",
      "enum_declaration",
    ],
    functionTypes: [
      "function_declaration",
      "generator_function_declaration",
      "method_definition",
      "abstract_method_signature",
      "arrow_function",
    ],
    importTypes: ["import_statement"],
    callTypes: ["call_expression", "new_expression"],
    inheritanceTypes: ["class_heritage", "extends_type_clause"],
    testFileRegex: /(^|\/)(?:__tests__\/[^/]*|[^/]*\.(?:test|spec))\.tsx$/i,
  },
  javascript: {
    id: "javascript",
    extensions: [".js", ".jsx", ".mjs", ".cjs"],
    grammarWasm: "tree-sitter-javascript.wasm",
    classTypes: ["class_declaration", "class"],
    functionTypes: [
      "function_declaration",
      "generator_function_declaration",
      "method_definition",
      "arrow_function",
    ],
    importTypes: ["import_statement"],
    callTypes: ["call_expression", "new_expression"],
    inheritanceTypes: ["class_heritage"],
    testFileRegex: /(^|\/)(?:__tests__\/[^/]*|[^/]*\.(?:test|spec))\.(?:js|jsx|mjs|cjs)$/i,
  },
  go: {
    id: "go",
    extensions: [".go"],
    grammarWasm: "tree-sitter-go.wasm",
    classTypes: ["type_declaration"],
    functionTypes: ["function_declaration", "method_declaration"],
    importTypes: ["import_declaration"],
    callTypes: ["call_expression"],
    // Go has no superclass: embedding is a field list, so the clause lives in the type's body.
    inheritanceTypes: ["struct_type", "interface_type"],
    testFileRegex: /(^|\/)[^/]*_test\.go$/,
  },
  java: {
    id: "java",
    extensions: [".java"],
    grammarWasm: "tree-sitter-java.wasm",
    classTypes: ["class_declaration", "interface_declaration", "enum_declaration"],
    functionTypes: ["method_declaration", "constructor_declaration"],
    importTypes: ["import_declaration"],
    callTypes: ["method_invocation", "object_creation_expression", "method_reference"],
    inheritanceTypes: ["superclass", "super_interfaces"],
    testFileRegex: /(^|\/)(?:src\/test\/[^/]*|[^/]*(?:Test|Tests|IT))\.java$/i,
  },
  rust: {
    id: "rust",
    extensions: [".rs"],
    grammarWasm: "tree-sitter-rust.wasm",
    // `impl_item` is a scope for methods, not a second type definition, so it is not listed here.
    classTypes: ["struct_item", "enum_item", "trait_item"],
    functionTypes: ["function_item", "function_signature_item"],
    importTypes: ["use_declaration"],
    callTypes: ["call_expression", "macro_invocation"],
    // Rust's only superclass-like clause is a trait's supertraits (`trait A: B`). `impl B for A` is
    // an implementation clause on a top-level item and belongs to the parser's own dispatch.
    inheritanceTypes: ["trait_bounds"],
    testFileRegex: /(^|\/)(?:tests?\/[^/]*|[^/]*_test)\.rs$/i,
  },
  c: {
    id: "c",
    extensions: [".c", ".h"],
    grammarWasm: "tree-sitter-c.wasm",
    classTypes: ["struct_specifier", "type_definition"],
    functionTypes: ["function_definition"],
    importTypes: ["preproc_include"],
    callTypes: ["call_expression"],
    // C has no inheritance clause in the grammar; the empty list is the fact, not an oversight.
    inheritanceTypes: [],
    testFileRegex: /(^|\/)(?:tests?\/[^/]*|test_[^/]*|[^/]*_test)\.(?:c|h)$/i,
  },
  cpp: {
    id: "cpp",
    extensions: [".cpp", ".cc", ".cxx", ".hpp", ".hh"],
    grammarWasm: "tree-sitter-cpp.wasm",
    classTypes: ["class_specifier", "struct_specifier"],
    // Declarations are listed because a C++ out-of-line or header-only definition is one until the
    // parser sees its body; the reference parser counts them the same way.
    functionTypes: ["function_definition", "declaration", "field_declaration"],
    importTypes: ["preproc_include"],
    callTypes: ["call_expression"],
    inheritanceTypes: ["base_class_clause"],
    testFileRegex: /(^|\/)(?:tests?\/[^/]*|test_[^/]*|[^/]*_test)\.(?:cc|cpp|cxx|hpp|hh)$/i,
  },
  csharp: {
    id: "csharp",
    extensions: [".cs"],
    grammarWasm: "tree-sitter-c_sharp.wasm",
    classTypes: [
      "class_declaration",
      "interface_declaration",
      "enum_declaration",
      "struct_declaration",
      "record_declaration",
    ],
    functionTypes: ["method_declaration", "constructor_declaration"],
    importTypes: ["using_directive"],
    callTypes: ["invocation_expression", "object_creation_expression"],
    // `class W : Base, IShape` keeps both clauses in one base_list.
    inheritanceTypes: ["base_list"],
    testFileRegex: /(^|\/)(?:[^/]*\.Tests\/[^/]*|[^/]*(?:Test|Tests))\.cs$/i,
  },
  php: {
    id: "php",
    extensions: [".php"],
    grammarWasm: "tree-sitter-php.wasm",
    classTypes: [
      "class_declaration",
      "interface_declaration",
      "trait_declaration",
      "enum_declaration",
    ],
    functionTypes: ["function_definition", "method_declaration"],
    importTypes: [
      "namespace_use_declaration",
      "include_expression",
      "include_once_expression",
      "require_expression",
      "require_once_expression",
    ],
    callTypes: [
      "function_call_expression",
      "member_call_expression",
      "scoped_call_expression",
      "nullsafe_member_call_expression",
      "object_creation_expression",
    ],
    inheritanceTypes: ["base_clause", "class_interface_clause"],
    testFileRegex: /(^|\/)(?:tests?\/[^/]*|[^/]*Test)\.php$/i,
  },
  ruby: {
    id: "ruby",
    extensions: [".rb"],
    grammarWasm: "tree-sitter-ruby.wasm",
    classTypes: ["class", "module"],
    functionTypes: ["method", "singleton_method"],
    // `require` is an ordinary call node; the parser filters which calls are imports.
    importTypes: ["call"],
    callTypes: ["call"],
    inheritanceTypes: ["superclass"],
    testFileRegex: /(^|\/)(?:specs?\/[^/]*|[^/]*(?:_spec|_test))\.rb$/i,
  },
};

/**
 * Extension → language, built in `LANGS` order. The first row to claim an extension owns it, which
 * is the "built-ins always win" rule with a single source of truth: a duplicate extension is a
 * table edit, and `tests/lang.test.ts` catches it rather than a silent shadow.
 */
export const EXTENSION_MAP: Readonly<Record<string, LangId>> = buildExtensionMap();

function buildExtensionMap(): Readonly<Record<string, LangId>> {
  const map: Record<string, LangId> = {};
  // `LANGS` is an object literal, so its keys iterate in declaration order.
  for (const id of Object.keys(LANGS) as LangId[]) {
    for (const extension of LANGS[id].extensions) {
      if (map[extension] === undefined) {
        map[extension] = id;
      }
    }
  }
  return map;
}

/**
 * The language of a path, by extension alone. `null` means "not one of the 12", which the indexer
 * records as skipped; content sniffing and shebang detection are the parser's business, not this
 * table's. Extensions are case-insensitive because macOS and Windows checkouts still carry `.PY`.
 */
export function detectLanguage(filePath: string): LangId | null {
  const extension = extname(filePath).toLowerCase();
  if (extension === "") {
    return null;
  }
  return EXTENSION_MAP[extension] ?? null;
}

/** The resolved row for an id already known to be a language, e.g. from `detectLanguage`. */
export function specFor(id: LangId): LangRow {
  return LANGS[id];
}
