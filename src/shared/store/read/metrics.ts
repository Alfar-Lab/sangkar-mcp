/**
 * The derived per-node numbers `DATABASE.md` §3.4 computes once at publish, read back per node.
 *
 * `tested_depth` is the reason this table exists: `test_gaps` and `impact` answer "is this
 * tested, and through which caller" from one `INTEGER` instead of a graph walk, so the read
 * primitive must not lose the distinction between `0` (tested directly), a positive depth and
 * `NULL` (no test reachable). A missing metrics row is reported as `null`, not as zeroes:
 * "degree 0" is a measurement, "not measured" is not.
 */
import { type Conn, prepared } from "#shared/store/db";

/** `node_metrics` for one node, mapped from the SQL row. */
export interface NodeMetrics {
  readonly nodeId: number;
  readonly inDegree: number;
  readonly outDegree: number;
  /** `is_entry` as a boolean; the `1/0` view belongs at the read boundary, not in the column. */
  readonly isEntry: boolean;
  /** Minimum hops to a test node; `null` when no test is reachable. */
  readonly testedDepth: number | null;
  /** `summary(repo)` only: nodes reachable from this entry point; `null` elsewhere. */
  readonly reachableCount: number | null;
}

interface MetricsSqlRow {
  readonly node_id: number;
  readonly in_degree: number;
  readonly out_degree: number;
  readonly is_entry: number;
  readonly tested_depth: number | null;
  readonly reachable_count: number | null;
}

/** Metrics for one node, by primary key. `null` means the publish never measured this node. */
export function metricsFor(conn: Conn, nodeId: number): NodeMetrics | null {
  const row = prepared(conn, "metrics.forNode", (sqlite) =>
    sqlite.query<MetricsSqlRow, [number]>(
      "SELECT node_id, in_degree, out_degree, is_entry, tested_depth, reachable_count" +
        " FROM node_metrics WHERE node_id = ?",
    ),
  ).get(nodeId);
  if (row === null) {
    return null;
  }
  return {
    nodeId: row.node_id,
    inDegree: row.in_degree,
    outDegree: row.out_degree,
    isEntry: row.is_entry !== 0,
    testedDepth: row.tested_depth,
    reachableCount: row.reachable_count,
  };
}
