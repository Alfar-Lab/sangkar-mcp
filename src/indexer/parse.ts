/**
 * The indexer's single parser entry point. A grammar is one wasm file from `tree-sitter-wasms`; it
 * is loaded on first use, kept in an LRU of `MAX_CACHED_LANGUAGES` languages together with its
 * `Parser`, and reset before every reuse.
 *
 * Two things this module owns because a `Parser` is mutable wasm state:
 * - a parser is never reused while another parse is still running: every call goes through one FIFO
 *   queue (`parseFile` and `disposeLang` both enqueue), so one file cannot be parsed twice at once
 *   and no parser is ever shared between two calls;
 * - a grammar that cannot be loaded is a structured result, never an exception. A caller that gets
 *   `grammar_unavailable` records a coverage reason and keeps indexing the rest of the repository
 *   instead of aborting the run.
 */

import { createRequire } from "node:module";
import { Language, type Node, Parser, type Tree } from "web-tree-sitter";
import { LANGS, specFor } from "./lang/registry";
import type { LangId, LangRow } from "./lang/types";

/** How many languages keep a loaded grammar and a parser alive. Insertion order is the LRU order. */
export const MAX_CACHED_LANGUAGES = 6;

/** How many syntax-error ranges `errorRanges` reports before it summarises the rest as a count. */
export const MAX_ERROR_RANGES = 20;

const GRAMMAR_DIRECTORY = "tree-sitter-wasms/out";
const RUNTIME_WASM = "web-tree-sitter/web-tree-sitter.wasm";

/** Why a file produced no tree. Both values are facts the indexer records, not crashes. */
export type ParseFailureReason = "grammar_unavailable" | "parse_failed";

/** A parsed file: the tree the extractor walks, plus where the grammar had to recover. */
export interface ParseSuccess {
  readonly ok: true;
  readonly tree: Tree;
  /**
   * One 1-based line range per syntax error, in tree order: `"12"` or `"12-14"`. At most
   * `MAX_ERROR_RANGES` ranges; when there are more, the last element is `"+N more ranges"`, so a
   * consumer joining with `", "` gets the capped list and its count in one string.
   */
  readonly errorRanges: string[];
}

/** No tree. `reason` is the coverage fact; `detail` is for the indexer log on stderr. */
export interface ParseFailure {
  readonly ok: false;
  readonly reason: ParseFailureReason;
  readonly detail: string;
}

export type ParseResult = ParseSuccess | ParseFailure;

/** Observable pool state, so the LRU bound and the one-parse-at-a-time rule can be verified. */
export interface ParserPoolStats {
  /** Cached languages in LRU order, least recently used first. */
  readonly cached: LangId[];
  readonly inFlight: number;
  readonly peakInFlight: number;
}

interface PooledParser {
  readonly language: Language;
  readonly parser: Parser;
}

const requireFromModule = createRequire(import.meta.url);
const pool = new Map<LangId, PooledParser>();

let runtime: Promise<void> | null = null;
let tail: Promise<void> = Promise.resolve();
let inFlight = 0;
let peakInFlight = 0;

/**
 * Initialises the web-tree-sitter runtime once per process. Idempotent, and a failed initialisation
 * is not memoised, so a later call retries instead of failing forever on one transient error.
 */
export function initParsers(): Promise<void> {
  if (runtime === null) {
    runtime = Parser.init({
      locateFile: () => requireFromModule.resolve(RUNTIME_WASM),
    }).catch((error: unknown) => {
      runtime = null;
      throw error;
    });
  }
  return runtime;
}

/**
 * Parses one file. The promise settles with a `ParseFailure` for an unknown language, an unloadable
 * grammar, or a parser that returns no tree — it never rejects.
 */
