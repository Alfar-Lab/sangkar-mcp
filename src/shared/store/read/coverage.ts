/**
 * Coverage primitives: the two halves of "was this scope indexed?" (`DATABASE.md` §3.2, §6).
 *
 * A path prefix is answered from two tables, and both halves are mandatory. `file` says which
 * paths were parsed; `not_indexed` says which were looked at and skipped, with the reason. One
 * half alone cannot tell "nothing calls this" apart from "this was never parsed", which is the
 * difference between a correct empty answer and a wrong one (`DESIGN.md` §6).
 *
 * The prefix test reuses {@link prefixBounds} from `#shared/paths`, whose lower bound is
 * `prefix + "/"`: the naive `path >= prefix AND path < prefix + "0"` also matches a sibling
 * such as `src-old` for the prefix `src`, because `-` (0x2d) sorts below `/` (0x2f).
 */
import { prefixBounds } from "#shared/paths";
import { type Conn, prepared } from "#shared/store/db";

/** One parsed file, mapped from the `file` row. */
export interface IndexedFileRow {
  readonly id: number;
  readonly path: string;
  readonly language: string;
  readonly sha256: string;
  readonly size: number;
  readonly lineCount: number;
  /** `ok` | `partial` | `failed`; the census in `status` counts these per language. */
  readonly parseStatus: string;
  /** `'12-12,38-38'` when `parseStatus` is `partial`, otherwise `null`. */
  readonly parseErrorRanges: string | null;
  /** The publish that last wrote this file's rows; a stale value is a stale file. */
  readonly indexedGeneration: string;
}

/** One path the indexer looked at and did not index, with the reason it says so. */
export interface SkippedPathRow {
  readonly id: number;
  readonly path: string;
  /** `grammar_unavailable` | `binary` | `ignored` | `size_cap` | `unsupported_ext` | `unreadable` | … */
  readonly reason: string;
  readonly detail: string | null;
  readonly language: string | null;
}

/** Both halves of the answer for one prefix query; empty is meaningful only when both are empty. */
export interface PathCoverage {
  readonly indexed: readonly IndexedFileRow[];
  readonly notIndexed: readonly SkippedPathRow[];
}

/** One call site whose target did not bind, kept for the absence contract (`DATABASE.md` §7). */
export interface UnresolvedRef {
  readonly id: number;
  readonly fileId: number;
  /** `file.path` of the file the call site lives in. */
  readonly path: string;
  readonly line: number;
  readonly rawTarget: string;
  readonly evidenceKind: string | null;
  readonly evidence: string | null;
}

interface FileSqlRow {
  readonly id: number;
  readonly path: string;
  readonly language: string;
  readonly sha256: string;
  readonly size: number;
  readonly line_count: number;
  readonly parse_status: string;
  readonly parse_error_ranges: string | null;
  readonly indexed_generation: string;
}

interface SkippedSqlRow {
  readonly id: number;
  readonly path: string;
  readonly reason: string;
  readonly detail: string | null;
  readonly language: string | null;
}

interface UnresolvedSqlRow {
  readonly id: number;
  readonly file_id: number;
  readonly path: string;
  readonly line: number;
  readonly raw_target: string;
  readonly evidence_kind: string | null;
  readonly evidence: string | null;
}

/**
 * `(t.path = ? OR (t.path > ? AND t.path < ?))`: equality for the prefix itself, a strict range
 * for its children. One block per requested prefix, bound in `(exact, low, high)` order.
 */
function pathConditions(alias: string, prefixCount: number): string {
  const block = `(${alias}.path = ? OR (${alias}.path > ? AND ${alias}.path < ?))`;
  return new Array<string>(prefixCount).fill(block).join(" OR ");
}

/** One `not_indexed` row, mapped in one place so every reader of that table agrees on the shape. */
function toSkippedPathRow(row: SkippedSqlRow): SkippedPathRow {
  return {
    id: row.id,
    path: row.path,
    reason: row.reason,
    detail: row.detail,
    language: row.language,
  };
}

/** The three bounds of every prefix, flattened to match {@link pathConditions} placeholder order. */
function pathParams(prefixes: readonly string[]): string[] {
  const params: string[] = [];
  for (const prefix of prefixes) {
    const bounds = prefixBounds(prefix);
    params.push(bounds.exact, bounds.low, bounds.high);
  }
  return params;
}

