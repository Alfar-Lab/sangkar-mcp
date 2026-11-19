/**
 * `search`: BM25 over the symbol index (TOOLS-SPEC.md §5).
 *
 * The one rule this handler exists to keep is that a page boundary is stable. The reader returns
 * FTS hits already ordered by `bm25` ascending, then `in_degree` descending, then `id` ascending,
 * so the tool never re-sorts: a re-sort is how a cursor starts repeating or skipping rows, and
 * degree is only a *tie-breaker inside the reader's ordering*, not a second ranking here. A
 * cursor's `position` is therefore an index into the flattened, already-filtered hit list, and
 * page two is the same list sliced one item later.
 *
 * Nothing in this file queries SQL, opens a connection or reads the environment: every row comes
 * from `ctx.reader`, and the generation comes from `metaFor()` so `read_status` and the cursor
 * both name the generation the answer was served from.
 */
import { createHash } from "node:crypto";
import { budgetFloorOf, emit } from "#shared/budget";
import { assemble } from "#shared/domain/coverage";
import {
  bootstrapping,
  type Coverage,
  type Envelope,
  type EnvelopeSpec,
  fail,
  notFound,
  ok,
  type ReadStatus,
  type TotalRelation,
  type Totals,
} from "#shared/envelope";
import { AppError } from "#shared/errors";
import { isUnderPrefix, normalizeRel } from "#shared/paths";
import type { ToolContext } from "#shared/store/read/context";
import type { FtsHit, GraphReader, LineRange } from "#shared/store/read/reader";
// The default budget is a property of the advertised schema, so it lives in the manifest and is
// imported here: one number, so the default the root shows and the fallback used here cannot drift.
import { DEFAULT_BUDGET_TOKENS, type SearchKind } from "./manifest";

/**
 * The statement bound handed to `readNodeFts`, not a page size. `reader.ts` fixes `limit` as a
 * bound on how many rows the statement may return; the product decision about how many are shown
 * belongs to `emit`. 200 is ten times the ~20-hit page a default budget holds, so the cut a caller
 * sees is a budget cut with a continuation. A query with more than 200 matches is the one case the
 * bound rather than the budget ends the list, and the summary says so instead of pretending the
 * 200th hit was the last one.
 */
const READER_BOUND = 200;

const TOOL = "search";

/** Detail on demand for one hit: `cursor` pages a list, `expand(target)` opens an item (§1). */
const NEXT_TOOLS: readonly string[] = ["expand"];

/**
 * The index has not published a generation yet, so there is no graph to search. Answering with an
 * empty result list would be a false absence claim; the caller is told to retry instead.
 */
const BOOTSTRAP_RETRY_MS = 1_000;

/** The arguments the root parses from {@link input} in `./manifest.ts`. */
export interface SearchArgs {
  readonly query: string;
  readonly kind?: SearchKind;
  readonly path?: string;
  readonly budget?: number;
  readonly cursor?: string;
}

/** In/out degree, the cheapest disambiguator a ranked list can carry (§5). */
export interface SearchDegree {
  readonly in: number;
  readonly out: number;
}

/** One declaration. Never source text: same rationale as `summary` (§5 "No source"). */
export interface SearchResult {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: LineRange;
  /** Present only when the row has one — never `undefined` (exactOptionalPropertyTypes). */
  readonly signature?: string;
  readonly degree: SearchDegree;
  readonly score: number;
}

/**
 * `mode` is disclosed rather than implied: an agent that asked something semantic and got keyword
 * order should know before it trusts the ranking (§5).
 */
export interface SearchPayload {
  readonly mode: "bm25";
  readonly results: SearchResult[];
}

/** `ok`/`not_found` carry a payload; `error`/`bootstrapping` carry `null` (envelope.ts). */
export type SearchResponse = Envelope<SearchPayload | null>;

/**
 * The call identity a cursor pins: the three arguments that decide *which* rows are in the list
 * and in *what order*. `budget` is deliberately absent — it decides how much of the same list is
 * shown, so resuming the next page with a larger budget is allowed; `cursor` is the page itself.
 */
function argsHashOf(query: string, kind: SearchKind | null, prefix: string): string {
  return createHash("sha256")
    .update(JSON.stringify([query, kind, prefix]), "utf8")
    .digest("hex");
}

/**
 * The optional filters, applied after the read and without touching the order. `isUnderPrefix`
 * is what keeps `src` from matching `src-old` (`#shared/paths`), and an empty prefix is the whole
 * tree rather than a prefix of nothing.
 */
function matches(hit: FtsHit, kind: SearchKind | undefined, prefix: string): boolean {
  if (kind !== undefined && hit.kind !== kind) {
    return false;
  }
  return prefix === "" || isUnderPrefix(hit.file, prefix);
}

/**
 * One payload row. `metricsFor` returning `null` means the publish never measured the node; the
 * documented payload still requires two integers, and a search hit is a disambiguator rather than
 * an absence claim, so an unmeasured node reads `0/0` here and the *absence* questions stay with
 * the tools that gate on coverage.
 *
 * The signature is one unique-index lookup per candidate, and candidates are sliced to the page
 * window first, because `FtsHit` carries identity and rank only.
 */
