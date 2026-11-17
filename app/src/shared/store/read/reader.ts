/**
 * `GraphReader`: the aggregate of every read primitive, bound to one connection and handed to a
 * tool as `ToolContext.reader` (`TECH-SPEC.md` §6, `plan.constraint_invariant.md`).
 *
 * The methods take no `conn`. That is the whole point of the interface: a handler has no
 * connection, so "tools never see a connection" is a property of the type and not a rule someone
 * has to remember in review. `makeReader(conn)` closes over the handle once, at the composition
 * root, and what a tool holds afterwards can only read.
 *
 * The free functions in `read/{nodes,edges,metrics,coverage,files}.ts` keep the `conn`-first
 * shape, because they are the kernel and a caller inside `shared` may hold a raw handle. This
 * module is the seam above them: it maps each method to one prepared statement and returns the
 * domain rows those primitives already define, so no Drizzle `$inferSelect` type is reachable
 * from a tool.
 *
 * Every query here is a fixed shape assembled once by `prepared(conn, key, build)`; the only
 * shape that varies is an `IN (…)` list, whose arity is part of the cache key. Nothing in this
 * file builds a query per hop or per row.
 *
 * `limit` on {@link GraphReader.entryPoints} / {@link GraphReader.hotspots} /
 * {@link GraphReader.readNodeFts} is a statement bound, not a product knob: `shared` decides what
 * is true and in which stable order, the tool decides what to show and how much of it.
 */
import { AppError } from "#shared/errors";
import { type Conn, prepared } from "#shared/store/db";
import {
  type IndexedFileRow,
  type PathCoverage,
  rowsForPaths,
  type UnresolvedRef,
  unresolvedForFiles,
} from "#shared/store/read/coverage";
import { inbound, type NeighbourEdge, outbound } from "#shared/store/read/edges";
import {
  allFiles,
  censusByLanguage,
  fileByPath,
  type LanguageCensus,
} from "#shared/store/read/files";
import { metricsFor, type NodeMetrics } from "#shared/store/read/metrics";
import { findByName, findByQn, members, type NodeRow, nodesByFile } from "#shared/store/read/nodes";

export type {
  IndexedFileRow,
  LanguageCensus,
  NeighbourEdge,
  NodeMetrics,
  NodeRow,
  PathCoverage,
  UnresolvedRef,
};

/**
 * `test_gaps` is `tested_depth IS NULL OR tested_depth > 2` (`DATABASE.md` §3.4): a symbol with
 * no test within two hops is what a reviewer means by untested, and depth 0 (tested directly) and
 * 1 (tested through a direct caller) are not gaps.
 */
export const TEST_GAP_MAX_DEPTH = 2;

/** Identity and health from `meta`, plus the publish time of the current generation. */
export interface GraphMeta {
  /** `meta.project_name`: display identity, fixed at first index (`DATABASE.md` §3.1). */
  readonly projectName: string | null;
  /** `meta.project_root`: the guard against answering about a different tree. */
  readonly projectRoot: string | null;
  /** `meta.generation`: the generation a cursor binds to; `null` on an unpublished database. */
  readonly generation: string | null;
  /** `generation.published_at` of {@link generation}; `null` when there is no generation row. */
  readonly indexedAt: string | null;
  /** `meta.indexer_state`: `idle` | `running` | `failed`, as the indexer last wrote it. */
  readonly indexerState: string | null;
  /** `meta.consecutive_failures`, `0` when the indexer never recorded one. */
  readonly consecutiveFailures: number;
}

/** One publish, mapped from the `generation` row. */
export interface GenerationRow {
  readonly id: string;
  readonly seq: number;
  readonly publishedAt: string;
  readonly baseCommit: string | null;
  readonly worktreeDirty: boolean;
  readonly filesIndexed: number;
  readonly nodes: number;
  readonly edges: number;
  readonly durationMs: number;
  readonly mode: string;
  readonly status: string;
}

