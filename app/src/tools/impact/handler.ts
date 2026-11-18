/**
 * `impact`: what a change set can reach, how far, through what, and whether it is tested
 * (`TOOLS-SPEC.md` §6).
 *
 * The handler computes nothing a `#shared` module already owns. The change set is
 * `#shared/domain/diff` (`changedSinceFrom`, which takes the injected reader — a handler has no
 * connection); the blast radius is one inbound walk through `#shared/domain/neighbors`; the risk
 * number is `#shared/domain/risk`; coverage is `#shared/domain/coverage`; and sizing is
 * `#shared/budget`'s `emit`. This file only decides *which* of those to call, in *what order*, and
 * how their facts become the documented payload.
 *
 * Four decisions worth naming, because none of them is a formatting choice:
 *
 * - **Inbound only.** A changed symbol is reached *by* its dependents, so `direction: "in"` is the
 *   direction that answers "what breaks". Outbound reach is `trace`'s question.
 * - **`via` is the edge kind that reached the node first.** Deduping by node id at the lowest hop,
 *   then `via`/`tier` from the group that found it, is what makes "impacted" mean something: a
 *   bare id list would say a node is affected without saying whether that is a call, an import or
 *   an inheritance.
 * - **A changed path the graph cannot explain never disappears.** It lands in `unmatched_files`
 *   with a reason. The reason vocabulary is `diff.ts`'s `UnmatchedReason` extended by two values
 *   the diff cannot produce: `not_indexed` (the caller asked about a path with no `file` row at
 *   all) and `no_nodes_in_graph` (the row exists but the file defines no symbols, so it seeds
 *   nothing).
 * - **Freshness is never assumed.** With an explicit `paths` argument the working tree is never
 *   compared to the index, so `read_status.stale` is `true` and `total_relation` can only be
 *   `gte`: the tool refuses to claim its emptiness is complete.
 *
 * Pagination is the flattened `impacted` list and nothing else: `cursor.position` is an index into
 * that list, and the resumed page continues from it, so page N+1 overlaps page N by nothing.
 */
