/**
 * `status`: is the graph current, and what does it cover (TOOLS-SPEC.md §2)?
 *
 * The trust question, answered from the reader alone. Three facts make the answer usable and each
 * is reported even when it is bad news: `coverage.complete` plus `coverage.unresolved_edges` gate
 * every later absence claim; the language census reports a language that parsed nothing as
 * `parsed: 0` with a note instead of dropping it; and a requested path that is in neither `file`
 * nor `not_indexed` comes back as `not_indexed_unknown` rather than as a zero.
 *
 * Two limits are deliberate and visible in the payload:
 *
 * - `read_status.stale` is derived from what the graph can prove without touching the filesystem: a
 *   `file` row written by an earlier generation than the one being served. Working-tree drift and a
 *   `project_root` mismatch need `#shared/domain/freshness.ts` (phase 3, TASK-030), which is where
 *   this field moves; until then the verdict is graph-side only and `stale_checked_at` says when it
 *   ran, so the claim is dated rather than absolute.
 * - `graph.db_path` and `graph.size_bytes`: `GraphReader` deliberately exposes no database path, so
 *   the path is derived from the injected `ctx.config.dataDir` — never from the environment — and
 *   the size is a `stat` of that file.
 *
 * The arithmetic behind `coverage` is the one place this handler has to be careful.
 * `censusByLanguage` folds `not_indexed` into the census (a skipped path is one more file of its
 * language that failed), so `sum(languages[].files) === file rows + not_indexed rows`. The counts
 * therefore come from `allFiles()` for the indexed side and from that identity for the skipped
 * side, and `sum(languages[].failed) === failed parses + skipped paths`, which the tests assert.
 */
import { createHash } from "node:crypto";
import { statSync } from "node:fs";
import { join } from "node:path";
import { emit } from "#shared/budget";
import {
  bootstrapping,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  ok,
  type ReadStatus,
  relationFor,
} from "#shared/envelope";
import { isUnderPrefix, normalizeRel } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type {
  GenerationRow,
  GraphMeta,
  IndexedFileRow,
  LanguageCensus,
  PathCoverage,
} from "#shared/store/read/reader";
import { manifest, STATUS_DEFAULT_TOKENS, STATUS_RETRY_AFTER_MS } from "./manifest";

/** The reason a requested path carries when neither half of the coverage answer mentions it. */
const REASON_NOT_INDEXED_UNKNOWN = "not_indexed_unknown";

/** The `note` a language gets when it contributed no parse at all (TOOLS-SPEC.md §2). */
const NOTE_GRAMMAR_UNAVAILABLE = "grammar_unavailable";

/**
 * One language of the census. `language` stays `null` for a skipped path whose grammar the indexer
 * could not name — the reader keeps that bucket rather than inventing a name for it — and `note`
 * is absent unless there is something to explain.
 */
export interface StatusLanguage {
  readonly language: string | null;
  readonly files: number;
  readonly parsed: number;
  readonly failed: number;
  readonly note?: string;
}

/**
 * One path in the answer. `indexed: false` always carries a `reason`, so "not indexed" is never a
 * bare `false`. `symbols` is present only when the request named the file: it costs one
 * `nodesByFile` query, and a directory request can match hundreds of files, which would turn the
 * cheap tool into the most expensive one.
 */
export interface StatusPath {
  readonly path: string;
  readonly indexed: boolean;
  readonly symbols?: number;
  readonly sha256?: string;
  readonly reason?: string;
}

/** The payload keys TOOLS-SPEC.md §2 documents, in that order. */
export interface StatusPayload {
  readonly project: {
    readonly name: string | null;
    readonly root: string | null;
    readonly commit: string | null;
    readonly db_path: string;
  };
  readonly graph: {
    readonly nodes: number;
    readonly edges: number;
    readonly size_bytes: number;
    readonly db_path: string;
  };
  readonly languages: readonly StatusLanguage[];
  readonly paths: readonly StatusPath[];
  readonly health: {
    readonly indexer: string | null;
    readonly last_publish_ms: number;
    readonly consecutive_failures: number;
  };
}

