import {
  APP_ERROR_CODES,
  type AppErrorCode,
  type ErrorPayload,
  isAppError,
  toErrorPayload,
} from "#shared/errors";

/**
 * The five statuses a response may carry (DESIGN.md §5). `error` and `bootstrapping` are the
 * only two that also carry an `error` object; the rest are a positive or explicitly negative
 * answer, which is why their absence can be trusted.
 */
export const ENVELOPE_STATUSES = [
  "ok",
  "ambiguous",
  "not_found",
  "error",
  "bootstrapping",
] as const;
export type EnvelopeStatus = (typeof ENVELOPE_STATUSES)[number];

/**
 * Why a payload was cut (DESIGN.md §5). `budget` is set by `emit`; the other three are set by a
 * tool whose own hard limit was reached before the budget was.
 */
export type TruncationReason = "budget" | "item_cap" | "depth" | "page";

/**
 * `eq` claims the total is exact. It is permitted only when coverage is complete, no unresolved
 * edges exist in scope, the generation is fresh and the payload is not truncated; anything else
 * is a lower bound and must say `gte` (DESIGN.md §6).
 */
export type TotalRelation = "eq" | "gte";

/** One path the index knows it did not cover, with the reason it was skipped (DATABASE.md §4). */
export interface NotIndexedEntry {
  path: string;
  reason: string;
}

/**
 * Freshness of the answer itself. `stale_checked_at` is separate from `indexed_at` because
 * "when was this built" and "when did we last look" are different questions, and only the
 * second one lets an agent decide whether re-asking is worth it (TOOLS-SPEC.md §1.2).
 */
export interface ReadStatus {
  generation: string;
  indexed_at: string;
  stale: boolean;
  stale_reason?: string;
  stale_checked_at?: string;
}

/** What the answer is allowed to claim. `complete` gates every absence claim. */
export interface Coverage {
  complete: boolean;
  indexed: number;
  skipped: number;
  failed: number;
  unresolved_edges: number;
  not_indexed: NotIndexedEntry[];
}

/**
 * Per-list counts plus the relation they are counted under. Every `<list>_total` key survives
 * truncation untouched, so `calls_total: 2` with one item shown still reads "showing 1 of 2".
 *
 * A total is `null` only when the list it counts does not apply to this scope — an empty change
 * set has no test gaps to count, which is a different answer from "zero gaps" — and every other
 * total stays a number. `emit` copies totals verbatim, so neither may be inferred downstream.
 */
export interface Totals {
  total_relation: TotalRelation;
  [listTotal: `${string}_total`]: number | null;
  /** Optional because only a scope that assembles a coverage census has unresolved call sites to count. */
  readonly unresolved_material?: number;
  /** Optional because only an ambiguous answer served a slice of a tie. */
  readonly candidates_shown?: number;
}

/**
 * The one response shape every tool returns (TOOLS-SPEC.md §1.1). Nine fields are always
 * present, plus `payload`; `error` is present if and only if `status` is `error` or
 * `bootstrapping`. Construction happens only through the functions below, so a handler cannot
 * omit `read_status` or `coverage` by accident.
 */
export interface Envelope<T> {
  status: EnvelopeStatus;
  summary: string;
  read_status: ReadStatus;
  coverage: Coverage;
  totals: Totals;
  truncated: boolean;
  truncation_reason: TruncationReason | null;
  next: string | null;
  next_tools: string[];
  payload: T;
  error?: ErrorPayload;
}

/**
 * Everything a constructor needs that is not the payload. `totals` defaults to the relation the
 * read_status/coverage pair can honestly support; `next`, `truncated` and `truncation_reason`
 * are overridable for the tools that hit `depth`, `page` or `item_cap` before `budget`.
 */
export interface EnvelopeSpec {
  summary: string;
  read_status: ReadStatus;
  coverage: Coverage;
  totals?: Totals;
  next_tools?: readonly string[];
  next?: string | null;
  truncated?: boolean;
  truncation_reason?: TruncationReason | null;
}

export interface BootstrappingSpec extends EnvelopeSpec {
  retry_after_ms: number;
  message?: string;
  hint?: string;
}

/**
 * The relation an untruncated payload may claim, derived from the three conditions this module
 * can see: complete coverage, no unresolved edges in scope, fresh generation. `emit` owns the
 * fourth (truncation) and downgrades when it cuts.
 */
