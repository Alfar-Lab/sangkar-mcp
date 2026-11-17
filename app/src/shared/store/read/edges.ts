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
 */
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
 * One direction and kind-filter arity, assembled only when the shape is a cache miss. The two
 * interpolated values are a closed union and a count, never caller input.
 */
function incidentSql(direction: Direction, kindCount: number): string {
  const near = direction === "inbound" ? "e.dst_id" : "e.src_id";
  const far = direction === "inbound" ? "e.src_id" : "e.dst_id";
  const kinds = kindCount === 0 ? "" : ` AND e.kind IN (${placeholders(kindCount)})`;
  return (
    `SELECT ${EDGE_SELECT}, ${NODE_SELECT} FROM edge e` +
    ` JOIN node n ON n.id = ${far} JOIN file f ON f.id = n.file_id` +
    ` WHERE ${near} = ?${kinds} ORDER BY e.kind, e.tier, e.line, n.qn`
  );
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
    sqlite.query<IncidentSqlRow, (string | number)[]>(incidentSql(direction, filter.length)),
  );
  return statement.all(nodeId, ...filter).map(toNeighbourEdge);
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
