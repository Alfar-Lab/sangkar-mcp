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
 * - `read_status` is the verdict of `#shared/domain/freshness.ts`, not a reading of `meta`: the
 *   graph-side half of it survives (a `file` row written by an earlier generation than the one
 *   being served) and the filesystem-side half is added — a `project_root` mismatch, a base commit
 *   that is no longer `HEAD`, a watcher that has been busy with nothing to publish for ten minutes,
 *   and a digest that no longer matches the bytes on disk. `stale_checked_at` is that module's
 *   `checked_at`, so the claim is dated rather than absolute. This handler only decides how to word
 *   the reason it is handed; it does not decide whether the answer is stale.
 * - `graph.db_path` and `graph.size_bytes`: `GraphReader` deliberately exposes no database path, so
 *   the path is derived from the injected `ctx.config.dataDir` — never from the environment — and
 *   the size is the one recorded when the reader opened that file (`ctx.dbSizeBytes`); a `stat` of
 *   the derived path is only the fallback for a context that carries no record, because a publish
 *   can rename a new file over the path.
 *
 * The arithmetic behind `coverage` is the one place this handler has to be careful. A scope answers
 * about itself: `coverage.skipped` counts exactly the rows `coverage.not_indexed` lists, so the two
 * can never describe two populations, and `coverage.complete` is that scope's verdict. The
 * repository's own count travels beside it as `totals.not_indexed_total`, which is why the two
 * numbers may differ without either lying.
 *
 * The census identity those numbers rest on is `sum(languages[].files) === indexed files +
 * not_indexed rows`: `censusByLanguage` folds `not_indexed` into the census (a skipped path is one
 * more file of its language that failed), so the skipped side of the census is the *graph's*, not
 * the scope's. The same holds for `sum(languages[].failed) === failed parses + skipped paths`,
 * which the tests assert.
 *
 * `payload.unresolved` is the bucket breakdown of the flat `coverage.unresolved_edges` count, taken
 * from the one row set that produced it: a census over different rows would let two numbers in the
 * same answer describe two populations, so `total`, `sum(buckets)` and `unresolved_edges` are the
 * same measurement read once.
 */
import { createHash } from "node:crypto";
import { statSync } from "node:fs";
import { join } from "node:path";
import { budgetFloorOf, emit } from "#shared/budget";
import { checkFreshness, type Freshness, health, type ToolHealth } from "#shared/domain/freshness";
import { bucketCensus, type UnresolvedBucket } from "#shared/domain/unresolved";
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
import { AppError } from "#shared/errors";
import { isUnderPrefix, normalizeRel } from "#shared/paths";
import type { RepoConfigSource } from "#shared/repo-config";
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

/**
 * The `unresolved` census: the flat count with the reason each row never bound (#5 in
 * `#shared/domain/unresolved.ts`). `material` is the part that could still be user-defined code.
 */
export interface StatusUnresolved {
  readonly total: number;
  readonly material: number;
  readonly buckets: Readonly<Record<UnresolvedBucket, number>>;
}

/** The payload keys TOOLS-SPEC.md §2 documents, in that order. */
export interface StatusPayload {
  readonly project: {
    readonly name: string | null;
    readonly root: string | null;
    /**
     * `null` when the store never recorded it: a graph published before this build wrote the two
     * keys below, or a publish where git could not answer. `status` still answers.
     */
    readonly branch: string | null;
    readonly remote: string | null;
    readonly commit: string | null;
    readonly db_path: string;
  };
  /**
   * Which file the three repository settings were read from. `project` means this tree's own
   * `<dataDir>/config.json` was read; `global` means it is absent (or unreadable) and the machine
   * file supplied the values. The fallback is normal and silent, so this field is where it becomes
   * visible.
   */
  readonly config: {
    readonly source: RepoConfigSource;
  };
  readonly graph: {
    readonly nodes: number;
    readonly edges: number;
    readonly size_bytes: number;
    readonly db_path: string;
  };
  readonly unresolved: StatusUnresolved;
  readonly languages: readonly StatusLanguage[];
  readonly paths: readonly StatusPath[];
  readonly health: ToolHealth;
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
  // One skipped-path list answers one call: a request that names paths scopes to those paths, and a
  // request that names none scopes to the repository. `rowsForPaths([])` matches nothing at all, so
  // the no-paths case has to assemble its list from the graph or report `not_indexed: []` about a
  // repository it never looked at.
  const allSkipped = ctx.reader.notIndexedAll();
  const rows = ctx.reader.rowsForPaths(requested);
  const scoped = requested.length === 0 ? allSkipped : rows.notIndexed;
  const census = ctx.reader.censusByLanguage();
  // One row set, read once: `coverage.unresolved_edges` and `payload.unresolved.total` are the same
  // measurement, so the census can never contradict the count it explains.
  const unresolvedRows = ctx.reader.unresolvedForFiles(files.map((file) => file.id));
  const unresolvedCensus = bucketCensus(unresolvedRows);
  const unresolvedTotal = unresolvedRows.length;