/** The arguments the manifest declares; every one of them is optional. */
export interface StatusArgs {
  readonly paths?: readonly string[] | undefined;
  readonly budget?: number | undefined;
}

/**
 * `null` is the payload of the two statuses that carry none: `bootstrapping` (nothing is published
 * yet) and `error` (the budget could not be applied). Both still carry the nine envelope fields.
 */
export type StatusEnvelope = Envelope<StatusPayload | null>;

/**
 * Answers "is the graph current and what does it cover" without opening a database, reading the
 * environment or writing to stdout. A call that names no paths asks about the whole repository.
 */
export function handle(ctx: ToolContext, args: StatusArgs = {}): StatusEnvelope {
  const requested = requestedPaths(args.paths);
  const budget = budgetOf(args.budget);

  const meta = ctx.reader.metaFor();
  const generation = meta.generation;
  // A reader with no published generation has nothing to report on: `bootstrapping` is the honest
  // answer, and it is cheaper for the agent than an `ok` envelope with an invented generation id.
  const generationRow = generation === null ? null : ctx.reader.latestGeneration();
  if (generation === null || generationRow === null) {
    return unpublished(ctx, meta);
  }

  const files = ctx.reader.allFiles();
  const rows = ctx.reader.rowsForPaths(requested);
  const census = ctx.reader.censusByLanguage();
  const unresolved = ctx.reader.unresolvedForFiles(files.map((file) => file.id));

  const failed = files.filter((file) => file.parseStatus === "failed").length;
  const censusFiles = census.reduce((total, row) => total + row.files, 0);
  // `max` guards the subtraction rather than assuming the two tables agree: a negative skip count
  // would be a writer bug, and reporting a negative number to the agent would be worse than 0.
  const skipped = Math.max(0, censusFiles - files.length);

  const coverage: Coverage = {
    complete: files.length > 0 && skipped === 0 && failed === 0,
    indexed: files.length,
    skipped,
    failed,
    unresolved_edges: unresolved.length,
    // Scope-limited by construction: `not_indexed` lists the skipped paths under the requested
    // paths, which is why the repo-wide `skipped` count can be larger than this list.
    not_indexed: rows.notIndexed.map((row) => ({ path: row.path, reason: row.reason })),
  };

  const staleFiles = files.filter((file) => file.indexedGeneration !== generation).length;
  const readStatus = readStatusOf(ctx, meta, generation, generationRow, staleFiles, files.length);

  const report = pathReport(ctx, requested, rows);
  const dbPath = join(ctx.config.dataDir, "graph.db");

  const payload: StatusPayload = {
    project: {
      name: meta.projectName,
      root: meta.projectRoot,
      commit: generationRow.baseCommit,
      db_path: dbPath,
    },
    graph: {
      nodes: generationRow.nodes,
      edges: generationRow.edges,
      size_bytes: dbSizeBytes(ctx, dbPath),
      db_path: dbPath,
    },
    languages: languageEntries(census),
    paths: report.entries,
    health: {
      indexer: meta.indexerState,
      last_publish_ms: generationRow.durationMs,
      consecutive_failures: meta.consecutiveFailures,
    },
  };

  const spec: EnvelopeSpec = {
    summary: summaryOf(coverage, generation, staleFiles, requested.length, report.indexedPaths),
    read_status: readStatus,
    coverage,
    totals: {
      total_relation: relationFor(readStatus, coverage),
      paths_total: report.entries.length,
    },
    // `expand` is the tool that consumes a `next` cursor, and it is the only useful continuation
    // this tool has: everything else `status` reports is already in the payload.
    next_tools: ["expand"],
  };

  const argsHash = argsHashOf(requested, budget);
  try {
    return emit(ok<StatusPayload>(payload, spec), budget, {
      mint: (position) => ctx.cursor.sign({ generation, tool: manifest.name, argsHash, position }),
    });
  } catch (error) {
    // `emit` refuses a budget above its ceiling with an `AppError`, which is a client mistake and
    // part of the closed error union: it comes back as an envelope rather than as a thrown handler.
    // Anything else caught here is a genuine bug, and it surfaces as `internal` through `fail` —
    // still the contract's shape, still with no partial payload.
    return fail(error, { ...spec, summary: "the requested budget could not be applied" });
  }
}