export function relationFor(readStatus: ReadStatus, coverage: Coverage): TotalRelation {
  const exact = coverage.complete && coverage.unresolved_edges === 0 && !readStatus.stale;
  return exact ? "eq" : "gte";
}

function asErrorCode(value: unknown): AppErrorCode | undefined {
  if (typeof value === "string" && (APP_ERROR_CODES as readonly string[]).includes(value)) {
    return value as AppErrorCode;
  }
  return undefined;
}

/**
 * Normalises anything a caller hands `fail` into a contract error payload. A plain object is
 * rebuilt from the contract fields only, and a code outside the closed union becomes `internal`
 * rather than being forwarded, so no handler can widen the error surface with a hand-built shape.
 */
function errorPayloadOf(error: unknown): ErrorPayload {
  if (isAppError(error)) {
    return toErrorPayload(error);
  }
  if (
    typeof error === "object" &&
    error !== null &&
    typeof (error as { message?: unknown }).message === "string"
  ) {
    const candidate = error as {
      code?: unknown;
      message: string;
      hint?: unknown;
      retryable?: unknown;
      retry_after_ms?: unknown;
    };
    const payload: ErrorPayload = {
      code: asErrorCode(candidate.code) ?? "internal",
      message: candidate.message,
    };
    if (typeof candidate.hint === "string") {
      payload.hint = candidate.hint;
    }
    if (candidate.retryable === true) {
      payload.retryable = true;
    }
    if (typeof candidate.retry_after_ms === "number" && Number.isFinite(candidate.retry_after_ms)) {
      payload.retry_after_ms = candidate.retry_after_ms;
    }
    return payload;
  }
  return toErrorPayload(error);
}

function buildEnvelope<T>(status: EnvelopeStatus, payload: T, spec: EnvelopeSpec): Envelope<T> {
  return {
    status,
    summary: spec.summary,
    // Copied so a caller reusing one fixture cannot mutate an envelope that is already built.
    read_status: { ...spec.read_status },
    coverage: { ...spec.coverage, not_indexed: [...spec.coverage.not_indexed] },
    totals: spec.totals ?? { total_relation: relationFor(spec.read_status, spec.coverage) },
    truncated: spec.truncated ?? false,
    truncation_reason: spec.truncation_reason ?? null,
    next: spec.next ?? null,
    next_tools: [...(spec.next_tools ?? [])],
    payload,
  };
}

/**
 * A copy of `env` carrying a different payload. `emit` derives through this instead of writing
 * the shape again, so the one envelope literal in the codebase stays in this file.
 */
export function withPayload<T>(env: Envelope<T>, payload: T): Envelope<T> {
  return { ...env, payload };
}

/** A copy of `env` with new totals; every `<list>_total` in `totals` is the caller's to keep. */
export function withTotals<T>(env: Envelope<T>, totals: Totals): Envelope<T> {
  return { ...env, totals };
}

/** A query that answered. `payload` is the tool's own shape, unbounded here. */
export function ok<T>(payload: T, spec: EnvelopeSpec): Envelope<T> {
  return buildEnvelope("ok", payload, spec);
}

/** A query whose target matched more than one node. `payload` carries the candidates. */
export function ambiguous<T>(payload: T, spec: EnvelopeSpec): Envelope<T> {
  return buildEnvelope("ambiguous", payload, spec);
}

/** A query that found nothing. `payload` carries near misses, never a bare zero. */
export function notFound<T>(payload: T, spec: EnvelopeSpec): Envelope<T> {
  return buildEnvelope("not_found", payload, spec);
}

/** A failure: `status: error` plus the contract error payload derived from anything thrown. */
export function fail(error: unknown, spec: EnvelopeSpec): Envelope<null> {
  const env = buildEnvelope<null>("error", null, spec);
  env.error = errorPayloadOf(error);
  return env;
}

/**
 * The index is still being built. Returns `retry_after_ms` instead of blocking the client past
 * its deadline (DESIGN.md §5 rule 3).
 */
export function bootstrapping(spec: BootstrappingSpec): Envelope<null> {
  const env = buildEnvelope<null>("bootstrapping", null, spec);
  const error: ErrorPayload = {
    code: "bootstrapping",
    message: spec.message ?? "the index is still being built",
    retry_after_ms: spec.retry_after_ms,
  };
  if (spec.hint !== undefined) {
    error.hint = spec.hint;
  }
  env.error = error;
  return env;
}