/** A symbol that disappeared, kept so `summary(change)` can report it without the base commit. */
export interface TombstoneRow {
  readonly qn: string;
  readonly kind: string;
  readonly fileId: number | null;
  readonly lastSeenGeneration: string;
  readonly removedInGeneration: string;
  /** A removed symbol that still had callers is the highest-value review signal. */
  readonly hadCallers: boolean;
}

/** `[start_line, end_line]`, the `lines` pair every tool payload shows. */
export type LineRange = readonly [start: number, end: number];

/** One symbol with no inbound edges worth naming: what `summary(repo)` calls `entry_points`. */
export interface EntryPointRow {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  /** `file.path` of the defining file. */
  readonly file: string;
  readonly lines: LineRange;
  /** `reachable_count`, computed at publish from the entry point; `null` when unmeasured. */
  readonly reachable: number | null;
}

/** One highly connected symbol, ordered by total degree (`DATABASE.md` §6 `summary(repo)`). */
export interface HotspotRow {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: LineRange;
  readonly inDegree: number;
  readonly outDegree: number;
}

/**
 * One symbol that failed the test-gap test. `testedDepth` carries the measurement — `null` means
 * no test is reachable at all, a number greater than {@link TEST_GAP_MAX_DEPTH} means one is
 * reachable but far — so the tool can word the `reason` without re-deriving it.
 */
export interface TestGapRow {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: LineRange;
  readonly testedDepth: number | null;
}

/**
 * One FTS5 hit. `score` is the raw `bm25()` value, where **smaller is better** (the function
 * returns a negative number), which is why the ordering below is ascending: the tool decides how
 * to present it, but it never has to re-sort to know which rows are best.
 */
export interface FtsHit {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: LineRange;
  readonly score: number;
}

/**
 * Every read a tool may perform. One method per query shape in `DATABASE.md` §6, so a handler's
 * data needs are visible in one interface and a fake is a bounded amount of work.
 */
export interface GraphReader {
  /** Exact `qn` resolution (unique index); `null` when nothing matches. */
  findByQn(qn: string): NodeRow | null;
  /** Exact bare-name resolution, ordered by `qn`; `kinds` is all-or-nothing. */
  findByName(name: string, kinds?: readonly string[]): NodeRow[];
  /** Direct children of `parentId` in source order (`expand(what="members")`). */
  members(parentId: number): NodeRow[];
  /** Every node one file defines, in source order. */
  nodesByFile(fileId: number): NodeRow[];
  /** Edges into `nodeId` with the calling node attached. */
  inbound(nodeId: number, kinds?: readonly string[]): NeighbourEdge[];
  /** Edges out of `nodeId` with the called node attached. */
  outbound(nodeId: number, kinds?: readonly string[]): NeighbourEdge[];
  /** Derived degrees, entry flag and `tested_depth`; `null` when the publish never measured it. */
  metricsFor(nodeId: number): NodeMetrics | null;
  /** Indexed files and skipped paths under `prefixes`: both halves of a coverage answer. */
  rowsForPaths(prefixes: readonly string[]): PathCoverage;
  /** Unbound call sites for `fileIds`; an empty list is an absence only when coverage is complete. */
  unresolvedForFiles(fileIds: readonly number[]): UnresolvedRef[];
  /** One indexed file by repo-relative path; `null` means "look in `rowsForPaths`". */
  fileByPath(path: string): IndexedFileRow | null;
  /** Every indexed file in path order: the staleness manifest. */
  allFiles(): IndexedFileRow[];
  /** `{language, files, parsed, failed}` per language, `not_indexed` folded in. */
  censusByLanguage(): LanguageCensus[];
  /** Identity and health; never the environment, never the filesystem. */
  metaFor(): GraphMeta;
  /** The newest publish by `seq`; `null` before the first publish. */
  latestGeneration(): GenerationRow | null;
  /**
   * Symbols removed by the publish named `sinceGeneration`, over `tombstone(qn)`.
   * `DATABASE.md` §6 fixes this query as an equality on `removed_in_generation`, so a caller
   * comparing against a base asks for the generation the removals were recorded in.
   */
  removedSymbolsFor(sinceGeneration: string): TombstoneRow[];
  /** Entry points by reachable count descending, over `idx_metrics_entry`. */
  entryPoints(limit: number): EntryPointRow[];
  /** Top symbols by `in_degree + out_degree` descending, ties by `id`. */
  hotspots(limit: number): HotspotRow[];
  /** Gaps among `ids` where `tested_depth` is `NULL` or greater than {@link TEST_GAP_MAX_DEPTH}. */
  testGaps(ids: readonly number[]): TestGapRow[];
  /**
   * FTS5 `MATCH`, ordered by `bm25` ascending, then `in_degree` descending, then `id` ascending.
   * That triple is what makes a cursor page boundary stable inside one generation
   * (`DATABASE.md` §8): the ordering key never changes between the first page and the last.
   */
  readNodeFts(matchQuery: string, limit: number): FtsHit[];
}

