/**
 * The persist pass: one unit's rows into the stage, then the values the query path must never
 * recompute (`DATABASE.md` §5).
 *
 * Two functions, called in that order inside the caller's transaction:
 * - {@link persistUnit} replaces one file's contribution — its nodes, edges, unresolved sites and
 *   coverage rows — deleting by `file_id` first, because a `file` row is the staleness manifest and
 *   outlives a re-parse, so `ON DELETE CASCADE` from `file` would be the wrong tool;
 * - {@link finalizeGeneration} derives what only a whole graph can answer: `tested_by` edges,
 *   `tested_depth`, degrees, the FTS rowid delta, tombstones and the `generation` row.
 *
 * Neither function opens a connection. Both take the caller's `WriteTx` and reach its handle
 * through {@link handleOf}, so the stage is written in one transaction and a failure anywhere is a
 * discard, never a partial publish.
 *
 * ## `tested_by` runs test → symbol, the inverse of GRAPH-MODEL.md §4
 *
 * §4 catalogues `TESTED_BY` as *function → test function*, and this code deliberately inverts it:
 * the source is the test node and the target is the symbol it exercises. The reason is the read
 * path — `TOOLS-SPEC.md` §3 renders `tested_by` inside the **inbound** group of the queried symbol,
 * and an edge whose target is the test would put every test under the test's own inbound group and
 * leave the tested function looking untested. The inversion is the whole point, so it is stated
 * here rather than discovered by whoever next compares this file with §4.
 *
 * `tested_depth` falls out of the same derivation and is stored, not walked: 1 for a node a test
 * calls directly, 2 for a node one further `calls` hop away, `NULL` otherwise. That makes
 * `summary`'s `test_gaps` a column read (`tested_depth IS NULL OR tested_depth > 2`) instead of a
 * BFS per query.
 *
 * ## A symbol that disappears is journalled in the tombstone table
 *
 * `finalizeGeneration` receives only `{generationId, baseCommit, mode}`, so the disappeared set has
 * to be durable rather than passed along. {@link persistUnit} records every qn it drops as a
 * tombstone row whose `removed_in_generation` is {@link PENDING_GENERATION} — the previous symbol
 * set is only visible at delete time, and a per-run in-memory set would silently skip metrics and
 * tombstones for a caller that persists in several transactions. `finalizeGeneration` stamps those
 * pending rows with the new generation id and prunes the ones whose qn came back.
 */
import { Database } from "bun:sqlite";
import { AppError } from "#shared/errors";
import { log } from "#shared/log";
import { normalizeRel } from "#shared/paths";
import type { WriteTx } from "#shared/store/publish";
import type { SkipReason } from "./discover";
import type { BindResult, FileUnit, ResolvedEdge } from "./resolve";

/** `file.parse_status`; the one closed set `DATABASE.md` §3.2 declares. */
export type ParseStatus = "ok" | "partial" | "failed";

/**
 * Why a path is not in the graph: the reasons discovery finds plus the two only parsing can
 * produce and the symlink case the walk may meet later (`DATABASE.md` §3.2).
 */
export type NotIndexedReason = SkipReason | "grammar_unavailable" | "parse_failed" | "symlink";

/** One coverage row: a path the indexer looked at and did not index, with the reason. */
export interface NotIndexedEntry {
  readonly path: string;
  readonly reason: NotIndexedReason;
  readonly detail?: string | null;
  readonly language?: string | null;
}

/**
 * One unit as the worker hands it over: the extractor's `FileUnit` plus the coverage rows parsing
 * discovered for this path. The `file` row itself is the caller's business — it owns the manifest
 * facts (`sha256`, `size`, `line_count`, `parse_status`) the incremental planner compares — so this
 * module only reads its id.
 */
export interface PersistUnit extends FileUnit {
  /** Rows for **this** path only; a repo-wide discovery list belongs to the worker. */
  readonly notIndexed?: readonly NotIndexedEntry[];
}

