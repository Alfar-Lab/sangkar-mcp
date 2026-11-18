/**
 * Directional edge lookups: the two halves of `trace` and every hop of the `impact` BFS
 * (`DATABASE.md` §6), each served by its own covering index — `idx_edge_dst_kind` for
 * {@link inbound}, `idx_edge_src_kind` for {@link outbound}.
 *
 * The far end of the edge is joined in rather than left as an id. A BFS that received bare ids
 * would pay one extra round trip per hop to name what it found, which is the N+1 the prepared
 * path exists to avoid; the join itself is a primary-key lookup per row.
 *
 * Statements come from `prepared(conn, key, build)` and rows are mapped to
 * {@link NeighbourEdge} here — no Drizzle `$inferSelect` type crosses the boundary.
 *
 * The batched pair ({@link inboundMany}, {@link outboundMany}) exists because a walk's cost must
 * not scale with the number of nodes it visits: a depth-2 hop over a dense corpus asks about ~1 200
 * near-ids, and one statement per id spends the whole expansion budget on round trips. One
 * `IN (?, …)` statement asks about {@link EDGE_BATCH_SIZE} of them at once.
 */
import { AppError } from "#shared/errors";
import { type Conn, prepared } from "#shared/store/db";
import { NODE_SELECT, type NodeRow, type NodeSqlRow, toNodeRow } from "#shared/store/read/nodes";

/** The edge itself, with the columns a caller groups or reports on. */
export interface EdgeRow {
  readonly id: number;
  readonly srcId: number;
  readonly dstId: number;
  readonly kind: string;
  readonly tier: string;
  /** The file the edge was extracted from, which is not always the file of either endpoint. */
  readonly fileId: number;
  readonly line: number;
  readonly localName: string;
  readonly evidence: string | null;
}

/**
 * An edge seen from one endpoint: `node` is the far end — the source for {@link inbound}, the
 * destination for {@link outbound}.
 */
export interface NeighbourEdge {
  readonly edge: EdgeRow;
  readonly node: NodeRow;
}

/** Which endpoint was asked about, and therefore which endpoint is joined in. */
type Direction = "inbound" | "outbound";

/** Prefixed so an edge column can never collide with the `node` columns of {@link NODE_SELECT}. */
const EDGE_SELECT =
  "e.id AS edge_id, e.src_id AS edge_src_id, e.dst_id AS edge_dst_id, " +
  "e.kind AS edge_kind, e.tier AS edge_tier, e.file_id AS edge_file_id, " +
  "e.line AS edge_line, e.local_name AS edge_local_name, e.evidence AS edge_evidence";

/** The row the joined select produces: edge columns plus a whole node. */
interface IncidentSqlRow extends NodeSqlRow {
  readonly edge_id: number;
  readonly edge_src_id: number;
  readonly edge_dst_id: number;
  readonly edge_kind: string;
  readonly edge_tier: string;
  readonly edge_file_id: number;
  readonly edge_line: number;
  readonly edge_local_name: string;
  readonly edge_evidence: string | null;
}

/** `?, ?, ?` for an `IN (...)` list; the arity is part of the cache key, never a per-call loop. */
function placeholders(count: number): string {
  return new Array<string>(count).fill("?").join(", ");
}

/**
 * The near-side ids one batched statement asks about. Every batch is bound at exactly this arity,
 * so the `IN (?, …)` shape is `(direction, kind arity)` — a handful of entries in the 200-entry
 * statement cache, however many ids a walk visits. 200 stays far below SQLite's variable ceiling
 * (999 even on the oldest supported builds) and keeps each chunk one range scan per index seek.
 */
const EDGE_BATCH_SIZE = 200;

/**
 * One statement shape. The near-side predicate is assembled by the caller ({@link singlePredicate}
 * or {@link batchPredicate}) and the two interpolated values are a closed union and a count, never
 * caller input.
 *
 * `ordered` is false only for the batched lookup: the sort that puts a chunk in `(kind, tier, line,
 * qn)` order costs ~14 % of a dense walk, and the walk imposes its own total order on what it
 * collects anyway (`#shared/domain/neighbors` `compareReached`). A batch therefore arrives in scan
 * order — index order per near id — which is a fact a caller may not rely on.
 */
function incidentSql(
  direction: Direction,
  kindCount: number,
  nearPredicate: string,
  ordered: boolean,
): string {
  const far = direction === "inbound" ? "e.src_id" : "e.dst_id";
  const kinds = kindCount === 0 ? "" : ` AND e.kind IN (${placeholders(kindCount)})`;
  const order = ordered ? " ORDER BY e.kind, e.tier, e.line, n.qn" : "";
  return (
    `SELECT ${EDGE_SELECT}, ${NODE_SELECT} FROM edge e` +
    ` JOIN node n ON n.id = ${far} JOIN file f ON f.id = n.file_id` +
    ` WHERE ${nearPredicate}${kinds}${order}`
  );
}

/** The near side of a single-id lookup: the endpoint that was asked about, bound to one value. */
function singlePredicate(direction: Direction): string {
  return `${direction === "inbound" ? "e.dst_id" : "e.src_id"} = ?`;
}