/** `?, ?, ?` for an `IN (...)` list; the arity is part of the cache key, never a per-call loop. */
function placeholders(count: number): string {
  return new Array<string>(count).fill("?").join(", ");
}

interface MetaSqlRow {
  readonly key: string;
  readonly value: string;
}

interface PublishedAtSqlRow {
  readonly published_at: string;
}

interface GenerationSqlRow {
  readonly id: string;
  readonly seq: number;
  readonly published_at: string;
  readonly base_commit: string | null;
  readonly worktree_dirty: number;
  readonly files_indexed: number;
  readonly nodes: number;
  readonly edges: number;
  readonly duration_ms: number;
  readonly mode: string;
  readonly status: string;
}

interface TombstoneSqlRow {
  readonly qn: string;
  readonly kind: string;
  readonly file_id: number | null;
  readonly last_seen_generation: string;
  readonly removed_in_generation: string;
  readonly had_callers: number;
}

/** The identity columns every ranked row selects, so the four ranked queries stay comparable. */
const RANKED_NODE_SELECT =
  "n.id AS id, n.qn AS qn, n.kind AS kind, f.path AS path," +
  " n.start_line AS start_line, n.end_line AS end_line";

interface RankedNodeSqlRow {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly path: string;
  readonly start_line: number;
  readonly end_line: number;
}

const GENERATION_SELECT =
  "g.id AS id, g.seq AS seq, g.published_at AS published_at, g.base_commit AS base_commit," +
  " g.worktree_dirty AS worktree_dirty, g.files_indexed AS files_indexed, g.nodes AS nodes," +
  " g.edges AS edges, g.duration_ms AS duration_ms, g.mode AS mode, g.status AS status";

function toGenerationRow(row: GenerationSqlRow): GenerationRow {
  return {
    id: row.id,
    seq: row.seq,
    publishedAt: row.published_at,
    baseCommit: row.base_commit,
    worktreeDirty: row.worktree_dirty !== 0,
    filesIndexed: row.files_indexed,
    nodes: row.nodes,
    edges: row.edges,
    durationMs: row.duration_ms,
    mode: row.mode,
    status: row.status,
  };
}

function toLineRange(row: RankedNodeSqlRow): LineRange {
  return [row.start_line, row.end_line];
}

/**
 * `meta.consecutive_failures` is written by the indexer, so a non-integer is a writer bug and
 * raises instead of degrading to `0`: "no failures" and "unreadable counter" are different
 * answers to `status.health`, and only one of them is true.
 */
function failureCount(raw: string | undefined): number {
  if (raw === undefined) {
    return 0;
  }
  const value = Number(raw);
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new AppError(
      "internal",
      `meta.consecutive_failures is not a non-negative integer: ${raw}`,
      {
        hint: "re-index the database; the counter is written by the indexer",
      },
    );
  }
  return value;
}

