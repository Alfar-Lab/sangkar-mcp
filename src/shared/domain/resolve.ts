/**
 * The resolution contract, written once for `trace`, `search` and `expand` (`DESIGN.md` §8).
 *
 * One free-text query becomes at most one node. The failure mode this module exists to prevent
 * is a confident answer about the wrong node, so it never guesses: when two or more candidates
 * score the same it answers `ambiguous`, and when nothing matches it answers `not_found` with
 * near misses instead of a bare zero.
 *
 * Detection order is fixed and observable through {@link RESOLUTION_STEPS}: exact qualified
 * name, exact name plus kind, dotted tail, path, then `@decorator` as a *property filter*
 * (`DESIGN.md` §8: decorators are properties, not a node kind, so no `decorator` kind is ever
 * invented here).
 *
 * This module decides what is true, never what to show: scores and `why` are part of the
 * candidate contract, the ≤5 candidate cap is the contract's own limit, and there is no
 * caller-tunable `limit`.
 */
import { isUnderPrefix, normalizeRel } from "#shared/paths";
import type { Conn } from "#shared/store/db";
import { findByName, findByQn, type NodeRow } from "#shared/store/read/nodes";

/**
 * The contract cap on ambiguity output. Not a presentation knob: it is the width of the
 * `candidates` list `TOOLS-SPEC.md` §3 allows, so the tool never has to decide.
 */
export const CANDIDATE_CAP = 5;

/** The detection order, as data, so a caller can report which step answered. */
export const RESOLUTION_STEPS = [
  "exact_qn",
  "exact_name",
  "dotted_tail",
  "path",
  "decorator",
] as const;
export type ResolutionStep = (typeof RESOLUTION_STEPS)[number];

/** One plausible match, with everything needed to judge it without a second call. */
export interface ResolutionCandidate {
  readonly id: number;
  readonly qn: string;
  readonly kind: string;
  readonly file: string;
  readonly lines: readonly [number, number];
  readonly score: number;
  readonly why: string;
}

/**
 * A resolved query is one of three answers and never a partial one: `ok` carries the node,
 * `ambiguous` carries the tied candidates (no answer body), `not_found` carries near misses.
 *
 * `candidates_total` is the size of the tie *before* the `CANDIDATE_CAP` slice, so an answer that
 * shows five of twelve says so; `candidates_shown` is `candidates.length` and the two cannot drift.
 */
export type Resolution =
  | {
      readonly status: "ok";
      readonly step: ResolutionStep;
      readonly score: number;
      readonly why: string;
      readonly node: NodeRow;
    }
  | {
      readonly status: "ambiguous";
      readonly step: ResolutionStep;
      readonly candidates: readonly ResolutionCandidate[];
      readonly candidates_total: number;
      readonly candidates_shown: number;
    }
  | {
      readonly status: "not_found";
      readonly near_misses: readonly ResolutionCandidate[];
    };

/**
 * Caller-side narrowing. `kinds` is all-or-nothing like `findByName`; `path` is a repo-relative
 * prefix applied before any cap, so a filtered query never answers from outside it.
 *
 * A bare `readonly string[]` is still accepted in this position and means `{ kinds }`: `trace`
 * passes its `kinds` array positionally and must keep compiling and answering unchanged.
 */
export interface ResolveOptions {
  readonly kinds?: readonly string[];
  readonly path?: string;
}

/** The options after normalization: a normalized `kinds` list and a normalized prefix or none. */
interface Filter {
  readonly kinds: readonly string[];
  readonly under: string | null;
}

/**
 * The read surface resolution needs, with the connection already bound. A `Conn`-backed
 * implementation comes from {@link resolutionSource}; a test passes a hand-written object over
 * an in-memory fixture, so the ambiguity contract is provable without a database.
 *
 * `findByDecorator` is optional because the store has no props query yet. When it is absent the
 * decorator step degrades to "name matches *and* carries the property", which is the honest
 * subset of the filter rather than an invented node kind.
 */
export interface ResolutionSource {
  findByQn(qn: string): NodeRow | null;
  findByName(name: string, kinds?: readonly string[]): NodeRow[];
  findByDecorator?(name: string, kinds?: readonly string[]): NodeRow[];
}

/** Binds the store's prepared lookups to a connection; the only database-aware line here. */
export function resolutionSource(conn: Conn): ResolutionSource {
  return {
    findByQn: (name) => findByQn(conn, name),
    findByName: (name, kinds) => findByName(conn, name, kinds),
  };
}

