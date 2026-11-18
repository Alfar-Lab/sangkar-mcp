/**
 * `expand` — detail on demand (`TOOLS-SPEC.md` §7, `DESIGN.md` §7).
 *
 * One target goes in, one detail view comes out, and the view is *disclosed*: `payload.what` is
 * the mode that answered and `payload.why` says whether the kind inferred it or the caller asked
 * for it. Inference is never silent, which is the whole reason the other six tools can stay
 * small — a wrong guess is visible in the answer instead of being buried in a flag.
 *
 * Target addressing, and the honest subset of it this reader can answer. The spec says "pass the
 * id, qualified name or path"; the reader has no lookup by id (`GraphReader` exposes `findByQn`,
 * `findByName`, `fileByPath`, `members` and `nodesByFile`, and inventing an id index here would
 * be a query shape the store does not have). So:
 *
 * - a qualified name (`path::scope.name`) or a bare name resolves through `#shared/domain/resolve`;
 * - a repo-relative path resolves to the file node that owns it, or to a symbol declared in it;
 * - a folder prefix resolves to nothing at all — it is answered from coverage rows;
 * - a bare numeric id is refused with a hint naming the qn form. It is not silently reinterpreted
 *   as a name, because "1006" as a symbol name does not exist and a fabricated match would be the
 *   confident answer about the wrong node that `resolve` exists to prevent.
 *
 * Reading bytes. `what: "source"` is the one mode that touches the working tree, and it is
 * permitted to by this tool's contract. It verifies the file's `sha256` against the indexed row
 * before returning any text: a mismatch means the graph's view of that file is out of date, so the
 * answer is `source_drift: true` with `content: null` rather than stale lines the agent would
 * quote as current. Every other mode answers from the graph alone.
 *
 * That gives the freshness rule for `read_status.stale`: it is true exactly when this call
 * observed drift in the bytes it read, and `stale_checked_at` is present exactly when a check ran.
 * A mode that read no bytes omits it rather than claiming a working-tree verdict it did not
 * compute — `status` owns the repo-wide check.
 */
