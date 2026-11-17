/**
 * Bounded-hop expansion, unbounded output (`TECH-SPEC.md` §5.1, `DESIGN.md` §7.2).
 *
 * `expand` walks the graph from a set of seeds and returns *everything* it found, grouped by
 * edge kind and then by tier. It never truncates: ranking, dropping `external` nodes and
 * ordering groups by tool preference are the tool's decisions (`TECH-SPEC.md` §5.3), so the
 * only bound here is the hop budget, which is part of the contract (1–4) and not a `limit`.
 *
 * Grouping is the payload's semantic content, not formatting: direction × edge kind × tier is
 * what tells a call from an implementation at a glance, and hoisting `tier` onto the group is
 * why a row does not repeat it. Every item carries `hop` so a 2-hop neighbour is never mistaken
 * for a direct dependency, and `external` so the agent stops trying to resolve a builtin.
 *
 * Each hop goes through `inbound`/`outbound`, whose statements come from
 * `prepared(conn, sqlKey, build)`; nothing here builds a query inside the walk.
 */
import type { Conn } from "#shared/store/db";
import { inbound, type NeighbourEdge, outbound } from "#shared/store/read/edges";
import type { NodeRow } from "#shared/store/read/nodes";

/** The hop budget ceiling. `TOOLS-SPEC.md` §3 clamps `depth` to `1–4`, not a caller knob. */
export const EXPAND_DEPTH_MAX = 4;

/** Which way to walk. `all` is the common question; `in`/`out` exist for when it is not. */
export type ExpandDirection = "in" | "out" | "all";

export interface ExpandOptions {
  readonly direction: ExpandDirection;
  /** Requested hops; clamped to `1–${EXPAND_DEPTH_MAX}` rather than rejected. */
  readonly depth: number;
  /** Edge-kind filter, all-or-nothing: empty or omitted means every kind. */
  readonly kinds?: readonly string[];
}

/** One neighbour: identity, where to read it, which hop found it, and whether it is external. */
export interface NeighbourItem {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly line: number;
  readonly hop: number;
  readonly external: boolean;
}

/** All items of one `(edge kind, tier)` pair. The tier is shared by construction, hence here. */
export interface NeighbourGroup {
  readonly edge: string;
  readonly tier: string;
  readonly items: readonly NeighbourItem[];
}

/**
 * The whole expansion. Both directions are always present so a caller can read `in`/`out`
 * unconditionally; the direction that was not asked for is empty rather than absent.
 */
export interface GroupedNeighbors {
  readonly direction: ExpandDirection;
  /** The depth actually walked, after clamping. */
  readonly depth: number;
  readonly seeds: readonly number[];
  readonly in: readonly NeighbourGroup[];
  readonly out: readonly NeighbourGroup[];
}

/**
 * The read surface expansion needs, with the connection already bound. A `Conn`-backed
 * implementation comes from {@link neighbourSource}; a test supplies a hand-written object, so
 * the hop-and-group contract is provable without a database.
 */
export interface NeighbourSource {
  inbound(nodeId: number, kinds?: readonly string[]): NeighbourEdge[];
  outbound(nodeId: number, kinds?: readonly string[]): NeighbourEdge[];
}

/** Binds the store's two directional lookups to a connection. */
export function neighbourSource(conn: Conn): NeighbourSource {
  return {
    inbound: (nodeId, kinds) => inbound(conn, nodeId, kinds),
    outbound: (nodeId, kinds) => outbound(conn, nodeId, kinds),
  };
}

/**
 * `depth` clamped to the contract's `1–4`. A non-finite depth becomes 1 (the smallest useful
 * walk) and a fractional one truncates, so a caller can never widen the walk past the ceiling.
 */
export function clampDepth(depth: number): number {
  if (!Number.isFinite(depth)) {
    return 1;
  }
  return Math.min(EXPAND_DEPTH_MAX, Math.max(1, Math.trunc(depth)));
}

/**
 * External means "outside this repo": the indexer says so in `props`, or the `qn` carries no
 * file path at all (a builtin such as `builtins.dict`), which no repo-relative symbol can do.
 */