/** What one {@link persistUnit} call wrote, for the worker's stderr log. */
export interface PersistCounts {
  readonly fileId: number;
  readonly nodes: number;
  readonly edges: number;
  readonly unresolved: number;
  /** Sites whose other end has no row: dropped rather than written as a dangling edge. */
  readonly skippedEdges: number;
  /** qns this unit dropped, recorded as pending tombstones. */
  readonly pendingTombstones: number;
}

/** Everything {@link finalizeGeneration} needs besides the transaction. */
export interface GenerationInput {
  readonly generationId: string;
  readonly baseCommit: string | null;
  /** `full` for a forced index, `fast` for an incremental publish. */
  readonly mode: "full" | "fast";
  /** Injected clock for `generation.published_at`; defaults to the current instant. */
  readonly publishedAt?: string;
  /** Wall time the run took; the worker owns the timing, this module only records it. */
  readonly durationMs?: number;
  readonly worktreeDirty?: boolean;
}

/** Which way the FTS mirror was brought up to date, and why. */
export interface FtsReport {
  readonly mode: "delta" | "rebuild";
  /** Mirror rows added (the nodes that had none) or, on a rebuild, every node. */
  readonly inserted: number;
  /** Mirror rows dropped because their node is gone. */
  readonly removed: number;
  /** `node` rows at the time of the decision: the denominator of {@link fraction}. */
  readonly rows: number;
  /** `(inserted + removed) / rows` before the decision; above the threshold means a rebuild. */
  readonly fraction: number;
}

/** Which nodes the degree pass visited: the touched neighbourhood, or the whole table. */
export interface MetricsReport {
  readonly mode: "neighbourhood" | "all";
  readonly rows: number;
}

export interface GenerationResult {
  /** `count(node)` at publish. */
  readonly nodes: number;
  /** `count(edge)` at publish, derived `tested_by` edges included. */
  readonly edges: number;
  /** qns this generation removed, in qn order: `tombstones.length` is the count. */
  readonly tombstones: readonly string[];
  readonly fts: FtsReport;
  readonly metrics: MetricsReport;
}

/**
 * The `removed_in_generation` of a tombstone that is not a removal yet, and the marker a symbol
 * which vanished in this run carries until {@link finalizeGeneration} names the generation. An
 * empty value cannot collide with a generation id, and the stage is discarded on failure, so a
 * pending row never reaches a published graph.
 */
export const PENDING_GENERATION = "";

/**
 * Above this fraction of `node` rows, applying the FTS delta row by row costs more than dropping
 * the index and rebuilding it from the mirror (`DATABASE.md` §5).
 */
export const FTS_REBUILD_FRACTION = 0.35;

/**
 * Above this many touched nodes the two-hop expansion visits more rows than the whole table, so
 * the degree pass stops pretending it is a neighbourhood.
 */
const NEIGHBOURHOOD_LIMIT = 5_000;

/** Bound on one `IN (...)` list, well under SQLite's variable ceiling. */
const ID_BATCH = 400;

const META_GENERATION = "generation";
const META_GENERATION_SEQ = "generation_seq";

/** The degree of a node is a count over its own edges, so it is one correlated subquery each. */
const DEGREE_UPDATE =
  "UPDATE node_metrics SET" +
  " in_degree = (SELECT count(*) FROM edge e WHERE e.dst_id = node_metrics.node_id)," +
  " out_degree = (SELECT count(*) FROM edge e WHERE e.src_id = node_metrics.node_id)";

/** The five columns the FTS5 index and its mirror share, in table order. */
const MIRROR_COLUMNS = "rowid_, name, qn, signature, docstring, props_text";

interface MirrorRow {
  readonly rowid_: number;
  readonly name: string;
  readonly qn: string;
  readonly signature: string | null;
  readonly docstring: string | null;
  readonly props_text: string | null;
}

/**
 * `tested_by`, derived in one statement: a test node's outgoing `calls` edge into a non-test node
 * is a test of that node. Direction and tier are the contract's (see the module comment); one edge
 * per `(test, symbol)` pair, so a test that calls a helper twice does not report two tests.
 */
