//! Bounded-hop expansion, unbounded output (`TECH-SPEC.md` §5.1, `DESIGN.md` §7.2).
//!
//! Ported from `src/shared/domain/neighbors.ts`. [`expand`] walks the graph from one seed and
//! returns *everything* it found, grouped by edge kind and then by tier; it never truncates.
//! Ranking, dropping `external` nodes and ordering groups by tool preference are the tool's
//! decisions (`TECH-SPEC.md` §5.3), so the only bound here is the hop budget, which is part of the
//! contract ([`EXPAND_DEPTH_MAX`]) and not a `limit`.
//!
//! Grouping is the payload's semantic content, not formatting: edge kind × tier is what tells a call
//! from an implementation at a glance, and hoisting `tier` onto the group is why a row does not
//! repeat it. Every item carries `hop` so a 2-hop neighbour is never mistaken for a direct
//! dependency, and `external` so the agent stops trying to resolve a builtin.
//!
//! Three porting decisions are visible in the types:
//!
//! - the whole expansion is `Result<GroupedNeighbors, AppError>`, not the contracted bare
//!   `GroupedNeighbors`: a failed read must not be flattened into an empty neighbourhood, which
//!   reads as "this symbol has no callers".
//! - the batched `inboundMany`/`outboundMany` pair is not exposed by `sangkar_store`'s
//!   `GraphReader` (it was ruled out of scope), so each hop asks for one node's edges at a time.
//!   The groups are identical because the walk imposes its own total order after every hop; only a
//!   wide frontier's sort cost differs.
//! - the TypeScript payload names its halves `in`/`out`; both are Rust keywords, so the fields are
//!   `inbound`/`outbound`. [`Direction`] lives here because the store's own direction enum is
//!   private to `read::edges`.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use sangkar_core::errors::AppError;
use sangkar_store::db::Conn;
use sangkar_store::read::edges::{NeighbourEdge, inbound, outbound};
use sangkar_store::read::nodes::NodeRow;
use sangkar_store::read::reader::GraphReader;

use crate::resolve::props;

/// The hop budget ceiling. `TOOLS-SPEC.md` §3 clamps `depth` to `1–4`, not a caller knob.
pub const EXPAND_DEPTH_MAX: u8 = 4;

/// Which way to walk. `All` is the common question; `In`/`Out` exist for when it is not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    In,
    Out,
    All,
}

impl Direction {
    /// The wire spelling of the argument enum.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::All => "all",
        }
    }

    /// Whether the inbound half is walked. Both halves are always present in the payload; the one
    /// that was not asked for is empty rather than absent.
    const fn walks_in(self) -> bool {
        !matches!(self, Self::Out)
    }

    const fn walks_out(self) -> bool {
        !matches!(self, Self::In)
    }
}

/// One neighbour: identity, where to read it, which hop found it, and whether it is external.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeighbourItem {
    pub id: i64,
    pub qn: String,
    pub kind: String,
    pub file: String,
    pub line: i64,
    pub hop: u8,
    pub external: bool,
}

/// All items of one `(edge kind, tier)` pair. The tier is shared by construction, hence here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeighbourGroup {
    pub kind: String,
    pub tier: String,
    pub items: Vec<NeighbourItem>,
}

/// The whole expansion. Both directions are always present so a caller can read `inbound`/`outbound`
/// unconditionally; the direction that was not asked for is empty rather than absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupedNeighbors {
    pub direction: Direction,
    /// The depth actually walked, after clamping.
    pub depth: u8,
    pub seeds: Vec<i64>,
    pub inbound: Vec<NeighbourGroup>,
    pub outbound: Vec<NeighbourGroup>,
}

/// The read surface expansion needs, with the connection already bound. A `Conn`-backed
/// implementation comes from [`neighbor_source`], and `dyn GraphReader` implements this trait
/// directly; a test supplies a hand-written object, so the hop-and-group contract is provable
/// without a database.
pub trait NeighbourSource {
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError>;
    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError>;
}

/// The store's two directional lookups, bound to a connection.
#[derive(Clone, Copy, Debug)]
pub struct ConnNeighbourSource<'a> {
    conn: &'a Conn,
}

/// Binds [`NeighbourSource`] to a connection; the only database-aware function here.
#[must_use]
pub fn neighbor_source(conn: &Conn) -> ConnNeighbourSource<'_> {
    ConnNeighbourSource { conn }
}

impl NeighbourSource for ConnNeighbourSource<'_> {
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        inbound(self.conn, node_id, kinds)
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        outbound(self.conn, node_id, kinds)
    }
}

/// A tool holds `Rc<dyn GraphReader>` and must not build a second adapter, so the aggregate is a
/// neighbour source by itself.
impl NeighbourSource for dyn GraphReader + '_ {
    fn inbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        GraphReader::inbound(self, node_id, kinds)
    }

    fn outbound(
        &self,
        node_id: i64,
        kinds: Option<&[String]>,
    ) -> Result<Vec<NeighbourEdge>, AppError> {
        GraphReader::outbound(self, node_id, kinds)
    }
}

