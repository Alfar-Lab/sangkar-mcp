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
 * - **the page is sized before it is emitted.** `emit` cuts the *largest* list first, which on a
 *   dense symbol is the neighbour sequence itself: handing it 3 000 rows so it can tell us what fits
 *   leaves the flat `unresolved` evidence list as the only survivor and mints a page that advances by
 *   zero rows. So the sequence is measured here, the longest prefix that fits is materialised, and
 *   `emit` is handed a page it can only confirm — the boundary is still emit's to move, and a cut it
 *   makes is read back out of what survived;
 * - **content may not be outbid by evidence.** `unresolved` is the explanation of a miss, not the
 *   answer, so it is capped to a share of the allowance instead of eating the page;
 * - **a truncated page always advances.** The continuation token is minted past the rows served; the
 *   only zero-row page is a terminal one, which says so and points at the cheaper query;
 * - **a direction that was not walked is not a zero.** `callers_total` is omitted for
 *   `direction: "out"` rather than reported as `0`.
 *
 * Never source text: nothing here reads a file, and `emit` is the only place the payload shrinks.
 */
import { BYTES_PER_TOKEN, emit } from "#shared/budget";
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

/**
 * Where a page that fits no neighbour sends the agent: the same query one hop cheaper. `expand` is
 * not offered because a neighbour list is what could not be served, and detail on one node is not a
 * substitute for it.
 */
const NEXT_TOOLS_NARROWER = ["trace"] as const;

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

/**
 * Characters reserved for the continuation token while the page is being sized. `budget.ts` charges
 * 128 for the same token, and a real one is ~150; over-reserving costs a few bytes of page, while
 * under-reserving would make `emit` cut a page that was meant to fit.
 */
const CURSOR_RESERVE_CHARS = 192;

/**
 * Bytes of slack between the measured envelope floor and the page that is added to it: the summary
 * digits move with the counts, and a group contributes a separator that is charged at its start.
 * A page that lands a little short is fine; one that lands long is cut by `emit`.
 */
const SKELETON_SLACK_BYTES = 128;

/**
 * Bytes a row may cost beyond its own field lengths: JSON escaping in a name (a quote or a backslash
 * adds one) and the group separators. Escapes in indexed names are rare, so this is a floor under the
 * estimate rather than a per-row allowance — `emit` still measures the page it is handed.
 */
const ITEM_SLACK_BYTES = 8;

/**
 * How much of the byte allowance the evidence lists may claim: `unresolved` plus `not_indexed` may
 * take a quarter, and the neighbour rows — the answer — keep the rest. Before this cap, a dense
 * symbol's 94 unresolved call sites alone were ~90 % of a 3 200-token budget.
 */
const EVIDENCE_SHARE = 4;

/**
 * Page emits per response. Sizing is one emit; a cut `emit` makes anyway is re-sized from what
 * survived, and five attempts is far past the one or two a real payload needs. With the terminal
 * page's own emit, six calls size any call this tool can make.
 */
const MAX_SIZING_PASSES = 5;

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

/** The flattened sequence's two directions, in the order the sequence is defined by `in` then `out`. */
const DIRECTIONS = ["in", "out"] as const;

/**
 * Bytes a serialised value occupies. `emit` owns the budget decision, but sizing a page needs a
 * number *before* there is an envelope to hand it, so the same measurement is taken here.
 */
function serialisedBytes(value: unknown): number {
  const text = JSON.stringify(value) as string | undefined;
  return text === undefined ? 0 : Buffer.byteLength(text, "utf8");
}

/**
 * Bytes a row's punctuation costs, measured once from a template with empty fields and one-digit
 * numbers: the key order is `toItem`'s, and the template is cheaper to read than a hand-counted
 * constant is to verify.
 */
const ITEM_PUNCTUATION_BYTES = serialisedBytes({
  id: 0,
  qn: "",
  kind: "",
  file: "",
  line: 0,
  hop: 0,
});

/** `,"external":true` and `,"is_test":true`, present only when true (`toItem`). */
const EXTERNAL_FLAG_BYTES = Buffer.byteLength(',"external":true', "utf8");
const IS_TEST_FLAG_BYTES = Buffer.byteLength(',"is_test":true', "utf8");

/** Digits of a non-negative integer, without allocating a string for the common widths. */
function digitCount(value: number): number {
  if (value < 10) {
    return 1;
  }
  if (value < 100) {
    return 2;
  }
  if (value < 1_000) {
    return 3;
  }
  if (value < 10_000) {
    return 4;
  }
  if (value < 1_000_000) {
    return 5;
  }
  return String(Math.trunc(value)).length;
}

