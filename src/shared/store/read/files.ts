/**
 * The file census: which paths the indexer parsed, which it looked at and skipped, and the
 * per-language counts `status` reports (`DATABASE.md` §3.2).
 *
 * Two tables answer one question, and both halves are mandatory. `file.parse_status` alone says
 * "0 files of language xslt exist"; `not_indexed` says "4 xslt paths were seen and none of them
 * could be parsed". The census therefore counts a skipped path as a file of its language that
 * failed, which is the difference between an empty language and a broken one — the exact
 * confusion `TOOL-OUTPUTS.md` records in CRG, where `xslt` and `liquid` are advertised as
 * supported while both grammars fail to load.
 *
 * `parse_status` maps to the census without a third bucket: a `partial` parse produced nodes and
 * counts as parsed (its `parse_error_ranges` are still on the row for `status` to report), only
 * `failed` is a failure. `files` is always `parsed + failed`, so a caller can check the
 * arithmetic instead of trusting it.
 *
 * Rows stay mapped to {@link IndexedFileRow}, the domain type `coverage.ts` already returns for
 * the same table, so no Drizzle `$inferSelect` type crosses this boundary. `coverage.ts` keeps
 * its own copy of the row mapper because its query is not this one; the shape is one interface
 * in one place, and a drift would fail the type check rather than a test.
 */
import { normalizeRel } from "#shared/paths";
import { type Conn, prepared } from "#shared/store/db";
import type { IndexedFileRow } from "#shared/store/read/coverage";

/** The `file` columns, in the order {@link toIndexedFileRow} reads them. */
const FILE_SELECT =
  "f.id AS id, f.path AS path, f.language AS language, f.sha256 AS sha256, f.size AS size," +
  " f.line_count AS line_count, f.parse_status AS parse_status," +
  " f.parse_error_ranges AS parse_error_ranges, f.indexed_generation AS indexed_generation";

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

/**
 * One language's census. `language` is `null` for a skipped path whose grammar the indexer could
 * not name (`not_indexed.language` is nullable); those rows are reported last rather than
 * dropped, because an unattributed failure is still a failure the caller must be told about.
 */
export interface LanguageCensus {
  readonly language: string | null;
  /** Paths seen for this language: indexed rows plus skipped rows. Always `parsed + failed`. */
  readonly files: number;
  /** `parse_status` of `ok` or `partial`: rows that produced a graph contribution. */
  readonly parsed: number;
  /** `parse_status = 'failed'` plus every `not_indexed` row for the language. */
  readonly failed: number;
}

interface IndexedCensusSqlRow {
  readonly language: string;
  readonly files: number;
  readonly parsed: number;
  readonly failed: number;
}

interface SkippedCensusSqlRow {
  readonly language: string | null;
  readonly skipped: number;
}

function toIndexedFileRow(row: FileSqlRow): IndexedFileRow {
  return {
    id: row.id,
    path: row.path,
    language: row.language,
    sha256: row.sha256,
    size: row.size,
    lineCount: row.line_count,
    parseStatus: row.parse_status,
    parseErrorRanges: row.parse_error_ranges,
    indexedGeneration: row.indexed_generation,
  };
}

/**
 * One file by its repo-relative path, over the unique `path` index (`DATABASE.md` §4). The path
 * is normalized first, so `./src/a.ts` and `src/a.ts` are one lookup and not two rows.
 *
 * `null` means "no `file` row", which is not the same as "not indexed": a skipped path has its
 * reason in `not_indexed` (`rowsForPaths`), and `null` here is what tells a caller to look there.
 */
export function fileByPath(conn: Conn, path: string): IndexedFileRow | null {
  const row = prepared(conn, "files.byPath", (sqlite) =>
    sqlite.query<FileSqlRow, [string]>(`SELECT ${FILE_SELECT} FROM file f WHERE f.path = ?`),
  ).get(normalizeRel(path));
  return row === null ? null : toIndexedFileRow(row);
}

/**
 * Every indexed file in path order. The staleness check needs the whole manifest at once
 * (`#shared/domain/freshness.ts` compares each row's `sha256` against the bytes on disk), so this
 * one is deliberately unbounded — the caller's alternative is one query per file.
 */
export function allFiles(conn: Conn): IndexedFileRow[] {
  return prepared(conn, "files.all", (sqlite) =>
    sqlite.query<FileSqlRow, []>(`SELECT ${FILE_SELECT} FROM file f ORDER BY f.path`),
  )
    .all()
    .map(toIndexedFileRow);
}

/**
 * `{language, files, parsed, failed}` per language, with `not_indexed` folded in: every skipped
 * path adds one to `files` and one to `failed` for its language.
 *
 * Languages are ordered by name with the unattributed bucket (`language: null`) last, so the
 * ordering is stable for a caller that decides to truncate the list.
 */
export function censusByLanguage(conn: Conn): LanguageCensus[] {
  const indexed = prepared(conn, "files.censusIndexed", (sqlite) =>
    sqlite.query<IndexedCensusSqlRow, []>(
      "SELECT f.language AS language, COUNT(*) AS files," +
        " SUM(CASE WHEN f.parse_status = 'failed' THEN 0 ELSE 1 END) AS parsed," +
        " SUM(CASE WHEN f.parse_status = 'failed' THEN 1 ELSE 0 END) AS failed" +
        " FROM file f GROUP BY f.language",
    ),
  ).all();

  const skipped = prepared(conn, "files.censusSkipped", (sqlite) =>
    sqlite.query<SkippedCensusSqlRow, []>(
      "SELECT ni.language AS language, COUNT(*) AS skipped FROM not_indexed ni" +
        " GROUP BY ni.language",
    ),
  ).all();

  const census = new Map<string | null, { files: number; parsed: number; failed: number }>();
  const bucket = (language: string | null) => {
    const existing = census.get(language);
    if (existing !== undefined) {
      return existing;
    }
    const fresh = { files: 0, parsed: 0, failed: 0 };
    census.set(language, fresh);
    return fresh;
  };

  for (const row of indexed) {
    const counts = bucket(row.language);
    counts.files += row.files;
    counts.parsed += row.parsed;
    counts.failed += row.failed;
  }
  for (const row of skipped) {
    const counts = bucket(row.language);
    counts.files += row.skipped;
    counts.failed += row.skipped;
  }

  return [...census.entries()]
    .sort(([a], [b]) => {
      if (a === b) {
        return 0;
      }
      if (a === null) {
        return 1;
      }
      if (b === null) {
        return -1;
      }
      return a < b ? -1 : 1;
    })
    .map(([language, counts]) => ({
      language,
      files: counts.files,
      parsed: counts.parsed,
      failed: counts.failed,
    }));
}
