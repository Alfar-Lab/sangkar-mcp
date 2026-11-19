/**
 * Coverage assembly: the absence contract's arithmetic (`DESIGN.md` §6, `DATABASE.md` §3.2).
 *
 * The question this answers is *"nothing calls X — are you sure?"*, and it is answered with
 * four distinct outcomes that must never collapse: absent (`eq`), unresolved (`gte`),
 * out-of-scope (`gte`) and stale (`gte`). `assemble` is the one place that decides which of
 * them holds, so two tools cannot disagree about whether an empty list is a fact.
 *
 * `eq` requires all three conditions this module can see: complete coverage, no unresolved row
 * in the scope, and a fresh generation. Truncation is the fourth and belongs to `emit`; that is
 * why the relation is returned alongside the coverage the envelope's `relationFor` already
 * understands, rather than baked into a payload.
 *
 * The returned `coverage` is the `Coverage` interface of `#shared/envelope`, not a copy of it:
 * one contract, one definition.
 *
 * Two skip counts travel together because they answer different questions and neither implies the
 * other: `coverage.skipped` counts the paths *this scope* skipped (`status` over a directory), while
 * `notIndexedTotal` is the whole graph's count that a scoped answer (`summary`, `expand`) publishes
 * as `not_indexed_total`, so a narrow scope cannot understate what the repository never read.
 */
import type { Coverage, TotalRelation } from "#shared/envelope";
import type { Conn } from "#shared/store/db";
import {
  type IndexedFileRow,
  notIndexedCount,
  type PathCoverage,
  rowsForPaths,
  type SkippedPathRow,
  type UnresolvedRef,
  unresolvedForFiles,
} from "#shared/store/read/coverage";

/**
 * What a query is about. `paths` are repo-relative prefixes, `generation` is the generation the
 * answer will be served from, and `stale` is the freshness verdict of that generation against
 * the working tree (`DESIGN.md` §10.4) — a domain module does not read git.
 *
 * `fileIds` lets a caller narrow which files' unresolved call sites count; omitted means "every
 * file indexed under `paths`", which is the whole scope.
 */
export interface CoverageScope {
  readonly paths: readonly string[];
  readonly generation: string;
  readonly stale: boolean;
  readonly fileIds?: readonly number[];
}

/**
 * The two halves of a scope question, with the connection already bound. A `Conn`-backed
 * implementation comes from {@link coverageSource}; a test supplies a hand-written object over
 * an in-memory fixture, so the `eq`/`gte` matrix is provable without a database.
 */
export interface CoverageSource {
  readonly rowsForPaths: (prefixes: readonly string[]) => PathCoverage;
  readonly unresolvedForFiles: (fileIds: readonly number[]) => readonly UnresolvedRef[];
  /** The repo-wide skipped-path count (`reader.notIndexedCount()`). */
  readonly notIndexedCount: () => number;
}

/** Binds the store's coverage lookups to a connection. */
export function coverageSource(conn: Conn): CoverageSource {
  return {
    rowsForPaths: (prefixes) => rowsForPaths(conn, prefixes),
    unresolvedForFiles: (fileIds) => unresolvedForFiles(conn, fileIds),
    notIndexedCount: () => notIndexedCount(conn),
  };
}

/**
 * The assembled answer: the envelope's `coverage`, the unresolved rows the scope's call sites
 * did not bind (the tool decides how to show them) and the relation those two support.
 */
export interface CoverageAssembly {
  readonly coverage: Coverage;
  readonly unresolved: readonly UnresolvedRef[];
  readonly relation: TotalRelation;
  /** `coverage.skipped` is scoped to `scope.paths`; this is the whole graph. Never smaller than
   * `coverage.not_indexed.length`. */
  readonly notIndexedTotal: number;
}

/** `partial` and `failed` parses are both "we did not fully read this file" (`DESIGN.md` §9.2). */
function isFailedParse(file: IndexedFileRow): boolean {
  return file.parseStatus !== "ok";
}

/**
 * A generation is fresh for this scope when the caller found no working-tree drift *and* every
 * file in the scope was written by the generation being served. A file from an older publish
 * is a stale answer even when the repository itself has not moved.
 */
function isFresh(scope: CoverageScope, indexed: readonly IndexedFileRow[]): boolean {
  if (scope.stale) {
    return false;
  }
  return indexed.every((file) => file.indexedGeneration === scope.generation);
}

/**
 * Assembles coverage for one scope and derives the relation it can honestly support.
 *
 * `complete` is false when the scope holds a skipped path or a `partial`/`failed` parse, and
 * also when nothing was indexed at all: an absence claim over zero files is unverified, so it
 * must not be the one case that reports `eq` (the empty-scope trap `DESIGN.md` §6 names).
 * `unresolved_edges` counts the rows in *this* scope, which is what makes `gte` local instead
 * of repo-wide.
 */
export function assemble(source: CoverageSource, scope: CoverageScope): CoverageAssembly {
  const paths = source.rowsForPaths(scope.paths);
  const indexed: readonly IndexedFileRow[] = paths.indexed;
  const skipped: readonly SkippedPathRow[] = paths.notIndexed;

  const fileIds = scope.fileIds ?? indexed.map((file) => file.id);
  const unresolved = source.unresolvedForFiles([...fileIds]);

  const failed = indexed.filter(isFailedParse).length;
  const complete = indexed.length > 0 && skipped.length === 0 && failed === 0;
  const fresh = isFresh(scope, indexed);

  const coverage: Coverage = {
    complete,
    indexed: indexed.length,
    skipped: skipped.length,
    failed,
    unresolved_edges: unresolved.length,
    not_indexed: skipped.map((row) => ({ path: row.path, reason: row.reason })),
  };

  const relation: TotalRelation = complete && unresolved.length === 0 && fresh ? "eq" : "gte";
  return { coverage, unresolved, relation, notIndexedTotal: source.notIndexedCount() };
}