function toResult(reader: GraphReader, hit: FtsHit): SearchResult {
  const metrics = reader.metricsFor(hit.id);
  const base = {
    id: hit.id,
    qn: hit.qn,
    kind: hit.kind,
    file: hit.file,
    lines: hit.lines,
    degree: { in: metrics?.inDegree ?? 0, out: metrics?.outDegree ?? 0 },
    score: hit.score,
  };
  const signature = reader.findByQn(hit.qn)?.signature ?? null;
  // The key is added, never written as `undefined`: exactOptionalPropertyTypes treats the two as
  // different payloads, and a client diffing two answers would see a `signature` that vanished.
  return signature === null || signature === "" ? base : { ...base, signature };
}

/**
 * The one line a model can quote (§1.2). "showing N of M" is a range once a page starts past the
 * first item, and `capped` says so plainly when the statement bound, rather than the budget, is
 * what ended the candidate set.
 */
function summaryFor(options: {
  query: string;
  total: number;
  start: number;
  shown: number;
  capped: boolean;
}): string {
  const { query, total, start, shown, capped } = options;
  const head =
    total === 1
      ? `Found 1 declaration for '${query}'`
      : `Found ${total} declarations for '${query}'`;
  const cap = capped ? ` among the ${READER_BOUND} best matches` : "";
  if (shown === 0) {
    return `${head}${cap}; nothing further from position ${start}.`;
  }
  const first = start + 1;
  const last = start + shown;
  const showing =
    first === 1 && last >= total ? `${total} of ${total}` : `${first}-${last} of ${total}`;
  return `${head}${cap}, showing ${showing}.`;
}

/** The query matched, the filters did not: saying which one emptied the list is the difference. */
function filteredOutSummary(query: string, matched: number): string {
  return (
    `No declarations matched '${query}' under the requested kind or path filter,` +
    ` though ${matched} matched the query alone.`
  );
}

/** Everything an answer says about the generation it was served from and the files it covers. */
interface Scope {
  readonly readStatus: ReadStatus;
  readonly coverage: Coverage;
  readonly relation: TotalRelation;
}

/**
 * Scope, freshness and coverage from one `rowsForPaths` read.
 *
 * The staleness verdict is the graph-side one `coverage.ts` and `status` use: a file whose
 * `indexed_generation` is not the served generation makes the answer stale even when the working
 * tree has not moved, and a handler has no filesystem to compare bytes against. Handing
 * `assemble` the rows already read keeps one lookup for both, and `stale` reaches it so the
 * relation is downgraded rather than computed from a constant.
 *
 * `stale_checked_at` is claimed only when there was a file to look at: "we did not look" and "we
 * looked and it is current" are different statements (`#shared/envelope.ts`). A repo-wide search
 * passes no prefix, so `rowsForPaths` has nothing to answer — `complete: false` and `gte` are the
 * whole verdict, exactly as for a scope the indexer never saw.
 */
function scopeOf(
  ctx: ToolContext,
  generation: string,
  indexedAt: string,
  scopePaths: readonly string[],
): Scope {
  const rows = ctx.reader.rowsForPaths(scopePaths);
  const staleFiles = rows.indexed.filter((file) => file.indexedGeneration !== generation).length;
  const stale = staleFiles > 0;
  const checked: ReadStatus =
    rows.indexed.length === 0
      ? { generation, indexed_at: indexedAt, stale }
      : { generation, indexed_at: indexedAt, stale, stale_checked_at: ctx.now().toISOString() };
  const readStatus: ReadStatus = stale
    ? {
        ...checked,
        stale_reason: `${staleFiles} of ${rows.indexed.length} file(s) in scope were written by an earlier generation`,
      }
    : checked;
  const assembly = assemble(
    {
      rowsForPaths: () => rows,
      unresolvedForFiles: (fileIds) => ctx.reader.unresolvedForFiles(fileIds),
      notIndexedCount: () => ctx.reader.notIndexedCount(),
    },
    { paths: scopePaths, generation, stale },
  );
  return { readStatus, coverage: assembly.coverage, relation: assembly.relation };
}

/**
 * Answers one `search` call. The order of the steps is part of the contract: a cursor is verified
 * against `{generation, tool, argsHash}` *before* the search work, so a stale or foreign token can
 * never be served a page from this call.
 */
