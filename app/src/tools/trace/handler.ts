/**
 * `trace`: one free-text input in, the resolved symbol and its neighbours grouped by relation out
 * (`TOOLS-SPEC.md` §3, `DESIGN.md` §8).
 *
 * The handler is the tool's whole policy and does no work of its own: resolution is
 * `#shared/domain/resolve`, the walk is `#shared/domain/neighbors`, the absence arithmetic is
 * `#shared/domain/coverage`, sizing is `#shared/budget` and the envelope comes from
 * `#shared/envelope`. What is decided here is:
 *
 * - **the cursor comes first.** A resumed call that started at the first item and only then failed
 *   would re-serve a page the client already saw, so a refused cursor returns before resolution runs;
 * - **grouping is the payload's semantic content**, so groups follow a fixed edge order rather than
 *   whatever order the walk produced, and `tier` is hoisted onto the group because every edge in one
 *   group shares it by construction;
 * - **the page boundary lies on the flattened neighbour sequence.** `position` indexes that sequence,
 *   so a resumed page continues exactly where the last one stopped;
 * - **a direction that was not walked is not a zero.** `callers_total` is omitted for
 *   `direction: "out"` rather than reported as `0`.
 *
 * Never source text: nothing here reads a file, and `emit` is the only place the payload shrinks.
 */