/** One scored hit, kept with its row so `ok` can return the node without a second lookup. */
interface Hit {
  readonly node: NodeRow;
  readonly score: number;
  readonly why: string;
}

/** The hits of one detection step; only the first non-empty step is consulted. */
interface StepHit {
  readonly step: ResolutionStep;
  readonly hits: readonly Hit[];
}

/**
 * Step scores. Their absolute values matter only for ordering; what matters for the ambiguity
 * contract is that every candidate produced by the same step with the same evidence scores
 * *identically*, which is what makes a tie a genuine tie.
 */
const STEP_SCORE: Readonly<Record<ResolutionStep, number>> = {
  exact_qn: 1,
  exact_name: 0.9,
  dotted_tail: 0.8,
  path: 0.7,
  decorator: 0.6,
};

/** A dotted query whose receiver matches the node's own scope is the better of two tail hits. */
const DOTTED_SCOPE_BONUS = 0.05;
/** A path query names the file node itself, not one of its symbols. */
const PATH_FILE_BONUS = 0.05;
/** Everything below the weakest step: plausible, never a match. */
const NEAR_MISS_SCORE = 0.3;

/** `0.8 + 0.05` is `0.8500000000000001`; two decimals keeps the tie comparison exact. */
function round2(value: number): number {
  return Math.round(value * 100) / 100;
}

function candidateOf(hit: Hit): ResolutionCandidate {
  return {
    id: hit.node.id,
    qn: hit.node.qn,
    kind: hit.node.kind,
    file: hit.node.path,
    lines: [hit.node.startLine, hit.node.endLine],
    score: hit.score,
    why: hit.why,
  };
}

/** An empty `kinds` list means every kind, matching `findByName`'s all-or-nothing filter. */
function acceptsKind(node: NodeRow, kinds: readonly string[]): boolean {
  return kinds.length === 0 || kinds.includes(node.kind);
}

/**
 * The path half of the filter. It can never be pushed into a store lookup, so every step applies
 * it to the rows it got back — that is what keeps a near miss from outside the prefix out.
 */
function acceptsPath(node: NodeRow, filter: Filter): boolean {
  return filter.under === null || isUnderPrefix(node.path, filter.under);
}

function isLegacyKinds(value: ResolveOptions | readonly string[]): value is readonly string[] {
  return Array.isArray(value);
}

/**
 * Absent, empty and whitespace-only all mean "no filter", so a caller that passes `path: ""`
 * gets the answer it got before the option existed. `normalizeRel` would turn `""` into a prefix
 * that matches nothing, which is the one outcome this must not produce.
 */
function filterOf(options?: ResolveOptions | readonly string[]): Filter {
  const resolved: ResolveOptions =
    options === undefined ? {} : isLegacyKinds(options) ? { kinds: options } : options;
  const path = normalizeRel((resolved.path ?? "").trim());
  return { kinds: resolved.kinds ?? [], under: path === "" ? null : path };
}

function hit(node: NodeRow, score: number, why: string): Hit {
  return { node, score: round2(score), why };
}

/** Step 1: the unique `qn`. Nothing can tie with it, so it is answered immediately. */
function exactQn(source: ResolutionSource, query: string, filter: Filter): StepHit | null {
  const node = source.findByQn(query);
  if (node === null || !acceptsKind(node, filter.kinds) || !acceptsPath(node, filter)) {
    return null;
  }
  return { step: "exact_qn", hits: [hit(node, STEP_SCORE.exact_qn, "exact qualified name")] };
}

/** Step 2: bare name, narrowed by `kinds` when the caller supplied them. */
function exactName(source: ResolutionSource, query: string, filter: Filter): StepHit | null {
  const nodes = source.findByName(query, filter.kinds).filter((node) => acceptsPath(node, filter));
  if (nodes.length === 0) {
    return null;
  }
  const total = nodes.length;
  return {
    step: "exact_name",
    hits: nodes.map((node, index) =>
      hit(
        node,
        STEP_SCORE.exact_name,
        total === 1
          ? "exact name, 1 of 1 candidate"
          : `exact name, ${index + 1} of ${total} candidates`,
      ),
    ),
  };
}