  const failed = files.filter((file) => file.parseStatus === "failed").length;
  // Read once: the summary names it and `totals.not_indexed_total` publishes it, and the two must
  // be the same measurement.
  const notIndexedTotal = ctx.reader.notIndexedCount();

  const coverage: Coverage = {
    // The scope's own verdict: its lists and its parse failures. The repository-wide count lives in
    // `totals.not_indexed_total`, so it does not gate whether *this* answer is complete.
    complete: files.length > 0 && scoped.length === 0 && failed === 0,
    indexed: files.length,
    // `skipped` counts the assembled scope and `totals.not_indexed_total` counts the graph, so the
    // number always equals the length of the `not_indexed` list printed beside it.
    skipped: scoped.length,
    failed,
    unresolved_edges: unresolvedTotal,
    not_indexed: scoped.map((row) => ({ path: row.path, reason: row.reason })),
    // The test corpus the graph indexed: `nodes: 0` is "this index saw no test symbols", which is a
    // different fact from "nothing covers this change".
    tests: ctx.reader.testsCensus(),
  };

  const staleFiles = files.filter((file) => file.indexedGeneration !== generation).length;
  // Freshness is computed once, from the reader: both the envelope's `read_status` and the summary
  // are worded from it. A tool decides what to show; `shared/domain` decides what is true.
  const freshness = checkFreshness(ctx.reader, ctx.config.repoRoot, { now: ctx.now });
  const readStatus = readStatusOf(
    freshness,
    meta,
    generation,
    generationRow,
    staleFiles,
    files.length,
  );

  const report = pathReport(ctx, requested, rows);
  const dbPath = join(ctx.config.dataDir, "graph.db");

  const payload: StatusPayload = {
    project: {
      name: meta.projectName,
      root: meta.projectRoot,
      // A hand-written reader written before these keys existed answers without them; `?? null`
      // keeps `undefined` out of the payload, where it would serialise as a missing key.
      branch: meta.projectBranch ?? null,
      remote: meta.projectRemote ?? null,
      commit: generationRow.baseCommit,
      db_path: dbPath,
    },
    config: { source: ctx.config.repoConfigSource },
    graph: {
      nodes: generationRow.nodes,
      edges: generationRow.edges,
      size_bytes: sizeBytesOf(ctx, dbPath),
      db_path: dbPath,
    },
    unresolved: {
      total: unresolvedTotal,
      material: unresolvedCensus.material,
      buckets: unresolvedCensus.buckets,
    },
    languages: languageEntries(census),
    paths: report.entries,
    // The same module's answer: indexer state, the served publish's duration, the failure counter
    // and the reader handle's prepared-statement cache size.
    health: health(ctx.reader),
  };

  const spec: EnvelopeSpec = {
    summary: summaryOf(
      coverage,
      generation,
      staleFiles,
      freshness.files_changed_since,
      requested.length,
      report.indexedPaths,
      notIndexedTotal,
    ),
    read_status: readStatus,
    coverage,
    totals: {
      total_relation: relationFor(readStatus, coverage),
      paths_total: report.entries.length,
      // Repo-wide, read from the graph rather than from `scoped`: the same number `summary` and
      // `expand` publish, so a narrow scope cannot understate what the repository never read.
      not_indexed_total: notIndexedTotal,
    },
    // `expand` is the tool that consumes a `next` cursor, and it is the only useful continuation
    // this tool has: everything else `status` reports is already in the payload.
    next_tools: ["expand"],
  };