/** `bootstrapping` with the fields an unpublished database has: no generation and no coverage. */
function unpublished(ctx: ToolContext, meta: GraphMeta): StatusEnvelope {
  return bootstrapping({
    summary: "No generation has been published yet, so there is no graph to report on.",
    read_status: {
      // The empty id is the truthful value: a fabricated one would give a cursor something to
      // bind to that no publish will ever match.
      generation: meta.generation ?? "",
      indexed_at: meta.indexedAt ?? "",
      stale: true,
      stale_reason: "no published generation is readable",
      stale_checked_at: ctx.now().toISOString(),
    },
    coverage: {
      complete: false,
      indexed: 0,
      skipped: 0,
      failed: 0,
      unresolved_edges: 0,
      not_indexed: [],
    },
    retry_after_ms: STATUS_RETRY_AFTER_MS,
    message: "the index has not published a generation yet",
    hint: "retry after retry_after_ms; if the indexer is not running, start it",
    next_tools: ["status"],
  });
}

/**
 * Freshness of the answer, stamped with the moment it was checked. The verdict is graph-side (see
 * the module comment), so it is reported as a dated claim rather than as an absolute one.
 */
function readStatusOf(
  ctx: ToolContext,
  meta: GraphMeta,
  generation: string,
  generationRow: GenerationRow,
  staleFiles: number,
  indexedFiles: number,
): ReadStatus {
  const checkedAt = ctx.now().toISOString();
  const indexedAt = meta.indexedAt ?? generationRow.publishedAt;
  if (staleFiles === 0) {
    return {
      generation,
      indexed_at: indexedAt,
      stale: false,
      stale_checked_at: checkedAt,
    };
  }
  return {
    generation,
    indexed_at: indexedAt,
    stale: true,
    stale_reason: `${staleFiles} of ${indexedFiles} indexed files were written by an earlier generation`,
    stale_checked_at: checkedAt,
  };
}

/**
 * The census with the one note the spec asks for. A language that produced no parse at all is
 * exactly the case CRG's census hid by listing `xslt` and `liquid` as supported, so `parsed: 0` and
 * a note together say "this language is unavailable to the build" instead of implying zero files
 * of that language exist. The unattributed bucket (`language: null`) gets no note: the note names a
 * language, and that bucket has no name to attach it to.
 */
function languageEntries(census: readonly LanguageCensus[]): StatusLanguage[] {
  return census.map((row) => {
    const entry: StatusLanguage = {
      language: row.language,
      files: row.files,
      parsed: row.parsed,
      failed: row.failed,
    };
    if (row.language === null || row.parsed > 0 || row.failed === 0) {
      return entry;
    }
    return { ...entry, note: NOTE_GRAMMAR_UNAVAILABLE };
  });
}

interface PathReport {
  /** One entry per row the requested paths matched, best-first within each request. */
  readonly entries: StatusPath[];
  /** How many of the requested paths matched at least one indexed file. */
  readonly indexedPaths: number;
}

/**
 * Turns the requested paths into payload entries.
 *
 * A requested path is answered at the granularity it asks for: naming a file returns that file with
 * its symbol count and digest, while naming a directory returns the files and skipped paths under
 * it, each with its own reason. That expansion is what makes "is `src/auth/` indexed, and did it
 * parse cleanly?" one call, which is the reason `paths` exists at all (TOOLS-SPEC.md §2).
 *
 * Overlapping requests (`["src", "src/a.ts"]`) emit each path once: the first request that matches
 * a row owns it, so a payload entry is never duplicated by the shape of the request.
 */