/**
 * What one row adds to its group: its own field sizes plus the punctuation `toItem` writes.
 *
 * Measured arithmetically rather than serialised, because sizing a dense page walks the sequence
 * row by row and serialising every row to learn its length costs more than the walk it is sizing.
 * `ITEM_SLACK_BYTES` covers what a string length cannot see, and the estimate is only a starting
 * point: `emit` measures the page it is handed.
 */
function itemBytes(item: TraceItem): number {
  return (
    ITEM_PUNCTUATION_BYTES +
    (digitCount(item.id) - 1) +
    (digitCount(item.line) - 1) +
    (digitCount(item.hop) - 1) +
    Buffer.byteLength(item.qn, "utf8") +
    Buffer.byteLength(item.kind, "utf8") +
    Buffer.byteLength(item.file, "utf8") +
    (item.external === true ? EXTERNAL_FLAG_BYTES : 0) +
    (item.is_test === true ? IS_TEST_FLAG_BYTES : 0) +
    ITEM_SLACK_BYTES
  );
}

/**
 * What one group adds on top of its rows: `{"edge":…,"tier":…,"items":[]}`. Its `[]` is charged
 * here, its rows are charged per row, and the separator before a later group is charged with it.
 */
function groupBytes(edge: string, tier: string): number {
  return serialisedBytes({ edge, tier, items: [] }) + 1;
}

/**
 * The longest prefix of the remaining neighbour sequence that fits `allowance` bytes, materialised
 * in the same forward pass that measures it: `in` groups in the fixed order, then `out`.
 *
 * Two calls with the same arguments and the same generation walk the same sequence in the same
 * order, which is what makes `position` meaningful as an index into it (`DATABASE.md` §8); this
 * pass only picks where the page ends. It stops at the first row that does not fit, so a symbol with
 * 3 000 neighbours costs the page it serves and not the 3 000 rows it does not.
 *
 * The number is an estimate, not the sizing decision: `emit` measures the envelope it is handed and
 * cuts it if the estimate was optimistic, and the caller re-sizes from what survived.
 */
function takePage(
  groups: {
    readonly in: readonly NeighbourGroup[];
    readonly out: readonly NeighbourGroup[];
  },
  start: number,
  allowance: number,
): FlatEntry[] {
  const entries: FlatEntry[] = [];
  let used = 0;
  let offset = 0;
  let run = "";

  for (const direction of DIRECTIONS) {
    for (const group of orderedGroups(groups[direction])) {
      const items = group.items;
      // Rows of this group that belong to earlier pages are skipped, not counted.
      const first = Math.max(0, start - offset);
      offset += items.length;
      if (first >= items.length) {
        continue;
      }
      const key = `${direction}\u0000${group.edge}\u0000${group.tier}`;
      for (let index = first; index < items.length; index += 1) {
        const neighbour = items[index];
        if (neighbour === undefined) {
          continue;
        }
        const item = toItem(neighbour);
        // The comma before this row, and the group wrapper when the page opens a new group.
        const cost =
          itemBytes(item) +
          (entries.length > 0 ? 1 : 0) +
          (key === run ? 0 : groupBytes(group.edge, group.tier));
        if (used + cost > allowance) {
          return entries;
        }
        used += cost;
        run = key;
        entries.push({ direction, edge: group.edge, tier: group.tier, item });
      }
    }
  }
  return entries;
}

/** Groups rebuilt from a page, keeping the order the entries arrived in. */
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
 * How many entries of a page `emit` still carried, counted as a prefix.
 *
 * `emit` pops the last row of the largest list, which can be a whole group rather than the tail of
 * this sequence, so what it delivered is not necessarily a prefix. The page is cut back to the prefix
 * it did deliver and the remainder is served by the next page; that is what keeps pages
 * duplicate-free when `emit` disagrees with the page's own estimate.
 */