export function parseFile(path: string, source: string, lang: LangId): Promise<ParseResult> {
  return track(async () => {
    const spec = languageSpec(lang);
    if (spec === null) {
      return {
        ok: false,
        reason: "grammar_unavailable",
        detail: `no grammar is registered for language ${String(lang)} (${path})`,
      };
    }

    let entry: PooledParser;
    try {
      await initParsers();
      entry = await acquire(spec);
    } catch (error) {
      return {
        ok: false,
        reason: "grammar_unavailable",
        detail: `grammar ${spec.grammarWasm} for ${spec.id} could not be loaded (${path}): ${describe(error)}`,
      };
    }

    // A previous parse may have stopped early (a progress callback, a failed callback), so the
    // parser is told to start from the beginning rather than resume where it left off.
    entry.parser.reset();

    let tree: Tree | null;
    try {
      tree = entry.parser.parse(source);
    } catch (error) {
      // The parser is no longer trusted after it threw; the next call reloads its grammar.
      release(spec.id);
      return {
        ok: false,
        reason: "parse_failed",
        detail: `the ${spec.id} parser failed on ${path}: ${describe(error)}`,
      };
    }
    if (tree === null) {
      release(spec.id);
      return {
        ok: false,
        reason: "parse_failed",
        detail: `the ${spec.id} parser returned no tree for ${path}`,
      };
    }

    return { ok: true, tree, errorRanges: collectErrorRanges(tree) };
  });
}

/**
 * Drops a language's grammar and parser. Queued behind any running parse, because deleting a parser
 * that a parse still holds would pull the wasm object out from under it.
 */
export function disposeLang(lang: LangId): Promise<void> {
  return track(async () => {
    release(lang);
  });
}

export function parserPoolStats(): ParserPoolStats {
  return { cached: [...pool.keys()], inFlight, peakInFlight };
}

/** The resolved row for a language id, or `null` when the id names no grammar. */
function languageSpec(lang: LangId): LangRow | null {
  // `lang` is a `LangId` at compile time but arrives from registry data at run time, so the lookup
  // is defensive: an unknown id is a coverage fact to report, not an exception to throw.
  return Object.hasOwn(LANGS, lang) ? specFor(lang) : null;
}

/**
 * The parser and grammar for a language, loading them on a miss and moving the entry to the
 * most-recently-used end on a hit. Eviction deletes the parser so its wasm memory is released;
 * the chance that a `Language` for an evicted id is loaded again is accepted, since re-parsing it
 * with a different language would be the alternative and there is none.
 */
async function acquire(spec: LangRow): Promise<PooledParser> {
  const cached = pool.get(spec.id);
  if (cached !== undefined) {
    pool.delete(spec.id);
    pool.set(spec.id, cached);
    return cached;
  }

  const language = await loadGrammar(spec);
  const parser = new Parser();
  try {
    parser.setLanguage(language);
  } catch (error) {
    parser.delete();
    throw error;
  }
  const entry: PooledParser = { language, parser };
  pool.set(spec.id, entry);

  while (pool.size > MAX_CACHED_LANGUAGES) {
    const oldest = pool.keys().next();
    if (oldest.done === true || oldest.value === undefined) {
      break;
    }
    release(oldest.value);
  }
  return entry;
}

function release(lang: LangId): void {
  const entry = pool.get(lang);
  if (entry === undefined) {
    return;
  }
  pool.delete(lang);
  entry.parser.delete();
}

async function loadGrammar(spec: LangRow): Promise<Language> {
  const wasmPath = requireFromModule.resolve(`${GRAMMAR_DIRECTORY}/${spec.grammarWasm}`);
  const bytes = new Uint8Array(await Bun.file(wasmPath).arrayBuffer());
  return Language.load(upgradeLegacyDylink(bytes));
}

/**
 * `tree-sitter-wasms@0.1.13` ships grammars carrying the Emscripten custom section as it was written
 * before `dylink.0`: a bare `dylink` payload of memory and table numbers, which the Emscripten
 * runtime inside `web-tree-sitter@0.27.2` rejects with "invalid dylink metadata". Only that one
 * section header is rewritten, into a single `WASM_DYLINK_MEM_INFO` subsection; every size is
 * carried over verbatim. A payload with anything else in it is refused rather than loaded on a
 * guess, because the section decides the grammar's memory layout. A wasm that already carries
 * `dylink.0`, or has no dylink section at all, is returned untouched, so a bundle upgrade needs no
 * change here.
 */