/** The near side of a batched lookup: the same endpoint, bound to {@link EDGE_BATCH_SIZE} values. */
function batchPredicate(direction: Direction): string {
  return `${direction === "inbound" ? "e.dst_id" : "e.src_id"} IN (${placeholders(EDGE_BATCH_SIZE)})`;
}

/**
 * A chunk grown to the batch arity by repeating an id **from that same chunk**. Two consequences,
 * both load-bearing:
 *
 * - `IN (?, …)` is a membership test, so a repeated value cannot match a row twice — the padding is
 *   why the tail chunk needs no statement shape of its own, and the cache holds one entry per
 *   direction and kind arity instead of one per frontier length;
 * - ending a chunk with an id it already contains is what keeps a batch from repeating another
 *   chunk's rows, so a batched lookup returns exactly what the same ids return one at a time.
 */
function toBatch(chunk: readonly number[]): number[] {
  const filler = firstId(chunk);
  const bound = [...chunk];
  for (let index = chunk.length; index < EDGE_BATCH_SIZE; index += 1) {
    bound.push(filler);
  }
  return bound;
}

/** The first id of a chunk the walk loop guarantees is non-empty; SQLite rejects `IN ()`. */
function firstId(chunk: readonly number[]): number {
  const id = chunk[0];
  if (id === undefined) {
    throw new AppError("internal", "edges: a chunk of ids cannot be empty");
  }
  return id;
}

function toNeighbourEdge(row: IncidentSqlRow): NeighbourEdge {
  return {
    edge: {
      id: row.edge_id,
      srcId: row.edge_src_id,
      dstId: row.edge_dst_id,
      kind: row.edge_kind,
      tier: row.edge_tier,
      fileId: row.edge_file_id,
      line: row.edge_line,
      localName: row.edge_local_name,
      evidence: row.edge_evidence,
    },
    node: toNodeRow(row),
  };
}

/** Both directions share the fetch and the mapping; only the SQL differs by {@link Direction}. */
function incident(
  conn: Conn,
  direction: Direction,
  nodeId: number,
  kinds: readonly string[] | undefined,
): NeighbourEdge[] {
  const filter = kinds ?? [];
  const statement = prepared(conn, `edges.${direction}/${filter.length}`, (sqlite) =>
    sqlite.query<IncidentSqlRow, (string | number)[]>(
      incidentSql(direction, filter.length, singlePredicate(direction), true),
    ),
  );
  return statement.all(nodeId, ...filter).map(toNeighbourEdge);
}

/**
 * The batched half of {@link incident}: one statement for the whole frontier, spilled over
 * `EDGE_BATCH_SIZE`-sized chunks. Rows arrive in scan order, chunk by chunk, so a caller that needs
 * an order imposes it — which is exactly what the walk does, and why this one is not sorted here.
 */
function incidentMany(
  conn: Conn,
  direction: Direction,
  nodeIds: readonly number[],
  kinds: readonly string[] | undefined,
): NeighbourEdge[] {
  if (nodeIds.length === 0) {
    return [];
  }
  const filter = kinds ?? [];
  const statement = prepared(conn, `edges.${direction}Many/${filter.length}`, (sqlite) =>
    sqlite.query<IncidentSqlRow, (string | number)[]>(
      incidentSql(direction, filter.length, batchPredicate(direction), false),
    ),
  );
  const rows: NeighbourEdge[] = [];
  for (let start = 0; start < nodeIds.length; start += EDGE_BATCH_SIZE) {
    const chunk = nodeIds.slice(start, start + EDGE_BATCH_SIZE);
    for (const row of statement.all(...toBatch(chunk), ...filter)) {
      rows.push(toNeighbourEdge(row));
    }
  }
  return rows;
}

/**
 * Edges whose destination is `nodeId`, with the calling node attached. `kinds` filters on
 * `edge.kind` and is all-or-nothing: an empty or omitted list means every kind. Ordered by
 * `(kind, tier, line, qn)` so a caller grouping by kind sees a stable order inside each group.
 */
export function inbound(conn: Conn, nodeId: number, kinds?: readonly string[]): NeighbourEdge[] {
  return incident(conn, "inbound", nodeId, kinds);
}

/**
 * Edges whose source is `nodeId`, with the called node attached. Same filter and ordering
 * contract as {@link inbound}.
 */
export function outbound(conn: Conn, nodeId: number, kinds?: readonly string[]): NeighbourEdge[] {
  return incident(conn, "outbound", nodeId, kinds);
}

/**
 * {@link inbound} for a whole set of ids: every edge whose destination is one of `nodeIds`, with
 * the calling node attached. Same rows and same `kinds` filter as a loop of {@link inbound} calls;
 * **unordered** — chunk by chunk in scan order — because a caller of a batch orders the batch, and
 * the walk does exactly that.
 */
export function inboundMany(
  conn: Conn,
  nodeIds: readonly number[],
  kinds?: readonly string[],
): NeighbourEdge[] {
  return incidentMany(conn, "inbound", nodeIds, kinds);
}

/** {@link outbound} for a whole set of ids: same contract as {@link inboundMany}. */
export function outboundMany(
  conn: Conn,
  nodeIds: readonly number[],
  kinds?: readonly string[],
): NeighbourEdge[] {
  return incidentMany(conn, "outbound", nodeIds, kinds);
}