/** Step 3: `Receiver.name` resolved through its tail, preferring the tail in its own scope. */
function dottedTail(source: ResolutionSource, query: string, filter: Filter): StepHit | null {
  const dot = query.lastIndexOf(".");
  if (dot <= 0 || dot === query.length - 1) {
    return null;
  }
  const tail = query.slice(dot + 1);
  const nodes = source.findByName(tail, filter.kinds).filter((node) => acceptsPath(node, filter));
  if (nodes.length === 0) {
    return null;
  }
  return {
    step: "dotted_tail",
    hits: nodes.map((node) => {
      const scoped = node.qn.endsWith(`.${query}`) || node.qn.endsWith(`::${query}`);
      return hit(
        node,
        STEP_SCORE.dotted_tail + (scoped ? DOTTED_SCOPE_BONUS : 0),
        scoped
          ? `dotted name "${query}" matched in the node's own scope`
          : `dotted tail "${tail}" of "${query}"`,
      );
    }),
  };
}

/**
 * A query shaped like a path rather than a symbol: it has a separator, or it ends in a known
 * source extension. The extension list is what keeps `Receiver.name` out of this step.
 */
const PATH_LIKE =
  /(^|\/)\.?[\w.+-]+\.(?:[cm]?[jt]sx?|py|go|rs|java|kt|rb|cs|cpp|cc|cxx|c|h|hpp|php|swift|scala|sql|sh|md|json|ya?ml|toml|xml|html)$/;

/** Step 4: a path resolves to the file node that owns it, or to symbols declared inside it. */
function pathStep(source: ResolutionSource, query: string, filter: Filter): StepHit | null {
  if (!query.includes("/") && !PATH_LIKE.test(query)) {
    return null;
  }
  const normalized = normalizeRel(query);
  if (normalized === "") {
    return null;
  }
  const basename = normalized.slice(normalized.lastIndexOf("/") + 1);
  const names = [basename];
  const extensionDot = basename.lastIndexOf(".");
  if (extensionDot > 0) {
    names.push(basename.slice(0, extensionDot));
  }

  const seen = new Set<number>();
  const hits: Hit[] = [];
  for (const name of names) {
    for (const node of source.findByName(name, filter.kinds)) {
      if (seen.has(node.id)) {
        continue;
      }
      if (node.path !== normalized && !isUnderPrefix(node.path, normalized)) {
        continue;
      }
      if (!acceptsPath(node, filter)) {
        continue;
      }
      seen.add(node.id);
      const isFile = node.kind === "file";
      hits.push(
        hit(
          node,
          STEP_SCORE.path + (isFile ? PATH_FILE_BONUS : 0),
          isFile ? `file "${normalized}"` : `symbol declared in "${normalized}"`,
        ),
      );
    }
  }
  return hits.length === 0 ? null : { step: "path", hits };
}

/** Annotated and namespaced decorators both compare by their bare name. */
function decoratorName(text: string): string {
  const withoutAt = text.startsWith("@") ? text.slice(1) : text;
  const call = withoutAt.indexOf("(");
  return (call === -1 ? withoutAt : withoutAt.slice(0, call)).trim();
}

/**
 * `PROPS.decorators` is `string[]` or `{name}[]` depending on the grammar, so both shapes are
 * read here instead of forcing one on the indexer.
 */
function hasDecorator(node: NodeRow, name: string): boolean {
  const decorators: unknown = node.props.decorators;
  if (!Array.isArray(decorators)) {
    return false;
  }
  return decorators.some((entry) => {
    if (typeof entry === "string") {
      return decoratorName(entry) === name;
    }
    if (typeof entry === "object" && entry !== null) {
      const value = (entry as { name?: unknown }).name;
      return typeof value === "string" && decoratorName(value) === name;
    }
    return false;
  });
}

/**
 * Step 5: `@name` is a property filter, never a kind. With no props query in the store the
 * fallback can only keep candidates whose name already matches, which is documented rather
 * than guessed away.
 */
function decoratorStep(source: ResolutionSource, query: string, filter: Filter): StepHit | null {
  if (!query.startsWith("@")) {
    return null;
  }
  const name = decoratorName(query);
  if (name === "") {
    return null;
  }
  const nodes = (
    source.findByDecorator !== undefined
      ? source.findByDecorator(name, filter.kinds)
      : source.findByName(name, filter.kinds).filter((node) => hasDecorator(node, name))
  ).filter((node) => acceptsPath(node, filter));
  if (nodes.length === 0) {
    return null;
  }
  return {
    step: "decorator",
    hits: nodes.map((node) =>
      hit(node, STEP_SCORE.decorator, `property filter @${name} on "${node.name}"`),
    ),
  };
}