/// `depth` clamped to the contract's `1–4`. A caller can never widen the walk past the ceiling: a
/// zero or negative depth becomes 1 (the smallest useful walk) and anything above four is four.
///
/// The TypeScript also maps a non-finite or fractional number onto 1 and `trunc`; an `i64` cannot
/// hold either, so that half of the clamp belongs to whoever parses the request, not here.
#[must_use]
pub fn clamp_depth(depth: i64) -> u8 {
    // The clamp lands in `1..=4`, so the narrowing cannot truncate.
    depth.clamp(1, i64::from(EXPAND_DEPTH_MAX)) as u8
}

/// External means "outside this repo": the indexer says so in `props`, or the `qn` carries no file
/// path at all (a builtin such as `builtins.dict`), which no repo-relative symbol can do.
fn is_external(node: &NodeRow) -> bool {
    props::boolean(&node.props, "external")
        || props::boolean(&node.props, "builtin")
        || !node.qn.contains("::")
}

/// Deterministic order inside one hop, so which group claims a node reachable by two edges does not
/// depend on the order the frontier happened to be built in.
///
/// The comparison is byte order rather than `localeCompare`: `qn`, `kind` and `tier` are also
/// ordered by SQLite's binary collation in the store, so matching it keeps a hop's order identical to
/// the rows' own order.
fn compare_reached(left: &NeighbourEdge, right: &NeighbourEdge) -> Ordering {
    left.edge
        .kind
        .cmp(&right.edge.kind)
        .then_with(|| left.edge.tier.cmp(&right.edge.tier))
        .then_with(|| left.edge.line.cmp(&right.edge.line))
        .then_with(|| left.node.qn.cmp(&right.node.qn))
        .then_with(|| left.node.id.cmp(&right.node.id))
}

/// Which statement a hop asks for. [`Direction::All`] is never a single walk.
#[derive(Clone, Copy)]
enum Side {
    In,
    Out,
}

/// One direction of the BFS. The seed is never returned as its own neighbour, and a node is reported
/// once per direction at the hop that first reached it — a cycle back to the seed is the caller's
/// starting point, not new information.
///
/// Both a node's dedup and its group come from the same deterministic sort: after a hop's edges are
/// ordered by `(kind, tier, line, qn, id)`, the first edge to a node claims it, so a node reachable as
/// both a call and a reference is reported once, in the group the sort put first, at the closest hop.
fn walk(
    source: &dyn NeighbourSource,
    seed: i64,
    side: Side,
    depth: u8,
    kinds: Option<&[String]>,
) -> Result<Vec<NeighbourGroup>, AppError> {
    let mut visited: BTreeSet<i64> = BTreeSet::from([seed]);
    let mut buckets: BTreeMap<(String, String), Vec<NeighbourItem>> = BTreeMap::new();
    let mut frontier: Vec<i64> = vec![seed];

    for hop in 1..=depth {
        if frontier.is_empty() {
            break;
        }
        let mut reached: Vec<NeighbourEdge> = Vec::new();
        for node_id in &frontier {
            let edges = match side {
                Side::In => source.inbound(*node_id, kinds)?,
                Side::Out => source.outbound(*node_id, kinds)?,
            };
            reached.extend(
                edges
                    .into_iter()
                    .filter(|edge| !visited.contains(&edge.node.id)),
            );
        }

        reached.sort_by(compare_reached);
        let mut next: Vec<i64> = Vec::new();
        for edge in reached {
            let NeighbourEdge { edge: row, node } = edge;
            if visited.contains(&node.id) {
                // An earlier hop claimed it, or an earlier edge in this same hop did.
                continue;
            }
            visited.insert(node.id);
            next.push(node.id);
            buckets
                .entry((row.kind, row.tier))
                .or_default()
                .push(item_of(&node, hop));
        }
        frontier = next;
    }

    // A `BTreeMap` keyed by `(kind, tier)` iterates in exactly the order the TypeScript sorts at the
    // end: edge kind first, then tier. Items keep the order the walk found them in.
    Ok(buckets
        .into_iter()
        .map(|((kind, tier), items)| NeighbourGroup { kind, tier, items })
        .collect())
}

fn item_of(node: &NodeRow, hop: u8) -> NeighbourItem {
    NeighbourItem {
        id: node.id,
        qn: node.qn.clone(),
        kind: node.kind.clone(),
        file: node.path.clone(),
        line: node.start_line,
        hop,
        external: is_external(node),
    }
}

/// Every neighbour of `node_id` within the clamped hop budget, grouped by edge kind and tier, in
/// both requested directions. Nothing is dropped for size: the caller's `emit` is the only place a
/// payload is cut.
///
/// `depth` is clamped again even though a caller may already have called [`clamp_depth`], because the
/// reported depth is the depth walked.
pub fn expand(
    source: &dyn NeighbourSource,
    node_id: i64,
    direction: Direction,
    depth: u8,
    kinds: Option<&[String]>,
) -> Result<GroupedNeighbors, AppError> {
    let depth = depth.clamp(1, EXPAND_DEPTH_MAX);
    let inbound_groups = if direction.walks_in() {
        walk(source, node_id, Side::In, depth, kinds)?
    } else {
        Vec::new()
    };
    let outbound_groups = if direction.walks_out() {
        walk(source, node_id, Side::Out, depth, kinds)?
    } else {
        Vec::new()
    };
    Ok(GroupedNeighbors {
        direction,
        depth,
        seeds: vec![node_id],
        inbound: inbound_groups,
        outbound: outbound_groups,
    })
}