export async function handle(ctx: ToolContext, args: SearchArgs): Promise<SearchResponse> {
  // The generation is the precondition of everything below — a cursor binds to it and `read_status`
  // must name it — so this one meta read happens before the cursor check, not as search work.
  const { generation, indexedAt } = ctx.reader.metaFor();
  if (generation === null || indexedAt === null) {
    const { coverage } = assemble(ctx.reader, { paths: [], generation: "", stale: true });
    return bootstrapping({
      summary: "The index has not published a generation yet.",
      read_status: { generation: "", indexed_at: ctx.now().toISOString(), stale: true },
      coverage,
      retry_after_ms: BOOTSTRAP_RETRY_MS,
      hint: "re-run once the indexer has published; `status` reports the progress",
    });
  }

  // `stale` reports a *known* drift, and the only drift a handler can see is graph-side (a file
  // written by an earlier publish); nothing in `ToolContext` compares the working tree against
  // the manifest — that is the phase-3 freshness seam — so this never claims the tree was read.
  const query = args.query.trim();
  const prefix = normalizeRel(args.path ?? "");
  const scopePaths = prefix === "" ? [] : [prefix];
  const argsHash = argsHashOf(query, args.kind ?? null, prefix);
  // The envelope's own scope reads (freshness, coverage) happen before the cursor check because a
  // refusal envelope must carry them too; the *search* — the MATCH and its ranking — does not.
  const { readStatus, coverage, relation } = scopeOf(ctx, generation, indexedAt, scopePaths);

  let start = 0;
  if (args.cursor !== undefined) {
    const verified = ctx.cursor.verify(args.cursor, { generation, tool: TOOL, argsHash });
    if (!verified.ok) {
      // The refusal code and hint are the cursor module's to word (DATABASE.md §8): a handler that
      // re-worded them could tell an agent to guess a page instead of re-running the call.
      return fail(
        new AppError(verified.code, `the cursor was not accepted: ${verified.code}`, {
          hint: verified.hint,
        }),
        {
          summary: `Cursor refused (${verified.code}); re-run the search for the first page.`,
          read_status: readStatus,
          coverage,
        },
      );
    }
    start = verified.position;
  }

  if (query === "") {
    return notFound(
      { mode: "bm25", results: [] },
      {
        summary: "An empty query has nothing to match; give search a word or a name.",
        read_status: readStatus,
        coverage,
        totals: { total_relation: relation, results_total: 0 },
        next_tools: [],
      },
    );
  }

  // The scope of the absence claim: exactly the prefix that filtered the rows, so `complete` and
  // `total_relation` describe the same set of files the answer could have come from.
  const hits = ctx.reader.readNodeFts(query, READER_BOUND);
  const filtered = hits.filter((hit) => matches(hit, args.kind, prefix));
  const window = filtered.slice(start);
  const results = window.map((hit) => toResult(ctx.reader, hit));
  const capped = hits.length === READER_BOUND;

  const totals: Totals = { total_relation: relation, results_total: filtered.length };
  const spec: EnvelopeSpec = {
    summary:
      filtered.length === 0 && hits.length > 0
        ? filteredOutSummary(query, hits.length)
        : summaryFor({ query, total: filtered.length, start, shown: results.length, capped }),
    read_status: readStatus,
    coverage,
    totals,
    next_tools: results.length === 0 ? [] : NEXT_TOOLS,
  };
  const payload: SearchPayload = { mode: "bm25", results };
  const env: Envelope<SearchPayload> =
    filtered.length === 0 ? notFound(payload, spec) : ok(payload, spec);

  const budget = args.budget ?? DEFAULT_BUDGET_TOKENS;
  // `emit` is the only place a payload is cut, and it reports what it dropped: the kept count is
  // `window.length - dropped`, so the resumed page starts exactly where this one stopped and the
  // ordering is never re-derived from anything but the reader's own list. The floor at zero is
  // there because `emit` keeps going after the results list is empty (it then trims envelope-level
  // lists), and a signed cursor may never carry a negative position.
  const mint = (dropped: number): string => {
    const kept = Math.max(0, window.length - dropped);
    return ctx.cursor.sign({ generation, tool: TOOL, argsHash, position: start + kept });
  };

  let emitted: Envelope<SearchPayload>;
  try {
    emitted = emit(env, budget, { mint });
  } catch (error) {
    // `budget_too_large` is a client error (TOOLS-SPEC.md §9), and a handler returns envelopes.
    return fail(error, spec);
  }

  // A zero-row page carrying a continuation token would be an infinite loop, and a budget below the
  // floor this answer needs cannot be cut to fit at all (`emit` then returns the envelope untouched
  // rather than mislabelling a cut). Both are the refusal the caller can act on, not a page they
  // cannot use.
  const floor = budgetFloorOf(emitted);
  if (window.length > 0 && (emitted.payload.results.length === 0 || floor > budget)) {
    return fail(
      new AppError(
        "budget_too_large",
        `budget ${budget} fits none of the ${window.length} remaining declarations`,
        {
          hint: `raise the budget; this answer needs at least ${floor} tokens`,
        },
      ),
      spec,
    );
  }

  // The cut belongs to `emit`, so the exact "showing N of M" count exists only after it runs: the
  // summary is corrected on the copy it returns rather than guessed before the cut.
  return emitted.truncated
    ? {
        ...emitted,
        summary: summaryFor({
          query,
          total: filtered.length,
          start,
          shown: emitted.payload.results.length,
          capped,
        }),
      }
    : emitted;
}