const TESTED_BY_INSERT =
  "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)" +
  " SELECT c.src_id, c.dst_id, 'tested_by', 'inferred', min(c.file_id), min(c.line), '', 'tested_by'" +
  " FROM edge c" +
  " JOIN node t ON t.id = c.src_id AND t.is_test = 1" +
  " JOIN node d ON d.id = c.dst_id AND d.is_test = 0" +
  " WHERE c.kind = 'calls'" +
  " GROUP BY c.src_id, c.dst_id" +
  " ON CONFLICT DO NOTHING";

/** `SELECT ?` lists, batched: the arity is part of the cached statement, never a per-call loop. */
function placeholders(count: number): string {
  return new Array<string>(count).fill("?").join(", ");
}

function batch<T>(items: readonly T[], size: number): T[][] {
  const batches: T[][] = [];
  for (let start = 0; start < items.length; start += size) {
    batches.push(items.slice(start, start + size));
  }
  return batches;
}

/**
 * The `bun:sqlite` handle behind the Drizzle transaction.
 *
 * This pass is statement-shaped — per-node upserts, FTS5's `delete`/`rebuild` commands, anti-join
 * deltas — and `drizzle-orm` may not be imported outside `src/shared/store/**` (boundary rule 4),
 * so the raw handle is the honest route. Drizzle keeps it in a private `session.client`; one cast
 * here, checked at runtime, is the whole coupling. A drizzle upgrade that renames the field fails
 * loudly at the first persist instead of writing through the wrong object.
 */
function handleOf(tx: WriteTx): Database {
  const session = (tx as unknown as { session?: { client?: unknown } }).session;
  const client = session?.client;
  if (!(client instanceof Database)) {
    throw new AppError("internal", "the write transaction does not expose its bun:sqlite handle", {
      hint: "this module reads session.client from the transaction; update handleOf if drizzle-orm moves it",
    });
  }
  return client;
}

function readMeta(handle: Database, key: string): string | null {
  const row = handle
    .query<{ value: string }, [string]>("SELECT value FROM meta WHERE key = ?")
    .get(key);
  return row?.value ?? null;
}