function isExternal(node: NodeRow): boolean {
  if (node.props.external === true || node.props.builtin === true) {
    return true;
  }
  return !node.qn.includes("::");
}

interface Reached {
  readonly edge: NeighbourEdge;
  readonly hop: number;
}

/**
 * Deterministic order inside one hop, so which group claims a node reachable by two edges does
 * not depend on the order the frontier happened to be built in.
 */
function compareReached(a: Reached, b: Reached): number {
  const byKind = a.edge.edge.kind.localeCompare(b.edge.edge.kind);
  if (byKind !== 0) {
    return byKind;
  }
  const byTier = a.edge.edge.tier.localeCompare(b.edge.edge.tier);
  if (byTier !== 0) {
    return byTier;
  }
  if (a.edge.edge.line !== b.edge.edge.line) {
    return a.edge.edge.line - b.edge.edge.line;
  }
  const byQn = a.edge.node.qn.localeCompare(b.edge.node.qn);
  return byQn !== 0 ? byQn : a.edge.node.id - b.edge.node.id;
}

interface Bucket {
  readonly edge: string;
  readonly tier: string;
  readonly items: NeighbourItem[];
}

/**
 * One direction of the BFS. Seeds are never returned as their own neighbours, and a node is
 * reported once per direction at the hop that first reached it — a cycle back to the seed is
 * the caller's starting point, not new information.
 */
function walk(
  source: NeighbourSource,
  seeds: readonly number[],
  direction: "in" | "out",
  depth: number,
  kinds: readonly string[] | undefined,
): NeighbourGroup[] {
  const visited = new Set<number>(seeds);
  const buckets = new Map<string, Bucket>();
  let frontier: number[] = [...seeds];

  for (let hop = 1; hop <= depth && frontier.length > 0; hop += 1) {
    const reached: Reached[] = [];
    for (const id of frontier) {
      const edges = direction === "in" ? source.inbound(id, kinds) : source.outbound(id, kinds);
      for (const edge of edges) {
        if (!visited.has(edge.node.id)) {
          reached.push({ edge, hop });
        }
      }
    }

    reached.sort(compareReached);
    const next: number[] = [];
    for (const { edge, hop: foundAt } of reached) {
      const node = edge.node;
      if (visited.has(node.id)) {
        continue;
      }
      visited.add(node.id);
      next.push(node.id);
      const bucket = buckets.get(`${edge.edge.kind}\u0000${edge.edge.tier}`);
      const item: NeighbourItem = {
        id: node.id,
        qn: node.qn,
        kind: node.kind,
        file: node.path,
        line: node.startLine,
        hop: foundAt,
        external: isExternal(node),
      };
      if (bucket === undefined) {
        buckets.set(`${edge.edge.kind}\u0000${edge.edge.tier}`, {
          edge: edge.edge.kind,
          tier: edge.edge.tier,
          items: [item],
        });
      } else {
        bucket.items.push(item);
      }
    }
    frontier = next;
  }

  return [...buckets.values()]
    .sort((a, b) =>
      a.edge === b.edge ? a.tier.localeCompare(b.tier) : a.edge.localeCompare(b.edge),
    )
    .map((bucket) => ({ edge: bucket.edge, tier: bucket.tier, items: bucket.items }));
}

/**
 * Every neighbour of `seedIds` within the clamped hop budget, grouped by edge kind and tier,
 * in both requested directions. Nothing is dropped for size: the caller's `emit` is the only
 * place a payload is cut.
 */
export function expand(
  source: NeighbourSource,
  seedIds: readonly number[],
  options: ExpandOptions,
): GroupedNeighbors {
  const depth = clampDepth(options.depth);
  const seeds = [...new Set(seedIds)];
  const inboundGroups =
    options.direction === "out" ? [] : walk(source, seeds, "in", depth, options.kinds);
  const outboundGroups =
    options.direction === "in" ? [] : walk(source, seeds, "out", depth, options.kinds);
  return {
    direction: options.direction,
    depth,
    seeds,
    in: inboundGroups,
    out: outboundGroups,
  };
}