  const argsHash = argsHashOf(requested, budget);
  try {
    const emitted = emit(ok<StatusPayload>(payload, spec), budget, {
      mint: (position) => ctx.cursor.sign({ generation, tool: manifest.name, argsHash, position }),
    });
    // A budget below the floor this report needs cannot be cut to fit, so `emit` hands the envelope
    // back untouched: serving it would be a report many times the allowance. The status tool answers
    // with the floor instead, and the same envelope-shape rule as the ceiling refuses it.
    const floor = budgetFloorOf(emitted);
    if (floor > budget) {
      return fail(
        new AppError("budget_too_large", `budget ${budget} is below the floor this report needs`, {
          hint: `raise the budget to at least ${floor} tokens; the default is ${STATUS_DEFAULT_TOKENS}`,
        }),
        { ...spec, summary: "the requested budget could not be applied" },
      );
    }
    return emitted;
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
 * The freshness verdict as the envelope spells it. `stale` and `stale_checked_at` come from
 * `#shared/domain/freshness.ts` untouched; this function only chooses the words.
 *
 * The graph-side fact stays visible, because it is a different fact from disk drift: a `file` row
 * the indexer wrote in an earlier generation means the served graph is behind its own tree. It is
 * reported first whenever freshness found nothing, in the wording earlier answers used, and it is
 * appended when freshness did find something, so one reason never hides the other. A fresh answer
 * carries no `stale_reason` key at all — absent, not empty.
 */
function readStatusOf(
  freshness: Freshness,
  meta: GraphMeta,
  generation: string,
  generationRow: GenerationRow,
  staleFiles: number,
  indexedFiles: number,
): ReadStatus {
  const status: ReadStatus = {
    generation,
    indexed_at: meta.indexedAt ?? generationRow.publishedAt,
    stale: freshness.stale,
    stale_checked_at: freshness.checked_at,
  };
  const older = olderGenerationReason(staleFiles, indexedFiles);
  const reason = freshnessReason(freshness, indexedFiles);
  if (reason !== null) {
    status.stale = true;
    status.stale_reason = older === null ? reason : `${reason}; ${older}`;
    return status;
  }
  if (older !== null) {
    status.stale = true;
    status.stale_reason = older;
  }
  return status;
}

/** The words for each filesystem-side verdict; `none` and the graph-side fact are handled above. */
function freshnessReason(freshness: Freshness, indexedFiles: number): string | null {
  switch (freshness.stale_reason) {
    case "root_mismatch":
      // The one verdict that makes every other comparison meaningless: the graph describes another
      // directory, so no count of this one belongs next to it.
      return "the graph was built for a different project_root";
    case "base_commit_moved":
      return "the indexed base commit is no longer HEAD";
    case "watcher_stalled":
      return "the indexer has been busy for over 10 minutes with nothing published";
    case "files_changed":
      return `${freshness.files_changed_since} of ${indexedFiles} indexed files changed on disk since the index`;
    case "none":
      return null;
  }
}

/** The graph-side half: rows the indexer wrote in a generation other than the one being served. */
function olderGenerationReason(staleFiles: number, indexedFiles: number): string | null {
  if (staleFiles === 0) {
    return null;
  }
  return `${staleFiles} of ${indexedFiles} indexed files were written by an earlier generation`;
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
 * The graph file's size as a `stat` of `dbPath`, or `0` when it cannot be read — the fallback for a
 * context that carries no recorded size (a hand-written one), and the only answer available there.
 * `0` is not a fallback for "unknown size" — it is reported next to `nodes`/`edges`, so a zero with
 * a non-zero node count reads as "the file was not statable" and the reason goes to the log instead
 * of into the payload.
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
 * The graph file's size in bytes, preferring what was recorded when the reader opened it
 * (`ToolContext.dbSizeBytes`): a publish renames a new file over `graph.db`, so a `stat` taken here
 * can describe a generation this handle is not serving. The stat below is the fallback for a
 * hand-written context that carries no record, where it is still the only answer available.
 */
function sizeBytesOf(ctx: ToolContext, dbPath: string): number {
  const recorded = ctx.dbSizeBytes;
  return typeof recorded === "number" ? recorded : dbSizeBytes(ctx, dbPath);
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

/**
 * The skipped clause of the summary line: the count that matches the scope, and the repository's
 * total only when it says more than the scope did — otherwise the line would name one number under
 * the other's label.
 */
function skippedClause(skipped: number, notIndexedTotal: number): string[] {
  if (skipped === 0) {
    return [];
  }
  return [
    notIndexedTotal > skipped
      ? `${skipped} skipped of ${notIndexedTotal} repo-wide`
      : `${skipped} skipped`,
  ];
}

function summaryOf(
  coverage: Coverage,
  generation: string,
  staleFiles: number,
  filesChanged: number,
  requestedCount: number,
  indexedPaths: number,
  notIndexedTotal: number,
): string {
  const counts = [
    `${coverage.indexed} files indexed`,
    `generation ${generation}`,
    ...skippedClause(coverage.skipped, notIndexedTotal),
    ...(coverage.failed > 0 ? [`${coverage.failed} failed to parse`] : []),
    ...(staleFiles > 0 ? [`${staleFiles} written by an earlier generation`] : []),
    ...(filesChanged > 0 ? [`${filesChanged} changed on disk since the index`] : []),
  ].join(", ");
  const scope =
    requestedCount === 0
      ? "Whole repository."
      : `${indexedPaths} of ${requestedCount} requested paths indexed.`;
  return `${counts}. ${scope}`;
}
