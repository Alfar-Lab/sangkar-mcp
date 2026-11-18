/**
 * `summary` — the hot path (`TOOLS-SPEC.md` §4): a few hundred tokens that say what changed,
 * how risky it is, and what to call next.
 *
 * One envelope, three payloads. The scope dispatch happens after the work every scope shares,
 * and the `repo`/`area` aggregates are never computed on the `change` path: the measured failure
 * in the parent tools was a "minimal context" call paying for an architecture overview.
 *
 * Every read comes from `ctx.reader`: no connection, no environment, no stdout. What this module
 * decides is *what to show* — which list leads, how many entry points are worth naming, where a
 * page boundary falls. Whether an absence may claim `eq` is `#shared/domain/coverage`'s decision,
 * taken once over the scope's paths, and how the payload is sized is `#shared/budget`'s.
 *
 * Pagination is one integer. Each scope names exactly one *paged* list — the ranked one for
 * `change` and `area`, the only unbounded one for `repo` — and the cursor's `position` is the
 * number of its items already delivered. The supporting lists (the changed files, the suggested
 * calls, the entry points) are module-sized and are rebuilt on every page, because they are the
 * context that makes a continuation readable rather than a second half of a ranking. Resuming
 * slices the paged list at `position`, so a continuation cannot repeat a ranked item. `emit`
 * remains the only thing that cuts the payload: its drop count becomes the next position, which
 * is exact while the paged list is the list being cut (it leads and is the largest).
 */