import { createHash } from "node:crypto";
import { emit, MAX_BUDGET_TOKENS } from "#shared/budget";
import { assemble } from "#shared/domain/coverage";
import { changedSinceFrom, type UnmatchedReason } from "#shared/domain/diff";
import { clampDepth, expand } from "#shared/domain/neighbors";
import { type RiskBand, score } from "#shared/domain/risk";
import {
  bootstrapping,
  type Coverage,
  type Envelope,
  fail,
  ok,
  type ReadStatus,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import { normalizeRel } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import { input, manifest } from "./manifest";

const TOOL = manifest.name;

/** Defaults live in the manifest the model reads, so the two can never disagree. */
const DEFAULT_DEPTH = input.depth.default;
const DEFAULT_BUDGET = input.budget.default;

/** The same routing hint `TOOLS-SPEC.md` §6 publishes: widen one node, or open one of these. */
const NEXT_TOOLS = ["trace", "expand"] as const;

/** 16 bytes of SHA-256 is ample for "is this the same call"; the full digest only lengthens `next`. */
const ARGS_HASH_CHARS = 32;

/** Coverage of a request that answered before any scope was read: nothing is claimed by it. */
const NO_COVERAGE: Coverage = {
  complete: false,
  indexed: 0,
  skipped: 0,
  failed: 0,
  unresolved_edges: 0,
  not_indexed: [],
};

/**
 * `UnmatchedReason` plus the two cases only an explicit change set can produce. Each value is one
 * distinct answer to "why did this path contribute nothing?", which is the whole point of not
 * dropping it:
 * - `no_file_row_and_unparseable_language` / `unreadable_file` — reported by the diff, reused
 *   verbatim so `impact` and `summary` word the same fact the same way;
 * - `not_indexed` — an explicit `paths` entry with no `file` row and no `not_indexed` reason of its
 *   own: never looked at, rather than looked at and refused;
 * - `no_nodes_in_graph` — indexed, but the file defines no symbols (empty, or the publish wrote no
 *   nodes for it), so nothing can be reached from it.
 */
export type ImpactUnmatchedReason = UnmatchedReason | "not_indexed" | "no_nodes_in_graph";

/** One changed path the graph can explain. `symbols` is a count, per the documented payload. */
export interface ChangedFileRow {
  readonly path: string;
  readonly symbols: number;
  readonly entry_point: boolean;
}

/** One dependant of the change set, with how it was reached and whether a test covers it. */
export interface ImpactedNodeRow {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly hop: number;
  /** The edge kind that first reached this node: `calls`, `imports`, `inherits`, … */
  readonly via: string;
  /** `extracted` | `inferred` | … — extracted facts outrank inferred ones in reviews. */
  readonly tier: string;
  /** `tested_depth !== null`: a test is reachable, at whatever depth. */
  readonly tested: boolean;
  /** Present only when true, as the documented payload shows it. */
  readonly entry_point?: true;
}

export interface UnmatchedFileRow {
  readonly path: string;
  readonly reason: ImpactUnmatchedReason;
}

/** The payload `TOOLS-SPEC.md` §6 documents, and no other key. */
export interface ImpactPayload {
  readonly changed: readonly ChangedFileRow[];
  readonly impacted: readonly ImpactedNodeRow[];
  readonly unmatched_files: readonly UnmatchedFileRow[];
  readonly risk: {
    readonly score: number;
    readonly band: RiskBand;
    readonly drivers: readonly string[];
  };
}

/**
 * `cursor` is excluded from the hash on purpose: a page request must verify against the same call
 * identity as the page it continues, or every resumed call would read as `cursor_mismatch`.
 */
export interface ImpactArgs {
  readonly paths?: readonly string[];
  readonly base?: string;
  readonly depth?: number;
  readonly budget?: number;
  readonly cursor?: string;
}

/** What the summary sentence needs; a fact object keeps the wording out of the control flow. */
interface SummaryFacts {
  readonly explicit: boolean;
  readonly requested: number;
  readonly baseLabel: string;
  readonly depth: number;
  readonly changed: number;
  readonly impacted: number;
  readonly impactedFiles: number;
  readonly entryPoints: number;
  readonly tested: number;
  readonly unmatched: number;
}

/** One visited node before its metrics are attached; `file` is for counting, not for the payload. */
interface Candidate {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly hop: number;
  readonly via: string;
  readonly tier: string;
  readonly file: string;
}

function finiteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

/**
 * The caller's `paths`, normalised, deduplicated and sorted. The runtime type check is not
 * redundant: arguments arrive as JSON, so a hand-written client can send anything, and a tool that
 * threw on `paths: [null]` would be a tool an agent cannot recover from.
 */
function requestedPaths(paths: readonly string[] | undefined): string[] {
  if (!Array.isArray(paths)) {
    return [];
  }
  const kept = new Set<string>();
  for (const raw of paths) {
    if (typeof raw !== "string") {
      continue;
    }
    const rel = normalizeRel(raw);
    if (rel !== "") {
      kept.add(rel);
    }
  }
  return [...kept].sort();
}

/** The call identity a cursor binds to: everything except the cursor, in a fixed key order. */
function argsHashOf(call: {
  readonly paths: readonly string[];
  readonly base: string;
  readonly depth: number;
  readonly budget: number;
}): string {
  return createHash("sha256").update(JSON.stringify(call)).digest("hex").slice(0, ARGS_HASH_CHARS);
}

/**
 * `stale_reason` is absent exactly when the generation is fresh, matching the envelope's own rule
 * that an optional field is omitted rather than sent as `undefined`.
 */
function readStatusOf(
  generation: string,
  indexedAt: string,
  checkedAt: string,
  staleness: string | null,
): ReadStatus {
  return {
    generation,
    indexed_at: indexedAt,
    stale: staleness !== null,
    ...(staleness === null ? {} : { stale_reason: staleness }),
    stale_checked_at: checkedAt,
  };
}

/** Why the tree moved since the last publish, or `null` when it did not. */
function stalenessOf(changed: number, removed: number): string | null {
  if (changed === 0 && removed === 0) {
    return null;
  }
  const parts: string[] = [];
  if (changed > 0) {
    parts.push(`${changed} file${changed === 1 ? "" : "s"} changed`);
  }
  if (removed > 0) {
    parts.push(`${removed} path${removed === 1 ? "" : "s"} removed`);
  }
  return `${parts.join(" and ")} since index`;
}

/** `showing N of M` when a page was cut, and the window when it was resumed. */
function pageSummary(
  summary: string,
  start: number,
  shown: number,
  total: number,
  truncated: boolean,
): string {
  if (truncated) {
    return `${summary} Showing ${shown} of ${total}.`;
  }
  if (start === 0) {
    return summary;
  }
  if (shown === 0) {
    return `${summary} No impacted nodes remain after position ${start} of ${total}.`;
  }
  return `${summary} Showing ${start + 1}-${start + shown} of ${total}.`;
}

function summaryOf(facts: SummaryFacts): string {
  if (facts.changed === 0) {
    if (facts.unmatched === 0) {
      return `no changed files against ${facts.baseLabel}: nothing to impact`;
    }
    return (
      `no changed files in the graph; ${facts.unmatched} changed path` +
      `${facts.unmatched === 1 ? "" : "s"} could not be matched: nothing to impact`
    );
  }
  const scope = facts.explicit
    ? `${facts.changed} of ${facts.requested} requested paths`
    : `${facts.changed} changed files`;
  if (facts.impacted === 0) {
    return `${scope} reach no dependents within ${facts.depth} hops`;
  }
  return (
    `${scope} reach ${facts.impacted} nodes in ${facts.impactedFiles} files. ` +
    `${facts.entryPoints} entry points affected. ` +
    `${facts.tested} of ${facts.impacted} covered by tests.`
  );
}

/**
 * Runs one `impact` query. Never throws for caller input: a refused cursor, an oversized budget
 * and an unpublished database each return an envelope, because a handler that throws leaves the
 * agent with a protocol error instead of a hint it can act on.
 */
export async function handle(
  ctx: ToolContext,
  args: ImpactArgs,
): Promise<Envelope<ImpactPayload | null>> {
  const paths = requestedPaths(args.paths);
  const depth = clampDepth(finiteNumber(args.depth) ?? DEFAULT_DEPTH);
  const budget = Math.max(1, Math.trunc(finiteNumber(args.budget) ?? DEFAULT_BUDGET));

  // Read status is needed by every branch below, including the refusals, so it is read once here.
  const meta = ctx.reader.metaFor();
  const latest = ctx.reader.latestGeneration();
  const generation = meta.generation ?? latest?.id ?? "";
  const indexedAt = latest?.publishedAt ?? meta.indexedAt ?? "";
  const checkedAt = ctx.now().toISOString();

  if (generation === "") {
    // No generation means no cursor can bind and no absence can be claimed: say so, do not guess.
    return bootstrapping({
      summary: "the graph has no published generation yet; impact needs an index",
      read_status: readStatusOf("", indexedAt, checkedAt, "no generation has been published"),
      coverage: NO_COVERAGE,
      retry_after_ms: 1000,
    });
  }

  const base =
    typeof args.base === "string" && args.base !== "" ? args.base : (latest?.baseCommit ?? "");
  const argsHash = argsHashOf({ paths, base, depth, budget });
  const cursorExpected = { generation, tool: TOOL, argsHash };

  // Verify before any work: a refused cursor must not cost a diff and a graph walk, and must never
  // be answered with a partial page that looks like a real one (DATABASE.md §8).
  let start = 0;
  if (typeof args.cursor === "string" && args.cursor !== "") {
    const verdict = ctx.cursor.verify(args.cursor, cursorExpected);
    if (!verdict.ok) {
      return fail(
        new AppError(verdict.code, `the cursor was refused: ${verdict.code}`, {
          hint: verdict.hint,
        }),
        {
          summary: `the cursor could not be resumed (${verdict.code}); nothing was computed`,
          read_status: readStatusOf(
            generation,
            indexedAt,
            checkedAt,
            "the cursor was refused before the change set was read",
          ),
          coverage: NO_COVERAGE,
          totals: { impacted_total: 0, total_relation: "gte" },
        },
      );
    }
    start = verdict.position;
  }

  const ceiling = Math.min(ctx.config.budgets.maxTokens, MAX_BUDGET_TOKENS);
  if (budget > ceiling) {
    return fail(
      new AppError("budget_too_large", `budget ${budget} exceeds the ${ceiling} token ceiling`, {
        hint: `request at most ${ceiling} tokens`,
      }),
      {
        summary: `budget ${budget} exceeds the ${ceiling} token ceiling`,
        read_status: readStatusOf(generation, indexedAt, checkedAt, "the call was refused"),
        coverage: NO_COVERAGE,
        totals: { impacted_total: 0, total_relation: "gte" },
      },
    );
  }

  // The change set is the diff when no explicit paths were given; `changedSinceFrom` reads the
  // reader's own `allFiles()` digests, so this stays a handler with no connection (diff.ts).
  const changeSet =
    paths.length === 0 ? await changedSinceFrom(ctx.config.repoRoot, base, ctx.reader, {}) : null;
  const changedPaths = changeSet === null ? paths : [...changeSet.changed];
  const removed = changeSet === null ? 0 : changeSet.removed.length;
  const staleness =
    changeSet === null
      ? "the change set was given explicitly, so the working tree was not compared to the index"
      : stalenessOf(changedPaths.length, removed);

  const unmatched = new Map<string, UnmatchedFileRow>();
  if (changeSet !== null) {
    for (const row of changeSet.unmatched) {
      unmatched.set(row.path, { path: row.path, reason: row.reason });
    }
  }

  const changed: ChangedFileRow[] = [];
  const seeds: number[] = [];
  let changedSymbols = 0;
  let changedEntryPoints = 0;
  let changedUntested = 0;

  for (const path of changedPaths) {
    const file = ctx.reader.fileByPath(path);
    if (file === null) {
      // Not in the graph at all: the caller still needs to know its change was not analysed.
      if (!unmatched.has(path)) {
        unmatched.set(path, { path, reason: "not_indexed" });
      }
      continue;
    }
    const nodes = ctx.reader.nodesByFile(file.id);
    if (nodes.length === 0) {
      if (!unmatched.has(path)) {
        unmatched.set(path, { path, reason: "no_nodes_in_graph" });
      }
      continue;
    }

    let entryPoint = false;
    for (const node of nodes) {
      seeds.push(node.id);
      const metrics = ctx.reader.metricsFor(node.id);
      if (metrics?.isEntry === true) {
        entryPoint = true;
        changedEntryPoints += 1;
      }
      // A node the publish never measured reads as untested: the conservative direction, because
      // "probably covered" is the claim that gets a regression merged.
      if ((metrics?.testedDepth ?? null) === null) {
        changedUntested += 1;
      }
    }
    changedSymbols += nodes.length;
    changed.push({ path, symbols: nodes.length, entry_point: entryPoint });
  }

  // One inbound walk from every changed node. `expand` already reports a node once per direction at
  // the hop that first reached it; the dedup below keeps the lowest hop and stays explicable when
  // a future walk returns the same node through two groups.
  const grouped = expand(ctx.reader, seeds, { direction: "in", depth });
  const candidates = new Map<number, Candidate>();
  for (const group of grouped.in) {
    for (const item of group.items) {
      const seen = candidates.get(item.id);
      if (seen !== undefined && seen.hop <= item.hop) {
        continue;
      }
      candidates.set(item.id, {
        id: item.id,
        qn: item.qn,
        kind: item.kind,
        hop: item.hop,
        via: group.edge,
        tier: group.tier,
        file: item.file,
      });
    }
  }
  // Ordered by hop then id, so a page boundary is a property of the graph rather than of the order
  // the frontier happened to be built in — which is what makes a cursor page stable.
  const ordered = [...candidates.values()].sort((a, b) =>
    a.hop === b.hop ? a.id - b.id : a.hop - b.hop,
  );

  const impacted: ImpactedNodeRow[] = [];
  const impactedFiles = new Set<string>();
  let testedCount = 0;
  let impactedEntryPoints = 0;
  let maxIndegree = 0;
  let topQn: string | null = null;
  for (const candidate of ordered) {
    const metrics = ctx.reader.metricsFor(candidate.id);
    const tested = (metrics?.testedDepth ?? null) !== null;
    const plain: ImpactedNodeRow = {
      id: candidate.id,
      qn: candidate.qn,
      kind: candidate.kind,
      hop: candidate.hop,
      via: candidate.via,
      tier: candidate.tier,
      tested,
    };
    const row: ImpactedNodeRow =
      metrics?.isEntry === true ? { ...plain, entry_point: true } : plain;
    if (metrics?.isEntry === true) {
      impactedEntryPoints += 1;
    }
    if (tested) {
      testedCount += 1;
    }
    impacted.push(row);
    impactedFiles.add(candidate.file);
    const inDegree = metrics?.inDegree ?? 0;
    if (inDegree > maxIndegree) {
      maxIndegree = inDegree;
      topQn = candidate.qn;
    }
  }

  const risk = score(
    {
      symbols: changedSymbols,
      files: changed.length,
      entryPoints: changedEntryPoints,
      untested: changedUntested,
      removed,
    },
    { nodes: impacted.length, files: impactedFiles.size, maxIndegree, topQn },
  );

  // The change scope's own coverage: a changed file with an unresolved call site is why the total
  // can only be a lower bound, and `assemble` is the module that decides that (coverage.ts).
  const assembly = assemble(ctx.reader, {
    paths: changedPaths,
    generation,
    stale: staleness !== null,
  });

  const unmatchedRows = [...unmatched.values()].sort((a, b) =>
    a.path < b.path ? -1 : a.path > b.path ? 1 : 0,
  );
  const page = impacted.slice(start);
  const payload: ImpactPayload = {
    changed,
    impacted: page,
    unmatched_files: unmatchedRows,
    risk: { score: risk.score, band: risk.band, drivers: [...risk.drivers] },
  };

  const baseLabel = base === "" ? "HEAD" : base;
  const summary = summaryOf({
    explicit: paths.length > 0,
    requested: paths.length,
    baseLabel,
    depth,
    changed: changed.length,
    impacted: impacted.length,
    impactedFiles: impactedFiles.size,
    entryPoints: impactedEntryPoints,
    tested: testedCount,
    unmatched: unmatchedRows.length,
  });

  const sized = emit(
    ok(payload, {
      summary,
      read_status: readStatusOf(generation, indexedAt, checkedAt, staleness),
      coverage: assembly.coverage,
      totals: { impacted_total: impacted.length, total_relation: assembly.relation },
      next_tools: NEXT_TOOLS,
    }),
    budget,
    {
      mint: (dropped) =>
        ctx.cursor.sign({
          generation,
          tool: TOOL,
          argsHash,
          position: start + Math.max(0, page.length - dropped),
        }),
    },
  );

  if (!sized.truncated) {
    return { ...sized, summary: pageSummary(summary, start, page.length, impacted.length, false) };
  }

  // `emit` dropped items and minted a cursor from its own drop count, which counts every list it
  // shrank. The continuation that is true for *this* list is the offset of the row after the last
  // one actually shown, so it is restated from the payload `emit` returned. `next` and `summary`
  // are the two contract fields `emit` defines as "derived from the truncation"; both are rewritten
  // to the measured outcome, and nothing about the size decision is re-done here.
  const shown = sized.payload.impacted.length;
  return {
    ...sized,
    summary: pageSummary(summary, start, shown, impacted.length, true),
    next: ctx.cursor.sign({ generation, tool: TOOL, argsHash, position: start + shown }),
  };
}