/**
 * The steps in order. `exact_qn` produces one hit by construction, so a single winner there is
 * always a `ok` answer; every later step can tie and therefore can be `ambiguous`.
 */
const STEPS: ReadonlyArray<
  (source: ResolutionSource, query: string, filter: Filter) => StepHit | null
> = [exactQn, exactName, dottedTail, pathStep, decoratorStep];

/** Nothing matched: another symbol that shares a segment of the query is the useful answer. */
function nearMissNames(query: string): string[] {
  if (query.startsWith("@")) {
    const name = decoratorName(query);
    return name === "" ? [] : [name];
  }
  if (query.includes("/") || PATH_LIKE.test(query)) {
    const normalized = normalizeRel(query);
    const basename = normalized.slice(normalized.lastIndexOf("/") + 1);
    const extensionDot = basename.lastIndexOf(".");
    return extensionDot > 0 ? [basename, basename.slice(0, extensionDot)] : [basename];
  }
  return query.split(".").filter((segment) => segment !== "");
}

function nearMisses(
  source: ResolutionSource,
  query: string,
  filter: Filter,
): ResolutionCandidate[] {
  const candidates: ResolutionCandidate[] = [];
  const seen = new Set<number>();
  for (const name of nearMissNames(query)) {
    if (candidates.length >= CANDIDATE_CAP) {
      break;
    }
    for (const node of source.findByName(name, filter.kinds)) {
      if (!acceptsPath(node, filter)) {
        continue;
      }
      if (seen.has(node.id) || candidates.length >= CANDIDATE_CAP) {
        continue;
      }
      seen.add(node.id);
      candidates.push(
        candidateOf(hit(node, NEAR_MISS_SCORE, `segment "${name}" of "${query}" matched a symbol`)),
      );
    }
  }
  return candidates;
}

/**
 * Turns one step's hits into an answer: a strictly highest score wins, an equal highest score
 * is a tie and becomes `ambiguous` with both (or all) candidates. Never picks on a tie.
 */
function decide(stepHit: StepHit): Resolution {
  let top = stepHit.hits[0]?.score ?? 0;
  for (const entry of stepHit.hits) {
    if (entry.score > top) {
      top = entry.score;
    }
  }
  const winners = stepHit.hits.filter((entry) => entry.score === top);
  const first = winners[0];
  if (first === undefined) {
    // Unreachable: `decide` is only called with at least one hit.
    return { status: "not_found", near_misses: [] };
  }
  if (winners.length === 1) {
    return {
      status: "ok",
      step: stepHit.step,
      score: first.score,
      why: first.why,
      node: first.node,
    };
  }
  // The cap is applied after the tie is known, so the payload can say "five of twelve" instead of
  // implying the tie was five wide.
  const shown = winners.slice(0, CANDIDATE_CAP);
  return {
    status: "ambiguous",
    step: stepHit.step,
    candidates: shown.map(candidateOf),
    candidates_total: winners.length,
    candidates_shown: shown.length,
  };
}

/**
 * Resolves one free-text query against `source`. `kinds` narrows every step and is
 * all-or-nothing, exactly like `findByName`: an empty or omitted list means every kind. `path`
 * restricts every step — and the near misses — to nodes under that repo-relative prefix.
 *
 * Evaluation stops at the first step that produces a hit, so an exact name always wins over a
 * dotted tail and a dotted tail always wins over a path, even though a later step might also
 * have matched. A step whose only hits fall outside `path` produces no hit at all, so the filter
 * can narrow an answer and can empty it, but can never widen the detection order.
 */
export function resolve(
  source: ResolutionSource,
  query: string,
  options?: ResolveOptions | readonly string[],
): Resolution {
  const text = query.trim();
  if (text === "") {
    return { status: "not_found", near_misses: [] };
  }
  const filter = filterOf(options);
  for (const step of STEPS) {
    const stepHit = step(source, text, filter);
    if (stepHit === null || stepHit.hits.length === 0) {
      continue;
    }
    return decide(stepHit);
  }
  return { status: "not_found", near_misses: nearMisses(source, text, filter) };
}