import { budgetFloorOf, emit } from "#shared/budget";
import { assemble, type CoverageSource } from "#shared/domain/coverage";
import { type ChangeSet, changedSinceFrom } from "#shared/domain/diff";
import { type ChangedFacts, type ImpactedFacts, score } from "#shared/domain/risk";
import {
  bootstrapping,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  ok,
  type ReadStatus,
  type TotalRelation,
  type Totals,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import { normalizeRel } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import {
  type GenerationRow,
  type IndexedFileRow,
  type NodeMetrics,
  type NodeRow,
  type PathCoverage,
  TEST_GAP_MAX_DEPTH,
} from "#shared/store/read/reader";
import { input } from "./manifest";

export interface SummaryArgs {
  readonly scope?: "change" | "repo" | "area";
  readonly target?: string;
  readonly base?: string;
  readonly budget?: number;
  readonly cursor?: string;
}

/**
 * `change` is the diff's letter-free view: `ChangeSet` keeps what changed and what is gone, not
 * git's `A`/`M` status, and a symbol this tool can name belongs to a file the index has a row for.
 * So a shown symbol is "modified"; inventing "added" from line numbers would be a guess.
 */
export const SYMBOL_CHANGE_MODIFIED = "modified";

export interface ChangedSymbol {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly change: string;
  readonly lines: [number, number];
  readonly risk: number;
}

export interface TestGap {
  readonly qn: string;
  readonly reason: string;
}

export interface RemovedSymbol {
  readonly qn: string;
  readonly kind: string;
  readonly had_callers: boolean;
}

export interface SuggestedNext {
  readonly tool: string;
  readonly query?: string;
  readonly why: string;
}

export interface RiskPayload {
  readonly score: number;
  readonly band: string;
  readonly drivers: string[];
}

export interface ChangePayload {
  readonly base: string;
  readonly changed_files: string[];
  readonly changed_symbols: ChangedSymbol[];
  readonly risk: RiskPayload;
  readonly test_gaps: TestGap[];
  readonly removed_symbols: RemovedSymbol[];
  readonly suggested_next: SuggestedNext[];
}

export interface RepoPayload {
  readonly size: { readonly files: number; readonly nodes: number; readonly edges: number };
  readonly languages: { readonly language: string | null; readonly files: number }[];
  readonly entry_points: {
    readonly qn: string;
    readonly kind: string;
    readonly reachable: number | null;
  }[];
  readonly coupling_hotspots: { readonly qn: string; readonly in: number; readonly out: number }[];
  readonly flows: { readonly name: string; readonly steps: number; readonly files: number }[];
  readonly not_indexed: { readonly path: string; readonly reason: string }[];
}

export interface AreaPayload {
  readonly target: string;
  readonly surface: { readonly qn: string; readonly kind: string }[];
  readonly dependents_total: number;
  readonly risk: RiskPayload;
}

export type SummaryPayload = ChangePayload | RepoPayload | AreaPayload;

/** Named so the tool name in a cursor and in a log line cannot drift from the manifest. */
const TOOL = "summary";

/**
 * Display bounds, not product knobs: ten entry points and three flows answer "what is this
 * repository" and ten hotspots answer "what is coupled"; the payload is ~300 tokens against the
 * 1200 budget, because a cold answer that does not fit is not a summary.
 */
const ENTRY_POINT_LIMIT = 10;
const HOTSPOT_LIMIT = 10;
const FLOW_LIMIT = 3;
const SUGGESTION_LIMIT = 3;

/** What `status`/`index` ask a client to wait when there is no published generation yet. */
const BOOTSTRAP_RETRY_MS = 2_000;

/** Ordered `next_tools`: how many of these an agent can follow from this payload's scope. */
const CHANGE_NEXT_TOOLS = ["impact", "trace", "expand"] as const;
const REPO_NEXT_TOOLS = ["trace", "impact", "search"] as const;
const AREA_NEXT_TOOLS = ["trace", "impact", "expand"] as const;

/**
 * The part of a paged list that belongs to a page starting at `start`; a position past the end is
 * an empty page rather than an error, because the last page is allowed to be the short one.
 */
function sliceFrom<T>(items: readonly T[], start: number): T[] {
  return items.slice(Math.max(0, Math.min(items.length, start)));
}

/** An envelope's parts plus the size of the paged list they were taken from. */
interface Built {
  readonly payload: SummaryPayload;
  readonly summary: string;
  readonly totals: Totals;
  readonly nextTools: readonly string[];
  /** Items in the scope's paged list, before this page was taken. */
  readonly pagedLength: number;
  /**
   * A bounded list hit its bound, so the payload shows a leading sample rather than everything.
   * `emit` has the `item_cap` reason for exactly this, and reporting it keeps the relation honest.
   */
  readonly itemCapped: boolean;
}

interface Freshness {
  readonly stale: boolean;
  readonly reason: string | null;
}

/** A refusal or a bootstrap answer still has to carry `read_status` and `coverage`. */
function emptyCoverage(): Coverage {
  return {
    complete: false,
    indexed: 0,
    skipped: 0,
    failed: 0,
    unresolved_edges: 0,
    not_indexed: [],
  };
}

/**
 * Freshness has two sources and needs both: a file row written by an older publish is stale even
 * when its bytes are unchanged, and a working tree that differs from the generation's base commit
 * is stale even when every row was written by the served generation (`DESIGN.md` §10.4).
 */
function freshnessOf(olderFiles: number, drift: number): Freshness {
  if (olderFiles > 0) {
    return {
      stale: true,
      reason:
        `${counted(olderFiles, "file")} in scope ` +
        `${olderFiles === 1 ? "was" : "were"} indexed by an older generation`,
    };
  }
  if (drift > 0) {
    return { stale: true, reason: `${counted(drift, "file")} changed since index` };
  }
  return { stale: false, reason: null };
}

/** `1 file` / `3 files`: a stale reason is read by a person, not parsed by a machine. */
function counted(count: number, noun: string): string {
  return `${count} ${noun}${count === 1 ? "" : "s"}`;
}

function readStatusOf(
  generation: string,
  indexedAt: string,
  freshness: Freshness,
  now: Date,
): ReadStatus {
  const status: ReadStatus = {
    generation,
    indexed_at: indexedAt,
    stale: freshness.stale,
    // The check just ran; saying when is what lets an agent decide whether re-asking is worth it.
    stale_checked_at: now.toISOString(),
  };
  if (freshness.reason !== null) {
    status.stale_reason = freshness.reason;
  }
  return status;
}

/**
 * The identity of the call a cursor belongs to. The cursor argument is excluded — it is the page
 * being resumed, not part of the query — and the base and budget are the resolved values, so an
 * omitted base and an explicit one hash the same call.
 */
function argsHashOf(identity: {
  readonly scope: string;
  readonly target: string;
  readonly base: string;
  readonly budget: number;
}): string {
  const canonical = JSON.stringify([
    identity.scope,
    identity.target,
    identity.base,
    identity.budget,
  ]);
  return `sha256:${new Bun.CryptoHasher("sha256").update(canonical).digest("hex")}`;
}

/**
 * The prefixes a scope is about. The reader answers path *prefixes*, so a whole-repository scope
 * queries one prefix per top-level segment: that is the documented coverage of the change payload
 * (e.g. 530 indexed files, not the 3 changed ones), while a directory that no indexed file lives
 * in is outside every prefix — the reader exposes no "every skipped path" query.
 */
function topLevelPrefixes(files: readonly IndexedFileRow[]): string[] {
  const prefixes = new Set<string>();
  for (const file of files) {
    const slash = file.path.indexOf("/");
    prefixes.add(slash === -1 ? file.path : file.path.slice(0, slash));
  }
  return [...prefixes].sort();
}

/** The `test_gaps` sentence: depth is the measurement, so it is what the reason must carry. */
function gapReason(testedDepth: number | null): string {
  return testedDepth === null
    ? `no tested_by edge within ${TEST_GAP_MAX_DEPTH} hops`
    : `nearest test is ${testedDepth} hops away, beyond the ${TEST_GAP_MAX_DEPTH}-hop limit`;
}

/**
 * One symbol's risk, from the same formula as the aggregate (`#shared/domain/risk`): its
 * precomputed in-degree is the radius summary can afford, because the full walk is `impact`'s job.
 */
function symbolRisk(node: NodeRow, metrics: NodeMetrics | null, untested: boolean): number {
  const dependents = metrics?.inDegree ?? 0;
  const changed: ChangedFacts = {
    symbols: 1,
    files: 1,
    entryPoints: metrics?.isEntry === true ? 1 : 0,
    untested: untested ? 1 : 0,
    removed: 0,
  };
  const impacted: ImpactedFacts = {
    nodes: dependents,
    files: dependents > 0 ? 1 : 0,
    maxIndegree: dependents,
    topQn: node.qn,
  };
  return score(changed, impacted).score;
}

/** `reachable_count` is the whole component; the file set is only measured at the first call hop. */
function firstHopFiles(ctx: ToolContext, nodeId: number): number {
  const files = new Set<string>();
  for (const edge of ctx.reader.outbound(nodeId)) {
    files.add(edge.node.path);
  }
  return files.size;
}

function buildChange(
  ctx: ToolContext,
  changes: ChangeSet,
  generation: string,
  base: string,
  start: number,
  relation: TotalRelation,
): Built {
  const changedPaths = [...changes.changed];

  // A changed path with no `file` row is a new (or unparseable) file: it belongs to
  // `changed_files`, which is where an agent looks for it, but it has no symbols to rank.
  const nodes: NodeRow[] = [];
  for (const path of changedPaths) {
    const file = ctx.reader.fileByPath(path);
    if (file === null) {
      continue;
    }
    nodes.push(...ctx.reader.nodesByFile(file.id));
  }

  const metrics = new Map<number, NodeMetrics | null>();
  for (const node of nodes) {
    metrics.set(node.id, ctx.reader.metricsFor(node.id));
  }

  const gaps = ctx.reader.testGaps(nodes.map((node) => node.id));
  const gapIds = new Set(gaps.map((gap) => gap.id));

  const symbols: ChangedSymbol[] = nodes
    .map((node) => ({
      id: node.id,
      qn: node.qn,
      kind: node.kind,
      change: SYMBOL_CHANGE_MODIFIED,
      lines: [node.startLine, node.endLine] as [number, number],
      risk: symbolRisk(node, metrics.get(node.id) ?? null, gapIds.has(node.id)),
    }))
    .sort((a, b) => b.risk - a.risk || a.id - b.id);

  // A tombstone that still had callers is the highest-value review signal, so it leads.
  const removed: RemovedSymbol[] = ctx.reader
    .removedSymbolsFor(generation)
    .map((row) => ({ qn: row.qn, kind: row.kind, had_callers: row.hadCallers }))
    .sort(
      (a, b) =>
        Number(b.had_callers) - Number(a.had_callers) || (a.qn < b.qn ? -1 : a.qn > b.qn ? 1 : 0),
    );

  const dependents = nodes.reduce(
    (total, node) => total + (metrics.get(node.id)?.inDegree ?? 0),
    0,
  );
  const changedFilesWithDependents = new Set(
    nodes.filter((node) => (metrics.get(node.id)?.inDegree ?? 0) > 0).map((node) => node.path),
  );
  let maxIndegree = 0;
  let topQn: string | null = null;
  for (const node of nodes) {
    const indegree = metrics.get(node.id)?.inDegree ?? 0;
    if (indegree > maxIndegree) {
      maxIndegree = indegree;
      topQn = node.qn;
    }
  }

  const changedFacts: ChangedFacts = {
    symbols: nodes.length,
    files: changedPaths.length,
    entryPoints: nodes.filter((node) => metrics.get(node.id)?.isEntry === true).length,
    untested: gaps.length,
    removed: removed.filter((row) => row.had_callers).length,
  };
  const impactedFacts: ImpactedFacts = {
    nodes: dependents,
    files: changedFilesWithDependents.size,
    maxIndegree,
    topQn,
  };
  const risk = score(changedFacts, impactedFacts);

  const testGaps: TestGap[] = gaps.map((gap) => ({
    qn: gap.qn,
    reason: gapReason(gap.testedDepth),
  }));

  const riskiest = symbols[0];
  const firstGap = testGaps[0];
  const suggestedNext: SuggestedNext[] = [];
  if (nodes.length > 0) {
    suggestedNext.push({
      tool: "impact",
      why: `${dependents} direct dependents on the changed symbols, before expanding the radius`,
    });
  }
  if (riskiest !== undefined) {
    suggestedNext.push({
      tool: "trace",
      query: riskiest.qn,
      why: `the riskiest changed symbol (${riskiest.risk}), so read its callers before editing`,
    });
  }
  if (firstGap !== undefined) {
    suggestedNext.push({ tool: "trace", query: firstGap.qn, why: firstGap.reason });
  }
  if (suggestedNext.length === 0) {
    suggestedNext.push({
      tool: "summary",
      query: "repo",
      why: "no working-tree changes; summarize the repository instead",
    });
  }
  suggestedNext.length = Math.min(suggestedNext.length, SUGGESTION_LIMIT);

  // `changed_symbols` is the paged list: it is the ranking, and it is the largest array, so a
  // budget cut takes its tail and the next position addresses exactly what was not delivered.
  const summaryParts = [
    `${changedPaths.length} files changed, ${nodes.length} symbols touched, risk ${risk.score} (${risk.band}).`,
  ];
  summaryParts.push(
    topQn === null
      ? "no dependants were found for the changed symbols."
      : `Top impact: ${topQn} (${dependents} dependents).`,
  );
  summaryParts.push(
    testGaps.length === 0
      ? "every changed symbol has a test within the tested-depth limit."
      : `${testGaps.length} untested changes.`,
  );

  const payload: ChangePayload = {
    base,
    changed_files: changedPaths,
    changed_symbols: sliceFrom(symbols, start),
    risk: { score: risk.score, band: risk.band, drivers: [...risk.drivers] },
    test_gaps: testGaps,
    removed_symbols: removed,
    suggested_next: suggestedNext,
  };

  return {
    payload,
    summary: summaryParts.join(" "),
    totals: {
      total_relation: relation,
      changed_files_total: changedPaths.length,
      changed_symbols_total: nodes.length,
      test_gaps_total: testGaps.length,
      removed_symbols_total: removed.length,
      impacted_total: dependents,
    },
    nextTools: CHANGE_NEXT_TOOLS,
    pagedLength: symbols.length,
    itemCapped: false,
  };
}

function buildRepo(
  ctx: ToolContext,
  generation: GenerationRow,
  rows: PathCoverage,
  start: number,
  relation: TotalRelation,
): Built {
  const languages = ctx.reader
    .censusByLanguage()
    .map((row) => ({ language: row.language, files: row.files }));

  const entryRows = ctx.reader.entryPoints(ENTRY_POINT_LIMIT);
  const hotspots = ctx.reader
    .hotspots(HOTSPOT_LIMIT)
    .map((row) => ({ qn: row.qn, in: row.inDegree, out: row.outDegree }));

  const flows: RepoPayload["flows"][number][] = [];
  for (const row of entryRows) {
    if (row.reachable === null || flows.length >= FLOW_LIMIT) {
      continue;
    }
    flows.push({ name: row.qn, steps: row.reachable, files: firstHopFiles(ctx, row.id) });
  }

  const notIndexed = rows.notIndexed.map((row) => ({ path: row.path, reason: row.reason }));
  const entryPoints = entryRows.map((row) => ({
    qn: row.qn,
    kind: row.kind,
    reachable: row.reachable,
  }));

  const dominant = languages.reduce<RepoPayload["languages"][number] | null>(
    (best, row) => (best === null || row.files > best.files ? row : best),
    null,
  );
  const largestCoupling = hotspots[0];

  // `not_indexed` is the one list this scope does not bound, so it is the paged list: it is what
  // a budget cuts, and a continuation resumes exactly the paths that were not shown.
  const dominance =
    dominant === null
      ? "no language was indexed"
      : `${dominant.language ?? "unattributed"}-dominant`;
  const coupling =
    largestCoupling === undefined
      ? "no coupling hotspots were measured."
      : `Largest coupling: ${largestCoupling.qn} (${largestCoupling.in} in, ${largestCoupling.out} out).`;
  // Entry points and hotspots are statement bounds; a full list means the bound was reached and
  // the repository has more of them than one summary line can name.
  const itemCapped = entryRows.length >= ENTRY_POINT_LIMIT || hotspots.length >= HOTSPOT_LIMIT;
  const entrySentence = `${entryPoints.length} entry points${itemCapped ? " (top of a longer list)" : ""}.`;

  const payload: RepoPayload = {
    size: {
      files: generation.filesIndexed,
      nodes: generation.nodes,
      edges: generation.edges,
    },
    languages,
    entry_points: entryPoints,
    coupling_hotspots: hotspots,
    flows,
    not_indexed: sliceFrom(notIndexed, start),
  };

  return {
    payload,
    summary:
      `${generation.filesIndexed} files, ${generation.nodes} nodes, ${generation.edges} edges. ` +
      `${dominance}. ${entrySentence} ${coupling}`,
    totals: {
      total_relation: relation,
      languages_total: languages.length,
      entry_points_total: entryPoints.length,
      coupling_hotspots_total: hotspots.length,
      flows_total: flows.length,
      not_indexed_total: notIndexed.length,
    },
    nextTools: REPO_NEXT_TOOLS,
    pagedLength: notIndexed.length,
    itemCapped,
  };
}

function buildArea(
  ctx: ToolContext,
  target: string,
  rows: PathCoverage,
  start: number,
  relation: TotalRelation,
): Built {
  // The surface is the declarations a reader of this area sees first; a member is reached through
  // its parent, which is what `expand(what="members")` is for. Source order inside a file, files
  // in path order, so the list is stable for a cursor.
  const nodes: NodeRow[] = [];
  for (const file of rows.indexed) {
    for (const node of ctx.reader.nodesByFile(file.id)) {
      if (node.parentId === null) {
        nodes.push(node);
      }
    }
  }

  const metrics = new Map<number, NodeMetrics | null>();
  for (const node of nodes) {
    metrics.set(node.id, ctx.reader.metricsFor(node.id));
  }
  const gaps = ctx.reader.testGaps(nodes.map((node) => node.id));

  const dependentsTotal = nodes.reduce(
    (total, node) => total + (metrics.get(node.id)?.inDegree ?? 0),
    0,
  );
  let maxIndegree = 0;
  let topQn: string | null = null;
  for (const node of nodes) {
    const indegree = metrics.get(node.id)?.inDegree ?? 0;
    if (indegree > maxIndegree) {
      maxIndegree = indegree;
      topQn = node.qn;
    }
  }
  const filesWithDependents = new Set(
    nodes.filter((node) => (metrics.get(node.id)?.inDegree ?? 0) > 0).map((node) => node.path),
  );

  const risk = score(
    {
      symbols: nodes.length,
      files: rows.indexed.length,
      entryPoints: nodes.filter((node) => metrics.get(node.id)?.isEntry === true).length,
      untested: gaps.length,
      removed: 0,
    },
    {
      nodes: dependentsTotal,
      files: filesWithDependents.size,
      maxIndegree,
      topQn,
    },
  );

  const surface = nodes.map((node) => ({ qn: node.qn, kind: node.kind }));

  const label = target === "" ? "the repository root" : target;
  return {
    payload: {
      target,
      surface: sliceFrom(surface, start),
      dependents_total: dependentsTotal,
      risk: { score: risk.score, band: risk.band, drivers: [...risk.drivers] },
    },
    summary:
      `${label}: ${surface.length} symbols in ${rows.indexed.length} files. ` +
      `${dependentsTotal} dependents. risk ${risk.score} (${risk.band}).`,
    totals: { total_relation: relation, surface_total: surface.length },
    nextTools: AREA_NEXT_TOOLS,
    pagedLength: surface.length,
    itemCapped: false,
  };
}

function refuse(
  code: "cursor_invalid" | "cursor_expired" | "cursor_mismatch",
  hint: string,
  spec: EnvelopeSpec,
) {
  return fail(
    new AppError(code, "the summary cursor cannot be used for this call", { hint }),
    spec,
  );
}

export async function handle(
  ctx: ToolContext,
  args: SummaryArgs,
): Promise<Envelope<SummaryPayload | null>> {
  const scope = args.scope ?? input.scope.default;
  const budget = args.budget ?? input.budget.default;
  const target = normalizeRel(args.target ?? "");

  const meta = ctx.reader.metaFor();
  const generation = ctx.reader.latestGeneration();
  const generationId = meta.generation ?? generation?.id ?? null;
  const indexedAt = meta.indexedAt ?? generation?.publishedAt ?? null;

  if (generation === null || generationId === null) {
    return bootstrapping({
      summary: "the index has no published generation yet",
      read_status: {
        generation: generationId ?? "",
        indexed_at: indexedAt ?? "",
        stale: true,
        stale_reason: "no generation has been published",
      },
      coverage: emptyCoverage(),
      retry_after_ms: BOOTSTRAP_RETRY_MS,
      message: "summary needs a published generation to answer from",
      hint: "run the index tool, then retry",
    });
  }

  const base = args.base ?? generation.baseCommit ?? "";
  const argsHash = argsHashOf({ scope, target, base, budget });

  // Verify before any work: a stale or foreign token must never produce a partial page.
  let start = 0;
  if (args.cursor !== undefined && args.cursor !== "") {
    const acceptance = ctx.cursor.verify(args.cursor, {
      generation: generationId,
      tool: TOOL,
      argsHash,
    });
    if (!acceptance.ok) {
      return refuse(acceptance.code, acceptance.hint, {
        summary: "the request was refused before any page was built",
        read_status: {
          generation: generationId,
          indexed_at: indexedAt ?? generation.publishedAt,
          stale: true,
          stale_reason: "the freshness check did not run because the call was refused",
        },
        coverage: emptyCoverage(),
      });
    }
    start = acceptance.position;
  }

  const changes = await changedSinceFrom(ctx.config.repoRoot, base, ctx.reader);
  if (changes.degraded) {
    ctx.log.warn("summary.change.degraded", { base, reason: changes.degradedReason });
  }
  if (changes.unmatched.length > 0) {
    ctx.log.info("summary.change.unmatched", { count: changes.unmatched.length });
  }

  const prefixes =
    scope === "area" && target !== "" ? [target] : topLevelPrefixes(ctx.reader.allFiles());
  const rows = ctx.reader.rowsForPaths(prefixes);
  const olderFiles = rows.indexed.filter((file) => file.indexedGeneration !== generationId).length;
  const drift = changes.changed.length + changes.removed.length;
  const freshness = freshnessOf(olderFiles, drift);

  // One rows query, memoised for `assemble`: coverage stays the domain module's decision, and the
  // rows it needs are already in hand.
  const source: CoverageSource = {
    rowsForPaths: () => rows,
    unresolvedForFiles: (fileIds) => ctx.reader.unresolvedForFiles(fileIds),
  };
  const assembly = assemble(source, {
    paths: prefixes,
    generation: generationId,
    stale: freshness.stale,
  });

  const built =
    scope === "repo"
      ? buildRepo(ctx, generation, rows, start, assembly.relation)
      : scope === "area"
        ? buildArea(ctx, target, rows, start, assembly.relation)
        : buildChange(ctx, changes, generationId, base, start, assembly.relation);

  const env = ok(built.payload, {
    summary: built.summary,
    read_status: readStatusOf(
      generationId,
      indexedAt ?? generation.publishedAt,
      freshness,
      ctx.now(),
    ),
    coverage: assembly.coverage,
    totals: built.totals,
    next_tools: built.nextTools,
    // A bounded sample is a cut the tool made before the budget did, and it must say so.
    truncated: built.itemCapped,
    truncation_reason: built.itemCapped ? "item_cap" : null,
  });

  const emitted = emit(env, budget, {
    // `emit` reports how many items it dropped, and the paged list is what it cuts: the undelivered
    // remainder of that list starts there, so a continuation cannot repeat a ranked item. A budget
    // too small to carry the supporting lists as well is the one case where the count includes
    // their items; the page then resumes slightly early rather than skipping a ranked item.
    mint: (dropped) =>
      ctx.cursor.sign({
        generation: generationId,
        tool: TOOL,
        argsHash,
        position: Math.min(built.pagedLength, Math.max(start, built.pagedLength - dropped)),
      }),
  });

  // Below the floor this summary needs nothing can be cut to fit, so `emit` returns the envelope
  // untouched: a full repository summary is many times a tiny allowance, and `summary` is the hot
  // path — the refusal names the floor rather than spending the caller's budget on an answer it
  // cannot afford to read.
  const floor = budgetFloorOf(emitted);
  return floor > budget
    ? fail(
        new AppError("budget_too_large", `budget ${budget} is below the floor this summary needs`, {
          hint: `raise the budget to at least ${floor} tokens`,
        }),
        {
          summary: built.summary,
          read_status: readStatusOf(
            generationId,
            indexedAt ?? generation.publishedAt,
            freshness,
            ctx.now(),
          ),
          // The refusal is about the budget, so it carries no payload and no path list: the counts
          // stay exact (`skipped` says how many were not indexed) while the 80 entries that would
          // make this envelope several kilobytes are what the caller was told to make room for.
          coverage: { ...assembly.coverage, not_indexed: [] },
          totals: built.totals,
          next_tools: built.nextTools,
          truncated: false,
          truncation_reason: null,
          next: null,
        },
      )
    : emitted;
}