/**
 * Binds `conn` once and returns the reader a tool sees. Nothing else in the process needs to know
 * about the handle, and nothing a handler does with the result can write.
 */
export function makeReader(conn: Conn): GraphReader {
  /** `meta` is five keys on one row each; one query per call is one index lookup per key. */
  const metaValues = (): Map<string, string> => {
    const rows = prepared(conn, "reader.meta", (sqlite) =>
      sqlite.query<MetaSqlRow, []>(
        "SELECT m.key AS key, m.value AS value FROM meta m WHERE m.key IN" +
          " ('project_name', 'project_root', 'generation', 'indexer_state', 'consecutive_failures')",
      ),
    ).all();
    return new Map(rows.map((row) => [row.key, row.value]));
  };

  const publishedAt = (generationId: string): string | null => {
    const row = prepared(conn, "reader.publishedAt", (sqlite) =>
      sqlite.query<PublishedAtSqlRow, [string]>(
        "SELECT g.published_at AS published_at FROM generation g WHERE g.id = ?",
      ),
    ).get(generationId);
    return row === null ? null : row.published_at;
  };

  const testGaps = (ids: readonly number[]): TestGapRow[] => {
    if (ids.length === 0) {
      return [];
    }
    const rows = prepared(conn, `reader.testGaps/${ids.length}`, (sqlite) =>
      sqlite.query<RankedNodeSqlRow & { tested_depth: number | null }, (number | string)[]>(
        `SELECT ${RANKED_NODE_SELECT}, m.tested_depth AS tested_depth FROM node_metrics m` +
          " JOIN node n ON n.id = m.node_id JOIN file f ON f.id = n.file_id" +
          ` WHERE n.id IN (${placeholders(ids.length)})` +
          " AND (m.tested_depth IS NULL OR m.tested_depth > ?)" +
          " ORDER BY (m.tested_depth IS NULL) DESC, m.tested_depth DESC, n.id ASC",
      ),
    ).all(...ids, TEST_GAP_MAX_DEPTH);
    return rows.map((row) => ({
      id: row.id,
      qn: row.qn,
      kind: row.kind,
      file: row.path,
      lines: toLineRange(row),
      testedDepth: row.tested_depth,
    }));
  };

  return {
    findByQn: (qn) => findByQn(conn, qn),
    findByName: (name, kinds) => findByName(conn, name, kinds),
    members: (parentId) => members(conn, parentId),
    nodesByFile: (fileId) => nodesByFile(conn, fileId),
    inbound: (nodeId, kinds) => inbound(conn, nodeId, kinds),
    outbound: (nodeId, kinds) => outbound(conn, nodeId, kinds),
    metricsFor: (nodeId) => metricsFor(conn, nodeId),
    rowsForPaths: (prefixes) => rowsForPaths(conn, prefixes),
    unresolvedForFiles: (fileIds) => unresolvedForFiles(conn, fileIds),
    fileByPath: (path) => fileByPath(conn, path),
    allFiles: () => allFiles(conn),
    censusByLanguage: () => censusByLanguage(conn),

    metaFor(): GraphMeta {
      const values = metaValues();
      const generation = values.get("generation") ?? null;
      return {
        projectName: values.get("project_name") ?? null,
        projectRoot: values.get("project_root") ?? null,
        generation,
        // `meta` has no publish timestamp; the generation row is the record of when it happened.
        indexedAt: generation === null ? null : publishedAt(generation),
        indexerState: values.get("indexer_state") ?? null,
        consecutiveFailures: failureCount(values.get("consecutive_failures")),
      };
    },

    latestGeneration(): GenerationRow | null {
      const row = prepared(conn, "reader.latestGeneration", (sqlite) =>
        sqlite.query<GenerationSqlRow, []>(
          `SELECT ${GENERATION_SELECT} FROM generation g ORDER BY g.seq DESC LIMIT 1`,
        ),
      ).get();
      return row === null ? null : toGenerationRow(row);
    },

    removedSymbolsFor(sinceGeneration: string): TombstoneRow[] {
      return prepared(conn, "reader.tombstones", (sqlite) =>
        sqlite.query<TombstoneSqlRow, [string]>(
          "SELECT t.qn AS qn, t.kind AS kind, t.file_id AS file_id," +
            " t.last_seen_generation AS last_seen_generation," +
            " t.removed_in_generation AS removed_in_generation, t.had_callers AS had_callers" +
            " FROM tombstone t WHERE t.removed_in_generation = ? ORDER BY t.qn",
        ),
      )
        .all(sinceGeneration)
        .map((row) => ({
          qn: row.qn,
          kind: row.kind,
          fileId: row.file_id,
          lastSeenGeneration: row.last_seen_generation,
          removedInGeneration: row.removed_in_generation,
          hadCallers: row.had_callers !== 0,
        }));
    },

    entryPoints(limit: number): EntryPointRow[] {
      const rows = prepared(conn, "reader.entryPoints", (sqlite) =>
        sqlite.query<RankedNodeSqlRow & { reachable_count: number | null }, [number]>(
          `SELECT ${RANKED_NODE_SELECT}, m.reachable_count AS reachable_count` +
            " FROM node_metrics m JOIN node n ON n.id = m.node_id" +
            " JOIN file f ON f.id = n.file_id WHERE m.is_entry = 1" +
            " ORDER BY (m.reachable_count IS NULL) ASC, m.reachable_count DESC, n.id ASC LIMIT ?",
        ),
      ).all(limit);
      return rows.map((row) => ({
        id: row.id,
        qn: row.qn,
        kind: row.kind,
        file: row.path,
        lines: toLineRange(row),
        reachable: row.reachable_count,
      }));
    },

    hotspots(limit: number): HotspotRow[] {
      const rows = prepared(conn, "reader.hotspots", (sqlite) =>
        sqlite.query<RankedNodeSqlRow & { in_degree: number; out_degree: number }, [number]>(
          `SELECT ${RANKED_NODE_SELECT}, m.in_degree AS in_degree, m.out_degree AS out_degree` +
            " FROM node_metrics m JOIN node n ON n.id = m.node_id" +
            " JOIN file f ON f.id = n.file_id" +
            " ORDER BY (m.in_degree + m.out_degree) DESC, n.id ASC LIMIT ?",
        ),
      ).all(limit);
      return rows.map((row) => ({
        id: row.id,
        qn: row.qn,
        kind: row.kind,
        file: row.path,
        lines: toLineRange(row),
        inDegree: row.in_degree,
        outDegree: row.out_degree,
      }));
    },

    testGaps,

    readNodeFts(matchQuery: string, limit: number): FtsHit[] {
      // `node_fts` is a virtual table, so Drizzle cannot model it and the statement stays raw.
      // `bm25()` and `MATCH` are the two things only this module may run: the ranking is part of
      // the contract, and a caller that re-sorted would break the cursor's stable ordering.
      const rows = prepared(conn, "reader.nodeFts", (sqlite) =>
        sqlite.query<RankedNodeSqlRow & { score: number }, [string, number]>(
          `SELECT ${RANKED_NODE_SELECT}, bm25(node_fts) AS score FROM node_fts` +
            " JOIN node n ON n.id = node_fts.rowid JOIN file f ON f.id = n.file_id" +
            " LEFT JOIN node_metrics m ON m.node_id = n.id" +
            " WHERE node_fts MATCH ?" +
            " ORDER BY bm25(node_fts) ASC, COALESCE(m.in_degree, 0) DESC, n.id ASC LIMIT ?",
        ),
      ).all(matchQuery, limit);
      return rows.map((row) => ({
        id: row.id,
        qn: row.qn,
        kind: row.kind,
        file: row.path,
        lines: toLineRange(row),
        score: row.score,
      }));
    },
  };
}