function writeMeta(handle: Database, key: string, value: string): void {
  handle
    .query<unknown, [string, string]>(
      "INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .run(key, value);
}

function countRows(handle: Database, table: "node" | "edge" | "file"): number {
  const row = handle.query<{ rows: number }, []>(`SELECT count(*) AS rows FROM ${table}`).get();
  return row?.rows ?? 0;
}

/**
 * Writes one file's rows into the stage: the file's previous contribution is deleted by `file_id`
 * first, then nodes, containment parents, edges, unresolved sites and coverage rows are written.
 *
 * The unit is authoritative for what the file declares. `resolved.edges` is the only edge input:
 * the bind pass already decided which raw sites became edges and at which tier, and an edge whose
 * other end has no node row is dropped here rather than written (a dangling edge violates the
 * schema's contract; `resolve.ts` does not produce one, and a mismatch means the unit set handed
 * over is inconsistent).
 */
export function persistUnit(tx: WriteTx, unit: PersistUnit, resolved: BindResult): PersistCounts {
  const handle = handleOf(tx);
  const relPath = normalizeRel(unit.relPath);

  const fileRow = handle
    .query<{ id: number }, [string]>("SELECT id FROM file WHERE path = ?")
    .get(relPath);
  if (fileRow === null) {
    throw new AppError("internal", `no file row for ${relPath}`, {
      hint: "write the file row (sha256, size, line count, parse status) before persisting its unit",
    });
  }
  const fileId = fileRow.id;

  // Which nodes this file contributed before, and how many callers each had: both facts are only
  // visible before the delete, and both are needed for the tombstone.
  const previous = handle
    .query<{ id: number; qn: string; kind: string }, [number]>(
      "SELECT id, qn, kind FROM node WHERE file_id = ? ORDER BY id",
    )
    .all(fileId);
  // `had_callers` is inbound edges that are not structural or derived: a `contains` edge is
  // containment and a `tested_by` edge is produced from a `calls` edge, so counting either would
  // report a caller the symbol does not have.
  const inbound = handle.query<{ rows: number }, [number]>(
    "SELECT count(*) AS rows FROM edge WHERE dst_id = ? AND kind NOT IN ('contains', 'tested_by')",
  );
  const removed = previous.map((row) => ({
    qn: row.qn,
    kind: row.kind,
    callers: inbound.get(row.id)?.rows ?? 0,
  }));

  // Edges are deleted by `file_id` (the site's file) before the nodes: an edge extracted from this
  // file whose target lives elsewhere does not cascade with this file's node rows.
  handle.query<unknown, [number]>("DELETE FROM edge WHERE file_id = ?").run(fileId);
  handle.query<unknown, [number]>("DELETE FROM unresolved WHERE file_id = ?").run(fileId);
  handle.query<unknown, [number]>("DELETE FROM node WHERE file_id = ?").run(fileId);

  // `qn` embeds the path, so a second row carrying this qn means the same file was handed over as
  // two units. Adopting that id would leave `node_fts_state` describing the old values (the delta
  // keys on ids, and a rewritten node is supposed to get a new one), so the stale row goes: the
  // write stays deterministic and the FTS delta sees one delete and one insert.
  const replaceQn = handle.query<unknown, [string, number]>(
    "DELETE FROM node WHERE qn = ? AND file_id <> ?",
  );
  const insertNode = handle.query<
    { id: number },
    [number, string, string, string, number, number, string | null, number, string]
  >(
    "INSERT INTO node (file_id, kind, name, qn, start_line, end_line, signature, is_test, props)" +
      " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING id",
  );
  const ids = new Map<string, number>();
  for (const node of unit.nodes) {
    if (ids.has(node.qn)) {
      continue;
    }
    replaceQn.run(node.qn, fileId);
    const row = insertNode.get(
      fileId,
      node.kind,
      node.name,
      node.qn,
      node.startLine,
      node.endLine,
      node.signature,
      node.isTest ? 1 : 0,
      JSON.stringify(node.props),
    );
    if (row === null) {
      throw new AppError("internal", `inserting ${node.qn} returned no id`);
    }
    ids.set(node.qn, row.id);
  }

  // Containment is the only source of `parent_id`: the extractor emits it as a `contains` edge, so
  // the hierarchy is read from the edges the bind pass kept rather than re-derived from qn text.
  const setParent = handle.query<unknown, [number, number]>(
    "UPDATE node SET parent_id = ? WHERE id = ?",
  );
  const fileIdOf = new Map<string, number>([[relPath, fileId]]);
  const fileIdByPath = handle.query<{ id: number }, [string]>("SELECT id FROM file WHERE path = ?");
  const nodeIdByQn = handle.query<{ id: number }, [string]>("SELECT id FROM node WHERE qn = ?");

  const edgeFile = (edge: ResolvedEdge): number => {
    const path = normalizeRel(edge.file);
    const cached = fileIdOf.get(path);
    if (cached !== undefined) {
      return cached;
    }
    const row = fileIdByPath.get(path);
    if (row === null) {
      throw new AppError("internal", `edge site names ${path}, which has no file row`, {
        hint: "every edge carries the file its site sits in; persist that file before the unit that references it",
      });
    }
    fileIdOf.set(path, row.id);
    return row.id;
  };

  for (const edge of resolved.edges) {
    if (edge.kind !== "contains") {
      continue;
    }
    const parent = ids.get(edge.src);
    const child = ids.get(edge.dst);
    if (parent !== undefined && child !== undefined) {
      setParent.run(parent, child);
    }
  }

  const insertEdge = handle.query<
    unknown,
    [number, number, string, string, number, number, string, string | null]
  >(
    "INSERT INTO edge (src_id, dst_id, kind, tier, file_id, line, local_name, evidence)" +
      " VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
  );
  let edges = 0;
  let skippedEdges = 0;
  for (const edge of resolved.edges) {
    const src = ids.get(edge.src) ?? nodeIdByQn.get(edge.src)?.id;
    const dst = ids.get(edge.dst) ?? nodeIdByQn.get(edge.dst)?.id;
    if (src === undefined || dst === undefined) {
      skippedEdges += 1;
      continue;
    }
    insertEdge.run(
      src,
      dst,
      edge.kind,
      edge.tier,
      edgeFile(edge),
      edge.line,
      edge.localName,
      edge.evidence,
    );
    edges += 1;
  }

  // The extractor's own dynamic sites and the bind pass's failures are the same fact for the
  // reader: a site that names nothing the graph can bind.
  const insertUnresolved = handle.query<unknown, [number, number, string, string, string | null]>(
    "INSERT INTO unresolved (file_id, line, raw_target, evidence_kind, evidence)" +
      " VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
  );
  let unresolved = 0;
  for (const site of [...unit.unresolved, ...resolved.unresolved]) {
    insertUnresolved.run(fileId, site.line, site.rawTarget, site.evidenceKind, site.evidence);
    unresolved += 1;
  }

  // A persisted path is in the graph, so any coverage row claiming otherwise is stale; the rows the
  // caller supplies are the failures parsing found while producing this unit.
  handle.query<unknown, [string]>("DELETE FROM not_indexed WHERE path = ?").run(relPath);
  const insertNotIndexed = handle.query<unknown, [string, string, string | null, string | null]>(
    "INSERT INTO not_indexed (path, reason, detail, language) VALUES (?, ?, ?, ?)" +
      " ON CONFLICT DO NOTHING",
  );
  for (const entry of unit.notIndexed ?? []) {
    const path = normalizeRel(entry.path);
    if (path !== relPath) {
      throw new AppError(
        "internal",
        `not_indexed entry for ${path} inside the unit for ${relPath}`,
        {
          hint: "a unit owns the coverage rows for its own path; discovery's repo-wide list is the worker's",
        },
      );
    }
    insertNotIndexed.run(path, entry.reason, entry.detail ?? null, entry.language ?? null);
  }

  // Tombstones: a qn that was here and is not any more. `last_seen_generation` is still the
  // previous generation at this point, which is exactly what the column means.
  const lastSeen = readMeta(handle, META_GENERATION) ?? PENDING_GENERATION;
  const insertTombstone = handle.query<unknown, [string, string, number, string, string, number]>(
    "INSERT INTO tombstone (qn, kind, file_id, last_seen_generation, removed_in_generation, had_callers)" +
      " VALUES (?, ?, ?, ?, ?, ?)" +
      " ON CONFLICT(qn) DO UPDATE SET kind = excluded.kind, file_id = excluded.file_id," +
      " last_seen_generation = excluded.last_seen_generation," +
      " removed_in_generation = excluded.removed_in_generation," +
      " had_callers = excluded.had_callers",
  );
  let pendingTombstones = 0;
  for (const gone of removed) {
    if (ids.has(gone.qn)) {
      continue;
    }
    insertTombstone.run(gone.qn, gone.kind, fileId, lastSeen, PENDING_GENERATION, gone.callers);
    pendingTombstones += 1;
  }
  // A qn that exists again is not a removal, whatever an earlier unit of this run recorded: the same
  // file handed over twice, or a symbol that came back, leaves no tombstone behind.
  handle
    .query<unknown, [string]>(
      "DELETE FROM tombstone WHERE removed_in_generation = ? AND qn IN (SELECT qn FROM node)",
    )
    .run(PENDING_GENERATION);

  log.debug("unit persisted", {
    file: relPath,
    fileId,
    nodes: ids.size,
    edges,
    unresolved,
    skippedEdges,
    pendingTombstones,
  });

  return {
    fileId,
    nodes: ids.size,
    edges,
    unresolved,
    skippedEdges,
    pendingTombstones,
  };
}

/**
 * The nodes whose degree has to be recounted: the touched set plus two hops of `edge` around it.
 * One hop is the degree a re-written edge moves at its far end; the second is the neighbourhood a
 * `tested_by` edge and a `tested_depth = 2` value are read from.
 */
function neighbourhood(handle: Database, touched: readonly number[]): number[] {
  const dirty = new Set<number>(touched);
  for (let hop = 0; hop < 2; hop += 1) {
    for (const chunk of batch([...dirty], ID_BATCH)) {
      const list = placeholders(chunk.length);
      const inbound = handle
        .query<{ id: number }, number[]>(
          `SELECT DISTINCT src_id AS id FROM edge WHERE dst_id IN (${list})`,
        )
        .all(...chunk);
      const outbound = handle
        .query<{ id: number }, number[]>(
          `SELECT DISTINCT dst_id AS id FROM edge WHERE src_id IN (${list})`,
        )
        .all(...chunk);
      for (const row of [...inbound, ...outbound]) {
        dirty.add(row.id);
      }
    }
  }
  return [...dirty];
}

/**
 * Recounts degrees for `ids`, in batches so one statement never carries an unbounded parameter
 * list. A statement per arity is the cached shape, not a query built per node.
 */
function recountDegrees(handle: Database, ids: readonly number[]): void {
  for (const chunk of batch(ids, ID_BATCH)) {
    handle
      .query<unknown, number[]>(`${DEGREE_UPDATE} WHERE node_id IN (${placeholders(chunk.length)})`)
      .run(...chunk);
  }
}

/**
 * Brings `node_fts` in line with `node`, by rowid.
 *
 * The delta is derivable because node ids are never reused: a rewritten file deletes its old rows
 * and inserts new ones, so a node that changed has a new id. That leaves exactly two sets — mirror
 * rows whose node is gone (deleted with FTS5's `delete` command, which needs the old column values
 * and therefore the mirror) and nodes with no mirror row (inserted) — and no bookkeeping between
 * calls. A rewritten row therefore counts twice, once on each side; the fraction below is measured
 * against that, which errs towards rebuilding rather than towards a stale index. Above
 * {@link FTS_REBUILD_FRACTION} the row-by-row cost exceeds a rebuild from the mirror, which is what
 * `DATABASE.md` §5 asks for.
 */
function syncFts(handle: Database): FtsReport {
  const rows = countRows(handle, "node");
  const stale = handle
    .query<{ rowid_: number }, []>(
      "SELECT s.rowid_ AS rowid_ FROM node_fts_state s" +
        " WHERE NOT EXISTS (SELECT 1 FROM node n WHERE n.id = s.rowid_) ORDER BY s.rowid_",
    )
    .all();
  const missing = handle
    .query<MirrorRow, []>(
      "SELECT n.id AS rowid_, n.name AS name, n.qn AS qn, n.signature AS signature," +
        " n.docstring AS docstring, n.props AS props_text FROM node n" +
        " WHERE NOT EXISTS (SELECT 1 FROM node_fts_state s WHERE s.rowid_ = n.id) ORDER BY n.id",
    )
    .all();
  const fraction = rows === 0 ? 0 : (stale.length + missing.length) / rows;

  if (fraction > FTS_REBUILD_FRACTION) {
    // Rebuild from the mirror, not from `node`: the content table of an external-content FTS5
    // index is `node_fts_state`, so it has to be complete before `rebuild` reads it.
    handle.query<unknown, []>("DELETE FROM node_fts_state").run();
    const insertMirror = handle.query<
      unknown,
      [number, string, string, string | null, string | null, string | null]
    >(`INSERT INTO node_fts_state (${MIRROR_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?)`);
    const all = handle
      .query<MirrorRow, []>(
        "SELECT n.id AS rowid_, n.name AS name, n.qn AS qn, n.signature AS signature," +
          " n.docstring AS docstring, n.props AS props_text FROM node n ORDER BY n.id",
      )
      .all();
    for (const row of all) {
      insertMirror.run(row.rowid_, row.name, row.qn, row.signature, row.docstring, row.props_text);
    }
    handle.query<unknown, []>("INSERT INTO node_fts (node_fts) VALUES ('rebuild')").run();
    log.debug("fts rebuilt", { rows, stale: stale.length, fraction });
    return { mode: "rebuild", inserted: all.length, removed: stale.length, rows, fraction };
  }

  const mirrorOf = handle.query<MirrorRow, [number]>(
    `SELECT ${MIRROR_COLUMNS} FROM node_fts_state WHERE rowid_ = ?`,
  );
  const deleteFts = handle.query<
    unknown,
    [number, string, string, string | null, string | null, string | null]
  >(
    "INSERT INTO node_fts (node_fts, rowid, name, qn, signature, docstring, props_text)" +
      " VALUES ('delete', ?, ?, ?, ?, ?, ?)",
  );
  const deleteMirror = handle.query<unknown, [number]>(
    "DELETE FROM node_fts_state WHERE rowid_ = ?",
  );
  for (const row of stale) {
    const mirror = mirrorOf.get(row.rowid_);
    if (mirror === null) {
      continue;
    }
    deleteFts.run(
      mirror.rowid_,
      mirror.name,
      mirror.qn,
      mirror.signature,
      mirror.docstring,
      mirror.props_text,
    );
    deleteMirror.run(mirror.rowid_);
  }

  const insertMirror = handle.query<
    unknown,
    [number, string, string, string | null, string | null, string | null]
  >(`INSERT INTO node_fts_state (${MIRROR_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?)`);
  const insertFts = handle.query<
    unknown,
    [number, string, string, string | null, string | null, string | null]
  >(
    "INSERT INTO node_fts (rowid, name, qn, signature, docstring, props_text)" +
      " VALUES (?, ?, ?, ?, ?, ?)",
  );
  for (const row of missing) {
    insertMirror.run(row.rowid_, row.name, row.qn, row.signature, row.docstring, row.props_text);
    insertFts.run(row.rowid_, row.name, row.qn, row.signature, row.docstring, row.props_text);
  }

  log.debug("fts delta applied", {
    rows,
    inserted: missing.length,
    removed: stale.length,
    fraction,
  });
  return {
    mode: "delta",
    inserted: missing.length,
    removed: stale.length,
    rows,
    fraction,
  };
}

/**
 * Seals a generation: derives `tested_by`, `tested_depth` and degrees, applies the FTS delta, names
 * the tombstones and records the `generation` row plus `meta.generation`.
 *
 * Called once per run, inside the same transaction as the persist calls, before `seal`/`publish`.
 */
export function finalizeGeneration(tx: WriteTx, input: GenerationInput): GenerationResult {
  const handle = handleOf(tx);
  const generationId = input.generationId;
  if (generationId === PENDING_GENERATION) {
    throw new AppError("internal", "a generation id may not be the pending marker", {
      hint: `"${PENDING_GENERATION}" marks an unconfirmed tombstone, not a publish`,
    });
  }
  if (input.mode !== "full" && input.mode !== "fast") {
    throw new AppError("internal", `unknown generation mode ${String(input.mode)}`, {
      hint: "generation.mode is the closed set full | fast",
    });
  }
  const recorded = handle
    .query<{ rows: number }, [string]>("SELECT count(*) AS rows FROM generation WHERE id = ?")
    .get(generationId);
  if ((recorded?.rows ?? 0) > 0) {
    throw new AppError("internal", `generation ${generationId} is already recorded`, {
      hint: "a publish is recorded once; a retry starts from a new stage and a new id",
    });
  }

  // Which nodes were written in this run: a re-inserted node has a fresh id and therefore no
  // `node_metrics` row yet. That is the touched set, without any state passed between calls.
  const touched = handle
    .query<{ id: number }, []>(
      "SELECT n.id AS id FROM node n WHERE NOT EXISTS" +
        " (SELECT 1 FROM node_metrics m WHERE m.node_id = n.id) ORDER BY n.id",
    )
    .all()
    .map((row) => row.id);
  const pendingRemovals =
    handle
      .query<{ rows: number }, [string]>(
        "SELECT count(*) AS rows FROM tombstone WHERE removed_in_generation = ?",
      )
      .get(PENDING_GENERATION)?.rows ?? 0;

  // A removal deletes its incident edges by cascade, which changes the degree of nodes that are no
  // longer within reach of any touched node and whose previous adjacency is already gone. Rather
  // than pretend the neighbourhood set is complete, a run that removed a symbol recounts every row.
  const scope: MetricsReport["mode"] =
    pendingRemovals > 0 || touched.length > NEIGHBOURHOOD_LIMIT ? "all" : "neighbourhood";

  handle
    .query<unknown, []>("INSERT OR IGNORE INTO node_metrics (node_id) SELECT id FROM node")
    .run();

  // `tested_by` is derived, so it is rebuilt rather than patched: the calls edges it reads are the
  // authority, and a stale `tested_by` edge would claim a test that no longer calls anything.
  handle.query<unknown, []>("DELETE FROM edge WHERE kind = 'tested_by'").run();
  handle.query<unknown, []>(TESTED_BY_INSERT).run();

  // `tested_depth`: 1 for a node a test calls, 2 for a node one `calls` hop further out, NULL for
  // everything else — the column `summary`'s test_gaps reads.
  handle.query<unknown, []>("UPDATE node_metrics SET tested_depth = NULL").run();
  handle
    .query<unknown, []>(
      "UPDATE node_metrics SET tested_depth = 1" +
        " WHERE node_id IN (SELECT dst_id FROM edge WHERE kind = 'tested_by')",
    )
    .run();
  handle
    .query<unknown, []>(
      "UPDATE node_metrics SET tested_depth = 2 WHERE tested_depth IS NULL AND node_id IN" +
        " (SELECT c.dst_id FROM edge c JOIN node_metrics m ON m.node_id = c.src_id" +
        " WHERE c.kind = 'calls' AND m.tested_depth = 1)",
    )
    .run();

  let metrics: MetricsReport;
  if (scope === "all") {
    handle.query<unknown, []>(DEGREE_UPDATE).run();
    metrics = { mode: "all", rows: countRows(handle, "node") };
  } else {
    const dirty = neighbourhood(handle, touched);
    recountDegrees(handle, dirty);
    metrics = { mode: "neighbourhood", rows: dirty.length };
  }

  const fts = syncFts(handle);

  handle
    .query<unknown, [string, string]>(
      "UPDATE tombstone SET removed_in_generation = ? WHERE removed_in_generation = ?",
    )
    .run(generationId, PENDING_GENERATION);
  const tombstones = handle
    .query<{ qn: string }, [string]>(
      "SELECT qn FROM tombstone WHERE removed_in_generation = ? ORDER BY qn",
    )
    .all(generationId)
    .map((row) => row.qn);

  const nodes = countRows(handle, "node");
  const edges = countRows(handle, "edge");
  // The graph's file count, not this run's: the incremental planner owns "which files changed", and
  // the generation row records the graph it published (`nodes`/`edges` below are the same measure).
  const filesIndexed = countRows(handle, "file");
  const seq =
    handle
      .query<{ seq: number }, []>("SELECT coalesce(max(seq), 0) + 1 AS seq FROM generation")
      .get()?.seq ?? 1;

  handle
    .query<unknown, [string]>(
      "UPDATE generation SET status = 'superseded' WHERE status = 'published' AND id <> ?",
    )
    .run(generationId);
  handle
    .query<
      unknown,
      [string, number, string, string | null, number, number, number, number, number, string]
    >(
      "INSERT INTO generation (id, seq, published_at, base_commit, worktree_dirty, files_indexed," +
        " nodes, edges, duration_ms, mode, status)" +
        " VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'published')",
    )
    .run(
      generationId,
      seq,
      input.publishedAt ?? new Date().toISOString(),
      input.baseCommit,
      input.worktreeDirty === true ? 1 : 0,
      filesIndexed,
      nodes,
      edges,
      input.durationMs ?? 0,
      input.mode,
    );
  writeMeta(handle, META_GENERATION, generationId);
  writeMeta(handle, META_GENERATION_SEQ, String(seq));

  log.debug("generation finalized", {
    generationId,
    seq,
    nodes,
    edges,
    tombstones: tombstones.length,
    fts: fts.mode,
    metrics: metrics.mode,
  });

  return { nodes, edges, tombstones, fts, metrics };
}