import { isAbsolute, relative, resolve as resolvePath } from "node:path";
import { budgetFloorOf, emit } from "#shared/budget";
import { assemble } from "#shared/domain/coverage";
import {
  type ExpandDirection,
  expand as expandNeighbours,
  type NeighbourGroup,
} from "#shared/domain/neighbors";
import { type ResolutionCandidate, resolve as resolveTarget } from "#shared/domain/resolve";
import {
  ambiguous,
  bootstrapping,
  type Coverage,
  type Envelope,
  fail,
  notFound,
  ok,
  type ReadStatus,
  type TotalRelation,
  type Totals,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import { normalizeRel } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type { SkippedPathRow } from "#shared/store/read/coverage";
import type { GraphReader, IndexedFileRow, NodeRow } from "#shared/store/read/reader";
import { DEFAULT_BUDGET_TOKENS, input } from "./manifest";

/** The tool name as the cursor signs it; also what `next_tools` and logs spell. */
export const TOOL_NAME = "expand";

/**
 * The four detail modes, in the order `TOOLS-SPEC.md` §7 lists them. Taken from the manifest's own
 * enum so the declared schema and the inference table cannot drift apart.
 */
export const EXPAND_WHATS = input.what.values;
export type ExpandWhat = (typeof EXPAND_WHATS)[number];

/** `[start_line, end_line]`, the pair every payload row shows. */
export type LineWindow = readonly [start: number, end: number];

export interface ExpandArgs {
  target: string;
  what?: ExpandWhat;
  budget?: number;
}

/** The two numbers `metricsFor` publishes per node; `null` when the publish never measured it. */
export interface Degree {
  readonly in: number;
  readonly out: number;
}

/**
 * The address that was expanded, restated in full (`TOOLS-SPEC.md` §7): the agent may be
 * expanding a handle it obtained several turns ago, so repeating `{qn, kind, lines}` is what
 * removes the mismatched-id class of bug. A folder has no node behind it, so `id`, `qn` and
 * `lines` are `null` and `kind` names the address form (`folder`) instead of a node kind.
 */
export interface TargetRef {
  readonly id: number | null;
  readonly qn: string | null;
  readonly kind: string;
  readonly lines: LineWindow | null;
}

/** One direct child of the expanded node, with the degrees that make it worth expanding next. */
export interface MemberRow {
  readonly id: number;
  readonly name: string;
  readonly kind: string;
  readonly lines: LineWindow;
  readonly degree: Degree | null;
}

export interface MembersPayload {
  readonly target: TargetRef;
  readonly what: "members";
  readonly why: string;
  readonly members: MemberRow[];
}

/**
 * The grouped neighbour shape `trace` ships, for one node at one hop. `in`/`out` are always
 * present so a caller can read them unconditionally; the direction that was not asked for is
 * empty rather than absent.
 */
export interface EdgesPayload {
  readonly target: TargetRef;
  readonly what: "edges";
  readonly why: string;
  readonly direction: ExpandDirection;
  readonly depth: number;
  readonly in: readonly NeighbourGroup[];
  readonly out: readonly NeighbourGroup[];
}

/** One indexed file under the prefix, trimmed to what a coverage answer is read for. */
export interface CoverageFileRow {
  readonly id: number;
  readonly path: string;
  readonly language: string;
  readonly sha256: string;
  readonly line_count: number;
  readonly parse_status: string;
  readonly indexed_generation: string;
}

/** One path the indexer saw and did not index, with the reason it says so. */
export interface CoverageSkipRow {
  readonly path: string;
  readonly reason: string;
  readonly detail: string | null;
}

export interface CoveragePayload {
  readonly target: TargetRef;
  readonly what: "coverage";
  readonly why: string;
  readonly prefix: string;
  readonly files: CoverageFileRow[];
  readonly not_indexed: CoverageSkipRow[];
}

/** One numbered source line, so a quoted snippet carries its own position. */
export interface SourceLine {
  readonly line: number;
  readonly text: string;
}

/**
 * A verified line window. `content` is `null` only when `source_drift` is true: absence of a list
 * means "empty", so an unreadable answer is stated as `null` rather than as zero lines.
 */
export interface SourcePayload {
  readonly target: TargetRef;
  readonly what: "source";
  readonly why: string;
  readonly path: string;
  readonly source_drift: boolean;
  /** The window that was asked for, clamped to the bytes that exist on disk. */
  readonly window: LineWindow;
  readonly content: SourceLine[] | null;
}

/** A target that matched more than one node; the envelope carries no answer body beside these. */
export interface AmbiguousPayload {
  readonly target: string;
  readonly candidates: readonly ResolutionCandidate[];
  readonly hint: string;
}

/** A target nothing answered for, with the forms that would have been accepted. */
export interface MissingPayload {
  readonly target: string;
  readonly why: string;
  readonly hint: string;
}

export type ExpandPayload =
  | MembersPayload
  | EdgesPayload
  | CoveragePayload
  | SourcePayload
  | AmbiguousPayload
  | MissingPayload;

/**
 * Kinds whose detail is what they contain. `file` and `module` are containers in this graph, not
 * callables, so their detail is the symbols declared inside them. The type kinds a grammar may
 * emit one day (`interface`, `enum`, `struct`, …) are listed here because they are containers
 * wherever they appear: inferring nothing for a kind the indexer *can* produce would turn a
 * legitimate node into a `not_found`.
 */
const CONTAINER_KINDS: ReadonlySet<string> = new Set([
  "file",
  "module",
  "class",
  "interface",
  "enum",
  "struct",
  "type",
  "namespace",
  "trait",
]);

/** Kinds whose detail is what they call and what calls them. */
const CALLABLE_KINDS: ReadonlySet<string> = new Set([
  "function",
  "method",
  "constructor",
  "getter",
  "setter",
  "test",
  "endpoint",
]);

/** A target made only of digits: an id, which this tool cannot resolve (see the module comment). */
const BARE_ID = /^[0-9]+$/;

/**
 * The generation id `read_status` reports before the first publish. No generation can hold it —
 * `DATABASE.md` §3 formats them as `g-<hex>` — so it names the state instead of inventing an id.
 */
const UNPUBLISHED = "unpublished";

/** How long an unpublished graph asks to be retried in; the first index is minutes, not seconds. */
const BOOTSTRAP_RETRY_MS = 5_000;

/**
 * `expand` is the leaf of the detail chain, so its own continuations are graph-level: trace a
 * member it just named, or check freshness when drift was reported.
 */
const NEXT_TOOLS = ["trace", "status"] as const;

const HINT_FORMS =
  "pass a qualified name (path::scope.name), a repo-relative path, or a folder prefix";
const HINT_ID =
  "a numeric id is not an address on its own: pass the item's qualified name (path::scope.name)";
const HINT_WHAT = `pass one of: ${EXPAND_WHATS.join(", ")}`;

/**
 * What this call learned about the working tree. A union rather than a boolean so that "stale"
 * cannot be reported without the reason that makes it actionable (`DESIGN.md` §6).
 */
type Freshness = { readonly stale: false } | { readonly stale: true; readonly reason: string };

const FRESH: Freshness = { stale: false };

/** The coverage an answer with no scope can claim: zero files, and therefore nothing complete. */
const NO_COVERAGE: Coverage = {
  complete: false,
  indexed: 0,
  skipped: 0,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

/** The read status, the coverage and the relation they support for one scope of one answer. */
interface Scoped {
  readonly read_status: ReadStatus;
  readonly coverage: Coverage;
  readonly relation: TotalRelation;
}

function isExpandWhat(value: unknown): value is ExpandWhat {
  return typeof value === "string" && (EXPAND_WHATS as readonly string[]).includes(value);
}

/**
 * `stale_checked_at` is the difference between "we looked" and "we assume": it is stamped only
 * when this call actually compared bytes against the index, so its absence means "not checked by
 * this tool", never "checked and fresh".
 */
function readStatusOf(
  generation: string,
  indexedAt: string,
  freshness: Freshness,
  now: Date,
): ReadStatus {
  if (!freshness.stale) {
    return { generation, indexed_at: indexedAt, stale: false };
  }
  return {
    generation,
    indexed_at: indexedAt,
    stale: true,
    stale_reason: freshness.reason,
    stale_checked_at: now.toISOString(),
  };
}

/**
 * The envelope's own `coverage` for this answer, assembled by `#shared/domain/coverage` so
 * `expand` cannot disagree with the other tools about whether an empty list is a fact. `paths`
 * is the one file the answer is about, or the folder prefix, or empty when nothing matched.
 */
function scopeOf(
  reader: GraphReader,
  generation: string,
  indexedAt: string,
  paths: readonly string[],
  freshness: Freshness,
  now: Date,
): Scoped {
  const assembly = assemble(reader, { paths, generation, stale: freshness.stale });
  return {
    read_status: readStatusOf(generation, indexedAt, freshness, now),
    coverage: assembly.coverage,
    relation: assembly.relation,
  };
}

function targetRefOf(node: NodeRow): TargetRef {
  return {
    id: node.id,
    qn: node.qn,
    kind: node.kind,
    lines: [node.startLine, node.endLine],
  };
}

function folderRef(): TargetRef {
  return { id: null, qn: null, kind: "folder", lines: null };
}

function labelOf(target: TargetRef): string {
  return target.qn ?? target.kind;
}

/** `null` is "the publish did not measure this node", which is not the same answer as degree 0. */
function degreeOf(reader: GraphReader, nodeId: number): Degree | null {
  const metrics = reader.metricsFor(nodeId);
  return metrics === null ? null : { in: metrics.inDegree, out: metrics.outDegree };
}

/**
 * A container's detail is its direct children, in source order — the reader's `members` query and
 * what `TOOLS-SPEC.md` §7 means by "members of GraphStore". A file's children are therefore its
 * top-level symbols (or its module node, where the grammar declares one), and a class's are its
 * methods: one level, so the agent can pick what to expand next instead of reading the file.
 */
function memberRowsOf(reader: GraphReader, node: NodeRow): MemberRow[] {
  return reader.members(node.id).map((child) => ({
    id: child.id,
    name: child.name,
    kind: child.kind,
    lines: [child.startLine, child.endLine],
    degree: degreeOf(reader, child.id),
  }));
}

function fileRowsOf(rows: readonly IndexedFileRow[]): CoverageFileRow[] {
  return rows.map((row) => ({
    id: row.id,
    path: row.path,
    language: row.language,
    sha256: row.sha256,
    line_count: row.lineCount,
    parse_status: row.parseStatus,
    indexed_generation: row.indexedGeneration,
  }));
}

function skipRowsOf(rows: readonly SkippedPathRow[]): CoverageSkipRow[] {
  return rows.map((row) => ({ path: row.path, reason: row.reason, detail: row.detail }));
}

function countItems(groups: readonly NeighbourGroup[]): number {
  let total = 0;
  for (const group of groups) {
    total += group.items.length;
  }
  return total;
}

/**
 * The absolute path of an indexed file, or `null` when the stored path would leave the tree.
 * The index stores repo-relative paths, so an escaping one is a writer bug; refusing it here is
 * what keeps `what: "source"` from becoming an arbitrary-file reader (`DESIGN.md` §10.4).
 */
function sourcePathOf(repoRoot: string, storedPath: string): string | null {
  const rel = normalizeRel(storedPath);
  if (rel === "" || rel.startsWith("..") || isAbsolute(rel) || /^[A-Za-z]:/.test(rel)) {
    return null;
  }
  const absolute = resolvePath(repoRoot, rel);
  const back = relative(repoRoot, absolute);
  if (back === "" || back.startsWith("..") || isAbsolute(back)) {
    return null;
  }
  return absolute;
}

function sha256Of(bytes: Uint8Array): string {
  return new Bun.CryptoHasher("sha256").update(bytes).digest("hex");
}

/** The bytes on disk, or `null` when the path is gone or unreadable — both are drift. */
async function readBytes(absolute: string): Promise<Uint8Array | null> {
  try {
    return new Uint8Array(await Bun.file(absolute).arrayBuffer());
  } catch {
    return null;
  }
}

/**
 * The numbered lines of `[from, to]` inside `text`. A window past the end of the file clamps;
 * a window that starts past it is empty rather than an error, because the index's line numbers
 * for a verified file are facts about that file.
 */
function windowLines(text: string, from: number, to: number): SourceLine[] {
  const all = text.split(/\r?\n/);
  const start = Math.max(1, from);
  const end = Math.min(to, all.length);
  const lines: SourceLine[] = [];
  for (let number = start; number <= end; number += 1) {
    lines.push({ line: number, text: all[number - 1] ?? "" });
  }
  return lines;
}

/** A cursor `argsHash` in the shape `tests/cursor.test.ts` already uses: `sha256:<hex>`. */
function argsHashOf(target: string, what: ExpandWhat | null): string {
  const digest = new Bun.CryptoHasher("sha256")
    .update(JSON.stringify({ target, what }))
    .digest("hex");
  return `sha256:${digest}`;
}

function membersSummary(payload: MembersPayload, totals: Totals, truncated: boolean): string {
  const shown = payload.members.length;
  const total = totals.members_total ?? shown;
  const head = `members of ${labelOf(payload.target)}`;
  return truncated ? `${head}, showing ${shown} of ${total}.` : `${head}: ${total}.`;
}

function edgesSummary(payload: EdgesPayload, totals: Totals, truncated: boolean): string {
  const shown = countItems(payload.in) + countItems(payload.out);
  const total = totals.edges_total ?? shown;
  const head = `neighbours of ${labelOf(payload.target)} at depth ${payload.depth}`;
  return truncated ? `${head}, showing ${shown} of ${total}.` : `${head}: ${total}.`;
}

function coverageSummary(payload: CoveragePayload, totals: Totals, truncated: boolean): string {
  const shown = payload.files.length;
  const total = totals.files_total ?? shown;
  const skipped = totals.not_indexed_total ?? payload.not_indexed.length;
  const head = `coverage for ${payload.prefix}`;
  const body = truncated
    ? `showing ${shown} of ${total} files, ${skipped} not indexed`
    : `${total} files, ${skipped} not indexed`;
  return `${head}: ${body}.`;
}

function sourceSummary(payload: SourcePayload): string {
  if (payload.source_drift) {
    return `${payload.path} changed since the index: no source returned.`;
  }
  return `lines ${payload.window[0]}-${payload.window[1]} of ${payload.path}.`;
}

/**
 * The one place this tool applies a budget. `emit` is the only sizing/truncation point in the
 * codebase, so the summary is rewritten *after* it ran: only then does "showing N of M" know what
 * was actually delivered.
 *
 * The `next` token `emit` mints is a signed cursor bound to this target, its mode and the
 * generation. §7's argument table declares no `cursor` field, so this tool cannot accept one back:
 * a truncation here is flagged — `truncated`, `truncation_reason`, `next`, and a `gte` relation —
 * but not resumable. What survives a cut is the head of the reader's stable order (source order
 * for members, grouped by edge then tier for edges) rather than an arbitrary sample.
 */
function sized<T extends ExpandPayload>(
  ctx: ToolContext,
  generation: string,
  call: { readonly target: string; readonly what: ExpandWhat | null },
  budget: number,
  env: Envelope<T>,
  summarize: (env: Envelope<T>) => string,
): Envelope<T | null> {
  const argsHash = argsHashOf(call.target, call.what);
  try {
    const emitted = emit(env, budget, {
      mint: (position) => ctx.cursor.sign({ generation, tool: TOOL_NAME, argsHash, position }),
    });
    // Below the floor this answer needs, `emit` returns the envelope untouched rather than
    // mislabelling a cut. Serving that would hand the caller a payload many times its allowance (a
    // dense `members` list is hundreds of kilobytes), so the answer is the floor to raise to.
    const floor = budgetFloorOf(emitted);
    if (floor > budget) {
      return fail(
        new AppError("budget_too_large", `budget ${budget} is below the floor this answer needs`, {
          hint: `raise the budget to at least ${floor} tokens; the default is ${DEFAULT_BUDGET_TOKENS}`,
        }),
        { ...env, summary: summarize(env) },
      );
    }
    return { ...emitted, summary: summarize(emitted) };
  } catch (error) {
    // `emit` refuses a budget above the protocol ceiling with `budget_too_large`; a handler
    // returns, it never throws (`TECH-SPEC.md` §6).
    return fail(error, { ...env, summary: summarize(env) });
  }
}

/**
 * Answers one `expand` call. Every branch returns an envelope built from `#shared/envelope` and
 * sized by `emit`; nothing here opens a connection, reads the environment or writes to stdout.
 */
export async function handle(
  ctx: ToolContext,
  args: ExpandArgs,
): Promise<Envelope<ExpandPayload | null>> {
  const target = args.target.trim();
  const meta = ctx.reader.metaFor();
  const publish = ctx.reader.latestGeneration();

  if (meta.generation === null || publish === null) {
    return bootstrapping({
      summary: "the graph has not published a generation yet",
      read_status: {
        generation: UNPUBLISHED,
        indexed_at: "",
        stale: true,
        stale_reason: "no published generation",
      },
      coverage: NO_COVERAGE,
      retry_after_ms: BOOTSTRAP_RETRY_MS,
      message: "the index is still being built",
      hint: "call status and retry; expand answers from a published generation",
    });
  }

  const generation = meta.generation;
  const indexedAt = publish.publishedAt;
  const now = ctx.now();
  const budget = args.budget ?? DEFAULT_BUDGET_TOKENS;
  const empty = scopeOf(ctx.reader, generation, indexedAt, [], FRESH, now);
  const base = {
    read_status: empty.read_status,
    coverage: empty.coverage,
    next_tools: NEXT_TOOLS,
  };

  const requested = args.what;
  if (requested !== undefined && !isExpandWhat(requested)) {
    return fail(
      new AppError("internal", `unknown what "${String(requested)}"`, { hint: HINT_WHAT }),
      { ...base, summary: `unknown what "${String(requested)}"` },
    );
  }

  if (target === "") {
    return notFound(
      { target: args.target, why: "the target is empty", hint: HINT_FORMS },
      { ...base, summary: "no target to expand" },
    );
  }

  if (BARE_ID.test(target)) {
    return notFound(
      { target, why: "a bare numeric id cannot be resolved", hint: HINT_ID },
      { ...base, summary: `id ${target} is not an address expand can resolve` },
    );
  }

  const resolution = resolveTarget(ctx.reader, target);

  if (resolution.status === "ambiguous") {
    const candidates = resolution.candidates;
    const payload: AmbiguousPayload = {
      target,
      candidates,
      hint: "pass the qn of the item you meant; expand never picks one of a tie",
    };
    const spec = {
      ...base,
      summary: `"${target}" matches ${candidates.length} nodes; pass the qn of the one you meant`,
      totals: { candidates_total: candidates.length, total_relation: "gte" as const },
    };
    return sized(
      ctx,
      generation,
      { target, what: requested ?? null },
      budget,
      ambiguous(payload, spec),
      (env) => env.summary,
    );
  }

  if (resolution.status === "ok") {
    const node = resolution.node;
    const inferred = inferWhat(node.kind);
    const chosen = requested ?? inferred;
    if (chosen === null) {
      const scope = scopeOf(ctx.reader, generation, indexedAt, [node.path], FRESH, now);
      return notFound(
        {
          target,
          why: `kind "${node.kind}" has no default detail`,
          hint: `pass what explicitly: ${HINT_WHAT}`,
        },
        {
          summary: `kind "${node.kind}" has no default detail`,
          read_status: scope.read_status,
          coverage: scope.coverage,
          next_tools: NEXT_TOOLS,
          totals: { total_relation: scope.relation },
        },
      );
    }
    const why =
      requested === undefined ? `inferred: ${node.kind} -> ${chosen}` : `requested: ${chosen}`;
    return await answerFor(ctx, {
      generation,
      indexedAt,
      now,
      budget,
      target,
      node,
      what: chosen,
      why,
    });
  }

  // No node matched. The one remaining address is a folder prefix, and only coverage answers it.
  const prefix = normalizeRel(target);
  const rows = prefix === "" ? { indexed: [], notIndexed: [] } : ctx.reader.rowsForPaths([prefix]);
  const isFolder = rows.indexed.length > 0 || rows.notIndexed.length > 0;
  if (isFolder && (requested === undefined || requested === "coverage")) {
    const why = requested === undefined ? "inferred: folder -> coverage" : "requested: coverage";
    return coverageAnswer(ctx, {
      generation,
      indexedAt,
      now,
      budget,
      target,
      prefix,
      why,
      rows,
    });
  }

  const scope = scopeOf(ctx.reader, generation, indexedAt, [prefix], FRESH, now);
  const why = isFolder
    ? `"${prefix}" is a folder, so only coverage can be expanded from it`
    : "nothing in the index matches the target";
  return notFound(
    { target, why, hint: isFolder ? `pass what=coverage for a folder` : HINT_FORMS },
    {
      summary: isFolder
        ? `"${prefix}" is a folder; pass what=coverage`
        : `nothing in the graph matches "${target}"`,
      read_status: scope.read_status,
      coverage: scope.coverage,
      next_tools: NEXT_TOOLS,
      totals: { total_relation: scope.relation },
    },
  );
}

/**
 * The kind-to-`what` table (`TOOLS-SPEC.md` §7) plus the kinds this indexer declares. `null` means
 * this kind has no defensible default, which the caller reports as `not_found` with a hint rather
 * than guessing a mode the agent did not want.
 */
function inferWhat(kind: string): ExpandWhat | null {
  if (CONTAINER_KINDS.has(kind)) {
    return "members";
  }
  if (CALLABLE_KINDS.has(kind)) {
    return "edges";
  }
  return null;
}

interface AnswerRequest {
  readonly generation: string;
  readonly indexedAt: string;
  readonly now: Date;
  readonly budget: number;
  readonly target: string;
  readonly node: NodeRow;
  readonly what: ExpandWhat;
  readonly why: string;
}

/** Dispatches one resolved node to its detail mode. Each mode is its own answer. */
async function answerFor(
  ctx: ToolContext,
  request: AnswerRequest,
): Promise<Envelope<ExpandPayload | null>> {
  const { generation, indexedAt, now, budget, target, node, what, why } = request;

  if (what === "members") {
    const scope = scopeOf(ctx.reader, generation, indexedAt, [node.path], FRESH, now);
    const payload: MembersPayload = {
      target: targetRefOf(node),
      what: "members",
      why,
      members: memberRowsOf(ctx.reader, node),
    };
    const totals: Totals = {
      members_total: payload.members.length,
      total_relation: scope.relation,
    };
    return sized(
      ctx,
      generation,
      { target, what },
      budget,
      ok(payload, {
        summary: membersSummary(payload, totals, false),
        read_status: scope.read_status,
        coverage: scope.coverage,
        next_tools: NEXT_TOOLS,
        totals,
      }),
      (env) => membersSummary(env.payload, env.totals, env.truncated),
    );
  }

  if (what === "edges") {
    const scope = scopeOf(ctx.reader, generation, indexedAt, [node.path], FRESH, now);
    // Depth 1 is "the edges of this node": one hop is the grouped neighbour shape `trace` ships,
    // and a deeper walk is `trace`'s question, not an expansion of one item.
    const grouped = expandNeighbours(ctx.reader, [node.id], { direction: "all", depth: 1 });
    const payload: EdgesPayload = {
      target: targetRefOf(node),
      what: "edges",
      why,
      direction: grouped.direction,
      depth: grouped.depth,
      in: grouped.in,
      out: grouped.out,
    };
    const totals: Totals = {
      edges_total: countItems(payload.in) + countItems(payload.out),
      total_relation: scope.relation,
    };
    return sized(
      ctx,
      generation,
      { target, what },
      budget,
      ok(payload, {
        summary: edgesSummary(payload, totals, false),
        read_status: scope.read_status,
        coverage: scope.coverage,
        next_tools: NEXT_TOOLS,
        totals,
      }),
      (env) => edgesSummary(env.payload, env.totals, env.truncated),
    );
  }

  if (what === "coverage") {
    const rows = ctx.reader.rowsForPaths([node.path]);
    return coverageAnswer(ctx, {
      generation,
      indexedAt,
      now,
      budget,
      target,
      prefix: node.path,
      why,
      rows,
      node,
    });
  }

  // `source`: the only mode that reads bytes, and therefore the only one that can report drift.
  const file = ctx.reader.fileByPath(node.path);
  if (file === null) {
    const scope = scopeOf(ctx.reader, generation, indexedAt, [node.path], FRESH, now);
    return notFound(
      {
        target,
        why: `"${node.path}" is not an indexed file`,
        hint: "call status to see which paths the index covers",
      },
      {
        summary: `"${node.path}" is not an indexed file`,
        read_status: scope.read_status,
        coverage: scope.coverage,
        next_tools: NEXT_TOOLS,
        totals: { total_relation: scope.relation },
      },
    );
  }

  const absolute = sourcePathOf(ctx.config.repoRoot, node.path);
  if (absolute === null) {
    return fail(
      new AppError("internal", `refusing to read "${node.path}" outside the repository root`, {
        hint: "re-index; the stored path is repo-relative by construction",
      }),
      { summary: `refusing to read "${node.path}"`, ...baseSpec(generation, indexedAt, now) },
    );
  }

  const bytes = await readBytes(absolute);
  const driftReason =
    bytes === null
      ? `"${node.path}" is no longer readable on disk`
      : `"${node.path}" changed since the index`;
  const drifted = bytes === null || sha256Of(bytes) !== file.sha256.toLowerCase();
  const freshness: Freshness = drifted ? { stale: true, reason: driftReason } : FRESH;
  const scope = scopeOf(ctx.reader, generation, indexedAt, [node.path], freshness, now);
  const from = Math.max(1, node.startLine);
  const to = Math.max(from, node.endLine);
  const content =
    drifted || bytes === null ? null : windowLines(new TextDecoder().decode(bytes), from, to);
  const payload: SourcePayload = {
    target: targetRefOf(node),
    what: "source",
    why,
    path: node.path,
    source_drift: drifted,
    window: [from, to],
    content,
  };
  const totals: Totals = {
    lines_total: content === null ? 0 : content.length,
    total_relation: scope.relation,
  };
  return sized(
    ctx,
    generation,
    { target, what },
    budget,
    ok(payload, {
      summary: sourceSummary(payload),
      read_status: scope.read_status,
      coverage: scope.coverage,
      next_tools: NEXT_TOOLS,
      totals,
    }),
    (env) => sourceSummary(env.payload),
  );
}

interface CoverageRequest {
  readonly generation: string;
  readonly indexedAt: string;
  readonly now: Date;
  readonly budget: number;
  readonly target: string;
  readonly prefix: string;
  readonly why: string;
  readonly rows: {
    readonly indexed: readonly IndexedFileRow[];
    readonly notIndexed: readonly SkippedPathRow[];
  };
  readonly node?: NodeRow;
}

/** A folder answer, either inferred from a prefix that matched no node or asked for on a node. */
function coverageAnswer(
  ctx: ToolContext,
  request: CoverageRequest,
): Envelope<ExpandPayload | null> {
  const { generation, indexedAt, now, budget, target, prefix, why, rows, node } = request;
  const scope = scopeOf(ctx.reader, generation, indexedAt, [prefix], FRESH, now);
  const payload: CoveragePayload = {
    target: node === undefined ? folderRef() : targetRefOf(node),
    what: "coverage",
    why,
    prefix,
    files: fileRowsOf(rows.indexed),
    not_indexed: skipRowsOf(rows.notIndexed),
  };
  const totals: Totals = {
    files_total: payload.files.length,
    not_indexed_total: payload.not_indexed.length,
    total_relation: scope.relation,
  };
  return sized(
    ctx,
    generation,
    { target, what: "coverage" },
    budget,
    ok(payload, {
      summary: coverageSummary(payload, totals, false),
      read_status: scope.read_status,
      coverage: scope.coverage,
      next_tools: NEXT_TOOLS,
      totals,
    }),
    (env) => coverageSummary(env.payload, env.totals, env.truncated),
  );
}

/** The spec fields a `fail` needs when the answer never reached a scope of its own. */
function baseSpec(
  generation: string,
  indexedAt: string,
  now: Date,
): { read_status: ReadStatus; coverage: Coverage; next_tools: readonly string[] } {
  return {
    read_status: readStatusOf(generation, indexedAt, FRESH, now),
    coverage: NO_COVERAGE,
    next_tools: NEXT_TOOLS,
  };
}