function leadingSurvivors(payload: TracePayload, page: readonly FlatEntry[]): number {
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
  for (const entry of page) {
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

/**
 * The longest prefix of `rows` whose serialised form fits `maxBytes`, separators included. The tail
 * is what goes: these lists are read from the top, and `coverage` keeps the exact counts, so a
 * shortened list still reads as "some of N".
 */
function capRows<T>(rows: readonly T[], maxBytes: number): T[] {
  const kept: T[] = [];
  let used = 0;
  for (const row of rows) {
    const size = serialisedBytes(row) + (kept.length > 0 ? 1 : 0);
    if (used + size > maxBytes) {
      break;
    }
    used += size;
    kept.push(row);
  }
  return kept;
}

/**
 * The byte allowance a budget buys, computed the way `budget.ts` computes it so a page is sized
 * against the number `emit` will apply. A budget above the protocol ceiling is still `emit`'s
 * refusal: sizing it as if it were legal is what lets `emit` answer exactly once.
 */
function allowanceOf(budgetTokens: number): number {
  return Math.max(0, Math.floor(budgetTokens) * BYTES_PER_TOKEN);
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

/**
 * How many neighbours the walk reached in each direction. Counted over the groups rather than over
 * a materialised sequence: the totals and the page arithmetic need the counts, not the rows.
 */
function countsOf(groups: {
  readonly in: readonly NeighbourGroup[];
  readonly out: readonly NeighbourGroup[];
}): { in: number; out: number } {
  let inCount = 0;
  for (const group of groups.in) {
    inCount += group.items.length;
  }
  let outCount = 0;
  for (const group of groups.out) {
    outCount += group.items.length;
  }
  return { in: inCount, out: outCount };
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

/**
 * The counts line every trace summary opens with. Extracted because a page that could not be served
 * still has to say what it is a page *of*.
 */
function headOf(
  node: NodeRow,
  counts: { in: number; out: number },
  direction: TraceDirection,
): string {
  return direction === "in"
    ? `${node.name}: ${counts.in} callers`
    : direction === "out"
      ? `${node.name}: ${counts.out} callees`
      : `${node.name}: ${counts.in} callers, ${counts.out} callees`;
}

function summaryOf(
  node: NodeRow,
  counts: { in: number; out: number },
  direction: TraceDirection,
  page: { shown: number; available: number } | null,
  unresolvedCount: number,
): string {
  const head = headOf(node, counts, direction);
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

/**
 * A page that fits no neighbour at all: the terminal answer to a budget below one row.
 *
 * It is the *only* zero-row page this tool serves. A zero-row page that carries a continuation token
 * would hand the client the position it just asked from, and a client that follows `next` would read
 * the same envelope forever and never reach a single caller; so the page states the floor it hit and
 * points at the cheaper query instead of minting one.
 *
 * The evidence lists ride along when they fit (`emit` measures), because "there are 94 unbound call
 * sites here" is still true and still useful even when no neighbour row can be served.
 */
function noNeighbourFits(page: {
  readonly node: NodeRow;
  readonly counts: { readonly in: number; readonly out: number };
  readonly direction: TraceDirection;
  readonly query: string;
  readonly resolution: Extract<Resolution, { status: "ok" }>;
  readonly readStatus: ReadStatus;
  readonly coverage: Coverage;
  readonly totals: Totals;
  readonly unresolved: readonly TraceUnresolved[];
  readonly notIndexed: readonly NotIndexedEntry[];
  readonly budgetTokens: number;
  readonly remaining: number;
}): Envelope<TracePayload> {
  const gap =
    page.remaining > 0
      ? `a single neighbour does not fit budget ${page.budgetTokens} tokens`
      : `the remaining page does not fit budget ${page.budgetTokens} tokens`;
  const spec: EnvelopeSpec = {
    summary: `${headOf(page.node, page.counts, page.direction)}; ${gap}, re-run trace with depth: 1`,
    read_status: page.readStatus,
    coverage: page.coverage,
    totals: page.totals,
    next_tools: NEXT_TOOLS_NARROWER,
    truncated: true,
    truncation_reason: "budget",
    next: null,
  };
  const served = emit(
    ok(
      payloadOf(page.node, page.query, page.resolution, [], page.unresolved, page.notIndexed),
      spec,
    ),
    page.budgetTokens,
    // A terminal page never carries a token: the cut is already at the envelope's floor, so a minted
    // position could only repeat the one this call was given.
    { mint: () => "" },
  );
  return { ...served, next: null, next_tools: [...NEXT_TOOLS_NARROWER] };
}

/**
 * A truncation that could not advance the cursor is terminal. The candidate and near-miss lists are
 * capped upstream, so a cut here means the budget is below the envelope's floor: minting a position at
 * the head of that list would re-serve the same candidates forever.
 */
function noRepeat<T>(envelope: Envelope<T>): Envelope<T> {
  return envelope.truncated && envelope.next !== null ? { ...envelope, next: null } : envelope;
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
  const budgetTokens = budgetOf(args);
  const readStatus = readStatusFor(ctx, meta, generation, node);
  const assembly: CoverageAssembly = assemble(ctx.reader, {
    paths: [node.path],
    generation,
    stale: readStatus.stale,
    fileIds: [node.fileId],
  });
  const grouped = expand(ctx.reader, [node.id], { direction, depth: depthOf(args) });
  const counts = countsOf(grouped);
  const totals = totalsOf(counts, direction, assembly.relation);
  // Neighbours still to serve once the rows earlier pages delivered are discounted.
  const remaining = Math.max(0, counts.in + counts.out - start);

  // Evidence may not outbid content. `unresolved` is the explanation of a miss and `not_indexed` is
  // the caveat on coverage, so between them they may claim a quarter of the allowance; the neighbour
  // rows — the answer — keep the rest. Both counts stay exact in `coverage`, so a shortened list is
  // still readable as "some of N".
  const allowance = allowanceOf(budgetTokens);
  const evidenceAllowance = Math.floor(allowance / EVIDENCE_SHARE);
  const notIndexed = capRows(assembly.coverage.not_indexed, Math.floor(evidenceAllowance / 2));
  const unresolvedAll = unresolvedOf(assembly.unresolved);
  const unresolved = capRows(
    unresolvedAll,
    Math.max(0, evidenceAllowance - serialisedBytes(notIndexed)),
  );
  const coverage: Coverage = { ...assembly.coverage, not_indexed: notIndexed };
  const spec: EnvelopeSpec = {
    summary: summaryOf(node, counts, direction, null, unresolvedAll.length),
    read_status: readStatus,
    coverage,
    totals,
    next_tools: NEXT_TOOLS,
  };

  const pageEnvelope = (
    entries: readonly FlatEntry[],
    truncated: boolean,
  ): Envelope<TracePayload> => {
    const end = start + entries.length;
    const pageSpec: EnvelopeSpec = {
      ...spec,
      summary: summaryOf(
        node,
        counts,
        direction,
        truncated ? { shown: entries.length, available: remaining } : null,
        unresolvedAll.length,
      ),
      truncated,
      truncation_reason: truncated ? "budget" : null,
      next: truncated
        ? ctx.cursor.sign({ generation, tool: TOOL, argsHash: identity.argsHash, position: end })
        : null,
    };
    return emit(
      ok(payloadOf(node, args.query, resolution, entries, unresolved, notIndexed), pageSpec),
      budgetTokens,
      { mint: mintAt(ctx, identity, end) },
    );
  };

  // What the envelope costs before a single neighbour is served. Measured, not guessed: the node, the
  // resolution block, the evidence and the continuation token all have to fit first, and the summary
  // is taken at its longest (every neighbour shown) so its digits cannot make the page overshoot.
  const floor = serialisedBytes(
    ok(payloadOf(node, args.query, resolution, [], unresolved, notIndexed), {
      ...spec,
      summary: summaryOf(
        node,
        counts,
        direction,
        { shown: remaining, available: remaining },
        unresolvedAll.length,
      ),
      truncated: true,
      truncation_reason: "budget",
      next: "x".repeat(CURSOR_RESERVE_CHARS),
    }),
  );

  const terminal = (): Envelope<TracePayload> =>
    noNeighbourFits({
      node,
      counts,
      direction,
      query: args.query,
      resolution,
      readStatus,
      coverage,
      totals,
      unresolved,
      notIndexed,
      budgetTokens,
      remaining,
    });

  // The page: the longest prefix of the remaining sequence that fits what is left of the allowance.
  let entries = takePage(grouped, start, allowance - floor - SKELETON_SLACK_BYTES);
  if (entries.length === 0 && remaining > 0) {
    return terminal();
  }

  let envelope = pageEnvelope(entries, entries.length < remaining);

  // `emit` measures the page it was handed and is the only sizing decision; if it cuts the page's
  // tail (the estimate was optimistic), the page shrinks to the prefix `emit` kept. That is what keeps
  // every page a contiguous slice of the sequence: no duplicates, and no neighbour skipped between
  // one page and the next. A page that cannot advance is terminal rather than a loop.
  for (let pass = 1; envelope.truncated && entries.length > 0; pass += 1) {
    const survived = leadingSurvivors(envelope.payload, entries);
    if (survived >= entries.length) {
      break; // only the evidence lists were cut; every neighbour of this page is here
    }
    if (survived === 0 || pass >= MAX_SIZING_PASSES) {
      return terminal();
    }
    entries = entries.slice(0, survived);
    envelope = pageEnvelope(entries, true);
  }

  // The progress guarantee, stated once for every path above: a truncated page never hands back a
  // position at or before the one it was given. Zero rows served means nothing can advance.
  if (envelope.truncated && envelope.next !== null && entries.length === 0) {
    return terminal();
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
      // nothing to cut unless the budget is below the envelope's own floor; a cut then cannot advance
      // the cursor, and re-serving the same candidates is a loop rather than a page.
      return noRepeat(
        emit(
          ambiguous<TraceCandidates>(
            { candidates: resolution.candidates.slice(0, CANDIDATE_CAP) },
            spec,
          ),
          budgetOf(args),
          { mint: mintAt(ctx, identity, start) },
        ),
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
      return noRepeat(
        emit(
          notFound<TraceNearMisses>(
            { near_misses: resolution.near_misses.slice(0, CANDIDATE_CAP) },
            spec,
          ),
          budgetOf(args),
          { mint: mintAt(ctx, identity, start) },
        ),
      );
    }
    default:
      return traced(ctx, args, resolution, meta, start, identity);
  }
}