import { emit } from "#shared/budget";
import type { CursorRefusal } from "#shared/cursor";
import { assemble, type CoverageAssembly } from "#shared/domain/coverage";
import {
  clampDepth,
  expand,
  type NeighbourGroup,
  type NeighbourItem,
} from "#shared/domain/neighbors";
import {
  CANDIDATE_CAP,
  type Resolution,
  type ResolutionCandidate,
  type ResolutionStep,
  resolve,
} from "#shared/domain/resolve";
import {
  ambiguous,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  type NotIndexedEntry,
  notFound,
  ok,
  type ReadStatus,
  type Totals,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import type { ToolContext } from "#shared/store/read/context";
import type { GraphMeta, NodeRow, UnresolvedRef } from "#shared/store/read/reader";
import {
  DEFAULT_BUDGET_TOKENS,
  DEFAULT_DEPTH,
  type TraceArgs,
  type TraceDirection,
} from "./manifest";

/** The tool name as a cursor pins it: a token minted by one tool must not resume another. */
const TOOL = "trace";

/**
 * The fixed group order (`TOOLS-SPEC.md` §3). It is a contract, not a preference: an agent reads a
 * trace by scanning "who calls me" before "what I import", and the order has to be identical on every
 * page or a resumed page would interleave with the first one.
 *
 * `contains` and `defines` are structural edges the walk can also return. They are outside the
 * documented sequence and are appended after it, alphabetically, rather than dropped.
 */
const EDGE_ORDER = [
  "calls",
  "inherits",
  "implements",
  "references",
  "tested_by",
  "imports",
] as const;

/** Where an agent goes next from a trace: one item's detail. `expand` is the only address for it. */
const NEXT_TOOLS = ["expand"] as const;

/** A miss is routed to `search` (a broader query) or `status` (is the graph even complete). */
const NEXT_TOOLS_AFTER_MISS = ["search", "status"] as const;

/**
 * `retry_after_ms` for an unpublished database. The watcher publishes on a debounce, so the useful
 * instruction is "wait one debounce and ask again", not a bare failure.
 */
const UNPUBLISHED_RETRY_MS = 1_000;

/** An `unresolved` row with no recorded evidence still has to say something; never an empty string. */
const NO_EVIDENCE = "no evidence recorded";

/** `DESIGN.md` §8's tier for a call site whose target no node matches. */
const UNRESOLVED_TIER = "unresolved";

/** One neighbour as the payload shows it: identity, where to read it, which hop found it. */
export interface TraceItem {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly line: number;
  readonly hop: number;
  /** Present only when true: a repo-relative neighbour is the normal case and costs no bytes. */
  readonly external?: boolean;
  /** Present only when true. See {@link toItem} for why `kind` is the signal available here. */
  readonly is_test?: boolean;
}

/** All items of one `(edge, tier)` pair; `tier` is shared by construction, hence hoisted here. */
export interface TraceGroup {
  readonly edge: string;
  readonly tier: string;
  readonly items: readonly TraceItem[];
}

/** The resolution block that lets the agent judge the match without a second call. */
export interface TraceResolution {
  readonly query: string;
  readonly matched: string;
  readonly kind: string;
  readonly how: string;
  readonly confidence: string;
}

export interface TraceNode {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: readonly [number, number];
  readonly signature: string | null;
  readonly is_test: boolean;
}

/** One call site the index could not bind, named with the reason so the agent can decide to read it. */
export interface TraceUnresolved {
  readonly file: string;
  readonly line: number;
  readonly name: string;
  readonly evidence: string;
  readonly tier: string;
}

export interface TracePayload {
  readonly resolution: TraceResolution;
  readonly node: TraceNode;
  readonly in: readonly TraceGroup[];
  readonly out: readonly TraceGroup[];
  readonly unresolved: readonly TraceUnresolved[];
  readonly not_indexed: readonly NotIndexedEntry[];
}

/** An ambiguous answer is candidates only: no node, no neighbours (`DESIGN.md` §8). */
export interface TraceCandidates {
  readonly candidates: readonly ResolutionCandidate[];
}

/** Nothing matched: near misses instead of a bare zero. */
export interface TraceNearMisses {
  readonly near_misses: readonly ResolutionCandidate[];
}

/** Every shape this tool returns; `status` tells the client which payload it got. */
export type TraceResponse =
  | Envelope<TracePayload>
  | Envelope<TraceCandidates>
  | Envelope<TraceNearMisses>
  | Envelope<null>;

/** The effective arguments, in a fixed key order, so an implicit default hashes like an explicit one. */
function effectiveArgs(args: TraceArgs): Record<string, unknown> {
  return {
    query: typeof args.query === "string" ? args.query : "",
    direction: directionOf(args),
    kinds: args.kinds ?? null,
    depth: args.depth ?? DEFAULT_DEPTH,
    budget: budgetOf(args),
  };
}

/**
 * The `argsHash` a cursor binds (`DATABASE.md` §8): `sha256:<hex>` over the effective arguments —
 * after defaults, without the cursor itself — so `{query:"x"}` and `{query:"x", depth:2}` are the same
 * call and a resume cannot silently address a different query. Bun's hasher is used rather than
 * `node:crypto` because the runtime is Bun and this is a synchronous one-shot over a few dozen bytes.
 *
 * The key order of the literal above is the canonical order; `JSON.stringify` preserves it, so the
 * hash is stable across processes. `kinds: null` keeps "absent" and "present" distinguishable.
 */
export function argsHashOf(args: TraceArgs): string {
  const text = JSON.stringify(effectiveArgs(args));
  return `sha256:${new Bun.CryptoHasher("sha256").update(text, "utf8").digest("hex")}`;
}

function budgetOf(args: TraceArgs): number {
  return typeof args.budget === "number" && Number.isFinite(args.budget)
    ? args.budget
    : DEFAULT_BUDGET_TOKENS;
}

function directionOf(args: TraceArgs): TraceDirection {
  return args.direction ?? "all";
}

/** `expand` clamps too; clamping here keeps the hop count in the summary honest. */
function depthOf(args: TraceArgs): number {
  return clampDepth(args.depth ?? DEFAULT_DEPTH);
}

/**
 * Confidence of the *match*, not of an edge: `extracted` when the source names the node outright (an
 * exact qualified name or an exact bare name), `inferred` when the hit came from weaker evidence,
 * such as a dotted tail or a path (`DESIGN.md` §8's tiers). A tie never reaches this function — a tie
 * is `ambiguous`.
 */
function confidenceOf(step: ResolutionStep): string {
  return step === "exact_qn" || step === "exact_name" ? "extracted" : "inferred";
}

function edgeRank(edge: string): number {
  const index = (EDGE_ORDER as readonly string[]).indexOf(edge);
  return index === -1 ? EDGE_ORDER.length : index;
}

/**
 * The walk's groups in the contract's order. `expand` already sorts by edge name; this re-sorts by the
 * fixed list so a first page and a resumed page group identically, then by edge name (for edges
 * outside the list) and by tier, so the order is total rather than merely "usually stable".
 */
function orderedGroups(groups: readonly NeighbourGroup[]): NeighbourGroup[] {
  return [...groups].sort((a, b) => {
    const byRank = edgeRank(a.edge) - edgeRank(b.edge);
    if (byRank !== 0) {
      return byRank;
    }
    const byEdge = a.edge.localeCompare(b.edge);
    return byEdge !== 0 ? byEdge : a.tier.localeCompare(b.tier);
  });
}

/**
 * One neighbour as the payload shows it. `external` and `is_test` appear only when true, because a
 * repo-relative, non-test neighbour is the normal case and a repeated `false` is pure payload weight.
 *
 * `is_test` reads the node's kind: `expand`'s item does not carry `node.is_test`, and looking every
 * neighbour up again would be one query per item. `test` is the kind the indexer gives a test node,
 * so it is the label that is known at this seam rather than one guessed from a path.
 */
function toItem(item: NeighbourItem): TraceItem {
  const shaped: TraceItem = {
    id: item.id,
    qn: item.qn,
    kind: item.kind,
    file: item.file,
    line: item.line,
    hop: item.hop,
    ...(item.external ? { external: true } : {}),
  };
  return item.kind === "test" ? { ...shaped, is_test: true } : shaped;
}

/** One entry of the flattened sequence: which direction, which group, which neighbour. */
interface FlatEntry {
  readonly direction: "in" | "out";
  readonly edge: string;
  readonly tier: string;
  readonly item: TraceItem;
}

/**
 * The flattened neighbour sequence the page boundary and the payload are both derived from: `in`
 * groups in the fixed order, then `out`. Two calls with the same arguments and the same generation
 * produce the same sequence, which is what makes `position` meaningful (`DATABASE.md` §8).
 */
function flatten(groups: {
  readonly in: readonly NeighbourGroup[];
  readonly out: readonly NeighbourGroup[];
}): FlatEntry[] {
  const entries: FlatEntry[] = [];
  for (const direction of ["in", "out"] as const) {
    for (const group of orderedGroups(groups[direction])) {
      for (const item of group.items) {
        entries.push({
          direction,
          edge: group.edge,
          tier: group.tier,
          item: toItem(item),
        });
      }
    }
  }
  return entries;
}

/** Groups rebuilt from a slice of {@link flatten}, keeping the order the entries arrived in. */
function groupsOf(entries: readonly FlatEntry[], direction: "in" | "out"): TraceGroup[] {
  const byKey = new Map<string, TraceItem[]>();
  for (const entry of entries) {
    if (entry.direction !== direction) {
      continue;
    }
    const key = `${entry.edge}\u0000${entry.tier}`;
    const items = byKey.get(key);
    if (items === undefined) {
      byKey.set(key, [entry.item]);
    } else {
      items.push(entry.item);
    }
  }
  return [...byKey].map(([key, items]) => {
    const separator = key.indexOf("\u0000");
    return { edge: key.slice(0, separator), tier: key.slice(separator + 1), items };
  });
}

function keyOf(direction: "in" | "out", edge: string, tier: string, id: number): string {
  return `${direction}\u0000${edge}\u0000${tier}\u0000${id}`;
}

/**
 * How many entries of `rest` a truncated envelope still carried, counted as a prefix.
 *
 * `emit` pops the last row of the largest list, which can be a whole group rather than the tail of
 * this sequence, so what it delivered is not necessarily a prefix. The page is cut back to the prefix
 * it did deliver and the remainder is served by the next page; that is what keeps pages
 * duplicate-free without hand-sizing the payload here.
 */
function leadingSurvivors(payload: TracePayload, rest: readonly FlatEntry[]): number {
  const present = new Set<string>();
  for (const group of payload.in) {
    for (const item of group.items) {
      present.add(keyOf("in", group.edge, group.tier, item.id));
    }
  }
  for (const group of payload.out) {
    for (const item of group.items) {
      present.add(keyOf("out", group.edge, group.tier, item.id));
    }
  }
  let survived = 0;
  for (const entry of rest) {
    if (!present.has(keyOf(entry.direction, entry.edge, entry.tier, entry.item.id))) {
      break;
    }
    survived += 1;
  }
  return survived;
}

function unresolvedOf(rows: readonly UnresolvedRef[]): TraceUnresolved[] {
  return rows.map((row) => ({
    file: row.path,
    line: row.line,
    name: row.rawTarget,
    evidence: row.evidence ?? row.evidenceKind ?? NO_EVIDENCE,
    tier: UNRESOLVED_TIER,
  }));
}

function payloadOf(
  node: NodeRow,
  query: string,
  resolution: Extract<Resolution, { status: "ok" }>,
  page: readonly FlatEntry[],
  unresolved: readonly TraceUnresolved[],
  notIndexed: readonly NotIndexedEntry[],
): TracePayload {
  return {
    resolution: {
      query,
      matched: node.qn,
      kind: node.kind,
      how: resolution.why,
      confidence: confidenceOf(resolution.step),
    },
    node: {
      id: node.id,
      qn: node.qn,
      kind: node.kind,
      file: node.path,
      lines: [node.startLine, node.endLine] as const,
      signature: node.signature,
      is_test: node.isTest,
    },
    in: groupsOf(page, "in"),
    out: groupsOf(page, "out"),
    unresolved,
    not_indexed: notIndexed,
  };
}

function countsOf(entries: readonly FlatEntry[]): { in: number; out: number } {
  let inCount = 0;
  for (const entry of entries) {
    if (entry.direction === "in") {
      inCount += 1;
    }
  }
  return { in: inCount, out: entries.length - inCount };
}

/**
 * Per-list counts plus the relation the scope supports. A direction that was not walked contributes
 * no key at all: `callers_total: 0` on a `direction: "out"` call reads as "nothing calls this", which
 * is the one claim this tool must never make by accident.
 */
function totalsOf(
  counts: { in: number; out: number },
  direction: TraceDirection,
  relation: Totals["total_relation"],
): Totals {
  const totals: Totals = { total_relation: relation };
  if (direction !== "out") {
    totals.callers_total = counts.in;
  }
  if (direction !== "in") {
    totals.callees_total = counts.out;
  }
  return totals;
}

function summaryOf(
  node: NodeRow,
  counts: { in: number; out: number },
  direction: TraceDirection,
  page: { shown: number; available: number } | null,
  unresolvedCount: number,
): string {
  const head =
    direction === "in"
      ? `${node.name}: ${counts.in} callers`
      : direction === "out"
        ? `${node.name}: ${counts.out} callees`
        : `${node.name}: ${counts.in} callers, ${counts.out} callees`;
  const paged = page === null ? "" : ` (${page.shown} of ${page.available} neighbours shown)`;
  const gap =
    unresolvedCount === 0
      ? "."
      : `; ${unresolvedCount} unresolved call site${unresolvedCount === 1 ? "" : "s"} in ${node.path}.`;
  return `${head}${paged}${gap}`;
}

/**
 * The generation as reported to a client, with the node's own file deciding freshness. `coverage.ts`
 * treats a file written by another publish as a stale *answer* even when the repository has not moved,
 * so `stale` here means exactly that, and `stale_checked_at` records that a check happened at all.
 */
function readStatusFor(
  ctx: ToolContext,
  meta: GraphMeta,
  generation: string,
  node: NodeRow,
): ReadStatus {
  const file = ctx.reader.fileByPath(node.path);
  const stale = file === null || file.indexedGeneration !== generation;
  const status: ReadStatus = {
    generation,
    indexed_at: meta.indexedAt ?? "",
    stale,
    stale_checked_at: ctx.now().toISOString(),
  };
  if (stale) {
    status.stale_reason =
      file === null
        ? `"${node.path}" has no indexed file row`
        : `"${node.path}" was last written by generation ${file.indexedGeneration}`;
  }
  return status;
}

/**
 * The read status of an answer with no node behind it. There is no file scope to compare, so no
 * `stale_checked_at` is claimed: "we did not look" and "we looked and it is current" are different
 * statements (`#shared/envelope.ts`), and only `coverage.complete: false` carries the caveat here.
 */
function unscopedReadStatus(meta: GraphMeta): ReadStatus {
  return { generation: meta.generation ?? "", indexed_at: meta.indexedAt ?? "", stale: false };
}

/** No scope was checked, so nothing may be claimed: `complete: false` is the whole answer. */
function unscopedCoverage(): Coverage {
  return {
    complete: false,
    indexed: 0,
    skipped: 0,
    failed: 0,
    unresolved_edges: 0,
    not_indexed: [],
  };
}

function unpublished(query: string): Envelope<null> {
  // Not `notFound`: an empty database is no evidence that the symbol does not exist, and answering
  // `not_found` would let the agent tell the user it does not. `fail` with the contract's
  // `bootstrapping` code is the closed-union way to say "there is no graph to answer from yet".
  const error = new AppError("bootstrapping", "the graph has no published generation yet", {
    retryable: true,
    retryAfterMs: UNPUBLISHED_RETRY_MS,
    hint: "index the repository, then call trace again",
  });
  return fail(error, {
    summary: `trace "${query}": no published generation`,
    read_status: { generation: "", indexed_at: "", stale: true },
    coverage: unscopedCoverage(),
  });
}

function cursorFailure(refusal: CursorRefusal, meta: GraphMeta, query: string): Envelope<null> {
  // A partial page is never served: the refusal carries the contract's code and its hint, and the
  // client re-runs the original call instead of resuming into a page it cannot trust.
  const error = new AppError(refusal.code, `cannot resume this trace page: ${refusal.code}`, {
    hint: refusal.hint,
  });
  return fail(error, {
    summary: `trace "${query}": the cursor was refused, re-run the original call`,
    read_status: unscopedReadStatus(meta),
    coverage: unscopedCoverage(),
  });
}

/**
 * One run of `emit`. `dropped` is deliberately not the resume position: `emit` counts the rows it
 * removed, and a removed row can be a whole group from the middle of the sequence rather than the
 * tail, so only the page boundary computed here is an offset a resumed call may trust. The count is
 * logged instead, which is what makes a surprising truncation diagnosable.
 */
function mintAt(
  ctx: ToolContext,
  identity: { generation: string; argsHash: string },
  pageEnd: number,
): (dropped: number) => string {
  return (dropped) => {
    ctx.log.debug("trace page truncated", { dropped, resume_at: pageEnd });
    return ctx.cursor.sign({
      generation: identity.generation,
      tool: TOOL,
      argsHash: identity.argsHash,
      position: pageEnd,
    });
  };
}

/** A resolved query: the walk, the page and the envelope. */
function traced(
  ctx: ToolContext,
  args: TraceArgs,
  resolution: Extract<Resolution, { status: "ok" }>,
  meta: GraphMeta,
  start: number,
  identity: { generation: string; argsHash: string },
): TraceResponse {
  const generation = identity.generation;
  const node = resolution.node;
  const direction = directionOf(args);
  const readStatus = readStatusFor(ctx, meta, generation, node);
  const assembly: CoverageAssembly = assemble(ctx.reader, {
    paths: [node.path],
    generation,
    stale: readStatus.stale,
    fileIds: [node.fileId],
  });
  const grouped = expand(ctx.reader, [node.id], { direction, depth: depthOf(args) });
  const all = flatten({ in: grouped.in, out: grouped.out });
  const counts = countsOf(all);
  const unresolved = unresolvedOf(assembly.unresolved);
  const notIndexed = [...assembly.coverage.not_indexed];
  const rest = all.slice(start);
  const totals = totalsOf(counts, direction, assembly.relation);
  const spec: EnvelopeSpec = {
    summary: summaryOf(node, counts, direction, null, unresolved.length),
    read_status: readStatus,
    coverage: assembly.coverage,
    totals,
    next_tools: NEXT_TOOLS,
  };

  // Pass one measures with the whole remaining sequence. `emit` is the only sizing decision, so the
  // page boundary is read back out of what it chose to keep rather than computed here.
  let pageLength = rest.length;
  let envelope: Envelope<TracePayload> = emit(
    ok(payloadOf(node, args.query, resolution, rest, unresolved, notIndexed), spec),
    budgetOf(args),
    { mint: mintAt(ctx, identity, start + pageLength) },
  );

  // `emit` pops the last row of the largest list, which can be a whole group from the middle of the
  // sequence, so what it kept is not always a prefix of `rest`. Shrinking the page to the prefix that
  // did survive is what makes every page a contiguous slice of the sequence: no duplicates, and no
  // neighbour skipped between one page and the next.
  //
  // The price is that a page can stop at a group boundary and leave part of the budget unused: losing
  // no row is worth more than filling the budget exactly, and the alternative — trusting `emit`'s drop
  // count as an offset — would skip neighbours a client never received.
  while (envelope.truncated) {
    const survived = leadingSurvivors(envelope.payload, rest);
    if (survived >= pageLength) {
      // Either every neighbour is here (only the evidence lists were cut), or a budget below one
      // neighbour cannot advance: both are `emit`'s own answer, and `next` re-serves the same page
      // rather than silently skipping rows the client never received.
      break;
    }
    pageLength = survived;
    const page = rest.slice(0, pageLength);
    const pageSpec: EnvelopeSpec = {
      ...spec,
      summary: summaryOf(
        node,
        counts,
        direction,
        { shown: page.length, available: rest.length },
        unresolved.length,
      ),
      truncated: true,
      truncation_reason: "budget",
      next: ctx.cursor.sign({
        generation,
        tool: TOOL,
        argsHash: identity.argsHash,
        position: start + page.length,
      }),
    };
    envelope = emit(
      ok(payloadOf(node, args.query, resolution, page, unresolved, notIndexed), pageSpec),
      budgetOf(args),
      { mint: mintAt(ctx, identity, start + page.length) },
    );
  }
  return envelope;
}

/**
 * One free-text query in, one envelope out (`TECH-SPEC.md` §6: handlers return envelopes, never
 * throw, never read the environment and never open a database).
 */
export async function handle(ctx: ToolContext, args: TraceArgs): Promise<TraceResponse> {
  const query = typeof args.query === "string" ? args.query : "";
  const meta = ctx.reader.metaFor();
  const generation = meta.generation;
  if (generation === null) {
    return unpublished(query);
  }

  const identity = { generation, argsHash: argsHashOf(args) };
  let start = 0;
  if (typeof args.cursor === "string" && args.cursor !== "") {
    const verified = ctx.cursor.verify(args.cursor, { ...identity, tool: TOOL });
    if (!verified.ok) {
      return cursorFailure(verified, meta, query);
    }
    start = verified.position;
  }

  const resolution = resolve(ctx.reader, query, args.kinds);
  switch (resolution.status) {
    case "ambiguous": {
      const spec: EnvelopeSpec = {
        summary: `${query}: ${resolution.candidates.length} candidates matched, none preferred`,
        read_status: unscopedReadStatus(meta),
        coverage: unscopedCoverage(),
        totals: { total_relation: "gte", candidates_total: resolution.candidates.length },
        next_tools: NEXT_TOOLS_AFTER_MISS,
      };
      // The candidate list is capped at `CANDIDATE_CAP` by the resolution contract, so `emit` has
      // nothing to cut unless the budget is below the envelope's own floor; the minted position is
      // then the head of the list, which re-serves the same candidates rather than skipping any.
      return emit(
        ambiguous<TraceCandidates>(
          { candidates: resolution.candidates.slice(0, CANDIDATE_CAP) },
          spec,
        ),
        budgetOf(args),
        { mint: mintAt(ctx, identity, start) },
      );
    }
    case "not_found": {
      const spec: EnvelopeSpec = {
        summary: `${query}: no symbol matched, ${resolution.near_misses.length} near misses`,
        read_status: unscopedReadStatus(meta),
        coverage: unscopedCoverage(),
        totals: { total_relation: "gte", near_misses_total: resolution.near_misses.length },
        next_tools: NEXT_TOOLS_AFTER_MISS,
      };
      return emit(
        notFound<TraceNearMisses>(
          { near_misses: resolution.near_misses.slice(0, CANDIDATE_CAP) },
          spec,
        ),
        budgetOf(args),
        { mint: mintAt(ctx, identity, start) },
      );
    }
    default:
      return traced(ctx, args, resolution, meta, start, identity);
  }
}