function upgradeLegacyDylink(wasm: Uint8Array): Uint8Array {
  const cursor: ByteCursor = { offset: 8 }; // wasm magic (4 bytes) + version (4 bytes)
  if (readByte(wasm, cursor.offset) !== 0) {
    return wasm; // not a custom section, so there is no dylink header to rewrite
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
    return wasm; // `dylink.0` or an unrelated custom section: nothing to upgrade
  }

  const memorySize = readLeb(wasm, cursor);
  const memoryAlign = readLeb(wasm, cursor);
  const tableSize = readLeb(wasm, cursor);
  const tableAlign = readLeb(wasm, cursor);
  for (let offset = cursor.offset; offset < sectionEnd; offset += 1) {
    // The trailing field is a zero needed-dynlibs count. Anything else is metadata this rewrite
    // would drop, so it is refused rather than ignored.
    if (readByte(wasm, offset) !== 0) {
      throw new Error("grammar wasm carries dylink metadata this loader does not understand");
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

interface ByteCursor {
  offset: number;
}

/** `noUncheckedIndexedAccess` makes every byte a possible undefined; a wasm reader wants a failure. */
function readByte(bytes: Uint8Array, offset: number): number {
  const byte = bytes[offset];
  if (byte === undefined) {
    throw new Error(`truncated grammar wasm at byte ${offset}`);
  }
  return byte;
}

function readLeb(bytes: Uint8Array, cursor: ByteCursor): number {
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
 * The syntax-error ranges of a tree: every `ERROR` node and every `MISSING` node, formatted as a
 * 1-based line range and deduplicated, capped at `MAX_ERROR_RANGES` followed by a count suffix. The
 * walk is a `TreeCursor` because a file with no errors still visits every node.
 */
function collectErrorRanges(tree: Tree): string[] {
  const ranges: string[] = [];
  const seen = new Set<string>();
  const cursor = tree.walk();
  try {
    for (;;) {
      const node = cursor.currentNode;
      if (node.isError || node.isMissing) {
        const range = formatLineRange(node);
        if (!seen.has(range)) {
          seen.add(range);
          ranges.push(range);
        }
      }
      if (cursor.gotoFirstChild()) {
        continue;
      }
      for (;;) {
        if (cursor.gotoNextSibling()) {
          break;
        }
        if (!cursor.gotoParent()) {
          return capped(ranges);
        }
      }
    }
  } finally {
    cursor.delete();
  }
}

/** An error that ends at column 0 of the next row does not cover that row, so it is not named. */
function formatLineRange(node: Node): string {
  const startLine = node.startPosition.row + 1;
  const endRow = node.endPosition.row;
  const endLine =
    node.endPosition.column === 0 && endRow > node.startPosition.row ? endRow : endRow + 1;
  return endLine > startLine ? `${startLine}-${endLine}` : `${startLine}`;
}

function capped(ranges: string[]): string[] {
  if (ranges.length <= MAX_ERROR_RANGES) {
    return ranges;
  }
  return [...ranges.slice(0, MAX_ERROR_RANGES), `+${ranges.length - MAX_ERROR_RANGES} more ranges`];
}

/**
 * Serialises one unit of work onto the module's FIFO queue. `tail` is kept settled so a rejected
 * parse cannot poison the queue for every later file, and the in-flight counters are the evidence
 * that the serialisation actually holds.
 */
function enqueue<T>(work: () => Promise<T>): Promise<T> {
  const run = tail.then(work, work);
  tail = run.then(
    () => undefined,
    () => undefined,
  );
  return run;
}

function track<T>(work: () => Promise<T>): Promise<T> {
  return enqueue(async () => {
    inFlight += 1;
    peakInFlight = Math.max(peakInFlight, inFlight);
    try {
      return await work();
    } finally {
      inFlight -= 1;
    }
  });
}

function describe(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