/**
 * Every indexed file under `prefixes`, plus every skipped path under the same prefixes with its
 * reason. A path matches a prefix by equality or by being strictly inside it; the two lists are
 * disjoint because a path has a `file` row or a `not_indexed` row, never both as the same fact.
 *
 * No limit and no ordering decision beyond `path`, which keeps a caller's pagination stable;
 * ranking and formatting belong to the tool (§5.3).
 */
export function rowsForPaths(conn: Conn, prefixes: readonly string[]): PathCoverage {
  if (prefixes.length === 0) {
    return { indexed: [], notIndexed: [] };
  }
  const params = pathParams(prefixes);

  const indexed = prepared(conn, `coverage.indexed/${prefixes.length}`, (sqlite) =>
    sqlite.query<FileSqlRow, string[]>(
      "SELECT f.id, f.path, f.language, f.sha256, f.size, f.line_count, f.parse_status," +
        " f.parse_error_ranges, f.indexed_generation FROM file f WHERE " +
        `${pathConditions("f", prefixes.length)} ORDER BY f.path`,
    ),
  ).all(...params);

  const notIndexed = prepared(conn, `coverage.notIndexed/${prefixes.length}`, (sqlite) =>
    sqlite.query<SkippedSqlRow, string[]>(
      "SELECT ni.id, ni.path, ni.reason, ni.detail, ni.language FROM not_indexed ni WHERE " +
        `${pathConditions("ni", prefixes.length)} ORDER BY ni.path, ni.reason`,
    ),
  ).all(...params);

  return {
    indexed: indexed.map((row) => ({
      id: row.id,
      path: row.path,
      language: row.language,
      sha256: row.sha256,
      size: row.size,
      lineCount: row.line_count,
      parseStatus: row.parse_status,
      parseErrorRanges: row.parse_error_ranges,
      indexedGeneration: row.indexed_generation,
    })),
    notIndexed: notIndexed.map(toSkippedPathRow),
  };
}

/**
 * Every skipped path in the graph, with no prefix filter.
 *
 * {@link rowsForPaths} answers "is this scope covered", which is a slice by construction, so a
 * caller that sums slices reports a smaller number than the graph holds whenever a skip sits
 * outside every scope it asked about. This is the list that total is counted from.
 */
export function notIndexedAll(conn: Conn): SkippedPathRow[] {
  return prepared(conn, "coverage.notIndexedAll", (sqlite) =>
    sqlite.query<SkippedSqlRow, []>(
      "SELECT ni.id, ni.path, ni.reason, ni.detail, ni.language FROM not_indexed ni" +
        " ORDER BY ni.path, ni.reason",
    ),
  )
    .all()
    .map(toSkippedPathRow);
}

/**
 * The repo-wide number of skipped paths: the one total every tool can quote.
 *
 * A separate `COUNT(*)` rather than `notIndexedAll().length` because the total is a headline
 * number read on every `status`, and materialising every row to count it costs the whole table.
 */
export function notIndexedCount(conn: Conn): number {
  const row = prepared(conn, "coverage.notIndexedCount", (sqlite) =>
    sqlite.query<{ readonly n: number }, []>("SELECT COUNT(*) AS n FROM not_indexed"),
  ).get();
  return row === null ? 0 : row.n;
}

/**
 * Unbound call sites for the given files, over `idx_unresolved_file` (§6). An empty result is
 * only an absence when coverage for the same scope is complete (`DATABASE.md` §7), which is why
 * this returns rows and never a verdict.
 */
export function unresolvedForFiles(conn: Conn, fileIds: readonly number[]): UnresolvedRef[] {
  if (fileIds.length === 0) {
    return [];
  }
  const list = new Array<string>(fileIds.length).fill("?").join(", ");
  const rows = prepared(conn, `coverage.unresolved/${fileIds.length}`, (sqlite) =>
    sqlite.query<UnresolvedSqlRow, number[]>(
      "SELECT u.id, u.file_id, f.path, u.line, u.raw_target, u.evidence_kind, u.evidence" +
        " FROM unresolved u JOIN file f ON f.id = u.file_id" +
        ` WHERE u.file_id IN (${list}) ORDER BY u.file_id, u.line, u.raw_target`,
    ),
  ).all(...fileIds);
  return rows.map((row) => ({
    id: row.id,
    fileId: row.file_id,
    path: row.path,
    line: row.line,
    rawTarget: row.raw_target,
    evidenceKind: row.evidence_kind,
    evidence: row.evidence,
  }));
}