function pathReport(
  ctx: ToolContext,
  requested: readonly string[],
  rows: PathCoverage,
): PathReport {
  const entries: StatusPath[] = [];
  const seen = new Set<string>();
  const add = (entry: StatusPath): void => {
    if (seen.has(entry.path)) {
      return;
    }
    seen.add(entry.path);
    entries.push(entry);
  };

  let indexedPaths = 0;
  for (const path of requested) {
    const exact = rows.indexed.find((file) => file.path === path);
    if (exact !== undefined) {
      indexedPaths += 1;
      add(entryForFile(exact, ctx.reader.nodesByFile(exact.id).length));
      continue;
    }

    const under = rows.indexed.filter((file) => isUnderPrefix(file.path, path));
    const skipped = rows.notIndexed.filter((row) => isUnderPrefix(row.path, path));
    if (under.length > 0) {
      indexedPaths += 1;
    }
    for (const file of under) {
      add(entryForFile(file));
    }
    for (const row of skipped) {
      add({ path: row.path, indexed: false, reason: row.reason });
    }
    if (under.length === 0 && skipped.length === 0) {
      // Neither `file` nor `not_indexed` knows this path, so the indexer never looked at it. That is
      // a different fact from "looked at and skipped", and it is the one that must not read as zero.
      add({ path, indexed: false, reason: REASON_NOT_INDEXED_UNKNOWN });
    }
  }

  return { entries, indexedPaths };
}

/** One indexed file's entry; `symbols` is passed only when the request justified the query. */
function entryForFile(file: IndexedFileRow, symbols?: number): StatusPath {
  const entry: StatusPath = { path: file.path, indexed: true, sha256: file.sha256 };
  return symbols === undefined ? entry : { ...entry, symbols };
}

/**
 * The graph file's size, or `0` when it cannot be read. `0` is not a fallback for "unknown size" —
 * it is reported next to `nodes`/`edges`, so a zero with a non-zero node count reads as "the file
 * was not statable" and the reason goes to the log instead of into the payload.
 */
function dbSizeBytes(ctx: ToolContext, dbPath: string): number {
  try {
    return statSync(dbPath).size;
  } catch (error) {
    ctx.log.warn("status.db_size_unavailable", {
      path: dbPath,
      reason: error instanceof Error ? error.message : String(error),
    });
    return 0;
  }
}

/**
 * The requested paths in the one form the prefix queries and the payload agree on: repo-relative,
 * forward slashes, no duplicates. The input is agent-supplied, so a non-string element is dropped
 * instead of reaching `normalizeRel`, and the order is preserved because it is the payload order.
 */
function requestedPaths(raw: readonly string[] | undefined): string[] {
  if (!Array.isArray(raw)) {
    return [];
  }
  const unique: string[] = [];
  const seen = new Set<string>();
  for (const candidate of raw) {
    if (typeof candidate !== "string") {
      continue;
    }
    const path = normalizeRel(candidate);
    if (path === "" || seen.has(path)) {
      continue;
    }
    seen.add(path);
    unique.push(path);
  }
  return unique;
}

/** A budget is a positive integer; anything else is the manifest's default (TOOLS-SPEC.md §2). */
function budgetOf(raw: number | undefined): number {
  return typeof raw === "number" && Number.isInteger(raw) && raw > 0 ? raw : STATUS_DEFAULT_TOKENS;
}

/**
 * The arguments a cursor is pinned to (`#shared/cursor.ts`). Canonical JSON, so the same call signs
 * the same hash twice, and `status` is in the payload because `expand` verifies this token against
 * the tool that minted it.
 */
function argsHashOf(requested: readonly string[], budget: number): string {
  const canonical = JSON.stringify({ budget, paths: requested });
  return `sha256:${createHash("sha256").update(canonical).digest("hex")}`;
}

function summaryOf(
  coverage: Coverage,
  generation: string,
  staleFiles: number,
  requestedCount: number,
  indexedPaths: number,
): string {
  const counts = [
    `${coverage.indexed} files indexed`,
    `generation ${generation}`,
    ...(coverage.skipped > 0 ? [`${coverage.skipped} skipped`] : []),
    ...(coverage.failed > 0 ? [`${coverage.failed} failed to parse`] : []),
    ...(staleFiles > 0 ? [`${staleFiles} written by an earlier generation`] : []),
  ].join(", ");
  const scope =
    requestedCount === 0
      ? "Whole repository."
      : `${indexedPaths} of ${requestedCount} requested paths indexed.`;
  return `${counts}. ${scope}`;
}
